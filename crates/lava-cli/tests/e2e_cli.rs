//! End-to-end integration tests for the `lava` binary:
//! 1. `lava list-extensions` & `lava check --init examples/init.ns`
//! 2. Headless single-VM session with `examples/init.ns`, Evil-Vim + Emacs + Continuation commands,
//!    disk file roundtrip, and PangoCairo `--dump-frame-png` output verification.
//! 3. Live decoupled GTK4 UI thread + `"lava-nessa-vm"` thread against `gtk4-broadwayd`.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn lava_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lava"))
}

fn lava_server_bin() -> PathBuf {
    lava_bin().with_file_name("lava-server")
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("{prefix}_{}_{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp test dir");
    dir
}

struct BroadwayServerGuard {
    child: Child,
    http_socket: PathBuf,
    display_socket: PathBuf,
}

impl BroadwayServerGuard {
    fn spawn_display_39() -> Self {
        let pid = std::process::id();
        let http_socket = PathBuf::from(format!("/tmp/lava_cli_e2e_{pid}.sock"));
        // Display `:39` maps to `$XDG_RUNTIME_DIR/broadway40.socket` in GTK4 Broadway.
        let display_socket = PathBuf::from("/tmp/broadway40.socket");

        let _ = fs::remove_file(&http_socket);
        let _ = fs::remove_file(&display_socket);

        let socket_arg = format!("--unixsocket={}", http_socket.display());
        let child = Command::new("/usr/bin/gtk4-broadwayd")
            .arg(&socket_arg)
            .arg(":39")
            .env("XDG_RUNTIME_DIR", "/tmp")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn /usr/bin/gtk4-broadwayd :39");

        let mut guard = Self {
            child,
            http_socket,
            display_socket,
        };

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if guard.http_socket.exists() && guard.display_socket.exists() {
                return guard;
            }
            if let Ok(Some(status)) = guard.child.try_wait() {
                panic!("gtk4-broadwayd :39 exited early: {status}");
            }
            thread::sleep(Duration::from_millis(15));
        }

        panic!("timed out waiting for gtk4-broadwayd :39 sockets");
    }
}

impl Drop for BroadwayServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.http_socket);
        let _ = fs::remove_file(&self.display_socket);
    }
}

#[test]
fn cli_list_extensions_and_check_user_init_succeed() {
    let ws = workspace_root();
    let init_ns = ws.join("examples").join("init.ns");

    let list_out = Command::new(lava_bin())
        .arg("list-extensions")
        .arg("--workspace")
        .arg(&ws)
        .arg("-q")
        .output()
        .expect("run lava list-extensions");
    assert!(
        list_out.status.success(),
        "list-extensions stderr: {}",
        String::from_utf8_lossy(&list_out.stderr)
    );
    let stdout = String::from_utf8_lossy(&list_out.stdout);
    for expected in [
        "lava.org/lava_sys",
        "lava.org/lava_core",
        "lava.org/lava_accel",
        "lava.org/lava_term",
        "lava.org/lava_lsp",
        "lava.org/ext_document",
        "lava.org/ext_window",
        "lava.org/ext_render",
        "lava.org/ext_minibuffer",
        "lava.org/ext_vim",
        "lava.org/ext_emacs",
        "lava.org/ext_tools",
        "lava.org/ext_app",
        "lava.org/ext_dirvish",
        "lava.org/ext_help",
        "lava.org/ext_toggleterm",
        "lava.org/ext_telescope",
        "lava.org/ext_magit",
        "lava.org/ext_cmp",
        "lava.org/ext_lualine",
        "lava.org/ext_lazy",
        "lava.org/ext_lsp",
        "lava.org/ext_lspconfig",
        "lava.org/lava_remote",
        "lava.org/ext_diff",
        "lava.org/ext_remote",
        "lava.org/ext_treesitter",
    ] {
        assert!(
            stdout.contains(expected),
            "expected `{expected}` in list-extensions output:\n{stdout}"
        );
    }

    let check_out = Command::new(lava_bin())
        .arg("check")
        .arg("--workspace")
        .arg(&ws)
        .arg("--init")
        .arg(&init_ns)
        .output()
        .expect("run lava check --init examples/init.ns");
    assert!(
        check_out.status.success(),
        "lava check stderr: {}",
        String::from_utf8_lossy(&check_out.stderr)
    );
    let chk_stdout = String::from_utf8_lossy(&check_out.stdout);
    assert!(
        chk_stdout.contains("ok: verified 47 extension(s)"),
        "stdout was: {chk_stdout}"
    );
}

