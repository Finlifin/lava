//! Single-VM Host Runtime & Dynamic Workspace Composer for Lava.
//!
//! Wires `lava-proto` (`FrameSlot`, `EventQueue`, `RenderFrame`, `UiEvent`) to Nessa's
//! `interpreter::HostHandler`, provides non-blocking background job/timer execution, and
//! dynamically composes `packages/*`, `extensions/*`, `--ext-dir`, and user `init.ns`
//! scripts into a single Nessa compilation graph and VM instance.

pub mod composer;
pub mod host;
pub mod jobs;
pub mod wire;

pub use lava_proto;

pub use composer::{
    BootOptions, ComposedBootProgram, ExtensionPackageInfo, LavaWorkspace, PackageOrigin,
    check_workspace, compile_and_run_boot, discover_extensions, run_compiled_artifact,
    run_entry_package,
};
pub use host::{
    BootRuntimeArgs, ClipboardGetFn, ClipboardSetFn, HostCallbacks, HostConfig, HostWaitController,
    LavaHostHandler, MeasureFontFn, WaitingTaskEntry, drive_vm_loop,
};
pub use jobs::JobManager;
pub use wire::{
    draw_cmd_to_native, native_to_ui_event, pack_rgba, parse_commit_frame_args, parse_draw_cmd,
    ui_event_to_native, unpack_rgba,
};

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use interpreter::{HostCallOutcome, HostHandler, NativeValue};
    use lava_proto::{
        CaretShape, ColorRgba, DrawCmd, EventQueue, FontMetrics, FrameSlot, MouseAction,
        PointerStyle, UiEvent, mods,
    };
    use runtime::TaskId;

    use super::*;

    struct TempTestDir(PathBuf);

    impl TempTestDir {
        fn new(label: &str) -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(1);
            loop {
                let candidate = std::env::temp_dir().join(format!(
                    "lava-vm-test-{label}-{}-{}",
                    std::process::id(),
                    NEXT_ID.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&candidate) {
                    Ok(()) => return Self(candidate),
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(err) => panic!("failed to create temp test directory: {err}"),
                }
            }
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn lava_workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("lava workspace root")
            .to_path_buf()
    }

    fn copy_dir_recursive(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).expect("create destination directory");
        for entry in fs::read_dir(src).expect("read source directory") {
            let entry = entry.expect("dir entry");
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            if src_path.is_dir() {
                copy_dir_recursive(&src_path, &dst_path);
            } else {
                fs::copy(&src_path, &dst_path).expect("copy file");
            }
        }
    }

    /// Stage an isolated workspace containing real `packages/lava_sys`, `packages/lava_core`,
    /// and `extensions/ext_document` so parallel edits to unrelated extensions never perturb tests.
    fn stage_isolated_workspace(tmp_root: &Path) -> PathBuf {
        let src_ws = lava_workspace_root();
        let staged_ws = tmp_root.join("staged_ws");
        for rel in [
            "packages/lava_sys",
            "packages/lava_core",
            "packages/lava_accel",
            "packages/lava_remote",
            "extensions/ext_document",
        ] {
            copy_dir_recursive(&src_ws.join(rel), &staged_ws.join(rel));
        }
        staged_ws
    }

    #[test]
    fn wire_roundtrip_all_nine_ui_events_and_all_seven_draw_cmds() {
        let events = vec![
            UiEvent::Key {
                key: "a".to_string(),
                modifiers: mods::CTRL | mods::ALT,
                text: Some("a".to_string()),
            },
            UiEvent::Key {
                key: "Escape".to_string(),
                modifiers: 0,
                text: None,
            },
            UiEvent::MouseButton {
                action: MouseAction::Press,
                button: 1,
                x: 124.5,
                y: 310.25,
                modifiers: mods::SHIFT,
                press_count: 2,
            },
            UiEvent::MouseButton {
                action: MouseAction::Release,
                button: 3,
                x: 40.0,
                y: 80.0,
                modifiers: 0,
                press_count: 1,
            },
            UiEvent::MouseMove {
                x: 512.75,
                y: 256.125,
                modifiers: mods::SUPER,
            },
            UiEvent::MouseScroll {
                dx: -1.5,
                dy: 3.0,
                x: 200.0,
                y: 150.0,
                modifiers: mods::CTRL,
            },
            UiEvent::Resize {
                width: 1920,
                height: 1080,
                scale_factor: 2,
            },
            UiEvent::Focus { focused: true },
            UiEvent::CloseRequested,
            UiEvent::Tick {
                timer_id: 77,
                now_ms: 1_763_000_123_456,
            },
            UiEvent::AsyncJobDone {
                job_id: 9,
                ok: true,
                exit_code: 0,
                stdout: "hello\n".to_string(),
                stderr: String::new(),
            },
        ];

        for expected in &events {
            let native = ui_event_to_native(expected);
            let decoded = native_to_ui_event(&native).expect("decode wire UiEvent");
            assert_eq!(&decoded, expected);
        }

        let commands = vec![
            DrawCmd::ClipPush {
                x: 0.0,
                y: 24.0,
                w: 800.0,
                h: 576.0,
            },
            DrawCmd::ClipPop,
            DrawCmd::FillRect {
                x: 10.0,
                y: 20.0,
                w: 300.0,
                h: 40.0,
                color: ColorRgba::from_rgba(30, 30, 46, 255),
                radius: 6.0,
            },
            DrawCmd::StrokeRect {
                x: 12.0,
                y: 22.0,
                w: 296.0,
                h: 36.0,
                color: ColorRgba::from_rgba(137, 180, 250, 200),
                line_width: 1.5,
                radius: 4.0,
            },
            DrawCmd::Line {
                x1: 0.0,
                y1: 580.0,
                x2: 800.0,
                y2: 580.0,
                color: ColorRgba::from_rgba(69, 71, 90, 255),
                line_width: 1.0,
            },
            DrawCmd::TextRun {
                x: 16.0,
                y: 38.0,
                text: "fn main() -> Unit".to_string(),
                font_family: "JetBrains Mono".to_string(),
                font_size_pt: 13.5,
                color: ColorRgba::from_rgba(205, 214, 244, 255),
                bold: true,
                italic: false,
                underline: true,
            },
            DrawCmd::CursorCaret {
                x: 16.0,
                y: 24.0,
                w: 8.4,
                h: 19.0,
                shape: CaretShape::Beam,
                color: ColorRgba::from_rgba(245, 224, 220, 255),
            },
        ];

        let mut wire_cmds = Vec::with_capacity(commands.len());
        for expected in &commands {
            let native = draw_cmd_to_native(expected);
            let parsed = parse_draw_cmd(&native).expect("parse wire DrawCmd");
            assert_eq!(&parsed, expected);
            wire_cmds.push(native);
        }

        let bg = ColorRgba::from_rgba(24, 24, 37, 255);
        // Verify signed negative i64 bit pattern with high bit set (>= 0x8000_0000) also unpacks cleanly.
        let high_bit_color = ColorRgba::from_rgba(255, 128, 64, 255);
        let signed_negative = (high_bit_color.0 as i32) as i64;
        assert_eq!(unpack_rgba(signed_negative), high_bit_color);

        let frame = parse_commit_frame_args(&[
            NativeValue::Str("Lava Test Window".to_string()),
            NativeValue::I64(1440),
            NativeValue::I64(900),
            NativeValue::I64(pack_rgba(bg)),
            NativeValue::I64(PointerStyle::Text as i64),
            NativeValue::List(wire_cmds),
        ])
        .expect("parse_commit_frame_args");

        assert_eq!(frame.window.title, "Lava Test Window");
        assert_eq!(frame.window.width, 1440);
        assert_eq!(frame.window.height, 900);
        assert_eq!(frame.window.background, bg);
        assert_eq!(frame.window.pointer, PointerStyle::Text);
        assert_eq!(frame.commands, commands);
    }

    #[test]
    fn host_handler_file_io_clipboard_metrics_timer_and_job_cancellation() {
        let tmp = TempTestDir::new("host-ops");
        let frame_slot = Arc::new(FrameSlot::new());
        let event_queue = Arc::new(EventQueue::new());
        let clip_store = Arc::new(Mutex::new(String::from("seed")));
        let clip_r = Arc::clone(&clip_store);
        let clip_w = Arc::clone(&clip_store);

        let callbacks = HostCallbacks::new()
            .with_measure_font(|family, size| FontMetrics {
                cell_width: if family == "Iosevka" {
                    size * 0.5
                } else {
                    size * 0.6
                },
                cell_height: size * 1.4,
                ascent: size * 1.1,
                descent: size * 0.3,
            })
            .with_clipboard_get(move || clip_r.lock().unwrap().clone())
            .with_clipboard_set(move |s| *clip_w.lock().unwrap() = s.to_owned());

        let config = HostConfig::new(Arc::clone(&frame_slot), Arc::clone(&event_queue))
            .with_callbacks(callbacks);
        let (mut handler, _ctl, _) = LavaHostHandler::from_config(config);

        // 1. Font measurement & clipboard callbacks
        let HostCallOutcome::Return(NativeValue::List(m)) = handler.call(
            "ui_measure_font",
            &[
                NativeValue::Str("Iosevka".to_string()),
                NativeValue::F64(16.0),
            ],
            TaskId(0),
        ) else {
            panic!("expected font metrics list");
        };
        assert_eq!(wire::expect_f64(&m[0], "cw").unwrap(), 8.0);

        assert!(matches!(
            handler.call(
                "ui_clipboard_set",
                &[NativeValue::Str("copied-from-vm".to_string())],
                TaskId(0)
            ),
            HostCallOutcome::Return(NativeValue::Unit)
        ));
        let HostCallOutcome::Return(NativeValue::Str(clip_val)) =
            handler.call("ui_clipboard_get", &[], TaskId(0))
        else {
            panic!("expected clipboard string");
        };
        assert_eq!(clip_val, "copied-from-vm");

        // 2. Empty file, CRLF file, and fs_write_lines roundtrip
        let empty_path = tmp.path().join("empty.txt");
        fs::write(&empty_path, b"").unwrap();
        let HostCallOutcome::Return(NativeValue::List(empty_res)) = handler.call(
            "fs_read_lines",
            &[NativeValue::Str(empty_path.to_string_lossy().into_owned())],
            TaskId(0),
        ) else {
            panic!("expected fs_read_lines result");
        };
        assert!(!wire::expect_bool(&empty_res[1], "crlf").unwrap());
        let empty_lines = wire::expect_list(&empty_res[0], "lines").unwrap();
        assert_eq!(empty_lines.len(), 1);
        assert_eq!(wire::expect_str(&empty_lines[0], "line0").unwrap(), "");

        let crlf_path = tmp.path().join("crlf.txt");
        fs::write(&crlf_path, b"first\r\nsecond\r\n").unwrap();
        let HostCallOutcome::Return(NativeValue::List(crlf_res)) = handler.call(
            "fs_read_lines",
            &[NativeValue::Str(crlf_path.to_string_lossy().into_owned())],
            TaskId(0),
        ) else {
            panic!("expected crlf fs_read_lines result");
        };
        assert!(wire::expect_bool(&crlf_res[1], "crlf").unwrap());
        let crlf_lines = wire::expect_list(&crlf_res[0], "lines").unwrap();
        assert_eq!(crlf_lines.len(), 2);
        assert_eq!(wire::expect_str(&crlf_lines[0], "l0").unwrap(), "first");
        assert_eq!(wire::expect_str(&crlf_lines[1], "l1").unwrap(), "second");

        let out_path = tmp.path().join("written.txt");
        let HostCallOutcome::Return(NativeValue::I64(written_bytes)) = handler.call(
            "fs_write_lines",
            &[
                NativeValue::Str(out_path.to_string_lossy().into_owned()),
                NativeValue::List(vec![
                    NativeValue::Str("alpha".to_string()),
                    NativeValue::Str("beta".to_string()),
                ]),
                NativeValue::Bool(true),
            ],
            TaskId(0),
        ) else {
            panic!("expected fs_write_lines byte count");
        };
        assert_eq!(written_bytes, 11);
        assert_eq!(fs::read_to_string(&out_path).unwrap(), "alpha\nbeta\n");

        // 3. Schedule timer & cancel long-running background job
        let HostCallOutcome::Return(NativeValue::I64(timer_id)) =
            handler.call("sys_schedule_timer", &[NativeValue::I64(15)], TaskId(0))
        else {
            panic!("expected timer_id");
        };
        let tick_events = event_queue.wait_for_events(Some(Duration::from_secs(2)));
        assert!(
            tick_events.iter().any(
                |ev| matches!(ev, UiEvent::Tick { timer_id: tid, .. } if *tid == timer_id as u64)
            ),
            "expected Tick event, got {tick_events:?}"
        );

        let start = Instant::now();
        let HostCallOutcome::Return(NativeValue::I64(job_id)) = handler.call(
            "sys_spawn_job",
            &[
                NativeValue::Str("exec sleep 30".to_string()),
                NativeValue::Str(String::new()),
                NativeValue::Str(String::new()),
            ],
            TaskId(0),
        ) else {
            panic!("expected job_id");
        };
        thread::sleep(Duration::from_millis(30));
        let HostCallOutcome::Return(NativeValue::Bool(cancelled)) =
            handler.call("sys_cancel_job", &[NativeValue::I64(job_id)], TaskId(0))
        else {
            panic!("expected cancel bool");
        };
        assert!(cancelled);

        let job_events = event_queue.wait_for_events(Some(Duration::from_secs(3)));
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "cancelled job must terminate promptly"
        );
        assert!(
            job_events.iter().any(|ev| matches!(
                ev,
                UiEvent::AsyncJobDone {
                    job_id: jid,
                    ok: false,
                    ..
                } if *jid == job_id as u64
            )),
            "expected cancelled AsyncJobDone(ok=false), got {job_events:?}"
        );
    }

    #[test]
    fn single_vm_end_to_end_boot_concurrent_async_effect_wakeup_bg_job_frame_and_user_init() {
        let tmp = TempTestDir::new("single-vm-e2e");
        let staged_ws = stage_isolated_workspace(tmp.path());

        // Build an isolated extra extension directory containing a synthetic entry package
        // (`lava.org/ext_e2e_harness`) that exercises:
        //   1. Real `packages/lava_sys` + `packages/lava_core` + `extensions/ext_document`
        //   2. Concurrent Nessa `async effect` task running in the SAME VM while the main UI
        //      loop task is suspended inside `host_wait_events(2000)`!
        //   3. Cross-thread `UiEvent::Key` wakeup from Rust
        //   4. Non-blocking `host_spawn_job("printf 'hello_bg\\n'", ...)` delivering `AsyncJobDone`
        //   5. User `init.ns` registering a custom Extension + Command into the shared
        //      `SystemKernel` and mutating the shared `EditorWorld` buffer!
        //   6. `host_commit_frame(...)` publishing `DrawCmd`s into `FrameSlot`!
        let extra_ext_root = tmp.path().join("extra_exts");
        let harness_dir = extra_ext_root.join("ext_e2e_harness");
        fs::create_dir_all(harness_dir.join("src")).unwrap();
        fs::write(
            harness_dir.join("package.toml"),
            r#"[package]
name = "ext_e2e_harness"
domain = "lava.org"
version = "0.1.0"
type = "lib"

[dependencies]
"lava.org/lava_sys" = "0.1.0"
"lava.org/lava_core" = "0.1.0"
"lava.org/ext_document" = "0.1.0"
"#,
        )
        .unwrap();

        fs::write(
            harness_dir.join("src/lib.ns"),
            r#"-- ext_e2e_harness: End-to-end single-VM verification harness.

use @lava_sys.{
    CaretShape, DrawCmd, PointerHint, RawUiEvent, WindowSpec, host_commit_frame,
    host_request_quit, host_spawn_job, host_wait_events, rgba
}
use @lava_core.{
    HostClipboardCap, LogCollectorCap, StatusCollectorCap, SystemKernel, ask_prompt
}
use @ext_document.{register_document_extension, require_editor_world}

pub async effect compute_in_background(steps: i64) -> i64

private global _async_task_completed: bool = false

pub fn boot_kernel() -> !String SystemKernel {
    let kernel = SystemKernel.new()
    register_document_extension(kernel)!
    kernel.activate_all()!
    kernel
}

fn await_key_and_bg_job(
    kernel: SystemKernel,
    startup_files: List,
    headless_keys: String,
) -> #compute_in_background !String i64 {
    -- Spawn a concurrent Nessa `async effect` task BEFORE suspending on `host_wait_events`!
    let bg_future = compute_in_background(250)#

    -- Signal to the external Rust test thread that the VM is live and entering `host_wait_events`.
    let ready_spec = WindowSpec {
        title: "Lava Booting",
        width: 1024,
        height: 768,
        bg_rgba: rgba(24, 24, 37, 255),
        pointer: PointerHint.Default,
    }
    let _ = host_commit_frame(ready_spec, List())

    -- Suspend the main UI task waiting for an external `UiEvent::Key` from another Rust thread.
    -- While suspended here, the VM scheduler MUST run `bg_future` concurrently in the same VM!
    var got_key = ""
    while got_key == "" {
        let batch = host_wait_events(5000)
        for item in batch {
            item.as(RawUiEvent) match {
                RawUiEvent.Key(k, _mods, _txt) => {
                    got_key = k
                },
                _ => (),
            }
        }
    }

    if not _async_task_completed {
        return error "concurrent async effect task did not run while main task was suspended"
    }
    let async_sum = bg_future.await
    if async_sum != 31125 {
        return error ("unexpected async sum: " ++ async_sum.to_string())
    }

    -- Spawn a real non-blocking background OS process (`printf 'hello_bg\n'`) and receive AsyncJobDone.
    let job_id = host_spawn_job("printf 'hello_bg\n'", "", "")
    var job_out = ""
    while job_out == "" {
        let batch = host_wait_events(2000)
        for item in batch {
            item.as(RawUiEvent) match {
                RawUiEvent.AsyncJobDone(jid, ok, code, stdout, stderr) => {
                    if jid == job_id {
                        if not ok or code != 0 {
                            return error ("background job failed: " ++ stderr)
                        }
                        job_out = stdout.trim()
                    }
                },
                _ => (),
            }
        }
    }

    -- Invoke the custom command registered by user `init.ns` inside the shared `SystemKernel`!
    let log_cap = LogCollectorCap.new()
    let status_cap = StatusCollectorCap.new()
    let clip_cap = HostClipboardCap {}
    let summary = [got_key, job_out, startup_files.get(0.as(usize)).as(String), headless_keys].join("|")
    let spec? = kernel.get_command("user-stamp-buffer") else {
        return error "expected user-stamp-buffer command from user init.ns"
    }
    let act = spec.action
    let raw: !String Any = (act(kernel, [summary]) # {
        ask_prompt(_req, _k) => {
            let fallback: !String Any = null
            fallback
        },
    }).use(log_cap).use(status_cap).use(clip_cap)
    let _ = raw!

    let world = require_editor_world(kernel)!
    let line0 = world.active_buffer().line_at(0)

    -- Commit a RenderFrame containing DrawCmds to `FrameSlot`!
    let cmds = List()
    cmds.push(DrawCmd.ClipPush(0.0, 0.0, 1024.0, 768.0))
    cmds.push(DrawCmd.FillRect(0.0, 0.0, 1024.0, 28.0, rgba(30, 30, 46, 255), 0.0))
    cmds.push(DrawCmd.TextRun(12.0, 20.0, line0, "Monospace", 14.0, rgba(166, 227, 161, 255), true, false, false))
    cmds.push(DrawCmd.CursorCaret(12.0, 4.0, 8.4, 19.0, CaretShape.Block, rgba(245, 224, 220, 255)))
    cmds.push(DrawCmd.ClipPop)

    let spec = WindowSpec {
        title: "Lava Single-VM: " ++ line0,
        width: 1024,
        height: 768,
        bg_rgba: rgba(24, 24, 37, 255),
        pointer: PointerHint.Text,
    }
    let gen = host_commit_frame(spec, cmds)
    if gen < 1 {
        return error "expected positive frame generation"
    }

    host_request_quit(0)
    0
}

