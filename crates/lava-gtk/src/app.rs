//! Dedicated GTK4 window, event controllers, system clipboard sync, and GLib main-context driver.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString};
use std::mem::transmute;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use lava_proto::{EventQueue, FrameSlot, MouseAction, PointerStyle, UiEvent};
use libc::{c_double, c_int, c_uint, c_void};

use crate::anim::{AnimController, ScrollDeltaAccumulator};
use crate::ffi::{
    self, CairoT, G_PRIORITY_DEFAULT, G_PRIORITY_DEFAULT_IDLE, G_SOURCE_CONTINUE, G_SOURCE_REMOVE,
    GClosure, GTK_EVENT_CONTROLLER_SCROLL_BOTH_AXES, GdkFrameClock, GtkDrawingArea,
    GtkEventController, GtkGesture, GtkGestureSingle, GtkWidget, GtkWindow,
};
use crate::keymap::{translate_gdk_key, translate_modifiers};

static GTK_DISPLAY_ACTIVE: AtomicBool = AtomicBool::new(false);
static CLIPBOARD_MIRROR: OnceLock<Mutex<String>> = OnceLock::new();

fn clipboard_mirror() -> &'static Mutex<String> {
    CLIPBOARD_MIRROR.get_or_init(|| Mutex::new(String::new()))
}

/// Configuration for bootstrapping the native GTK4 host window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GtkAppConfig {
    pub initial_title: String,
    pub initial_width: i32,
    pub initial_height: i32,
}

impl Default for GtkAppConfig {
    fn default() -> Self {
        Self {
            initial_title: "Lava".to_string(),
            initial_width: 1280,
            initial_height: 800,
        }
    }
}

/// Checks whether GTK4 can initialize a display connection (Wayland, X11, or Broadway).
pub fn is_display_available() -> bool {
    // SAFETY: `gtk_init_check` takes no pointers and safely probes available GDK display backends.
    let ok = unsafe { ffi::gtk_init_check() } != 0;
    if ok {
        GTK_DISPLAY_ACTIVE.store(true, Ordering::Release);
    }
    ok
}

unsafe extern "C" fn apply_clipboard_idle(user_data: *mut c_void) -> c_int {
    if !user_data.is_null() {
        // SAFETY: `user_data` points to a heap-allocated `CString` created in `set_system_clipboard`
        // that stays alive until `drop_cstring_notify` runs after this callback returns.
        let text_cstr = unsafe { &*(user_data.cast::<CString>()) };
        // SAFETY: Executed on the default GLib main context thread; GDK display/clipboard pointers
        // are checked for null before invoking `gdk_clipboard_set_text`.
        unsafe {
            let display = ffi::gdk_display_get_default();
            if !display.is_null() {
                let clipboard = ffi::gdk_display_get_clipboard(display);
                if !clipboard.is_null() {
                    ffi::gdk_clipboard_set_text(clipboard, text_cstr.as_ptr());
                }
            }
        }
    }
    G_SOURCE_REMOVE
}

unsafe extern "C" fn drop_cstring_notify(data: *mut c_void) {
    if !data.is_null() {
        // SAFETY: `data` was allocated via `Box::into_raw(Box::<CString>::new(...))` and GLib
        // invokes `GDestroyNotify` at most once when removing the idle source.
        unsafe {
            drop(Box::from_raw(data.cast::<CString>()));
        }
    }
}

/// Updates the in-process clipboard mirror and schedules a GTK4 system clipboard write if active.
pub fn set_system_clipboard(text: &str) {
    {
        let mut guard = clipboard_mirror()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        guard.clear();
        guard.push_str(text);
    }

    if GTK_DISPLAY_ACTIVE.load(Ordering::Acquire)
        && let Ok(cstr) = CString::new(text)
    {
        let raw = Box::into_raw(Box::new(cstr)).cast::<c_void>();
        // SAFETY: `g_idle_add_full` is thread-safe in GLib; `raw` is owned by the source and
        // reclaimed exactly once by `drop_cstring_notify`.
        unsafe {
            ffi::g_idle_add_full(
                G_PRIORITY_DEFAULT_IDLE,
                Some(apply_clipboard_idle),
                raw,
                Some(drop_cstring_notify),
            );
        }
    }
}

/// Reads the current text from Lava's thread-safe clipboard mirror.
pub fn get_system_clipboard() -> String {
    let guard = clipboard_mirror()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    guard.clone()
}

