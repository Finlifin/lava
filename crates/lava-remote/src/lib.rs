//! SSH Remote Transport, `lava-server --stdio` JSON-RPC 2.0 Client, Agentless POSIX Fallback,
//! `~/.ssh/config` Discovery, and Port-Forwarding Manager for Lava.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use lava_lsp::{JsonValue, parse_json, which_binary};

/// Remote files strictly larger than 512 KiB stream over `lava-server`'s dedicated
/// authenticated HTTP endpoint (`GET` / `PUT /lava-file`) instead of occupying the
/// SSH stdio JSON-RPC channel.
pub const HTTP_TRANSFER_THRESHOLD_BYTES: u64 = 512 * 1024;

// ---------------------------------------------------------------------------
// Shared JSON-RPC 2.0 `Content-Length` Framing (`lava-remote` <-> `lava-server`)
// ---------------------------------------------------------------------------

/// Read one JSON-RPC 2.0 message from `reader`.
/// Supports standard `Content-Length: <N>\r\n\r\n<body>` framing as well as raw single-line
/// JSON objects (`{...}\n`) so interactive CLI / test pipes also work effortlessly.
pub fn read_rpc_frame<R: BufRead>(reader: &mut R) -> Result<Option<String>, String> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("RPC header read error: {e}"))?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if content_length.is_some() {
                break;
            }
            continue;
        }
        if content_length.is_none() && trimmed.starts_with('{') {
            return Ok(Some(trimmed.to_string()));
        }
        if let Some((name, val)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("Content-Length")
        {
            content_length = val.trim().parse::<usize>().ok();
        }
    }

    let len = content_length.ok_or_else(|| "missing Content-Length header".to_string())?;
    if len > 128 * 1024 * 1024 {
        return Err(format!("RPC frame exceeds 128 MiB limit ({len} bytes)"));
    }
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .map_err(|e| format!("RPC body read error: {e}"))?;
    Ok(Some(String::from_utf8_lossy(&body).into_owned()))
}

/// Write one JSON-RPC 2.0 `Content-Length` framed payload to `writer` and flush immediately.
pub fn write_rpc_frame<W: Write>(writer: &mut W, raw_json: &str) -> Result<(), String> {
    let bytes = raw_json.as_bytes();
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len())
        .and_then(|()| writer.write_all(bytes))
        .and_then(|()| writer.flush())
        .map_err(|e| format!("RPC write error: {e}"))
}

// ---------------------------------------------------------------------------
// SSH & Emacs TRAMP URI Parsing (`ssh://[user@]host[:port]/path` & `/ssh:host:/path`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUri {
    pub authority: String,
    pub prefix: String,
    pub user: String,
    pub host: String,
    pub port: u16,
    pub remote_path: String,
}

impl RemoteUri {
    pub fn to_canonical_uri(&self) -> String {
        format!("{}{}", self.prefix, self.remote_path)
    }

    pub fn ssh_destination(&self) -> String {
        if self.user.is_empty() {
            self.host.clone()
        } else {
            format!("{}@{}", self.user, self.host)
        }
    }
}

pub fn is_remote_uri(raw: &str) -> bool {
    let t = raw.trim();
    t.starts_with("ssh://") || t.starts_with("/ssh:")
}

fn normalize_posix_path(raw_path: &str) -> String {
    if raw_path.is_empty() {
        return "/".to_string();
    }
    let mut stack: Vec<&str> = Vec::new();
    for seg in raw_path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            let _ = stack.pop();
        } else {
            stack.push(seg);
        }
    }
    if stack.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", stack.join("/"))
    }
}

fn parse_authority_spec(
    raw_auth: &str,
    tramp_port_sep: char,
) -> Result<(String, String, u16, String), String> {
    let trimmed = raw_auth.trim();
    if trimmed.is_empty() {
        return Err("empty SSH host authority".to_string());
    }
    let (user, host_port) = match trimmed.rsplit_once('@') {
        Some((u, hp)) => (u.trim().to_string(), hp.trim()),
        None => (String::new(), trimmed),
    };
    let (host, port) = if tramp_port_sep == '#' {
        if let Some((h, p)) = host_port.rsplit_once('#') {
            let parsed = p
                .parse::<u16>()
                .map_err(|_| format!("invalid SSH port '{p}' in '{raw_auth}'"))?;
            (h.trim().to_string(), parsed)
        } else {
            (host_port.to_string(), 0)
        }
    } else if let Some((h, p)) = host_port.rsplit_once(':')
        && !h.contains(':')
        && !p.is_empty()
    {
        let parsed = p
            .parse::<u16>()
            .map_err(|_| format!("invalid SSH port '{p}' in '{raw_auth}'"))?;
        (h.trim().to_string(), parsed)
    } else {
        (host_port.to_string(), 0)
    };

    if host.is_empty() {
        return Err(format!("missing hostname in SSH target '{raw_auth}'"));
    }

    let mut authority = String::new();
    if !user.is_empty() {
        authority.push_str(&user);
        authority.push('@');
    }
    authority.push_str(&host);
    if port != 0 && port != 22 {
        authority.push(':');
        authority.push_str(&port.to_string());
    }
    Ok((user, host, port, authority))
}

/// Parse an `ssh://[user@]host[:port][/path]`, `/ssh:[user@]host[#port]:[/path]`,
/// or bare `[user@]host[:port]` target specification.
pub fn parse_remote_uri(raw: &str) -> Result<RemoteUri, String> {
    let t = raw.trim();
    if let Some(after_scheme) = t.strip_prefix("ssh://") {
        let (auth_part, path_part) = match after_scheme.find('/') {
            Some(idx) => (&after_scheme[..idx], &after_scheme[idx..]),
            None => (after_scheme, "/"),
        };
        let (user, host, port, authority) = parse_authority_spec(auth_part, ':')?;
        let prefix = format!("ssh://{authority}");
        let remote_path = normalize_posix_path(path_part);
        return Ok(RemoteUri {
            authority,
            prefix,
            user,
            host,
            port,
            remote_path,
        });
    }

    if let Some(after_tramp) = t.strip_prefix("/ssh:") {
        let (auth_part, path_part) = after_tramp
            .split_once(':')
            .ok_or_else(|| format!("invalid TRAMP URI '{t}' (expected /ssh:host:/path)"))?;
        let (user, host, port, authority) = parse_authority_spec(auth_part, '#')?;
        let prefix = format!("ssh://{authority}");
        let remote_path = normalize_posix_path(path_part);
        return Ok(RemoteUri {
            authority,
            prefix,
            user,
            host,
            port,
            remote_path,
        });
    }

    let (user, host, port, authority) = parse_authority_spec(t, ':')?;
    let prefix = format!("ssh://{authority}");
    Ok(RemoteUri {
        authority,
        prefix,
        user,
        host,
        port,
        remote_path: "/".to_string(),
    })
}

pub fn normalize_remote_uri(raw: &str) -> String {
    match parse_remote_uri(raw) {
        Ok(u) => u.to_canonical_uri(),
        Err(_) => raw.trim().to_string(),
    }
}

// ---------------------------------------------------------------------------
// `~/.ssh/config` Host Discovery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshConfigHost {
    pub alias: String,
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub identity_file: String,
    pub proxy_jump: String,
}

fn user_home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

fn new_hidden_command<S: AsRef<std::ffi::OsStr>>(program: S) -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub fn discover_ssh_config_hosts() -> Vec<SshConfigHost> {
    if let Ok(custom) = env::var("LAVA_SSH_CONFIG") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return discover_ssh_config_hosts_from_path(Path::new(trimmed));
        }
    }
    let Some(home) = user_home_dir() else {
        return Vec::new();
    };
    let main_cfg = home.join(".ssh").join("config");
    discover_ssh_config_hosts_from_path(&main_cfg)
}

fn wildcard_matches(pattern: &str, text: &str) -> bool {
    let p_bytes = pattern.as_bytes();
    let t_bytes = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None::<usize>, 0usize);
    while ti < t_bytes.len() {
        if pi < p_bytes.len() && (p_bytes[pi] == b'?' || p_bytes[pi] == t_bytes[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p_bytes.len() && p_bytes[pi] == b'*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p_bytes.len() && p_bytes[pi] == b'*' {
        pi += 1;
    }
    pi == p_bytes.len()
}

fn expand_ssh_include_pattern(ssh_dir: &Path, raw_pat: &str) -> Vec<PathBuf> {
    let cleaned = raw_pat.trim().trim_matches('"');
    if cleaned.is_empty() {
        return Vec::new();
    }
    let resolved = if let Some(after_tilde) = cleaned
        .strip_prefix("~/")
        .or_else(|| cleaned.strip_prefix("~\\"))
    {
        if let Some(home) = user_home_dir() {
            home.join(after_tilde)
        } else {
            ssh_dir.join(after_tilde)
        }
    } else if Path::new(cleaned).is_absolute() {
        PathBuf::from(cleaned)
    } else {
        ssh_dir.join(cleaned)
    };

    let resolved_str = resolved.to_string_lossy();
    if !resolved_str.contains('*') && !resolved_str.contains('?') {
        return if resolved.is_file() {
            vec![resolved]
        } else {
            Vec::new()
        };
    }

    let Some(parent) = resolved.parent() else {
        return Vec::new();
    };
    let Some(file_pat) = resolved.file_name().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let Ok(rd) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut matched: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| !name.starts_with('.') && wildcard_matches(file_pat, name))
        })
        .collect();
    matched.sort();
    matched
}

fn parse_ssh_config_file_recursive(
    file: &Path,
    ssh_dir: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<SshConfigHost>,
) {
    if depth > 4 || !file.is_file() {
        return;
    }
    let canon = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    if !visited.insert(canon) {
        return;
    }
    let Ok(content) = fs::read_to_string(file) else {
        return;
    };

    let mut current_aliases: Vec<String> = Vec::new();
    let mut hostname = String::new();
    let mut user = String::new();
    let mut port: u16 = 0;
    let mut identity_file = String::new();
    let mut proxy_jump = String::new();

    let flush_entry = |aliases: &[String],
                       hname: &str,
                       usr: &str,
                       prt: u16,
                       ident: &str,
                       jump: &str,
                       acc: &mut Vec<SshConfigHost>| {
        for alias in aliases {
            if alias.is_empty()
                || alias.contains('*')
                || alias.contains('?')
                || alias.starts_with('!')
            {
                continue;
            }
            if acc.iter().any(|existing| existing.alias == *alias) {
                continue;
            }
            acc.push(SshConfigHost {
                alias: alias.clone(),
                hostname: if hname.is_empty() {
                    alias.clone()
                } else {
                    hname.to_string()
                },
                user: usr.to_string(),
                port: prt,
                identity_file: ident.to_string(),
                proxy_jump: jump.to_string(),
            });
        }
    };

    for raw_line in content.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, |c: char| c.is_whitespace() || c == '=');
        let Some(key) = parts.next().map(str::trim) else {
            continue;
        };
        let Some(val) = parts
            .next()
            .map(|v| v.trim().trim_start_matches('=').trim())
        else {
            continue;
        };
        let val_clean = val.trim_matches('"');
        if key.eq_ignore_ascii_case("Include") {
            flush_entry(
                &current_aliases,
                &hostname,
                &user,
                port,
                &identity_file,
                &proxy_jump,
                out,
            );
            current_aliases.clear();
            hostname.clear();
            user.clear();
            port = 0;
            identity_file.clear();
            proxy_jump.clear();
            for tok in val.split_whitespace() {
                for inc_path in expand_ssh_include_pattern(ssh_dir, tok) {
                    parse_ssh_config_file_recursive(&inc_path, ssh_dir, depth + 1, visited, out);
                }
            }
        } else if key.eq_ignore_ascii_case("Host") {
            flush_entry(
                &current_aliases,
                &hostname,
                &user,
                port,
                &identity_file,
                &proxy_jump,
                out,
            );
            current_aliases = val_clean
                .split_whitespace()
                .map(ToString::to_string)
                .collect();
            hostname.clear();
            user.clear();
            port = 0;
            identity_file.clear();
            proxy_jump.clear();
        } else if key.eq_ignore_ascii_case("Match") {
            flush_entry(
                &current_aliases,
                &hostname,
                &user,
                port,
                &identity_file,
                &proxy_jump,
                out,
            );
            current_aliases.clear();
        } else if key.eq_ignore_ascii_case("HostName") {
            hostname = val_clean.to_string();
        } else if key.eq_ignore_ascii_case("User") {
            user = val_clean.to_string();
        } else if key.eq_ignore_ascii_case("Port") {
            port = val_clean.parse::<u16>().unwrap_or(0);
        } else if key.eq_ignore_ascii_case("IdentityFile") {
            identity_file = val_clean.to_string();
        } else if key.eq_ignore_ascii_case("ProxyJump") {
            proxy_jump = val_clean.to_string();
        }
    }
    flush_entry(
        &current_aliases,
        &hostname,
        &user,
        port,
        &identity_file,
        &proxy_jump,
        out,
    );
}

