//! SSH Remote Transport, `lava-server --stdio` JSON-RPC 2.0 Client, Agentless POSIX Fallback,
//! `~/.ssh/config` Discovery, and Port-Forwarding Manager for Lava.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use lava_lsp::{JsonValue, parse_json, which_binary};

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

pub fn discover_ssh_config_hosts() -> Vec<SshConfigHost> {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let main_cfg = home.join(".ssh").join("config");
    discover_ssh_config_hosts_from_path(&main_cfg)
}

pub fn discover_ssh_config_hosts_from_path(config_path: &Path) -> Vec<SshConfigHost> {
    let mut files = Vec::new();
    if let Some(ssh_dir) = config_path.parent() {
        let conf_d = ssh_dir.join("config.d");
        if let Ok(rd) = fs::read_dir(&conf_d) {
            let mut sub: Vec<PathBuf> = rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect();
            sub.sort();
            files.extend(sub);
        }
    }
    if config_path.is_file() {
        files.push(config_path.to_path_buf());
    }

    let mut out: Vec<SshConfigHost> = Vec::new();
    for file in files {
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
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
            if key.eq_ignore_ascii_case("Host") {
                flush_entry(
                    &current_aliases,
                    &hostname,
                    &user,
                    port,
                    &identity_file,
                    &proxy_jump,
                    &mut out,
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
                    &mut out,
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
            &mut out,
        );
    }

    out
}

// ---------------------------------------------------------------------------
// Persistent `lava-server --stdio` JSON-RPC 2.0 Client & SSH Fallback
// ---------------------------------------------------------------------------

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
        let resolved = if bin.contains('/') || Path::new(bin).is_absolute() {
            bin.to_string()
        } else {
            let w = which_binary(bin);
            if w.is_empty() {
                return Err(format!("remote transport binary '{bin}' not found in PATH"));
            }
            w
        };

        let mut child = Command::new(&resolved)
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

struct RemoteSession {
    info: Mutex<RemoteConnInfo>,
    backend: RemoteBackend,
    profile: Option<RemoteTransportProfile>,
    uri_meta: RemoteUri,
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
}

impl Default for RemoteManager {
    fn default() -> Self {
        Self::new()
    }
}

fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn ssh_base_args(port: u16, control_path: &str) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=8".to_string(),
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        format!("ControlPath={control_path}"),
        "-o".to_string(),
        "ControlPersist=600".to_string(),
    ];
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
    let mut cmd = Command::new(ssh_bin);
    cmd.args(ssh_base_args(port, control_path))
        .arg(ssh_dest)
        .arg(remote_sh_cmd)
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
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

