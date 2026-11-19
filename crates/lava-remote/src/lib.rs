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

/// Dedicated UI `EventQueue` wakeup timer ID (`"RMT\0"`) pushed whenever an asynchronous
/// `RemoteManager` background operation finishes.
pub const REMOTE_WAKE_TIMER_ID: u64 = 0x524D_5400;

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
    pub start_row: i64,
    pub total_lines: i64,
    pub is_binary: bool,
    pub truncated: bool,
    pub total_size: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRankedFileItem {
    pub full_uri: String,
    pub rel_path: String,
    pub is_dir: bool,
    pub score: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteRankedFilesResult {
    pub total_indexed: i64,
    pub total_matched: i64,
    pub items: Vec<RemoteRankedFileItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRichDirEntry {
    pub name: String,
    pub full_uri: String,
    pub remote_path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub is_exec: bool,
    pub size: i64,
    pub modified_secs: i64,
    pub perms: String,
    pub symlink_target: String,
    pub git_badge: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteEntryPreview {
    pub kind: String,
    pub target_uri: String,
    pub target_name: String,
    pub total_count: i64,
    pub total_size: i64,
    pub lines: Vec<String>,
    pub child_entries: Vec<(String, bool, bool, i64)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteDirSnapshot {
    pub resolved_uri: String,
    pub parent_uri: String,
    pub parent_entries: Vec<(String, String, bool)>,
    pub entries: Vec<RemoteRichDirEntry>,
    pub git_is_repo: bool,
    pub git_root_uri: String,
    pub git_branch: String,
    pub git_staged: i64,
    pub git_modified: i64,
    pub git_untracked: i64,
    pub preview: RemoteEntryPreview,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteGitSummary {
    pub is_repo: bool,
    pub root: String,
    pub branch: String,
    pub staged_count: i64,
    pub modified_count: i64,
    pub untracked_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteGitHunk {
    pub old_start: i64,
    pub old_count: i64,
    pub new_start: i64,
    pub new_count: i64,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub enum AsyncRemoteOp {
    SearchFiles {
        uri: String,
        query: String,
        include_hidden: bool,
        include_dirs: bool,
        limit: i64,
    },
    SearchGrep {
        uri: String,
        pattern: String,
        case_sensitive: bool,
        file_filter: String,
        limit: i64,
    },
    ReadPreview {
        uri: String,
        start_row: i64,
        max_lines: i64,
        max_bytes: i64,
    },
    DirSnapshot {
        uri: String,
        show_hidden: bool,
        filter_query: String,
        sort_by: String,
        sort_reverse: bool,
        focus_name: String,
        preferred_row: i64,
        include_parent: bool,
        include_preview: bool,
    },
    EntryPreview {
        uri: String,
        is_dir: bool,
        show_hidden: bool,
        max_lines: i64,
        max_bytes: i64,
    },
    GitSummary {
        uri: String,
    },
    GitFileHunks {
        cwd_uri: String,
        file_uri: String,
        override_lines: Option<Vec<String>>,
    },
    GitBlameLine {
        cwd_uri: String,
        file_uri: String,
        row: i64,
    },
    ScanTodos {
        root_uri: String,
        limit: i64,
    },
    WorkspaceReplace {
        root_uri: String,
        query: String,
        replacement: String,
        case_sensitive: bool,
        file_filter: String,
    },
    ReaddirSimple {
        uri: String,
        show_hidden: bool,
    },
}

#[derive(Debug, Clone)]
pub enum AsyncRemoteResult {
    SearchFiles(RemoteRankedFilesResult),
    SearchGrep {
        total_matched: i64,
        hits: Vec<(String, i64, i64, String)>,
    },
    ReadPreview(RemotePreviewResult),
    DirSnapshot(Box<RemoteDirSnapshot>),
    EntryPreview(RemoteEntryPreview),
    GitSummary(RemoteGitSummary),
    GitFileHunks {
        tracked: bool,
        hunks: Vec<RemoteGitHunk>,
    },
    GitBlameLine(Option<RemoteGitBlame>),
    ScanTodos(Vec<(String, i64, i64, String, String)>),
    WorkspaceReplace {
        files_changed: i64,
        total_replacements: i64,
    },
    ReaddirSimple(Vec<RemoteDirEntry>),
}

#[derive(Debug, Clone)]
pub enum AsyncPollOutcome {
    Idle,
    Pending {
        seq: u64,
    },
    Ready {
        seq: u64,
        elapsed_ms: i64,
        result: AsyncRemoteResult,
    },
    Error {
        seq: u64,
        message: String,
    },
}

struct AsyncCompletedItem {
    seq: u64,
    elapsed_ms: i64,
    outcome: Result<AsyncRemoteResult, String>,
}

#[derive(Default)]
struct AsyncSlotState {
    latest_seq: u64,
    pending: Option<(u64, AsyncRemoteOp)>,
    worker_running: bool,
    completed: Option<AsyncCompletedItem>,
}

pub type RemoteWakeCallback = Arc<dyn Fn() + Send + Sync>;

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
        let sim_delay = GLOBAL_TEST_RPC_DELAY_MS.load(Ordering::Relaxed);
        if sim_delay > 0 && method != "server/ping" && method != "server/shutdown" {
            thread::sleep(Duration::from_millis(sim_delay));
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

static GLOBAL_TEST_RPC_DELAY_MS: AtomicU64 = AtomicU64::new(0);

pub fn set_simulated_rpc_delay_ms(ms: u64) {
    GLOBAL_TEST_RPC_DELAY_MS.store(ms, Ordering::SeqCst);
}

pub struct RemoteManager {
    profiles: Mutex<HashMap<String, RemoteTransportProfile>>,
    sessions: Mutex<HashMap<String, Arc<RemoteSession>>>,
    forwards: Mutex<HashMap<u64, PortForwardProcess>>,
    next_forward_id: AtomicU64,
    last_progress: Mutex<TransferProgressSnapshot>,
    progress_cb: Mutex<Option<TransferProgressCallback>>,
    waker: Mutex<Option<RemoteWakeCallback>>,
    async_slots: Mutex<HashMap<String, AsyncSlotState>>,
    active_async_workers: AtomicU64,
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
#[cfg(not(windows))]
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

/// Format a single argv token placed after `<ssh_dest>` on `Command::new("ssh")` so both
/// Windows `Win32-OpenSSH` (`ssh.exe`, which forwards `CreateProcessW`'s `lpCommandLine` tail and
/// preserves outer `"..."` around space-containing args) and Unix OpenSSH (`ssh`, which space-joins
/// `argv` unquoted) deliver the exact token to remote `fish` as well as POSIX `sh`.
fn ssh_exec_token(token: &str) -> String {
    #[cfg(windows)]
    {
        token.to_string()
    }
    #[cfg(not(windows))]
    {
        if !token.is_empty()
            && token.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '=' | '+')
            })
        {
            token.to_string()
        } else {
            shell_universal_quote(token)
        }
    }
}

fn encode_base64_lines(data: &[u8]) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity((data.len() / 3 + 1) * 4 + (data.len() / 57) + 4);
    let mut col = 0usize;
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(TABLE[(b0 >> 2) as usize]);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize]);
        out.push(if chunk.len() > 1 {
            TABLE[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize]
        } else {
            b'='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(b2 & 0x3F) as usize]
        } else {
            b'='
        });
        col += 4;
        if col >= 76 {
            out.push(b'\n');
            col = 0;
        }
    }
    if col > 0 {
        out.push(b'\n');
    }
    out
}

fn encode_urlsafe_b64_nopad(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((data.len() * 4).div_ceil(3));
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3F) as usize] as char);
        }
    }
    out
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
    let is_raw_sh_s = remote_sh_cmd.trim() == "sh -s";
    let use_sentinel = !is_raw_sh_s;

    // Always pass bare `sh` and `-s` as two separate space-free, quote-free tokens!
    // On Windows `Win32-OpenSSH` (`ssh.exe`), passing `"sh -c ..."` as a single space-containing
    // `.arg(...)` causes `CreateProcessW` + `ssh.exe` to send `"sh -c ..."` with outer double
    // quotes to remote `fish`, triggering `Expected a command, but instead found a string`.
    // Separate `"sh"` + `"-s"` tokens contain zero spaces or quotes and feed pure POSIX scripts
    // over `stdin` cleanly across every SSH client and remote login shell (`fish`, `csh`, `zsh`, `sh`).
    let mut cmd = new_hidden_command(ssh_bin);
    cmd.args(ssh_base_args(port, control_path))
        .arg(ssh_dest)
        .arg("sh")
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to invoke ssh to '{ssh_dest}': {e}"))?;
    if let Some(mut child_in) = child.stdin.take() {
        if is_raw_sh_s {
            if let Some(input) = stdin_bytes {
                let _ = child_in.write_all(input);
            }
        } else if let Some(input) = stdin_bytes {
            let _ = writeln!(child_in, "printf '{SSH_STDOUT_SENTINEL_TAG}\\n'");
            let _ = writeln!(child_in, "__lava_in=\"${{TMPDIR:-/tmp}}/.lava-stdin.$$\"");
            let _ = writeln!(
                child_in,
                "base64 -d <<'__LAVA_B64_EOF_9A4E__' > \"$__lava_in\""
            );
            let b64 = encode_base64_lines(input);
            let _ = child_in.write_all(&b64);
            let _ = writeln!(child_in, "__LAVA_B64_EOF_9A4E__");
            let _ = writeln!(child_in, "(\n{remote_sh_cmd}\n) < \"$__lava_in\"");
            let _ = writeln!(
                child_in,
                "__lava_rc=$?\nrm -f \"$__lava_in\"\nexit $__lava_rc"
            );
        } else {
            let _ = writeln!(child_in, "printf '{SSH_STDOUT_SENTINEL_TAG}\\n'");
            let _ = writeln!(child_in, "{remote_sh_cmd}");
        }
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

pub type RemoteGrepHit = (String, i64, i64, String);
pub type RemoteTodoItem = (String, i64, i64, String, String);

impl RemoteManager {
    pub fn new() -> Self {
        if let Ok(raw_d) = env::var("LAVA_TEST_RPC_DELAY_MS")
            && let Ok(ms) = raw_d.trim().parse::<u64>()
        {
            GLOBAL_TEST_RPC_DELAY_MS.store(ms, Ordering::SeqCst);
        }
        Self {
            profiles: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            forwards: Mutex::new(HashMap::new()),
            next_forward_id: AtomicU64::new(1),
            last_progress: Mutex::new(TransferProgressSnapshot::default()),
            progress_cb: Mutex::new(None),
            waker: Mutex::new(None),
            async_slots: Mutex::new(HashMap::new()),
            active_async_workers: AtomicU64::new(0),
        }
    }

    pub fn set_waker(&self, waker: RemoteWakeCallback) {
        if let Ok(mut guard) = self.waker.lock() {
            *guard = Some(waker);
        }
    }

    fn notify_waker(&self) {
        let cb_opt = self.waker.lock().ok().and_then(|g| g.clone());
        if let Some(cb) = cb_opt {
            cb();
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
                if !srv_root.is_empty() {
                    let cache_dir = Path::new(&srv_bin)
                        .parent()
                        .map(|p| p.join("cache").to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("{remote_home}/.lava-server/cache"));
                    ssh_args.push("env".to_string());
                    ssh_args.push(ssh_exec_token(&format!(
                        "LAVA_SERVER_CACHE_DIR={cache_dir}"
                    )));
                    ssh_args.push(ssh_exec_token(&srv_bin));
                    ssh_args.push("--stdio".to_string());
                    ssh_args.push("--server-root".to_string());
                    ssh_args.push(ssh_exec_token(&srv_root));
                } else {
                    ssh_args.push(ssh_exec_token(&srv_bin));
                    ssh_args.push("--stdio".to_string());
                }
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

    pub fn search_files_ranked(
        &self,
        raw_uri: &str,
        query: &str,
        include_hidden: bool,
        include_dirs: bool,
        limit: i64,
    ) -> Result<RemoteRankedFilesResult, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let cap = limit.clamp(1, 5000);
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let res = client.call(
                    "search/files",
                    JsonValue::obj([
                        ("root", JsonValue::Str(uri.remote_path.clone())),
                        ("query", JsonValue::Str(query.to_string())),
                        ("include_hidden", JsonValue::Bool(include_hidden)),
                        ("include_dirs", JsonValue::Bool(include_dirs)),
                        ("limit", JsonValue::Int(cap)),
                    ]),
                    Duration::from_secs(20),
                )?;
                let mut items = Vec::new();
                if let Some(entries_arr) = res.get("entries").and_then(JsonValue::as_array) {
                    for e in entries_arr {
                        let Some(rel_raw) = e.get_str("path") else {
                            continue;
                        };
                        let rel_clean = rel_raw.trim_start_matches('/').to_string();
                        let full_rpath = if rel_raw.starts_with('/') {
                            normalize_posix_path(rel_raw)
                        } else {
                            normalize_posix_path(&format!("{}/{rel_raw}", uri.remote_path))
                        };
                        let is_dir = e
                            .get("is_dir")
                            .and_then(JsonValue::as_bool)
                            .unwrap_or(false);
                        let score = e.get_i64("score").unwrap_or(100);
                        let rel_path = if !rel_raw.starts_with('/') {
                            rel_clean
                        } else {
                            full_rpath
                                .strip_prefix(&format!(
                                    "{}/",
                                    uri.remote_path.trim_end_matches('/')
                                ))
                                .unwrap_or(&full_rpath)
                                .to_string()
                        };
                        items.push(RemoteRankedFileItem {
                            full_uri: format!("{}{}", uri.prefix, full_rpath),
                            rel_path,
                            is_dir,
                            score,
                        });
                    }
                } else if let Some(arr) = res.get("files").and_then(JsonValue::as_array) {
                    for f in arr {
                        let Some(s) = f.as_str() else { continue };
                        let full_rpath = if s.starts_with('/') {
                            normalize_posix_path(s)
                        } else {
                            normalize_posix_path(&format!("{}/{s}", uri.remote_path))
                        };
                        let rel_path = if !s.starts_with('/') {
                            s.to_string()
                        } else {
                            full_rpath
                                .strip_prefix(&format!(
                                    "{}/",
                                    uri.remote_path.trim_end_matches('/')
                                ))
                                .unwrap_or(&full_rpath)
                                .to_string()
                        };
                        items.push(RemoteRankedFileItem {
                            full_uri: format!("{}{}", uri.prefix, full_rpath),
                            rel_path,
                            is_dir: false,
                            score: 100,
                        });
                    }
                }
                let total_matched = res
                    .get_i64("total_matched")
                    .or_else(|| res.get_i64("count"))
                    .unwrap_or(items.len() as i64);
                let total_indexed = res
                    .get_i64("total_indexed")
                    .unwrap_or(total_matched.max(items.len() as i64));
                Ok(RemoteRankedFilesResult {
                    total_indexed,
                    total_matched,
                    items,
                })
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q_root = sh_single_quote(&uri.remote_path);
                let hidden_rg = if include_hidden {
                    "--hidden -g '!.git'"
                } else {
                    ""
                };
                let hidden_find = if include_hidden {
                    "-not -path '*/.git/*'"
                } else {
                    "-not -path '*/.*'"
                };
                let script = format!(
                    "cd -- {q_root} && (if command -v rg >/dev/null 2>&1; then rg --files {hidden_rg}; else find . -type f {hidden_find}; fi)"
                );
                let (code, stdout, stderr) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                if code != 0 && stdout.trim().is_empty() {
                    return Err(stderr.trim().to_string());
                }
                let mut all_rels: Vec<(String, bool)> = Vec::new();
                let mut seen_dirs = HashSet::new();
                for line in stdout.lines() {
                    let clean = line.trim().trim_start_matches("./");
                    if clean.is_empty() {
                        continue;
                    }
                    if include_dirs {
                        let mut from = 0usize;
                        while let Some(sl) = clean[from..].find('/') {
                            let end = from + sl;
                            if end > 0 {
                                let d = &clean[..end];
                                if seen_dirs.insert(d.to_string()) {
                                    all_rels.push((d.to_string(), true));
                                }
                            }
                            from = end + 1;
                        }
                    }
                    all_rels.push((clean.to_string(), false));
                }
                let total_indexed = all_rels.len() as i64;
                let tokens: Vec<String> = query
                    .split_whitespace()
                    .map(|t| t.to_lowercase())
                    .filter(|t| !t.is_empty())
                    .collect();
                let mut matched: Vec<RemoteRankedFileItem> = Vec::new();
                for (rel, is_dir) in all_rels {
                    let low = rel.to_lowercase();
                    let mut ok = true;
                    let mut score = 100i64;
                    for tok in &tokens {
                        if let Some(neg) = tok.strip_prefix('!') {
                            if !neg.is_empty() && low.contains(neg) {
                                ok = false;
                                break;
                            }
                        } else if let Some(pos) = low.find(tok.as_str()) {
                            score += 5000 - (pos as i64) * 3 - (low.len() as i64);
                        } else {
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        let full_rpath =
                            normalize_posix_path(&format!("{}/{rel}", uri.remote_path));
                        matched.push(RemoteRankedFileItem {
                            full_uri: format!("{}{}", uri.prefix, full_rpath),
                            rel_path: rel,
                            is_dir,
                            score,
                        });
                    }
                }
                if !tokens.is_empty() {
                    matched.sort_by(|a, b| {
                        b.score
                            .cmp(&a.score)
                            .then_with(|| a.rel_path.cmp(&b.rel_path))
                    });
                }
                let total_matched = matched.len() as i64;
                if matched.len() > cap as usize {
                    matched.truncate(cap as usize);
                }
                Ok(RemoteRankedFilesResult {
                    total_indexed,
                    total_matched,
                    items: matched,
                })
            }
        }
    }

    pub fn search_files(
        &self,
        raw_uri: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<String>, String> {
        let ranked = self.search_files_ranked(raw_uri, query, false, false, limit)?;
        Ok(ranked.items.into_iter().map(|it| it.full_uri).collect())
    }

    pub fn search_grep_ext(
        &self,
        raw_uri: &str,
        pattern: &str,
        case_sensitive: bool,
        file_filter: &str,
        limit: i64,
    ) -> Result<(i64, Vec<RemoteGrepHit>), String> {
        if pattern.trim().is_empty() {
            return Ok((0, Vec::new()));
        }
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let cap = limit.clamp(1, 2000);
        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                let res = client.call(
                    "search/grep",
                    JsonValue::obj([
                        ("root", JsonValue::Str(uri.remote_path.clone())),
                        ("pattern", JsonValue::Str(pattern.to_string())),
                        ("limit", JsonValue::Int(cap)),
                        ("case_sensitive", JsonValue::Bool(case_sensitive)),
                        ("file_filter", JsonValue::Str(file_filter.to_string())),
                    ]),
                    Duration::from_secs(20),
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
                let total_matched = res
                    .get_i64("total_matched")
                    .or_else(|| res.get_i64("count"))
                    .unwrap_or(out.len() as i64);
                Ok((total_matched, out))
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q_root = sh_single_quote(&uri.remote_path);
                let q_pat = sh_single_quote(pattern);
                let rg_case = if case_sensitive { "-s" } else { "-i" };
                let grep_case = if case_sensitive { "" } else { "-i" };
                let script = format!(
                    "cd -- {q_root} && if command -v rg >/dev/null 2>&1; then rg {rg_case} -F --line-number --column --no-heading --color=never -- {q_pat} . | head -n {cap}; else grep -rnI {grep_case} -F -- {q_pat} . | head -n {cap}; fi"
                );
                let (_, stdout, _) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                let filt_low = file_filter.trim().to_lowercase();
                let mut out = Vec::new();
                for line in stdout.lines() {
                    let mut p = line.splitn(4, ':');
                    let Some(rel) = p.next() else { continue };
                    let Some(r_s) = p.next() else { continue };
                    let Some(c_or_txt) = p.next() else { continue };
                    let clean_rel = rel.trim_start_matches("./");
                    if !filt_low.is_empty() && !clean_rel.to_lowercase().contains(&filt_low) {
                        continue;
                    }
                    let row = r_s.parse::<i64>().unwrap_or(1).saturating_sub(1);
                    let (col, text) = if let Some(rest_txt) = p.next()
                        && let Ok(c_num) = c_or_txt.parse::<i64>()
                    {
                        (c_num.saturating_sub(1), rest_txt.to_string())
                    } else {
                        (0, c_or_txt.to_string())
                    };
                    let rpath = normalize_posix_path(&format!("{}/{clean_rel}", uri.remote_path));
                    out.push((format!("{}{}", uri.prefix, rpath), row, col, text));
                }
                let total = out.len() as i64;
                Ok((total, out))
            }
        }
    }

    pub fn search_grep(
        &self,
        raw_uri: &str,
        pattern: &str,
        limit: i64,
    ) -> Result<Vec<(String, i64, i64, String)>, String> {
        Ok(self.search_grep_ext(raw_uri, pattern, false, "", limit)?.1)
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
        let b64_bin = encode_base64_lines(&bytes);
        let mut bin_stream = Vec::with_capacity(b64_bin.len() + 320);
        let _ = writeln!(
            bin_stream,
            "set -e\nDEST={dest_expr}\nmkdir -p \"$DEST/cache\"\n\
             base64 -d <<'__LAVA_SRV_BIN_EOF__' > \"$DEST/.lava-server.tmp.$$\""
        );
        bin_stream.extend_from_slice(&b64_bin);
        let _ = writeln!(
            bin_stream,
            "__LAVA_SRV_BIN_EOF__\n\
             chmod 755 \"$DEST/.lava-server.tmp.$$\"\n\
             mv -f \"$DEST/.lava-server.tmp.$$\" \"$DEST/lava-server\""
        );
        let (code, _, stderr) = run_ssh_posix_cmd(
            &parsed.ssh_destination(),
            parsed.port,
            &control_path,
            "sh -s",
            Some(&bin_stream),
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
                let mut child = new_hidden_command(ssh_bin)
                    .args(ssh_base_args(*port, control_path))
                    .arg(ssh_dest)
                    .arg("sh")
                    .arg("-s")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("failed to spawn ssh cat: {e}"))?;
                if let Some(mut child_in) = child.stdin.take() {
                    let _ = writeln!(
                        child_in,
                        "printf '{SSH_STDOUT_SENTINEL_TAG}\\n'; exec cat -- {q}"
                    );
                }

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

    /// Read a capped window (`start_row`, `max_lines`, `max_bytes`) of a remote file for
    /// Dirvish / Telescope without ever downloading full multi-MB files!
    pub fn read_preview_window(
        &self,
        raw_uri: &str,
        start_row: i64,
        max_lines: i64,
        max_bytes: i64,
    ) -> Result<RemotePreviewResult, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let srow = start_row.max(0);
        let cap_bytes = max_bytes.clamp(256, 262_144);
        let cap_lines = max_lines.clamp(5, 2000);

        match &sess.backend {
            RemoteBackend::LavaServer(client) => {
                match client.call(
                    "fs/read_preview",
                    JsonValue::obj([
                        ("path", JsonValue::Str(uri.remote_path.clone())),
                        ("start_row", JsonValue::Int(srow)),
                        ("max_bytes", JsonValue::Int(cap_bytes)),
                        ("max_lines", JsonValue::Int(cap_lines)),
                    ]),
                    Duration::from_secs(8),
                ) {
                    Ok(res) => {
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
                        let res_start = res.get_i64("start_row").unwrap_or(0);
                        let mut lines = Vec::new();
                        if let Some(arr) = res.get("lines").and_then(JsonValue::as_array) {
                            for v in arr {
                                lines.push(v.as_str().unwrap_or("").to_string());
                            }
                        }
                        let total_lines = res
                            .get_i64("total_lines")
                            .unwrap_or(res_start + lines.len() as i64);
                        Ok(RemotePreviewResult {
                            lines,
                            start_row: res_start,
                            total_lines,
                            is_binary,
                            truncated,
                            total_size,
                        })
                    }
                    Err(err) if err.contains("-32601") || err.contains("Method not found") => {
                        let (lines, _) = self.read_lines(raw_uri)?;
                        let total_lines = lines.len() as i64;
                        let skip_n = (srow as usize).min(lines.len());
                        let truncated = total_lines > (skip_n as i64 + cap_lines);
                        let clipped = lines
                            .into_iter()
                            .skip(skip_n)
                            .take(cap_lines as usize)
                            .collect();
                        Ok(RemotePreviewResult {
                            lines: clipped,
                            start_row: skip_n as i64,
                            total_lines,
                            is_binary: false,
                            truncated,
                            total_size: 0,
                        })
                    }
                    Err(err) => Err(err),
                }
            }
            RemoteBackend::SshPosix {
                ssh_dest,
                port,
                control_path,
            } => {
                let q = sh_single_quote(&uri.remote_path);
                let script = if srow > 0 {
                    let start_1 = srow + 1;
                    let end_1 = srow + cap_lines;
                    format!(
                        "sed -n '{start_1},{end_1}p' -- {q} 2>/dev/null | head -c {cap_bytes} || true"
                    )
                } else {
                    format!("head -c {cap_bytes} -- {q} 2>/dev/null || true")
                };
                let (_, stdout, _) =
                    run_ssh_posix_cmd(ssh_dest, *port, control_path, &script, None)?;
                let is_binary = stdout.as_bytes().contains(&0);
                if is_binary {
                    return Ok(RemotePreviewResult {
                        lines: Vec::new(),
                        start_row: 0,
                        total_lines: 0,
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
                let clipped: Vec<String> = all_lines.into_iter().take(cap_lines as usize).collect();
                let total_lines = srow + clipped.len() as i64;
                Ok(RemotePreviewResult {
                    lines: clipped,
                    start_row: srow,
                    total_lines,
                    is_binary: false,
                    truncated,
                    total_size: stdout.len() as i64,
                })
            }
        }
    }

    /// Read a capped preview (`max_bytes`, `max_lines`) starting at row 0.
    pub fn read_preview(
        &self,
        raw_uri: &str,
        max_bytes: i64,
        max_lines: i64,
    ) -> Result<RemotePreviewResult, String> {
        self.read_preview_window(raw_uri, 0, max_lines, max_bytes)
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
            let mut payload = Vec::new();
            payload.extend_from_slice(clean_root.as_bytes());
            payload.push(0);
            payload.extend_from_slice(server_cmd.as_bytes());
            for a in server_args {
                payload.push(0);
                payload.extend_from_slice(a.as_bytes());
            }
            ssh_args.push(ssh_exec_token(&sess.remote_server_bin));
            ssh_args.push("--lsp-proxy-b64".to_string());
            ssh_args.push(encode_urlsafe_b64_nopad(&payload));
            return Ok(("ssh".to_string(), ssh_args, uri.prefix, clean_root));
        }

        let remote_home = sess
            .info
            .lock()
            .ok()
            .map(|g| g.remote_home.clone())
            .filter(|h| !h.is_empty() && h != "/")
            .unwrap_or_default();
        let path_val = if remote_home.is_empty() {
            "/usr/local/bin:/usr/bin:/bin".to_string()
        } else {
            format!(
                "PATH={remote_home}/.lava-server:{remote_home}/.cargo/bin:{remote_home}/.local/bin:/usr/local/bin:/usr/bin:/bin"
            )
        };
        ssh_args.push("env".to_string());
        ssh_args.push("-C".to_string());
        ssh_args.push(ssh_exec_token(&clean_root));
        if !remote_home.is_empty() {
            ssh_args.push(ssh_exec_token(&path_val));
        } else {
            ssh_args.push(format!("PATH={path_val}"));
        }
        ssh_args.push(ssh_exec_token(server_cmd));
        for a in server_args {
            ssh_args.push(ssh_exec_token(a));
        }
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

    fn decode_entry_preview_json(uri_prefix: &str, pv_json: &JsonValue) -> RemoteEntryPreview {
        let kind = pv_json.get_str("kind").unwrap_or("none").to_string();
        let raw_tpath = pv_json.get_str("target_path").unwrap_or("");
        let target_uri = if raw_tpath.is_empty() {
            String::new()
        } else {
            format!("{uri_prefix}{}", normalize_posix_path(raw_tpath))
        };
        let target_name = pv_json.get_str("target_name").unwrap_or("").to_string();
        let total_count = pv_json.get_i64("total_count").unwrap_or(0);
        let total_size = pv_json.get_i64("total_size").unwrap_or(0);
        let mut lines = Vec::new();
        if let Some(arr) = pv_json.get("lines").and_then(JsonValue::as_array) {
            for v in arr {
                lines.push(v.as_str().unwrap_or("").to_string());
            }
        }
        let mut child_entries = Vec::new();
        if let Some(carr) = pv_json.get("child_entries").and_then(JsonValue::as_array) {
            for c in carr {
                let Some(cname) = c.get_str("name") else {
                    continue;
                };
                let cdir = c
                    .get("is_dir")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let csym = c
                    .get("is_symlink")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let csz = c.get_i64("size").unwrap_or(0);
                child_entries.push((cname.to_string(), cdir, csym, csz));
            }
        }
        RemoteEntryPreview {
            kind,
            target_uri,
            target_name,
            total_count,
            total_size,
            lines,
            child_entries,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn dir_snapshot(
        &self,
        raw_uri: &str,
        show_hidden: bool,
        filter_query: &str,
        sort_by: &str,
        sort_reverse: bool,
        focus_name: &str,
        preferred_row: i64,
        include_parent: bool,
        include_preview: bool,
    ) -> Result<RemoteDirSnapshot, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        if let RemoteBackend::LavaServer(client) = &sess.backend {
            match client.call(
                "fs/dir_snapshot",
                JsonValue::obj([
                    ("path", JsonValue::Str(uri.remote_path.clone())),
                    ("show_hidden", JsonValue::Bool(show_hidden)),
                    ("filter_query", JsonValue::Str(filter_query.to_string())),
                    ("sort_by", JsonValue::Str(sort_by.to_string())),
                    ("sort_reverse", JsonValue::Bool(sort_reverse)),
                    ("focus_name", JsonValue::Str(focus_name.to_string())),
                    ("preferred_row", JsonValue::Int(preferred_row.max(0))),
                    ("include_parent", JsonValue::Bool(include_parent)),
                    ("include_preview", JsonValue::Bool(include_preview)),
                ]),
                Duration::from_secs(12),
            ) {
                Ok(res) => {
                    let res_path = res.get_str("resolved_path").unwrap_or(&uri.remote_path);
                    let resolved_uri = format!("{}{}", uri.prefix, normalize_posix_path(res_path));
                    let par_raw = res.get_str("parent_path").unwrap_or("");
                    let parent_uri = if par_raw.is_empty() {
                        String::new()
                    } else {
                        format!("{}{}", uri.prefix, normalize_posix_path(par_raw))
                    };
                    let mut parent_entries = Vec::new();
                    if let Some(parr) = res.get("parent_entries").and_then(JsonValue::as_array) {
                        for p in parr {
                            let Some(pname) = p.get_str("name") else {
                                continue;
                            };
                            let prpath = p
                                .get_str("path")
                                .map(normalize_posix_path)
                                .unwrap_or_else(|| {
                                    normalize_posix_path(&format!("{par_raw}/{pname}"))
                                });
                            let pis_dir = p
                                .get("is_dir")
                                .and_then(JsonValue::as_bool)
                                .unwrap_or(false);
                            parent_entries.push((
                                pname.to_string(),
                                format!("{}{}", uri.prefix, prpath),
                                pis_dir,
                            ));
                        }
                    }
                    let mut entries = Vec::new();
                    if let Some(earr) = res.get("entries").and_then(JsonValue::as_array) {
                        for e in earr {
                            let Some(ename) = e.get_str("name") else {
                                continue;
                            };
                            let erpath = e
                                .get_str("path")
                                .map(normalize_posix_path)
                                .unwrap_or_else(|| {
                                    normalize_posix_path(&format!("{res_path}/{ename}"))
                                });
                            entries.push(RemoteRichDirEntry {
                                name: ename.to_string(),
                                full_uri: format!("{}{}", uri.prefix, erpath),
                                remote_path: erpath,
                                is_dir: e
                                    .get("is_dir")
                                    .and_then(JsonValue::as_bool)
                                    .unwrap_or(false),
                                is_symlink: e
                                    .get("is_symlink")
                                    .and_then(JsonValue::as_bool)
                                    .unwrap_or(false),
                                is_exec: e
                                    .get("is_exec")
                                    .and_then(JsonValue::as_bool)
                                    .unwrap_or(false),
                                size: e.get_i64("size").unwrap_or(0),
                                modified_secs: e.get_i64("modified_secs").unwrap_or(0),
                                perms: e.get_str("perms").unwrap_or("-rw-r--r--").to_string(),
                                symlink_target: e
                                    .get_str("symlink_target")
                                    .unwrap_or("")
                                    .to_string(),
                                git_badge: e.get_str("git_badge").unwrap_or(" ").to_string(),
                            });
                        }
                    }
                    let git_is_repo = res
                        .get("git_is_repo")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false);
                    let git_root_raw = res.get_str("git_root").unwrap_or("");
                    let git_root_uri = if git_root_raw.is_empty() {
                        String::new()
                    } else {
                        format!("{}{}", uri.prefix, normalize_posix_path(git_root_raw))
                    };
                    let preview = res
                        .get("preview")
                        .map(|pv| Self::decode_entry_preview_json(&uri.prefix, pv))
                        .unwrap_or_default();
                    return Ok(RemoteDirSnapshot {
                        resolved_uri,
                        parent_uri,
                        parent_entries,
                        entries,
                        git_is_repo,
                        git_root_uri,
                        git_branch: res.get_str("git_branch").unwrap_or("").to_string(),
                        git_staged: res.get_i64("git_staged").unwrap_or(0),
                        git_modified: res.get_i64("git_modified").unwrap_or(0),
                        git_untracked: res.get_i64("git_untracked").unwrap_or(0),
                        preview,
                    });
                }
                Err(err) if !err.contains("-32601") && !err.contains("Method not found") => {
                    return Err(err);
                }
                Err(_) => {}
            }
        }

        // Fallback path for `SshPosix` or older `lava-server` binary before `:RemoteInstallServer`:
        let base_items = self.readdir(raw_uri, show_hidden)?;
        let filt_low = filter_query.trim().to_lowercase();
        let mut entries: Vec<RemoteRichDirEntry> = base_items
            .into_iter()
            .filter(|it| filt_low.is_empty() || it.name.to_lowercase().contains(&filt_low))
            .map(|it| RemoteRichDirEntry {
                name: it.name,
                full_uri: it.full_uri,
                remote_path: it.remote_path,
                is_dir: it.is_dir,
                is_symlink: false,
                is_exec: false,
                size: it.size,
                modified_secs: 0,
                perms: if it.is_dir {
                    "drwxr-xr-x".to_string()
                } else {
                    "-rw-r--r--".to_string()
                },
                symlink_target: String::new(),
                git_badge: " ".to_string(),
            })
            .collect();
        entries.sort_by(|a, b| {
            if a.is_dir != b.is_dir {
                return b.is_dir.cmp(&a.is_dir);
            }
            let ord = match sort_by {
                "size" => a.size.cmp(&b.size),
                _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            };
            if sort_reverse { ord.reverse() } else { ord }
        });
        let (parent_uri, parent_entries) = if include_parent && uri.remote_path != "/" {
            let par_p = Path::new(&uri.remote_path)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("/");
            let par_u = format!("{}{}", uri.prefix, normalize_posix_path(par_p));
            let pitems = self
                .readdir(&par_u, show_hidden)
                .unwrap_or_default()
                .into_iter()
                .map(|p| (p.name, p.full_uri, p.is_dir))
                .collect();
            (par_u, pitems)
        } else {
            (String::new(), Vec::new())
        };
        let preview = if include_preview && !entries.is_empty() {
            let idx = if !focus_name.is_empty()
                && let Some(pos) = entries.iter().position(|e| e.name == focus_name)
            {
                pos
            } else {
                (preferred_row.max(0) as usize).min(entries.len() - 1)
            };
            let focused = &entries[idx];
            self.entry_preview(&focused.full_uri, focused.is_dir, show_hidden, 80, 8192)
                .unwrap_or_default()
        } else {
            RemoteEntryPreview::default()
        };
        Ok(RemoteDirSnapshot {
            resolved_uri: uri.to_canonical_uri(),
            parent_uri,
            parent_entries,
            entries,
            git_is_repo: false,
            git_root_uri: String::new(),
            git_branch: String::new(),
            git_staged: 0,
            git_modified: 0,
            git_untracked: 0,
            preview,
        })
    }

    pub fn entry_preview(
        &self,
        raw_uri: &str,
        is_dir: bool,
        show_hidden: bool,
        max_lines: i64,
        max_bytes: i64,
    ) -> Result<RemoteEntryPreview, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let leaf_name = Path::new(&uri.remote_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&uri.remote_path)
            .to_string();
        if let RemoteBackend::LavaServer(client) = &sess.backend {
            match client.call(
                "fs/entry_preview",
                JsonValue::obj([
                    ("path", JsonValue::Str(uri.remote_path.clone())),
                    ("is_dir", JsonValue::Bool(is_dir)),
                    ("show_hidden", JsonValue::Bool(show_hidden)),
                    ("max_lines", JsonValue::Int(max_lines.clamp(5, 500))),
                    ("max_bytes", JsonValue::Int(max_bytes.clamp(512, 131_072))),
                ]),
                Duration::from_secs(8),
            ) {
                Ok(res) => return Ok(Self::decode_entry_preview_json(&uri.prefix, &res)),
                Err(err) if !err.contains("-32601") && !err.contains("Method not found") => {
                    return Err(err);
                }
                Err(_) => {}
            }
        }
        if is_dir {
            let children = self.readdir(raw_uri, show_hidden)?;
            let total = children.len() as i64;
            let capped = children
                .into_iter()
                .take(80)
                .map(|c| (c.name, c.is_dir, false, c.size))
                .collect();
            Ok(RemoteEntryPreview {
                kind: "dir".to_string(),
                target_uri: uri.to_canonical_uri(),
                target_name: leaf_name,
                total_count: total,
                total_size: 0,
                lines: Vec::new(),
                child_entries: capped,
            })
        } else {
            let pv = self.read_preview_window(raw_uri, 0, max_lines, max_bytes)?;
            Ok(RemoteEntryPreview {
                kind: if pv.is_binary {
                    "binary".to_string()
                } else {
                    "text".to_string()
                },
                target_uri: uri.to_canonical_uri(),
                target_name: leaf_name,
                total_count: pv.total_lines,
                total_size: pv.total_size,
                lines: pv.lines,
                child_entries: Vec::new(),
            })
        }
    }

    pub fn git_summary(&self, raw_uri: &str) -> Result<RemoteGitSummary, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        if let RemoteBackend::LavaServer(client) = &sess.backend {
            match client.call(
                "git/summary",
                JsonValue::obj([("cwd", JsonValue::Str(uri.remote_path.clone()))]),
                Duration::from_secs(8),
            ) {
                Ok(res) => {
                    let root_raw = res.get_str("root").unwrap_or("");
                    let root = if root_raw.is_empty() {
                        String::new()
                    } else {
                        format!("{}{}", uri.prefix, normalize_posix_path(root_raw))
                    };
                    return Ok(RemoteGitSummary {
                        is_repo: res
                            .get("is_repo")
                            .and_then(JsonValue::as_bool)
                            .unwrap_or(false),
                        root,
                        branch: res.get_str("branch").unwrap_or("").to_string(),
                        staged_count: res.get_i64("staged_count").unwrap_or(0),
                        modified_count: res.get_i64("modified_count").unwrap_or(0),
                        untracked_count: res.get_i64("untracked_count").unwrap_or(0),
                    });
                }
                Err(err) if !err.contains("-32601") && !err.contains("Method not found") => {
                    return Err(err);
                }
                Err(_) => {}
            }
        }
        let st = self.git_status(raw_uri)?;
        Ok(RemoteGitSummary {
            is_repo: st.is_repo,
            root: st.root,
            branch: st.branch,
            staged_count: st.staged_count,
            modified_count: st.modified_count,
            untracked_count: st.untracked_count,
        })
    }

    pub fn git_file_hunks(
        &self,
        raw_cwd_uri: &str,
        file_uri: &str,
        override_lines: Option<&[String]>,
    ) -> Result<(bool, Vec<RemoteGitHunk>), String> {
        let (sess, cwd_u) = self.ensure_session(raw_cwd_uri)?;
        let clean_file = if is_remote_uri(file_uri) {
            parse_remote_uri(file_uri)?.remote_path
        } else {
            file_uri.to_string()
        };
        if let RemoteBackend::LavaServer(client) = &sess.backend {
            let mut pairs = vec![
                ("cwd", JsonValue::Str(cwd_u.remote_path.clone())),
                ("path", JsonValue::Str(clean_file)),
            ];
            if let Some(lines) = override_lines {
                pairs.push(("has_override", JsonValue::Bool(true)));
                pairs.push((
                    "lines",
                    JsonValue::Array(lines.iter().map(|s| JsonValue::Str(s.clone())).collect()),
                ));
            } else {
                pairs.push(("has_override", JsonValue::Bool(false)));
            }
            match client.call(
                "git/file_hunks",
                JsonValue::obj(pairs),
                Duration::from_secs(10),
            ) {
                Ok(res) => {
                    let tracked = res
                        .get("tracked")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false);
                    let mut hunks = Vec::new();
                    if let Some(arr) = res.get("hunks").and_then(JsonValue::as_array) {
                        for h in arr {
                            hunks.push(RemoteGitHunk {
                                old_start: h.get_i64("old_start").unwrap_or(0),
                                old_count: h.get_i64("old_count").unwrap_or(0),
                                new_start: h.get_i64("new_start").unwrap_or(0),
                                new_count: h.get_i64("new_count").unwrap_or(0),
                                kind: h.get_str("kind").unwrap_or("change").to_string(),
                            });
                        }
                    }
                    return Ok((tracked, hunks));
                }
                Err(err) if !err.contains("-32601") && !err.contains("Method not found") => {
                    return Err(err);
                }
                Err(_) => {}
            }
        }
        let Some(head_lines) = self.git_head_lines(raw_cwd_uri, file_uri, "HEAD")? else {
            return Ok((false, Vec::new()));
        };
        let cur_lines = match override_lines {
            Some(l) => l.to_vec(),
            None => self.read_lines(file_uri)?.0,
        };
        // Minimal prefix/suffix + coalesced hunk fallback when server lacks `git/file_hunks`.
        let mut pfx = 0usize;
        while pfx < head_lines.len() && pfx < cur_lines.len() && head_lines[pfx] == cur_lines[pfx] {
            pfx += 1;
        }
        if pfx == head_lines.len() && pfx == cur_lines.len() {
            return Ok((true, Vec::new()));
        }
        let mut oe = head_lines.len();
        let mut ne = cur_lines.len();
        while oe > pfx && ne > pfx && head_lines[oe - 1] == cur_lines[ne - 1] {
            oe -= 1;
            ne -= 1;
        }
        let oc = (oe - pfx) as i64;
        let nc = (ne - pfx) as i64;
        let kind = if oc == 0 {
            "add"
        } else if nc == 0 {
            "delete"
        } else {
            "change"
        };
        Ok((
            true,
            vec![RemoteGitHunk {
                old_start: pfx as i64,
                old_count: oc,
                new_start: pfx as i64,
                new_count: nc,
                kind: kind.to_string(),
            }],
        ))
    }

    pub fn scan_todos(&self, raw_uri: &str, limit: i64) -> Result<Vec<RemoteTodoItem>, String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        let cap = limit.clamp(1, 2000);
        if let RemoteBackend::LavaServer(client) = &sess.backend {
            match client.call(
                "search/todos",
                JsonValue::obj([
                    ("root", JsonValue::Str(uri.remote_path.clone())),
                    ("limit", JsonValue::Int(cap)),
                ]),
                Duration::from_secs(20),
            ) {
                Ok(res) => {
                    let mut out = Vec::new();
                    if let Some(arr) = res.get("items").and_then(JsonValue::as_array) {
                        for it in arr {
                            let Some(fp) = it.get_str("file_path") else {
                                continue;
                            };
                            let rpath = if fp.starts_with('/') {
                                normalize_posix_path(fp)
                            } else {
                                normalize_posix_path(&format!("{}/{fp}", uri.remote_path))
                            };
                            out.push((
                                format!("{}{}", uri.prefix, rpath),
                                it.get_i64("row").unwrap_or(0).max(0),
                                it.get_i64("col").unwrap_or(0).max(0),
                                it.get_str("tag").unwrap_or("TODO").to_string(),
                                it.get_str("text").unwrap_or("").to_string(),
                            ));
                        }
                    }
                    return Ok(out);
                }
                Err(err) if !err.contains("-32601") && !err.contains("Method not found") => {
                    return Err(err);
                }
                Err(_) => {}
            }
        }
        let mut out = Vec::new();
        for tag in ["TODO", "FIXME", "HACK", "WARN", "PERF", "NOTE", "BUG"] {
            if let Ok(hits) = self.search_grep_ext(raw_uri, tag, true, "", 100) {
                for (furi, row, col, txt) in hits.1 {
                    out.push((furi, row, col, tag.to_string(), txt.trim().to_string()));
                    if out.len() >= cap as usize {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn workspace_replace(
        &self,
        raw_uri: &str,
        query: &str,
        replacement: &str,
        case_sensitive: bool,
        file_filter: &str,
    ) -> Result<(i64, i64), String> {
        let (sess, uri) = self.ensure_session(raw_uri)?;
        if let RemoteBackend::LavaServer(client) = &sess.backend {
            let res = client.call(
                "search/replace",
                JsonValue::obj([
                    ("root", JsonValue::Str(uri.remote_path.clone())),
                    ("query", JsonValue::Str(query.to_string())),
                    ("replacement", JsonValue::Str(replacement.to_string())),
                    ("case_sensitive", JsonValue::Bool(case_sensitive)),
                    ("file_filter", JsonValue::Str(file_filter.to_string())),
                ]),
                Duration::from_secs(30),
            )?;
            return Ok((
                res.get_i64("files_changed").unwrap_or(0),
                res.get_i64("total_replacements").unwrap_or(0),
            ));
        }
        Err("remote search/replace requires lava-server (:RemoteInstallServer)".to_string())
    }

    fn execute_async_op(&self, op: AsyncRemoteOp) -> Result<AsyncRemoteResult, String> {
        match op {
            AsyncRemoteOp::SearchFiles {
                uri,
                query,
                include_hidden,
                include_dirs,
                limit,
            } => self
                .search_files_ranked(&uri, &query, include_hidden, include_dirs, limit)
                .map(AsyncRemoteResult::SearchFiles),
            AsyncRemoteOp::SearchGrep {
                uri,
                pattern,
                case_sensitive,
                file_filter,
                limit,
            } => self
                .search_grep_ext(&uri, &pattern, case_sensitive, &file_filter, limit)
                .map(|(total_matched, hits)| AsyncRemoteResult::SearchGrep {
                    total_matched,
                    hits,
                }),
            AsyncRemoteOp::ReadPreview {
                uri,
                start_row,
                max_lines,
                max_bytes,
            } => self
                .read_preview_window(&uri, start_row, max_lines, max_bytes)
                .map(AsyncRemoteResult::ReadPreview),
            AsyncRemoteOp::DirSnapshot {
                uri,
                show_hidden,
                filter_query,
                sort_by,
                sort_reverse,
                focus_name,
                preferred_row,
                include_parent,
                include_preview,
            } => self
                .dir_snapshot(
                    &uri,
                    show_hidden,
                    &filter_query,
                    &sort_by,
                    sort_reverse,
                    &focus_name,
                    preferred_row,
                    include_parent,
                    include_preview,
                )
                .map(|snap| AsyncRemoteResult::DirSnapshot(Box::new(snap))),
            AsyncRemoteOp::EntryPreview {
                uri,
                is_dir,
                show_hidden,
                max_lines,
                max_bytes,
            } => self
                .entry_preview(&uri, is_dir, show_hidden, max_lines, max_bytes)
                .map(AsyncRemoteResult::EntryPreview),
            AsyncRemoteOp::GitSummary { uri } => {
                self.git_summary(&uri).map(AsyncRemoteResult::GitSummary)
            }
            AsyncRemoteOp::GitFileHunks {
                cwd_uri,
                file_uri,
                override_lines,
            } => self
                .git_file_hunks(&cwd_uri, &file_uri, override_lines.as_deref())
                .map(|(tracked, hunks)| AsyncRemoteResult::GitFileHunks { tracked, hunks }),
            AsyncRemoteOp::GitBlameLine {
                cwd_uri,
                file_uri,
                row,
            } => self
                .git_blame_line(&cwd_uri, &file_uri, row)
                .map(AsyncRemoteResult::GitBlameLine),
            AsyncRemoteOp::ScanTodos { root_uri, limit } => self
                .scan_todos(&root_uri, limit)
                .map(AsyncRemoteResult::ScanTodos),
            AsyncRemoteOp::WorkspaceReplace {
                root_uri,
                query,
                replacement,
                case_sensitive,
                file_filter,
            } => self
                .workspace_replace(
                    &root_uri,
                    &query,
                    &replacement,
                    case_sensitive,
                    &file_filter,
                )
                .map(
                    |(files_changed, total_replacements)| AsyncRemoteResult::WorkspaceReplace {
                        files_changed,
                        total_replacements,
                    },
                ),
            AsyncRemoteOp::ReaddirSimple { uri, show_hidden } => self
                .readdir(&uri, show_hidden)
                .map(AsyncRemoteResult::ReaddirSimple),
        }
    }

    /// Submit a non-blocking remote operation into `slot_key` with latest-wins coalescing.
    /// Returns immediately in O(1) without blocking the local UI fiber!
    pub fn submit_async(self: &Arc<Self>, slot_key: String, op: AsyncRemoteOp) -> u64 {
        let (seq, spawn_worker) = {
            let mut guard = self
                .async_slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let slot = guard.entry(slot_key.clone()).or_default();
            slot.latest_seq = slot.latest_seq.wrapping_add(1);
            let seq = slot.latest_seq;
            slot.pending = Some((seq, op));
            slot.completed = None;
            if slot.worker_running {
                (seq, false)
            } else {
                slot.worker_running = true;
                self.active_async_workers.fetch_add(1, Ordering::SeqCst);
                (seq, true)
            }
        };

        if spawn_worker {
            let mgr = Arc::clone(self);
            thread::spawn(move || {
                loop {
                    let next_job = {
                        let mut guard = mgr
                            .async_slots
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let Some(slot) = guard.get_mut(&slot_key) else {
                            mgr.active_async_workers.fetch_sub(1, Ordering::SeqCst);
                            break;
                        };
                        match slot.pending.take() {
                            Some(job) => job,
                            None => {
                                slot.worker_running = false;
                                mgr.active_async_workers.fetch_sub(1, Ordering::SeqCst);
                                break;
                            }
                        }
                    };

                    let (run_seq, run_op) = next_job;
                    let t0 = Instant::now();
                    let outcome = mgr.execute_async_op(run_op);
                    let elapsed_ms = t0.elapsed().as_millis() as i64;

                    let should_wake = {
                        let mut guard = mgr
                            .async_slots
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if let Some(slot) = guard.get_mut(&slot_key) {
                            if slot.pending.is_some() {
                                // A newer request arrived on this slot; loop immediately without
                                // publishing the superseded result!
                                continue;
                            }
                            if slot.latest_seq == run_seq {
                                slot.completed = Some(AsyncCompletedItem {
                                    seq: run_seq,
                                    elapsed_ms,
                                    outcome,
                                });
                                slot.worker_running = false;
                                mgr.active_async_workers.fetch_sub(1, Ordering::SeqCst);
                                true
                            } else {
                                slot.worker_running = false;
                                mgr.active_async_workers.fetch_sub(1, Ordering::SeqCst);
                                false
                            }
                        } else {
                            mgr.active_async_workers.fetch_sub(1, Ordering::SeqCst);
                            false
                        }
                    };

                    if should_wake {
                        mgr.notify_waker();
                    }
                    break;
                }
            });
        }

        seq
    }

    pub fn poll_async(&self, slot_key: &str) -> AsyncPollOutcome {
        let mut guard = self
            .async_slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(slot) = guard.get_mut(slot_key) else {
            return AsyncPollOutcome::Idle;
        };
        if let Some(done) = slot.completed.take() {
            return match done.outcome {
                Ok(result) => AsyncPollOutcome::Ready {
                    seq: done.seq,
                    elapsed_ms: done.elapsed_ms,
                    result,
                },
                Err(message) => AsyncPollOutcome::Error {
                    seq: done.seq,
                    message,
                },
            };
        }
        if slot.worker_running || slot.pending.is_some() {
            AsyncPollOutcome::Pending {
                seq: slot.latest_seq,
            }
        } else {
            AsyncPollOutcome::Idle
        }
    }

    pub fn cancel_async(&self, slot_key: &str) {
        let mut guard = self
            .async_slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(slot) = guard.get_mut(slot_key) {
            slot.latest_seq = slot.latest_seq.wrapping_add(1);
            slot.pending = None;
            slot.completed = None;
        }
    }

    pub fn active_async_count(&self) -> i64 {
        let workers = self.active_async_workers.load(Ordering::SeqCst);
        if workers > 0 {
            return workers as i64;
        }
        let Ok(guard) = self.async_slots.lock() else {
            return 0;
        };
        guard
            .values()
            .filter(|s| s.worker_running || s.pending.is_some() || s.completed.is_some())
            .count() as i64
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