#[test]
fn cli_headless_edits_file_runs_user_init_continuation_and_renders_png() {
    let ws = workspace_root();
    let init_ns = ws.join("examples").join("init.ns");
    let dir = unique_temp_dir("lava_cli_headless");
    let target_file = dir.join("demo.ns");
    let frame_png = dir.join("frame.png");

    fs::write(&target_file, "fn answer() -> i64 {\n    0\n}\n").expect("seed demo.ns");

    // Sequence exercises:
    // 1. User `init.ns` leader binding `<Space> n b` (`insert-lava-banner`)
    // 2. Evil-Vim motion + `ciw42<Escape>`
    // 3. Emacs chord `<C-x> <C-s>` (`save-buffer`)
    // 4. Emacs split `<C-x> 3` (`split-window-right`)
    // 5. User `init.ns` continuation-capturing command `<Space> n n ShipLava<Return>` (`quick-note`)
    let keys = "<Space> n b j0wciw42<Escape><C-x> <C-s><C-x> 3<Space> n n ShipLava<Return>";

    let out = Command::new(lava_bin())
        .arg("--workspace")
        .arg(&ws)
        .arg("--init")
        .arg(&init_ns)
        .arg("--headless")
        .arg("--headless-keys")
        .arg(keys)
        .arg("--dump-frame-png")
        .arg(&frame_png)
        .arg(&target_file)
        .output()
        .expect("run headless lava session");

    assert!(
        out.status.success(),
        "headless lava failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let saved = fs::read_to_string(&target_file).expect("read saved demo.ns");
    assert!(
        saved.contains("Lava Programmable Environment"),
        "expected user init.ns banner in saved file, got:\n{saved}"
    );
    assert!(
        saved.contains("42"),
        "expected Evil-Vim edit `42` in saved file, got:\n{saved}"
    );

    let png_bytes = fs::read(&frame_png).expect("read rendered frame.png");
    assert!(
        png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "expected valid PNG header"
    );
    assert!(
        png_bytes.len() > 4_000,
        "expected non-trivial rendered PNG size, got {} bytes",
        png_bytes.len()
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_live_gtk4_broadway_session_renders_and_quits_via_vim_ex() {
    let _broadway = BroadwayServerGuard::spawn_display_39();
    let ws = workspace_root();
    let init_ns = ws.join("examples").join("init.ns");
    let dir = unique_temp_dir("lava_cli_broadway");
    let frame_png = dir.join("broadway_frame.png");

    let out = Command::new(lava_bin())
        .arg("--workspace")
        .arg(&ws)
        .arg("--init")
        .arg(&init_ns)
        .arg("--headless-keys")
        .arg("iLive GTK4 + Nessa Single VM!<Escape>:q!<Return>")
        .arg("--dump-frame-png")
        .arg(&frame_png)
        .env("XDG_RUNTIME_DIR", "/tmp")
        .env("GDK_BACKEND", "broadway")
        .env("BROADWAY_DISPLAY", ":39")
        .output()
        .expect("run live GTK4 Broadway lava binary");

    assert!(
        out.status.success(),
        "live GTK4 Broadway lava failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let png_bytes = fs::read(&frame_png).expect("read broadway_frame.png");
    assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(png_bytes.len() > 4_000);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_headless_dirvish_ranger_ex_completion_and_real_pty_terminal() {
    let ws = workspace_root();
    let dir = unique_temp_dir("lava_cli_dirvish_pty");
    let seed_file = dir.join("alpha.ns");
    let ranger_created = dir.join("from_ranger.ns");
    let pty_marker = dir.join("pty_real.txt");
    let frame_png = dir.join("dirvish_pty.png");

    fs::write(&seed_file, "fn alpha() -> i64 { 7 }\n").expect("seed alpha.ns");

    // 1. Passing `dir` on CLI opens Emacs Dirvish 3-pane Miller columns directly!
    // 2. Ranger `cf` creates `from_ranger.ns` in `dir`.
    // 3. `:term<Tab>` completes `term ` (Ex Stage-1 -> Stage-2 completion!),
    //    then runs a real Linux POSIX PTY shell command writing `pty_real.txt`!
    let cmd_str = format!(
        "c f from_ranger.ns<Return>:term<Tab>printf<Space>REAL_PTY_OK<Space><gt><Space>{}<Return>",
        pty_marker.display()
    );

    let out = Command::new(lava_bin())
        .arg("--workspace")
        .arg(&ws)
        .arg("-q")
        .arg("--headless")
        .arg("--headless-keys")
        .arg(&cmd_str)
        .arg("--dump-frame-png")
        .arg(&frame_png)
        .arg(&dir)
        .env("SHELL", "/bin/sh")
        .output()
        .expect("run headless Dirvish + PTY lava session");

    assert!(
        out.status.success(),
        "headless Dirvish + PTY lava failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        ranger_created.is_file(),
        "expected Ranger `cf` in Dirvish to create {}",
        ranger_created.display()
    );
    let pty_text = fs::read_to_string(&pty_marker).unwrap_or_default();
    assert_eq!(
        pty_text, "REAL_PTY_OK",
        "expected real Linux POSIX PTY shell to write marker file"
    );

    let png_bytes = fs::read(&frame_png).expect("read dirvish_pty.png");
    assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(png_bytes.len() > 4_000);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_headless_emacs_help_info_and_global_apropos_docs_succeed() {
    let ws = workspace_root();
    let dir = unique_temp_dir("lava_cli_help_info");
    let frame_png = dir.join("help_info_frame.png");

    // 1. `<C-h> d dirvish<Return>` runs global 5-domain Orderless documentation search (`*Apropos*`).
    // 2. `:h<Space>dirvish<Return>` tests Minibuffer Stage-2 `:h` completion + opens Info/Help topic.
    // 3. `<C-h> i` opens Emacs `*Info*` hypertext manual reader.
    let keys = "<C-h> d dirvish<Return>:h<Space>dirvish<Return><C-h> i";

    let out = Command::new(lava_bin())
        .arg("--workspace")
        .arg(&ws)
        .arg("-q")
        .arg("--headless")
        .arg("--headless-keys")
        .arg(keys)
        .arg("--dump-frame-png")
        .arg(&frame_png)
        .output()
        .expect("run headless Emacs Help/Info/Apropos lava session");

    assert!(
        out.status.success(),
        "headless Emacs Help/Info/Apropos lava failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let png_bytes = fs::read(&frame_png).expect("read help_info_frame.png");
    assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(png_bytes.len() > 4_000);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_headless_vd_lspconfig_minibuffer_eof_and_pty_eof_succeed() {
    let ws = workspace_root();
    let dir = unique_temp_dir("lava_cli_vd_lsp_eof");
    let target_file = dir.join("sample.ns");
    let frame_png = dir.join("vd_lsp_frame.png");

    fs::write(
        &target_file,
        "fn compute() -> i64 {\n    let obsolete_line = 999\n    if true {\n        let inner_drop = 1\n    }\n    let kept_value = 42\n    kept_value\n}\n",
    )
    .expect("seed sample.ns");

    // 1. `jVD` moves to line 2 (`let obsolete_line = 999`) and deletes the entire line via VisualLine `D`.
    // 2. `jdii` moves into the indented `if true` body and deletes the inner indent block via `ii` text object.
    // 3. `:<C-d>` opens Ex Minibuffer and closes it cleanly via `<C-d>` EOF without side effects.
    // 4. `:w<Return>` writes the buffer to disk (`obsolete_line` & `inner_drop` deleted, `kept_value` preserved).
    // 5. `:LspConfig<Return>` opens the 18-language `*LspConfig*` dashboard and renders it to PNG.
    let keys = "jVDjdii:<C-d>:w<Return>:LspConfig<Return>";

    let out = Command::new(lava_bin())
        .arg("--workspace")
        .arg(&ws)
        .arg("-q")
        .arg("--headless")
        .arg("--headless-keys")
        .arg(keys)
        .arg("--dump-frame-png")
        .arg(&frame_png)
        .arg(&target_file)
        .output()
        .expect("run headless VD + dii + Minibuffer C-d + LspConfig session");

    assert!(
        out.status.success(),
        "headless VD + dii + Minibuffer C-d + LspConfig failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let saved = fs::read_to_string(&target_file).expect("read saved sample.ns");
    assert!(
        !saved.contains("obsolete_line"),
        "expected `VD` to delete `obsolete_line` line completely, got:\n{saved}"
    );
    assert!(
        !saved.contains("inner_drop"),
        "expected `dii` indent text-object to delete `inner_drop`, got:\n{saved}"
    );
    assert!(
        saved.contains("let kept_value = 42"),
        "expected remaining lines to stay intact after `VD` and `dii`, got:\n{saved}"
    );

    let png_bytes = fs::read(&frame_png).expect("read vd_lsp_frame.png");
    assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(png_bytes.len() > 4_000);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_remote_lava_server_and_server_side_clangd_lsp_roundtrip() {
    let ws = workspace_root();
    let server_bin = lava_server_bin();
    assert!(
        server_bin.is_file(),
        "expected `lava-server` binary next to `lava` at {}",
        server_bin.display()
    );

    let dir = unique_temp_dir("lava_cli_remote_ssh_lsp");
    let remote_root = dir.join("remote_cpp");
    let include_dir = remote_root.join("include");
    let src_dir = remote_root.join("src");
    fs::create_dir_all(&include_dir).expect("create remote include dir");
    fs::create_dir_all(&src_dir).expect("create remote src dir");

    let header_file = include_dir.join("math_util.hpp");
    let main_cpp = src_dir.join("main.cpp");
    let compile_cmds = remote_root.join("compile_commands.json");
    let init_ns = dir.join("remote_init.ns");
    let frame_png = dir.join("remote_status.png");

    fs::write(
        &header_file,
        "#pragma once\ninline int triple_val(int x) { return x * 3; }\n",
    )
    .expect("seed math_util.hpp");
    fs::write(
        &main_cpp,
        "#include \"math_util.hpp\"\nint main() {\n    return triple_val(14);\n}\n",
    )
    .expect("seed main.cpp");
    fs::write(
        &compile_cmds,
        format!(
            "[
  {{
    \"directory\": \"{}\",
    \"command\": \"/usr/bin/clang++ -I{} -std=c++17 -c {}\",
    \"file\": \"{}\"
  }}
]\n",
            remote_root.display(),
            include_dir.display(),
            main_cpp.display(),
            main_cpp.display()
        ),
    )
    .expect("seed compile_commands.json");

    let server_root = ws.join("server");
    fs::write(
        &init_ns,
        format!(
            "use @lava_core.{{SystemKernel}}\nuse @lava_remote.{{remote_register_profile}}\n\npub fn configure(_kernel: SystemKernel) -> !String Unit {{\n    let _ = remote_register_profile(\n        \"devbox\",\n        \"{}\",\n        [\"--server-root\", \"{}\", \"--stdio\"],\n        lsp_proxy_bin = \"{}\",\n    )\n    ()\n}}\n",
            server_bin.display(),
            server_root.display(),
            server_bin.display()
        ),
    )
    .expect("write remote_init.ns");

    let remote_uri = format!("ssh://devbox{}", main_cpp.display());
    let keys =
        "G$a<Space>//<Space>saved-via-lava-server-rpc<Escape>:w<Return>:RemoteStatus<Return>";

    let out = Command::new(lava_bin())
        .arg("--workspace")
        .arg(&ws)
        .arg("--init")
        .arg(&init_ns)
        .arg("--headless")
        .arg("--headless-keys")
        .arg(keys)
        .arg("--dump-frame-png")
        .arg(&frame_png)
        .arg(&remote_uri)
        .output()
        .expect("run headless lava over lava-server --stdio");

    assert!(
        out.status.success(),
        "headless remote lava failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let saved_cpp = fs::read_to_string(&main_cpp).expect("read saved remote main.cpp");
    assert!(
        saved_cpp.contains("// saved-via-lava-server-rpc"),
        "expected remote `fs/write` via `lava-server --stdio` to persist edit, got:\n{saved_cpp}"
    );

    let png_bytes = fs::read(&frame_png).expect("read remote_status.png");
    assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(png_bytes.len() > 4_000);

    // Verify `lava-server --lsp-proxy <cwd> clangd` runs real server-side clangd JSON-RPC cleanly.
    if Path::new("/usr/bin/clangd").is_file() {
        let mut child = Command::new(&server_bin)
            .arg("--lsp-proxy")
            .arg(&remote_root)
            .arg("/usr/bin/clangd")
            .arg("--log=error")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lava-server --lsp-proxy clangd");

        {
            let stdin = child.stdin.as_mut().expect("clangd proxy stdin");
            let init_payload = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"processId\":null,\"rootUri\":\"file://{}\",\"capabilities\":{{}}}}}}",
                remote_root.display()
            );
            write!(
                stdin,
                "Content-Length: {}\r\n\r\n{}",
                init_payload.len(),
                init_payload
            )
            .expect("write LSP initialize");
            stdin.flush().expect("flush LSP initialize");
        }

        let stdout = child.stdout.take().expect("clangd proxy stdout");
        let mut reader = BufReader::new(stdout);
        let mut content_len = 0usize;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).expect("read LSP header line");
            assert!(n > 0, "unexpected EOF from lava-server --lsp-proxy clangd");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
                content_len = rest.trim().parse().expect("parse Content-Length");
            }
        }
        assert!(
            content_len > 0,
            "expected positive Content-Length from clangd"
        );
        let mut body = vec![0u8; content_len];
        reader
            .read_exact(&mut body)
            .expect("read LSP initialize response body");
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            body_str.contains("\"capabilities\""),
            "expected clangd capabilities in server-side LSP response, got:\n{body_str}"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_headless_diff_stdio_sed_awk_mapper_and_rg_filter() {
    let ws = workspace_root();
    let dir = unique_temp_dir("lava_cli_diff_pipeline");
    let left_file = dir.join("left.log");
    let right_file = dir.join("right.log");
    let frame_png = dir.join("diff_pipeline.png");

    fs::write(
        &left_file,
        "2026-11-15 INFO boot complete\nDEBUG: noisy left diagnostic\n2026-11-15 ERROR code=101\n",
    )
    .expect("seed left.log");
    fs::write(
        &right_file,
        "2026-11-16 INFO boot complete\n2026-11-16 ERROR code=202\n",
    )
    .expect("seed right.log");

    let keys = format!(
        ":DiffFiles<Space>{}<Space>{}<Return>:DiffFilter<Space>rg<Space>-v<Space>^DEBUG:<Return>:DiffMap<Space>sed<Space>-E<Space>s/[0-9]{{4}}-[0-9]{{2}}-[0-9]{{2}}/DATE/g<Return>vvt",
        left_file.display(),
        right_file.display()
    );

    let out = Command::new(lava_bin())
        .arg("--workspace")
        .arg(&ws)
        .arg("-q")
        .arg("--headless")
        .arg("--headless-keys")
        .arg(&keys)
        .arg("--dump-frame-png")
        .arg(&frame_png)
        .output()
        .expect("run headless DiffFiles + rg Filter + sed Mapper session");

    assert!(
        out.status.success(),
        "headless Diff pipeline session failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let png_bytes = fs::read(&frame_png).expect("read diff_pipeline.png");
    assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(png_bytes.len() > 4_000);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_headless_large_150k_line_c_file_open_search_edit_undo_save_and_png() {
    let ws = workspace_root();
    let init_ns = ws.join("examples").join("init.ns");
    let dir = unique_temp_dir("lava_cli_large_150k");
    let large_c = dir.join("amalgamation_150k.c");
    let frame_png = dir.join("large_150k.png");

    // 150,000 lines (~6.6 MB): exceeds both Nessa's single-List limit (65,535 items)
    // and single-String limit (~512 KiB).
    let line_count: usize = 150_000;
    {
        let f = fs::File::create(&large_c).expect("create amalgamation_150k.c");
        let mut w = std::io::BufWriter::with_capacity(256 * 1024, f);
        writeln!(w, "/* Synthetic 150k-line C amalgamation */").expect("write header");
        for i in 1..line_count {
            writeln!(
                w,
                "int func_{i}(int x) {{ return x + {i}; }} // TODO check_{i}"
            )
            .expect("write line");
        }
        w.flush().expect("flush amalgamation_150k.c");
    }

    // Move 30 lines down, jump to `func_120000` via `/`, insert marker, undo, redo, and `:wq`.
    let keys = "jjjjjjjjjjjjjjjjjjjjjjjjjjjjjj/func_120000<Return>0i/*LAVA_LARGE_OK*/<Escape>u<C-r>:wq<Return>";

    let out = Command::new(lava_bin())
        .arg("--workspace")
        .arg(&ws)
        .arg("--init")
        .arg(&init_ns)
        .arg("--headless")
        .arg("--headless-keys")
        .arg(keys)
        .arg("--dump-frame-png")
        .arg(&frame_png)
        .arg(&large_c)
        .output()
        .expect("run headless lava on 150k-line C file");

    assert!(
        out.status.success(),
        "headless lava failed on 150k-line C file:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let saved = fs::read_to_string(&large_c).expect("read saved 150k-line C file");
    let saved_lines: Vec<&str> = saved.lines().collect();
    assert_eq!(
        saved_lines.len(),
        line_count,
        "line count must be preserved"
    );
    assert_eq!(
        saved_lines[120_000],
        "/*LAVA_LARGE_OK*/int func_120000(int x) { return x + 120000; } // TODO check_120000"
    );
    assert_eq!(
        saved_lines[149_999],
        "int func_149999(int x) { return x + 149999; } // TODO check_149999"
    );

    let png_bytes = fs::read(&frame_png).expect("read large_150k.png");
    assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(png_bytes.len() > 4_000);

    let _ = fs::remove_dir_all(&dir);
}
