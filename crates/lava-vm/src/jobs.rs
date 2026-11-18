//! Event-driven background subprocess pool and zero-polling `BinaryHeap` timer wheel.
//!
//! Background jobs block directly on OS `waitpid` (zero `try_wait` sleep loops) with process-group
//! signal termination on cancel, and all host timers share a single `Condvar`-parked min-heap
//! thread (0% idle CPU, 0ms polling jitter, zero per-timer thread spawns).

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lava_proto::{EventQueue, UiEvent};

struct TimerEntry {
    deadline: Instant,
    seq: u64,
    callback: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        // Reverse ordering so `BinaryHeap` acts as a min-heap by earliest `deadline`.
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

struct SharedTimerWheel {
    state: Mutex<(u64, BinaryHeap<TimerEntry>)>,
    cvar: Condvar,
}

static GLOBAL_TIMER_WHEEL: OnceLock<Arc<SharedTimerWheel>> = OnceLock::new();

fn timer_wheel() -> &'static Arc<SharedTimerWheel> {
    GLOBAL_TIMER_WHEEL.get_or_init(|| {
        let wheel = Arc::new(SharedTimerWheel {
            state: Mutex::new((0, BinaryHeap::new())),
            cvar: Condvar::new(),
        });
        let worker_wheel = Arc::clone(&wheel);
        let _ = thread::Builder::new()
            .name("lava-timer-wheel".to_string())
            .spawn(move || timer_wheel_loop(worker_wheel));
        wheel
    })
}

fn timer_wheel_loop(wheel: Arc<SharedTimerWheel>) {
    loop {
        let mut due_callbacks: Vec<Box<dyn FnOnce() + Send + 'static>> = Vec::new();
        {
            let mut guard = wheel
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                let now = Instant::now();
                while let Some(top) = guard.1.peek() {
                    if top.deadline <= now {
                        if let Some(mut entry) = guard.1.pop()
                            && let Some(cb) = entry.callback.take()
                        {
                            due_callbacks.push(cb);
                        }
                    } else {
                        break;
                    }
                }
                if !due_callbacks.is_empty() {
                    break;
                }
                match guard.1.peek() {
                    None => {
                        guard = wheel
                            .cvar
                            .wait(guard)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    Some(top) => {
                        let wait_dur = top.deadline.saturating_duration_since(now);
                        let (g, _) = wheel
                            .cvar
                            .wait_timeout(guard, wait_dur)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        guard = g;
                    }
                }
            }
        }
        for cb in due_callbacks {
            cb();
        }
    }
}

/// Schedule a one-shot callback at `deadline` on the shared process-wide timer wheel.
pub fn schedule_deadline_callback<F>(deadline: Instant, callback: F)
where
    F: FnOnce() + Send + 'static,
{
    let wheel = timer_wheel();
    let mut guard = wheel
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let seq = guard.0;
    guard.0 = guard.0.wrapping_add(1);
    let notify = guard.1.peek().is_none_or(|top| deadline < top.deadline);
    guard.1.push(TimerEntry {
        deadline,
        seq,
        callback: Some(Box::new(callback)),
    });
    if notify {
        wheel.cvar.notify_one();
    }
}

struct ActiveJobHandle {
    cancelled: AtomicBool,
    pid: Mutex<Option<u32>>,
    #[cfg(windows)]
    win_job: Mutex<usize>,
}

impl ActiveJobHandle {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            pid: Mutex::new(None),
            #[cfg(windows)]
            win_job: Mutex::new(0),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        #[cfg(windows)]
        {
            let jh = *self
                .win_job
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if jh != 0 {
                win_job_ctrl::terminate_job(jh);
            }
        }
        let pid_opt = *self
            .pid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(pid) = pid_opt {
            kill_process_group(pid);
        }
    }
}

