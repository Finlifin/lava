//! Standalone Headless Lava Remote Server Daemon (`lava-server`).
//!
//! Boots the independent Server-Side Lava Collection (`server/packages/*` + `server/extensions/*`)
//! inside a single Nessa VM over `Content-Length`-framed JSON-RPC 2.0 stdio, and provides a direct
//! `--lsp-proxy <cwd> <cmd> [args...]` mode for zero-overhead server-side LSP execution (`clangd`,
//! `rust-analyzer`, `pyright`, `gopls`, etc.).

use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lava_lsp::which_binary;
use lava_proto::{EventQueue, FrameSlot};
use lava_vm::{BootOptions, HostConfig, LavaWorkspace, PackageOrigin, check_workspace};

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServerMode {
    Stdio,
    Check,
    ListExtensions,
    LspProxy {
        cwd: String,
        cmd: String,
        args: Vec<String>,
    },
    Help,
    Version,
}

#[derive(Debug, Clone)]
struct ServerCliOptions {
    mode: ServerMode,
    server_root: Option<PathBuf>,
    extra_ext_dirs: Vec<PathBuf>,
    init_script: Option<PathBuf>,
    entry_pkg: String,
}

impl Default for ServerCliOptions {
    fn default() -> Self {
        Self {
            mode: ServerMode::Stdio,
            server_root: None,
            extra_ext_dirs: Vec::new(),
            init_script: None,
            entry_pkg: "lava.server/srv_app".to_string(),
        }
    }
}

fn parse_args<I, S>(args: I) -> Result<ServerCliOptions, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let raw: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    let mut opts = ServerCliOptions::default();
    let mut i = 0usize;

    while i < raw.len() {
        let arg = &raw[i];
        match arg.as_str() {
            "-h" | "--help" | "help" => {
                opts.mode = ServerMode::Help;
                return Ok(opts);
            }
            "-V" | "--version" | "version" => {
                opts.mode = ServerMode::Version;
                return Ok(opts);
            }
            "--stdio" | "stdio" => {
                opts.mode = ServerMode::Stdio;
                i += 1;
            }
            "check" if i == 0 => {
                opts.mode = ServerMode::Check;
                i += 1;
            }
            "list-extensions" | "extensions" if i == 0 => {
                opts.mode = ServerMode::ListExtensions;
                i += 1;
            }
            "--lsp-proxy-b64" => {
                let b64 = raw
                    .get(i + 1)
                    .ok_or_else(|| "missing payload after `--lsp-proxy-b64`".to_string())?;
                let decoded = decode_urlsafe_b64(b64)?;
                let text = String::from_utf8(decoded)
                    .map_err(|_| "invalid UTF-8 in `--lsp-proxy-b64` payload".to_string())?;
                let mut parts = text.split('\0');
                let cwd = parts
                    .next()
                    .ok_or_else(|| "missing cwd in `--lsp-proxy-b64`".to_string())?
                    .to_string();
                let cmd = parts
                    .next()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "missing cmd in `--lsp-proxy-b64`".to_string())?
                    .to_string();
                let rest: Vec<String> = parts.map(ToString::to_string).collect();
                opts.mode = ServerMode::LspProxy {
                    cwd,
                    cmd,
                    args: rest,
                };
                return Ok(opts);
            }
            "--lsp-proxy" => {
                let cwd = raw
                    .get(i + 1)
                    .ok_or_else(|| "missing <cwd> after `--lsp-proxy`".to_string())?
                    .clone();
                let cmd = raw
                    .get(i + 2)
                    .ok_or_else(|| "missing <cmd> after `--lsp-proxy <cwd>`".to_string())?
                    .clone();
                let rest = if i + 3 <= raw.len() {
                    raw[i + 3..].to_vec()
                } else {
                    Vec::new()
                };
                opts.mode = ServerMode::LspProxy {
                    cwd,
                    cmd,
                    args: rest,
                };
                return Ok(opts);
            }
            "--server-root" | "--workspace" => {
                let dir = raw
                    .get(i + 1)
                    .ok_or_else(|| format!("missing directory after `{arg}`"))?;
                opts.server_root = Some(PathBuf::from(dir));
                i += 2;
            }
            "--ext-dir" => {
                let dir = raw
                    .get(i + 1)
                    .ok_or_else(|| "missing directory after `--ext-dir`".to_string())?;
                opts.extra_ext_dirs.push(PathBuf::from(dir));
                i += 2;
            }
            "--init" | "-u" => {
                let script = raw
                    .get(i + 1)
                    .ok_or_else(|| format!("missing script path after `{arg}`"))?;
                opts.init_script = Some(PathBuf::from(script));
                i += 2;
            }
            "--entry" => {
                let pkg = raw
                    .get(i + 1)
                    .ok_or_else(|| "missing package name after `--entry`".to_string())?;
                opts.entry_pkg = pkg.clone();
                i += 2;
            }
            other => {
                return Err(format!("unrecognized lava-server option: `{other}`"));
            }
        }
    }

    Ok(opts)
}

