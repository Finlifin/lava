//! Standalone Headless Lava Remote Server Daemon (`lava-server`).
//!
//! Boots the independent Server-Side Lava Collection (`server/packages/*` + `server/extensions/*`)
//! inside a single Nessa VM over `Content-Length`-framed JSON-RPC 2.0 stdio, and provides a direct
//! `--lsp-proxy <cwd> <cmd> [args...]` mode for zero-overhead server-side LSP execution (`clangd`,
//! `rust-analyzer`, `pyright`, `gopls`, etc.).

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;

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
            cursor = dir.parent();
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

fn build_server_boot_options(entry_pkg: &str) -> BootOptions {
    let short_mod = entry_pkg
        .split_once('/')
        .map(|(_, short)| short)
        .unwrap_or(entry_pkg);
    let main_src = format!(
        "use @{short_mod}\n\nfn main() -> Unit {{\n    let _ = {short_mod}.boot_and_run_stdio() ! {{ e => {{\n        println(\"lava-server fatal: \" ++ e.to_string())\n        ()\n    }} }}\n    ()\n}}\n"
    );
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
            let boot_opts = build_server_boot_options(&opts.entry_pkg);
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
            let boot_opts = build_server_boot_options(&opts.entry_pkg);
            let frame_slot = Arc::new(FrameSlot::new());
            let event_queue = Arc::new(EventQueue::new());
            let host_config = HostConfig::new(frame_slot, event_queue);
            ws.compile_and_run_boot(&boot_opts, host_config)
        }
        ServerMode::Help | ServerMode::Version | ServerMode::LspProxy { .. } => Ok(0),
    }
}

fn main() -> ExitCode {
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
