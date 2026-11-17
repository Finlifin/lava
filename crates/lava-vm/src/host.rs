//! [`LavaHostHandler`] implementing [`interpreter::HostHandler`] for all 12 `lava_sys` ops,
//! plus cooperative single-VM suspension/wakeup driving.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering, fence};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use interpreter::{HostCallOutcome, HostHandler, NativeValue, Vm, VmResult};
use lava_lsp::{LSP_WAKE_TIMER_ID, LspEvent, LspManager, LspTextEditItem};
use lava_proto::{AnimHints, EventQueue, FontMetrics, FrameSlot, ScrollViewport, UiEvent};
use lava_remote::{
    RemoteConnInfo, RemoteManager, discover_ssh_config_hosts, read_rpc_frame, write_rpc_frame,
};
use lava_term::PtyManager;
use runtime::TaskId;
use scheduler::Scheduler;

use crate::jobs::{JobManager, schedule_deadline_callback};
use crate::wire::{
    expect_bool, expect_f64, expect_i64, expect_list, expect_str, parse_commit_frame_args,
    parse_commit_frame_bytes_args, parse_commit_frame_flat_args, ui_event_to_native,
};

/// Host font-measurement callback signature (`(family, size_pt) -> FontMetrics`).
pub type MeasureFontFn = Box<dyn Fn(&str, f64) -> FontMetrics + Send + Sync>;
/// Host clipboard reader callback signature (`() -> String`).
pub type ClipboardGetFn = Box<dyn Fn() -> String + Send + Sync>;
/// Host clipboard writer callback signature (`(&str) -> ()`).
pub type ClipboardSetFn = Box<dyn Fn(&str) + Send + Sync>;
/// Entry recording a suspended VM task and its optional wakeup deadline.
pub type WaitingTaskEntry = (TaskId, Option<Instant>);

/// Optional UI-toolkit callbacks injected into [`LavaHostHandler`] so `lava-vm` remains
/// completely independent of GTK4.
pub struct HostCallbacks {
    pub measure_font: MeasureFontFn,
    pub clipboard_get: ClipboardGetFn,
    pub clipboard_set: ClipboardSetFn,
}

impl Default for HostCallbacks {
    fn default() -> Self {
        let clipboard_buf = Arc::new(Mutex::new(String::new()));
        let clip_read = Arc::clone(&clipboard_buf);
        let clip_write = clipboard_buf;

        Self {
            measure_font: Box::new(|_family, size_pt| {
                let base = FontMetrics::default();
                if size_pt <= 0.0 || (size_pt - 14.0).abs() < 1e-6 {
                    base
                } else {
                    let scale = size_pt / 14.0;
                    FontMetrics {
                        cell_width: base.cell_width * scale,
                        cell_height: (base.cell_height * scale).round().max(1.0),
                        ascent: (base.ascent * scale).round().max(1.0),
                        descent: (base.descent * scale).round().max(1.0),
                    }
                }
            }),
            clipboard_get: Box::new(move || {
                clip_read
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            }),
            clipboard_set: Box::new(move |text| {
                let mut guard = clip_write
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.clear();
                guard.push_str(text);
            }),
        }
    }
}

impl HostCallbacks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_measure_font<F>(mut self, f: F) -> Self
    where
        F: Fn(&str, f64) -> FontMetrics + Send + Sync + 'static,
    {
        self.measure_font = Box::new(f);
        self
    }

    pub fn with_clipboard_get<F>(mut self, f: F) -> Self
    where
        F: Fn() -> String + Send + Sync + 'static,
    {
        self.clipboard_get = Box::new(f);
        self
    }

    pub fn with_clipboard_set<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.clipboard_set = Box::new(f);
        self
    }
}

/// Runtime startup overrides supplied via [`HostConfig`] so cached `.nsbc` boot artifacts
/// can open arbitrary CLI files or headless key sequences without recompiling Nessa sources.
#[derive(Debug, Clone, Default)]
pub struct BootRuntimeArgs {
    pub startup_files: Vec<String>,
    pub headless_keys: String,
    pub headless: bool,
}

/// Complete host runtime configuration wired into a single Nessa VM instance.
#[derive(Default)]
pub struct HostConfig {
    pub frame_slot: Arc<FrameSlot>,
    pub event_queue: Arc<EventQueue>,
    pub jobs: Arc<JobManager>,
    pub ptys: Arc<PtyManager>,
    pub lsps: Arc<LspManager>,
    pub remotes: Arc<RemoteManager>,
    pub callbacks: HostCallbacks,
    pub script_args: Vec<String>,
    pub boot_runtime: Option<BootRuntimeArgs>,
}

impl HostConfig {
    pub fn new(frame_slot: Arc<FrameSlot>, event_queue: Arc<EventQueue>) -> Self {
        Self {
            frame_slot,
            event_queue,
            jobs: Arc::new(JobManager::new()),
            ptys: Arc::new(PtyManager::new()),
            lsps: Arc::new(LspManager::new()),
            remotes: Arc::new(RemoteManager::new()),
            callbacks: HostCallbacks::default(),
            script_args: Vec::new(),
            boot_runtime: None,
        }
    }

    pub fn with_jobs(mut self, jobs: Arc<JobManager>) -> Self {
        self.jobs = jobs;
        self
    }

    pub fn with_callbacks(mut self, callbacks: HostCallbacks) -> Self {
        self.callbacks = callbacks;
        self
    }

    pub fn with_script_args(mut self, script_args: Vec<String>) -> Self {
        self.script_args = script_args;
        self
    }

    pub fn with_boot_runtime(
        mut self,
        startup_files: Vec<String>,
        headless_keys: impl Into<String>,
        headless: bool,
    ) -> Self {
        self.boot_runtime = Some(BootRuntimeArgs {
            startup_files,
            headless_keys: headless_keys.into(),
            headless,
        });
        self
    }
}

struct ParkedUiWaiter {
    task_id: TaskId,
    token: u64,
    deadline: Option<Instant>,
    counted_in_scheduler: bool,
}

struct HostWaitInner {
    scheduler: Option<Scheduler>,
    next_token: u64,
    parked: Vec<ParkedUiWaiter>,
    woken_tasks: HashSet<TaskId>,
}

impl HostWaitInner {
    fn new() -> Self {
        Self {
            scheduler: None,
            next_token: 1,
            parked: Vec::new(),
            woken_tasks: HashSet::new(),
        }
    }
}

fn wake_waiters_on_event_or_quit(wait_inner: &Mutex<HostWaitInner>, event_queue: &EventQueue) {
    let quit_requested = event_queue.quit_code().is_some();
    let (sched_opt, to_wake) = {
        let mut guard = wait_inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sched_opt = guard.scheduler.clone();
        let popped: Vec<ParkedUiWaiter> = if quit_requested {
            std::mem::take(&mut guard.parked)
        } else if !guard.parked.is_empty() {
            vec![guard.parked.remove(0)]
        } else {
            Vec::new()
        };
        for w in &popped {
            guard.woken_tasks.insert(w.task_id);
        }
        (sched_opt, popped)
    };
    if let Some(sched) = sched_opt {
        for w in to_wake {
            let _ = sched.wake(w.task_id);
            if w.counted_in_scheduler {
                sched.dec_active_io();
            }
        }
    }
}

fn wake_waiter_on_deadline(wait_inner: &Mutex<HostWaitInner>, token: u64) {
    let (sched_opt, popped_opt) = {
        let mut guard = wait_inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(idx) = guard.parked.iter().position(|w| w.token == token) {
            let w = guard.parked.remove(idx);
            guard.woken_tasks.insert(w.task_id);
            (guard.scheduler.clone(), Some(w))
        } else {
            (None, None)
        }
    };
    if let (Some(sched), Some(w)) = (sched_opt, popped_opt) {
        let _ = sched.wake(w.task_id);
        if w.counted_in_scheduler {
            sched.dec_active_io();
        }
    }
}

/// Shared wakeup coordination handle between [`LavaHostHandler`] and the outer VM run loop.
#[derive(Clone)]
pub struct HostWaitController {
    event_queue: Arc<EventQueue>,
    jobs: Arc<JobManager>,
    ptys: Arc<PtyManager>,
    lsps: Arc<LspManager>,
    remotes: Arc<RemoteManager>,
    wait_inner: Arc<Mutex<HostWaitInner>>,
}

impl HostWaitController {
    fn attach_scheduler(&self, scheduler: Scheduler) {
        {
            let mut guard = self
                .wait_inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for w in &mut guard.parked {
                if !w.counted_in_scheduler {
                    scheduler.inc_active_io();
                    w.counted_in_scheduler = true;
                }
            }
            guard.scheduler = Some(scheduler);
        }
        let wait_inner = Arc::clone(&self.wait_inner);
        let eq = Arc::clone(&self.event_queue);
        self.event_queue.set_push_waker(Some(Arc::new(move || {
            wake_waiters_on_event_or_quit(&wait_inner, &eq);
        })));
        if self.event_queue.has_pending_or_quit() {
            wake_waiters_on_event_or_quit(&self.wait_inner, &self.event_queue);
        }
    }

    fn detach_scheduler(&self) {
        self.event_queue.set_push_waker(None);
        let (sched_opt, remaining_counted) = {
            let mut guard = self
                .wait_inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let sched_opt = guard.scheduler.take();
            let count = guard
                .parked
                .drain(..)
                .filter(|w| w.counted_in_scheduler)
                .count();
            guard.woken_tasks.clear();
            (sched_opt, count)
        };
        if let Some(sched) = sched_opt {
            for _ in 0..remaining_counted {
                sched.dec_active_io();
            }
        }
    }
}

/// Flat host bridge implementing all `lava_sys` operations for a single Nessa VM.
pub struct LavaHostHandler {
    frame_slot: Arc<FrameSlot>,
    event_queue: Arc<EventQueue>,
    jobs: Arc<JobManager>,
    ptys: Arc<PtyManager>,
    lsps: Arc<LspManager>,
    remotes: Arc<RemoteManager>,
    srv_stdin_reader: Mutex<Option<BufReader<std::io::Stdin>>>,
    callbacks: HostCallbacks,
    boot_runtime: Option<BootRuntimeArgs>,
    mock_mode: AtomicBool,
    wait_inner: Arc<Mutex<HostWaitInner>>,
}

impl LavaHostHandler {
    /// Construct a [`LavaHostHandler`] and its paired [`HostWaitController`] from [`HostConfig`].
    pub fn from_config(config: HostConfig) -> (Self, HostWaitController, Vec<String>) {
        let wait_inner = Arc::new(Mutex::new(HostWaitInner::new()));

        let eq_for_pty = Arc::clone(&config.event_queue);
        config.ptys.set_waker(Arc::new(move || {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            eq_for_pty.push(UiEvent::Tick {
                timer_id: 0x5054_5900,
                now_ms,
            });
        }));

        let eq_for_lsp = Arc::clone(&config.event_queue);
        config.lsps.set_waker(Arc::new(move || {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            eq_for_lsp.push(UiEvent::Tick {
                timer_id: LSP_WAKE_TIMER_ID,
                now_ms,
            });
        }));

        let controller = HostWaitController {
            event_queue: Arc::clone(&config.event_queue),
            jobs: Arc::clone(&config.jobs),
            ptys: Arc::clone(&config.ptys),
            lsps: Arc::clone(&config.lsps),
            remotes: Arc::clone(&config.remotes),
            wait_inner: Arc::clone(&wait_inner),
        };

        let handler = Self {
            frame_slot: config.frame_slot,
            event_queue: config.event_queue,
            jobs: config.jobs,
            ptys: config.ptys,
            lsps: config.lsps,
            remotes: config.remotes,
            srv_stdin_reader: Mutex::new(None),
            callbacks: config.callbacks,
            boot_runtime: config.boot_runtime,
            mock_mode: AtomicBool::new(false),
            wait_inner,
        };

        (handler, controller, config.script_args)
    }

    pub fn frame_slot(&self) -> &Arc<FrameSlot> {
        &self.frame_slot
    }

    pub fn event_queue(&self) -> &Arc<EventQueue> {
        &self.event_queue
    }

    pub fn jobs(&self) -> &Arc<JobManager> {
        &self.jobs
    }

    pub fn waiting_tasks_snapshot(&self) -> Vec<WaitingTaskEntry> {
        let guard = self
            .wait_inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .parked
            .iter()
            .map(|w| (w.task_id, w.deadline))
            .collect()
    }

    fn drain_all_available_events(&self) -> Vec<NativeValue> {
        self.event_queue
            .drain()
            .iter()
            .map(ui_event_to_native)
            .collect()
    }

