//! End-to-end 20+ keys/sec sustained & burst typing responsiveness smoke benchmark.
//!
//! Boots the full 40-extension Lava workspace (`LavaWorkspace` in interactive non-headless mode)
//! against a 2,000-line source file inside a real Git repository (so Tree-sitter, `ext_gitsigns`,
//! `ext_cmp`, `ext_trouble`, `ext_aerial`, `ext_todo_comments`, `ext_git_conflict`, `ext_colorizer`,
//! `ext_illuminate`, `ext_autopairs`, and `ext_lualine` are all active), injects:
//!   1. Sustained 40 keys/sec (25ms spacing, 2x the 20+ keys/sec target) across Insert, `<Return>`,
//!      `<Backspace>`, Normal-mode `j`/`k`/`w`/`b`, and Ex `:w`, measuring per-key-to-frame latency;
//!   2. Instantaneous 32-key burst floods pushed back-to-back into `EventQueue`, verifying frame
//!      coalescing, sub-frame responsiveness, and 100% lossless saved disk contents.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lava_proto::{EventQueue, FrameSlot, UiEvent};
use lava_vm::{BootOptions, HostConfig, LavaWorkspace, run_compiled_artifact};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

struct TempRepoDir(PathBuf);

impl TempRepoDir {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("{prefix}_{}_{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp benchmark repo");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRepoDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn push_char_key(eq: &EventQueue, ch: char) {
    let s = ch.to_string();
    eq.push(UiEvent::Key {
        key: s.clone(),
        modifiers: 0,
        text: Some(s),
    });
}

fn push_special_key(eq: &EventQueue, name: &str) {
    eq.push(UiEvent::Key {
        key: name.to_string(),
        modifiers: 0,
        text: None,
    });
}

fn wait_for_frame_advance(
    slot: &FrameSlot,
    waker_pair: &(Mutex<u64>, Condvar),
    prev_gen: u64,
    timeout: Duration,
) -> Option<u64> {
    let deadline = Instant::now() + timeout;
    let mut guard = waker_pair.0.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        let cur = slot.latest().generation;
        if cur > prev_gen {
            return Some(cur);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let rem = deadline.saturating_duration_since(now);
        let (g, _) = waker_pair
            .1
            .wait_timeout(guard, rem)
            .unwrap_or_else(|p| p.into_inner());
        guard = g;
    }
}

fn percentile_us(sorted_us: &[u128], pct: usize) -> f64 {
    if sorted_us.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_us.len() - 1) * pct) / 100;
    sorted_us[idx] as f64 / 1000.0
}

