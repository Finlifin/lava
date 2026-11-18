//! Live headless GTK4 integration test against `gtk4-broadwayd`.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use lava_gtk::{
    GtkAppConfig, get_system_clipboard, is_display_available, run_gtk_app, set_system_clipboard,
};
use lava_proto::{
    CaretShape, ColorRgba, DrawCmd, EventQueue, FrameSlot, PointerStyle, RenderFrame, UiEvent,
    WindowConfig,
};

struct BroadwayDaemonGuard {
    child: Child,
    http_socket: PathBuf,
    display_socket: PathBuf,
}

impl BroadwayDaemonGuard {
    fn spawn() -> Self {
        let pid = std::process::id();
        let http_socket = PathBuf::from(format!("/tmp/lava_gtk_test_{pid}.sock"));
        // Display `:37` corresponds to `$XDG_RUNTIME_DIR/broadway38.socket` in GTK4 Broadway.
        let display_socket = PathBuf::from("/tmp/broadway38.socket");

        let _ = std::fs::remove_file(&http_socket);
        let _ = std::fs::remove_file(&display_socket);

        let socket_arg = format!("--unixsocket={}", http_socket.display());
        let child = Command::new("/usr/bin/gtk4-broadwayd")
            .arg(&socket_arg)
            .arg(":37")
            .env("XDG_RUNTIME_DIR", "/tmp")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn /usr/bin/gtk4-broadwayd");

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
                panic!("gtk4-broadwayd exited early with status {status}");
            }
            thread::sleep(Duration::from_millis(15));
        }

        panic!(
            "timed out waiting for gtk4-broadwayd sockets ({}, {})",
            guard.http_socket.display(),
            guard.display_socket.display()
        );
    }
}

impl Drop for BroadwayDaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.http_socket);
        let _ = std::fs::remove_file(&self.display_socket);
    }
}

#[test]
fn live_broadway_window_renders_frame_and_exits_cleanly() {
    let _daemon = BroadwayDaemonGuard::spawn();

    // SAFETY: This integration test crate contains a single test and mutates process environment
    // variables prior to initializing GTK/GLib or spawning worker threads.
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp");
        std::env::set_var("GDK_BACKEND", "broadway");
        std::env::set_var("BROADWAY_DISPLAY", ":37");
    }

    assert!(
        is_display_available(),
        "gtk_init_check should succeed against gtk4-broadwayd :37"
    );

    let frame_slot = Arc::new(FrameSlot::new());
    let event_queue = Arc::new(EventQueue::new());

    let vm_frame_slot = Arc::clone(&frame_slot);
    let vm_event_queue = Arc::clone(&event_queue);

    let vm_thread = thread::spawn(move || {
        // Poll event queue until GTK4 maps the Broadway surface and emits `UiEvent::Resize`.
        let mut saw_resize = false;
        let resize_deadline = Instant::now() + Duration::from_secs(2);
        while !saw_resize && Instant::now() < resize_deadline {
            for ev in vm_event_queue.wait_for_events(Some(Duration::from_millis(50))) {
                if let UiEvent::Resize { width, height, .. } = ev
                    && width > 0
                    && height > 0
                {
                    saw_resize = true;
                }
            }
        }

        let gen_id = vm_frame_slot.commit(RenderFrame {
            generation: 0,
            window: WindowConfig {
                title: "Lava Broadway Live Frame".to_string(),
                width: 640,
                height: 400,
                background: ColorRgba::from_rgba(30, 30, 46, 255),
                pointer: PointerStyle::Text,
                decorated: false,
            },
            commands: vec![
                DrawCmd::FillRect {
                    x: 16.0,
                    y: 16.0,
                    w: 608.0,
                    h: 368.0,
                    color: ColorRgba::from_rgba(49, 50, 68, 255),
                    radius: 6.0,
                },
                DrawCmd::TextRun {
                    x: 28.0,
                    y: 28.0,
                    text: "Broadway GTK4 + PangoCairo live test".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: ColorRgba::from_rgba(205, 214, 244, 255),
                    bold: false,
                    italic: false,
                    underline: false,
                },
                DrawCmd::CursorCaret {
                    x: 28.0,
                    y: 56.0,
                    w: 9.0,
                    h: 19.0,
                    shape: CaretShape::Block,
                    color: ColorRgba::from_rgba(245, 224, 220, 220),
                },
            ],
        });
        assert_eq!(gen_id, 1);

        set_system_clipboard("broadway-clipboard-payload");
        assert_eq!(get_system_clipboard(), "broadway-clipboard-payload");

        // Give the GLib main loop time to run the frame waker, redraw on Broadway, and sync clipboard.
        thread::sleep(Duration::from_millis(80));
        vm_event_queue.request_quit(0);
        saw_resize
    });

    run_gtk_app(
        GtkAppConfig {
            initial_title: "Lava Boot".to_string(),
            initial_width: 640,
            initial_height: 400,
        },
        Arc::clone(&frame_slot),
        Arc::clone(&event_queue),
    )
    .expect("run_gtk_app should run and exit cleanly on Broadway");

    let saw_resize = vm_thread.join().expect("VM thread should not panic");
    assert!(
        saw_resize,
        "expected initial UiEvent::Resize from GTK4 DrawingArea"
    );
    assert_eq!(event_queue.quit_code(), Some(0));
    assert_eq!(frame_slot.latest().generation, 1);
}