fn pointer_cursor_cstr(style: PointerStyle) -> &'static CStr {
    match style {
        PointerStyle::Default => c"default",
        PointerStyle::Text => c"text",
        PointerStyle::Pointer => c"pointer",
        PointerStyle::ResizeHorizontal => c"ew-resize",
        PointerStyle::ResizeVertical => c"ns-resize",
    }
}

struct UiSharedState {
    frame_slot: Arc<FrameSlot>,
    event_queue: Arc<EventQueue>,
    alive: AtomicBool,
    wakeup_queued: AtomicBool,
    epoch: Instant,
    window_addr: usize,
    drawing_area_addr: usize,
    applied_meta: Mutex<(String, PointerStyle, bool)>,
    pointer_pos: Mutex<(f64, f64)>,
    scroll_accum: Mutex<ScrollDeltaAccumulator>,
}

/// UI-thread-only animation state shared exclusively between `GtkDrawingArea`'s draw and VSync
/// tick callbacks on the GTK main-context thread.
struct GtkAnimSlot {
    shared: Arc<UiSharedState>,
    anim: RefCell<AnimController>,
    tick_registered: Cell<bool>,
}

impl GtkAnimSlot {
    /// Ensures a VSync tick callback is registered on `drawing_area` while animations run.
    ///
    /// # Safety
    /// `slot_ptr` must be a live pointer originally produced by `Rc::into_raw` on the GTK main thread.
    unsafe fn ensure_tick_callback(slot_ptr: *const Self) {
        // SAFETY: `slot_ptr` is a live `GtkAnimSlot` on the single GTK main thread.
        let slot = unsafe { &*slot_ptr };
        if !slot.shared.alive.load(Ordering::Acquire) {
            return;
        }
        let drawing_area = slot.shared.drawing_area_addr as *mut GtkWidget;
        if drawing_area.is_null() {
            return;
        }
        if !slot.tick_registered.replace(true) {
            // SAFETY: `slot_ptr` was created via `Rc::into_raw` on this thread and remains live;
            // incrementing its strong count gives `gtk_widget_add_tick_callback` an independent
            // owned reference reclaimed once by `drop_anim_slot_source`.
            unsafe {
                Rc::increment_strong_count(slot_ptr);
                ffi::gtk_widget_add_tick_callback(
                    drawing_area,
                    Some(on_drawing_area_tick),
                    slot_ptr.cast_mut().cast::<c_void>(),
                    Some(drop_anim_slot_source),
                );
            }
        }
    }
}

impl UiSharedState {
    /// Applies updated window title, cursor hint, and redraw invalidation on the GTK main thread.
    ///
    /// # Safety
    /// Must only be called on the GTK main-context thread (`run_gtk_app`).
    unsafe fn sync_window_and_queue_draw(&self) {
        if !self.alive.load(Ordering::Acquire) {
            return;
        }

        let window = self.window_addr as *mut GtkWindow;
        let drawing_area = self.drawing_area_addr as *mut GtkWidget;
        if window.is_null() || drawing_area.is_null() {
            return;
        }

        let frame = self.frame_slot.latest();
        let mut title_to_apply: Option<CString> = None;
        let mut cursor_to_apply: Option<PointerStyle> = None;
        let mut decorated_to_apply: Option<bool> = None;

        {
            let mut meta = self
                .applied_meta
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            if meta.0 != frame.window.title {
                meta.0.clone_from(&frame.window.title);
                let sanitized = if frame.window.title.contains('\0') {
                    frame.window.title.replace('\0', "")
                } else {
                    frame.window.title.clone()
                };
                title_to_apply = CString::new(sanitized).ok();
            }
            if meta.1 != frame.window.pointer {
                meta.1 = frame.window.pointer;
                cursor_to_apply = Some(frame.window.pointer);
            }
            if meta.2 != frame.window.decorated {
                meta.2 = frame.window.decorated;
                decorated_to_apply = Some(frame.window.decorated);
            }
        }

        if let Some(title_cstr) = title_to_apply {
            // SAFETY: `self.alive` was checked on the GTK main thread before `gtk_window_destroy`,
            // so `window` is a live `GtkWindow` and `title_cstr` is a valid NUL-terminated string.
            unsafe {
                ffi::gtk_window_set_title(window, title_cstr.as_ptr());
            }
        }

        if let Some(decorated) = decorated_to_apply {
            // SAFETY: `window` is a live `GtkWindow` accessed on the GTK main thread.
            unsafe {
                ffi::gtk_window_set_decorated(window, c_int::from(decorated));
            }
        }

        if let Some(style) = cursor_to_apply {
            let cursor_name = pointer_cursor_cstr(style);
            // SAFETY: `drawing_area` is a live `GtkWidget` on the GTK thread and `cursor_name` is static.
            unsafe {
                ffi::gtk_widget_set_cursor_from_name(drawing_area, cursor_name.as_ptr());
            }
        }

        // SAFETY: `drawing_area` is a live `GtkWidget` accessed on the GTK main thread.
        unsafe {
            ffi::gtk_widget_queue_draw(drawing_area);
        }
    }
}