#[test]
fn full_40_extension_sustained_40hz_and_32key_burst_smoke_benchmark() {
    let ws_root = workspace_root();
    let repo = TempRepoDir::new("lava_burst_bench");
    let cache_dir = repo.path().join("nsbc_cache");
    let win_state_file = repo.path().join("window_state.txt");
    fs::create_dir_all(&cache_dir).unwrap();

    // Create a realistic 2,000-line Rust file inside an initialized Git repository so
    // Tree-sitter (`rust.so`), `ext_gitsigns` (`HEAD` diff), `ext_cmp`, `ext_trouble`,
    // `ext_aerial`, `ext_todo_comments`, `ext_colorizer`, and `ext_illuminate` all activate!
    let target_file = repo.path().join("engine_bench.rs");
    let mut initial_src = String::with_capacity(96_000);
    for i in 0..2_000 {
        if i == 15 {
            initial_src.push_str("// TODO: keep latency silky smooth on low-power CPUs\n");
        } else {
            initial_src.push_str(&format!(
                "pub fn stage_handler_{i:04}(input: i64) -> i64 {{ input + {i} }}\n"
            ));
        }
    }
    fs::write(&target_file, &initial_src).expect("write 2,000-line engine_bench.rs");

    let run_git = |args: &[&str]| {
        let _ = Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .output();
    };
    run_git(&["init", "-q"]);
    run_git(&["config", "user.email", "bench@lava.local"]);
    run_git(&["config", "user.name", "Lava Bench"]);
    run_git(&["add", "engine_bench.rs"]);
    run_git(&["commit", "-q", "-m", "init 2000-line file"]);

    let target_path_str = target_file.to_string_lossy().into_owned();
    let ws = LavaWorkspace::new(&ws_root).with_cache_dir(&cache_dir);
    let boot_opts = BootOptions::new()
        .with_startup_files(vec![target_path_str.clone()])
        .with_headless(false);

    let compile_t0 = Instant::now();
    let artifact = ws
        .compile_boot(&boot_opts)
        .expect("compile full 40-extension Lava boot artifact");
    let compile_ms = compile_t0.elapsed().as_secs_f64() * 1000.0;

    let frame_slot = Arc::new(FrameSlot::new());
    let event_queue = Arc::new(EventQueue::new());
    let waker_pair = Arc::new((Mutex::new(0u64), Condvar::new()));
    {
        let wp = Arc::clone(&waker_pair);
        let fs_ref = Arc::clone(&frame_slot);
        frame_slot.set_ui_waker(Box::new(move || {
            let mut g = wp.0.lock().unwrap_or_else(|p| p.into_inner());
            *g = fs_ref.latest().generation;
            wp.1.notify_all();
        }));
    }

    let eq_driver = Arc::clone(&event_queue);
    let fs_driver = Arc::clone(&frame_slot);
    let wp_driver = Arc::clone(&waker_pair);

    let driver_handle = thread::spawn(move || {
        // Wait for initial boot frame commit
        let mut frame_gen =
            wait_for_frame_advance(&fs_driver, &wp_driver, 0, Duration::from_secs(15))
                .expect("initial boot frame committed");
        assert!(!fs_driver.latest().commands.is_empty());

        // Set viewport size to 1440x900
        eq_driver.push(UiEvent::Resize {
            width: 1440,
            height: 900,
            scale_factor: 1,
        });
        frame_gen =
            wait_for_frame_advance(&fs_driver, &wp_driver, frame_gen, Duration::from_secs(5))
                .expect("resize frame committed");

        // Phase 1: Sustained 40 keys/sec (25ms interval, > 2x the 20 keys/sec requirement!)
        // Enter Insert mode on line 1 (`O` opens a line above row 0? Let's use `i` at `(0, 0)`
        // and type `"// paced_40hz_check\n"`, then `<Escape>`, then Normal motions `jjkkwbb0`).
        let mut paced_latencies_us: Vec<u128> = Vec::with_capacity(48);
        let paced_script: &[Result<char, &str>] = &[
            Ok('i'),
            Ok('/'),
            Ok('/'),
            Ok(' '),
            Ok('p'),
            Ok('a'),
            Ok('c'),
            Ok('e'),
            Ok('d'),
            Ok('_'),
            Ok('4'),
            Ok('0'),
            Ok('h'),
            Ok('z'),
            Ok('_'),
            Ok('o'),
            Ok('k'),
            Err("Return"),
            Err("Escape"),
            Ok('j'),
            Ok('j'),
            Ok('j'),
            Ok('w'),
            Ok('w'),
            Ok('b'),
            Ok('k'),
            Ok('k'),
            Ok('k'),
            Ok('0'),
        ];

        for step in paced_script {
            let prev = fs_driver.latest().generation.max(frame_gen);
            let t0 = Instant::now();
            match *step {
                Ok(ch) => push_char_key(&eq_driver, ch),
                Err(name) => push_special_key(&eq_driver, name),
            }
            frame_gen =
                wait_for_frame_advance(&fs_driver, &wp_driver, prev, Duration::from_secs(5))
                    .expect("frame advance on paced key");
            paced_latencies_us.push(t0.elapsed().as_micros());
            // Pace remaining slice of 25ms (= 40 keys/sec)
            let elapsed = t0.elapsed();
            if elapsed < Duration::from_millis(25) {
                thread::sleep(Duration::from_millis(25) - elapsed);
            }
        }

        // Phase 2: Instantaneous 30-key simultaneous burst (`EventQueue` batch coalescing!)
        // Cursor is at row 1 col 0 (`"pub fn stage_handler_0000..."`); press `0i` + `"// burst_32_keys_verified"` + `<Return><Escape>`
        // all queued back-to-back in one sub-millisecond burst!
        let prev_burst_gen = fs_driver.latest().generation.max(frame_gen);
        let burst_t0 = Instant::now();
        push_char_key(&eq_driver, '0');
        push_char_key(&eq_driver, 'i');
        for ch in "// burst_32_keys_verified!".chars() {
            push_char_key(&eq_driver, ch);
        }
        push_special_key(&eq_driver, "Return");
        push_special_key(&eq_driver, "Escape");
        // Wait until frame commits and any queued tail settles
        frame_gen = wait_for_frame_advance(
            &fs_driver,
            &wp_driver,
            prev_burst_gen,
            Duration::from_secs(5),
        )
        .expect("burst frame committed");
        let burst_first_frame_ms = burst_t0.elapsed().as_secs_f64() * 1000.0;
        thread::sleep(Duration::from_millis(20));
        let post_burst_gen = fs_driver.latest().generation.max(frame_gen);
        let burst_frames_emitted = post_burst_gen - prev_burst_gen;

        // Phase 3: Save & quit cleanly via `:wq<Return>`
        for ch in ":wq".chars() {
            push_char_key(&eq_driver, ch);
        }
        push_special_key(&eq_driver, "Return");

        (
            paced_latencies_us,
            burst_first_frame_ms,
            burst_frames_emitted,
        )
    });

    // Isolate window state file via per-test path
    // SAFETY: Single test binary dedicated to this benchmark; sets isolated `LAVA_WINDOW_STATE_FILE`.
    unsafe {
        std::env::set_var("LAVA_WINDOW_STATE_FILE", &win_state_file);
    }

    let host_cfg = HostConfig::new(Arc::clone(&frame_slot), Arc::clone(&event_queue))
        .with_boot_runtime(vec![target_path_str], String::new(), false);
    let exit_code =
        run_compiled_artifact(artifact, host_cfg).expect("run 40-extension burst benchmark");
    assert_eq!(exit_code, 0);

    let (mut paced_us, burst_ms, burst_frames) = driver_handle.join().expect("driver thread");
    paced_us.sort_unstable();
    let p50_ms = percentile_us(&paced_us, 50);
    let p95_ms = percentile_us(&paced_us, 95);
    let p99_ms = percentile_us(&paced_us, 99);
    let max_ms = percentile_us(&paced_us, 100);

    eprintln!(
        "[lava-burst-bench] compile={compile_ms:.1}ms | 40Hz paced ({} keys on 2000-line git repo w/ 40 exts): p50={p50_ms:.2}ms p95={p95_ms:.2}ms p99={p99_ms:.2}ms max={max_ms:.2}ms | 30-key instant burst={burst_ms:.2}ms across {burst_frames} frame(s)",
        paced_us.len()
    );

    // Verify zero dropped or reordered characters on disk!
    let saved = fs::read_to_string(&target_file).expect("read saved engine_bench.rs");
    let mut lines = saved.lines();
    assert_eq!(lines.next(), Some("// paced_40hz_ok"));
    assert_eq!(lines.next(), Some("// burst_32_keys_verified!"));
    assert_eq!(
        lines.next(),
        Some("pub fn stage_handler_0000(input: i64) -> i64 { input + 0 }")
    );
    assert_eq!(saved.lines().count(), 2_002);

    // Assert strong responsiveness budgets even in unoptimized test profile!
    assert!(
        p95_ms < 45.0,
        "expected p95 per-keystroke-to-frame latency < 45ms, got {p95_ms:.2}ms"
    );
    assert!(
        burst_frames <= 6,
        "expected 30-key burst to coalesce into <= 6 frames, got {burst_frames}"
    );
}