fn print_usage() {
    println!(
        "lava-server {} — Standalone Headless Lava Remote Server Collection

USAGE:
    lava-server --stdio [--server-root <DIR>] [--ext-dir <DIR>] [--init <FILE.ns>]
    lava-server check [--server-root <DIR>] [--ext-dir <DIR>]
    lava-server list-extensions [--server-root <DIR>]
    lava-server --lsp-proxy <CWD> <LSP_CMD> [LSP_ARGS...]

SUBCOMMANDS & MODES:
    --stdio                         Run JSON-RPC 2.0 server collection over stdin/stdout
    check                           Type-check & cache all `lava.server/*` packages and extensions
    list-extensions                 List discovered `lava.server/*` extensions
    --lsp-proxy <CWD> <CMD> [ARGS]  Launch server-side LSP daemon (`clangd`, `rust-analyzer`, ...)

OPTIONS:
        --server-root <DIR>         Override `server/` collection root directory
        --ext-dir <DIR>             Add extra server extension directory (repeatable)
    -u, --init <FILE.ns>            Load custom server Nessa init script into the VM
        --entry <PKG>               Entry server package (default: `lava.server/srv_app`)
    -h, --help                      Show this help message
    -V, --version                   Show version information",
        env!("CARGO_PKG_VERSION")
    );
}

fn decode_urlsafe_b64(input: &str) -> Result<Vec<u8>, String> {
    let mut vals = Vec::with_capacity(input.len());
    for b in input.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' | b'\r' | b'\n' => continue,
            _ => return Err(format!("invalid base64 byte 0x{b:02x}")),
        };
        vals.push(v);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        if chunk.len() < 2 {
            break;
        }
        out.push((chunk[0] << 2) | (chunk[1] >> 4));
        if chunk.len() >= 3 {
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
        }
        if chunk.len() >= 4 {
            out.push((chunk[2] << 6) | chunk[3]);
        }
    }
    Ok(out)
}

fn is_valid_server_root(dir: &Path) -> bool {
    dir.join("packages")
        .join("server_core")
        .join("package.toml")
        .is_file()
}

fn resolve_server_root(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(dir) = explicit {
        if is_valid_server_root(dir) {
            return Ok(dir.to_path_buf());
        }
        let nested = dir.join("server");
        if is_valid_server_root(&nested) {
            return Ok(nested);
        }
        return Err(format!(
            "specified `--server-root` directory `{}` does not contain `packages/server_core/package.toml`",
            dir.display()
        ));
    }

    if let Ok(env_root) = env::var("LAVA_SERVER_ROOT")
        && !env_root.trim().is_empty()
    {
        let cand = PathBuf::from(env_root);
        if is_valid_server_root(&cand) {
            return Ok(cand);
        }
    }

    if let Ok(cwd) = env::current_dir() {
        let mut cursor: Option<&Path> = Some(cwd.as_path());
        while let Some(dir) = cursor {
            if is_valid_server_root(dir) {
                return Ok(dir.to_path_buf());
            }
            let sub = dir.join("server");
            if is_valid_server_root(&sub) {
                return Ok(sub);
            }
            cursor = dir.parent();
        }
    }

    if let Ok(exe) = env::current_exe() {
        let mut cursor = exe.parent();
        while let Some(dir) = cursor {
            if is_valid_server_root(dir) {
                return Ok(dir.to_path_buf());
            }
            let sub = dir.join("server");
            if is_valid_server_root(&sub) {
                return Ok(sub);
            }
            let share_sub = dir.join("share").join("lava").join("server");
            if is_valid_server_root(&share_sub) {
                return Ok(share_sub);
            }
            cursor = dir.parent();
        }
    }

    for sys_dir in ["/usr/share/lava/server", "/usr/local/share/lava/server"] {
        let cand = Path::new(sys_dir);
        if is_valid_server_root(cand) {
            return Ok(cand.to_path_buf());
        }
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(repo_root) = manifest_dir.parent().and_then(Path::parent) {
        let srv = repo_root.join("server");
        if is_valid_server_root(&srv) {
            return Ok(srv);
        }
    }

    Err(
        "unable to locate Lava `server/` collection root; pass `--server-root <DIR>` or set `LAVA_SERVER_ROOT`"
            .to_string(),
    )
}

fn augmented_path_env() -> String {
    let cur = env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string());
    let home = env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        format!("/usr/local/bin:{cur}")
    } else {
        format!("{home}/.cargo/bin:{home}/.local/bin:/usr/local/bin:{cur}")
    }
}