unsafe extern "C" fn drop_state_source(data: *mut c_void) {
    if !data.is_null() {
        // SAFETY: `data` was created by `Arc::into_raw` when registering a GLib source or draw func
        // and GLib invokes `GDestroyNotify` once when tearing down the registration.
        unsafe {
            drop(Arc::from_raw(data.cast::<UiSharedState>()));
        }
    }
}

unsafe extern "C" fn drop_state_closure(data: *mut c_void, _closure: *mut GClosure) {
    if !data.is_null() {
        // SAFETY: `data` was created by `Arc::into_raw` in `connect_signal` and GObject invokes
        // `GClosureNotify` once when the signal closure is finalized.
        unsafe {
            drop(Arc::from_raw(data.cast::<UiSharedState>()));
        }
    }
}

unsafe extern "C" fn on_frame_wakeup_idle(user_data: *mut c_void) -> c_int {
    if !user_data.is_null() {
        // SAFETY: `user_data` is a borrowed `&UiSharedState` kept alive by `Arc` until
        // `drop_state_source` runs on source removal, and GLib executes this on the UI thread.
        let state = unsafe { &*(user_data.cast::<UiSharedState>()) };
        state.wakeup_queued.store(false, Ordering::Release);
        // SAFETY: Executed on the default GLib main context thread owned by `run_gtk_app`.
        unsafe {
            state.sync_window_and_queue_draw();
        }
    }
    G_SOURCE_REMOVE
}

unsafe extern "C" fn on_quit_wakeup_idle(_user_data: *mut c_void) -> c_int {
    G_SOURCE_REMOVE
}

fn schedule_quit_wakeup(state: &Arc<UiSharedState>) {
    if !state.alive.load(Ordering::Acquire) {
        return;
    }
    let raw = Arc::into_raw(Arc::clone(state)).cast_mut().cast::<c_void>();
    // SAFETY: `g_idle_add_full` is thread-safe and wakes a blocked `g_main_context_iteration`
    // immediately; `raw` is reclaimed once by `drop_state_source` when the idle source runs.
    unsafe {
        ffi::g_idle_add_full(
            G_PRIORITY_DEFAULT,
            Some(on_quit_wakeup_idle),
            raw,
            Some(drop_state_source),
        );
    }
}

fn schedule_ui_wakeup(state: &Arc<UiSharedState>) {
    if !state.alive.load(Ordering::Acquire) {
        return;
    }
    if !state.wakeup_queued.swap(true, Ordering::AcqRel) {
        let raw = Arc::into_raw(Arc::clone(state)).cast_mut().cast::<c_void>();
        // SAFETY: `g_idle_add_full` is thread-safe; `raw` is an owned `Arc<UiSharedState>` released
        // by `drop_state_source` once `on_frame_wakeup_idle` completes or the source is removed.
        unsafe {
            ffi::g_idle_add_full(
                G_PRIORITY_DEFAULT_IDLE,
                Some(on_frame_wakeup_idle),
                raw,
                Some(drop_state_source),
            );
        }
    }
}

unsafe extern "C" fn drop_anim_slot_source(data: *mut c_void) {
    if !data.is_null() {
        // SAFETY: `data` is an owned `Rc<GtkAnimSlot>` pointer created via `Rc::into_raw` /
        // `Rc::increment_strong_count` on the GTK main thread and finalized once by GTK.
        unsafe {
            drop(Rc::from_raw(data.cast::<GtkAnimSlot>()));
        }
    }
}

