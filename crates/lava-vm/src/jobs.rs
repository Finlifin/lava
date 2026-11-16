//! Non-blocking background subprocess pool and lightweight timer scheduler.
//!
//! Background jobs and timers execute off the single Nessa VM thread and deliver completion
//! notifications exclusively through [`EventQueue`], waking any VM wait cleanly.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lava_proto::{EventQueue, UiEvent};

const JOB_POLL_INTERVAL: Duration = Duration::from_millis(8);
const TIMER_QUIT_POLL_SLICE: Duration = Duration::from_millis(25);

/// Manages asynchronous background shell subprocesses and timers for the Lava host bridge.
pub struct JobManager {
    next_job_id: AtomicU64,
    next_timer_id: AtomicU64,
    active_jobs: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
}

impl Default for JobManager {
    fn default() -> Self {
        Self::new()
    }
}

impl JobManager {
    /// Construct a new [`JobManager`] with monotonic ID counters starting at `1`.
    pub fn new() -> Self {
        Self {
            next_job_id: AtomicU64::new(1),
            next_timer_id: AtomicU64::new(1),
            active_jobs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Spawn `sh -c <cmd>` on a background OS thread and return its monotonic `job_id`.
    ///
    /// When the child process exits (or is cancelled via [`Self::cancel_job`]), pushes
    /// [`UiEvent::AsyncJobDone`] into `event_queue`.
    pub fn spawn_job(
        &self,
        cmd: String,
        cwd: String,
        stdin_data: String,
        event_queue: Arc<EventQueue>,
    ) -> u64 {
        let job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
        let cancel_flag = Arc::new(AtomicBool::new(false));
        {
            let mut active = self
                .active_jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            active.insert(job_id, Arc::clone(&cancel_flag));
        }

        let active_jobs = Arc::clone(&self.active_jobs);
        thread::spawn(move || {
            let outcome = run_child_process(&cmd, &cwd, stdin_data, &cancel_flag, &event_queue);
            {
                let mut active = active_jobs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                active.remove(&job_id);
            }
            event_queue.push(UiEvent::AsyncJobDone {
                job_id,
                ok: outcome.ok,
                exit_code: outcome.exit_code,
                stdout: outcome.stdout,
                stderr: outcome.stderr,
            });
        });

        job_id
    }

    /// Request cancellation of a running background job. Returns `true` if `job_id` was active.
    pub fn cancel_job(&self, job_id: u64) -> bool {
        let active = self
            .active_jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(flag) = active.get(&job_id) {
            flag.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Signal cancellation for all currently active background jobs.
    pub fn cancel_all(&self) {
        let active = self
            .active_jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for flag in active.values() {
            flag.store(true, Ordering::Release);
        }
    }

    /// Return the number of background subprocesses currently registered as active.
    pub fn active_job_count(&self) -> usize {
        self.active_jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Schedule a one-shot timer firing after `delay_ms` milliseconds.
    ///
    /// Pushes [`UiEvent::Tick`] into `event_queue` unless quit has been requested.
    pub fn schedule_timer(&self, delay_ms: u64, event_queue: Arc<EventQueue>) -> u64 {
        let timer_id = self.next_timer_id.fetch_add(1, Ordering::Relaxed);
        thread::spawn(move || {
            if delay_ms > 0 {
                let target = Instant::now() + Duration::from_millis(delay_ms);
                loop {
                    if event_queue.quit_code().is_some() {
                        return;
                    }
                    let now = Instant::now();
                    if now >= target {
                        break;
                    }
                    let remaining = target.saturating_duration_since(now);
                    thread::sleep(remaining.min(TIMER_QUIT_POLL_SLICE));
                }
            }
            if event_queue.quit_code().is_some() {
                return;
            }
            event_queue.push(UiEvent::Tick {
                timer_id,
                now_ms: unix_now_millis(),
            });
        });
        timer_id
    }
}

struct ChildOutcome {
    ok: bool,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

fn run_child_process(
    cmd: &str,
    cwd: &str,
    stdin_data: String,
    cancel_flag: &AtomicBool,
    event_queue: &EventQueue,
) -> ChildOutcome {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if stdin_data.is_empty() {
        command.stdin(Stdio::null());
    } else {
        command.stdin(Stdio::piped());
    }

    if !cwd.is_empty() {
        let dir = Path::new(cwd);
        if dir.exists() {
            command.current_dir(dir);
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return ChildOutcome {
                ok: false,
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("failed to spawn shell job: {err}"),
            };
        }
    };

    let stdout_handle = child.stdout.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        })
    });

    let stderr_handle = child.stderr.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        })
    });

    let stdin_handle = child.stdin.take().map(|mut pipe| {
        thread::spawn(move || {
            let _ = pipe.write_all(stdin_data.as_bytes());
            let _ = pipe.flush();
        })
    });

    let mut was_cancelled = false;
    let exit_code = loop {
        if cancel_flag.load(Ordering::Acquire) || event_queue.quit_code().is_some() {
            was_cancelled = true;
            let _ = child.kill();
            let status = child.wait();
            break status.ok().and_then(|s| s.code()).unwrap_or(-1);
        }

        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => thread::sleep(JOB_POLL_INTERVAL),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = stdout_handle
                    .and_then(|h| h.join().ok())
                    .unwrap_or_default();
                let mut stderr = stderr_handle
                    .and_then(|h| h.join().ok())
                    .unwrap_or_default();
                if let Some(h) = stdin_handle {
                    let _ = h.join();
                }
                if !stderr.is_empty() && !stderr.ends_with('\n') {
                    stderr.push('\n');
                }
                stderr.push_str(&format!("error waiting on child process: {err}"));
                return ChildOutcome {
                    ok: false,
                    exit_code: -1,
                    stdout,
                    stderr,
                };
            }
        }
    };

    if let Some(h) = stdin_handle {
        let _ = h.join();
    }
    let stdout = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    ChildOutcome {
        ok: !was_cancelled && exit_code == 0,
        exit_code,
        stdout,
        stderr,
    }
}

fn unix_now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