pub fn discover_ssh_config_hosts_from_path(config_path: &Path) -> Vec<SshConfigHost> {
    let ssh_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut visited = HashSet::new();
    let mut out = Vec::new();

    if config_path.is_file() {
        parse_ssh_config_file_recursive(config_path, &ssh_dir, 0, &mut visited, &mut out);
    }

    let conf_d = ssh_dir.join("config.d");
    if let Ok(rd) = fs::read_dir(&conf_d) {
        let mut sub: Vec<PathBuf> = rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        sub.sort();
        for f in sub {
            parse_ssh_config_file_recursive(&f, &ssh_dir, 1, &mut visited, &mut out);
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Persistent `lava-server --stdio` JSON-RPC 2.0 Client & SSH Fallback
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferProgressSnapshot {
    pub active: bool,
    pub title: String,
    pub detail: String,
    pub transport: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub lines_loaded: u64,
}

pub type TransferProgressCallback = Arc<dyn Fn(Option<TransferProgressSnapshot>) + Send + Sync>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemotePreviewResult {
    pub lines: Vec<String>,
    pub is_binary: bool,
    pub truncated: bool,
    pub total_size: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteGitFileEntry {
    pub index_status: String,
    pub worktree_status: String,
    pub rel_path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteGitStatus {
    pub is_repo: bool,
    pub root: String,
    pub branch: String,
    pub staged_count: i64,
    pub modified_count: i64,
    pub untracked_count: i64,
    pub entries: Vec<RemoteGitFileEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteGitBlame {
    pub found: bool,
    pub commit_short: String,
    pub author: String,
    pub date_ymd: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTransportProfile {
    pub authority: String,
    pub transport_bin: String,
    pub transport_args: Vec<String>,
    pub lsp_proxy_bin: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConnInfo {
    pub authority: String,
    pub prefix: String,
    pub mode: String,
    pub alive: bool,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub remote_home: String,
    pub remote_cwd: String,
    pub server_version: String,
    pub latency_us: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDirEntry {
    pub name: String,
    pub full_uri: String,
    pub remote_path: String,
    pub is_dir: bool,
    pub size: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForwardInfo {
    pub id: u64,
    pub authority: String,
    pub direction: String,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub alive: bool,
}

type PendingRpcSender = mpsc::SyncSender<Result<JsonValue, String>>;
type PendingRpcMap = Arc<Mutex<HashMap<i64, PendingRpcSender>>>;

struct ServerStdioClient {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    alive: Arc<AtomicBool>,
    next_id: AtomicI64,
    pending: PendingRpcMap,
}

impl ServerStdioClient {
    fn spawn(bin: &str, args: &[String]) -> Result<Self, String> {
        let resolved = if bin.contains('/') || bin.contains('\\') || Path::new(bin).is_absolute() {
            bin.to_string()
        } else {
            let w = which_binary(bin);
            if w.is_empty() {
                return Err(format!("remote transport binary '{bin}' not found in PATH"));
            }
            w
        };

        let mut child = new_hidden_command(&resolved)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn remote server transport '{resolved}': {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open remote server stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to open remote server stdout".to_string())?;
        let stderr = child.stderr.take();

        let alive = Arc::new(AtomicBool::new(true));
        let pending: PendingRpcMap = Arc::new(Mutex::new(HashMap::new()));

        if let Some(err_pipe) = stderr {
            thread::spawn(move || {
                let reader = BufReader::new(err_pipe);
                for _ in reader.lines().map_while(Result::ok) {}
            });
        }

        let alive_reader = Arc::clone(&alive);
        let pending_reader = Arc::clone(&pending);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(raw_json)) = read_rpc_frame(&mut reader) {
                let Ok(msg) = parse_json(&raw_json) else {
                    continue;
                };
                let Some(id) = msg.get_i64("id") else {
                    continue;
                };
                let waiter = pending_reader
                    .lock()
                    .ok()
                    .and_then(|mut map| map.remove(&id));
                if let Some(tx) = waiter {
                    if let Some(err_obj) = msg.get("error") {
                        let emsg = err_obj
                            .get_str("message")
                            .unwrap_or("remote lava-server RPC error")
                            .to_string();
                        let _ = tx.send(Err(emsg));
                    } else {
                        let res = msg.get("result").cloned().unwrap_or(JsonValue::Null);
                        let _ = tx.send(Ok(res));
                    }
                }
            }
            alive_reader.store(false, Ordering::Release);
            if let Ok(mut map) = pending_reader.lock() {
                for (_, tx) in map.drain() {
                    let _ = tx.send(Err("remote lava-server connection closed".to_string()));
                }
            }
        });

        Ok(Self {
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            alive,
            next_id: AtomicI64::new(1),
            pending,
        })
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn call(
        &self,
        method: &str,
        params: JsonValue,
        timeout: Duration,
    ) -> Result<JsonValue, String> {
        if !self.is_alive() {
            return Err("remote lava-server session is no longer alive".to_string());
        }
        let req_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .map_err(|_| "remote pending map lock poisoned".to_string())?
            .insert(req_id, tx);

        let req = JsonValue::obj([
            ("jsonrpc", JsonValue::Str("2.0".to_string())),
            ("id", JsonValue::Int(req_id)),
            ("method", JsonValue::Str(method.to_string())),
            ("params", params),
        ]);
        let raw = req.to_json_string();
        let write_res = {
            let mut guard = self
                .stdin
                .lock()
                .map_err(|_| "remote stdin lock poisoned".to_string())?;
            write_rpc_frame(&mut *guard, &raw)
        };
        if let Err(err) = write_res {
            if let Ok(mut map) = self.pending.lock() {
                let _ = map.remove(&req_id);
            }
            self.alive.store(false, Ordering::Release);
            return Err(err);
        }

        match rx.recv_timeout(timeout) {
            Ok(res) => res,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(mut map) = self.pending.lock() {
                    let _ = map.remove(&req_id);
                }
                Err(format!(
                    "remote RPC '{method}' timed out after {} ms",
                    timeout.as_millis()
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.alive.store(false, Ordering::Release);
                Err(format!("remote server disconnected during '{method}'"))
            }
        }
    }

    fn shutdown(&self) {
        if self.is_alive() {
            let _ = self.call(
                "server/shutdown",
                JsonValue::obj([]),
                Duration::from_millis(250),
            );
            self.alive.store(false, Ordering::Release);
        }
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

enum RemoteBackend {
    LavaServer(ServerStdioClient),
    SshPosix {
        ssh_dest: String,
        port: u16,
        control_path: String,
    },
}

struct HttpTunnel {
    local_port: u16,
    child: Child,
}

struct RemoteSession {
    info: Mutex<RemoteConnInfo>,
    backend: RemoteBackend,
    profile: Option<RemoteTransportProfile>,
    uri_meta: RemoteUri,
    http_port: u16,
    http_token: String,
    remote_server_bin: String,
    control_path: String,
    http_tunnel: Mutex<Option<HttpTunnel>>,
}

struct PortForwardProcess {
    info: PortForwardInfo,
    child: Option<Mutex<Child>>,
}

pub struct RemoteManager {
    profiles: Mutex<HashMap<String, RemoteTransportProfile>>,
    sessions: Mutex<HashMap<String, Arc<RemoteSession>>>,
    forwards: Mutex<HashMap<u64, PortForwardProcess>>,
    next_forward_id: AtomicU64,
    last_progress: Mutex<TransferProgressSnapshot>,
    progress_cb: Mutex<Option<TransferProgressCallback>>,
}

impl Default for RemoteManager {
    fn default() -> Self {
        Self::new()
    }
}

fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Quote `s` as a single shell word that evaluates identically in POSIX `sh` (`sh`, `bash`,
/// `dash`, `zsh`) AND `fish` (where `\` escapes `\` and `'` even inside single quotes).
/// Runs containing neither `'` nor `\` are wrapped in `'...'`, while `'` and `\` are emitted
/// outside single quotes as `\'` and `\\`.
fn shell_universal_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut in_quotes = false;
    for ch in s.chars() {
        match ch {
            '\'' => {
                if in_quotes {
                    out.push('\'');
                    in_quotes = false;
                }
                out.push_str("\\'");
            }
            '\\' => {
                if in_quotes {
                    out.push('\'');
                    in_quotes = false;
                }
                out.push_str("\\\\");
            }
            _ => {
                if !in_quotes {
                    out.push('\'');
                    in_quotes = true;
                }
                out.push(ch);
            }
        }
    }
    if in_quotes {
        out.push('\'');
    }
    out
}

const SSH_STDOUT_SENTINEL_TAG: &str = "__LAVA_SSH_OUT_7F3A__";
const SSH_STDOUT_SENTINEL_BYTES: &[u8] = b"__LAVA_SSH_OUT_7F3A__\n";

/// Wrap a POSIX shell script in `sh -c <universal-quoted>` so SSH remote commands execute
/// reliably regardless of whether the remote user's login shell is `fish`, `zsh`, `bash`, or `sh`.
fn wrap_posix_sh(script: &str) -> String {
    format!("sh -c {}", shell_universal_quote(script))
}

fn strip_ssh_banner_bytes(raw: &[u8]) -> &[u8] {
    if let Some(pos) = raw
        .windows(SSH_STDOUT_SENTINEL_BYTES.len())
        .position(|w| w == SSH_STDOUT_SENTINEL_BYTES)
    {
        &raw[pos + SSH_STDOUT_SENTINEL_BYTES.len()..]
    } else {
        raw
    }
}

fn percent_encode_query_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    for &b in raw.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn ssh_base_args(port: u16, control_path: &str) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=8".to_string(),
    ];
    #[cfg(unix)]
    if !control_path.is_empty() {
        args.push("-o".to_string());
        args.push("ControlMaster=auto".to_string());
        args.push("-o".to_string());
        args.push(format!("ControlPath={control_path}"));
        args.push("-o".to_string());
        args.push("ControlPersist=600".to_string());
    }
    #[cfg(not(unix))]
    let _ = control_path;
    if let Ok(custom_cfg) = env::var("LAVA_SSH_CONFIG") {
        let trimmed = custom_cfg.trim();
        if !trimmed.is_empty() {
            args.push("-F".to_string());
            args.push(trimmed.to_string());
        }
    }
    if port != 0 && port != 22 {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    args
}

fn run_ssh_posix_cmd(
    ssh_dest: &str,
    port: u16,
    control_path: &str,
    remote_sh_cmd: &str,
    stdin_bytes: Option<&[u8]>,
) -> Result<(i32, String, String), String> {
    let ssh_bin = which_binary("ssh");
    if ssh_bin.is_empty() {
        return Err("OpenSSH client 'ssh' not found in local PATH".to_string());
    }
    let use_sentinel = remote_sh_cmd.trim() != "sh -s";
    let wrapped_remote_cmd = if !use_sentinel {
        "sh -s".to_string()
    } else {
        wrap_posix_sh(&format!(
            "printf '{SSH_STDOUT_SENTINEL_TAG}\\n'; {remote_sh_cmd}"
        ))
    };
    let mut cmd = new_hidden_command(ssh_bin);
    cmd.args(ssh_base_args(port, control_path))
        .arg(ssh_dest)
        .arg(&wrapped_remote_cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if stdin_bytes.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to invoke ssh to '{ssh_dest}': {e}"))?;
    if let Some(input) = stdin_bytes
        && let Some(mut child_in) = child.stdin.take()
    {
        let _ = child_in.write_all(input);
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("ssh execution error on '{ssh_dest}': {e}"))?;
    let clean_stdout = if use_sentinel {
        strip_ssh_banner_bytes(&out.stdout)
    } else {
        &out.stdout[..]
    };
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(clean_stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn is_linux_elf_file(path: &Path) -> bool {
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).is_ok() && magic == *b"\x7fELF"
}

fn discover_local_server_binary(explicit: &str) -> Result<PathBuf, String> {
    let trimmed = explicit.trim();
    if !trimmed.is_empty() {
        let p = PathBuf::from(trimmed);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "specified lava-server binary not found: '{trimmed}'"
        ));
    }
    if let Ok(env_bin) = env::var("LAVA_SERVER_BIN") {
        let p = PathBuf::from(env_bin.trim());
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Ok(exe) = env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for cand in [
            dir.join("share").join("lava-server").join("lava-server"),
            dir.join("..")
                .join("share")
                .join("lava-server")
                .join("lava-server"),
            dir.join("lava-server"),
        ] {
            if cand.is_file() && is_linux_elf_file(&cand) {
                return Ok(cand);
            }
        }
    }
    for rel in [
        "./dist/lava-server-linux-x86_64-ubuntu22/lava-server",
        "./target/release/lava-server",
        "./target/debug/lava-server",
    ] {
        let p = PathBuf::from(rel);
        if p.is_file() && is_linux_elf_file(&p) {
            return Ok(p);
        }
    }
    let w = which_binary("lava-server");
    if !w.is_empty() {
        let p = PathBuf::from(w);
        if p.is_file() && is_linux_elf_file(&p) {
            return Ok(p);
        }
    }
    Err(
        "could not locate Linux ELF 'lava-server' binary (build with `bash scripts/build-server-ubuntu22.sh` or set `LAVA_SERVER_BIN`)"
            .to_string(),
    )
}

fn discover_local_server_root(hint_bin: Option<&Path>) -> Option<PathBuf> {
    let is_valid_srv = |p: &Path| p.join("packages").is_dir() && p.join("extensions").is_dir();
    if let Ok(env_root) = env::var("LAVA_SERVER_ROOT") {
        let p = PathBuf::from(env_root.trim());
        if is_valid_srv(&p) {
            return Some(p);
        }
    }
    let mut seeds: Vec<PathBuf> = Vec::new();
    if let Some(bin) = hint_bin {
        if let Ok(canon) = fs::canonicalize(bin) {
            seeds.push(canon);
        }
        seeds.push(bin.to_path_buf());
    }
    if let Ok(cwd) = env::current_dir() {
        seeds.push(cwd);
    }
    if let Ok(exe) = env::current_exe() {
        if let Ok(canon) = fs::canonicalize(&exe) {
            seeds.push(canon);
        }
        seeds.push(exe);
    }
    for seed in seeds {
        let mut cur: Option<&Path> = if seed.is_dir() {
            Some(seed.as_path())
        } else {
            seed.parent()
        };
        for _ in 0..8 {
            let Some(dir) = cur else { break };
            let cand = dir.join("server");
            if is_valid_srv(&cand) {
                return Some(cand);
            }
            cur = dir.parent();
        }
    }
    None
}

fn collect_server_source_files(root: &Path, rel_prefix: &str, out: &mut Vec<(String, String)>) {
    let dir = if rel_prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel_prefix)
    };
    let Ok(rd) = fs::read_dir(&dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for entry in entries {
        let Some(name) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let rel = if rel_prefix.is_empty() {
            name.to_string()
        } else {
            format!("{rel_prefix}/{name}")
        };
        if entry.is_dir() {
            collect_server_source_files(root, &rel, out);
        } else if entry.is_file()
            && (name.ends_with(".ns") || name.ends_with(".toml"))
            && let Ok(text) = fs::read_to_string(&entry)
        {
            out.push((rel, text));
        }
    }
}

impl RemoteManager {
    pub fn new() -> Self {
        Self {
            profiles: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            forwards: Mutex::new(HashMap::new()),
            next_forward_id: AtomicU64::new(1),
            last_progress: Mutex::new(TransferProgressSnapshot::default()),
            progress_cb: Mutex::new(None),
        }
    }

    pub fn set_progress_callback(&self, cb: Option<TransferProgressCallback>) {
        if let Ok(mut guard) = self.progress_cb.lock() {
            *guard = cb;
        }
    }

    pub fn last_transfer_progress(&self) -> TransferProgressSnapshot {
        self.last_progress
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    fn emit_progress(&self, snap: TransferProgressSnapshot) {
        let overlay = if snap.active {
            Some(snap.clone())
        } else {
            None
        };
        if let Ok(mut guard) = self.last_progress.lock() {
            *guard = snap;
        }
        let cb_opt = self.progress_cb.lock().ok().and_then(|g| g.clone());
        if let Some(cb) = cb_opt {
            cb(overlay);
        }
    }

    /// Register a custom transport profile for `authority` (e.g. `"devbox"` -> local or container
    /// `lava-server --stdio`, with `lsp_proxy_bin` pointing to `lava-server` for `--lsp-proxy`).
    pub fn register_profile(
        &self,
        authority: &str,
        transport_bin: &str,
        transport_args: Vec<String>,
        lsp_proxy_bin: &str,
    ) -> Result<(), String> {
        let parsed = parse_remote_uri(authority)?;
        let prof = RemoteTransportProfile {
            authority: parsed.authority.clone(),
            transport_bin: transport_bin.to_string(),
            transport_args,
            lsp_proxy_bin: lsp_proxy_bin.to_string(),
        };
        self.profiles
            .lock()
            .map_err(|_| "remote profile lock poisoned".to_string())?
            .insert(parsed.authority, prof);
        Ok(())
    }

    pub fn connect(&self, raw_target: &str) -> Result<RemoteConnInfo, String> {
        let parsed = parse_remote_uri(raw_target)?;
        if let Some(existing) = self
            .sessions
            .lock()
            .ok()
            .and_then(|g| g.get(&parsed.authority).cloned())
        {
            let alive = match &existing.backend {
                RemoteBackend::LavaServer(client) => client.is_alive(),
                RemoteBackend::SshPosix { .. } => true,
            };
            if alive && let Ok(mut info) = existing.info.lock() {
                info.alive = true;
                return Ok(info.clone());
            }
        }

        let profile_opt = self
            .profiles
            .lock()
            .ok()
            .and_then(|g| g.get(&parsed.authority).cloned());

        let t0 = Instant::now();
        let session = if let Some(prof) = profile_opt {
            let client = ServerStdioClient::spawn(&prof.transport_bin, &prof.transport_args)?;
            let ping = client.call("server/ping", JsonValue::obj([]), Duration::from_secs(10))?;
            let sys = client
                .call("exec/sysinfo", JsonValue::obj([]), Duration::from_secs(10))
                .unwrap_or(JsonValue::Null);
            let http_port = ping.get_i64("http_port").unwrap_or(0).clamp(0, 65535) as u16;
            let http_token = ping.get_str("http_token").unwrap_or("").to_string();
            let latency_us = t0.elapsed().as_micros() as i64;
            let info = RemoteConnInfo {
                authority: parsed.authority.clone(),
                prefix: parsed.prefix.clone(),
                mode: "lava-server".to_string(),
                alive: true,
                hostname: sys.get_str("hostname").unwrap_or(&parsed.host).to_string(),
                os: sys.get_str("os").unwrap_or("linux").to_string(),
                arch: sys.get_str("arch").unwrap_or("x86_64").to_string(),
                remote_home: sys.get_str("home").unwrap_or("/root").to_string(),
                remote_cwd: sys.get_str("cwd").unwrap_or("/").to_string(),
                server_version: ping.get_str("version").unwrap_or("0.1.0").to_string(),
                latency_us,
            };
            Arc::new(RemoteSession {
                info: Mutex::new(info),
                backend: RemoteBackend::LavaServer(client),
                profile: Some(prof.clone()),
                uri_meta: parsed.clone(),
                http_port,
                http_token,
                remote_server_bin: prof.transport_bin,
                control_path: String::new(),
                http_tunnel: Mutex::new(None),
            })
        } else {
            let control_path = format!(
                "/tmp/lava-ssh-{}-{}",
                std::process::id(),
                parsed
                    .authority
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect::<String>()
            );
            let ssh_dest = parsed.ssh_destination();
            let custom_install_dir = env::var("LAVA_REMOTE_INSTALL_DIR")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let install_expr = match &custom_install_dir {
                Some(d) => sh_single_quote(d),
                None => "\"$HOME/.lava-server\"".to_string(),
            };
            let probe_script = format!(
                "printf '__LAVA_PROBE_BEGIN__\\n'; \
                 LAVA_INST={install_expr}; \
                 if [ -x \"$LAVA_INST/lava-server\" ]; then \
                   printf 'LAVA_SRV=%s\\n' \"$LAVA_INST/lava-server\"; \
                   if [ -d \"$LAVA_INST/server\" ]; then printf 'LAVA_SRV_ROOT=%s\\n' \"$LAVA_INST/server\"; else printf 'LAVA_SRV_ROOT=\\n'; fi; \
                 elif [ -x \"$HOME/.lava-server/lava-server\" ]; then \
                   printf 'LAVA_SRV=%s\\n' \"$HOME/.lava-server/lava-server\"; \
                   if [ -d \"$HOME/.lava-server/server\" ]; then printf 'LAVA_SRV_ROOT=%s\\n' \"$HOME/.lava-server/server\"; else printf 'LAVA_SRV_ROOT=\\n'; fi; \
                 elif [ -x \"$HOME/.local/bin/lava-server\" ]; then \
                   printf 'LAVA_SRV=%s\\nLAVA_SRV_ROOT=\\n' \"$HOME/.local/bin/lava-server\"; \
                 elif command -v lava-server >/dev/null 2>&1; then \
                   printf 'LAVA_SRV=%s\\nLAVA_SRV_ROOT=\\n' \"$(command -v lava-server)\"; \
                 else \
                   printf 'LAVA_SRV=\\nLAVA_SRV_ROOT=\\n'; \
                 fi; \
                 uname -s; uname -m; hostname; printf '%s\\n' \"$HOME\"; pwd"
            );
            let (code, stdout, stderr) =
                run_ssh_posix_cmd(&ssh_dest, parsed.port, &control_path, &probe_script, None)?;
            if code != 0 {
                return Err(format!(
                    "SSH connection to '{}' failed (exit {code}): {}",
                    parsed.authority,
                    stderr.trim()
                ));
            }
            let all_lines: Vec<&str> = stdout.lines().collect();
            let lines: &[&str] = match all_lines
                .iter()
                .position(|l| l.trim() == "__LAVA_PROBE_BEGIN__")
            {
                Some(idx) => &all_lines[idx + 1..],
                None => &all_lines[..],
            };
            let srv_bin = lines
                .first()
                .and_then(|l| l.strip_prefix("LAVA_SRV="))
                .unwrap_or("")
                .trim()
                .to_string();
            let srv_root = lines
                .get(1)
                .and_then(|l| l.strip_prefix("LAVA_SRV_ROOT="))
                .unwrap_or("")
                .trim()
                .to_string();
            let os = lines
                .get(2)
                .copied()
                .unwrap_or("linux")
                .trim()
                .to_lowercase();
            let arch = lines.get(3).copied().unwrap_or("x86_64").trim().to_string();
            let hostname = lines
                .get(4)
                .copied()
                .unwrap_or(&parsed.host)
                .trim()
                .to_string();
            let remote_home = lines.get(5).copied().unwrap_or("/").trim().to_string();
            let remote_cwd = lines
                .get(6)
                .copied()
                .unwrap_or(&remote_home)
                .trim()
                .to_string();

            if !srv_bin.is_empty() {
                let mut ssh_args = ssh_base_args(parsed.port, &control_path);
                ssh_args.push(ssh_dest.clone());
                let inner_launch = if !srv_root.is_empty() {
                    let cache_dir = Path::new(&srv_bin)
                        .parent()
                        .map(|p| p.join("cache").to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("{remote_home}/.lava-server/cache"));
                    format!(
                        "LAVA_SERVER_CACHE_DIR={} exec {} --stdio --server-root {}",
                        sh_single_quote(&cache_dir),
                        sh_single_quote(&srv_bin),
                        sh_single_quote(&srv_root)
                    )
                } else {
                    format!("exec {} --stdio", sh_single_quote(&srv_bin))
                };
                ssh_args.push(wrap_posix_sh(&inner_launch));
                if let Ok(client) = ServerStdioClient::spawn("ssh", &ssh_args)
                    && let Ok(ping) =
                        client.call("server/ping", JsonValue::obj([]), Duration::from_secs(10))
                {
                    let http_port = ping.get_i64("http_port").unwrap_or(0).clamp(0, 65535) as u16;
                    let http_token = ping.get_str("http_token").unwrap_or("").to_string();
                    let latency_us = t0.elapsed().as_micros() as i64;
                    let info = RemoteConnInfo {
                        authority: parsed.authority.clone(),
                        prefix: parsed.prefix.clone(),
                        mode: "lava-server".to_string(),
                        alive: true,
                        hostname,
                        os,
                        arch,
                        remote_home,
                        remote_cwd,
                        server_version: ping.get_str("version").unwrap_or("0.1.0").to_string(),
                        latency_us,
                    };
                    let s = Arc::new(RemoteSession {
                        info: Mutex::new(info),
                        backend: RemoteBackend::LavaServer(client),
                        profile: None,
                        uri_meta: parsed.clone(),
                        http_port,
                        http_token,
                        remote_server_bin: srv_bin,
                        control_path,
                        http_tunnel: Mutex::new(None),
                    });
                    let out_info = s.info.lock().map(|g| g.clone()).unwrap();
                    if let Ok(mut map) = self.sessions.lock() {
                        map.insert(parsed.authority, s);
                    }
                    return Ok(out_info);
                }
            }

            let latency_us = t0.elapsed().as_micros() as i64;
            let info = RemoteConnInfo {
                authority: parsed.authority.clone(),
                prefix: parsed.prefix.clone(),
                mode: "ssh-posix".to_string(),
                alive: true,
                hostname,
                os,
                arch,
                remote_home,
                remote_cwd,
                server_version: "agentless-posix".to_string(),
                latency_us,
            };
            Arc::new(RemoteSession {
                info: Mutex::new(info),
                backend: RemoteBackend::SshPosix {
                    ssh_dest,
                    port: parsed.port,
                    control_path: control_path.clone(),
                },
                profile: None,
                uri_meta: parsed.clone(),
                http_port: 0,
                http_token: String::new(),
                remote_server_bin: String::new(),
                control_path,
                http_tunnel: Mutex::new(None),
            })
        };

        let out_info = session
            .info
            .lock()
            .map_err(|_| "remote info lock poisoned".to_string())?
            .clone();
        self.sessions
            .lock()
            .map_err(|_| "remote session lock poisoned".to_string())?
            .insert(parsed.authority, session);
        Ok(out_info)
    }

    fn ensure_session(&self, raw_target: &str) -> Result<(Arc<RemoteSession>, RemoteUri), String> {
        let parsed = parse_remote_uri(raw_target)?;
        if let Some(sess) = self
            .sessions
            .lock()
            .ok()
            .and_then(|g| g.get(&parsed.authority).cloned())
        {
            let ok = match &sess.backend {
                RemoteBackend::LavaServer(c) => c.is_alive(),
                RemoteBackend::SshPosix { .. } => true,
            };
            if ok {
                return Ok((sess, parsed));
            }
        }
        let _ = self.connect(raw_target)?;
        let sess = self
            .sessions
            .lock()
            .map_err(|_| "remote session lock poisoned".to_string())?
            .get(&parsed.authority)
            .cloned()
            .ok_or_else(|| format!("failed to establish remote session '{}'", parsed.authority))?;
        Ok((sess, parsed))
    }

    pub fn disconnect(&self, raw_target: &str) -> bool {
        let authority = parse_remote_uri(raw_target)
            .map(|u| u.authority)
            .unwrap_or_else(|_| raw_target.trim().to_string());
        let removed = self
            .sessions
            .lock()
            .ok()
            .and_then(|mut g| g.remove(&authority));
        let Some(sess) = removed else {
            return false;
        };
        if let Ok(mut tun_guard) = sess.http_tunnel.lock()
            && let Some(mut tun) = tun_guard.take()
        {
            let _ = tun.child.kill();
            let _ = tun.child.wait();
        }
        match &sess.backend {
            RemoteBackend::LavaServer(client) => client.shutdown(),
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                #[cfg(unix)]
                if !control_path.is_empty() {
                    let mut cmd = new_hidden_command("ssh");
                    if *port != 0 && *port != 22 {
                        cmd.arg("-p").arg(port.to_string());
                    }
                    let _ = cmd
                        .arg("-O")
                        .arg("exit")
                        .arg("-o")
                        .arg(format!("ControlPath={control_path}"))
                        .arg(ssh_dest)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
                #[cfg(not(unix))]
                let _ = (ssh_dest, port, control_path);
            }
        }
        true
    }

    pub fn disconnect_all(&self) {
        let keys: Vec<String> = self
            .sessions
            .lock()
            .map(|g| g.keys().cloned().collect())
            .unwrap_or_default();
        for k in keys {
            let _ = self.disconnect(&k);
        }
        let fwd_ids: Vec<u64> = self
            .forwards
            .lock()
            .map(|g| g.keys().copied().collect())
            .unwrap_or_default();
        for fid in fwd_ids {
            let _ = self.stop_port_forward(fid);
        }
    }

    pub fn list_connections(&self) -> Vec<RemoteConnInfo> {
        let Ok(guard) = self.sessions.lock() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for sess in guard.values() {
            let alive = match &sess.backend {
                RemoteBackend::LavaServer(c) => c.is_alive(),
                RemoteBackend::SshPosix { .. } => true,
            };
            if let Ok(mut info) = sess.info.lock() {
                info.alive = alive;
                out.push(info.clone());
            }
        }
        out.sort_by(|a, b| a.authority.cmp(&b.authority));
        out
    }

    pub fn stat(&self, raw_uri: &str) -> Result<(bool, bool, bool, i64, bool, String), String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let res = client.call(
                    "fs/stat",
                    JsonValue::obj([("path", JsonValue::Str(uri.remote_path.clone()))]),
                    Duration::from_secs(10),
                )?;
                let exists = res
                    .get("exists")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let is_dir = res
                    .get("is_dir")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let is_file = res
                    .get("is_file")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let size = res.get_i64("size").unwrap_or(0);
                let readonly = res
                    .get("readonly")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let rpath = res.get_str("path").unwrap_or(&uri.remote_path).to_string();
                Ok((exists, is_dir, is_file, size, readonly, rpath))
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q = sh_single_quote(&uri.remote_path);
                let script = format!(
                    "if [ ! -e {q} ]; then echo '0 0 0 0 0'; elif [ -d {q} ]; then echo '1 1 0 0 0'; else sz=$(wc -c < {q} 2>/dev/null || echo 0); ro=0; [ ! -w {q} ] && ro=1; echo \"1 0 1 $sz $ro\"; fi"
                );
                let (_, stdout, _) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                let nums: Vec<i64> = stdout
                    .split_whitespace()
                    .filter_map(|t| t.parse::<i64>().ok())
                    .collect();
                if nums.len() >= 5 {
                    Ok((
                        nums[0] != 0,
                        nums[1] != 0,
                        nums[2] != 0,
                        nums[3],
                        nums[4] != 0,
                        uri.remote_path,
                    ))
                } else {
                    Ok((false, false, false, 0, false, uri.remote_path))
                }
            }
        }
    }

    pub fn read_lines(&self, raw_uri: &str) -> Result<(Vec<String>, bool), String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let res = client.call(
                    "fs/read",
                    JsonValue::obj([("path", JsonValue::Str(uri.remote_path.clone()))]),
                    Duration::from_secs(15),
                )?;
                let crlf = res
                    .get("crlf")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let mut lines = Vec::new();
                if let Some(arr) = res.get("lines").and_then(JsonValue::as_array) {
                    for v in arr {
                        lines.push(v.as_str().unwrap_or("").to_string());
                    }
                }
                if lines.is_empty() {
                    lines.push(String::new());
                }
                Ok((lines, crlf))
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q = sh_single_quote(&uri.remote_path);
                let (code, stdout, stderr) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &format!("cat -- {q}"), None)?;
                if code != 0 {
                    return Err(format!(
                        "cannot read remote file '{}': {}",
                        uri.to_canonical_uri(),
                        stderr.trim()
                    ));
                }
                let has_crlf = stdout.contains("\r\n");
                let normalized = stdout.replace("\r\n", "\n");
                let mut lines: Vec<String> = if normalized.is_empty() {
                    vec![String::new()]
                } else {
                    let trimmed = normalized.strip_suffix('\n').unwrap_or(&normalized);
                    trimmed.split('\n').map(ToString::to_string).collect()
                };
                if lines.is_empty() {
                    lines.push(String::new());
                }
                Ok((lines, has_crlf))
            }
        }
    }

    pub fn write_lines(&self, raw_uri: &str, lines: &[String], crlf: bool) -> Result<i64, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let arr =
                    JsonValue::Array(lines.iter().map(|s| JsonValue::Str(s.clone())).collect());
                let res = client.call(
                    "fs/write",
                    JsonValue::obj([
                        ("path", JsonValue::Str(uri.remote_path.clone())),
                        ("lines", arr),
                        ("crlf", JsonValue::Bool(crlf)),
                        ("create_dirs", JsonValue::Bool(true)),
                    ]),
                    Duration::from_secs(15),
                )?;
                Ok(res.get_i64("bytes").unwrap_or(0))
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let sep = if crlf { "\r\n" } else { "\n" };
                let mut payload = lines.join(sep);
                if !(lines.len() == 1 && lines[0].is_empty()) {
                    payload.push_str(sep);
                }
                let bytes_len = payload.len() as i64;
                let parent = Path::new(&uri.remote_path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("/");
                let q_dir = sh_single_quote(parent);
                let q_path = sh_single_quote(&uri.remote_path);
                let q_tmp = sh_single_quote(&format!("{}.lava-tmp-$$", uri.remote_path));
                let script =
                    format!("mkdir -p -- {q_dir} && cat > {q_tmp} && mv -f -- {q_tmp} {q_path}");
                let (code, _, stderr) = run_ssh_posix_cmd(
                    ssh_dest,
                    *port,
                    control_path,
                    &script,
                    Some(payload.as_bytes()),
                )?;
                if code != 0 {
                    return Err(format!(
                        "cannot write remote file '{}': {}",
                        uri.to_canonical_uri(),
                        stderr.trim()
                    ));
                }
                Ok(bytes_len)
            }
        }
    }

    pub fn readdir(&self, raw_uri: &str, show_hidden: bool) -> Result<Vec<RemoteDirEntry>, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let res = client.call(
                    "fs/readdir",
                    JsonValue::obj([
                        ("path", JsonValue::Str(uri.remote_path.clone())),
                        ("show_hidden", JsonValue::Bool(show_hidden)),
                    ]),
                    Duration::from_secs(10),
                )?;
                let mut out = Vec::new();
                if let Some(arr) = res.get("entries").and_then(JsonValue::as_array) {
                    for item in arr {
                        let Some(name) = item.get_str("name") else {
                            continue;
                        };
                        let rpath = item
                            .get_str("path")
                            .map(ToString::to_string)
                            .unwrap_or_else(|| {
                                normalize_posix_path(&format!("{}/{name}", uri.remote_path))
                            });
                        let is_dir = item
                            .get("is_dir")
                            .and_then(JsonValue::as_bool)
                            .unwrap_or(false);
                        let size = item.get_i64("size").unwrap_or(0);
                        out.push(RemoteDirEntry {
                            name: name.to_string(),
                            full_uri: format!("{}{}", uri.prefix, rpath),
                            remote_path: rpath,
                            is_dir,
                            size,
                        });
                    }
                }
                Ok(out)
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q = sh_single_quote(&uri.remote_path);
                let script = format!(
                    "cd -- {q} && for f in * .[!.]* ..?*; do [ -e \"$f\" ] || [ -L \"$f\" ] || continue; if [ -d \"$f\" ]; then printf 'D\\t0\\t%s\\n' \"$f\"; else sz=$(wc -c < \"$f\" 2>/dev/null || echo 0); printf 'F\\t%s\\t%s\\n' \"$sz\" \"$f\"; fi; done"
                );
                let (code, stdout, stderr) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                if code != 0 {
                    return Err(format!(
                        "cannot list remote directory '{}': {}",
                        uri.to_canonical_uri(),
                        stderr.trim()
                    ));
                }
                let mut out = Vec::new();
                for line in stdout.lines() {
                    let mut parts = line.splitn(3, '\t');
                    let Some(kind) = parts.next() else { continue };
                    let Some(sz_str) = parts.next() else { continue };
                    let Some(name) = parts.next() else { continue };
                    if !show_hidden && name.starts_with('.') {
                        continue;
                    }
                    let is_dir = kind == "D";
                    let size = sz_str.trim().parse::<i64>().unwrap_or(0);
                    let rpath = normalize_posix_path(&format!("{}/{name}", uri.remote_path));
                    out.push(RemoteDirEntry {
                        name: name.to_string(),
                        full_uri: format!("{}{}", uri.prefix, rpath),
                        remote_path: rpath,
                        is_dir,
                        size,
                    });
                }
                out.sort_by(|a, b| {
                    b.is_dir
                        .cmp(&a.is_dir)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                Ok(out)
            }
        }
    }

    pub fn mkdir(&self, raw_uri: &str) -> Result<(), String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let _ = client.call(
                    "fs/mkdir",
                    JsonValue::obj([("path", JsonValue::Str(uri.remote_path))]),
                    Duration::from_secs(10),
                )?;
                Ok(())
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q = sh_single_quote(&uri.remote_path);
                let (code, _, stderr) = run_ssh_posix_cmd(
                    ssh_dest,
                    *port,
                    control_path,
                    &format!("mkdir -p -- {q}"),
                    None,
                )?;
                if code != 0 {
                    return Err(stderr.trim().to_string());
                }
                Ok(())
            }
        }
    }

    pub fn remove(&self, raw_uri: &str, recursive: bool) -> Result<(), String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let _ = client.call(
                    "fs/remove",
                    JsonValue::obj([
                        ("path", JsonValue::Str(uri.remote_path)),
                        ("recursive", JsonValue::Bool(recursive)),
                    ]),
                    Duration::from_secs(10),
                )?;
                Ok(())
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q = sh_single_quote(&uri.remote_path);
                let cmd = if recursive {
                    format!("rm -rf -- {q}")
                } else {
                    format!("rm -f -- {q} || rmdir -- {q}")
                };
                let (code, _, stderr) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &cmd, None)?;
                if code != 0 {
                    return Err(stderr.trim().to_string());
                }
                Ok(())
            }
        }
    }

    pub fn rename(&self, src_uri: &str, dst_uri: &str) -> Result<(), String> {
        let (sess, src) = self.ensure_session(src_uri)?;
        let dst = parse_remote_uri(dst_uri)?;
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let _ = client.call(
                    "fs/rename",
                    JsonValue::obj([
                        ("src", JsonValue::Str(src.remote_path)),
                        ("dst", JsonValue::Str(dst.remote_path)),
                    ]),
                    Duration::from_secs(10),
                )?;
                Ok(())
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let qs = sh_single_quote(&src.remote_path);
                let qd = sh_single_quote(&dst.remote_path);
                let (code, _, stderr) = run_ssh_posix_cmd(
                    ssh_dest,
                    *port,
                    control_path,
                    &format!("mv -f -- {qs} {qd}"),
                    None,
                )?;
                if code != 0 {
                    return Err(stderr.trim().to_string());
                }
                Ok(())
            }
        }
    }

    pub fn copy_entry(&self, src_uri: &str, dst_uri: &str) -> Result<(), String> {
        let (sess, src) = self.ensure_session(src_uri)?;
        let dst = parse_remote_uri(dst_uri)?;
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let _ = client.call(
                    "fs/copy",
                    JsonValue::obj([
                        ("src", JsonValue::Str(src.remote_path)),
                        ("dst", JsonValue::Str(dst.remote_path)),
                    ]),
                    Duration::from_secs(10),
                )?;
                Ok(())
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let qs = sh_single_quote(&src.remote_path);
                let qd = sh_single_quote(&dst.remote_path);
                let (code, _, stderr) = run_ssh_posix_cmd(
                    ssh_dest,
                    *port,
                    control_path,
                    &format!("cp -r -- {qs} {qd}"),
                    None,
                )?;
                if code != 0 {
                    return Err(stderr.trim().to_string());
                }
                Ok(())
            }
        }
    }

    /// Resolve an executable binary name (such as `clangd`, `rust-analyzer`, `rg`) on the remote server!
    pub fn which(&self, raw_target: &str, bin: &str) -> Result<String, String> {
        let (sess, _) = self.ensure_session(raw_target)?;
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let res = client.call(
                    "exec/which",
                    JsonValue::obj([("bin", JsonValue::Str(bin.to_string()))]),
                    Duration::from_secs(8),
                )?;
                Ok(res.get_str("path").unwrap_or("").to_string())
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let qb = sh_single_quote(bin.trim());
                let script = format!(
                    "PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:$PATH\" command -v -- {qb} 2>/dev/null || true"
                );
                let (_, stdout, _) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                Ok(stdout.lines().next().unwrap_or("").trim().to_string())
            }
        }
    }

    /// Walk upward on the remote server from `raw_uri` looking for project root markers
    /// (such as `compile_commands.json`, `Cargo.toml`, `pyproject.toml`, `.git`),
    /// returning a canonical `ssh://<authority>/<root_dir>` URI!
    pub fn find_root(&self, raw_uri: &str, markers: &[String]) -> Result<String, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let remote_root = match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let arr =
                    JsonValue::Array(markers.iter().map(|m| JsonValue::Str(m.clone())).collect());
                let res = client.call(
                    "fs/find_root",
                    JsonValue::obj([
                        ("start_path", JsonValue::Str(uri.remote_path.clone())),
                        ("markers", arr),
                    ]),
                    Duration::from_secs(8),
                )?;
                res.get_str("root").unwrap_or(&uri.remote_path).to_string()
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q_start = sh_single_quote(&uri.remote_path);
                let marker_checks: Vec<String> = markers
                    .iter()
                    .filter(|m| !m.is_empty())
                    .map(|m| format!("[ -e \"$d\"/{} ]", sh_single_quote(m)))
                    .collect();
                let cond = if marker_checks.is_empty() {
                    "false".to_string()
                } else {
                    marker_checks.join(" || ")
                };
                let script = format!(
                    "p={q_start}; if [ -d \"$p\" ]; then d=\"$p\"; else d=$(dirname -- \"$p\"); fi; orig=\"$d\"; while [ -n \"$d\" ]; do if {cond}; then printf '%s\\n' \"$d\"; exit 0; fi; [ \"$d\" = \"/\" ] && break; d=$(dirname -- \"$d\"); done; printf '%s\\n' \"$orig\""
                );
                let (_, stdout, _) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                stdout
                    .lines()
                    .next()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("/")
                    .to_string()
            }
        };
        let norm_root = normalize_posix_path(&remote_root);
        Ok(format!("{}{}", uri.prefix, norm_root))
    }

    pub fn search_files(
        &self,
        raw_uri: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<String>, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let cap = limit.clamp(1, 2000);
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let res = client.call(
                    "search/files",
                    JsonValue::obj([
                        ("root", JsonValue::Str(uri.remote_path.clone())),
                        ("query", JsonValue::Str(query.to_string())),
                        ("limit", JsonValue::Int(cap)),
                    ]),
                    Duration::from_secs(15),
                )?;
                let mut out = Vec::new();
                if let Some(arr) = res.get("files").and_then(JsonValue::as_array) {
                    for f in arr {
                        let Some(s) = f.as_str() else { continue };
                        let full_rpath = if s.starts_with('/') {
                            normalize_posix_path(s)
                        } else {
                            normalize_posix_path(&format!("{}/{s}", uri.remote_path))
                        };
                        out.push(format!("{}{}", uri.prefix, full_rpath));
                    }
                }
                Ok(out)
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q_root = sh_single_quote(&uri.remote_path);
                let script = format!(
                    "cd -- {q_root} && (if command -v rg >/dev/null 2>&1; then rg --files; else find . -type f -not -path '*/.git/*'; fi) | head -n {cap}"
                );
                let (code, stdout, stderr) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                if code != 0 && stdout.trim().is_empty() {
                    return Err(stderr.trim().to_string());
                }
                let q_low = query.trim().to_lowercase();
                let mut out = Vec::new();
                for line in stdout.lines() {
                    let clean = line.trim().trim_start_matches("./");
                    if clean.is_empty() {
                        continue;
                    }
                    if !q_low.is_empty() && !clean.to_lowercase().contains(&q_low) {
                        continue;
                    }
                    let full_rpath = normalize_posix_path(&format!("{}/{clean}", uri.remote_path));
                    out.push(format!("{}{}", uri.prefix, full_rpath));
                }
                Ok(out)
            }
        }
    }

    pub fn search_grep(
        &self,
        raw_uri: &str,
        pattern: &str,
        limit: i64,
    ) -> Result<Vec<(String, i64, i64, String)>, String> {
        if pattern.trim().is_empty() {
            return Ok(Vec::new());
        }
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let cap = limit.clamp(1, 1000);
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let res = client.call(
                    "search/grep",
                    JsonValue::obj([
                        ("root", JsonValue::Str(uri.remote_path.clone())),
                        ("pattern", JsonValue::Str(pattern.to_string())),
                        ("limit", JsonValue::Int(cap)),
                        ("case_sensitive", JsonValue::Bool(false)),
                    ]),
                    Duration::from_secs(15),
                )?;
                let mut out = Vec::new();
                if let Some(arr) = res.get("hits").and_then(JsonValue::as_array) {
                    for h in arr {
                        let Some(fp) = h.get_str("file_path") else {
                            continue;
                        };
                        let rpath = if fp.starts_with('/') {
                            normalize_posix_path(fp)
                        } else {
                            normalize_posix_path(&format!("{}/{fp}", uri.remote_path))
                        };
                        let row = h.get_i64("row").unwrap_or(0).max(0);
                        let col = h.get_i64("col").unwrap_or(0).max(0);
                        let txt = h.get_str("line_text").unwrap_or("").to_string();
                        out.push((format!("{}{}", uri.prefix, rpath), row, col, txt));
                    }
                }
                Ok(out)
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q_root = sh_single_quote(&uri.remote_path);
                let q_pat = sh_single_quote(pattern);
                let script = format!(
                    "cd -- {q_root} && if command -v rg >/dev/null 2>&1; then rg -i --line-number --column --no-heading --color=never -- {q_pat} | head -n {cap}; else grep -rnI -- {q_pat} . | head -n {cap}; fi"
                );
                let (_, stdout, _) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                let mut out = Vec::new();
                for line in stdout.lines() {
                    let mut p = line.splitn(4, ':');
                    let Some(rel) = p.next() else { continue };
                    let Some(r_s) = p.next() else { continue };
                    let Some(c_or_txt) = p.next() else { continue };
                    let row = r_s.parse::<i64>().unwrap_or(1).saturating_sub(1);
                    let (col, text) = if let Some(rest_txt) = p.next()
                        && let Ok(c_num) = c_or_txt.parse::<i64>()
                    {
                        (c_num.saturating_sub(1), rest_txt.to_string())
                    } else {
                        (0, c_or_txt.to_string())
                    };
                    let clean_rel = rel.trim_start_matches("./");
                    let rpath = normalize_posix_path(&format!("{}/{clean_rel}", uri.remote_path));
                    out.push((format!("{}{}", uri.prefix, rpath), row, col, text));
                }
                Ok(out)
            }
        }
    }

    pub fn exec_command(
        &self,
        raw_uri: &str,
        cmd: &str,
        cwd: &str,
        stdin_data: &str,
    ) -> Result<(i64, String, String), String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let remote_cwd = if cwd.trim().is_empty() {
            uri.remote_path.clone()
        } else if is_remote_uri(cwd) {
            parse_remote_uri(cwd)
                .map(|u| u.remote_path)
                .unwrap_or_else(|_| "/".to_string())
        } else {
            cwd.to_string()
        };

        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let res = client.call(
                    "exec/run",
                    JsonValue::obj([
                        ("cmd", JsonValue::Str(cmd.to_string())),
                        ("cwd", JsonValue::Str(remote_cwd)),
                        ("stdin", JsonValue::Str(stdin_data.to_string())),
                    ]),
                    Duration::from_secs(30),
                )?;
                let exit_code = res.get_i64("exit_code").unwrap_or(0);
                let stdout = res.get_str("stdout").unwrap_or("").to_string();
                let stderr = res.get_str("stderr").unwrap_or("").to_string();
                Ok((exit_code, stdout, stderr))
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q_cwd = sh_single_quote(&remote_cwd);
                let wrapped = format!("(cd -- {q_cwd} 2>/dev/null || true) && {cmd}");
                let stdin_opt = if stdin_data.is_empty() {
                    None
                } else {
                    Some(stdin_data.as_bytes())
                };
                let (code, stdout, stderr) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &wrapped, stdin_opt)?;
                Ok((i64::from(code), stdout, stderr))
            }
        }
    }

    /// Install or update `lava-server` + `server/{packages,extensions}` onto the remote host at
    /// `~/.lava-server` (`~/.lava-server/lava-server` + `~/.lava-server/server`), warm bytecode
    /// cache, and reconnect via `~/.lava-server/lava-server`!
    pub fn deploy_server(
        &self,
        raw_target: &str,
        local_server_bin: &str,
    ) -> Result<RemoteConnInfo, String> {
        let local_bin_path = discover_local_server_binary(local_server_bin)?;
        let bytes = fs::read(&local_bin_path).map_err(|e| {
            format!(
                "cannot read local lava-server binary '{}': {e}",
                local_bin_path.display()
            )
        })?;
        let srv_sources = {
            let mut list = Vec::new();
            if let Some(root) = discover_local_server_root(Some(&local_bin_path)) {
                collect_server_source_files(&root, "", &mut list);
            }
            list
        };

        let parsed = parse_remote_uri(raw_target)?;
        let custom_install_dir = env::var("LAVA_REMOTE_INSTALL_DIR")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let has_profile = self
            .profiles
            .lock()
            .ok()
            .is_some_and(|g| g.contains_key(&parsed.authority));

        let total_bytes = bytes.len() as u64;
        self.emit_progress(TransferProgressSnapshot {
            active: true,
            title: format!("Installing lava-server -> {}", parsed.authority),
            detail: "~/.lava-server/lava-server".to_string(),
            transport: "SSH-DEPLOY".to_string(),
            bytes_done: 0,
            bytes_total: total_bytes,
            lines_loaded: 0,
        });

        if has_profile && let Some(ref dest_root_str) = custom_install_dir {
            let dest_root = PathBuf::from(dest_root_str);
            let cache_dir = dest_root.join("cache");
            fs::create_dir_all(&cache_dir).map_err(|e| {
                format!(
                    "failed to create install dir '{}': {e}",
                    dest_root.display()
                )
            })?;
            let tmp_bin = dest_root.join(format!(".lava-server.tmp.{}", std::process::id()));
            let final_bin = dest_root.join("lava-server");
            fs::write(&tmp_bin, &bytes)
                .map_err(|e| format!("failed to write '{}': {e}", tmp_bin.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&tmp_bin, fs::Permissions::from_mode(0o755));
            }
            fs::rename(&tmp_bin, &final_bin)
                .map_err(|e| format!("failed to install '{}': {e}", final_bin.display()))?;

            let server_dir = dest_root.join("server");
            if !srv_sources.is_empty() {
                let tmp_srv = dest_root.join(format!("server.tmp.{}", std::process::id()));
                let _ = fs::remove_dir_all(&tmp_srv);
                for (rel, content) in &srv_sources {
                    let target_f = tmp_srv.join(rel);
                    if let Some(p) = target_f.parent() {
                        let _ = fs::create_dir_all(p);
                    }
                    let _ = fs::write(&target_f, content.as_bytes());
                }
                let _ = fs::remove_dir_all(&server_dir);
                let _ = fs::rename(&tmp_srv, &server_dir);
            }

            let mut targs = vec!["--stdio".to_string()];
            if server_dir.is_dir() {
                let _ = Command::new(&final_bin)
                    .env("LAVA_SERVER_CACHE_DIR", &cache_dir)
                    .arg("check")
                    .arg("--server-root")
                    .arg(&server_dir)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                targs.push("--server-root".to_string());
                targs.push(server_dir.to_string_lossy().into_owned());
            }
            let bin_str = final_bin.to_string_lossy().into_owned();
            let _ = self.register_profile(&parsed.authority, &bin_str, targs, &bin_str);
            self.emit_progress(TransferProgressSnapshot {
                active: false,
                title: format!("Installed lava-server -> {}", parsed.authority),
                detail: bin_str,
                transport: "SSH-DEPLOY".to_string(),
                bytes_done: total_bytes,
                bytes_total: total_bytes,
                lines_loaded: srv_sources.len() as u64,
            });
            let _ = self.disconnect(raw_target);
            return self.connect(raw_target);
        }

        let control_path = format!(
            "/tmp/lava-ssh-deploy-{}-{}",
            std::process::id(),
            parsed
                .authority
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect::<String>()
        );
        let dest_expr = match &custom_install_dir {
            Some(d) => sh_single_quote(d),
            None => "\"$HOME/.lava-server\"".to_string(),
        };
        let bin_script = format!(
            "DEST={dest_expr}; mkdir -p \"$DEST/cache\" && \
             cat > \"$DEST/.lava-server.tmp.$$\" && \
             chmod 755 \"$DEST/.lava-server.tmp.$$\" && \
             mv -f \"$DEST/.lava-server.tmp.$$\" \"$DEST/lava-server\""
        );
        let (code, _, stderr) = run_ssh_posix_cmd(
            &parsed.ssh_destination(),
            parsed.port,
            &control_path,
            &bin_script,
            Some(&bytes),
        )?;
        if code != 0 {
            self.emit_progress(TransferProgressSnapshot::default());
            return Err(format!(
                "failed to install lava-server to '{}': {}",
                parsed.authority,
                stderr.trim()
            ));
        }

        self.emit_progress(TransferProgressSnapshot {
            active: true,
            title: format!("Syncing server extensions -> {}", parsed.authority),
            detail: "~/.lava-server/server".to_string(),
            transport: "SSH-DEPLOY".to_string(),
            bytes_done: total_bytes,
            bytes_total: total_bytes,
            lines_loaded: srv_sources.len() as u64,
        });

        if !srv_sources.is_empty() {
            let mut bundle = format!(
                "set -e\nDEST={dest_expr}\nTMPD=\"$DEST/server.tmp.$$\"\nrm -rf \"$TMPD\"\nmkdir -p \"$TMPD\"\n"
            );
            for (idx, (rel, content)) in srv_sources.iter().enumerate() {
                let rel_dir = Path::new(rel)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("");
                if !rel_dir.is_empty() {
                    bundle.push_str(&format!(
                        "mkdir -p \"$TMPD\"/{}\n",
                        sh_single_quote(rel_dir)
                    ));
                }
                let delim = format!("__LAVA_SRV_FILE_EOF_{idx}__");
                bundle.push_str(&format!(
                    "cat <<'{delim}' > \"$TMPD\"/{}\n",
                    sh_single_quote(rel)
                ));
                bundle.push_str(content);
                if !content.ends_with('\n') {
                    bundle.push('\n');
                }
                bundle.push_str(&delim);
                bundle.push('\n');
            }
            bundle.push_str(
                "rm -rf \"$DEST/server\" && mv -f \"$TMPD\" \"$DEST/server\"\n\
                 LAVA_SERVER_CACHE_DIR=\"$DEST/cache\" \"$DEST/lava-server\" check --server-root \"$DEST/server\" >/dev/null 2>&1 || true\n",
            );
            let (s_code, _, s_err) = run_ssh_posix_cmd(
                &parsed.ssh_destination(),
                parsed.port,
                &control_path,
                "sh -s",
                Some(bundle.as_bytes()),
            )?;
            if s_code != 0 {
                self.emit_progress(TransferProgressSnapshot::default());
                return Err(format!(
                    "failed to sync ~/.lava-server/server on '{}': {}",
                    parsed.authority,
                    s_err.trim()
                ));
            }
        }

        self.emit_progress(TransferProgressSnapshot {
            active: false,
            title: format!("Installed lava-server -> {}", parsed.authority),
            detail: "~/.lava-server/lava-server".to_string(),
            transport: "SSH-DEPLOY".to_string(),
            bytes_done: total_bytes,
            bytes_total: total_bytes,
            lines_loaded: srv_sources.len() as u64,
        });

        let _ = self.disconnect(raw_target);
        self.connect(raw_target)
    }

    fn open_session_http_stream(&self, sess: &RemoteSession) -> Result<TcpStream, String> {
        if sess.http_port == 0 || sess.http_token.is_empty() {
            return Err("remote lava-server HTTP transport is not active".to_string());
        }
        if sess.profile.is_some() {
            return TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], sess.http_port)),
                Duration::from_secs(3),
            )
            .map_err(|e| format!("failed to connect to local profile HTTP port: {e}"));
        }

        // 1. Try direct TCP connection to `<host>:<http_port>` (fast path on LAN/VPN/Tailscale/localhost).
        let direct_target = format!("{}:{}", sess.uri_meta.host, sess.http_port);
        if let Ok(addrs) = direct_target.to_socket_addrs() {
            for addr in addrs {
                if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(400)) {
                    let _ = stream.set_nodelay(true);
                    return Ok(stream);
                }
            }
        }

        // 2. Fallback: lazily spawn/reuse a dedicated SSH `-L` local forward so HTTP streaming
        // still works through firewalls/ProxyJump while keeping the JSON-RPC stdio pipe untouched!
        let mut guard = sess
            .http_tunnel
            .lock()
            .map_err(|_| "HTTP tunnel lock poisoned".to_string())?;
        if let Some(ref mut existing) = *guard {
            let still_running = matches!(existing.child.try_wait(), Ok(None));
            if still_running
                && let Ok(stream) = TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], existing.local_port)),
                    Duration::from_millis(500),
                )
            {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            let _ = existing.child.kill();
            let _ = existing.child.wait();
            *guard = None;
        }

        let probe_listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("cannot allocate local HTTP tunnel port: {e}"))?;
        let local_port = probe_listener
            .local_addr()
            .map_err(|e| format!("local_addr error: {e}"))?
            .port();
        drop(probe_listener);

        let ssh_bin = which_binary("ssh");
        if ssh_bin.is_empty() {
            return Err("OpenSSH client 'ssh' not found for HTTP forward".to_string());
        }
        let mut cmd = new_hidden_command(ssh_bin);
        if !sess.control_path.is_empty() {
            cmd.args(ssh_base_args(sess.uri_meta.port, &sess.control_path));
        } else if sess.uri_meta.port != 0 && sess.uri_meta.port != 22 {
            cmd.arg("-p").arg(sess.uri_meta.port.to_string());
        }
        let mut child = cmd
            .arg("-N")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-L")
            .arg(format!(
                "127.0.0.1:{local_port}:127.0.0.1:{}",
                sess.http_port
            ))
            .arg(sess.uri_meta.ssh_destination())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn HTTP port forward: {e}"))?;

        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("SSH HTTP forward exited early ({status})"));
            }
            if let Ok(stream) = TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], local_port)),
                Duration::from_millis(120),
            ) {
                let _ = stream.set_nodelay(true);
                *guard = Some(HttpTunnel { local_port, child });
                return Ok(stream);
            }
            thread::sleep(Duration::from_millis(35));
        }
        let _ = child.kill();
        let _ = child.wait();
        Err("timed out establishing HTTP transfer channel".to_string())
    }

    fn http_get_file_bytes(
        &self,
        sess: &RemoteSession,
        uri: &RemoteUri,
        expected_size: u64,
    ) -> Result<Vec<u8>, String> {
        let stream = self.open_session_http_stream(sess)?;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));

        let enc_path = percent_encode_query_component(&uri.remote_path);
        let req = format!(
            "GET /lava-file?path={enc_path} HTTP/1.1\r\n\
             Host: 127.0.0.1:{}\r\n\
             Authorization: Bearer {}\r\n\
             Connection: close\r\n\r\n",
            sess.http_port, sess.http_token
        );
        (&stream)
            .write_all(req.as_bytes())
            .and_then(|()| (&stream).flush())
            .map_err(|e| format!("HTTP GET write error: {e}"))?;

        let mut reader = BufReader::with_capacity(64 * 1024, stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .map_err(|e| format!("HTTP status read error: {e}"))?;
        let status_code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);

        let mut content_length: Option<u64> = None;
        let mut hdr_line = String::new();
        loop {
            hdr_line.clear();
            let n = reader
                .read_line(&mut hdr_line)
                .map_err(|e| format!("HTTP header read error: {e}"))?;
            if n == 0 || hdr_line == "\r\n" || hdr_line == "\n" {
                break;
            }
            if let Some((k, v)) = hdr_line.split_once(':')
                && k.trim().eq_ignore_ascii_case("content-length")
            {
                content_length = v.trim().parse::<u64>().ok();
            }
        }

        if status_code != 200 {
            let mut err_body = String::new();
            let _ = reader.take(4096).read_to_string(&mut err_body);
            return Err(format!(
                "HTTP file fetch failed (status {status_code}): {}",
                err_body.trim()
            ));
        }

        let total = content_length.unwrap_or(expected_size);
        let file_label = Path::new(&uri.remote_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&uri.remote_path)
            .to_string();
        let detail_uri = uri.to_canonical_uri();

        let mut out = Vec::with_capacity((total as usize).min(256 * 1024 * 1024));
        let mut chunk = [0u8; 32 * 1024];
        let mut lines_loaded: u64 = 0;

        loop {
            let n = reader
                .read(&mut chunk)
                .map_err(|e| format!("HTTP body read error: {e}"))?;
            if n == 0 {
                break;
            }
            let slice = &chunk[..n];
            lines_loaded += bytecount_newlines(slice);
            out.extend_from_slice(slice);
            self.emit_progress(TransferProgressSnapshot {
                active: true,
                title: format!("Fetching {file_label}"),
                detail: detail_uri.clone(),
                transport: "HTTP".to_string(),
                bytes_done: out.len() as u64,
                bytes_total: total.max(out.len() as u64),
                lines_loaded,
            });
            if content_length.is_some_and(|cl| out.len() as u64 >= cl) {
                break;
            }
        }

        let final_lines = if out.is_empty() {
            1
        } else if out.ends_with(b"\n") {
            lines_loaded.max(1)
        } else {
            lines_loaded + 1
        };
        self.emit_progress(TransferProgressSnapshot {
            active: false,
            title: format!("Fetched {file_label}"),
            detail: detail_uri,
            transport: "HTTP".to_string(),
            bytes_done: out.len() as u64,
            bytes_total: out.len() as u64,
            lines_loaded: final_lines,
        });
        Ok(out)
    }

    fn http_put_file_bytes(
        &self,
        sess: &RemoteSession,
        uri: &RemoteUri,
        bytes: &[u8],
    ) -> Result<u64, String> {
        let mut stream = self.open_session_http_stream(sess)?;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));

        let enc_path = percent_encode_query_component(&uri.remote_path);
        let total = bytes.len() as u64;
        let req_hdr = format!(
            "PUT /lava-file?path={enc_path} HTTP/1.1\r\n\
             Host: 127.0.0.1:{}\r\n\
             Authorization: Bearer {}\r\n\
             Content-Length: {total}\r\n\
             Connection: close\r\n\r\n",
            sess.http_port, sess.http_token
        );
        stream
            .write_all(req_hdr.as_bytes())
            .map_err(|e| format!("HTTP PUT header write error: {e}"))?;

        let file_label = Path::new(&uri.remote_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&uri.remote_path)
            .to_string();
        let detail_uri = uri.to_canonical_uri();
        let mut sent = 0usize;
        let mut lines_sent: u64 = 0;

        while sent < bytes.len() {
            let end = (sent + 32 * 1024).min(bytes.len());
            let slice = &bytes[sent..end];
            stream
                .write_all(slice)
                .map_err(|e| format!("HTTP PUT body write error: {e}"))?;
            lines_sent += bytecount_newlines(slice);
            sent = end;
            self.emit_progress(TransferProgressSnapshot {
                active: true,
                title: format!("Uploading {file_label}"),
                detail: detail_uri.clone(),
                transport: "HTTP".to_string(),
                bytes_done: sent as u64,
                bytes_total: total,
                lines_loaded: lines_sent,
            });
        }
        stream
            .flush()
            .map_err(|e| format!("HTTP PUT flush error: {e}"))?;

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .map_err(|e| format!("HTTP PUT response read error: {e}"))?;
        let status_code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        if status_code != 200 {
            self.emit_progress(TransferProgressSnapshot::default());
            return Err(format!("HTTP file write failed (status {status_code})"));
        }

        self.emit_progress(TransferProgressSnapshot {
            active: false,
            title: format!("Uploaded {file_label}"),
            detail: detail_uri,
            transport: "HTTP".to_string(),
            bytes_done: total,
            bytes_total: total,
            lines_loaded: lines_sent.max(1),
        });
        Ok(total)
    }

    /// Read a remote file into raw bytes with live transfer progress tracking.
    /// Automatically switches to authenticated HTTP streaming when file size exceeds 512 KiB
    /// (`HTTP_TRANSFER_THRESHOLD_BYTES`) so large files (100k+ lines) never block SSH JSON-RPC!
    /// Returns `(exists, is_dir, readonly, bytes, transport_tag)`.
    pub fn read_file_bytes(
        &self,
        raw_uri: &str,
    ) -> Result<(bool, bool, bool, Vec<u8>, String), String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let (exists, is_dir, _is_file, size, readonly, _rpath) = self.stat(raw_uri)?;
        if !exists {
            return Ok((false, false, false, Vec::new(), "SSH".to_string()));
        }
        if is_dir {
            return Ok((true, true, readonly, Vec::new(), "SSH".to_string()));
        }

        let size_u64 = size.max(0) as u64;
        let can_http = sess.http_port > 0 && !sess.http_token.is_empty();
        if size_u64 > HTTP_TRANSFER_THRESHOLD_BYTES && can_http {
            let bytes = self.http_get_file_bytes(&sess, &uri, size_u64)?;
            return Ok((true, false, readonly, bytes, "HTTP".to_string()));
        }

        let file_label = Path::new(&uri.remote_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&uri.remote_path)
            .to_string();
        let detail_uri = uri.to_canonical_uri();
        self.emit_progress(TransferProgressSnapshot {
            active: true,
            title: format!("Fetching {file_label}"),
            detail: detail_uri.clone(),
            transport: "SSH".to_string(),
            bytes_done: 0,
            bytes_total: size_u64,
            lines_loaded: 0,
        });

        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                match client.call(
                    "fs/read",
                    JsonValue::obj([("path", JsonValue::Str(uri.remote_path.clone()))]),
                    Duration::from_secs(20),
                ) {
                    Ok(res) => {
                        let crlf = res
                            .get("crlf")
                            .and_then(JsonValue::as_bool)
                            .unwrap_or(false);
                        let sep: &[u8] = if crlf { b"\r\n" } else { b"\n" };
                        let mut out = Vec::with_capacity(size_u64 as usize + 16);
                        let mut line_cnt: u64 = 0;
                        if let Some(arr) = res.get("lines").and_then(JsonValue::as_array) {
                            let n_lines = arr.len();
                            for (idx, v) in arr.iter().enumerate() {
                                let s = v.as_str().unwrap_or("");
                                out.extend_from_slice(s.as_bytes());
                                if !(n_lines == 1 && s.is_empty()) && idx < n_lines {
                                    out.extend_from_slice(sep);
                                }
                                line_cnt += 1;
                            }
                        }
                        let done = out.len() as u64;
                        self.emit_progress(TransferProgressSnapshot {
                            active: false,
                            title: format!("Fetched {file_label}"),
                            detail: detail_uri,
                            transport: "SSH".to_string(),
                            bytes_done: done,
                            bytes_total: done,
                            lines_loaded: line_cnt.max(1),
                        });
                        Ok((true, false, readonly, out, "SSH".to_string()))
                    }
                    Err(rpc_err) => {
                        if can_http {
                            let bytes = self.http_get_file_bytes(&sess, &uri, size_u64)?;
                            Ok((true, false, readonly, bytes, "HTTP".to_string()))
                        } else {
                            self.emit_progress(TransferProgressSnapshot::default());
                            Err(rpc_err)
                        }
                    }
                }
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let ssh_bin = which_binary("ssh");
                if ssh_bin.is_empty() {
                    self.emit_progress(TransferProgressSnapshot::default());
                    return Err("OpenSSH client 'ssh' not found in local PATH".to_string());
                }
                let q = sh_single_quote(&uri.remote_path);
                let remote_cat = wrap_posix_sh(&format!(
                    "printf '{SSH_STDOUT_SENTINEL_TAG}\\n'; exec cat -- {q}"
                ));
                let mut child = new_hidden_command(ssh_bin)
                    .args(ssh_base_args(*port, control_path))
                    .arg(ssh_dest)
                    .arg(&remote_cat)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("failed to spawn ssh cat: {e}"))?;

                let mut raw_out = Vec::with_capacity((size_u64 as usize).min(256 * 1024 * 1024));
                let mut lines_loaded: u64 = 0;
                if let Some(mut stdout) = child.stdout.take() {
                    let mut chunk = [0u8; 32 * 1024];
                    loop {
                        let n = stdout
                            .read(&mut chunk)
                            .map_err(|e| format!("ssh cat read error: {e}"))?;
                        if n == 0 {
                            break;
                        }
                        lines_loaded += bytecount_newlines(&chunk[..n]);
                        raw_out.extend_from_slice(&chunk[..n]);
                        self.emit_progress(TransferProgressSnapshot {
                            active: true,
                            title: format!("Fetching {file_label}"),
                            detail: detail_uri.clone(),
                            transport: "SSH".to_string(),
                            bytes_done: raw_out.len() as u64,
                            bytes_total: size_u64.max(raw_out.len() as u64),
                            lines_loaded,
                        });
                    }
                }
                let out = strip_ssh_banner_bytes(&raw_out).to_vec();
                lines_loaded = bytecount_newlines(&out);
                let status = child
                    .wait()
                    .map_err(|e| format!("ssh cat wait error: {e}"))?;
                if !status.success() {
                    self.emit_progress(TransferProgressSnapshot::default());
                    return Err(format!(
                        "cannot read remote file '{}'",
                        uri.to_canonical_uri()
                    ));
                }
                let done = out.len() as u64;
                self.emit_progress(TransferProgressSnapshot {
                    active: false,
                    title: format!("Fetched {file_label}"),
                    detail: detail_uri,
                    transport: "SSH".to_string(),
                    bytes_done: done,
                    bytes_total: done,
                    lines_loaded: lines_loaded.max(1),
                });
                Ok((true, false, readonly, out, "SSH".to_string()))
            }
        }
    }

    /// Write raw bytes to a remote file with live progress tracking.
    /// Automatically uses HTTP `PUT /lava-file` when payload exceeds 512 KiB (or 40,000 lines)
    /// and `lava-server` HTTP is active!
    pub fn write_file_bytes(
        &self,
        raw_uri: &str,
        bytes: &[u8],
        crlf: bool,
    ) -> Result<(u64, String), String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let total = bytes.len() as u64;
        let line_cnt = bytecount_newlines(bytes);
        let can_http = sess.http_port > 0 && !sess.http_token.is_empty();

        if can_http && (total > HTTP_TRANSFER_THRESHOLD_BYTES || line_cnt > 40_000) {
            let written = self.http_put_file_bytes(&sess, &uri, bytes)?;
            return Ok((written, "HTTP".to_string()));
        }

        let file_label = Path::new(&uri.remote_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&uri.remote_path)
            .to_string();
        let detail_uri = uri.to_canonical_uri();

        match &sess.backend {
            RemoteBackend::LavaServer(_) => {
                let text = String::from_utf8_lossy(bytes);
                let normalized = text.replace("\r\n", "\n");
                let lines: Vec<String> = if normalized.is_empty() {
                    vec![String::new()]
                } else {
                    let stripped = normalized.strip_suffix('\n').unwrap_or(&normalized);
                    stripped.split('\n').map(ToString::to_string).collect()
                };
                let written = self.write_lines(raw_uri, &lines, crlf)?.max(0) as u64;
                self.emit_progress(TransferProgressSnapshot {
                    active: false,
                    title: format!("Uploaded {file_label}"),
                    detail: detail_uri,
                    transport: "SSH".to_string(),
                    bytes_done: written,
                    bytes_total: written,
                    lines_loaded: lines.len() as u64,
                });
                Ok((written, "SSH".to_string()))
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let parent = Path::new(&uri.remote_path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("/");
                let q_dir = sh_single_quote(parent);
                let q_path = sh_single_quote(&uri.remote_path);
                let q_tmp = sh_single_quote(&format!("{}.lava-tmp-$$", uri.remote_path));
                let script =
                    format!("mkdir -p -- {q_dir} && cat > {q_tmp} && mv -f -- {q_tmp} {q_path}");
                let (code, _, stderr) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, Some(bytes))?;
                if code != 0 {
                    return Err(format!(
                        "cannot write remote file '{}': {}",
                        uri.to_canonical_uri(),
                        stderr.trim()
                    ));
                }
                self.emit_progress(TransferProgressSnapshot {
                    active: false,
                    title: format!("Uploaded {file_label}"),
                    detail: detail_uri,
                    transport: "SSH".to_string(),
                    bytes_done: total,
                    bytes_total: total,
                    lines_loaded: line_cnt.max(1),
                });
                Ok((total, "SSH".to_string()))
            }
        }
    }

    /// Read a capped preview (`max_bytes`, `max_lines`) of a remote file for Dirvish / Telescope
    /// without downloading huge files!
    pub fn read_preview(
        &self,
        raw_uri: &str,
        max_bytes: i64,
        max_lines: i64,
    ) -> Result<RemotePreviewResult, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let cap_bytes = max_bytes.clamp(256, 262_144);
        let cap_lines = max_lines.clamp(5, 2000);

        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                if let Ok(res) = client.call(
                    "fs/read_preview",
                    JsonValue::obj([
                        ("path", JsonValue::Str(uri.remote_path.clone())),
                        ("max_bytes", JsonValue::Int(cap_bytes)),
                        ("max_lines", JsonValue::Int(cap_lines)),
                    ]),
                    Duration::from_secs(6),
                ) {
                    let is_binary = res
                        .get("is_binary")
                        .or_else(|| res.get("binary"))
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false);
                    let truncated = res
                        .get("truncated")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false);
                    let total_size = res
                        .get_i64("total_size")
                        .or_else(|| res.get_i64("size"))
                        .unwrap_or(0);
                    let mut lines = Vec::new();
                    if let Some(arr) = res.get("lines").and_then(JsonValue::as_array) {
                        for v in arr {
                            lines.push(v.as_str().unwrap_or("").to_string());
                        }
                    }
                    return Ok(RemotePreviewResult {
                        lines,
                        is_binary,
                        truncated,
                        total_size,
                    });
                }
                let (lines, _) = self.read_lines(raw_uri)?;
                let truncated = lines.len() as i64 > cap_lines;
                let clipped = lines.into_iter().take(cap_lines as usize).collect();
                Ok(RemotePreviewResult {
                    lines: clipped,
                    is_binary: false,
                    truncated,
                    total_size: 0,
                })
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q = sh_single_quote(&uri.remote_path);
                let script = format!("head -c {cap_bytes} -- {q} 2>/dev/null || true");
                let (_, stdout, _) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                let is_binary = stdout.as_bytes().contains(&0);
                if is_binary {
                    return Ok(RemotePreviewResult {
                        lines: Vec::new(),
                        is_binary: true,
                        truncated: true,
                        total_size: cap_bytes,
                    });
                }
                let all_lines: Vec<String> = stdout
                    .replace("\r\n", "\n")
                    .lines()
                    .map(ToString::to_string)
                    .collect();
                let truncated = all_lines.len() as i64 > cap_lines;
                Ok(RemotePreviewResult {
                    lines: all_lines.into_iter().take(cap_lines as usize).collect(),
                    is_binary: false,
                    truncated,
                    total_size: stdout.len() as i64,
                })
            }
        }
    }

    pub fn git_status(&self, raw_uri: &str) -> Result<RemoteGitStatus, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let res = client.call(
                    "git/status",
                    JsonValue::obj([("cwd", JsonValue::Str(uri.remote_path.clone()))]),
                    Duration::from_secs(10),
                )?;
                let is_repo = res
                    .get("is_repo")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let root_raw = res.get_str("root").unwrap_or("");
                let root = if root_raw.is_empty() {
                    String::new()
                } else {
                    format!("{}{}", uri.prefix, normalize_posix_path(root_raw))
                };
                let branch = res.get_str("branch").unwrap_or("").to_string();
                let staged_count = res.get_i64("staged_count").unwrap_or(0);
                let modified_count = res.get_i64("modified_count").unwrap_or(0);
                let untracked_count = res.get_i64("untracked_count").unwrap_or(0);
                let mut entries = Vec::new();
                if let Some(arr) = res.get("entries").and_then(JsonValue::as_array) {
                    for e in arr {
                        entries.push(RemoteGitFileEntry {
                            index_status: e.get_str("index_status").unwrap_or(" ").to_string(),
                            worktree_status: e
                                .get_str("worktree_status")
                                .unwrap_or(" ")
                                .to_string(),
                            rel_path: e.get_str("rel_path").unwrap_or("").to_string(),
                        });
                    }
                }
                Ok(RemoteGitStatus {
                    is_repo,
                    root,
                    branch,
                    staged_count,
                    modified_count,
                    untracked_count,
                    entries,
                })
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q = sh_single_quote(&uri.remote_path);
                let script = format!(
                    "d={q}; [ -d \"$d\" ] || d=$(dirname -- \"$d\"); \
                     cd -- \"$d\" 2>/dev/null || exit 0; \
                     root=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0; \
                     branch=$(git symbolic-ref --short HEAD 2>/dev/null || git rev-parse --short HEAD 2>/dev/null || echo HEAD); \
                     printf 'ROOT=%s\\nBRANCH=%s\\n' \"$root\" \"$branch\"; \
                     git status --porcelain=v1 -uall 2>/dev/null || true"
                );
                let (_, stdout, _) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                let mut lines = stdout.lines();
                let Some(first) = lines.next().and_then(|l| l.strip_prefix("ROOT=")) else {
                    return Ok(RemoteGitStatus::default());
                };
                let branch = lines
                    .next()
                    .and_then(|l| l.strip_prefix("BRANCH="))
                    .unwrap_or("HEAD")
                    .to_string();
                let mut staged = 0i64;
                let mut modified = 0i64;
                let mut untracked = 0i64;
                let mut entries = Vec::new();
                for l in lines {
                    if l.len() < 4 {
                        continue;
                    }
                    let idx_s = &l[0..1];
                    let wt_s = &l[1..2];
                    let rel = l[3..].trim().to_string();
                    if idx_s == "?" && wt_s == "?" {
                        untracked += 1;
                    } else {
                        if idx_s != " " && idx_s != "?" {
                            staged += 1;
                        }
                        if wt_s != " " && wt_s != "?" {
                            modified += 1;
                        }
                    }
                    entries.push(RemoteGitFileEntry {
                        index_status: idx_s.to_string(),
                        worktree_status: wt_s.to_string(),
                        rel_path: rel,
                    });
                }
                Ok(RemoteGitStatus {
                    is_repo: true,
                    root: format!("{}{}", uri.prefix, normalize_posix_path(first.trim())),
                    branch,
                    staged_count: staged,
                    modified_count: modified,
                    untracked_count: untracked,
                    entries,
                })
            }
        }
    }

    pub fn git_head_lines(
        &self,
        raw_uri: &str,
        file_path: &str,
        rev: &str,
    ) -> Result<Option<Vec<String>>, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let clean_file = if is_remote_uri(file_path) {
            parse_remote_uri(file_path)?.remote_path
        } else {
            file_path.to_string()
        };
        let rev_eff = if rev.trim().is_empty() {
            "HEAD"
        } else {
            rev.trim()
        };

        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let res = client.call(
                    "git/head_lines",
                    JsonValue::obj([
                        ("cwd", JsonValue::Str(uri.remote_path.clone())),
                        ("path", JsonValue::Str(clean_file)),
                        ("rev", JsonValue::Str(rev_eff.to_string())),
                    ]),
                    Duration::from_secs(10),
                )?;
                let tracked = res
                    .get("tracked")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                if !tracked {
                    return Ok(None);
                }
                let mut out = Vec::new();
                if let Some(arr) = res.get("lines").and_then(JsonValue::as_array) {
                    for v in arr {
                        out.push(v.as_str().unwrap_or("").to_string());
                    }
                }
                Ok(Some(out))
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q_cwd = sh_single_quote(&uri.remote_path);
                let q_file = sh_single_quote(&clean_file);
                let q_rev = sh_single_quote(rev_eff);
                let script = format!(
                    "d={q_cwd}; [ -d \"$d\" ] || d=$(dirname -- \"$d\"); cd -- \"$d\" && \
                     root=$(git rev-parse --show-toplevel 2>/dev/null) && \
                     rel=$(realpath --relative-to=\"$root\" -- {q_file} 2>/dev/null || printf '%s' {q_file}) && \
                     git show {q_rev}:\"$rel\" 2>/dev/null"
                );
                let (code, stdout, _) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                if code != 0 {
                    return Ok(None);
                }
                let norm = stdout.replace("\r\n", "\n");
                let stripped = norm.strip_suffix('\n').unwrap_or(&norm);
                let lines = if stripped.is_empty() && norm.is_empty() {
                    vec![String::new()]
                } else {
                    stripped.split('\n').map(ToString::to_string).collect()
                };
                Ok(Some(lines))
            }
        }
    }

    pub fn git_blame_line(
        &self,
        raw_uri: &str,
        file_path: &str,
        row: i64,
    ) -> Result<Option<RemoteGitBlame>, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let clean_file = if is_remote_uri(file_path) {
            parse_remote_uri(file_path)?.remote_path
        } else {
            file_path.to_string()
        };

        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let res = client.call(
                    "git/blame",
                    JsonValue::obj([
                        ("cwd", JsonValue::Str(uri.remote_path)),
                        ("path", JsonValue::Str(clean_file)),
                        ("row", JsonValue::Int(row.max(0))),
                    ]),
                    Duration::from_secs(8),
                )?;
                let found = res
                    .get("found")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                if !found {
                    return Ok(None);
                }
                Ok(Some(RemoteGitBlame {
                    found: true,
                    commit_short: res.get_str("commit_short").unwrap_or("").to_string(),
                    author: res.get_str("author").unwrap_or("").to_string(),
                    date_ymd: res.get_str("date_ymd").unwrap_or("").to_string(),
                    summary: res.get_str("summary").unwrap_or("").to_string(),
                }))
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let line_1 = row.max(0) + 1;
                let q_cwd = sh_single_quote(&uri.remote_path);
                let q_file = sh_single_quote(&clean_file);
                let script = format!(
                    "d={q_cwd}; [ -d \"$d\" ] || d=$(dirname -- \"$d\"); cd -- \"$d\" && \
                     git blame --porcelain -L {line_1},{line_1} -- {q_file} 2>/dev/null"
                );
                let (code, stdout, _) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                if code != 0 || stdout.trim().is_empty() {
                    return Ok(None);
                }
                let mut commit_short = String::new();
                let mut author = String::new();
                let mut summary = String::new();
                for (i, l) in stdout.lines().enumerate() {
                    if i == 0 {
                        commit_short = l
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(8)
                            .collect();
                    } else if let Some(a) = l.strip_prefix("author ") {
                        author = a.trim().to_string();
                    } else if let Some(s) = l.strip_prefix("summary ") {
                        summary = s.trim().to_string();
                    }
                }
                if commit_short.is_empty() {
                    return Ok(None);
                }
                Ok(Some(RemoteGitBlame {
                    found: true,
                    commit_short,
                    author,
                    date_ymd: String::new(),
                    summary,
                }))
            }
        }
    }

    /// Construct `(transport_bin, transport_args, remote_prefix, clean_remote_root)` to launch
    /// a server-side LSP daemon (`clangd`, `rust-analyzer`, `pyright`, `gopls`, etc.) on the
    /// remote machine and pipe its JSON-RPC 2.0 stdio straight into `LspManager::spawn_remote_server`!
    pub fn lsp_launch_command(
        &self,
        raw_target: &str,
        remote_root: &str,
        server_cmd: &str,
        server_args: &[String],
    ) -> Result<(String, Vec<String>, String, String), String> {
        let (sess, uri) = self.ensure_session(raw_target)?;
        let clean_root = if remote_root.trim().is_empty() {
            uri.remote_path.clone()
        } else if is_remote_uri(remote_root) {
            parse_remote_uri(remote_root)
                .map(|u| u.remote_path)
                .unwrap_or_else(|_| "/".to_string())
        } else {
            normalize_posix_path(remote_root)
        };

        if let Some(prof) = &sess.profile {
            let proxy_bin = if !prof.lsp_proxy_bin.trim().is_empty() {
                prof.lsp_proxy_bin.clone()
            } else {
                prof.transport_bin.clone()
            };
            let mut args = vec![
                "--lsp-proxy".to_string(),
                clean_root.clone(),
                server_cmd.to_string(),
            ];
            args.extend(server_args.iter().cloned());
            return Ok((proxy_bin, args, uri.prefix, clean_root));
        }

        let control_path = if !sess.control_path.is_empty() {
            sess.control_path.clone()
        } else {
            format!(
                "/tmp/lava-ssh-{}-{}",
                std::process::id(),
                sess.uri_meta
                    .authority
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect::<String>()
            )
        };
        let mut ssh_args = ssh_base_args(sess.uri_meta.port, &control_path);
        ssh_args.push(sess.uri_meta.ssh_destination());

        if !sess.remote_server_bin.is_empty() {
            let mut proxy_parts = vec![
                sh_single_quote(&sess.remote_server_bin),
                "--lsp-proxy".to_string(),
                sh_single_quote(&clean_root),
                sh_single_quote(server_cmd),
            ];
            for a in server_args {
                proxy_parts.push(sh_single_quote(a));
            }
            let inner = format!("exec {}", proxy_parts.join(" "));
            ssh_args.push(wrap_posix_sh(&inner));
            return Ok(("ssh".to_string(), ssh_args, uri.prefix, clean_root));
        }

        let mut quoted_cmd = vec![sh_single_quote(server_cmd)];
        for a in server_args {
            quoted_cmd.push(sh_single_quote(a));
        }
        let remote_exec = format!(
            "PATH=\"$HOME/.lava-server:$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:$PATH\"; cd -- {} && exec {}",
            sh_single_quote(&clean_root),
            quoted_cmd.join(" ")
        );
        ssh_args.push(wrap_posix_sh(&remote_exec));
        Ok(("ssh".to_string(), ssh_args, uri.prefix, clean_root))
    }

    pub fn start_port_forward(
        &self,
        raw_target: &str,
        direction: &str,
        local_port: u16,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<u64, String> {
        let (sess, uri) = self.ensure_session(raw_target)?;
        let dir_clean = if direction.eq_ignore_ascii_case("remote") || direction == "-R" {
            "remote"
        } else {
            "local"
        };
        let rhost = if remote_host.trim().is_empty() {
            "127.0.0.1"
        } else {
            remote_host.trim()
        };
        let id = self.next_forward_id.fetch_add(1, Ordering::Relaxed);

        let child_opt = if sess.profile.is_some() {
            None
        } else {
            let ssh_bin = which_binary("ssh");
            if ssh_bin.is_empty() {
                return Err("OpenSSH client 'ssh' not found in PATH".to_string());
            }
            let flag = if dir_clean == "remote" { "-R" } else { "-L" };
            let spec = if dir_clean == "remote" {
                format!("{remote_port}:{rhost}:{local_port}")
            } else {
                format!("{local_port}:{rhost}:{remote_port}")
            };
            let mut cmd = new_hidden_command(ssh_bin);
            cmd.arg("-N")
                .arg("-o")
                .arg("BatchMode=yes")
                .arg("-o")
                .arg("ExitOnForwardFailure=yes");
            if uri.port != 0 && uri.port != 22 {
                cmd.arg("-p").arg(uri.port.to_string());
            }
            let child = cmd
                .arg(flag)
                .arg(spec)
                .arg(uri.ssh_destination())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("failed to start SSH port forward: {e}"))?;
            Some(Mutex::new(child))
        };

        let info = PortForwardInfo {
            id,
            authority: uri.authority,
            direction: dir_clean.to_string(),
            local_port,
            remote_host: rhost.to_string(),
            remote_port,
            alive: true,
        };
        self.forwards
            .lock()
            .map_err(|_| "port forward lock poisoned".to_string())?
            .insert(
                id,
                PortForwardProcess {
                    info,
                    child: child_opt,
                },
            );
        Ok(id)
    }

    pub fn stop_port_forward(&self, id: u64) -> bool {
        let removed = self.forwards.lock().ok().and_then(|mut g| g.remove(&id));
        let Some(proc_entry) = removed else {
            return false;
        };
        if let Some(child_mtx) = proc_entry.child
            && let Ok(mut child) = child_mtx.lock()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        true
    }

    pub fn list_port_forwards(&self) -> Vec<PortForwardInfo> {
        let Ok(mut guard) = self.forwards.lock() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in guard.values_mut() {
            if let Some(ref child_mtx) = entry.child
                && let Ok(mut c) = child_mtx.lock()
                && let Ok(Some(_)) = c.try_wait()
            {
                entry.info.alive = false;
            }
            out.push(entry.info.clone());
        }
        out.sort_by_key(|f| f.id);
        out
    }
}