unsafe extern "C" fn on_drawing_area_tick(
    widget: *mut GtkWidget,
    frame_clock: *mut GdkFrameClock,
    user_data: *mut c_void,
) -> c_int {
    if widget.is_null() || user_data.is_null() {
        return G_SOURCE_REMOVE;
    }
    // SAFETY: `user_data` is a borrowed `&GtkAnimSlot` on the GTK main thread kept alive until
    // `drop_anim_slot_source` runs.
    let slot = unsafe { &*(user_data.cast::<GtkAnimSlot>()) };
    if !slot.shared.alive.load(Ordering::Acquire) {
        slot.tick_registered.set(false);
        return G_SOURCE_REMOVE;
    }

    let now_sec = if !frame_clock.is_null() {
        // SAFETY: `frame_clock` was supplied by GTK4's tick dispatcher on the UI thread.
        let frame_us = unsafe { ffi::gdk_frame_clock_get_frame_time(frame_clock) };
        if frame_us > 0 {
            (frame_us as f64) * 1e-6
        } else {
            slot.shared.epoch.elapsed().as_secs_f64()
        }
    } else {
        slot.shared.epoch.elapsed().as_secs_f64()
    };

    let (still_active, should_queue_draw) = slot.anim.borrow_mut().tick_at(now_sec);

    if should_queue_draw {
        // SAFETY: `widget` (`drawing_area`) is a live `GtkWidget` on the GTK main thread;
        // `gtk_widget_queue_draw` is ONLY invoked while cursor or scroll animation is unsettled
        // (plus the single final snap frame), keeping idle CPU at 0%.
        unsafe {
            ffi::gtk_widget_queue_draw(widget);
        }
    }

    if still_active {
        G_SOURCE_CONTINUE
    } else {
        slot.tick_registered.set(false);
        G_SOURCE_REMOVE
    }
}

unsafe extern "C" fn on_drawing_area_draw(
    area: *mut GtkDrawingArea,
    cr: *mut CairoT,
    width: c_int,
    height: c_int,
    user_data: *mut c_void,
) {
    if cr.is_null() || user_data.is_null() {
        return;
    }
    let widget_scale = if !area.is_null() {
        // SAFETY: `area` is a valid `GtkDrawingArea` (subclass of `GtkWidget`).
        f64::from(unsafe { ffi::gtk_widget_get_scale_factor(area.cast::<GtkWidget>()) }.max(1))
    } else {
        1.0
    };
    let slot_ptr = user_data.cast::<GtkAnimSlot>().cast_const();
    // SAFETY: `slot_ptr` is a valid `GtkAnimSlot` owned by `drawing_area`'s draw callback on the
    // GTK main thread, and `cr` is exclusively owned by GTK for this draw invocation.
    let slot = unsafe { &*slot_ptr };
    let frame = slot.shared.frame_slot.latest();
    let hints = slot.shared.frame_slot.anim_hints();
    let transfer_overlay = slot.shared.frame_slot.transfer_overlay();
    let anim_active = {
        let mut anim = slot.anim.borrow_mut();
        // SAFETY: `cr` is valid and exclusively owned by GTK for this draw callback invocation.
        unsafe {
            anim.render_to_cairo_scaled_with_overlay(
                cr,
                (width, height),
                widget_scale,
                &frame,
                &hints,
                transfer_overlay.as_ref(),
            );
        }
        anim.is_active()
    };
    let overlay_anim = transfer_overlay.as_ref().is_some_and(|o| o.active);
    if anim_active || overlay_anim {
        // SAFETY: `slot_ptr` was registered via `Rc::into_raw` on this GTK main thread.
        unsafe {
            GtkAnimSlot::ensure_tick_callback(slot_ptr);
        }
    }
}

unsafe extern "C" fn on_drawing_area_resize(
    area: *mut GtkDrawingArea,
    width: c_int,
    height: c_int,
    user_data: *mut c_void,
) {
    if area.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: `area` is a `GtkDrawingArea` (subclass of `GtkWidget`) and `user_data` is valid.
    let (state, scale_factor) = unsafe {
        let scale = ffi::gtk_widget_get_scale_factor(area.cast::<GtkWidget>()).max(1);
        (&*(user_data.cast::<UiSharedState>()), scale)
    };
    state.event_queue.push(UiEvent::Resize {
        width: width.max(1),
        height: height.max(1),
        scale_factor,
    });
}

unsafe extern "C" fn on_key_pressed(
    _controller: *mut GtkEventController,
    keyval: c_uint,
    _keycode: c_uint,
    gdk_state: c_uint,
    user_data: *mut c_void,
) -> c_int {
    if user_data.is_null() {
        return 0;
    }
    // SAFETY: `user_data` is a valid `UiSharedState` pointer managed by the signal closure.
    let state = unsafe { &*(user_data.cast::<UiSharedState>()) };
    if let Some(ev) = translate_gdk_key(keyval, gdk_state) {
        state.event_queue.push(ev);
        1
    } else {
        0
    }
}