fn run_lsp_proxy(cwd: &str, cmd_name: &str, args: &[String]) -> Result<i32, String> {
    let new_path = augmented_path_env();
    let resolved = if cmd_name.contains('/') || Path::new(cmd_name).is_absolute() {
        cmd_name.to_string()
    } else {
        let found = which_binary(cmd_name);
        if !found.is_empty() {
            found
        } else {
            new_path
                .split(':')
                .filter(|d| !d.is_empty())
                .map(|d| Path::new(d).join(cmd_name))
                .find(|p| p.is_file())
                .map(|p| p.to_string_lossy().into_owned())
                .ok_or_else(|| format!("server-side LSP binary `{cmd_name}` not found in PATH"))?
        }
    };

    let mut command = Command::new(&resolved);
    command.args(args).env("PATH", new_path);
    if !cwd.trim().is_empty() && Path::new(cwd).is_dir() {
        command.current_dir(cwd);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = command.exec();
        Err(format!(
            "failed to exec server-side LSP `{resolved}`: {err}"
        ))
    }

    #[cfg(not(unix))]
    {
        use std::process::Stdio;
        let status = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("failed to spawn server-side LSP `{resolved}`: {e}"))?;
        Ok(status.code().unwrap_or(1))
    }
}

fn generate_http_auth_token() -> String {
    let mut bytes = [0u8; 32];
    let mut filled = false;
    if let Ok(mut urandom) = File::open("/dev/urandom")
        && urandom.read_exact(&mut bytes).is_ok()
    {
        filled = true;
    }
    if !filled {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = u128::from(std::process::id());
        let seed_a = nanos ^ (pid << 64) ^ 0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C834_u128;
        let seed_b =
            nanos.rotate_left(37) ^ (pid << 17) ^ 0xBF58_476D_1CE4_E5B9_94D0_49BB_1331_11EB_u128;
        bytes[..16].copy_from_slice(&seed_a.to_le_bytes());
        bytes[16..].copy_from_slice(&seed_b.to_le_bytes());
    }
    let mut out = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn percent_decode(input: &str) -> String {
    let raw = input.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i] == b'%'
            && i + 2 < raw.len()
            && let (Some(hi), Some(lo)) = (
                (raw[i + 1] as char).to_digit(16),
                (raw[i + 2] as char).to_digit(16),
            )
        {
            out.push(((hi << 4) | lo) as u8);
            i += 3;
            continue;
        }
        out.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn expand_remote_server_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if (trimmed == "~" || trimmed.starts_with("~/"))
        && let Ok(home) = env::var("HOME")
        && !home.is_empty()
    {
        if trimmed == "~" {
            return PathBuf::from(home);
        }
        return Path::new(&home).join(&trimmed[2..]);
    }
    PathBuf::from(trimmed)
}