#[inline]
fn bytecount_newlines(buf: &[u8]) -> u64 {
    buf.iter().filter(|&&b| b == b'\n').count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_and_tramp_uris_consistently() {
        let u1 = parse_remote_uri("ssh://alice@devbox.internal:2222/srv/app/../app/src/main.cpp")
            .expect("parse ssh://");
        assert_eq!(u1.authority, "alice@devbox.internal:2222");
        assert_eq!(u1.prefix, "ssh://alice@devbox.internal:2222");
        assert_eq!(u1.user, "alice");
        assert_eq!(u1.host, "devbox.internal");
        assert_eq!(u1.port, 2222);
        assert_eq!(u1.remote_path, "/srv/app/src/main.cpp");
        assert_eq!(
            u1.to_canonical_uri(),
            "ssh://alice@devbox.internal:2222/srv/app/src/main.cpp"
        );

        let u2 = parse_remote_uri("/ssh:alice@devbox.internal#2222:/srv/app/src/main.cpp")
            .expect("parse tramp");
        assert_eq!(u2, u1);
    }

    #[test]
    fn parses_ssh_config_entries_and_include_globs_cleanly() {
        let tmp = std::env::temp_dir().join(format!("lava_ssh_cfg_test_{}", std::process::id()));
        let sub = tmp.join("conf.extra");
        let _ = fs::create_dir_all(&sub);
        fs::write(
            sub.join("10-cloud.conf"),
            "Host cloud-a100\n  HostName 172.16.8.9\n  User ubuntu\n  Port 2222\n  ProxyJump bastion\n",
        )
        .expect("write included ssh config");
        let cfg = tmp.join("config");
        fs::write(
            &cfg,
            "Include conf.extra/*.conf\n\nHost *\n  ServerAliveInterval 30\n\nHost gpu-cluster prod-box\n  HostName 10.20.30.40\n  User fin\n  Port 2200\n",
        )
        .expect("write temp ssh config");
        let hosts = discover_ssh_config_hosts_from_path(&cfg);
        let _ = fs::remove_dir_all(&tmp);
        assert_eq!(hosts.len(), 3);
        assert_eq!(hosts[0].alias, "cloud-a100");
        assert_eq!(hosts[0].hostname, "172.16.8.9");
        assert_eq!(hosts[0].user, "ubuntu");
        assert_eq!(hosts[0].port, 2222);
        assert_eq!(hosts[0].proxy_jump, "bastion");
        assert_eq!(hosts[1].alias, "gpu-cluster");
        assert_eq!(hosts[1].hostname, "10.20.30.40");
        assert_eq!(hosts[1].user, "fin");
        assert_eq!(hosts[1].port, 2200);
    }
}
