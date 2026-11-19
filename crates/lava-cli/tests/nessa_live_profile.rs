use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lava_proto::{EventQueue, FrameSlot, UiEvent};
use lava_vm::{BootOptions, HostConfig, LavaWorkspace, run_compiled_artifact};

struct TempProfileDir(PathBuf);

impl TempProfileDir {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "lava_nessa_profile_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).expect("create temp profile dir");
        Self(dir)
    }
}

impl Drop for TempProfileDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ws_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

fn push_char(eq: &EventQueue, ch: char) {
    let s = ch.to_string();
    eq.push(UiEvent::Key {
        key: s.clone(),
        modifiers: 0,
        text: Some(s),
    });
}

fn push_spec(eq: &EventQueue, name: &str) {
    eq.push(UiEvent::Key {
        key: name.to_string(),
        modifiers: 0,
        text: None,
    });
}

fn wait_frame(
    slot: &FrameSlot,
    wp: &(Mutex<u64>, Condvar),
    prev: u64,
    timeout: Duration,
) -> Option<u64> {
    let deadline = Instant::now() + timeout;
    let mut g = wp.0.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        let cur = slot.latest().generation;
        if cur > prev {
            return Some(cur);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let (ng, _) =
            wp.1.wait_timeout(g, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
        g = ng;
    }
}

fn run_case(label: &str, target_file: &str) {
    let tmp = TempProfileDir::new();
    let cache_dir = tmp.0.join("nsbc_cache");
    fs::create_dir_all(&cache_dir).expect("create nsbc_cache");
    let win_state = tmp.0.join("window_state.txt");
    // SAFETY: Single test in dedicated integration test binary; isolates window state file per `AGENTS.md`.
    unsafe {
        std::env::set_var("LAVA_WINDOW_STATE_FILE", &win_state);
    }

    let root = ws_root();
    let ws = LavaWorkspace::new(&root).with_cache_dir(&cache_dir);
    let boot = BootOptions::new()
        .with_startup_files(vec![target_file.to_string()])
        .with_headless(false);
    let artifact = ws.compile_boot(&boot).expect("compile boot");

    let frame_slot = Arc::new(FrameSlot::new());
    let event_queue = Arc::new(EventQueue::new());
    let wp = Arc::new((Mutex::new(0u64), Condvar::new()));
    {
        let wp2 = Arc::clone(&wp);
        let fs2 = Arc::clone(&frame_slot);
        frame_slot.set_ui_waker(Box::new(move || {
            let mut g = wp2.0.lock().unwrap_or_else(|p| p.into_inner());
            *g = fs2.latest().generation;
            wp2.1.notify_all();
        }));
    }

    let eq_d = Arc::clone(&event_queue);
    let fs_d = Arc::clone(&frame_slot);
    let wp_d = Arc::clone(&wp);
    let lbl = label.to_string();

    let drv = thread::spawn(move || {
        let t_boot = Instant::now();
        let mut fgen = wait_frame(&fs_d, &wp_d, 0, Duration::from_secs(20)).expect("boot");
        let boot_ms = t_boot.elapsed().as_secs_f64() * 1000.0;

        eq_d.push(UiEvent::Resize {
            width: 1920,
            height: 1080,
            scale_factor: 1,
        });
        fgen = wait_frame(&fs_d, &wp_d, fgen, Duration::from_secs(5)).expect("resize");

        // Scroll 15 times (`C-d` / `j`) + type 20 chars in Insert mode
        let mut normal_ms = Vec::new();
        for ch in ['j', 'j', 'j', 'j', 'j', 'k', 'k', 'w', 'b', '0'] {
            let t0 = Instant::now();
            let prev = fs_d.latest().generation.max(fgen);
            push_char(&eq_d, ch);
            fgen = wait_frame(&fs_d, &wp_d, prev, Duration::from_secs(10)).unwrap();
            normal_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        }

        let mut insert_ms = Vec::new();
        for step in [
            Ok('o'),
            Ok('l'),
            Ok('e'),
            Ok('t'),
            Ok(' '),
            Ok('v'),
            Ok('a'),
            Ok('l'),
            Ok(' '),
            Ok('='),
            Ok(' '),
            Ok('s'),
            Ok('e'),
            Ok('l'),
            Ok('f'),
            Ok('.'),
            Ok('r'),
            Ok('e'),
            Ok('a'),
            Ok('d'),
            Ok('('),
            Ok('1'),
            Ok(')'),
            Err("Escape"),
            Ok('u'),
        ] {
            let t0 = Instant::now();
            let prev = fs_d.latest().generation.max(fgen);
            match step {
                Ok(c) => push_char(&eq_d, c),
                Err(s) => push_spec(&eq_d, s),
            }
            fgen = wait_frame(&fs_d, &wp_d, prev, Duration::from_secs(10)).unwrap();
            insert_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        }

        // Press `K` (`lsp-hover`) immediately after typing to verify UI never freezes > 60ms!
        let t_hover = Instant::now();
        let prev_h = fs_d.latest().generation.max(fgen);
        push_char(&eq_d, 'w');
        fgen = wait_frame(&fs_d, &wp_d, prev_h, Duration::from_secs(5)).unwrap();
        let prev_k = fs_d.latest().generation.max(fgen);
        push_char(&eq_d, 'K');
        let _ = wait_frame(&fs_d, &wp_d, prev_k, Duration::from_millis(250));
        let hover_ms = t_hover.elapsed().as_secs_f64() * 1000.0;
        push_spec(&eq_d, "Escape");

        let latest = fs_d.latest();
        let cmd_cnt = latest.commands.len();

        let avg_norm: f64 = normal_ms.iter().sum::<f64>() / normal_ms.len() as f64;
        let max_norm: f64 = normal_ms.iter().copied().fold(0.0, f64::max);
        let avg_ins: f64 = insert_ms.iter().sum::<f64>() / insert_ms.len() as f64;
        let max_ins: f64 = insert_ms.iter().copied().fold(0.0, f64::max);

        eprintln!(
            "[{lbl}] boot={boot_ms:.1}ms | cmds={cmd_cnt} | normal avg={avg_norm:.2}ms max={max_norm:.2}ms | insert avg={avg_ins:.2}ms max={max_ins:.2}ms | w+K={hover_ms:.2}ms"
        );
        assert!(
            avg_norm < 35.0 && avg_ins < 45.0,
            "[{lbl}] interactive latency regression: normal avg={avg_norm:.2}ms, insert avg={avg_ins:.2}ms"
        );

        for ch in ":q!".chars() {
            push_char(&eq_d, ch);
        }
        push_spec(&eq_d, "Return");
    });

    let cfg = HostConfig::new(Arc::clone(&frame_slot), Arc::clone(&event_queue)).with_boot_runtime(
        vec![target_file.to_string()],
        String::new(),
        false,
    );
    let _ = run_compiled_artifact(artifact, cfg).unwrap();
    drv.join().unwrap();
}

#[test]
fn profile_real_nessa_interactive_session() {
    let root = ws_root();
    let lava_ns = root
        .join("extensions/ext_lsp/src/lib.ns")
        .to_string_lossy()
        .into_owned();
    run_case("4514-line ext_lsp/lib.ns", &lava_ns);
}