    fn handle_wait_events(&self, args: &[NativeValue], task_id: TaskId) -> HostCallOutcome {
        if args.len() != 1 {
            return HostCallOutcome::Error(format!(
                "ui_wait_events expects 1 argument, got {}",
                args.len()
            ));
        }
        let timeout_ms = match expect_i64(&args[0], "ui_wait_events.timeout_ms") {
            Ok(v) => v,
            Err(err) => return HostCallOutcome::Error(err),
        };

        let (was_woken, sched_opt, stale_counted) = {
            let mut guard = self
                .wait_inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let woken = guard.woken_tasks.remove(&task_id);
            let mut stale = 0usize;
            guard.parked.retain(|w| {
                if w.task_id == task_id {
                    if w.counted_in_scheduler {
                        stale += 1;
                    }
                    false
                } else {
                    true
                }
            });
            (woken, guard.scheduler.clone(), stale)
        };
        if stale_counted > 0
            && let Some(ref sched) = sched_opt
        {
            for _ in 0..stale_counted {
                sched.dec_active_io();
            }
        }

        let available = self.drain_all_available_events();
        if !available.is_empty() || self.event_queue.quit_code().is_some() {
            return HostCallOutcome::Return(NativeValue::List(available));
        }

        if timeout_ms == 0 || was_woken {
            return HostCallOutcome::Return(NativeValue::List(Vec::new()));
        }

        let deadline = if timeout_ms < 0 {
            None
        } else {
            let clamped_ms = (timeout_ms as u64).min(86_400_000);
            Instant::now().checked_add(Duration::from_millis(clamped_ms))
        };

        let (token, counted) = {
            let mut guard = self
                .wait_inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let tok = guard.next_token;
            guard.next_token = guard.next_token.wrapping_add(1);
            let is_counted = if let Some(ref sched) = guard.scheduler {
                sched.inc_active_io();
                true
            } else {
                false
            };
            guard.parked.push(ParkedUiWaiter {
                task_id,
                token: tok,
                deadline,
                counted_in_scheduler: is_counted,
            });
            (tok, is_counted)
        };

        fence(Ordering::SeqCst);
        if self.event_queue.has_pending_or_quit() {
            wake_waiters_on_event_or_quit(&self.wait_inner, &self.event_queue);
            return HostCallOutcome::SuspendRetry;
        }

        if counted && let Some(dl) = deadline {
            let wait_arc = Arc::clone(&self.wait_inner);
            schedule_deadline_callback(dl, move || {
                wake_waiter_on_deadline(&wait_arc, token);
            });
        }
        HostCallOutcome::SuspendRetry
    }

    fn handle_commit_frame(&self, args: &[NativeValue]) -> HostCallOutcome {
        match parse_commit_frame_args(args) {
            Ok(frame) => {
                let generation = self.frame_slot.commit(frame);
                HostCallOutcome::Return(NativeValue::I64(generation as i64))
            }
            Err(err) => HostCallOutcome::Error(err),
        }
    }

    fn handle_commit_frame_bytes(&self, args: &[NativeValue]) -> HostCallOutcome {
        match parse_commit_frame_bytes_args(args) {
            Ok(frame) => {
                let generation = self.frame_slot.commit(frame);
                HostCallOutcome::Return(NativeValue::I64(generation as i64))
            }
            Err(err) => HostCallOutcome::Error(err),
        }
    }

    fn handle_commit_frame_flat(&self, args: &[NativeValue]) -> HostCallOutcome {
        match parse_commit_frame_flat_args(args) {
            Ok(frame) => {
                let generation = self.frame_slot.commit(frame);
                HostCallOutcome::Return(NativeValue::I64(generation as i64))
            }
            Err(err) => HostCallOutcome::Error(err),
        }
    }

    fn handle_set_anim_hints(&self, args: &[NativeValue]) -> HostCallOutcome {
        if args.len() != 3 {
            return HostCallOutcome::Error(format!(
                "ui_set_anim_hints expects 3 arguments, got {}",
                args.len()
            ));
        }
        let smooth_scroll = match expect_bool(&args[0], "ui_set_anim_hints.smooth_scroll") {
            Ok(v) => v,
            Err(err) => return HostCallOutcome::Error(err),
        };
        let cursor_anim = match expect_bool(&args[1], "ui_set_anim_hints.cursor_anim") {
            Ok(v) => v,
            Err(err) => return HostCallOutcome::Error(err),
        };
        let raw_vps = match expect_list(&args[2], "ui_set_anim_hints.viewports") {
            Ok(v) => v,
            Err(err) => return HostCallOutcome::Error(err),
        };
        let mut viewports = Vec::with_capacity(raw_vps.len());
        for item in raw_vps {
            if let Ok(fields) = expect_list(item, "ui_set_anim_hints.viewport")
                && fields.len() >= 10
                && let (
                    Ok(pane_id),
                    Ok(doc_id),
                    Ok(x),
                    Ok(y),
                    Ok(w),
                    Ok(h),
                    Ok(top_row),
                    Ok(left_col),
                    Ok(cell_w),
                    Ok(cell_h),
                ) = (
                    expect_i64(&fields[0], "vp.pane_id"),
                    expect_i64(&fields[1], "vp.doc_id"),
                    expect_f64(&fields[2], "vp.x"),
                    expect_f64(&fields[3], "vp.y"),
                    expect_f64(&fields[4], "vp.w"),
                    expect_f64(&fields[5], "vp.h"),
                    expect_i64(&fields[6], "vp.top_row"),
                    expect_i64(&fields[7], "vp.left_col"),
                    expect_f64(&fields[8], "vp.cell_w"),
                    expect_f64(&fields[9], "vp.cell_h"),
                )
            {
                viewports.push(ScrollViewport {
                    pane_id,
                    doc_id,
                    x,
                    y,
                    w,
                    h,
                    top_row,
                    left_col,
                    cell_w,
                    cell_h,
                });
            }
        }
        self.frame_slot.set_anim_hints(AnimHints {
            smooth_scroll,
            cursor_anim,
            viewports,
        });
        HostCallOutcome::Return(NativeValue::Unit)
    }

    fn handle_measure_font(&self, args: &[NativeValue]) -> HostCallOutcome {
        if args.len() != 2 {
            return HostCallOutcome::Error(format!(
                "ui_measure_font expects 2 arguments, got {}",
                args.len()
            ));
        }
        let family = match expect_str(&args[0], "ui_measure_font.family") {
            Ok(s) => s,
            Err(err) => return HostCallOutcome::Error(err),
        };
        let size_pt = match expect_f64(&args[1], "ui_measure_font.size_pt") {
            Ok(v) => v,
            Err(err) => return HostCallOutcome::Error(err),
        };
        let metrics = (self.callbacks.measure_font)(&family, size_pt);
        HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::F64(metrics.cell_width),
            NativeValue::F64(metrics.cell_height),
            NativeValue::F64(metrics.ascent),
            NativeValue::F64(metrics.descent),
        ]))
    }

    fn handle_clipboard_set(&self, args: &[NativeValue]) -> HostCallOutcome {
        if args.len() != 1 {
            return HostCallOutcome::Error(format!(
                "ui_clipboard_set expects 1 argument, got {}",
                args.len()
            ));
        }
        match expect_str(&args[0], "ui_clipboard_set.text") {
            Ok(text) => {
                (self.callbacks.clipboard_set)(&text);
                HostCallOutcome::Return(NativeValue::Unit)
            }
            Err(err) => HostCallOutcome::Error(err),
        }
    }

    fn handle_request_quit(&self, args: &[NativeValue]) -> HostCallOutcome {
        if args.len() != 1 {
            return HostCallOutcome::Error(format!(
                "ui_request_quit expects 1 argument, got {}",
                args.len()
            ));
        }
        match expect_i64(&args[0], "ui_request_quit.exit_code") {
            Ok(code) => {
                self.event_queue.request_quit(code as i32);
                HostCallOutcome::Return(NativeValue::Unit)
            }
            Err(err) => HostCallOutcome::Error(err),
        }
    }

    fn handle_spawn_job(&self, args: &[NativeValue]) -> HostCallOutcome {
        if args.len() != 3 {
            return HostCallOutcome::Error(format!(
                "sys_spawn_job expects 3 arguments, got {}",
                args.len()
            ));
        }
        let cmd = match expect_str(&args[0], "sys_spawn_job.cmd") {
            Ok(s) => s,
            Err(err) => return HostCallOutcome::Error(err),
        };
        let cwd = match expect_str(&args[1], "sys_spawn_job.cwd") {
            Ok(s) => s,
            Err(err) => return HostCallOutcome::Error(err),
        };
        let stdin_data = match expect_str(&args[2], "sys_spawn_job.stdin_data") {
            Ok(s) => s,
            Err(err) => return HostCallOutcome::Error(err),
        };
        let job_id = self
            .jobs
            .spawn_job(cmd, cwd, stdin_data, Arc::clone(&self.event_queue));
        HostCallOutcome::Return(NativeValue::I64(job_id as i64))
    }

    fn handle_cancel_job(&self, args: &[NativeValue]) -> HostCallOutcome {
        if args.len() != 1 {
            return HostCallOutcome::Error(format!(
                "sys_cancel_job expects 1 argument, got {}",
                args.len()
            ));
        }
        match expect_i64(&args[0], "sys_cancel_job.job_id") {
            Ok(job_id) => {
                let cancelled = if job_id >= 0 {
                    self.jobs.cancel_job(job_id as u64)
                } else {
                    false
                };
                HostCallOutcome::Return(NativeValue::Bool(cancelled))
            }
            Err(err) => HostCallOutcome::Error(err),
        }
    }

    fn handle_schedule_timer(&self, args: &[NativeValue]) -> HostCallOutcome {
        if args.len() != 1 {
            return HostCallOutcome::Error(format!(
                "sys_schedule_timer expects 1 argument, got {}",
                args.len()
            ));
        }
        match expect_i64(&args[0], "sys_schedule_timer.delay_ms") {
            Ok(delay_ms) => {
                let delay = delay_ms.max(0) as u64;
                let timer_id = self
                    .jobs
                    .schedule_timer(delay, Arc::clone(&self.event_queue));
                HostCallOutcome::Return(NativeValue::I64(timer_id as i64))
            }
            Err(err) => HostCallOutcome::Error(err),
        }
    }
    fn handle_boot_config(&self, args: &[NativeValue]) -> HostCallOutcome {
        if !args.is_empty() {
            return HostCallOutcome::Error(format!(
                "sys_boot_config expects 0 arguments, got {}",
                args.len()
            ));
        }
        match &self.boot_runtime {
            Some(rt) => {
                let files = rt
                    .startup_files
                    .iter()
                    .cloned()
                    .map(NativeValue::Str)
                    .collect();
                HostCallOutcome::Return(NativeValue::List(vec![
                    NativeValue::Bool(true),
                    NativeValue::List(files),
                    NativeValue::Str(rt.headless_keys.clone()),
                    NativeValue::Bool(rt.headless),
                ]))
            }
            None => HostCallOutcome::Return(NativeValue::List(vec![NativeValue::Bool(false)])),
        }
    }
}

