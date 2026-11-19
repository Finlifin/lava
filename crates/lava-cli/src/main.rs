//! Command-line entrypoint for Lava: Single-VM Nessa Programmable Interactive Environment.
//!
//! Orchestrates workspace/extension discovery, optional user `init.ns` composition into the
//! single Nessa VM, decoupled GTK4 UI thread execution (`lava-gtk`), headless keystroke
//! automation, and offscreen PNG frame snapshots.

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;

use lava_gtk::{
    GtkAppConfig, get_system_clipboard, is_display_available, measure_monospace_font,
    render_frame_to_png, run_gtk_app_with_final_size, set_system_clipboard,
};
use lava_proto::{EventQueue, FrameSlot, UiEvent};
use lava_vm::{
    BootOptions, HostCallbacks, HostConfig, LavaWorkspace, PackageOrigin, check_workspace,
    discover_extensions, run_compiled_artifact, run_entry_package,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Subcommand {
    Launch,
    Check,
    ListExtensions,
    RunPackage(PathBuf),
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliOptions {
    subcommand: Subcommand,
    workspace_dir: Option<PathBuf>,
    extra_ext_dirs: Vec<PathBuf>,
    init_script: Option<PathBuf>,
    no_init: bool,
    entry_pkg: String,
    headless: bool,
    headless_keys: String,
    dump_frame_png: Option<PathBuf>,
    width: i32,
    height: i32,
    width_overridden: bool,
    height_overridden: bool,
    startup_files: Vec<String>,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            subcommand: Subcommand::Launch,
            workspace_dir: None,
            extra_ext_dirs: Vec::new(),
            init_script: None,
            no_init: false,
            entry_pkg: "lava.org/ext_app".to_string(),
            headless: false,
            headless_keys: String::new(),
            dump_frame_png: None,
            width: 1280,
            height: 800,
            width_overridden: false,
            height_overridden: false,
            startup_files: Vec::new(),
        }
    }
}

fn parse_cli_args<I, S>(args: I) -> Result<CliOptions, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let raw: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    let mut opts = CliOptions::default();
    let mut i = 0usize;
    let mut positional_only = false;

    while i < raw.len() {
        let arg = &raw[i];
        if positional_only {
            opts.startup_files.push(arg.clone());
            i += 1;
            continue;
        }
        match arg.as_str() {
            "--" => {
                positional_only = true;
                i += 1;
            }
            "-h" | "--help" | "help" => {
                opts.subcommand = Subcommand::Help;
                return Ok(opts);
            }
            "-V" | "--version" | "version" => {
                opts.subcommand = Subcommand::Version;
                return Ok(opts);
            }
            "check" if i == 0 => {
                opts.subcommand = Subcommand::Check;
                i += 1;
            }
            "list-extensions" | "extensions" if i == 0 => {
                opts.subcommand = Subcommand::ListExtensions;
                i += 1;
            }
            "run-package" if i == 0 => {
                let target = raw
                    .get(i + 1)
                    .ok_or_else(|| "missing <pkg_dir> after `run-package`".to_string())?;
                opts.subcommand = Subcommand::RunPackage(PathBuf::from(target));
                i += 2;
            }
            "--workspace" => {
                let dir = raw
                    .get(i + 1)
                    .ok_or_else(|| "missing directory after `--workspace`".to_string())?;
                opts.workspace_dir = Some(PathBuf::from(dir));
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
                let path = raw
                    .get(i + 1)
                    .ok_or_else(|| format!("missing script path after `{arg}`"))?;
                if path == "NONE" {
                    opts.no_init = true;
                    opts.init_script = None;
                } else {
                    opts.init_script = Some(PathBuf::from(path));
                }
                i += 2;
            }
            "--no-init" | "-q" => {
                opts.no_init = true;
                i += 1;
            }
            "--entry" => {
                let entry = raw
                    .get(i + 1)
                    .ok_or_else(|| "missing package name after `--entry`".to_string())?;
                opts.entry_pkg = entry.clone();
                i += 2;
            }
            "--headless" => {
                opts.headless = true;
                i += 1;
            }
            "--headless-keys" | "--keys" => {
                let keys = raw
                    .get(i + 1)
                    .ok_or_else(|| format!("missing key sequence after `{arg}`"))?;
                opts.headless_keys = keys.clone();
                i += 2;
            }
            "--dump-frame-png" => {
                let out = raw
                    .get(i + 1)
                    .ok_or_else(|| "missing output path after `--dump-frame-png`".to_string())?;
                opts.dump_frame_png = Some(PathBuf::from(out));
                i += 2;
            }
            "--width" => {
                let val = raw
                    .get(i + 1)
                    .ok_or_else(|| "missing integer after `--width`".to_string())?;
                let parsed: i32 = val
                    .parse()
                    .map_err(|_| format!("invalid `--width` integer: `{val}`"))?;
                if parsed < 64 {
                    return Err("`--width` must be at least 64".to_string());
                }
                opts.width = parsed;
                opts.width_overridden = true;
                i += 2;
            }
            "--height" => {
                let val = raw
                    .get(i + 1)
                    .ok_or_else(|| "missing integer after `--height`".to_string())?;
                let parsed: i32 = val
                    .parse()
                    .map_err(|_| format!("invalid `--height` integer: `{val}`"))?;
                if parsed < 64 {
                    return Err("`--height` must be at least 64".to_string());
                }
                opts.height = parsed;
                opts.height_overridden = true;
                i += 2;
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown option `{flag}` (try `lava --help`)"));
            }
            file => {
                opts.startup_files.push(file.to_string());
                i += 1;
            }
        }
    }

    Ok(opts)
}