#[cfg(windows)]
impl Drop for ActiveJobHandle {
    fn drop(&mut self) {
        let jh = *self
            .win_job
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if jh != 0 {
            win_job_ctrl::close_job(jh);
        }
    }
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    if pid > 0 && pid <= i32::MAX as u32 {
        let pgid = -(pid as i32);
        // SAFETY: `kill` is async-signal-safe; sending `SIGKILL` to `-pid` terminates the
        // dedicated process group created via `setpgid(0, 0)` in `pre_exec` (falling back to `pid`).
        unsafe {
            if libc::kill(pgid, libc::SIGKILL) != 0 {
                let _ = libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
}

#[cfg(windows)]
mod win_job_ctrl {
    use std::ffi::c_void;
    use std::ptr;

    const PROCESS_TERMINATE: u32 = 0x0001;
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const INVALID_HANDLE_VALUE: isize = -1;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[repr(C)]
    struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(security_attrs: *mut c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            info_class: i32,
            info: *const c_void,
            info_len: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn TerminateJobObject(job: *mut c_void, exit_code: u32) -> i32;
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn TerminateProcess(process: *mut c_void, exit_code: u32) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, th32_process_id: u32) -> *mut c_void;
        fn Process32FirstW(snapshot: *mut c_void, lppe: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: *mut c_void, lppe: *mut ProcessEntry32W) -> i32;
        fn CloseHandle(object: *mut c_void) -> i32;
    }

    pub(super) fn create_and_assign(proc_handle: *mut c_void) -> usize {
        // SAFETY: `CreateJobObjectW`, `SetInformationJobObject`, and `AssignProcessToJobObject`
        // are Win2000+ `kernel32.dll` primitives valid on Windows 10 1709+ and Wine.
        unsafe {
            let job = CreateJobObjectW(ptr::null_mut(), ptr::null());
            if job.is_null() {
                return 0;
            }
            let mut info = JobObjectExtendedLimitInformation::default();
            info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let _ = SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                (&raw const info).cast::<c_void>(),
                std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
            );
            if AssignProcessToJobObject(job, proc_handle) == 0 {
                let _ = CloseHandle(job);
                return 0;
            }
            job as usize
        }
    }

    pub(super) fn terminate_job(job_addr: usize) {
        if job_addr != 0 {
            // SAFETY: `job_addr` is a live Job Object handle owned by `ActiveJobHandle`.
            unsafe {
                let _ = TerminateJobObject(job_addr as *mut c_void, 1);
            }
        }
    }

    pub(super) fn close_job(job_addr: usize) {
        if job_addr != 0 {
            // SAFETY: `CloseHandle` is called once in `ActiveJobHandle::drop`.
            unsafe {
                let _ = CloseHandle(job_addr as *mut c_void);
            }
        }
    }

    pub(super) fn kill_process_tree(root_pid: u32) {
        if root_pid == 0 {
            return;
        }
        // SAFETY: Snapshot process table via `CreateToolhelp32Snapshot` (Win2000+) and terminate
        // direct/transitive children plus `root_pid`.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if !snap.is_null() && (snap as isize) != INVALID_HANDLE_VALUE {
                let mut pairs: Vec<(u32, u32)> = Vec::new();
                let mut entry = ProcessEntry32W {
                    dw_size: std::mem::size_of::<ProcessEntry32W>() as u32,
                    cnt_usage: 0,
                    th32_process_id: 0,
                    th32_default_heap_id: 0,
                    th32_module_id: 0,
                    cnt_threads: 0,
                    th32_parent_process_id: 0,
                    pc_pri_class_base: 0,
                    dw_flags: 0,
                    sz_exe_file: [0; 260],
                };
                if Process32FirstW(snap, &raw mut entry) != 0 {
                    loop {
                        pairs.push((entry.th32_process_id, entry.th32_parent_process_id));
                        if Process32NextW(snap, &raw mut entry) == 0 {
                            break;
                        }
                    }
                }
                let _ = CloseHandle(snap);

                let mut to_kill = vec![root_pid];
                let mut idx = 0usize;
                while idx < to_kill.len() {
                    let parent = to_kill[idx];
                    for &(child, ppid) in &pairs {
                        if ppid == parent && child != 0 && !to_kill.contains(&child) {
                            to_kill.push(child);
                        }
                    }
                    idx += 1;
                }
                for &pid in to_kill.iter().rev() {
                    let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
                    if !h.is_null() {
                        let _ = TerminateProcess(h, 1);
                        let _ = CloseHandle(h);
                    }
                }
                return;
            }
            let h = OpenProcess(PROCESS_TERMINATE, 0, root_pid);
            if !h.is_null() {
                let _ = TerminateProcess(h, 1);
                let _ = CloseHandle(h);
            }
        }
    }
}

#[cfg(windows)]
fn kill_process_group(pid: u32) {
    win_job_ctrl::kill_process_tree(pid);
}

#[cfg(all(not(unix), not(windows)))]
fn kill_process_group(_pid: u32) {}