fn write_http_status(stream: &mut TcpStream, code: u16, reason: &str, msg: &str) {
    let body = msg.as_bytes();
    let hdr = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(hdr.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn handle_http_file_client(mut stream: TcpStream, expected_token: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
    let Ok(read_clone) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_clone);

    let mut req_line = String::new();
    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = req_line.split_whitespace();
    let Some(method) = parts.next() else {
        return;
    };
    let Some(target) = parts.next() else {
        return;
    };

    let mut auth_ok = false;
    let mut content_len: usize = 0;
    loop {
        let mut hdr = String::new();
        if reader.read_line(&mut hdr).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = hdr.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if key.eq_ignore_ascii_case("Authorization")
                && let Some(tok) = val.strip_prefix("Bearer ")
                && constant_time_eq(tok.trim().as_bytes(), expected_token.as_bytes())
            {
                auth_ok = true;
            } else if key.eq_ignore_ascii_case("Content-Length") {
                content_len = val.parse::<usize>().unwrap_or(0);
            }
        }
    }

    if !auth_ok {
        write_http_status(&mut stream, 403, "Forbidden", "invalid bearer token");
        return;
    }

    let Some(query) = target.strip_prefix("/lava-file?") else {
        write_http_status(&mut stream, 404, "Not Found", "unknown endpoint");
        return;
    };
    let mut raw_path = None;
    for kv in query.split('&') {
        if let Some(enc) = kv.strip_prefix("path=") {
            raw_path = Some(percent_decode(enc));
            break;
        }
    }
    let Some(path_str) = raw_path.filter(|s| !s.is_empty()) else {
        write_http_status(&mut stream, 400, "Bad Request", "missing path query param");
        return;
    };
    let target_path = expand_remote_server_path(&path_str);

    if method == "GET" {
        let mut file = match File::open(&target_path) {
            Ok(f) => f,
            Err(err) => {
                write_http_status(&mut stream, 404, "Not Found", &err.to_string());
                return;
            }
        };
        let meta = match file.metadata() {
            Ok(m) if m.is_file() => m,
            Ok(_) => {
                write_http_status(
                    &mut stream,
                    400,
                    "Bad Request",
                    "path is not a regular file",
                );
                return;
            }
            Err(err) => {
                write_http_status(&mut stream, 500, "Internal Server Error", &err.to_string());
                return;
            }
        };
        let len = meta.len();
        let hdr = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n"
        );
        if stream.write_all(hdr.as_bytes()).is_err() {
            return;
        }
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stream.write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = stream.flush();
    } else if method == "PUT" {
        if let Some(parent) = target_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let tmp_path = target_path.with_extension(format!("lava-http-tmp-{}", std::process::id()));
        let mut out_file = match File::create(&tmp_path) {
            Ok(f) => f,
            Err(err) => {
                write_http_status(&mut stream, 500, "Internal Server Error", &err.to_string());
                return;
            }
        };
        let mut remaining = content_len;
        let mut buf = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let to_read = remaining.min(buf.len());
            match reader.read(&mut buf[..to_read]) {
                Ok(0) => break,
                Ok(n) => {
                    if out_file.write_all(&buf[..n]).is_err() {
                        let _ = fs::remove_file(&tmp_path);
                        write_http_status(&mut stream, 500, "Internal Server Error", "write error");
                        return;
                    }
                    remaining -= n;
                }
                Err(err) => {
                    let _ = fs::remove_file(&tmp_path);
                    write_http_status(&mut stream, 500, "Internal Server Error", &err.to_string());
                    return;
                }
            }
        }
        let _ = out_file.flush();
        drop(out_file);
        if let Err(err) = fs::rename(&tmp_path, &target_path) {
            let _ = fs::remove_file(&tmp_path);
            write_http_status(&mut stream, 500, "Internal Server Error", &err.to_string());
            return;
        }
        write_http_status(&mut stream, 200, "OK", &content_len.to_string());
    } else {
        write_http_status(
            &mut stream,
            405,
            "Method Not Allowed",
            "expected GET or PUT",
        );
    }
}

/// Starts a background authenticated HTTP/1.1 streaming file server for >512 KiB transfers so
/// multi-hundred-thousand-line files never occupy or block the interactive SSH stdio JSON-RPC pipe.
fn start_http_file_server() -> Option<(u16, String)> {
    let bind_addr = env::var("LAVA_HTTP_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:0".to_string());
    let listener = TcpListener::bind(&bind_addr)
        .or_else(|_| TcpListener::bind("127.0.0.1:0"))
        .ok()?;
    let port = listener.local_addr().ok()?.port();
    let token = generate_http_auth_token();
    let token_bg = token.clone();
    thread::spawn(move || {
        for incoming in listener.incoming().flatten() {
            let tok = token_bg.clone();
            thread::spawn(move || {
                handle_http_file_client(incoming, &tok);
            });
        }
    });
    Some((port, token))
}