pub fn run_session(
    kernel: SystemKernel,
    startup_files: List,
    headless_keys: String,
) -> !String i64 {
    await_key_and_bg_job(kernel, startup_files, headless_keys) # {
        async compute_in_background(steps) => {
            var total: i64 = 0
            for i in 0..steps {
                total += i
            }
            _async_task_completed = true
            total
        },
    }
}
"#,
        )
        .unwrap();

        // Write a standalone user `init.ns` file (NO `package.toml`!) that imports `@lava_core`
        // and `@ext_document` directly, registers a custom Extension + Command into `SystemKernel`,
        // and writes into the active `EditorWorld` buffer when invoked.
        let user_init_path = tmp.path().join("init.ns");
        fs::write(
            &user_init_path,
            r#"use @lava_core.{ExtId, InteractiveEffects, SystemKernel, status_emit}
use @ext_document.require_editor_world

pub fn configure(kernel: SystemKernel) -> !String Unit {
    let uid = ExtId("user.local/custom_init_ext")
    kernel.register_extension(
        uid,
        "user.local",
        "custom_init_ext",
        "0.1.0",
        "User init.ns extension sharing SystemKernel and EditorWorld.",
        [ExtId("lava.org/ext_document")],
        |k: SystemKernel| -> !String Unit {
            k.register_command(
                "user-stamp-buffer",
                uid,
                "Stamp active buffer from user init.ns command.",
                |kern: SystemKernel, args: List| -> #InteractiveEffects !String Any {
                    let world = require_editor_world(kern)!
                    let payload = args.get(0.as(usize)).as(String)
                    let buf = world.active_buffer()
                    buf.set_lines(["USER_INIT:" ++ payload])
                    status_emit("Stamped by user init.ns")#
                    ()
                },
            )
            ()
        },
        |_k: SystemKernel| -> !String Unit { () },
    )!
    ()
}
"#,
        )
        .unwrap();

        let workspace = LavaWorkspace::new(&staged_ws)
            .with_extra_ext_dirs(vec![extra_ext_root.clone()])
            .with_user_script(Some(user_init_path.clone()));

        // Verify extension discovery sees both builtin `ext_document` and extra `ext_e2e_harness`.
        let discovered = workspace
            .discover_extensions()
            .expect("discover extensions");
        assert!(
            discovered
                .iter()
                .any(|e| e.qualified_name == "lava.org/ext_document"
                    && e.origin == PackageOrigin::BuiltinExtension)
        );
        assert!(
            discovered
                .iter()
                .any(|e| e.qualified_name == "lava.org/ext_e2e_harness"
                    && e.origin == PackageOrigin::UserExtension)
        );

        let boot_opts = BootOptions::new()
            .with_entry_pkg("lava.org/ext_e2e_harness")
            .with_startup_files(vec!["demo.ns".to_string()])
            .with_headless_keys("<Space>f");

        // Verify `check_boot` succeeds cleanly before execution.
        workspace.check_boot(&boot_opts).expect("check_boot");

        let frame_slot = Arc::new(FrameSlot::new());
        let event_queue = Arc::new(EventQueue::new());

        // Spawn a separate Rust producer thread that waits until the VM has booted and
        // committed its initial "Lava Booting" frame (so compilation time never races),
        // pauses briefly so the main VM task suspends in `host_wait_events(5000)` and lets
        // the concurrent `async effect` task finish, then pushes `UiEvent::Key` to wake it!
        let fs_observer = Arc::clone(&frame_slot);
        let eq_sender = Arc::clone(&event_queue);
        let sender_thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            while fs_observer.latest().generation == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            thread::sleep(Duration::from_millis(40));
            eq_sender.push(UiEvent::Key {
                key: "WakeKey".to_string(),
                modifiers: 0,
                text: None,
            });
        });

        let host_config = HostConfig::new(Arc::clone(&frame_slot), Arc::clone(&event_queue));
        let exit_code = workspace
            .compile_and_run_boot(&boot_opts, host_config)
            .expect("single-VM boot execution must succeed");
        sender_thread.join().unwrap();

        assert_eq!(exit_code, 0);

        let committed = frame_slot.latest();
        assert_eq!(
            committed.generation, 2,
            "expected committed frame generation == 2"
        );
        assert_eq!(
            committed.window.title,
            "Lava Single-VM: USER_INIT:WakeKey|hello_bg|demo.ns|<Space>f"
        );
        assert_eq!(committed.window.width, 1024);
        assert_eq!(committed.window.height, 768);
        assert_eq!(committed.window.pointer, PointerStyle::Text);
        assert_eq!(committed.commands.len(), 5);
        assert!(matches!(
            &committed.commands[2],
            DrawCmd::TextRun { text, bold: true, .. }
                if text == "USER_INIT:WakeKey|hello_bg|demo.ns|<Space>f"
        ));
    }

    #[test]
    fn run_entry_package_exe_and_wait_events_timeout_expiry() {
        let tmp = TempTestDir::new("entry-exe");
        let staged_ws = stage_isolated_workspace(tmp.path());

        let exe_dir = tmp.path().join("mini_exe");
        fs::create_dir_all(exe_dir.join("src")).unwrap();
        fs::write(
            exe_dir.join("package.toml"),
            r#"[package]
name = "mini_exe"
domain = "lava.org"
version = "0.1.0"
type = "exe"

[dependencies]
"lava.org/lava_sys" = "0.1.0"
"#,
        )
        .unwrap();
        fs::write(
            exe_dir.join("src/main.ns"),
            r#"use @lava_sys.{DrawCmd, PointerHint, WindowSpec, host_commit_frame, host_request_quit, host_wait_events, rgba}

fn main() {
    -- Timeout expiry with zero external events must wake cleanly and return empty List.
    let timed_out = host_wait_events(20)
    if timed_out.len() != 0 {
        host_request_quit(9)
        return
    }
    let cmds = List()
    cmds.push(DrawCmd.FillRect(0.0, 0.0, 320.0, 200.0, rgba(40, 50, 60, 255), 2.0))
    let spec = WindowSpec {
        title: "MiniExe",
        width: 320,
        height: 200,
        bg_rgba: rgba(10, 10, 15, 255),
        pointer: PointerHint.Pointer,
    }
    let _ = host_commit_frame(spec, cmds)
    host_request_quit(7)
}
"#,
        )
        .unwrap();

        let frame_slot = Arc::new(FrameSlot::new());
        let event_queue = Arc::new(EventQueue::new());
        let host_config = HostConfig::new(Arc::clone(&frame_slot), Arc::clone(&event_queue));

        let code = run_entry_package(&staged_ws, &exe_dir, host_config)
            .expect("run_entry_package must succeed");
        assert_eq!(code, 7);

        let frame = frame_slot.latest();
        assert_eq!(frame.generation, 1);
        assert_eq!(frame.window.title, "MiniExe");
        assert_eq!(frame.window.pointer, PointerStyle::Pointer);
        assert_eq!(frame.commands.len(), 1);
    }

    #[test]
    fn nsbc_cache_hit_invalidation_and_runtime_boot_override() {
        let tmp = TempTestDir::new("nsbc-cache");
        let staged_ws = stage_isolated_workspace(tmp.path());
        fs::create_dir_all(staged_ws.join("target")).unwrap();

        let ext_dir = staged_ws.join("extensions/ext_cache_probe");
        fs::create_dir_all(ext_dir.join("src")).unwrap();
        fs::write(
            ext_dir.join("package.toml"),
            r#"[package]
name = "ext_cache_probe"
domain = "lava.org"
version = "0.1.0"
type = "lib"

[dependencies]
"lava.org/lava_sys" = "0.1.0"
"lava.org/lava_core" = "0.1.0"
"#,
        )
        .unwrap();
        let src_v1 = r#"use @lava_sys.{DrawCmd, PointerHint, WindowSpec, host_commit_frame, host_request_quit, rgba}
use @lava_core.SystemKernel

pub fn build_kernel() -> SystemKernel = SystemKernel.new()

pub fn run_session(_kernel: SystemKernel, startup_files: List, headless_keys: String, headless: bool) -> !String i64 {
    let f0 = if startup_files.is_empty() { "none" } else { startup_files.get(0.as(usize)).as(String) }
    let tag = if headless { "H" } else { "W" }
    let spec = WindowSpec {
        title: "v1:" ++ f0 ++ ":" ++ headless_keys ++ ":" ++ tag,
        width: 200,
        height: 100,
        bg_rgba: rgba(0, 0, 0, 255),
        pointer: PointerHint.Default,
    }
    let _ = host_commit_frame(spec, List())
    host_request_quit(0)
    0
}
"#;
        fs::write(ext_dir.join("src/lib.ns"), src_v1).unwrap();

        let cache_dir = staged_ws.join("target/lava-nsbc-cache");
        let ws = LavaWorkspace::new(&staged_ws).with_cache_dir(&cache_dir);
        let opts = BootOptions::new().with_entry_pkg("lava.org/ext_cache_probe");

        // Cold compile populates `<cache_dir>/*.nsbc`.
        let _cold = ws.compile_boot(&opts).expect("cold compile_boot");
        let entries_v1: Vec<_> = fs::read_dir(&cache_dir)
            .expect("cache_dir created")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("nsbc"))
            .collect();
        assert_eq!(entries_v1.len(), 1);

        // Warm compile loads from `.nsbc` cache and applies `with_boot_runtime` overrides!
        let warm = ws.compile_boot(&opts).expect("warm compile_boot");
        let fs_a = Arc::new(FrameSlot::new());
        let eq_a = Arc::new(EventQueue::new());
        let cfg_a = HostConfig::new(Arc::clone(&fs_a), eq_a).with_boot_runtime(
            vec!["alpha.ns".to_string()],
            "jk",
            true,
        );
        let code_a = run_compiled_artifact(warm, cfg_a).expect("warm run");
        assert_eq!(code_a, 0);
        assert_eq!(fs_a.latest().window.title, "v1:alpha.ns:jk:H");

        // Modifying `.ns` source invalidates content fingerprint and compiles `v2`.
        let src_v2 = src_v1.replace("\"v1:\"", "\"v2:\"");
        fs::write(ext_dir.join("src/lib.ns"), src_v2).unwrap();
        let updated = ws.compile_boot(&opts).expect("recompiled v2");
        let fs_b = Arc::new(FrameSlot::new());
        let eq_b = Arc::new(EventQueue::new());
        let cfg_b = HostConfig::new(Arc::clone(&fs_b), eq_b).with_boot_runtime(
            vec!["beta.ns".to_string()],
            "ZZ",
            false,
        );
        let code_b = run_compiled_artifact(updated, cfg_b).expect("v2 run");
        assert_eq!(code_b, 0);
        assert_eq!(fs_b.latest().window.title, "v2:beta.ns:ZZ:W");
    }
}