unsafe extern "C" fn on_click_pressed(
    gesture: *mut GtkGesture,
    n_press: c_int,
    x: c_double,
    y: c_double,
    user_data: *mut c_void,
) {
    if gesture.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: `gesture` is a `GtkGestureClick` (subclass of `GtkGestureSingle` & `GtkEventController`)
    // and `user_data` is a valid `UiSharedState` reference on the GTK thread.
    let (state, button, raw_mods) = unsafe {
        let st = &*(user_data.cast::<UiSharedState>());
        if st.alive.load(Ordering::Acquire) {
            let area = st.drawing_area_addr as *mut GtkWidget;
            if !area.is_null() {
                ffi::gtk_widget_grab_focus(area);
            }
        }
        let btn = ffi::gtk_gesture_single_get_current_button(gesture.cast::<GtkGestureSingle>());
        let m =
            ffi::gtk_event_controller_get_current_event_state(gesture.cast::<GtkEventController>());
        (st, btn, m)
    };

    {
        let mut pos = state
            .pointer_pos
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        *pos = (x, y);
    }

    state.event_queue.push(UiEvent::MouseButton {
        action: MouseAction::Press,
        button,
        x,
        y,
        modifiers: translate_modifiers(raw_mods),
        press_count: n_press,
    });
}

unsafe extern "C" fn on_click_released(
    gesture: *mut GtkGesture,
    n_press: c_int,
    x: c_double,
    y: c_double,
    user_data: *mut c_void,
) {
    if gesture.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: `gesture` is a `GtkGestureClick` and `user_data` is a valid `UiSharedState`.
    let (state, button, raw_mods) = unsafe {
        let st = &*(user_data.cast::<UiSharedState>());
        let btn = ffi::gtk_gesture_single_get_current_button(gesture.cast::<GtkGestureSingle>());
        let m =
            ffi::gtk_event_controller_get_current_event_state(gesture.cast::<GtkEventController>());
        (st, btn, m)
    };

    {
        let mut pos = state
            .pointer_pos
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        *pos = (x, y);
    }

    state.event_queue.push(UiEvent::MouseButton {
        action: MouseAction::Release,
        button,
        x,
        y,
        modifiers: translate_modifiers(raw_mods),
        press_count: n_press,
    });
}

unsafe extern "C" fn on_pointer_motion(
    controller: *mut GtkEventController,
    x: c_double,
    y: c_double,
    user_data: *mut c_void,
) {
    if controller.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: `controller` is a valid `GtkEventControllerMotion` and `user_data` is valid.
    let (state, raw_mods) = unsafe {
        (
            &*(user_data.cast::<UiSharedState>()),
            ffi::gtk_event_controller_get_current_event_state(controller),
        )
    };
    let should_emit = {
        let mut pos = state
            .pointer_pos
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let (prev_x, prev_y) = *pos;
        *pos = (x, y);
        let button_held = (raw_mods & 0x0700) != 0;
        let cell_changed =
            (x as i32 / 8) != (prev_x as i32 / 8) || (y as i32 / 8) != (prev_y as i32 / 8);
        button_held || cell_changed
    };
    if should_emit {
        state.event_queue.push(UiEvent::MouseMove {
            x,
            y,
            modifiers: translate_modifiers(raw_mods),
        });
    }
}

unsafe extern "C" fn on_pointer_scroll(
    controller: *mut GtkEventController,
    dx: c_double,
    dy: c_double,
    user_data: *mut c_void,
) -> c_int {
    if controller.is_null() || user_data.is_null() {
        return 0;
    }
    // SAFETY: `controller` is a valid `GtkEventControllerScroll` and `user_data` is valid.
    let (state, raw_mods) = unsafe {
        (
            &*(user_data.cast::<UiSharedState>()),
            ffi::gtk_event_controller_get_current_event_state(controller),
        )
    };
    let now_sec = state.epoch.elapsed().as_secs_f64();
    let emitted = {
        let mut accum = state
            .scroll_accum
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        accum.accumulate(dx, dy, now_sec)
    };
    if let Some((step_dx, step_dy)) = emitted {
        let (x, y) = *state
            .pointer_pos
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        state.event_queue.push(UiEvent::MouseScroll {
            dx: step_dx,
            dy: step_dy,
            x,
            y,
            modifiers: translate_modifiers(raw_mods),
        });
    }
    1
}

unsafe extern "C" fn on_focus_enter(_controller: *mut GtkEventController, user_data: *mut c_void) {
    if !user_data.is_null() {
        // SAFETY: `user_data` is a valid `UiSharedState` pointer managed by the signal closure.
        let state = unsafe { &*(user_data.cast::<UiSharedState>()) };
        state.event_queue.push(UiEvent::Focus { focused: true });
    }
}