fn print_usage() {
    println!(
        "\
Lava 0.1.0 — Single-VM Nessa Programmable Interactive System (GTK4 + Evil-Vim + Emacs Core)

USAGE:
    lava [OPTIONS] [FILES...]
    lava check [OPTIONS]
    lava list-extensions [OPTIONS]
    lava run-package <PKG_DIR> [OPTIONS]

OPTIONS:
    -u, --init <PATH>           Load user Nessa script (`init.ns`) into the shared VM
    -q, --no-init               Do not load default `~/.config/lava/init.ns`
        --ext-dir <DIR>         Add extra Extension package directory (repeatable)
        --workspace <DIR>       Override Lava workspace root directory
        --entry <PKG>           Entry Extension package (default: `lava.org/ext_app`)
        --headless              Run without opening a GTK4 window
        --headless-keys <KEYS>  Inject startup key sequence (e.g. \"iHello<Escape>:wq<Return>\")
        --dump-frame-png <PNG>  Render final committed RenderFrame to PNG via PangoCairo
        --width <PX>            Initial window width in pixels (default: 1280)
        --height <PX>           Initial window height in pixels (default: 800)
    -h, --help                  Show this help message
    -V, --version               Show version information"
    );
}

fn is_valid_workspace_root(candidate: &Path) -> bool {
    candidate
        .join("packages")
        .join("lava_core")
        .join("package.toml")
        .is_file()
}

fn resolve_workspace_root(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(dir) = explicit {
        if is_valid_workspace_root(dir) {
            return Ok(dir.to_path_buf());
        }
        return Err(format!(
            "specified `--workspace` directory `{}` does not contain `packages/lava_core/package.toml`",
            dir.display()
        ));
    }

    if let Ok(env_ws) = env::var("LAVA_WORKSPACE") {
        let candidate = PathBuf::from(env_ws);
        if is_valid_workspace_root(&candidate) {
            return Ok(candidate);
        }
    }

    if let Ok(cwd) = env::current_dir() {
        let mut cursor: Option<&Path> = Some(cwd.as_path());
        while let Some(dir) = cursor {
            if is_valid_workspace_root(dir) {
                return Ok(dir.to_path_buf());
            }
            cursor = dir.parent();
        }
    }

    if let Ok(exe_path) = env::current_exe() {
        let mut cursor = exe_path.parent();
        while let Some(dir) = cursor {
            if is_valid_workspace_root(dir) {
                return Ok(dir.to_path_buf());
            }
            let share_dir = dir.join("share").join("lava");
            if is_valid_workspace_root(&share_dir) {
                return Ok(share_dir);
            }
            cursor = dir.parent();
        }
    }

    for sys_dir in ["/usr/share/lava", "/usr/local/share/lava"] {
        let cand = Path::new(sys_dir);
        if is_valid_workspace_root(cand) {
            return Ok(cand.to_path_buf());
        }
    }

    let manifest_anc = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(ws) = manifest_anc.parent().and_then(Path::parent)
        && is_valid_workspace_root(ws)
    {
        return Ok(ws.to_path_buf());
    }

    Err(
        "unable to locate Lava workspace root; pass `--workspace <DIR>` or set `LAVA_WORKSPACE`"
            .to_string(),
    )
}

fn resolve_user_init_script(opts: &CliOptions) -> Result<Option<PathBuf>, String> {
    if let Some(explicit) = &opts.init_script {
        if !explicit.is_file() {
            return Err(format!(
                "user init script does not exist: {}",
                explicit.display()
            ));
        }
        return Ok(Some(explicit.clone()));
    }
    if opts.no_init {
        return Ok(None);
    }
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        let candidate = PathBuf::from(xdg).join("lava").join("init.ns");
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }
    if let Ok(appdata) = env::var("APPDATA")
        && !appdata.is_empty()
    {
        let candidate = PathBuf::from(appdata).join("lava").join("init.ns");
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }
    if let Ok(home) = env::var("HOME").or_else(|_| env::var("USERPROFILE"))
        && !home.is_empty()
    {
        let candidate = PathBuf::from(home)
            .join(".config")
            .join("lava")
            .join("init.ns");
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn resolve_window_state_path() -> Option<PathBuf> {
    if let Ok(explicit) = env::var("LAVA_WINDOW_STATE_FILE")
        && !explicit.trim().is_empty()
    {
        return Some(PathBuf::from(explicit.trim()));
    }
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("lava").join("window_state.txt"));
    }
    if let Ok(appdata) = env::var("APPDATA")
        && !appdata.is_empty()
    {
        return Some(PathBuf::from(appdata).join("lava").join("window_state.txt"));
    }
    if let Ok(home) = env::var("HOME").or_else(|_| env::var("USERPROFILE"))
        && !home.is_empty()
    {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("lava")
                .join("window_state.txt"),
        );
    }
    None
}