impl HostHandler for LavaHostHandler {
    fn call(&mut self, op: &str, args: &[NativeValue], task_id: TaskId) -> HostCallOutcome {
        match op {
            "sys_boot_config" => self.handle_boot_config(args),
            "ui_poll_events" => {
                if !args.is_empty() {
                    return HostCallOutcome::Error(format!(
                        "ui_poll_events expects 0 arguments, got {}",
                        args.len()
                    ));
                }
                HostCallOutcome::Return(NativeValue::List(self.drain_all_available_events()))
            }
            "ui_wait_events" => self.handle_wait_events(args, task_id),
            "ui_commit_frame" => self.handle_commit_frame(args),
            "ui_commit_frame_bytes" => self.handle_commit_frame_bytes(args),
            "ui_commit_frame_flat" => self.handle_commit_frame_flat(args),
            "ui_set_anim_hints" => self.handle_set_anim_hints(args),
            "ui_measure_font" => self.handle_measure_font(args),
            "ui_clipboard_get" => {
                if !args.is_empty() {
                    return HostCallOutcome::Error(format!(
                        "ui_clipboard_get expects 0 arguments, got {}",
                        args.len()
                    ));
                }
                HostCallOutcome::Return(NativeValue::Str((self.callbacks.clipboard_get)()))
            }
            "ui_clipboard_set" => self.handle_clipboard_set(args),
            "ui_request_quit" => self.handle_request_quit(args),
            "sys_set_mock_mode" => {
                if args.len() != 1 {
                    return HostCallOutcome::Error(format!(
                        "sys_set_mock_mode expects 1 argument, got {}",
                        args.len()
                    ));
                }
                match expect_bool(&args[0], op) {
                    Ok(enabled) => {
                        self.mock_mode.store(enabled, Ordering::SeqCst);
                        HostCallOutcome::Return(NativeValue::Unit)
                    }
                    Err(msg) => HostCallOutcome::Error(msg),
                }
            }
            "sys_spawn_job" => self.handle_spawn_job(args),
            "sys_cancel_job" => self.handle_cancel_job(args),
            "sys_schedule_timer" => self.handle_schedule_timer(args),
            "fs_read_lines" => handle_fs_read_lines(args),
            "fs_write_lines" => handle_fs_write_lines(args),
            "accel_fuzzy_rank" => handle_accel_fuzzy_rank(args),
            "accel_orderless_rank" => handle_accel_orderless_rank(args),
            "accel_workspace_files" => handle_accel_workspace_files(args),
            "accel_workspace_grep" => handle_accel_workspace_grep(args),
            "accel_workspace_replace" => handle_accel_workspace_replace(args),
            "accel_diff_lines" => handle_accel_diff_lines(args),
            "accel_git_status" => handle_accel_git_status(args),
            "accel_git_head_lines" => handle_accel_git_head_lines(args),
            "accel_git_blame_line" => handle_accel_git_blame_line(args),
            "accel_scan_symbols" => handle_accel_scan_symbols(args),
            "accel_scan_buffer_todos" => handle_accel_scan_buffer_todos(args),
            "accel_scan_workspace_todos" => handle_accel_scan_workspace_todos(args),
            "accel_index_docs" => handle_accel_index_docs(args),
            "accel_search_docs" => handle_accel_search_docs(args),
            "accel_find_symbol_defs" => handle_accel_find_symbol_defs(args),
            "accel_doc_new" => handle_accel_doc_new(args),
            "accel_doc_open_file" => handle_accel_doc_open_file(args),
            "accel_doc_save_file" => handle_accel_doc_save_file(args),
            "accel_doc_clone_from_lines" => handle_accel_doc_clone_from_lines(args),
            "accel_doc_free" => handle_accel_doc_free(args),
            "accel_doc_line_count" => handle_accel_doc_line_count(args),
            "accel_doc_version" => handle_accel_doc_version(args),
            "accel_doc_line_at" => handle_accel_doc_line_at(args),
            "accel_doc_line_char_len" => handle_accel_doc_line_char_len(args),
            "accel_doc_slice_lines" => handle_accel_doc_slice_lines(args),
            "accel_doc_set_line" => handle_accel_doc_set_line(args),
            "accel_doc_insert_line" => handle_accel_doc_insert_line(args),
            "accel_doc_insert_lines" => handle_accel_doc_insert_lines(args),
            "accel_doc_remove_line" => handle_accel_doc_remove_line(args),
            "accel_doc_delete_lines" => handle_accel_doc_delete_lines(args),
            "accel_doc_insert_text" => handle_accel_doc_insert_text(args),
            "accel_doc_delete_span" => handle_accel_doc_delete_span(args),
            "accel_doc_extract_span" => handle_accel_doc_extract_span(args),
            "accel_doc_to_text_capped" => handle_accel_doc_to_text_capped(args),
            "accel_doc_snapshot_take" => handle_accel_doc_snapshot_take(args),
            "accel_doc_snapshot_matches" => handle_accel_doc_snapshot_matches(args),
            "accel_doc_snapshot_restore" => handle_accel_doc_snapshot_restore(args),
            "accel_doc_find_forward" => handle_accel_doc_find_forward(args),
            "accel_doc_find_backward" => handle_accel_doc_find_backward(args),
            "accel_doc_find_matching_bracket" => handle_accel_doc_find_matching_bracket(args),
            "accel_doc_analyze_diagnostics" => handle_accel_doc_analyze_diagnostics(args),
            "accel_doc_scan_todos" => handle_accel_doc_scan_todos(args),
            "accel_doc_scan_symbol_spans" => handle_accel_doc_scan_symbol_spans(args),
            "accel_doc_diff_head" => handle_accel_doc_diff_head(args),
            "accel_doc_scan_conflicts" => handle_accel_doc_scan_conflicts(args),
            "accel_doc_find_word_occurrences" => handle_accel_doc_find_word_occurrences(args),
            "accel_doc_collect_words" => handle_accel_doc_collect_words(args),
            "accel_doc_search_preview" => handle_accel_doc_search_preview(args),
            "accel_ts_highlight_rows" => handle_accel_ts_highlight_rows(args),
            "accel_ts_highlight_snippet" => handle_accel_ts_highlight_snippet(args),
            "accel_ts_inspect_node" => handle_accel_ts_inspect_node(args),
            "accel_ts_status" => handle_accel_ts_status(args),
            "pty_available" => {
                HostCallOutcome::Return(NativeValue::Bool(!self.mock_mode.load(Ordering::SeqCst)))
            }
            "pty_spawn" => handle_pty_spawn(&self.ptys, args),
            "pty_write" => handle_pty_write(&self.ptys, args),
            "pty_send_key" => handle_pty_send_key(&self.ptys, args),
            "pty_resize" => handle_pty_resize(&self.ptys, args),
            "pty_snapshot" => handle_pty_snapshot(&self.ptys, args),
            "pty_kill" => handle_pty_kill(&self.ptys, args),
            "lsp_available" => HostCallOutcome::Return(NativeValue::Bool(true)),
            "lsp_which" => handle_lsp_which(args),
            "lsp_find_root" => handle_lsp_find_root(args),
            "lsp_spawn" => handle_lsp_spawn(&self.lsps, args),
            "lsp_spawn_remote" => handle_lsp_spawn_remote(&self.lsps, args),
            "lsp_did_open" => handle_lsp_did_open(&self.lsps, args),
            "lsp_did_change" => handle_lsp_did_change(&self.lsps, args),
            "lsp_did_save" => handle_lsp_did_save(&self.lsps, args),
            "lsp_did_open_doc" | "accel_lsp_did_open_doc" => {
                handle_lsp_did_open_doc(&self.lsps, args)
            }
            "lsp_did_change_doc" | "accel_lsp_did_change_doc" => {
                handle_lsp_did_change_doc(&self.lsps, args)
            }
            "lsp_did_save_doc" | "accel_lsp_did_save_doc" => {
                handle_lsp_did_save_doc(&self.lsps, args)
            }
            "lsp_did_close" => handle_lsp_did_close(&self.lsps, args),
            "lsp_request" => handle_lsp_request(&self.lsps, args),
            "lsp_poll_events" => handle_lsp_poll_events(&self.lsps, args),
            "lsp_status" => handle_lsp_status(&self.lsps, args),
            "lsp_stop" => handle_lsp_stop(&self.lsps, args),
            "srv_rpc_recv" => handle_srv_rpc_recv(&self.srv_stdin_reader, args),
            "srv_rpc_send" => handle_srv_rpc_send(args),
            "remote_available" => {
                HostCallOutcome::Return(NativeValue::Bool(!self.mock_mode.load(Ordering::SeqCst)))
            }
            "remote_register_profile" => handle_remote_register_profile(&self.remotes, args),
            "remote_ssh_hosts" => handle_remote_ssh_hosts(args),
            "remote_connect" => handle_remote_connect(&self.remotes, args),
            "remote_disconnect" => handle_remote_disconnect(&self.remotes, args),
            "remote_list_connections" => handle_remote_list_connections(&self.remotes, args),
            "remote_stat" => handle_remote_stat(&self.remotes, args),
            "remote_read_lines" => handle_remote_read_lines(&self.remotes, args),
            "remote_write_lines" => handle_remote_write_lines(&self.remotes, args),
            "remote_readdir" => handle_remote_readdir(&self.remotes, args),
            "remote_mkdir" => handle_remote_mkdir(&self.remotes, args),
            "remote_remove" => handle_remote_remove(&self.remotes, args),
            "remote_rename" => handle_remote_rename(&self.remotes, args),
            "remote_copy" => handle_remote_copy(&self.remotes, args),
            "remote_which" => handle_remote_which(&self.remotes, args),
            "remote_find_root" => handle_remote_find_root(&self.remotes, args),
            "remote_search_files" => handle_remote_search_files(&self.remotes, args),
            "remote_search_grep" => handle_remote_search_grep(&self.remotes, args),
            "remote_exec" => handle_remote_exec(&self.remotes, args),
            "remote_deploy_server" => handle_remote_deploy_server(&self.remotes, args),
            "remote_lsp_launch_command" => handle_remote_lsp_launch_command(&self.remotes, args),
            "remote_port_forward_start" => handle_remote_port_forward_start(&self.remotes, args),
            "remote_port_forward_stop" => handle_remote_port_forward_stop(&self.remotes, args),
            "remote_port_forward_list" => handle_remote_port_forward_list(&self.remotes, args),
            other => HostCallOutcome::Error(format!("unknown lava_sys host operation: {other}")),
        }
    }
}

fn handle_fs_read_lines(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error(format!(
            "fs_read_lines expects 1 argument, got {}",
            args.len()
        ));
    }
    let path = match expect_str(&args[0], "fs_read_lines.path") {
        Ok(p) => p,
        Err(err) => return HostCallOutcome::Error(err),
    };
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(err) => return HostCallOutcome::Error(format!("cannot open file '{path}': {err}")),
    };
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    let mut lines = Vec::new();
    let mut has_crlf = false;

    loop {
        buf.clear();
        let bytes_read = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(err) => return HostCallOutcome::Error(format!("read error on '{path}': {err}")),
        };
        if bytes_read == 0 {
            break;
        }
        if buf.ends_with(b"\n") {
            buf.pop();
            if buf.ends_with(b"\r") {
                buf.pop();
                has_crlf = true;
            }
        } else if buf.ends_with(b"\r") {
            buf.pop();
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        lines.push(NativeValue::Str(text));
    }

    if lines.is_empty() {
        lines.push(NativeValue::Str(String::new()));
        has_crlf = false;
    }

    HostCallOutcome::Return(NativeValue::List(vec![
        NativeValue::List(lines),
        NativeValue::Bool(has_crlf),
    ]))
}

fn handle_fs_write_lines(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 && args.len() != 3 {
        return HostCallOutcome::Error(format!(
            "fs_write_lines expects 2 or 3 arguments, got {}",
            args.len()
        ));
    }
    let path = match expect_str(&args[0], "fs_write_lines.path") {
        Ok(p) => p,
        Err(err) => return HostCallOutcome::Error(err),
    };
    let raw_lines = match expect_list(&args[1], "fs_write_lines.lines") {
        Ok(l) => l,
        Err(err) => return HostCallOutcome::Error(err),
    };
    let trailing_newline = if args.len() == 3 {
        match expect_bool(&args[2], "fs_write_lines.trailing_newline") {
            Ok(b) => b,
            Err(err) => return HostCallOutcome::Error(err),
        }
    } else {
        true
    };

    let file = match File::create(&path) {
        Ok(f) => f,
        Err(err) => {
            return HostCallOutcome::Error(format!("cannot open file '{path}' for write: {err}"));
        }
    };
    let mut writer = BufWriter::new(file);
    let mut total_bytes: i64 = 0;
    let line_count = raw_lines.len();

    for (idx, item) in raw_lines.iter().enumerate() {
        let line = match expect_str(item, "fs_write_lines.line") {
            Ok(s) => s,
            Err(err) => return HostCallOutcome::Error(err),
        };
        if let Err(err) = writer.write_all(line.as_bytes()) {
            return HostCallOutcome::Error(format!("write failed on '{path}': {err}"));
        }
        total_bytes = total_bytes.saturating_add(line.len() as i64);
        if idx + 1 < line_count || trailing_newline {
            if let Err(err) = writer.write_all(b"\n") {
                return HostCallOutcome::Error(format!("write newline failed on '{path}': {err}"));
            }
            total_bytes = total_bytes.saturating_add(1);
        }
    }

    if let Err(err) = writer.flush() {
        return HostCallOutcome::Error(format!("flush failed on '{path}': {err}"));
    }

    HostCallOutcome::Return(NativeValue::I64(total_bytes))
}

fn extract_str_vec(raw: &[NativeValue], ctx: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(raw.len());
    for item in raw {
        out.push(expect_str(item, ctx)?);
    }
    Ok(out)
}