fn build_server_boot_options(entry_pkg: &str, http_port: u16, http_token: &str) -> BootOptions {
    let short_mod = entry_pkg
        .split_once('/')
        .map(|(_, short)| short)
        .unwrap_or(entry_pkg);
    let main_src = if http_port > 0 && !http_token.is_empty() && short_mod == "srv_app" {
        format!(
            "use @{short_mod}\n\nfn main() -> Unit {{\n    let _ = {short_mod}.boot_and_run_stdio_with_http({http_port}, \"{http_token}\") ! {{ e => {{\n        println(\"lava-server fatal: \" ++ e.to_string())\n        ()\n    }} }}\n    ()\n}}\n"
        )
    } else {
        format!(
            "use @{short_mod}\n\nfn main() -> Unit {{\n    let _ = {short_mod}.boot_and_run_stdio() ! {{ e => {{\n        println(\"lava-server fatal: \" ++ e.to_string())\n        ()\n    }} }}\n    ()\n}}\n"
        )
    };
    BootOptions::new()
        .with_entry_pkg(entry_pkg)
        .with_headless(true)
        .with_custom_main_source(main_src)
}

fn run_server(opts: ServerCliOptions) -> Result<i32, String> {
    match &opts.mode {
        ServerMode::Help => {
            print_usage();
            return Ok(0);
        }
        ServerMode::Version => {
            println!("lava-server {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        ServerMode::LspProxy { cwd, cmd, args } => {
            return run_lsp_proxy(cwd, cmd, args);
        }
        _ => {}
    }

    let server_root = resolve_server_root(opts.server_root.as_deref())?;
    let ws = LavaWorkspace::new(&server_root)
        .with_extra_ext_dirs(opts.extra_ext_dirs.clone())
        .with_user_script(opts.init_script.clone());

    match &opts.mode {
        ServerMode::ListExtensions => {
            let exts = ws.discover_extensions()?;
            println!(
                "Discovered {} server extension(s) in {}:",
                exts.len(),
                server_root.display()
            );
            for ext in &exts {
                let tag = match ext.origin {
                    PackageOrigin::CorePackage => "core",
                    PackageOrigin::BuiltinExtension => "server-builtin",
                    PackageOrigin::UserExtension => "server-user",
                };
                if ext.description.is_empty() {
                    println!("  - {} v{} [{}]", ext.qualified_name, ext.version, tag);
                } else {
                    println!(
                        "  - {} v{} [{}] — {}",
                        ext.qualified_name, ext.version, tag, ext.description
                    );
                }
            }
            Ok(0)
        }
        ServerMode::Check => {
            let boot_opts = build_server_boot_options(&opts.entry_pkg, 0, "");
            let exts = check_workspace(
                &server_root,
                &opts.extra_ext_dirs,
                opts.init_script.as_deref(),
                &boot_opts,
            )?;
            println!(
                "OK: server collection `{}` ({} extension(s)) compiled and cached cleanly.",
                server_root.display(),
                exts.len()
            );
            Ok(0)
        }
        ServerMode::Stdio => {
            let (http_port, http_token) = start_http_file_server().unwrap_or((0, String::new()));
            let boot_opts = build_server_boot_options(&opts.entry_pkg, http_port, &http_token);
            let frame_slot = Arc::new(FrameSlot::new());
            let event_queue = Arc::new(EventQueue::new());
            let host_config = HostConfig::new(frame_slot, event_queue);
            ws.compile_and_run_boot(&boot_opts, host_config)
        }
        ServerMode::Help | ServerMode::Version | ServerMode::LspProxy { .. } => Ok(0),
    }
}

fn main() -> ExitCode {
    if env::var_os("LAVA_SERVER_CACHE_DIR").is_none()
        && env::var_os("LAVA_CACHE_DIR").is_none()
        && let Ok(exe) = env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling_cache = dir.join("cache");
        if sibling_cache.is_dir() {
            // SAFETY: Set before spawning any worker threads in `lava-server`.
            unsafe {
                env::set_var("LAVA_CACHE_DIR", &sibling_cache);
            }
        }
    }

    let opts = match parse_args(env::args().skip(1)) {
        Ok(o) => o,
        Err(err) => {
            eprintln!("lava-server error: {err}");
            return ExitCode::from(2);
        }
    };

    match run_server(opts) {
        Ok(code) => ExitCode::from((code.clamp(0, 255)) as u8),
        Err(err) => {
            eprintln!("lava-server error: {err}");
            ExitCode::from(1)
        }
    }
}