fn parse_saved_window_size(raw: &str) -> Option<(i32, i32)> {
    let mut width: Option<i32> = None;
    let mut height: Option<i32> = None;
    let mut remember = true;
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(v) = trimmed.strip_prefix("width=") {
            width = v.trim().parse::<i32>().ok();
        } else if let Some(v) = trimmed.strip_prefix("height=") {
            height = v.trim().parse::<i32>().ok();
        } else if let Some(v) = trimmed.strip_prefix("remember=") {
            remember = v.trim() != "false";
        }
    }
    if !remember {
        return None;
    }
    match (width, height) {
        (Some(w), Some(h)) if w >= 64 && h >= 64 => Some((w, h)),
        _ => None,
    }
}

fn load_saved_window_size() -> Option<(i32, i32)> {
    let path = resolve_window_state_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    parse_saved_window_size(&raw)
}

fn make_gtk_host_callbacks() -> HostCallbacks {
    HostCallbacks::new()
        .with_measure_font(measure_monospace_font)
        .with_clipboard_get(get_system_clipboard)
        .with_clipboard_set(set_system_clipboard)
}

fn run_cli(opts: CliOptions) -> Result<i32, String> {
    match &opts.subcommand {
        Subcommand::Help => {
            print_usage();
            return Ok(0);
        }
        Subcommand::Version => {
            println!("lava {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        _ => {}
    }

    let workspace_root = resolve_workspace_root(opts.workspace_dir.as_deref())?;

    match &opts.subcommand {
        Subcommand::ListExtensions => {
            let exts = discover_extensions(&workspace_root, &opts.extra_ext_dirs)?;
            println!(
                "Discovered {} extension(s) in {}:",
                exts.len(),
                workspace_root.display()
            );
            for ext in &exts {
                let origin_tag = match ext.origin {
                    PackageOrigin::CorePackage => "core",
                    PackageOrigin::BuiltinExtension => "builtin",
                    PackageOrigin::UserExtension => "user",
                };
                let deps = if ext.dependencies.is_empty() {
                    "none".to_string()
                } else {
                    ext.dependencies.join(", ")
                };
                println!(
                    "  - {} v{} [{}] (deps: {})",
                    ext.qualified_name, ext.version, origin_tag, deps
                );
                if !ext.description.is_empty() {
                    println!("      {}", ext.description);
                }
            }
            Ok(0)
        }
        Subcommand::Check => {
            let user_init = resolve_user_init_script(&opts)?;
            let boot_opts = BootOptions::new().with_entry_pkg(&opts.entry_pkg);
            let exts = check_workspace(
                &workspace_root,
                &opts.extra_ext_dirs,
                user_init.as_deref(),
                &boot_opts,
            )?;
            if let Some(init_path) = &user_init {
                println!(
                    "ok: verified {} extension(s) + user script `{}` in single-VM graph",
                    exts.len(),
                    init_path.display()
                );
            } else {
                println!(
                    "ok: verified {} extension(s) in single-VM graph",
                    exts.len()
                );
            }
            Ok(0)
        }
        Subcommand::RunPackage(pkg_dir) => {
            let frame_slot = Arc::new(FrameSlot::new());
            let event_queue = Arc::new(EventQueue::new());
            event_queue.push(UiEvent::Resize {
                width: opts.width,
                height: opts.height,
                scale_factor: 1,
            });
            let host_cfg = HostConfig::new(Arc::clone(&frame_slot), event_queue)
                .with_callbacks(make_gtk_host_callbacks())
                .with_script_args(opts.startup_files.clone());
            let code = run_entry_package(&workspace_root, pkg_dir, host_cfg)?;
            if let Some(png_path) = &opts.dump_frame_png {
                let latest = frame_slot.latest();
                render_frame_to_png(&latest, png_path)?;
            }
            Ok(code)
        }
        Subcommand::Launch => {
            let use_headless =
                opts.headless || (!opts.headless_keys.is_empty() && !is_display_available());

            let user_init = resolve_user_init_script(&opts)?;
            let boot_opts = BootOptions::new().with_entry_pkg(&opts.entry_pkg);

            let workspace = LavaWorkspace::new(&workspace_root)
                .with_extra_ext_dirs(opts.extra_ext_dirs.clone())
                .with_user_script(user_init);

            // Compile (or load content-addressed `.nsbc` cache for) the single-VM Nessa program
            // upfront so any syntax/type error reports cleanly before opening a native window.
            let artifact = workspace.compile_boot(&boot_opts)?;

            let (init_width, init_height) =
                if !use_headless && !opts.width_overridden && !opts.height_overridden {
                    load_saved_window_size().unwrap_or((opts.width, opts.height))
                } else {
                    (opts.width, opts.height)
                };

            let frame_slot = Arc::new(FrameSlot::new());
            let event_queue = Arc::new(EventQueue::new());
            event_queue.push(UiEvent::Resize {
                width: init_width,
                height: init_height,
                scale_factor: 1,
            });

            let callbacks = if use_headless && opts.dump_frame_png.is_none() {
                HostCallbacks::new()
            } else {
                make_gtk_host_callbacks()
            };
            let host_cfg = HostConfig::new(Arc::clone(&frame_slot), Arc::clone(&event_queue))
                .with_callbacks(callbacks)
                .with_script_args(opts.startup_files.clone())
                .with_boot_runtime(
                    opts.startup_files.clone(),
                    opts.headless_keys.clone(),
                    use_headless,
                );

            let exit_code = if use_headless {
                run_compiled_artifact(artifact, host_cfg)?
            } else {
                if !is_display_available() {
                    return Err(
                        "no GTK4 display server available (set DISPLAY / WAYLAND_DISPLAY / GDK_BACKEND=broadway, or pass `--headless --headless-keys ...`)"
                            .to_string(),
                    );
                }
                let eq_for_vm = Arc::clone(&event_queue);
                let vm_handle = thread::Builder::new()
                    .name("lava-nessa-vm".to_string())
                    .spawn(move || {
                        let res = run_compiled_artifact(artifact, host_cfg);
                        let code = match &res {
                            Ok(c) => *c,
                            Err(_) => 1,
                        };
                        eq_for_vm.request_quit(code);
                        res
                    })
                    .map_err(|err| format!("failed to spawn Nessa VM thread: {err}"))?;

                let final_window_size = Arc::new(Mutex::new(None));
                let gtk_cfg = GtkAppConfig {
                    initial_title: "Lava".to_string(),
                    initial_width: init_width,
                    initial_height: init_height,
                };
                let ui_res = run_gtk_app_with_final_size(
                    gtk_cfg,
                    Arc::clone(&frame_slot),
                    Arc::clone(&event_queue),
                    Some(Arc::clone(&final_window_size)),
                );
                // Ensure VM wakes up if the GTK loop exited first.
                if event_queue.quit_code().is_none() {
                    event_queue.request_quit(0);
                }
                let vm_res = vm_handle
                    .join()
                    .map_err(|_| "Nessa VM thread panicked".to_string())?;
                ui_res?;
                let exit_val = vm_res?;
                if let Some((win_w, win_h)) = *final_window_size
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    && let Some(state_path) = resolve_window_state_path()
                {
                    let remember = std::fs::read_to_string(&state_path)
                        .map(|raw| !raw.lines().any(|l| l.trim() == "remember=false"))
                        .unwrap_or(true);
                    if remember && win_w >= 64 && win_h >= 64 {
                        if let Some(parent) = state_path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let content = format!("width={win_w}\nheight={win_h}\nremember=true\n");
                        let _ = std::fs::write(&state_path, content);
                    }
                }
                exit_val
            };

            if let Some(png_path) = &opts.dump_frame_png {
                let latest = frame_slot.latest();
                render_frame_to_png(&latest, png_path)?;
            }

            Ok(exit_code)
        }
        Subcommand::Help | Subcommand::Version => Ok(0),
    }
}

fn bootstrap_bundled_environment() {
    #[cfg(windows)]
    {
        if env::var_os("HOME").is_none()
            && let Some(user_profile) = env::var_os("USERPROFILE")
            && !user_profile.is_empty()
        {
            // SAFETY: Invoked once at the very start of `main()` before any worker threads spawn.
            unsafe {
                env::set_var("HOME", user_profile);
            }
        }
    }

    if let Ok(exe) = env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        if env::var_os("LAVA_FONTS_DIR").is_none() {
            let bundled_fonts = exe_dir.join("fonts");
            if bundled_fonts.is_dir() {
                // SAFETY: Invoked once at the very start of `main()` before any worker threads spawn.
                unsafe {
                    env::set_var("LAVA_FONTS_DIR", &bundled_fonts);
                }
            }
        }

        let mut prepend_dirs: Vec<PathBuf> = Vec::new();
        let bundled_bin = exe_dir.join("bin");
        if bundled_bin.is_dir() {
            prepend_dirs.push(bundled_bin);
        }
        prepend_dirs.push(exe_dir.to_path_buf());

        let existing: Vec<PathBuf> = env::var_os("PATH")
            .map(|p| env::split_paths(&p).collect())
            .unwrap_or_default();
        for dir in existing {
            if !prepend_dirs.iter().any(|d| d == &dir) {
                prepend_dirs.push(dir);
            }
        }
        if let Ok(joined) = env::join_paths(prepend_dirs) {
            // SAFETY: Invoked once at the very start of `main()` before any worker threads spawn.
            unsafe {
                env::set_var("PATH", joined);
            }
        }
    }
}

fn main() -> ExitCode {
    bootstrap_bundled_environment();
    let args: Vec<String> = env::args().skip(1).collect();
    match parse_cli_args(args).and_then(run_cli) {
        Ok(code) => ExitCode::from((code.clamp(0, 255)) as u8),
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_launch_flags_init_headless_and_files() {
        let parsed = parse_cli_args([
            "--init",
            "examples/init.ns",
            "--ext-dir",
            "/tmp/my_exts",
            "--headless",
            "--headless-keys",
            "iHello<Escape>:wq<Return>",
            "--dump-frame-png",
            "/tmp/out.png",
            "--width",
            "1440",
            "--height",
            "900",
            "README.md",
            "src/lib.ns",
        ])
        .expect("valid cli args");

        assert_eq!(parsed.subcommand, Subcommand::Launch);
        assert_eq!(
            parsed.init_script.as_deref(),
            Some(Path::new("examples/init.ns"))
        );
        assert_eq!(parsed.extra_ext_dirs, vec![PathBuf::from("/tmp/my_exts")]);
        assert!(parsed.headless);
        assert_eq!(parsed.headless_keys, "iHello<Escape>:wq<Return>");
        assert_eq!(
            parsed.dump_frame_png.as_deref(),
            Some(Path::new("/tmp/out.png"))
        );
        assert_eq!(parsed.width, 1440);
        assert_eq!(parsed.height, 900);
        assert_eq!(parsed.startup_files, vec!["README.md", "src/lib.ns"]);
    }

    #[test]
    fn cli_parses_subcommands_and_rejects_invalid_flags() {
        let chk = parse_cli_args(["check", "-q"]).expect("check subcommand");
        assert_eq!(chk.subcommand, Subcommand::Check);
        assert!(chk.no_init);

        let lst = parse_cli_args(["list-extensions"]).expect("list-extensions");
        assert_eq!(lst.subcommand, Subcommand::ListExtensions);

        let none_init = parse_cli_args(["-u", "NONE"]).expect("-u NONE");
        assert!(none_init.no_init);
        assert!(none_init.init_script.is_none());

        assert!(parse_cli_args(["--unknown-flag"]).is_err());
        assert!(parse_cli_args(["--width", "20"]).is_err());
    }

    #[test]
    fn saved_window_size_parses_and_respects_remember_flag() {
        assert_eq!(
            parse_saved_window_size("width=1600\nheight=960\nremember=true\n"),
            Some((1600, 960))
        );
        assert_eq!(
            parse_saved_window_size("width=1600\nheight=960\nremember=false\n"),
            None
        );
        assert_eq!(
            parse_saved_window_size("width=32\nheight=960\nremember=true\n"),
            None
        );
        assert_eq!(parse_saved_window_size("invalid"), None);
    }
}