/// Manages asynchronous background shell subprocesses and timers for the Lava host bridge.
pub struct JobManager {
    next_job_id: AtomicU64,
    next_timer_id: AtomicU64,
    active_jobs: Arc<Mutex<HashMap<u64, Arc<ActiveJobHandle>>>>,
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
        let handle = Arc::new(ActiveJobHandle::new());
        {
            let mut active = self
                .active_jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            active.insert(job_id, Arc::clone(&handle));
        }

        let active_jobs = Arc::clone(&self.active_jobs);
        thread::spawn(move || {
            let outcome = run_child_process(&cmd, &cwd, stdin_data, &handle, &event_queue);
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
        let handle_opt = self
            .active_jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&job_id)
            .cloned();
        if let Some(handle) = handle_opt {
            handle.cancel();
            true
        } else {
            false
        }
    }

    /// Signal cancellation for all currently active background jobs.
    pub fn cancel_all(&self) {
        let handles: Vec<Arc<ActiveJobHandle>> = self
            .active_jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        for handle in handles {
            handle.cancel();
        }
    }

    /// Return the number of background subprocesses currently registered as active.
    pub fn active_job_count(&self) -> usize {
        self.active_jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Schedule a one-shot timer firing after `delay_ms` milliseconds on the shared timer wheel.
    ///
    /// Pushes [`UiEvent::Tick`] into `event_queue` unless quit has been requested.
    pub fn schedule_timer(&self, delay_ms: u64, event_queue: Arc<EventQueue>) -> u64 {
        let timer_id = self.next_timer_id.fetch_add(1, Ordering::Relaxed);
        if delay_ms == 0 {
            if event_queue.quit_code().is_none() {
                event_queue.push(UiEvent::Tick {
                    timer_id,
                    now_ms: unix_now_millis(),
                });
            }
            return timer_id;
        }
        let deadline = Instant::now() + Duration::from_millis(delay_ms);
        schedule_deadline_callback(deadline, move || {
            if event_queue.quit_code().is_none() {
                event_queue.push(UiEvent::Tick {
                    timer_id,
                    now_ms: unix_now_millis(),
                });
            }
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
    handle: &ActiveJobHandle,
    event_queue: &EventQueue,
) -> ChildOutcome {
    if handle.cancelled.load(Ordering::Acquire) || event_queue.quit_code().is_some() {
        return ChildOutcome {
            ok: false,
            exit_code: -1,
            stdout: String::new(),
            stderr: "job cancelled".to_string(),
        };
    }

    #[cfg(unix)]
    let mut command = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        let mut c = Command::new(comspec);
        c.raw_arg(format!("/d /s /c \"{cmd}\""));
        c.creation_flags(CREATE_NO_WINDOW);
        c
    };
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

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

    #[cfg(unix)]
    // SAFETY: `setpgid(0, 0)` is an async-signal-safe POSIX syscall placing the child shell in its
    // own process group so `cancel_job` terminates any spawned child pipeline cleanly.
    unsafe {
        command.pre_exec(|| {
            let _ = libc::setpgid(0, 0);
            Ok(())
        });
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

    let pid = child.id();
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let jh = win_job_ctrl::create_and_assign(child.as_raw_handle().cast());
        if jh != 0 {
            let mut slot = handle
                .win_job
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = jh;
        }
    }
    {
        let mut pid_slot = handle
            .pid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *pid_slot = Some(pid);
    }
    if handle.cancelled.load(Ordering::Acquire) || event_queue.quit_code().is_some() {
        #[cfg(windows)]
        {
            let jh = *handle
                .win_job
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if jh != 0 {
                win_job_ctrl::terminate_job(jh);
            }
        }
        kill_process_group(pid);
        let _ = child.kill();
    }

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

    let wait_res = child.wait();

    if let Some(h) = stdin_handle {
        let _ = h.join();
    }
    let stdout = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let mut stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    let was_cancelled =
        handle.cancelled.load(Ordering::Acquire) || event_queue.quit_code().is_some();

    match wait_res {
        Ok(status) => {
            let exit_code = status.code().unwrap_or(-1);
            ChildOutcome {
                ok: !was_cancelled && exit_code == 0,
                exit_code,
                stdout,
                stderr,
            }
        }
        Err(err) => {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str(&format!("error waiting on child process: {err}"));
            ChildOutcome {
                ok: false,
                exit_code: -1,
                stdout,
                stderr,
            }
        }
    }
}

fn unix_now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