impl RemoteManager {
    pub fn new() -> Self {
        Self {
            profiles: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            forwards: Mutex::new(HashMap::new()),
            next_forward_id: AtomicU64::new(1),
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
                profile: Some(prof),
                uri_meta: parsed.clone(),
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
            let probe_script = "if [ -x \"$HOME/.local/bin/lava-server\" ]; then printf 'LAVA_SRV=%s\\n' \"$HOME/.local/bin/lava-server\"; elif command -v lava-server >/dev/null 2>&1; then printf 'LAVA_SRV=%s\\n' \"$(command -v lava-server)\"; else printf 'LAVA_SRV=\\n'; fi; uname -s; uname -m; hostname; printf '%s\\n' \"$HOME\"; pwd";
            let (code, stdout, stderr) =
                run_ssh_posix_cmd(&ssh_dest, parsed.port, &control_path, probe_script, None)?;
            if code != 0 {
                return Err(format!(
                    "SSH connection to '{}' failed (exit {code}): {}",
                    parsed.authority,
                    stderr.trim()
                ));
            }
            let lines: Vec<&str> = stdout.lines().collect();
            let srv_bin = lines
                .first()
                .and_then(|l| l.strip_prefix("LAVA_SRV="))
                .unwrap_or("")
                .trim()
                .to_string();
            let os = lines
                .get(1)
                .copied()
                .unwrap_or("linux")
                .trim()
                .to_lowercase();
            let arch = lines.get(2).copied().unwrap_or("x86_64").trim().to_string();
            let hostname = lines
                .get(3)
                .copied()
                .unwrap_or(&parsed.host)
                .trim()
                .to_string();
            let remote_home = lines.get(4).copied().unwrap_or("/").trim().to_string();
            let remote_cwd = lines
                .get(5)
                .copied()
                .unwrap_or(&remote_home)
                .trim()
                .to_string();

            if !srv_bin.is_empty() {
                let mut ssh_args = ssh_base_args(parsed.port, &control_path);
                ssh_args.push(ssh_dest.clone());
                ssh_args.push(format!("{} --stdio", sh_single_quote(&srv_bin)));
                if let Ok(client) = ServerStdioClient::spawn("ssh", &ssh_args)
                    && let Ok(ping) =
                        client.call("server/ping", JsonValue::obj([]), Duration::from_secs(8))
                {
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
                    control_path,
                },
                profile: None,
                uri_meta: parsed.clone(),
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
        match &sess.backend {
            RemoteBackend::LavaServer(client) => client.shutdown(),
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let mut cmd = Command::new("ssh");
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

    /// Deploy a local `lava-server` binary onto the remote host at `~/.local/bin/lava-server`
    /// and reconnect in native `lava-server` mode!
    pub fn deploy_server(
        &self,
        raw_target: &str,
        local_server_bin: &str,
    ) -> Result<RemoteConnInfo, String> {
        let bytes = fs::read(local_server_bin).map_err(|e| {
            format!("cannot read local lava-server binary '{local_server_bin}': {e}")
        })?;
        let parsed = parse_remote_uri(raw_target)?;
        let control_path = format!(
            "/tmp/lava-ssh-deploy-{}-{}",
            std::process::id(),
            parsed
                .authority
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect::<String>()
        );
        let script = "mkdir -p \"$HOME/.local/bin\" && cat > \"$HOME/.local/bin/.lava-server.tmp.$$\" && chmod 755 \"$HOME/.local/bin/.lava-server.tmp.$$\" && mv -f \"$HOME/.local/bin/.lava-server.tmp.$$\" \"$HOME/.local/bin/lava-server\"";
        let (code, _, stderr) = run_ssh_posix_cmd(
            &parsed.ssh_destination(),
            parsed.port,
            &control_path,
            script,
            Some(&bytes),
        )?;
        if code != 0 {
            return Err(format!(
                "failed to deploy lava-server to '{}': {}",
                parsed.authority,
                stderr.trim()
            ));
        }
        let _ = self.disconnect(raw_target);
        self.connect(raw_target)
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

        let control_path = match &sess.backend {
            RemoteBackend::SshPosix { control_path, .. } => control_path.clone(),
            RemoteBackend::LavaServer(_) => format!(
                "/tmp/lava-ssh-{}-{}",
                std::process::id(),
                sess.uri_meta
                    .authority
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect::<String>()
            ),
        };
        let mut ssh_args = ssh_base_args(sess.uri_meta.port, &control_path);
        ssh_args.push(sess.uri_meta.ssh_destination());

        let mut quoted_cmd = vec![sh_single_quote(server_cmd)];
        for a in server_args {
            quoted_cmd.push(sh_single_quote(a));
        }
        let remote_exec = format!(
            "PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:$PATH\"; cd -- {} && exec {}",
            sh_single_quote(&clean_root),
            quoted_cmd.join(" ")
        );
        ssh_args.push(remote_exec);
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
            let mut cmd = Command::new(ssh_bin);
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
    fn parses_ssh_config_entries_cleanly() {
        let tmp = std::env::temp_dir().join(format!("lava_ssh_cfg_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let cfg = tmp.join("config");
        fs::write(
            &cfg,
            "Host *\n  ServerAliveInterval 30\n\nHost gpu-cluster prod-box\n  HostName 10.20.30.40\n  User fin\n  Port 2200\n",
        )
        .expect("write temp ssh config");
        let hosts = discover_ssh_config_hosts_from_path(&cfg);
        let _ = fs::remove_dir_all(&tmp);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].alias, "gpu-cluster");
        assert_eq!(hosts[0].hostname, "10.20.30.40");
        assert_eq!(hosts[0].user, "fin");
        assert_eq!(hosts[0].port, 2200);
    }
}