fn handle_accel_fuzzy_rank(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_fuzzy_rank expects 3 arguments".to_string());
    }
    let query = match expect_str(&args[0], "accel_fuzzy_rank.query") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let raw_cands = match expect_list(&args[1], "accel_fuzzy_rank.candidates") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let limit = match expect_i64(&args[2], "accel_fuzzy_rank.limit") {
        Ok(v) => v.max(0) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let cands = match extract_str_vec(raw_cands, "accel_fuzzy_rank.cand") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let ranked = lava_accel::fuzzy_rank_batch(&query, &cands, limit);
    let items = ranked
        .into_iter()
        .map(|m| {
            NativeValue::List(vec![
                NativeValue::I64(m.index as i64),
                NativeValue::I64(m.score),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_orderless_rank(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_orderless_rank expects 3 arguments".to_string());
    }
    let query = match expect_str(&args[0], "accel_orderless_rank.query") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let raw_cands = match expect_list(&args[1], "accel_orderless_rank.candidates") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let limit = match expect_i64(&args[2], "accel_orderless_rank.limit") {
        Ok(v) => v.max(0) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let cands = match extract_str_vec(raw_cands, "accel_orderless_rank.cand") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let (total, ranked) = lava_accel::orderless_rank_batch(&query, &cands, limit);
    let items = ranked
        .into_iter()
        .map(|m| {
            let pos = m.positions.into_iter().map(NativeValue::I64).collect();
            NativeValue::List(vec![
                NativeValue::I64(m.index as i64),
                NativeValue::I64(m.score),
                NativeValue::List(pos),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(vec![
        NativeValue::I64(total as i64),
        NativeValue::List(items),
    ]))
}

fn handle_accel_workspace_files(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_workspace_files expects 3 arguments".to_string());
    }
    let root = match expect_str(&args[0], "accel_workspace_files.root") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_files = match expect_i64(&args[1], "accel_workspace_files.max_files") {
        Ok(v) => v.max(0) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let include_hidden = match expect_bool(&args[2], "accel_workspace_files.include_hidden") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let files = lava_accel::workspace_files(&root, max_files, include_hidden);
    HostCallOutcome::Return(NativeValue::List(
        files.into_iter().map(NativeValue::Str).collect(),
    ))
}

fn handle_accel_workspace_grep(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 5 {
        return HostCallOutcome::Error("accel_workspace_grep expects 5 arguments".to_string());
    }
    let root = match expect_str(&args[0], "accel_workspace_grep.root") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let query = match expect_str(&args[1], "accel_workspace_grep.query") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let case_sensitive = match expect_bool(&args[2], "accel_workspace_grep.case_sensitive") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_results = match expect_i64(&args[3], "accel_workspace_grep.max_results") {
        Ok(v) => v.max(0) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_filter = match expect_str(&args[4], "accel_workspace_grep.file_filter") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let hits = lava_accel::workspace_grep(&root, &query, case_sensitive, max_results, &file_filter);
    let items = hits
        .into_iter()
        .map(|h| {
            NativeValue::List(vec![
                NativeValue::Str(h.file_path),
                NativeValue::I64(h.row),
                NativeValue::I64(h.col),
                NativeValue::Str(h.line_text),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_workspace_replace(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 5 {
        return HostCallOutcome::Error("accel_workspace_replace expects 5 arguments".to_string());
    }
    let root = match expect_str(&args[0], "accel_workspace_replace.root") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let query = match expect_str(&args[1], "accel_workspace_replace.query") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let replacement = match expect_str(&args[2], "accel_workspace_replace.replacement") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let case_sensitive = match expect_bool(&args[3], "accel_workspace_replace.case_sensitive") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_filter = match expect_str(&args[4], "accel_workspace_replace.file_filter") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lava_accel::workspace_replace(&root, &query, &replacement, case_sensitive, &file_filter) {
        Ok((files_changed, total_repl)) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::I64(files_changed as i64),
            NativeValue::I64(total_repl as i64),
        ])),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_accel_diff_lines(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("accel_diff_lines expects 2 arguments".to_string());
    }
    let raw_old = match expect_list(&args[0], "accel_diff_lines.old") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let raw_new = match expect_list(&args[1], "accel_diff_lines.new") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let old_lines = match extract_str_vec(raw_old, "accel_diff_lines.old_line") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let new_lines = match extract_str_vec(raw_new, "accel_diff_lines.new_line") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let hunks = lava_accel::diff_line_hunks(&old_lines, &new_lines);
    let items = hunks
        .into_iter()
        .map(|h| {
            NativeValue::List(vec![
                NativeValue::I64(h.old_start as i64),
                NativeValue::I64(h.old_count as i64),
                NativeValue::I64(h.new_start as i64),
                NativeValue::I64(h.new_count as i64),
                NativeValue::Str(h.kind),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_git_status(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("accel_git_status expects 1 argument".to_string());
    }
    let cwd = match expect_str(&args[0], "accel_git_status.cwd") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let st = lava_accel::git_repo_status(&cwd);
    let entries = st
        .entries
        .into_iter()
        .map(|(x, y, p)| {
            NativeValue::List(vec![
                NativeValue::Str(x),
                NativeValue::Str(y),
                NativeValue::Str(p),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(vec![
        NativeValue::Bool(st.is_repo),
        NativeValue::Str(st.root),
        NativeValue::Str(st.branch),
        NativeValue::I64(st.staged_count),
        NativeValue::I64(st.modified_count),
        NativeValue::I64(st.untracked_count),
        NativeValue::List(entries),
    ]))
}

fn handle_accel_git_head_lines(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("accel_git_head_lines expects 2 arguments".to_string());
    }
    let cwd = match expect_str(&args[0], "accel_git_head_lines.cwd") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[1], "accel_git_head_lines.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lava_accel::git_head_file_lines(&cwd, &file_path) {
        Some(lines) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::Bool(true),
            NativeValue::List(lines.into_iter().map(NativeValue::Str).collect()),
        ])),
        None => HostCallOutcome::Return(NativeValue::List(vec![NativeValue::Bool(false)])),
    }
}

fn handle_accel_git_blame_line(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_git_blame_line expects 3 arguments".to_string());
    }
    let cwd = match expect_str(&args[0], "accel_git_blame_line.cwd") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[1], "accel_git_blame_line.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let row = match expect_i64(&args[2], "accel_git_blame_line.row") {
        Ok(v) => v.max(0) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lava_accel::git_blame_line(&cwd, &file_path, row) {
        Some(b) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::Bool(true),
            NativeValue::Str(b.commit_short),
            NativeValue::Str(b.author),
            NativeValue::Str(b.date_ymd),
            NativeValue::Str(b.summary),
        ])),
        None => HostCallOutcome::Return(NativeValue::List(vec![NativeValue::Bool(false)])),
    }
}

fn handle_accel_scan_symbols(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("accel_scan_symbols expects 2 arguments".to_string());
    }
    let raw_lines = match expect_list(&args[0], "accel_scan_symbols.lines") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let mode = match expect_str(&args[1], "accel_scan_symbols.major_mode") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let lines = match extract_str_vec(raw_lines, "accel_scan_symbols.line") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let syms = lava_accel::scan_symbols(&lines, &mode);
    let items = syms
        .into_iter()
        .map(|s| {
            NativeValue::List(vec![
                NativeValue::Str(s.name),
                NativeValue::Str(s.kind),
                NativeValue::I64(s.row),
                NativeValue::I64(s.col),
                NativeValue::I64(s.level),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_scan_buffer_todos(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("accel_scan_buffer_todos expects 2 arguments".to_string());
    }
    let raw_lines = match expect_list(&args[0], "accel_scan_buffer_todos.lines") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[1], "accel_scan_buffer_todos.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let lines = match extract_str_vec(raw_lines, "accel_scan_buffer_todos.line") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let todos = lava_accel::scan_buffer_todos(&lines, &file_path);
    let items = todos
        .into_iter()
        .map(|t| {
            NativeValue::List(vec![
                NativeValue::Str(t.file_path),
                NativeValue::I64(t.row),
                NativeValue::I64(t.col),
                NativeValue::Str(t.tag),
                NativeValue::Str(t.text),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_scan_workspace_todos(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error(
            "accel_scan_workspace_todos expects 2 arguments".to_string(),
        );
    }
    let root = match expect_str(&args[0], "accel_scan_workspace_todos.root") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_results = match expect_i64(&args[1], "accel_scan_workspace_todos.max_results") {
        Ok(v) => v.max(0) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let todos = lava_accel::scan_workspace_todos(&root, max_results);
    let items = todos
        .into_iter()
        .map(|t| {
            NativeValue::List(vec![
                NativeValue::Str(t.file_path),
                NativeValue::I64(t.row),
                NativeValue::I64(t.col),
                NativeValue::Str(t.tag),
                NativeValue::Str(t.text),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_index_docs(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("accel_index_docs expects 1 argument".to_string());
    }
    let raw_roots = match expect_list(&args[0], "accel_index_docs.doc_roots") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let doc_roots = match extract_str_vec(raw_roots, "accel_index_docs.root") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let sections = lava_accel::index_docs(&doc_roots);
    let items = sections
        .into_iter()
        .map(|s| {
            NativeValue::List(vec![
                NativeValue::Str(s.node_id),
                NativeValue::Str(s.manual),
                NativeValue::Str(s.title),
                NativeValue::I64(s.level),
                NativeValue::Str(s.file_path),
                NativeValue::I64(s.start_row),
                NativeValue::I64(s.end_row),
                NativeValue::List(s.tags.into_iter().map(NativeValue::Str).collect()),
                NativeValue::Str(s.summary),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_search_docs(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_search_docs expects 3 arguments".to_string());
    }
    let raw_roots = match expect_list(&args[0], "accel_search_docs.doc_roots") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let doc_roots = match extract_str_vec(raw_roots, "accel_search_docs.root") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let query = match expect_str(&args[1], "accel_search_docs.query") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let limit = match expect_i64(&args[2], "accel_search_docs.limit") {
        Ok(v) => v.max(0) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let hits = lava_accel::search_docs(&doc_roots, &query, limit);
    let items = hits
        .into_iter()
        .map(|h| {
            NativeValue::List(vec![
                NativeValue::Str(h.node_id),
                NativeValue::Str(h.manual),
                NativeValue::Str(h.title),
                NativeValue::Str(h.file_path),
                NativeValue::I64(h.row),
                NativeValue::I64(h.score),
                NativeValue::Str(h.matched_tag),
                NativeValue::Str(h.snippet),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_find_symbol_defs(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_find_symbol_defs expects 3 arguments".to_string());
    }
    let raw_roots = match expect_list(&args[0], "accel_find_symbol_defs.code_roots") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let code_roots = match extract_str_vec(raw_roots, "accel_find_symbol_defs.root") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let query = match expect_str(&args[1], "accel_find_symbol_defs.query") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let limit = match expect_i64(&args[2], "accel_find_symbol_defs.limit") {
        Ok(v) => v.max(0) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let defs = lava_accel::find_symbol_defs(&code_roots, &query, limit);
    let items = defs
        .into_iter()
        .map(|d| {
            NativeValue::List(vec![
                NativeValue::Str(d.symbol),
                NativeValue::Str(d.kind),
                NativeValue::Str(d.file_path),
                NativeValue::I64(d.row),
                NativeValue::Str(d.signature),
                NativeValue::Str(d.doc_comment),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_doc_new(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("accel_doc_new expects 1 argument".to_string());
    }
    let raw_lines = match expect_list(&args[0], "accel_doc_new.initial_lines") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let lines = match extract_str_vec(raw_lines, "accel_doc_new.line") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let doc_id = lava_accel::doc_store::doc_new(lines);
    HostCallOutcome::Return(NativeValue::I64(doc_id))
}

fn handle_accel_doc_open_file(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("accel_doc_open_file expects 1 argument".to_string());
    }
    let path = match expect_str(&args[0], "accel_doc_open_file.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lava_accel::doc_store::doc_open_file(&path) {
        Ok((doc_id, line_count, crlf)) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::Bool(true),
            NativeValue::I64(doc_id),
            NativeValue::I64(line_count),
            NativeValue::Bool(crlf),
        ])),
        Err(err) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::Bool(false),
            NativeValue::Str(err),
        ])),
    }
}

fn handle_accel_doc_save_file(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_doc_save_file expects 3 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_save_file.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let path = match expect_str(&args[1], "accel_doc_save_file.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let crlf = match expect_bool(&args[2], "accel_doc_save_file.crlf") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lava_accel::doc_store::doc_save_file(doc_id, &path, crlf) {
        Ok(bytes) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::Bool(true),
            NativeValue::I64(bytes),
        ])),
        Err(err) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::Bool(false),
            NativeValue::Str(err),
        ])),
    }
}

fn handle_accel_doc_clone_from_lines(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error(
            "accel_doc_clone_from_lines expects 2 arguments".to_string(),
        );
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_clone_from_lines.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let raw_lines = match expect_list(&args[1], "accel_doc_clone_from_lines.lines") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let lines = match extract_str_vec(raw_lines, "accel_doc_clone_from_lines.line") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    lava_accel::doc_store::doc_clone_from_lines(doc_id, lines);
    HostCallOutcome::Return(NativeValue::Unit)
}

fn handle_accel_doc_free(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("accel_doc_free expects 1 argument".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_free.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    lava_accel::doc_store::doc_free(doc_id);
    HostCallOutcome::Return(NativeValue::Unit)
}

fn handle_accel_doc_line_count(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("accel_doc_line_count expects 1 argument".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_line_count.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::I64(lava_accel::doc_store::doc_line_count(
        doc_id,
    )))
}

fn handle_accel_doc_version(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("accel_doc_version expects 1 argument".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_version.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::I64(lava_accel::doc_store::doc_version(doc_id)))
}

fn handle_accel_doc_line_at(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("accel_doc_line_at expects 2 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_line_at.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let row = match expect_i64(&args[1], "accel_doc_line_at.row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::Str(lava_accel::doc_store::doc_line_at(
        doc_id, row,
    )))
}

fn handle_accel_doc_line_char_len(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("accel_doc_line_char_len expects 2 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_line_char_len.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let row = match expect_i64(&args[1], "accel_doc_line_char_len.row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::I64(lava_accel::doc_store::doc_line_char_len(
        doc_id, row,
    )))
}

fn handle_accel_doc_slice_lines(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_doc_slice_lines expects 3 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_slice_lines.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let start_row = match expect_i64(&args[1], "accel_doc_slice_lines.start_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let end_row = match expect_i64(&args[2], "accel_doc_slice_lines.end_row_exclusive") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let lines = lava_accel::doc_store::doc_slice_lines(doc_id, start_row, end_row);
    HostCallOutcome::Return(NativeValue::List(
        lines.into_iter().map(NativeValue::Str).collect(),
    ))
}

fn handle_accel_doc_set_line(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_doc_set_line expects 3 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_set_line.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let row = match expect_i64(&args[1], "accel_doc_set_line.row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let text = match expect_str(&args[2], "accel_doc_set_line.text") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    lava_accel::doc_store::doc_set_line(doc_id, row, text);
    HostCallOutcome::Return(NativeValue::Unit)
}

fn handle_accel_doc_insert_line(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_doc_insert_line expects 3 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_insert_line.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let row = match expect_i64(&args[1], "accel_doc_insert_line.row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let text = match expect_str(&args[2], "accel_doc_insert_line.text") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    lava_accel::doc_store::doc_insert_line(doc_id, row, text);
    HostCallOutcome::Return(NativeValue::Unit)
}

fn handle_accel_doc_insert_lines(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_doc_insert_lines expects 3 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_insert_lines.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let row = match expect_i64(&args[1], "accel_doc_insert_lines.row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let raw_lines = match expect_list(&args[2], "accel_doc_insert_lines.lines") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let lines = match extract_str_vec(raw_lines, "accel_doc_insert_lines.line") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    lava_accel::doc_store::doc_insert_lines(doc_id, row, lines);
    HostCallOutcome::Return(NativeValue::Unit)
}

fn handle_accel_doc_remove_line(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("accel_doc_remove_line expects 2 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_remove_line.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let row = match expect_i64(&args[1], "accel_doc_remove_line.row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::Str(lava_accel::doc_store::doc_remove_line(
        doc_id, row,
    )))
}

fn handle_accel_doc_delete_lines(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_doc_delete_lines expects 3 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_delete_lines.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let start_row = match expect_i64(&args[1], "accel_doc_delete_lines.start_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let end_row = match expect_i64(&args[2], "accel_doc_delete_lines.end_row_inclusive") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::Str(lava_accel::doc_store::doc_delete_lines(
        doc_id, start_row, end_row,
    )))
}

fn handle_accel_doc_insert_text(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 4 {
        return HostCallOutcome::Error("accel_doc_insert_text expects 4 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_insert_text.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let row = match expect_i64(&args[1], "accel_doc_insert_text.row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let col = match expect_i64(&args[2], "accel_doc_insert_text.col") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let text = match expect_str(&args[3], "accel_doc_insert_text.text") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let (end_r, end_c) = lava_accel::doc_store::doc_insert_text(doc_id, row, col, &text);
    HostCallOutcome::Return(NativeValue::List(vec![
        NativeValue::I64(end_r),
        NativeValue::I64(end_c),
    ]))
}

fn handle_accel_doc_delete_span(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 5 {
        return HostCallOutcome::Error("accel_doc_delete_span expects 5 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_delete_span.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let s_row = match expect_i64(&args[1], "accel_doc_delete_span.s_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let s_col = match expect_i64(&args[2], "accel_doc_delete_span.s_col") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let e_row = match expect_i64(&args[3], "accel_doc_delete_span.e_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let e_col = match expect_i64(&args[4], "accel_doc_delete_span.e_col") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::Str(lava_accel::doc_store::doc_delete_span(
        doc_id, s_row, s_col, e_row, e_col,
    )))
}

fn handle_accel_doc_extract_span(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 5 {
        return HostCallOutcome::Error("accel_doc_extract_span expects 5 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_extract_span.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let s_row = match expect_i64(&args[1], "accel_doc_extract_span.s_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let s_col = match expect_i64(&args[2], "accel_doc_extract_span.s_col") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let e_row = match expect_i64(&args[3], "accel_doc_extract_span.e_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let e_col = match expect_i64(&args[4], "accel_doc_extract_span.e_col") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::Str(lava_accel::doc_store::doc_extract_span(
        doc_id, s_row, s_col, e_row, e_col,
    )))
}

fn handle_accel_doc_to_text_capped(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("accel_doc_to_text_capped expects 2 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_to_text_capped.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let crlf = match expect_bool(&args[1], "accel_doc_to_text_capped.crlf") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::Str(lava_accel::doc_store::doc_to_text_capped(
        doc_id, crlf,
    )))
}

fn handle_accel_doc_snapshot_take(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("accel_doc_snapshot_take expects 1 argument".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_snapshot_take.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::I64(lava_accel::doc_store::doc_snapshot_take(
        doc_id,
    )))
}

fn handle_accel_doc_snapshot_matches(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error(
            "accel_doc_snapshot_matches expects 2 arguments".to_string(),
        );
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_snapshot_matches.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let snap_id = match expect_i64(&args[1], "accel_doc_snapshot_matches.snap_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::Bool(
        lava_accel::doc_store::doc_snapshot_matches(doc_id, snap_id),
    ))
}

fn handle_accel_doc_snapshot_restore(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error(
            "accel_doc_snapshot_restore expects 2 arguments".to_string(),
        );
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_snapshot_restore.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let snap_id = match expect_i64(&args[1], "accel_doc_snapshot_restore.snap_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::I64(
        lava_accel::doc_store::doc_snapshot_restore(doc_id, snap_id),
    ))
}

fn encode_opt_pos(opt: Option<(i64, i64)>) -> NativeValue {
    match opt {
        Some((r, c)) => NativeValue::List(vec![
            NativeValue::Bool(true),
            NativeValue::I64(r),
            NativeValue::I64(c),
        ]),
        None => NativeValue::List(vec![NativeValue::Bool(false)]),
    }
}

fn handle_accel_doc_find_forward(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 8 {
        return HostCallOutcome::Error("accel_doc_find_forward expects 8 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_find_forward.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let needle = match expect_str(&args[1], "accel_doc_find_forward.needle") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let from_row = match expect_i64(&args[2], "accel_doc_find_forward.from_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let from_col = match expect_i64(&args[3], "accel_doc_find_forward.from_col") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let min_row = match expect_i64(&args[4], "accel_doc_find_forward.min_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_row = match expect_i64(&args[5], "accel_doc_find_forward.max_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let ignore_case = match expect_bool(&args[6], "accel_doc_find_forward.ignore_case") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let wrap = match expect_bool(&args[7], "accel_doc_find_forward.wrap") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let found = lava_accel::doc_store::doc_find_forward(
        doc_id,
        &needle,
        from_row,
        from_col,
        min_row,
        max_row,
        ignore_case,
        wrap,
    );
    HostCallOutcome::Return(encode_opt_pos(found))
}

fn handle_accel_doc_find_backward(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 8 {
        return HostCallOutcome::Error("accel_doc_find_backward expects 8 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_find_backward.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let needle = match expect_str(&args[1], "accel_doc_find_backward.needle") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let from_row = match expect_i64(&args[2], "accel_doc_find_backward.from_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let from_col = match expect_i64(&args[3], "accel_doc_find_backward.from_col") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let min_row = match expect_i64(&args[4], "accel_doc_find_backward.min_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_row = match expect_i64(&args[5], "accel_doc_find_backward.max_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let ignore_case = match expect_bool(&args[6], "accel_doc_find_backward.ignore_case") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let wrap = match expect_bool(&args[7], "accel_doc_find_backward.wrap") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let found = lava_accel::doc_store::doc_find_backward(
        doc_id,
        &needle,
        from_row,
        from_col,
        min_row,
        max_row,
        ignore_case,
        wrap,
    );
    HostCallOutcome::Return(encode_opt_pos(found))
}

fn handle_accel_doc_find_matching_bracket(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 6 {
        return HostCallOutcome::Error(
            "accel_doc_find_matching_bracket expects 6 arguments".to_string(),
        );
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_find_matching_bracket.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let row = match expect_i64(&args[1], "accel_doc_find_matching_bracket.row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let col = match expect_i64(&args[2], "accel_doc_find_matching_bracket.col") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let min_row = match expect_i64(&args[3], "accel_doc_find_matching_bracket.min_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_row = match expect_i64(&args[4], "accel_doc_find_matching_bracket.max_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_scan = match expect_i64(&args[5], "accel_doc_find_matching_bracket.max_scan_lines") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let found = lava_accel::doc_store::doc_find_matching_bracket(
        doc_id, row, col, min_row, max_row, max_scan,
    );
    HostCallOutcome::Return(encode_opt_pos(found))
}

fn handle_accel_doc_analyze_diagnostics(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error(
            "accel_doc_analyze_diagnostics expects 3 arguments".to_string(),
        );
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_analyze_diagnostics.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let mode = match expect_str(&args[1], "accel_doc_analyze_diagnostics.major_mode") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_items = match expect_i64(&args[2], "accel_doc_analyze_diagnostics.max_items") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let (hits, errs, warns) =
        lava_accel::doc_store::doc_analyze_diagnostics(doc_id, &mode, max_items);
    let items = hits
        .into_iter()
        .map(|d| {
            NativeValue::List(vec![
                NativeValue::I64(d.row),
                NativeValue::I64(d.col),
                NativeValue::I64(d.end_col),
                NativeValue::Str(d.severity),
                NativeValue::Str(d.source),
                NativeValue::Str(d.message),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(vec![
        NativeValue::List(items),
        NativeValue::I64(errs),
        NativeValue::I64(warns),
    ]))
}

fn handle_accel_doc_scan_todos(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_doc_scan_todos expects 3 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_scan_todos.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[1], "accel_doc_scan_todos.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_items = match expect_i64(&args[2], "accel_doc_scan_todos.max_items") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let hits = lava_accel::doc_store::doc_scan_todos(doc_id, &file_path, max_items);
    let items = hits
        .into_iter()
        .map(|t| {
            NativeValue::List(vec![
                NativeValue::Str(t.file_path),
                NativeValue::I64(t.row),
                NativeValue::I64(t.col),
                NativeValue::Str(t.tag),
                NativeValue::Str(t.text),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_doc_scan_symbol_spans(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error(
            "accel_doc_scan_symbol_spans expects 3 arguments".to_string(),
        );
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_scan_symbol_spans.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let mode = match expect_str(&args[1], "accel_doc_scan_symbol_spans.major_mode") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_items = match expect_i64(&args[2], "accel_doc_scan_symbol_spans.max_items") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let spans = lava_accel::doc_store::doc_scan_symbol_spans(doc_id, &mode, max_items);
    let items = spans
        .into_iter()
        .map(|s| {
            NativeValue::List(vec![
                NativeValue::Str(s.name),
                NativeValue::Str(s.kind),
                NativeValue::I64(s.row),
                NativeValue::I64(s.col),
                NativeValue::I64(s.level),
                NativeValue::I64(s.end_row),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_doc_diff_head(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("accel_doc_diff_head expects 3 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_diff_head.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[1], "accel_doc_diff_head.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let reload_head = match expect_bool(&args[2], "accel_doc_diff_head.reload_head") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lava_accel::doc_store::doc_diff_head(doc_id, &file_path, reload_head) {
        Some((hunks, added, changed, deleted)) => {
            let items = hunks
                .into_iter()
                .map(|h| {
                    NativeValue::List(vec![
                        NativeValue::I64(h.old_start as i64),
                        NativeValue::I64(h.old_count as i64),
                        NativeValue::I64(h.new_start as i64),
                        NativeValue::I64(h.new_count as i64),
                        NativeValue::Str(h.kind),
                    ])
                })
                .collect();
            HostCallOutcome::Return(NativeValue::List(vec![
                NativeValue::Bool(true),
                NativeValue::List(items),
                NativeValue::I64(added),
                NativeValue::I64(changed),
                NativeValue::I64(deleted),
            ]))
        }
        None => HostCallOutcome::Return(NativeValue::List(vec![NativeValue::Bool(false)])),
    }
}

fn handle_accel_doc_scan_conflicts(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("accel_doc_scan_conflicts expects 2 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_scan_conflicts.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_items = match expect_i64(&args[1], "accel_doc_scan_conflicts.max_items") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let conflicts = lava_accel::doc_store::doc_scan_conflicts(doc_id, max_items);
    let items = conflicts
        .into_iter()
        .map(|c| {
            NativeValue::List(vec![
                NativeValue::I64(c.start_row),
                NativeValue::I64(c.base_row),
                NativeValue::I64(c.sep_row),
                NativeValue::I64(c.end_row),
                NativeValue::Str(c.ours_label),
                NativeValue::Str(c.theirs_label),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_doc_find_word_occurrences(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 5 {
        return HostCallOutcome::Error(
            "accel_doc_find_word_occurrences expects 5 arguments".to_string(),
        );
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_find_word_occurrences.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let word = match expect_str(&args[1], "accel_doc_find_word_occurrences.word") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let start_row = match expect_i64(&args[2], "accel_doc_find_word_occurrences.start_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let end_row = match expect_i64(
        &args[3],
        "accel_doc_find_word_occurrences.end_row_inclusive",
    ) {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_items = match expect_i64(&args[4], "accel_doc_find_word_occurrences.max_items") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let occs = lava_accel::doc_store::doc_find_word_occurrences(
        doc_id, &word, start_row, end_row, max_items,
    );
    let items = occs
        .into_iter()
        .map(|(r, sc, ec)| {
            NativeValue::List(vec![
                NativeValue::I64(r),
                NativeValue::I64(sc),
                NativeValue::I64(ec),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(items))
}

fn handle_accel_doc_collect_words(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 6 {
        return HostCallOutcome::Error("accel_doc_collect_words expects 6 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_collect_words.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let center_row = match expect_i64(&args[1], "accel_doc_collect_words.center_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let radius_rows = match expect_i64(&args[2], "accel_doc_collect_words.radius_rows") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let skip_row = match expect_i64(&args[3], "accel_doc_collect_words.skip_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let skip_col = match expect_i64(&args[4], "accel_doc_collect_words.skip_col") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_words = match expect_i64(&args[5], "accel_doc_collect_words.max_words") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let words = lava_accel::doc_store::doc_collect_words(
        doc_id,
        center_row,
        radius_rows,
        skip_row,
        skip_col,
        max_words,
    );
    HostCallOutcome::Return(NativeValue::List(
        words.into_iter().map(NativeValue::Str).collect(),
    ))
}

fn handle_accel_doc_search_preview(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 6 {
        return HostCallOutcome::Error("accel_doc_search_preview expects 6 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_doc_search_preview.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let needle = match expect_str(&args[1], "accel_doc_search_preview.needle") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let min_row = match expect_i64(&args[2], "accel_doc_search_preview.min_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_row = match expect_i64(&args[3], "accel_doc_search_preview.max_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let ignore_case = match expect_bool(&args[4], "accel_doc_search_preview.ignore_case") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let max_previews = match expect_i64(&args[5], "accel_doc_search_preview.max_previews") {
        Ok(v) => v.max(0) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let (total, previews) = lava_accel::doc_store::doc_search_preview(
        doc_id,
        &needle,
        min_row,
        max_row,
        ignore_case,
        max_previews,
    );
    let items = previews
        .into_iter()
        .map(|(row, col, clipped)| {
            NativeValue::List(vec![
                NativeValue::I64(row),
                NativeValue::I64(col),
                NativeValue::Str(clipped),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(vec![
        NativeValue::I64(total),
        NativeValue::List(items),
    ]))
}

fn encode_ts_highlight_rows(rows: Vec<Vec<lava_accel::TsHighlightSpan>>) -> NativeValue {
    NativeValue::List(
        rows.into_iter()
            .map(|row| {
                NativeValue::List(
                    row.into_iter()
                        .map(|sp| {
                            NativeValue::List(vec![
                                NativeValue::I64(sp.start_col),
                                NativeValue::I64(sp.end_col),
                                NativeValue::I64(sp.hl_group),
                            ])
                        })
                        .collect(),
                )
            })
            .collect(),
    )
}

fn handle_accel_ts_highlight_rows(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 4 {
        return HostCallOutcome::Error("accel_ts_highlight_rows expects 4 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_ts_highlight_rows.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let lang = match expect_str(&args[1], "accel_ts_highlight_rows.lang") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let start_row = match expect_i64(&args[2], "accel_ts_highlight_rows.start_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let end_row = match expect_i64(&args[3], "accel_ts_highlight_rows.end_row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let s = start_row.max(0);
    if end_row < s {
        return HostCallOutcome::Return(NativeValue::List(Vec::new()));
    }
    let rows = lava_accel::ts_highlight_doc_rows(doc_id, &lang, s as usize, end_row as usize);
    HostCallOutcome::Return(encode_ts_highlight_rows(rows))
}

fn handle_accel_ts_highlight_snippet(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error(
            "accel_ts_highlight_snippet expects 2 arguments".to_string(),
        );
    }
    let text = match expect_str(&args[0], "accel_ts_highlight_snippet.text") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let lang = match expect_str(&args[1], "accel_ts_highlight_snippet.lang") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let rows = lava_accel::ts_highlight_snippet(&text, &lang);
    HostCallOutcome::Return(encode_ts_highlight_rows(rows))
}

fn handle_accel_ts_inspect_node(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 4 {
        return HostCallOutcome::Error("accel_ts_inspect_node expects 4 arguments".to_string());
    }
    let doc_id = match expect_i64(&args[0], "accel_ts_inspect_node.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let lang = match expect_str(&args[1], "accel_ts_inspect_node.lang") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let row = match expect_i64(&args[2], "accel_ts_inspect_node.row") {
        Ok(v) => v.max(0) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let col = match expect_i64(&args[3], "accel_ts_inspect_node.col") {
        Ok(v) => v.max(0) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let info = lava_accel::ts_inspect_doc_node(doc_id, &lang, row, col);
    HostCallOutcome::Return(NativeValue::Str(info))
}

fn handle_accel_ts_status(args: &[NativeValue]) -> HostCallOutcome {
    if !args.is_empty() {
        return HostCallOutcome::Error("accel_ts_status expects 0 arguments".to_string());
    }
    let (loaded, abi_ver, grammar_cnt, langs) = lava_accel::ts_status_info();
    let loaded_i64 = i64::from(loaded);
    let langs_list = langs.into_iter().map(NativeValue::Str).collect();
    HostCallOutcome::Return(NativeValue::List(vec![
        NativeValue::I64(loaded_i64),
        NativeValue::I64(abi_ver),
        NativeValue::I64(grammar_cnt),
        NativeValue::List(langs_list),
    ]))
}

fn handle_pty_spawn(ptys: &PtyManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 4 {
        return HostCallOutcome::Error("pty_spawn expects 4 arguments".to_string());
    }
    let shell = match expect_str(&args[0], "pty_spawn.shell") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let cwd = match expect_str(&args[1], "pty_spawn.cwd") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let rows = match expect_i64(&args[2], "pty_spawn.rows") {
        Ok(v) => v.max(2) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let cols = match expect_i64(&args[3], "pty_spawn.cols") {
        Ok(v) => v.max(4) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match ptys.spawn(&shell, &cwd, rows, cols) {
        Ok(id) => HostCallOutcome::Return(NativeValue::I64(id as i64)),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_pty_write(ptys: &PtyManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("pty_write expects 2 arguments".to_string());
    }
    let id = match expect_i64(&args[0], "pty_write.id") {
        Ok(v) if v > 0 => v as u64,
        Ok(v) => return HostCallOutcome::Error(format!("invalid PTY id {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    let data = match expect_str(&args[1], "pty_write.data") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match ptys.write_bytes(id, data.as_bytes()) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_pty_send_key(ptys: &PtyManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 6 {
        return HostCallOutcome::Error("pty_send_key expects 6 arguments".to_string());
    }
    let id = match expect_i64(&args[0], "pty_send_key.id") {
        Ok(v) if v > 0 => v as u64,
        Ok(v) => return HostCallOutcome::Error(format!("invalid PTY id {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    let key = match expect_str(&args[1], "pty_send_key.key") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let text = match expect_str(&args[2], "pty_send_key.text") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let ctrl = match expect_bool(&args[3], "pty_send_key.ctrl") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let alt = match expect_bool(&args[4], "pty_send_key.alt") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let shift = match expect_bool(&args[5], "pty_send_key.shift") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match ptys.send_key(id, &key, &text, ctrl, alt, shift) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_pty_resize(ptys: &PtyManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("pty_resize expects 3 arguments".to_string());
    }
    let id = match expect_i64(&args[0], "pty_resize.id") {
        Ok(v) if v > 0 => v as u64,
        Ok(v) => return HostCallOutcome::Error(format!("invalid PTY id {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    let rows = match expect_i64(&args[1], "pty_resize.rows") {
        Ok(v) => v.max(2) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let cols = match expect_i64(&args[2], "pty_resize.cols") {
        Ok(v) => v.max(4) as usize,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match ptys.resize(id, rows, cols) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_pty_snapshot(ptys: &PtyManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("pty_snapshot expects 2 arguments".to_string());
    }
    let id = match expect_i64(&args[0], "pty_snapshot.id") {
        Ok(v) if v > 0 => v as u64,
        Ok(v) => return HostCallOutcome::Error(format!("invalid PTY id {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    let include_scrollback = match expect_bool(&args[1], "pty_snapshot.include_scrollback") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match ptys.snapshot(id, include_scrollback) {
        Ok(snap) => {
            let lines = snap.lines.into_iter().map(NativeValue::Str).collect();
            let spans = snap
                .spans
                .into_iter()
                .map(|sp| {
                    NativeValue::List(vec![
                        NativeValue::I64(sp.row),
                        NativeValue::I64(sp.start_col),
                        NativeValue::I64(sp.end_col),
                        NativeValue::I64(sp.fg_rgba),
                        NativeValue::I64(sp.bg_rgba),
                        NativeValue::Bool(sp.bold),
                    ])
                })
                .collect();
            HostCallOutcome::Return(NativeValue::List(vec![
                NativeValue::List(lines),
                NativeValue::List(spans),
                NativeValue::I64(snap.cursor_row),
                NativeValue::I64(snap.cursor_col),
                NativeValue::Bool(snap.cursor_visible),
                NativeValue::Bool(snap.alt_screen),
                NativeValue::Str(snap.cwd),
                NativeValue::Str(snap.title),
                NativeValue::Bool(snap.alive),
                NativeValue::I64(snap.exit_code),
            ]))
        }
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_pty_kill(ptys: &PtyManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("pty_kill expects 1 argument".to_string());
    }
    let id = match expect_i64(&args[0], "pty_kill.id") {
        Ok(v) if v > 0 => v as u64,
        _ => return HostCallOutcome::Return(NativeValue::Bool(false)),
    };
    HostCallOutcome::Return(NativeValue::Bool(ptys.kill(id)))
}

fn encode_lsp_edits(edits: Vec<LspTextEditItem>) -> NativeValue {
    NativeValue::List(
        edits
            .into_iter()
            .map(|e| {
                NativeValue::List(vec![
                    NativeValue::Str(e.file_path),
                    NativeValue::I64(e.start_row),
                    NativeValue::I64(e.start_col),
                    NativeValue::I64(e.end_row),
                    NativeValue::I64(e.end_col),
                    NativeValue::Str(e.new_text),
                ])
            })
            .collect(),
    )
}

fn handle_lsp_which(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("lsp_which expects 1 argument".to_string());
    }
    let bin = match expect_str(&args[0], "lsp_which.bin") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::Str(lava_lsp::which_binary(&bin)))
}

fn handle_lsp_find_root(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("lsp_find_root expects 2 arguments".to_string());
    }
    let start_path = match expect_str(&args[0], "lsp_find_root.start_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let raw_markers = match expect_list(&args[1], "lsp_find_root.markers") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let markers = match extract_str_vec(raw_markers, "lsp_find_root.marker") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::Str(lava_lsp::find_project_root(
        &start_path,
        &markers,
    )))
}

fn handle_lsp_spawn(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 5 {
        return HostCallOutcome::Error("lsp_spawn expects 5 arguments".to_string());
    }
    let cmd = match expect_str(&args[0], "lsp_spawn.cmd") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let raw_args = match expect_list(&args[1], "lsp_spawn.args") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let cmd_args = match extract_str_vec(raw_args, "lsp_spawn.arg") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let cwd = match expect_str(&args[2], "lsp_spawn.cwd") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let root_dir = match expect_str(&args[3], "lsp_spawn.root_dir") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let init_json = match expect_str(&args[4], "lsp_spawn.init_options_json") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lsps.spawn_server(&cmd, &cmd_args, &cwd, &root_dir, &init_json) {
        Ok(id) => HostCallOutcome::Return(NativeValue::I64(id as i64)),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_lsp_did_open(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 5 {
        return HostCallOutcome::Error("lsp_did_open expects 5 arguments".to_string());
    }
    let id = match expect_i64(&args[0], "lsp_did_open.id") {
        Ok(v) if v > 0 => v as u64,
        Ok(v) => return HostCallOutcome::Error(format!("invalid LSP session id {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[1], "lsp_did_open.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let language_id = match expect_str(&args[2], "lsp_did_open.language_id") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let version = match expect_i64(&args[3], "lsp_did_open.version") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let text = match expect_str(&args[4], "lsp_did_open.text") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lsps.notify_did_open(id, &file_path, &language_id, version, &text) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_lsp_did_change(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 4 {
        return HostCallOutcome::Error("lsp_did_change expects 4 arguments".to_string());
    }
    let id = match expect_i64(&args[0], "lsp_did_change.id") {
        Ok(v) if v > 0 => v as u64,
        Ok(v) => return HostCallOutcome::Error(format!("invalid LSP session id {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[1], "lsp_did_change.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let version = match expect_i64(&args[2], "lsp_did_change.version") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let full_text = match expect_str(&args[3], "lsp_did_change.full_text") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lsps.notify_did_change(id, &file_path, version, &full_text) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_lsp_did_save(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("lsp_did_save expects 3 arguments".to_string());
    }
    let id = match expect_i64(&args[0], "lsp_did_save.id") {
        Ok(v) if v > 0 => v as u64,
        Ok(v) => return HostCallOutcome::Error(format!("invalid LSP session id {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[1], "lsp_did_save.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let text = match expect_str(&args[2], "lsp_did_save.text") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lsps.notify_did_save(id, &file_path, &text) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_lsp_did_open_doc(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 6 {
        return HostCallOutcome::Error("lsp_did_open_doc expects 6 arguments".to_string());
    }
    let id = match expect_i64(&args[0], "lsp_did_open_doc.client_id") {
        Ok(v) if v > 0 => v as u64,
        _ => return HostCallOutcome::Return(NativeValue::Bool(false)),
    };
    let doc_id = match expect_i64(&args[1], "lsp_did_open_doc.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[2], "lsp_did_open_doc.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let language_id = match expect_str(&args[3], "lsp_did_open_doc.language_id") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let version = match expect_i64(&args[4], "lsp_did_open_doc.version") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let crlf = match expect_bool(&args[5], "lsp_did_open_doc.crlf") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let Some(text) = lava_accel::doc_store::doc_full_text(doc_id, crlf) else {
        return HostCallOutcome::Return(NativeValue::Bool(false));
    };
    let ok = lsps
        .notify_did_open(id, &file_path, &language_id, version, &text)
        .is_ok();
    HostCallOutcome::Return(NativeValue::Bool(ok))
}

fn handle_lsp_did_change_doc(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 5 {
        return HostCallOutcome::Error("lsp_did_change_doc expects 5 arguments".to_string());
    }
    let id = match expect_i64(&args[0], "lsp_did_change_doc.client_id") {
        Ok(v) if v > 0 => v as u64,
        _ => return HostCallOutcome::Return(NativeValue::Bool(false)),
    };
    let doc_id = match expect_i64(&args[1], "lsp_did_change_doc.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[2], "lsp_did_change_doc.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let version = match expect_i64(&args[3], "lsp_did_change_doc.version") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let crlf = match expect_bool(&args[4], "lsp_did_change_doc.crlf") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let Some(text) = lava_accel::doc_store::doc_full_text(doc_id, crlf) else {
        return HostCallOutcome::Return(NativeValue::Bool(false));
    };
    let ok = lsps
        .notify_did_change(id, &file_path, version, &text)
        .is_ok();
    HostCallOutcome::Return(NativeValue::Bool(ok))
}

fn handle_lsp_did_save_doc(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 4 {
        return HostCallOutcome::Error("lsp_did_save_doc expects 4 arguments".to_string());
    }
    let id = match expect_i64(&args[0], "lsp_did_save_doc.client_id") {
        Ok(v) if v > 0 => v as u64,
        _ => return HostCallOutcome::Return(NativeValue::Bool(false)),
    };
    let doc_id = match expect_i64(&args[1], "lsp_did_save_doc.doc_id") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[2], "lsp_did_save_doc.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let crlf = match expect_bool(&args[3], "lsp_did_save_doc.crlf") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let Some(text) = lava_accel::doc_store::doc_full_text(doc_id, crlf) else {
        return HostCallOutcome::Return(NativeValue::Bool(false));
    };
    let ok = lsps.notify_did_save(id, &file_path, &text).is_ok();
    HostCallOutcome::Return(NativeValue::Bool(ok))
}

fn handle_lsp_did_close(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("lsp_did_close expects 2 arguments".to_string());
    }
    let id = match expect_i64(&args[0], "lsp_did_close.id") {
        Ok(v) if v > 0 => v as u64,
        Ok(v) => return HostCallOutcome::Error(format!("invalid LSP session id {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[1], "lsp_did_close.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lsps.notify_did_close(id, &file_path) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_lsp_request(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 6 {
        return HostCallOutcome::Error("lsp_request expects 6 arguments".to_string());
    }
    let id = match expect_i64(&args[0], "lsp_request.id") {
        Ok(v) if v > 0 => v as u64,
        Ok(v) => return HostCallOutcome::Error(format!("invalid LSP session id {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    let kind = match expect_str(&args[1], "lsp_request.kind") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let file_path = match expect_str(&args[2], "lsp_request.file_path") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let row = match expect_i64(&args[3], "lsp_request.row") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let col = match expect_i64(&args[4], "lsp_request.col") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let extra = match expect_str(&args[5], "lsp_request.extra") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lsps.send_request(id, &kind, &file_path, row, col, &extra) {
        Ok(req_id) => HostCallOutcome::Return(NativeValue::I64(req_id)),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_lsp_poll_events(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("lsp_poll_events expects 1 argument".to_string());
    }
    let id = match expect_i64(&args[0], "lsp_poll_events.id") {
        Ok(v) if v > 0 => v as u64,
        Ok(v) => return HostCallOutcome::Error(format!("invalid LSP session id {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    let events = match lsps.poll_events(id) {
        Ok(evs) => evs,
        Err(err) => return HostCallOutcome::Error(err),
    };
    let encoded = events
        .into_iter()
        .map(|ev| match ev {
            LspEvent::Initialized {
                server_name,
                server_version,
                capabilities,
            } => NativeValue::List(vec![
                NativeValue::Str("initialized".to_string()),
                NativeValue::Str(server_name),
                NativeValue::Str(server_version),
                NativeValue::Str(capabilities),
            ]),
            LspEvent::Diagnostics { file_path, items } => {
                let list = items
                    .into_iter()
                    .map(|d| {
                        NativeValue::List(vec![
                            NativeValue::I64(d.row),
                            NativeValue::I64(d.col),
                            NativeValue::I64(d.end_row),
                            NativeValue::I64(d.end_col),
                            NativeValue::Str(d.severity),
                            NativeValue::Str(d.source),
                            NativeValue::Str(d.code),
                            NativeValue::Str(d.message),
                        ])
                    })
                    .collect();
                NativeValue::List(vec![
                    NativeValue::Str("diagnostics".to_string()),
                    NativeValue::Str(file_path),
                    NativeValue::List(list),
                ])
            }
            LspEvent::Hover { req_id, markdown } => NativeValue::List(vec![
                NativeValue::Str("hover".to_string()),
                NativeValue::I64(req_id),
                NativeValue::Str(markdown),
            ]),
            LspEvent::Locations {
                req_id,
                kind,
                items,
            } => {
                let locs = items
                    .into_iter()
                    .map(|lc| {
                        NativeValue::List(vec![
                            NativeValue::Str(lc.file_path),
                            NativeValue::I64(lc.row),
                            NativeValue::I64(lc.col),
                            NativeValue::I64(lc.end_row),
                            NativeValue::I64(lc.end_col),
                        ])
                    })
                    .collect();
                NativeValue::List(vec![
                    NativeValue::Str("locations".to_string()),
                    NativeValue::I64(req_id),
                    NativeValue::Str(kind),
                    NativeValue::List(locs),
                ])
            }
            LspEvent::Symbols { req_id, items } => {
                let syms = items
                    .into_iter()
                    .map(|s| {
                        NativeValue::List(vec![
                            NativeValue::Str(s.name),
                            NativeValue::Str(s.kind),
                            NativeValue::Str(s.detail),
                            NativeValue::I64(s.row),
                            NativeValue::I64(s.col),
                            NativeValue::I64(s.end_row),
                        ])
                    })
                    .collect();
                NativeValue::List(vec![
                    NativeValue::Str("symbols".to_string()),
                    NativeValue::I64(req_id),
                    NativeValue::List(syms),
                ])
            }
            LspEvent::Completions { req_id, items } => {
                let cmps = items
                    .into_iter()
                    .map(|c| {
                        NativeValue::List(vec![
                            NativeValue::Str(c.label),
                            NativeValue::Str(c.insert_text),
                            NativeValue::Str(c.kind),
                            NativeValue::Str(c.detail),
                            NativeValue::Str(c.documentation),
                        ])
                    })
                    .collect();
                NativeValue::List(vec![
                    NativeValue::Str("completions".to_string()),
                    NativeValue::I64(req_id),
                    NativeValue::List(cmps),
                ])
            }
            LspEvent::SignatureHelp {
                req_id,
                label,
                documentation,
                parameters,
                active_param,
            } => {
                let params = parameters.into_iter().map(NativeValue::Str).collect();
                NativeValue::List(vec![
                    NativeValue::Str("signature_help".to_string()),
                    NativeValue::I64(req_id),
                    NativeValue::Str(label),
                    NativeValue::Str(documentation),
                    NativeValue::List(params),
                    NativeValue::I64(active_param),
                ])
            }
            LspEvent::WorkspaceEdit {
                req_id,
                label,
                edits,
            } => NativeValue::List(vec![
                NativeValue::Str("workspace_edit".to_string()),
                NativeValue::I64(req_id),
                NativeValue::Str(label),
                encode_lsp_edits(edits),
            ]),
            LspEvent::CodeActions { req_id, items } => {
                let acts = items
                    .into_iter()
                    .map(|ca| {
                        NativeValue::List(vec![
                            NativeValue::Str(ca.title),
                            NativeValue::Str(ca.kind),
                            encode_lsp_edits(ca.edits),
                        ])
                    })
                    .collect();
                NativeValue::List(vec![
                    NativeValue::Str("code_actions".to_string()),
                    NativeValue::I64(req_id),
                    NativeValue::List(acts),
                ])
            }
            LspEvent::Log { level, message } => NativeValue::List(vec![
                NativeValue::Str("log".to_string()),
                NativeValue::Str(level),
                NativeValue::Str(message),
            ]),
            LspEvent::RequestError {
                req_id,
                method,
                message,
            } => NativeValue::List(vec![
                NativeValue::Str("error".to_string()),
                NativeValue::I64(req_id),
                NativeValue::Str(method),
                NativeValue::Str(message),
            ]),
            LspEvent::Exited { exit_code } => NativeValue::List(vec![
                NativeValue::Str("exited".to_string()),
                NativeValue::I64(exit_code),
            ]),
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(encoded))
}

fn handle_lsp_status(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("lsp_status expects 1 argument".to_string());
    }
    let id = match expect_i64(&args[0], "lsp_status.id") {
        Ok(v) if v > 0 => v as u64,
        Ok(v) => return HostCallOutcome::Error(format!("invalid LSP session id {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lsps.status(id) {
        Ok(st) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::I64(st.id as i64),
            NativeValue::Str(st.command),
            NativeValue::Str(st.root_dir),
            NativeValue::I64(i64::from(st.pid)),
            NativeValue::Bool(st.alive),
            NativeValue::Bool(st.initialized),
            NativeValue::Str(st.server_name),
            NativeValue::Str(st.server_version),
            NativeValue::Str(st.capabilities),
            NativeValue::I64(st.pending_requests as i64),
        ])),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_lsp_stop(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("lsp_stop expects 1 argument".to_string());
    }
    let id = match expect_i64(&args[0], "lsp_stop.id") {
        Ok(v) if v > 0 => v as u64,
        _ => return HostCallOutcome::Return(NativeValue::Bool(false)),
    };
    HostCallOutcome::Return(NativeValue::Bool(lsps.stop(id)))
}

fn handle_lsp_spawn_remote(lsps: &LspManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 6 {
        return HostCallOutcome::Error("lsp_spawn_remote expects 6 arguments".to_string());
    }
    let transport_bin = match expect_str(&args[0], "lsp_spawn_remote.transport_bin") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let raw_args = match expect_list(&args[1], "lsp_spawn_remote.transport_args") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let transport_args = match extract_str_vec(raw_args, "lsp_spawn_remote.transport_arg") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let remote_prefix = match expect_str(&args[2], "lsp_spawn_remote.remote_prefix") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let remote_root_dir = match expect_str(&args[3], "lsp_spawn_remote.remote_root_dir") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let display_server_name = match expect_str(&args[4], "lsp_spawn_remote.display_server_name") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let init_json = match expect_str(&args[5], "lsp_spawn_remote.init_options_json") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match lsps.spawn_remote_server(
        &transport_bin,
        &transport_args,
        &remote_prefix,
        &remote_root_dir,
        &display_server_name,
        &init_json,
    ) {
        Ok(id) => HostCallOutcome::Return(NativeValue::I64(id as i64)),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_srv_rpc_recv(
    stdin_reader: &Mutex<Option<BufReader<std::io::Stdin>>>,
    args: &[NativeValue],
) -> HostCallOutcome {
    if !args.is_empty() {
        return HostCallOutcome::Error("srv_rpc_recv expects 0 arguments".to_string());
    }
    let mut guard = stdin_reader
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let reader = guard.get_or_insert_with(|| BufReader::new(std::io::stdin()));
    match read_rpc_frame(reader) {
        Ok(Some(raw_json)) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::Bool(false),
            NativeValue::Str(raw_json),
        ])),
        Ok(None) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::Bool(true),
            NativeValue::Str(String::new()),
        ])),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_srv_rpc_send(args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("srv_rpc_send expects 1 argument".to_string());
    }
    let raw_json = match expect_str(&args[0], "srv_rpc_send.raw_json") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    match write_rpc_frame(&mut lock, &raw_json) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn encode_remote_conn_info(info: RemoteConnInfo) -> NativeValue {
    NativeValue::List(vec![
        NativeValue::Str(info.authority),
        NativeValue::Str(info.prefix),
        NativeValue::Str(info.mode),
        NativeValue::Bool(info.alive),
        NativeValue::Str(info.hostname),
        NativeValue::Str(info.os),
        NativeValue::Str(info.arch),
        NativeValue::Str(info.remote_home),
        NativeValue::Str(info.remote_cwd),
        NativeValue::Str(info.server_version),
        NativeValue::I64(info.latency_us),
    ])
}

fn handle_remote_register_profile(
    remotes: &RemoteManager,
    args: &[NativeValue],
) -> HostCallOutcome {
    if args.len() != 4 {
        return HostCallOutcome::Error("remote_register_profile expects 4 arguments".to_string());
    }
    let authority = match expect_str(&args[0], "remote_register_profile.authority") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let transport_bin = match expect_str(&args[1], "remote_register_profile.transport_bin") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let raw_targs = match expect_list(&args[2], "remote_register_profile.transport_args") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let transport_args = match extract_str_vec(raw_targs, "remote_register_profile.arg") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let lsp_proxy_bin = match expect_str(&args[3], "remote_register_profile.lsp_proxy_bin") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.register_profile(&authority, &transport_bin, transport_args, &lsp_proxy_bin) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_ssh_hosts(args: &[NativeValue]) -> HostCallOutcome {
    if !args.is_empty() {
        return HostCallOutcome::Error("remote_ssh_hosts expects 0 arguments".to_string());
    }
    let hosts = discover_ssh_config_hosts();
    let list = hosts
        .into_iter()
        .map(|h| {
            NativeValue::List(vec![
                NativeValue::Str(h.alias),
                NativeValue::Str(h.hostname),
                NativeValue::Str(h.user),
                NativeValue::I64(i64::from(h.port)),
                NativeValue::Str(h.identity_file),
                NativeValue::Str(h.proxy_jump),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(list))
}

fn handle_remote_connect(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("remote_connect expects 1 argument".to_string());
    }
    let target = match expect_str(&args[0], "remote_connect.target") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.connect(&target) {
        Ok(info) => HostCallOutcome::Return(encode_remote_conn_info(info)),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_disconnect(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("remote_disconnect expects 1 argument".to_string());
    }
    let target = match expect_str(&args[0], "remote_disconnect.target") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    HostCallOutcome::Return(NativeValue::Bool(remotes.disconnect(&target)))
}

fn handle_remote_list_connections(
    remotes: &RemoteManager,
    args: &[NativeValue],
) -> HostCallOutcome {
    if !args.is_empty() {
        return HostCallOutcome::Error("remote_list_connections expects 0 arguments".to_string());
    }
    let list = remotes
        .list_connections()
        .into_iter()
        .map(encode_remote_conn_info)
        .collect();
    HostCallOutcome::Return(NativeValue::List(list))
}

fn handle_remote_stat(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("remote_stat expects 1 argument".to_string());
    }
    let uri = match expect_str(&args[0], "remote_stat.uri") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.stat(&uri) {
        Ok((exists, is_dir, is_file, size, readonly, rpath)) => {
            HostCallOutcome::Return(NativeValue::List(vec![
                NativeValue::Bool(exists),
                NativeValue::Bool(is_dir),
                NativeValue::Bool(is_file),
                NativeValue::I64(size),
                NativeValue::Bool(readonly),
                NativeValue::Str(rpath),
            ]))
        }
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_read_lines(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("remote_read_lines expects 1 argument".to_string());
    }
    let uri = match expect_str(&args[0], "remote_read_lines.uri") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.read_lines(&uri) {
        Ok((lines, crlf)) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::List(lines.into_iter().map(NativeValue::Str).collect()),
            NativeValue::Bool(crlf),
        ])),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_write_lines(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("remote_write_lines expects 3 arguments".to_string());
    }
    let uri = match expect_str(&args[0], "remote_write_lines.uri") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let raw_lines = match expect_list(&args[1], "remote_write_lines.lines") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let lines = match extract_str_vec(raw_lines, "remote_write_lines.line") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let crlf = match expect_bool(&args[2], "remote_write_lines.crlf") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.write_lines(&uri, &lines, crlf) {
        Ok(bytes) => HostCallOutcome::Return(NativeValue::I64(bytes)),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_readdir(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("remote_readdir expects 2 arguments".to_string());
    }
    let uri = match expect_str(&args[0], "remote_readdir.uri") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let show_hidden = match expect_bool(&args[1], "remote_readdir.show_hidden") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.readdir(&uri, show_hidden) {
        Ok(entries) => {
            let list = entries
                .into_iter()
                .map(|e| {
                    NativeValue::List(vec![
                        NativeValue::Str(e.name),
                        NativeValue::Str(e.full_uri),
                        NativeValue::Str(e.remote_path),
                        NativeValue::Bool(e.is_dir),
                        NativeValue::I64(e.size),
                    ])
                })
                .collect();
            HostCallOutcome::Return(NativeValue::List(list))
        }
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_mkdir(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("remote_mkdir expects 1 argument".to_string());
    }
    let uri = match expect_str(&args[0], "remote_mkdir.uri") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.mkdir(&uri) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_remove(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("remote_remove expects 2 arguments".to_string());
    }
    let uri = match expect_str(&args[0], "remote_remove.uri") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let recursive = match expect_bool(&args[1], "remote_remove.recursive") {
        Ok(b) => b,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.remove(&uri, recursive) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_rename(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("remote_rename expects 2 arguments".to_string());
    }
    let src = match expect_str(&args[0], "remote_rename.src") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let dst = match expect_str(&args[1], "remote_rename.dst") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.rename(&src, &dst) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_copy(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("remote_copy expects 2 arguments".to_string());
    }
    let src = match expect_str(&args[0], "remote_copy.src") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let dst = match expect_str(&args[1], "remote_copy.dst") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.copy_entry(&src, &dst) {
        Ok(()) => HostCallOutcome::Return(NativeValue::Unit),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_which(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("remote_which expects 2 arguments".to_string());
    }
    let target = match expect_str(&args[0], "remote_which.target") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let bin = match expect_str(&args[1], "remote_which.bin") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.which(&target, &bin) {
        Ok(path) => HostCallOutcome::Return(NativeValue::Str(path)),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_find_root(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("remote_find_root expects 2 arguments".to_string());
    }
    let uri = match expect_str(&args[0], "remote_find_root.uri") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let raw_markers = match expect_list(&args[1], "remote_find_root.markers") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let markers = match extract_str_vec(raw_markers, "remote_find_root.marker") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.find_root(&uri, &markers) {
        Ok(root) => HostCallOutcome::Return(NativeValue::Str(root)),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_search_files(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("remote_search_files expects 3 arguments".to_string());
    }
    let uri = match expect_str(&args[0], "remote_search_files.uri") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let query = match expect_str(&args[1], "remote_search_files.query") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let limit = match expect_i64(&args[2], "remote_search_files.limit") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.search_files(&uri, &query, limit) {
        Ok(files) => HostCallOutcome::Return(NativeValue::List(
            files.into_iter().map(NativeValue::Str).collect(),
        )),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_search_grep(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 3 {
        return HostCallOutcome::Error("remote_search_grep expects 3 arguments".to_string());
    }
    let uri = match expect_str(&args[0], "remote_search_grep.uri") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let pattern = match expect_str(&args[1], "remote_search_grep.pattern") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let limit = match expect_i64(&args[2], "remote_search_grep.limit") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.search_grep(&uri, &pattern, limit) {
        Ok(hits) => {
            let list = hits
                .into_iter()
                .map(|(full_uri, row, col, txt)| {
                    NativeValue::List(vec![
                        NativeValue::Str(full_uri),
                        NativeValue::I64(row),
                        NativeValue::I64(col),
                        NativeValue::Str(txt),
                    ])
                })
                .collect();
            HostCallOutcome::Return(NativeValue::List(list))
        }
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_exec(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 4 {
        return HostCallOutcome::Error("remote_exec expects 4 arguments".to_string());
    }
    let target = match expect_str(&args[0], "remote_exec.target") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let cmd = match expect_str(&args[1], "remote_exec.cmd") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let cwd = match expect_str(&args[2], "remote_exec.cwd") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let stdin_data = match expect_str(&args[3], "remote_exec.stdin_data") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.exec_command(&target, &cmd, &cwd, &stdin_data) {
        Ok((code, stdout, stderr)) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::I64(code),
            NativeValue::Str(stdout),
            NativeValue::Str(stderr),
        ])),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_deploy_server(remotes: &RemoteManager, args: &[NativeValue]) -> HostCallOutcome {
    if args.len() != 2 {
        return HostCallOutcome::Error("remote_deploy_server expects 2 arguments".to_string());
    }
    let target = match expect_str(&args[0], "remote_deploy_server.target") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let local_bin = match expect_str(&args[1], "remote_deploy_server.local_server_bin") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.deploy_server(&target, &local_bin) {
        Ok(info) => HostCallOutcome::Return(encode_remote_conn_info(info)),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_lsp_launch_command(
    remotes: &RemoteManager,
    args: &[NativeValue],
) -> HostCallOutcome {
    if args.len() != 4 {
        return HostCallOutcome::Error("remote_lsp_launch_command expects 4 arguments".to_string());
    }
    let target = match expect_str(&args[0], "remote_lsp_launch_command.target") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let remote_root = match expect_str(&args[1], "remote_lsp_launch_command.remote_root") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let server_cmd = match expect_str(&args[2], "remote_lsp_launch_command.server_cmd") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let raw_sargs = match expect_list(&args[3], "remote_lsp_launch_command.server_args") {
        Ok(l) => l,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let server_args = match extract_str_vec(raw_sargs, "remote_lsp_launch_command.arg") {
        Ok(v) => v,
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.lsp_launch_command(&target, &remote_root, &server_cmd, &server_args) {
        Ok((tbin, targs, prefix, clean_root)) => HostCallOutcome::Return(NativeValue::List(vec![
            NativeValue::Str(tbin),
            NativeValue::List(targs.into_iter().map(NativeValue::Str).collect()),
            NativeValue::Str(prefix),
            NativeValue::Str(clean_root),
        ])),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_port_forward_start(
    remotes: &RemoteManager,
    args: &[NativeValue],
) -> HostCallOutcome {
    if args.len() != 5 {
        return HostCallOutcome::Error("remote_port_forward_start expects 5 arguments".to_string());
    }
    let target = match expect_str(&args[0], "remote_port_forward_start.target") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let direction = match expect_str(&args[1], "remote_port_forward_start.direction") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let local_port = match expect_i64(&args[2], "remote_port_forward_start.local_port") {
        Ok(v) if (1..=65535).contains(&v) => v as u16,
        Ok(v) => return HostCallOutcome::Error(format!("invalid local port {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    let remote_host = match expect_str(&args[3], "remote_port_forward_start.remote_host") {
        Ok(s) => s,
        Err(e) => return HostCallOutcome::Error(e),
    };
    let remote_port = match expect_i64(&args[4], "remote_port_forward_start.remote_port") {
        Ok(v) if (1..=65535).contains(&v) => v as u16,
        Ok(v) => return HostCallOutcome::Error(format!("invalid remote port {v}")),
        Err(e) => return HostCallOutcome::Error(e),
    };
    match remotes.start_port_forward(&target, &direction, local_port, &remote_host, remote_port) {
        Ok(id) => HostCallOutcome::Return(NativeValue::I64(id as i64)),
        Err(err) => HostCallOutcome::Error(err),
    }
}

fn handle_remote_port_forward_stop(
    remotes: &RemoteManager,
    args: &[NativeValue],
) -> HostCallOutcome {
    if args.len() != 1 {
        return HostCallOutcome::Error("remote_port_forward_stop expects 1 argument".to_string());
    }
    let id = match expect_i64(&args[0], "remote_port_forward_stop.id") {
        Ok(v) if v > 0 => v as u64,
        _ => return HostCallOutcome::Return(NativeValue::Bool(false)),
    };
    HostCallOutcome::Return(NativeValue::Bool(remotes.stop_port_forward(id)))
}

fn handle_remote_port_forward_list(
    remotes: &RemoteManager,
    args: &[NativeValue],
) -> HostCallOutcome {
    if !args.is_empty() {
        return HostCallOutcome::Error("remote_port_forward_list expects 0 arguments".to_string());
    }
    let list = remotes
        .list_port_forwards()
        .into_iter()
        .map(|f| {
            NativeValue::List(vec![
                NativeValue::I64(f.id as i64),
                NativeValue::Str(f.authority),
                NativeValue::Str(f.direction),
                NativeValue::I64(i64::from(f.local_port)),
                NativeValue::Str(f.remote_host),
                NativeValue::I64(i64::from(f.remote_port)),
                NativeValue::Bool(f.alive),
            ])
        })
        .collect();
    HostCallOutcome::Return(NativeValue::List(list))
}

/// Drive `vm` inside a single continuous Nessa `Scheduler` + `libuv` session, waking
/// `ui_wait_events` directly through [`Scheduler::wake`] whenever [`EventQueue`] or the
/// shared deadline timer wheel fires.
pub fn drive_vm_loop(vm: &mut Vm, controller: &HostWaitController) -> Result<i32, String> {
    controller.attach_scheduler(vm.scheduler_snapshot_handle());
    let run_res = vm.run();
    controller.detach_scheduler();
    controller.jobs.cancel_all();
    controller.ptys.kill_all();
    controller.lsps.stop_all();
    controller.remotes.disconnect_all();

    match run_res {
        VmResult::Error(err) => Err(format!("VM runtime error: {err}")),
        VmResult::Finished => {
            if vm.has_suspended_tasks() {
                Err("VM halted with suspended tasks and no pending host event waiters".to_string())
            } else {
                Ok(controller.event_queue.quit_code().unwrap_or(0))
            }
        }
    }
}