unsafe extern "C" fn on_focus_leave(_controller: *mut GtkEventController, user_data: *mut c_void) {
    if !user_data.is_null() {
        // SAFETY: `user_data` is a valid `UiSharedState` pointer managed by the signal closure.
        let state = unsafe { &*(user_data.cast::<UiSharedState>()) };
        state.event_queue.push(UiEvent::Focus { focused: false });
    }
}

unsafe extern "C" fn on_window_close_request(
    _window: *mut GtkWindow,
    user_data: *mut c_void,
) -> c_int {
    if !user_data.is_null() {
        // SAFETY: `user_data` is a valid `UiSharedState` pointer managed by the signal closure.
        let state = unsafe { &*(user_data.cast::<UiSharedState>()) };
        state.event_queue.push(UiEvent::CloseRequested);
    }
    // Stop default GTK window destruction so Nessa decides whether/when to quit cleanly.
    1
}

fn connect_signal(
    instance: *mut c_void,
    signal_name: &'static CStr,
    callback: ffi::GCallback,
    state: &Arc<UiSharedState>,
) {
    let raw_state = Arc::into_raw(Arc::clone(state)).cast_mut().cast::<c_void>();
    // SAFETY: `instance` is a valid GObject instance on this thread, `signal_name` is a static C
    // string, `callback` matches the GSignal C ABI signature, and `drop_state_closure` reclaims `raw_state`.
    unsafe {
        ffi::g_signal_connect_data(
            instance,
            signal_name.as_ptr(),
            callback,
            raw_state,
            Some(drop_state_closure),
            0,
        );
    }
}

/// Initializes GTK4, creates the host window and drawing canvas, wires input controllers and frame
/// wakeups, and runs the GLib main loop on the calling thread until `event_queue.quit_code().is_some()`.
pub fn run_gtk_app(
    config: GtkAppConfig,
    frame_slot: Arc<FrameSlot>,
    event_queue: Arc<EventQueue>,
) -> Result<(), String> {
    run_gtk_app_with_final_size(config, frame_slot, event_queue, None)
}

/// Thread-safe slot capturing final `(width, height)` on clean GTK4 window shutdown.
pub type FinalWindowSizeSlot = Arc<Mutex<Option<(i32, i32)>>>;

pub fn run_gtk_app_with_final_size(
    config: GtkAppConfig,
    frame_slot: Arc<FrameSlot>,
    event_queue: Arc<EventQueue>,
    final_window_size: Option<FinalWindowSizeSlot>,
) -> Result<(), String> {
    if !is_display_available() {
        return Err(
            "GTK4 failed to initialize display (ensure WAYLAND_DISPLAY, DISPLAY, or BROADWAY_DISPLAY is set)"
                .to_string(),
        );
    }

    let latest_initial = frame_slot.latest();
    let (init_title, init_width, init_height, init_pointer, init_decorated) =
        if latest_initial.generation > 0 {
            (
                latest_initial.window.title.clone(),
                latest_initial.window.width.max(1),
                latest_initial.window.height.max(1),
                latest_initial.window.pointer,
                latest_initial.window.decorated,
            )
        } else {
            (
                config.initial_title,
                config.initial_width.max(1),
                config.initial_height.max(1),
                PointerStyle::Default,
                false,
            )
        };

    // SAFETY: GTK4 has been initialized on this thread via `is_display_available()`.
    let (window_widget, drawing_area) =
        unsafe { (ffi::gtk_window_new(), ffi::gtk_drawing_area_new()) };
    if window_widget.is_null() || drawing_area.is_null() {
        if !window_widget.is_null() {
            // SAFETY: `window_widget` is non-null and not yet presented; destroy to avoid leak.
            unsafe {
                ffi::gtk_window_destroy(window_widget.cast::<GtkWindow>());
            }
        }
        return Err("failed to create GTK4 window or GtkDrawingArea".to_string());
    }
    let window = window_widget.cast::<GtkWindow>();

    let sanitized_title = if init_title.contains('\0') {
        init_title.replace('\0', "")
    } else {
        init_title.clone()
    };
    let title_cstr =
        CString::new(sanitized_title).unwrap_or_else(|_| CString::new("Lava").unwrap_or_default());

    // SAFETY: `window` and `drawing_area` are newly created GTK4 widgets on the UI thread, and
    // `title_cstr` / cursor name are valid NUL-terminated strings.
    unsafe {
        ffi::gtk_window_set_title(window, title_cstr.as_ptr());
        ffi::gtk_window_set_decorated(window, c_int::from(init_decorated));
        ffi::gtk_window_set_default_size(window, init_width, init_height);
        ffi::gtk_widget_set_hexpand(drawing_area, 1);
        ffi::gtk_widget_set_vexpand(drawing_area, 1);
        ffi::gtk_widget_set_focusable(drawing_area, 1);
        ffi::gtk_widget_set_can_focus(drawing_area, 1);
        ffi::gtk_widget_set_cursor_from_name(
            drawing_area,
            pointer_cursor_cstr(init_pointer).as_ptr(),
        );
    }

    let state = Arc::new(UiSharedState {
        frame_slot: Arc::clone(&frame_slot),
        event_queue: Arc::clone(&event_queue),
        alive: AtomicBool::new(true),
        wakeup_queued: AtomicBool::new(false),
        epoch: Instant::now(),
        window_addr: window as usize,
        drawing_area_addr: drawing_area as usize,
        applied_meta: Mutex::new((init_title, init_pointer, init_decorated)),
        pointer_pos: Mutex::new((0.0, 0.0)),
        scroll_accum: Mutex::new(ScrollDeltaAccumulator::new()),
    });

    // Attach Cairo draw callback and VSync animation tick callback on `GtkDrawingArea`.
    {
        let anim_slot = Rc::new(GtkAnimSlot {
            shared: Arc::clone(&state),
            anim: RefCell::new(AnimController::new()),
            tick_registered: Cell::new(false),
        });
        let slot_ptr = Rc::into_raw(anim_slot);
        // SAFETY: `drawing_area` is a valid `GtkDrawingArea` on the UI thread; `drop_anim_slot_source`
        // releases `slot_ptr`, and `ensure_tick_callback` attaches the VSync tick callback.
        unsafe {
            ffi::gtk_drawing_area_set_draw_func(
                drawing_area.cast::<GtkDrawingArea>(),
                Some(on_drawing_area_draw),
                slot_ptr.cast_mut().cast::<c_void>(),
                Some(drop_anim_slot_source),
            );
            GtkAnimSlot::ensure_tick_callback(slot_ptr);
        }
    }

    // Attach resize and close-request signals.
    // SAFETY: Every `transmute` below converts a concrete `unsafe extern "C" fn(...)` pointer to
    // `unsafe extern "C" fn()` (`GCallback`), as required by GObject's `g_signal_connect_data`.
    unsafe {
        let resize_cb = transmute::<
            unsafe extern "C" fn(*mut GtkDrawingArea, c_int, c_int, *mut c_void),
            unsafe extern "C" fn(),
        >(on_drawing_area_resize);
        connect_signal(drawing_area.cast(), c"resize", Some(resize_cb), &state);

        let close_cb = transmute::<
            unsafe extern "C" fn(*mut GtkWindow, *mut c_void) -> c_int,
            unsafe extern "C" fn(),
        >(on_window_close_request);
        connect_signal(window.cast(), c"close-request", Some(close_cb), &state);

        // Key controller (attached to window so keys arrive regardless of child focus races).
        let key_ctrl = ffi::gtk_event_controller_key_new();
        if !key_ctrl.is_null() {
            let key_cb = transmute::<
                unsafe extern "C" fn(
                    *mut GtkEventController,
                    c_uint,
                    c_uint,
                    c_uint,
                    *mut c_void,
                ) -> c_int,
                unsafe extern "C" fn(),
            >(on_key_pressed);
            connect_signal(key_ctrl.cast(), c"key-pressed", Some(key_cb), &state);
            ffi::gtk_widget_add_controller(window_widget, key_ctrl);
        }

        // Mouse click gesture controller (button 0 = listen to all mouse buttons).
        let click_gesture = ffi::gtk_gesture_click_new();
        if !click_gesture.is_null() {
            ffi::gtk_gesture_single_set_button(click_gesture.cast::<GtkGestureSingle>(), 0);
            let press_cb = transmute::<
                unsafe extern "C" fn(*mut GtkGesture, c_int, c_double, c_double, *mut c_void),
                unsafe extern "C" fn(),
            >(on_click_pressed);
            let release_cb = transmute::<
                unsafe extern "C" fn(*mut GtkGesture, c_int, c_double, c_double, *mut c_void),
                unsafe extern "C" fn(),
            >(on_click_released);
            connect_signal(click_gesture.cast(), c"pressed", Some(press_cb), &state);
            connect_signal(click_gesture.cast(), c"released", Some(release_cb), &state);
            ffi::gtk_widget_add_controller(
                drawing_area,
                click_gesture.cast::<GtkEventController>(),
            );
        }

        // Pointer motion controller.
        let motion_ctrl = ffi::gtk_event_controller_motion_new();
        if !motion_ctrl.is_null() {
            let motion_cb = transmute::<
                unsafe extern "C" fn(*mut GtkEventController, c_double, c_double, *mut c_void),
                unsafe extern "C" fn(),
            >(on_pointer_motion);
            connect_signal(motion_ctrl.cast(), c"motion", Some(motion_cb), &state);
            ffi::gtk_widget_add_controller(drawing_area, motion_ctrl);
        }

        // Scroll controller (both axes).
        let scroll_ctrl =
            ffi::gtk_event_controller_scroll_new(GTK_EVENT_CONTROLLER_SCROLL_BOTH_AXES);
        if !scroll_ctrl.is_null() {
            let scroll_cb = transmute::<
                unsafe extern "C" fn(
                    *mut GtkEventController,
                    c_double,
                    c_double,
                    *mut c_void,
                ) -> c_int,
                unsafe extern "C" fn(),
            >(on_pointer_scroll);
            connect_signal(scroll_ctrl.cast(), c"scroll", Some(scroll_cb), &state);
            ffi::gtk_widget_add_controller(drawing_area, scroll_ctrl);
        }

        // Focus controller.
        let focus_ctrl = ffi::gtk_event_controller_focus_new();
        if !focus_ctrl.is_null() {
            let enter_cb = transmute::<
                unsafe extern "C" fn(*mut GtkEventController, *mut c_void),
                unsafe extern "C" fn(),
            >(on_focus_enter);
            let leave_cb = transmute::<
                unsafe extern "C" fn(*mut GtkEventController, *mut c_void),
                unsafe extern "C" fn(),
            >(on_focus_leave);
            connect_signal(focus_ctrl.cast(), c"enter", Some(enter_cb), &state);
            connect_signal(focus_ctrl.cast(), c"leave", Some(leave_cb), &state);
            ffi::gtk_widget_add_controller(drawing_area, focus_ctrl);
        }

        ffi::gtk_window_set_child(window, drawing_area);
        ffi::gtk_window_present(window);
        ffi::gtk_widget_grab_focus(drawing_area);
    }

    // Register cross-thread frame commit waker and kick one initial sync in case a frame arrived.
    {
        let waker_state = Arc::clone(&state);
        frame_slot.set_ui_waker(Box::new(move || {
            schedule_ui_wakeup(&waker_state);
        }));
        schedule_ui_wakeup(&state);
    }

    // Register event-driven quit waker so `request_quit` wakes a blocked `g_main_context_iteration`
    // immediately with zero periodic timer polling when idle.
    {
        let quit_state = Arc::clone(&state);
        event_queue.set_quit_waker(Some(Arc::new(move || {
            schedule_quit_wakeup(&quit_state);
        })));
    }

    // SAFETY: `g_main_context_default` returns the process-global default GLib main context.
    let main_ctx = unsafe { ffi::g_main_context_default() };
    while event_queue.quit_code().is_none() {
        // SAFETY: Iterates the default GLib main context on the dedicated UI thread (`may_block = 1`).
        unsafe {
            ffi::g_main_context_iteration(main_ctx, 1);
        }
    }

    // Detach wakers, mark window dead before destruction, destroy window, and drain pending cleanups.
    event_queue.set_quit_waker(None);
    frame_slot.set_ui_waker(Box::new(|| {}));
    state.alive.store(false, Ordering::Release);

    if let Some(out_size) = final_window_size {
        // SAFETY: `window_widget` is still alive before `gtk_window_destroy(window)`.
        let (win_w, win_h) = unsafe {
            (
                ffi::gtk_widget_get_width(window_widget),
                ffi::gtk_widget_get_height(window_widget),
            )
        };
        if win_w >= 64
            && win_h >= 64
            && let Ok(mut guard) = out_size.lock()
        {
            *guard = Some((win_w, win_h));
        }
    }

    // SAFETY: `window` was kept alive throughout the loop (close-request returned `GDK_EVENT_STOP`)
    // and `state.alive` is now `false` so no idle callback will touch `window` or `drawing_area`.
    unsafe {
        ffi::gtk_window_destroy(window);
        for _ in 0..32 {
            if ffi::g_main_context_iteration(main_ctx, 0) == 0 {
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{GtkAppConfig, get_system_clipboard, set_system_clipboard};

    #[test]
    fn clipboard_mirror_roundtrips_headless() {
        set_system_clipboard("hello from lava clipboard λ");
        assert_eq!(get_system_clipboard(), "hello from lava clipboard λ");

        let cfg = GtkAppConfig::default();
        assert_eq!(cfg.initial_title, "Lava");
        assert_eq!(cfg.initial_width, 1280);
        assert_eq!(cfg.initial_height, 800);
    }
}
