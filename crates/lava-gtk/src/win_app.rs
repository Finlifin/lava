//! Dedicated Win32 native window, zero-flicker 32-bit `DIBSection` presenter, IME & UTF-16 input,
//! Win32 `CF_UNICODETEXT` clipboard sync, and message-pump driver for Windows 10 1709+.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use lava_proto::{EventQueue, FrameSlot, MouseAction, PointerStyle, UiEvent};

use crate::win_ffi::{
    self, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CF_UNICODETEXT, CFS_POINT, COMPOSITIONFORM,
    CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, DIB_RGB_COLORS, GMEM_MOVEABLE, GWL_STYLE,
    GWLP_USERDATA, HBITMAP, HCURSOR, HDC, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTLEFT,
    HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, HWND, IDC_ARROW, IDC_HAND, IDC_IBEAM, IDC_SIZENS,
    IDC_SIZEWE, LOGPIXELSX, LPARAM, LRESULT, MINMAXINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MSG, NCCALCSIZE_PARAMS, PAINTSTRUCT, POINT, RECT,
    RGBQUAD, SM_CXSCREEN, SM_CYSCREEN, SRCCOPY, SW_SHOW, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, UINT, UNICODE_NOCHAR, WM_CHAR, WM_CLOSE, WM_DESTROY,
    WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_IME_STARTCOMPOSITION, WM_KEYDOWN,
    WM_KILLFOCUS, WM_LAVA_QUIT, WM_LAVA_WAKE, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCCALCSIZE, WM_NCHITTEST, WM_PAINT, WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SETCURSOR, WM_SETFOCUS, WM_SIZE, WM_SYSCHAR, WM_SYSKEYDOWN, WM_TIMER, WM_UNICHAR,
    WNDCLASSEXW, WPARAM, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_APPWINDOW, WS_MAXIMIZEBOX,
    WS_MINIMIZEBOX, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_SYSMENU, WS_THICKFRAME, WS_VISIBLE,
    to_wide_nul,
};
use crate::win_keymap::{query_win32_modifiers, translate_win32_char, translate_win32_keydown};
use crate::win_render::{PixelCanvas, WinAnimController, ensure_bundled_fonts_registered};

const ANIM_TIMER_ID: usize = 0x4C41_5641;
const ANIM_FRAME_MS: u32 = 16; // ~60 Hz animation tick while unsettled
const BORDERLESS_RESIZE_BORDER_PX: i32 = 6;

const BORDERLESS_STYLE: u32 = WS_POPUP
    | WS_THICKFRAME
    | WS_MINIMIZEBOX
    | WS_MAXIMIZEBOX
    | WS_SYSMENU
    | WS_CLIPSIBLINGS
    | WS_CLIPCHILDREN;

const DECORATED_STYLE: u32 = WS_OVERLAPPEDWINDOW | WS_CLIPSIBLINGS | WS_CLIPCHILDREN;

static CLIPBOARD_MIRROR: OnceLock<Mutex<String>> = OnceLock::new();
static WINDOW_CLASS_REGISTERED: OnceLock<bool> = OnceLock::new();
static DPI_AWARENESS_SET: OnceLock<()> = OnceLock::new();

fn clipboard_mirror() -> &'static Mutex<String> {
    CLIPBOARD_MIRROR.get_or_init(|| Mutex::new(String::new()))
}

/// Configuration for bootstrapping the native host window.
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

fn enable_per_monitor_dpi_awareness() {
    DPI_AWARENESS_SET.get_or_init(|| {
        let u32_name = to_wide_nul("user32.dll");
        // SAFETY: Dynamically probe `SetProcessDpiAwarenessContext` (Win10 1703+) in `user32.dll`
        // with graceful fallback to `SetProcessDPIAware` (Vista+).
        unsafe {
            let h_user32 = win_ffi::GetModuleHandleW(u32_name.as_ptr());
            if !h_user32.is_null() {
                let proc = win_ffi::GetProcAddress(
                    h_user32,
                    c"SetProcessDpiAwarenessContext".as_ptr().cast(),
                );
                if !proc.is_null() {
                    type SetDpiCtxFn = unsafe extern "system" fn(isize) -> i32;
                    let f: SetDpiCtxFn = std::mem::transmute(proc);
                    // `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` = `-4`
                    if f(-4) != 0 {
                        return;
                    }
                }
                let fallback =
                    win_ffi::GetProcAddress(h_user32, c"SetProcessDPIAware".as_ptr().cast());
                if !fallback.is_null() {
                    type SetDpiAwareFn = unsafe extern "system" fn() -> i32;
                    let f: SetDpiAwareFn = std::mem::transmute(fallback);
                    let _ = f();
                }
            }
        }
    });
}

/// Checks whether an interactive Win32 desktop window can be created on the current station/display.
pub fn is_display_available() -> bool {
    if std::env::var("LAVA_HEADLESS").as_deref() == Ok("1") {
        return false;
    }
    // SAFETY: Probe creating a tiny unshown overlapped window to verify interactive desktop/X11 driver availability.
    unsafe {
        let h_instance = win_ffi::GetModuleHandleW(ptr::null());
        let class_name = ensure_window_class(h_instance);
        let probe_title = to_wide_nul("");
        let hwnd = win_ffi::CreateWindowExW(
            0,
            class_name.as_ptr(),
            probe_title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            64,
            64,
            ptr::null_mut(),
            ptr::null_mut(),
            h_instance,
            ptr::null_mut(),
        );
        if hwnd.is_null() {
            return false;
        }
        let _ = win_ffi::DestroyWindow(hwnd);
        true
    }
}

/// Writes `text` to both the thread-safe mirror and the Win32 `CF_UNICODETEXT` system clipboard.
pub fn set_system_clipboard(text: &str) {
    {
        let mut guard = clipboard_mirror()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        guard.clear();
        guard.push_str(text);
    }

    // Convert bare `\n` to `\r\n` for standard Windows clipboard interoperability.
    let mut win_text = String::with_capacity(text.len() + 16);
    let mut prev_cr = false;
    for ch in text.chars() {
        if ch == '\n' && !prev_cr {
            win_text.push_str("\r\n");
        } else {
            win_text.push(ch);
        }
        prev_cr = ch == '\r';
    }
    let wide = to_wide_nul(&win_text);
    let byte_len = wide.len() * std::mem::size_of::<u16>();

    // SAFETY: Standard Win32 `OpenClipboard(null)` + `GlobalAlloc(GMEM_MOVEABLE)` + `SetClipboardData`.
    // Once `SetClipboardData` succeeds, Windows takes ownership of `h_mem`.
    unsafe {
        if win_ffi::OpenClipboard(ptr::null_mut()) != 0 {
            let _ = win_ffi::EmptyClipboard();
            let h_mem = win_ffi::GlobalAlloc(GMEM_MOVEABLE, byte_len);
            if !h_mem.is_null() {
                let dst = win_ffi::GlobalLock(h_mem);
                if !dst.is_null() {
                    ptr::copy_nonoverlapping(wide.as_ptr(), dst.cast::<u16>(), wide.len());
                    let _ = win_ffi::GlobalUnlock(h_mem);
                    if win_ffi::SetClipboardData(CF_UNICODETEXT, h_mem).is_null() {
                        let _ = win_ffi::GlobalFree(h_mem);
                    }
                } else {
                    let _ = win_ffi::GlobalFree(h_mem);
                }
            }
            let _ = win_ffi::CloseClipboard();
        }
    }
}

/// Reads UTF-8 text from the Win32 `CF_UNICODETEXT` clipboard (falling back to the in-process mirror).
pub fn get_system_clipboard() -> String {
    // SAFETY: Standard Win32 `OpenClipboard(null)` + `GetClipboardData(CF_UNICODETEXT)` + `GlobalLock`.
    unsafe {
        if win_ffi::OpenClipboard(ptr::null_mut()) != 0 {
            let h_data = win_ffi::GetClipboardData(CF_UNICODETEXT);
            if !h_data.is_null() {
                let ptr_u16 = win_ffi::GlobalLock(h_data).cast::<u16>();
                if !ptr_u16.is_null() {
                    let mut len = 0usize;
                    while len < (64 * 1024 * 1024) && *ptr_u16.add(len) != 0 {
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(ptr_u16, len);
                    let decoded = String::from_utf16_lossy(slice).replace("\r\n", "\n");
                    let _ = win_ffi::GlobalUnlock(h_data);
                    let _ = win_ffi::CloseClipboard();
                    let mut guard = clipboard_mirror()
                        .lock()
                        .unwrap_or_else(|err| err.into_inner());
                    *guard = decoded.clone();
                    return decoded;
                }
            }
            let _ = win_ffi::CloseClipboard();
        }
    }

    clipboard_mirror()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

struct DibBackbuffer {
    mem_dc: HDC,
    dib: HBITMAP,
    old_bmp: *mut c_void,
    bits: *mut u32,
    width: i32,
    height: i32,
}

impl DibBackbuffer {
    fn new(width: i32, height: i32) -> Option<Self> {
        let w = width.max(1);
        let h = height.max(1);
        // SAFETY: Allocate a 32-bit top-down `BI_RGB` DIB section (`bi_height = -h`).
        unsafe {
            let mem_dc = win_ffi::CreateCompatibleDC(ptr::null_mut());
            if mem_dc.is_null() {
                return None;
            }
            let bmi = BITMAPINFO {
                bmi_header: BITMAPINFOHEADER {
                    bi_size: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    bi_width: w,
                    bi_height: -h, // top-down origin `(0, 0)` at top-left!
                    bi_planes: 1,
                    bi_bit_count: 32,
                    bi_compression: BI_RGB,
                    bi_size_image: (w as u32) * (h as u32) * 4,
                    bi_x_pels_per_meter: 0,
                    bi_y_pels_per_meter: 0,
                    bi_clr_used: 0,
                    bi_clr_important: 0,
                },
                bmi_colors: [RGBQUAD::default(); 1],
            };
            let mut raw_bits: *mut c_void = ptr::null_mut();
            let dib = win_ffi::CreateDIBSection(
                mem_dc,
                &bmi,
                DIB_RGB_COLORS,
                &mut raw_bits,
                ptr::null_mut(),
                0,
            );
            if dib.is_null() || raw_bits.is_null() {
                win_ffi::DeleteDC(mem_dc);
                return None;
            }
            let old_bmp = win_ffi::SelectObject(mem_dc, dib);
            Some(Self {
                mem_dc,
                dib,
                old_bmp,
                bits: raw_bits.cast::<u32>(),
                width: w,
                height: h,
            })
        }
    }
}

impl Drop for DibBackbuffer {
    fn drop(&mut self) {
        // SAFETY: Deselect `self.dib` before deleting the bitmap and memory DC.
        unsafe {
            if !self.mem_dc.is_null() {
                if !self.old_bmp.is_null() {
                    win_ffi::SelectObject(self.mem_dc, self.old_bmp);
                }
                if !self.dib.is_null() {
                    win_ffi::DeleteObject(self.dib);
                }
                win_ffi::DeleteDC(self.mem_dc);
            }
        }
    }
}

struct WinWindowState {
    frame_slot: Arc<FrameSlot>,
    event_queue: Arc<EventQueue>,
    hwnd_atomic: Arc<AtomicUsize>,
    wake_pending: Arc<AtomicBool>,
    canvas: RefCell<PixelCanvas>,
    backbuffer: RefCell<Option<DibBackbuffer>>,
    anim: RefCell<WinAnimController>,
    timer_active: Cell<bool>,
    last_tick: Cell<Instant>,
    dpi_scale: Cell<f64>,
    client_size: Cell<(i32, i32)>,
    pointer_pos: Cell<(f64, f64)>,
    applied_title: RefCell<String>,
    applied_pointer: Cell<PointerStyle>,
    applied_decorated: Cell<bool>,
    high_surrogate: Cell<u16>,
    caret_pos_px: Cell<(i32, i32)>,
}

impl WinWindowState {
    fn cursor_handle(style: PointerStyle) -> HCURSOR {
        let id = match style {
            PointerStyle::Default => IDC_ARROW,
            PointerStyle::Text => IDC_IBEAM,
            PointerStyle::Pointer => IDC_HAND,
            PointerStyle::ResizeHorizontal => IDC_SIZEWE,
            PointerStyle::ResizeVertical => IDC_SIZENS,
        };
        // SAFETY: `LoadCursorW(null, id)` returns a shared system cursor handle.
        unsafe { win_ffi::LoadCursorW(ptr::null_mut(), id) }
    }

    fn ensure_anim_timer(&self, hwnd: HWND, want_active: bool) {
        if want_active && !self.timer_active.get() {
            // SAFETY: Schedule `ANIM_TIMER_ID` on `hwnd` at 60 Hz.
            unsafe {
                win_ffi::SetTimer(hwnd, ANIM_TIMER_ID, ANIM_FRAME_MS, ptr::null_mut());
            }
            self.last_tick.set(Instant::now());
            self.timer_active.set(true);
        } else if !want_active && self.timer_active.get() {
            // SAFETY: Stop `ANIM_TIMER_ID` once animations have settled to 0% idle CPU.
            unsafe {
                win_ffi::KillTimer(hwnd, ANIM_TIMER_ID);
            }
            self.timer_active.set(false);
        }
    }

    fn sync_window_metadata(&self, hwnd: HWND) {
        let latest = self.frame_slot.latest();
        {
            let mut cur_title = self.applied_title.borrow_mut();
            if *cur_title != latest.window.title {
                *cur_title = latest.window.title.clone();
                let wide = to_wide_nul(&cur_title);
                // SAFETY: `hwnd` is live and `wide` is NUL-terminated UTF-16.
                unsafe {
                    win_ffi::SetWindowTextW(hwnd, wide.as_ptr());
                }
            }
        }
        if self.applied_pointer.get() != latest.window.pointer {
            self.applied_pointer.set(latest.window.pointer);
            // SAFETY: Apply updated cursor shape immediately.
            unsafe {
                win_ffi::SetCursor(Self::cursor_handle(latest.window.pointer));
            }
        }
        if self.applied_decorated.get() != latest.window.decorated {
            self.applied_decorated.set(latest.window.decorated);
            let new_style = if latest.window.decorated {
                DECORATED_STYLE | WS_VISIBLE
            } else {
                BORDERLESS_STYLE | WS_VISIBLE
            };
            // SAFETY: Dynamically update `GWL_STYLE` and trigger `SWP_FRAMECHANGED`; if already
            // maximized (`IsZoomed`) and switching to borderless, snap outer rect to `rc_work` too.
            unsafe {
                win_ffi::SetWindowLongPtrW(hwnd, GWL_STYLE, new_style as isize);
                if !latest.window.decorated
                    && win_ffi::IsZoomed(hwnd) != 0
                    && let Some(mi) = query_monitor_info(hwnd, MONITOR_DEFAULTTONEAREST)
                {
                    let ww = (mi.rc_work.right - mi.rc_work.left).max(64);
                    let wh = (mi.rc_work.bottom - mi.rc_work.top).max(64);
                    win_ffi::SetWindowPos(
                        hwnd,
                        ptr::null_mut(),
                        mi.rc_work.left,
                        mi.rc_work.top,
                        ww,
                        wh,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                    );
                } else {
                    win_ffi::SetWindowPos(
                        hwnd,
                        ptr::null_mut(),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                    );
                }
            }
        }
    }

    fn paint_frame(&self, hwnd: HWND, target_hdc: HDC) {
        let (phys_w, phys_h) = self.client_size.get();
        let safe_phys_w = phys_w.max(1);
        let safe_phys_h = phys_h.max(1);
        let scale = self.dpi_scale.get().max(1.0);
        let logical_w = ((f64::from(safe_phys_w) / scale).round() as i32).max(1);
        let logical_h = ((f64::from(safe_phys_h) / scale).round() as i32).max(1);

        let frame = self.frame_slot.latest();
        let hints = self.frame_slot.anim_hints();
        let overlay = self.frame_slot.transfer_overlay();

        // Record primary caret position for IME candidate window placement (`WM_IME_STARTCOMPOSITION`).
        for cmd in frame.commands.iter().rev() {
            if let lava_proto::DrawCmd::CursorCaret { x, y, .. } = cmd {
                self.caret_pos_px
                    .set(((*x * scale).round() as i32, (*y * scale).round() as i32));
                break;
            }
        }

        let still_animating = {
            let mut canvas = self.canvas.borrow_mut();
            if canvas.width != safe_phys_w
                || canvas.height != safe_phys_h
                || (canvas.scale - scale).abs() > 1e-4
            {
                *canvas = PixelCanvas::new_scaled(logical_w, logical_h, scale);
                canvas.width = safe_phys_w;
                canvas.height = safe_phys_h;
                canvas
                    .pixels
                    .resize((safe_phys_w as usize) * (safe_phys_h as usize), 0xFF1E_1E2E);
            }

            let mut anim = self.anim.borrow_mut();
            anim.render_composited(
                &mut canvas,
                logical_w,
                logical_h,
                &frame,
                &hints,
                overlay.as_ref(),
            );

            let mut bb_slot = self.backbuffer.borrow_mut();
            let needs_realloc = bb_slot
                .as_ref()
                .is_none_or(|b| b.width != safe_phys_w || b.height != safe_phys_h);
            if needs_realloc {
                *bb_slot = DibBackbuffer::new(safe_phys_w, safe_phys_h);
            }
            if let Some(bb) = bb_slot.as_ref() {
                let count = (safe_phys_w as usize) * (safe_phys_h as usize);
                // SAFETY: `bb.bits` points to `count` u32 pixels in the top-down DIBSection;
                // `GdiFlush` serializes any prior GDI reads before writing and `BitBlt` copies to `target_hdc`.
                unsafe {
                    win_ffi::GdiFlush();
                    ptr::copy_nonoverlapping(canvas.pixels.as_ptr(), bb.bits, count);
                    win_ffi::BitBlt(
                        target_hdc,
                        0,
                        0,
                        safe_phys_w,
                        safe_phys_h,
                        bb.mem_dc,
                        0,
                        0,
                        SRCCOPY,
                    );
                }
            }
            anim.is_animating() || overlay.as_ref().is_some_and(|o| o.active)
        };

        self.ensure_anim_timer(hwnd, still_animating);
    }
}

fn query_monitor_info(hwnd: HWND, flags: u32) -> Option<MONITORINFO> {
    // SAFETY: `MonitorFromWindow` + `GetMonitorInfoW` with initialized `cb_size`.
    unsafe {
        let hmon = win_ffi::MonitorFromWindow(hwnd, flags);
        if hmon.is_null() {
            return None;
        }
        let mut mi = MONITORINFO {
            cb_size: std::mem::size_of::<MONITORINFO>() as u32,
            ..MONITORINFO::default()
        };
        if win_ffi::GetMonitorInfoW(hmon, &mut mi) != 0 {
            Some(mi)
        } else {
            None
        }
    }
}

#[inline]
fn loword_i16(lparam: LPARAM) -> i32 {
    (lparam as u32 & 0xFFFF) as i16 as i32
}

#[inline]
fn hiword_i16(lparam: LPARAM) -> i32 {
    ((lparam as u32 >> 16) & 0xFFFF) as i16 as i32
}

#[inline]
fn wheel_delta_wparam(wparam: WPARAM) -> i16 {
    ((wparam >> 16) & 0xFFFF) as i16
}

unsafe extern "system" fn lava_wnd_proc(
    hwnd: HWND,
    msg: UINT,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    // SAFETY: Retrieve per-window `*const WinWindowState` stored via `GWLP_USERDATA`.
    let state_ptr =
        unsafe { win_ffi::GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WinWindowState };
    if state_ptr.is_null() {
        // SAFETY: Forward early messages before `SetWindowLongPtrW` to `DefWindowProcW`.
        return unsafe { win_ffi::DefWindowProcW(hwnd, msg, w_param, l_param) };
    }
    // SAFETY: `state_ptr` remains valid on the Win32 UI thread until `DestroyWindow` finishes in `run_gtk_app_with_final_size`.
    let state = unsafe { &*state_ptr };

    match msg {
        WM_ERASEBKGND => 1,
        WM_GETMINMAXINFO => {
            if l_param != 0 {
                // SAFETY: `l_param` points to `MINMAXINFO` during `WM_GETMINMAXINFO`.
                let mmi = unsafe { &mut *(l_param as *mut MINMAXINFO) };
                mmi.pt_min_track_size = POINT { x: 240, y: 140 };
                // When borderless (`WS_POPUP | WS_THICKFRAME`), Windows inflates the maximized
                // outer rect by `SM_CXSIZEFRAME + SM_CXPADDEDBORDER` (~8px per edge) past the
                // monitor edges and ignores the taskbar for `WS_POPUP`. Constrain the primary
                // monitor max position/size to `rc_work` (Win32 automatically translates these
                // primary-monitor metrics when maximizing onto a secondary monitor).
                if !state.applied_decorated.get()
                    && let Some(mi) = query_monitor_info(ptr::null_mut(), MONITOR_DEFAULTTOPRIMARY)
                {
                    let work_w = mi.rc_work.right - mi.rc_work.left;
                    let work_h = mi.rc_work.bottom - mi.rc_work.top;
                    if work_w > 0 && work_h > 0 {
                        mmi.pt_max_position = POINT {
                            x: mi.rc_work.left - mi.rc_monitor.left,
                            y: mi.rc_work.top - mi.rc_monitor.top,
                        };
                        mmi.pt_max_size = POINT {
                            x: work_w,
                            y: work_h,
                        };
                    }
                }
            }
            0
        }
        WM_NCCALCSIZE => {
            if !state.applied_decorated.get() && w_param != 0 {
                // When maximized (`IsZoomed`), clamp the proposed client `rgrc[0]` strictly inside
                // the current monitor's `rc_work` so `WS_THICKFRAME`'s ~8px invisible border
                // overhang never pushes content off any of the 4 screen edges.
                // SAFETY: When `w_param != 0`, `l_param` points to `NCCALCSIZE_PARAMS`.
                unsafe {
                    if l_param != 0
                        && win_ffi::IsZoomed(hwnd) != 0
                        && let Some(mi) = query_monitor_info(hwnd, MONITOR_DEFAULTTONEAREST)
                    {
                        let params = &mut *(l_param as *mut NCCALCSIZE_PARAMS);
                        params.rgrc[0].left = params.rgrc[0].left.max(mi.rc_work.left);
                        params.rgrc[0].top = params.rgrc[0].top.max(mi.rc_work.top);
                        params.rgrc[0].right = params.rgrc[0].right.min(mi.rc_work.right);
                        params.rgrc[0].bottom = params.rgrc[0].bottom.min(mi.rc_work.bottom);
                    }
                }
                return 0;
            }
            // SAFETY: Default frame calculation when decorations are enabled.
            unsafe { win_ffi::DefWindowProcW(hwnd, msg, w_param, l_param) }
        }
        WM_NCHITTEST => {
            if !state.applied_decorated.get() {
                // SAFETY: Maximized windows should not expose 6px resize hit borders at screen edges.
                if unsafe { win_ffi::IsZoomed(hwnd) } != 0 {
                    return HTCLIENT;
                }
                let sx = loword_i16(l_param);
                let sy = hiword_i16(l_param);
                let mut wr = RECT::default();
                // SAFETY: `wr` is a stack `RECT`.
                if unsafe { win_ffi::GetWindowRect(hwnd, &mut wr) } != 0 {
                    let b = BORDERLESS_RESIZE_BORDER_PX;
                    let on_left = sx >= wr.left && sx < wr.left + b;
                    let on_right = sx < wr.right && sx >= wr.right - b;
                    let on_top = sy >= wr.top && sy < wr.top + b;
                    let on_bottom = sy < wr.bottom && sy >= wr.bottom - b;
                    if on_top && on_left {
                        return HTTOPLEFT;
                    }
                    if on_top && on_right {
                        return HTTOPRIGHT;
                    }
                    if on_bottom && on_left {
                        return HTBOTTOMLEFT;
                    }
                    if on_bottom && on_right {
                        return HTBOTTOMRIGHT;
                    }
                    if on_left {
                        return HTLEFT;
                    }
                    if on_right {
                        return HTRIGHT;
                    }
                    if on_top {
                        return HTTOP;
                    }
                    if on_bottom {
                        return HTBOTTOM;
                    }
                }
                return HTCLIENT;
            }
            // SAFETY: Default hit-test when decorated.
            unsafe { win_ffi::DefWindowProcW(hwnd, msg, w_param, l_param) }
        }
        WM_SETCURSOR => {
            if (l_param & 0xFFFF) == HTCLIENT {
                // SAFETY: Set cursor corresponding to latest `PointerStyle`.
                unsafe {
                    win_ffi::SetCursor(WinWindowState::cursor_handle(state.applied_pointer.get()));
                }
                return 1;
            }
            // SAFETY: Delegate non-client cursor shapes (resize arrows) to `DefWindowProcW`.
            unsafe { win_ffi::DefWindowProcW(hwnd, msg, w_param, l_param) }
        }
        WM_LAVA_WAKE => {
            state.wake_pending.store(false, Ordering::Release);
            if state.event_queue.quit_code().is_some() {
                // SAFETY: Exit message loop when VM requested quit.
                unsafe { win_ffi::PostQuitMessage(0) };
                return 0;
            }
            state.sync_window_metadata(hwnd);
            // SAFETY: Schedule client-area repaint.
            unsafe {
                win_ffi::InvalidateRect(hwnd, ptr::null(), 0);
            }
            0
        }
        WM_LAVA_QUIT => {
            // SAFETY: Exit message loop.
            unsafe { win_ffi::PostQuitMessage(0) };
            0
        }
        WM_TIMER => {
            if w_param == ANIM_TIMER_ID {
                let now = Instant::now();
                let dt = now.duration_since(state.last_tick.get()).as_secs_f64();
                state.last_tick.set(now);
                let keep_going = state.anim.borrow_mut().step(dt);
                // SAFETY: Queue repaint for the stepped animation frame.
                unsafe {
                    win_ffi::InvalidateRect(hwnd, ptr::null(), 0);
                }
                if !keep_going {
                    state.ensure_anim_timer(hwnd, false);
                }
            }
            0
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: Balanced `BeginPaint` / `EndPaint` pair on `hwnd`.
            unsafe {
                let hdc = win_ffi::BeginPaint(hwnd, &mut ps);
                if !hdc.is_null() {
                    state.paint_frame(hwnd, hdc);
                }
                win_ffi::EndPaint(hwnd, &ps);
            }
            0
        }
        WM_SIZE => {
            let phys_w = loword_i16(l_param).max(1);
            let phys_h = hiword_i16(l_param).max(1);
            if state.client_size.get() != (phys_w, phys_h) {
                state.client_size.set((phys_w, phys_h));
                let scale = state.dpi_scale.get().max(1.0);
                let logical_w = ((f64::from(phys_w) / scale).round() as i32).max(1);
                let logical_h = ((f64::from(phys_h) / scale).round() as i32).max(1);
                state.event_queue.push(UiEvent::Resize {
                    width: logical_w,
                    height: logical_h,
                    scale_factor: scale.round().max(1.0) as i32,
                });
                // SAFETY: Queue repaint at new size.
                unsafe {
                    win_ffi::InvalidateRect(hwnd, ptr::null(), 0);
                }
            }
            0
        }
        WM_DPICHANGED => {
            let new_dpi = (w_param & 0xFFFF) as f64;
            if new_dpi >= 72.0 {
                state.dpi_scale.set((new_dpi / 96.0).max(1.0));
            }
            if l_param != 0 {
                // SAFETY: `l_param` points to the suggested new window `RECT`.
                unsafe {
                    let r = &*(l_param as *const RECT);
                    win_ffi::SetWindowPos(
                        hwnd,
                        ptr::null_mut(),
                        r.left,
                        r.top,
                        (r.right - r.left).max(64),
                        (r.bottom - r.top).max(64),
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            0
        }
        WM_SETFOCUS => {
            state.event_queue.push(UiEvent::Focus { focused: true });
            0
        }
        WM_KILLFOCUS => {
            state.high_surrogate.set(0);
            state.event_queue.push(UiEvent::Focus { focused: false });
            0
        }
        WM_IME_STARTCOMPOSITION => {
            // Position Windows IME candidate window right below Lava's active cursor!
            let (cx, cy) = state.caret_pos_px.get();
            // SAFETY: `ImmGetContext` / `ImmSetCompositionWindow` / `ImmReleaseContext` on `hwnd`.
            unsafe {
                let himc = win_ffi::ImmGetContext(hwnd);
                if !himc.is_null() {
                    let cf = COMPOSITIONFORM {
                        dw_style: CFS_POINT,
                        pt_current_pos: POINT { x: cx, y: cy + 4 },
                        rc_area: RECT::default(),
                    };
                    let _ = win_ffi::ImmSetCompositionWindow(himc, &cf);
                    let _ = win_ffi::ImmReleaseContext(hwnd, himc);
                }
            }
            // SAFETY: Allow default IME UI processing to proceed.
            unsafe { win_ffi::DefWindowProcW(hwnd, msg, w_param, l_param) }
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = (w_param & 0xFFFF) as u32;
            let mods = query_win32_modifiers();
            if let Some(ev) = translate_win32_keydown(vk, mods) {
                state.event_queue.push(ev);
                return 0;
            }
            if msg == WM_SYSKEYDOWN {
                return 0;
            }
            // SAFETY: Default handling for unmodified keys so `WM_CHAR` is delivered.
            unsafe { win_ffi::DefWindowProcW(hwnd, msg, w_param, l_param) }
        }
        WM_SYSCHAR => 0,
        WM_CHAR => {
            let unit = (w_param & 0xFFFF) as u16;
            if (0xD800..=0xDBFF).contains(&unit) {
                state.high_surrogate.set(unit);
                return 0;
            }
            let ch_opt = if (0xDC00..=0xDFFF).contains(&unit) {
                let hi = state.high_surrogate.replace(0);
                if (0xD800..=0xDBFF).contains(&hi) {
                    let cp =
                        0x10000 + (((u32::from(hi) - 0xD800) << 10) | (u32::from(unit) - 0xDC00));
                    char::from_u32(cp)
                } else {
                    None
                }
            } else {
                state.high_surrogate.set(0);
                char::from_u32(u32::from(unit))
            };
            if let Some(ch) = ch_opt {
                let mods = query_win32_modifiers();
                if let Some(ev) = translate_win32_char(ch, mods) {
                    state.event_queue.push(ev);
                }
            }
            0
        }
        WM_UNICHAR => {
            if w_param == UNICODE_NOCHAR {
                return 1;
            }
            if let Some(ch) = char::from_u32(w_param as u32) {
                let mods = query_win32_modifiers();
                if let Some(ev) = translate_win32_char(ch, mods) {
                    state.event_queue.push(ev);
                }
            }
            0
        }
        WM_MOUSEMOVE => {
            let scale = state.dpi_scale.get().max(1.0);
            let lx = f64::from(loword_i16(l_param)) / scale;
            let ly = f64::from(hiword_i16(l_param)) / scale;
            state.pointer_pos.set((lx, ly));
            state.event_queue.push(UiEvent::MouseMove {
                x: lx,
                y: ly,
                modifiers: query_win32_modifiers(),
            });
            0
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK | WM_MBUTTONDOWN
        | WM_MBUTTONDBLCLK => {
            // SAFETY: Capture mouse while button is held so drag-selection tracks outside client rect.
            unsafe {
                win_ffi::SetCapture(hwnd);
            }
            let scale = state.dpi_scale.get().max(1.0);
            let lx = f64::from(loword_i16(l_param)) / scale;
            let ly = f64::from(hiword_i16(l_param)) / scale;
            state.pointer_pos.set((lx, ly));
            let (button, press_count) = match msg {
                WM_LBUTTONDOWN => (1, 1),
                WM_LBUTTONDBLCLK => (1, 2),
                WM_MBUTTONDOWN => (2, 1),
                WM_MBUTTONDBLCLK => (2, 2),
                WM_RBUTTONDOWN => (3, 1),
                WM_RBUTTONDBLCLK => (3, 2),
                _ => (1, 1),
            };
            state.event_queue.push(UiEvent::MouseButton {
                action: MouseAction::Press,
                button,
                x: lx,
                y: ly,
                modifiers: query_win32_modifiers(),
                press_count,
            });
            0
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            // SAFETY: Release mouse capture on button release.
            unsafe {
                win_ffi::ReleaseCapture();
            }
            let scale = state.dpi_scale.get().max(1.0);
            let lx = f64::from(loword_i16(l_param)) / scale;
            let ly = f64::from(hiword_i16(l_param)) / scale;
            let button = match msg {
                WM_LBUTTONUP => 1,
                WM_MBUTTONUP => 2,
                WM_RBUTTONUP => 3,
                _ => 1,
            };
            state.event_queue.push(UiEvent::MouseButton {
                action: MouseAction::Release,
                button,
                x: lx,
                y: ly,
                modifiers: query_win32_modifiers(),
                press_count: 1,
            });
            0
        }
        WM_MOUSEWHEEL => {
            let delta = f64::from(wheel_delta_wparam(w_param)) / 120.0;
            if delta.abs() > 1e-4 {
                let (px, py) = state.pointer_pos.get();
                state.event_queue.push(UiEvent::MouseScroll {
                    dx: 0.0,
                    dy: -delta * 3.0,
                    x: px,
                    y: py,
                    modifiers: query_win32_modifiers(),
                });
            }
            0
        }
        WM_MOUSEHWHEEL => {
            let delta = f64::from(wheel_delta_wparam(w_param)) / 120.0;
            if delta.abs() > 1e-4 {
                let (px, py) = state.pointer_pos.get();
                state.event_queue.push(UiEvent::MouseScroll {
                    dx: delta * 3.0,
                    dy: 0.0,
                    x: px,
                    y: py,
                    modifiers: query_win32_modifiers(),
                });
            }
            0
        }
        WM_CLOSE => {
            state.event_queue.push(UiEvent::CloseRequested);
            // SAFETY: Destroy window and exit UI loop cleanly when user closes window.
            unsafe {
                win_ffi::DestroyWindow(hwnd);
            }
            0
        }
        WM_DESTROY => {
            state.ensure_anim_timer(hwnd, false);
            state.hwnd_atomic.store(0, Ordering::Release);
            // SAFETY: Post `WM_QUIT` to terminate `GetMessageW` loop.
            unsafe {
                win_ffi::PostQuitMessage(0);
            }
            0
        }
        _ => {
            // SAFETY: Standard Win32 default window procedure.
            unsafe { win_ffi::DefWindowProcW(hwnd, msg, w_param, l_param) }
        }
    }
}

fn ensure_window_class(h_instance: *mut c_void) -> Vec<u16> {
    let class_name = to_wide_nul("LavaWin32HostWindow");
    WINDOW_CLASS_REGISTERED.get_or_init(|| {
        let wc = WNDCLASSEXW {
            cb_size: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfn_wnd_proc: Some(lava_wnd_proc),
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance,
            h_icon: ptr::null_mut(),
            h_cursor: WinWindowState::cursor_handle(PointerStyle::Default),
            hbr_background: ptr::null_mut(),
            lpsz_menu_name: ptr::null(),
            lpsz_class_name: class_name.as_ptr(),
            h_icon_sm: ptr::null_mut(),
        };
        // SAFETY: `wc` is a valid `WNDCLASSEXW` pointing to static `lava_wnd_proc`.
        unsafe { win_ffi::RegisterClassExW(&wc) != 0 }
    });
    class_name
}

/// Thread-safe slot capturing final `(width, height)` on clean Win32 window shutdown.
pub type FinalWindowSizeSlot = Arc<Mutex<Option<(i32, i32)>>>;

/// Runs the native Win32 window and message loop on the calling thread until quit or close.
pub fn run_gtk_app(
    config: GtkAppConfig,
    frame_slot: Arc<FrameSlot>,
    event_queue: Arc<EventQueue>,
) -> Result<(), String> {
    run_gtk_app_with_final_size(config, frame_slot, event_queue, None)
}

/// Runs the native Win32 window and stores its final logical `(width, height)` into `final_window_size`.
pub fn run_gtk_app_with_final_size(
    config: GtkAppConfig,
    frame_slot: Arc<FrameSlot>,
    event_queue: Arc<EventQueue>,
    final_window_size: Option<FinalWindowSizeSlot>,
) -> Result<(), String> {
    if !is_display_available() {
        return Err("Win32 failed to initialize desktop display".to_string());
    }
    enable_per_monitor_dpi_awareness();
    ensure_bundled_fonts_registered();

    let init_w = config.initial_width.max(64);
    let init_h = config.initial_height.max(64);

    // Query initial system DPI scale (`96` = `1.0`).
    let init_scale = unsafe {
        let dc = win_ffi::GetDC(ptr::null_mut());
        let dpi = if dc.is_null() {
            96
        } else {
            let d = win_ffi::GetDeviceCaps(dc, LOGPIXELSX);
            win_ffi::ReleaseDC(ptr::null_mut(), dc);
            if d >= 72 { d } else { 96 }
        };
        (f64::from(dpi) / 96.0).max(1.0)
    };

    let init_frame = frame_slot.latest();
    let init_decorated = init_frame.window.decorated;
    let init_title = if init_frame.generation > 0 && !init_frame.window.title.is_empty() {
        init_frame.window.title.clone()
    } else {
        config.initial_title.clone()
    };

    let phys_w = ((f64::from(init_w) * init_scale).round() as i32).max(64);
    let phys_h = ((f64::from(init_h) * init_scale).round() as i32).max(64);

    // SAFETY: `GetModuleHandleW(null)` returns the current executable module handle.
    let h_instance = unsafe { win_ffi::GetModuleHandleW(ptr::null()) };
    let class_name = ensure_window_class(h_instance);
    let title_wide = to_wide_nul(&init_title);

    let win_style = if init_decorated {
        DECORATED_STYLE
    } else {
        BORDERLESS_STYLE
    };
    let mut adj_rect = RECT {
        left: 0,
        top: 0,
        right: phys_w,
        bottom: phys_h,
    };
    if init_decorated {
        // SAFETY: Compute outer window rect for exact client area `(phys_w, phys_h)`.
        unsafe {
            let _ = win_ffi::AdjustWindowRectEx(&mut adj_rect, win_style, 0, WS_EX_APPWINDOW);
        }
    }
    let outer_w = (adj_rect.right - adj_rect.left).max(phys_w);
    let outer_h = (adj_rect.bottom - adj_rect.top).max(phys_h);
    // Note: Win32 `CW_USEDEFAULT` is only substituted for `WS_OVERLAPPED` windows! For borderless
    // `WS_POPUP` windows (`!init_decorated`), center explicitly on the primary screen.
    let (pos_x, pos_y) = if init_decorated {
        (CW_USEDEFAULT, CW_USEDEFAULT)
    } else {
        // SAFETY: `GetSystemMetrics` queries primary monitor pixel dimensions.
        let (scr_w, scr_h) = unsafe {
            (
                win_ffi::GetSystemMetrics(SM_CXSCREEN),
                win_ffi::GetSystemMetrics(SM_CYSCREEN),
            )
        };
        let x = if scr_w > outer_w {
            (scr_w - outer_w) / 2
        } else {
            40
        };
        let y = if scr_h > outer_h {
            (scr_h - outer_h) / 2
        } else {
            40
        };
        (x.max(0), y.max(0))
    };

    let hwnd_atomic = Arc::new(AtomicUsize::new(0));
    let wake_pending = Arc::new(AtomicBool::new(false));

    let state = Box::new(WinWindowState {
        frame_slot: Arc::clone(&frame_slot),
        event_queue: Arc::clone(&event_queue),
        hwnd_atomic: Arc::clone(&hwnd_atomic),
        wake_pending: Arc::clone(&wake_pending),
        canvas: RefCell::new(PixelCanvas::new_scaled(init_w, init_h, init_scale)),
        backbuffer: RefCell::new(None),
        anim: RefCell::new(WinAnimController::new()),
        timer_active: Cell::new(false),
        last_tick: Cell::new(Instant::now()),
        dpi_scale: Cell::new(init_scale),
        client_size: Cell::new((phys_w, phys_h)),
        pointer_pos: Cell::new((0.0, 0.0)),
        applied_title: RefCell::new(init_title),
        applied_pointer: Cell::new(init_frame.window.pointer),
        applied_decorated: Cell::new(init_decorated),
        high_surrogate: Cell::new(0),
        caret_pos_px: Cell::new((16, 16)),
    });

    // SAFETY: Create native Win32 window and attach `&*state` via `GWLP_USERDATA` before showing.
    let hwnd = unsafe {
        win_ffi::CreateWindowExW(
            WS_EX_APPWINDOW,
            class_name.as_ptr(),
            title_wide.as_ptr(),
            win_style,
            pos_x,
            pos_y,
            outer_w,
            outer_h,
            ptr::null_mut(),
            ptr::null_mut(),
            h_instance,
            ptr::null_mut(),
        )
    };
    if hwnd.is_null() {
        if event_queue.quit_code().is_none() {
            event_queue.request_quit(1);
        }
        return Err("CreateWindowExW failed to create Win32 host window".to_string());
    }

    hwnd_atomic.store(hwnd as usize, Ordering::Release);
    // SAFETY: Bind `GWLP_USERDATA` to `state` for the lifetime of the message loop below.
    unsafe {
        win_ffi::SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            (&*state as *const WinWindowState) as isize,
        );
    }

    // Wire cross-thread `FrameSlot` redraw waker and `EventQueue` quit waker via `PostMessageW`.
    {
        let h_ref = Arc::clone(&hwnd_atomic);
        let wp_ref = Arc::clone(&wake_pending);
        frame_slot.set_ui_waker(Box::new(move || {
            let raw_h = h_ref.load(Ordering::Acquire);
            if raw_h != 0 && !wp_ref.swap(true, Ordering::AcqRel) {
                // SAFETY: `PostMessageW` is thread-safe in Win32 across any thread.
                unsafe {
                    let _ = win_ffi::PostMessageW(raw_h as HWND, WM_LAVA_WAKE, 0, 0);
                }
            }
        }));
    }
    {
        let h_ref = Arc::clone(&hwnd_atomic);
        event_queue.set_quit_waker(Some(Arc::new(move || {
            let raw_h = h_ref.load(Ordering::Acquire);
            if raw_h != 0 {
                // SAFETY: `PostMessageW` safely wakes `GetMessageW` on the UI thread to exit.
                unsafe {
                    let _ = win_ffi::PostMessageW(raw_h as HWND, WM_LAVA_QUIT, 0, 0);
                }
            }
        })));
    }

    // Deliver initial logical dimensions before showing window.
    event_queue.push(UiEvent::Resize {
        width: init_w,
        height: init_h,
        scale_factor: init_scale.round().max(1.0) as i32,
    });

    if event_queue.quit_code().is_none() {
        // SAFETY: Show window and enter standard Win32 `GetMessageW` loop.
        unsafe {
            win_ffi::ShowWindow(hwnd, SW_SHOW);
            win_ffi::UpdateWindow(hwnd);

            let mut msg = MSG::default();
            while win_ffi::GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
                let _ = win_ffi::TranslateMessage(&msg);
                let _ = win_ffi::DispatchMessageW(&msg);
                if event_queue.quit_code().is_some() {
                    break;
                }
            }
        }
    }

    hwnd_atomic.store(0, Ordering::Release);
    event_queue.set_quit_waker(None);
    state.ensure_anim_timer(hwnd, false);
    // SAFETY: Detach `GWLP_USERDATA` and destroy `hwnd` if still open before dropping `state`.
    unsafe {
        win_ffi::SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        win_ffi::DestroyWindow(hwnd);
    }

    if event_queue.quit_code().is_none() {
        event_queue.request_quit(0);
    }
    let (final_phys_w, final_phys_h) = state.client_size.get();
    let final_scale = state.dpi_scale.get().max(1.0);
    let final_logical = (
        ((f64::from(final_phys_w) / final_scale).round() as i32).max(64),
        ((f64::from(final_phys_h) / final_scale).round() as i32).max(64),
    );
    if let Some(out_slot) = final_window_size
        && let Ok(mut guard) = out_slot.lock()
    {
        *guard = Some(final_logical);
    }
    drop(state);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_clipboard_roundtrips_cleanly() {
        let sample = "Lava Windows 10 1709 剪贴板\nsecond line";
        set_system_clipboard(sample);
        assert_eq!(get_system_clipboard(), sample);
    }

    #[test]
    fn live_win32_window_renders_dib_frame_and_exits_cleanly() {
        if !is_display_available() {
            return;
        }
        let frame_slot = Arc::new(FrameSlot::new());
        let event_queue = Arc::new(EventQueue::new());

        let fs_bg = Arc::clone(&frame_slot);
        let eq_bg = Arc::clone(&event_queue);
        let driver = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(40));
            let mut frame = lava_proto::RenderFrame::default();
            frame.window.title = "Lava Win32 Live Test".to_string();
            frame.window.width = 400;
            frame.window.height = 240;
            frame.window.decorated = false;
            frame.commands.push(lava_proto::DrawCmd::TextRun {
                x: 16.0,
                y: 16.0,
                text: " Lava Win10 1709 Live Window".to_string(),
                font_family: "Fira Code".to_string(),
                font_size_pt: 13.0,
                color: lava_proto::ColorRgba::from_rgba(166, 227, 161, 255),
                bold: true,
                italic: false,
                underline: false,
            });
            let _ = fs_bg.commit(frame);
            std::thread::sleep(std::time::Duration::from_millis(60));
            eq_bg.request_quit(0);
        });

        let final_size_slot: FinalWindowSizeSlot = Arc::new(Mutex::new(None));
        run_gtk_app_with_final_size(
            GtkAppConfig {
                initial_title: "Lava Boot".to_string(),
                initial_width: 400,
                initial_height: 240,
            },
            frame_slot,
            Arc::clone(&event_queue),
            Some(Arc::clone(&final_size_slot)),
        )
        .expect("run_gtk_app_with_final_size");
        driver.join().unwrap();
        let (fw, fh) = final_size_slot
            .lock()
            .unwrap()
            .expect("captured final size");
        assert!(fw >= 64 && fh >= 64);
        assert_eq!(event_queue.quit_code(), Some(0));
    }

    #[test]
    fn borderless_maximize_stays_strictly_within_monitor_work_area() {
        if !is_display_available() {
            return;
        }
        let frame_slot = Arc::new(FrameSlot::new());
        let event_queue = Arc::new(EventQueue::new());
        let mut init_frame = lava_proto::RenderFrame::default();
        init_frame.window.title = "Lava Borderless Maximize Test".to_string();
        init_frame.window.width = 640;
        init_frame.window.height = 420;
        init_frame.window.decorated = false;
        let _ = frame_slot.commit(init_frame);

        let eq_bg = Arc::clone(&event_queue);
        let bounds_slot: Arc<Mutex<Option<(RECT, RECT)>>> = Arc::new(Mutex::new(None));
        let bounds_out = Arc::clone(&bounds_slot);

        // Poll `hwnd` once created, maximize via `PostMessage` / `ShowWindow(SW_MAXIMIZE)`, and
        // inspect screen-space client rect vs `MONITORINFO.rc_work`.
        let h_instance = unsafe { win_ffi::GetModuleHandleW(ptr::null()) };
        let class_name = ensure_window_class(h_instance);
        let title_wide = to_wide_nul("Lava Borderless Direct Check");
        let state = Box::new(WinWindowState {
            frame_slot: Arc::clone(&frame_slot),
            event_queue: Arc::clone(&event_queue),
            hwnd_atomic: Arc::new(AtomicUsize::new(0)),
            wake_pending: Arc::new(AtomicBool::new(false)),
            canvas: RefCell::new(PixelCanvas::new_scaled(640, 420, 1.0)),
            backbuffer: RefCell::new(None),
            anim: RefCell::new(WinAnimController::new()),
            timer_active: Cell::new(false),
            last_tick: Cell::new(Instant::now()),
            dpi_scale: Cell::new(1.0),
            client_size: Cell::new((640, 420)),
            pointer_pos: Cell::new((0.0, 0.0)),
            applied_title: RefCell::new("Lava Borderless Direct Check".to_string()),
            applied_pointer: Cell::new(PointerStyle::Default),
            applied_decorated: Cell::new(false),
            high_surrogate: Cell::new(0),
            caret_pos_px: Cell::new((16, 16)),
        });

        unsafe {
            let hwnd = win_ffi::CreateWindowExW(
                WS_EX_APPWINDOW,
                class_name.as_ptr(),
                title_wide.as_ptr(),
                BORDERLESS_STYLE,
                40,
                40,
                640,
                420,
                ptr::null_mut(),
                ptr::null_mut(),
                h_instance,
                ptr::null_mut(),
            );
            assert!(!hwnd.is_null(), "CreateWindowExW must succeed");
            win_ffi::SetWindowLongPtrW(
                hwnd,
                GWLP_USERDATA,
                (&*state as *const WinWindowState) as isize,
            );
            win_ffi::ShowWindow(hwnd, SW_SHOW);
            win_ffi::ShowWindow(hwnd, crate::win_ffi::SW_MAXIMIZE);
            win_ffi::UpdateWindow(hwnd);

            let mut msg = MSG::default();
            while win_ffi::PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, crate::win_ffi::PM_REMOVE)
                != 0
            {
                let _ = win_ffi::TranslateMessage(&msg);
                let _ = win_ffi::DispatchMessageW(&msg);
            }

            let mi = query_monitor_info(hwnd, MONITOR_DEFAULTTONEAREST)
                .expect("MonitorFromWindow must return active monitor info");
            let mut cr = RECT::default();
            assert_ne!(win_ffi::GetClientRect(hwnd, &mut cr), 0);
            let mut tl = POINT { x: 0, y: 0 };
            let mut br = POINT {
                x: cr.right,
                y: cr.bottom,
            };
            assert_ne!(win_ffi::ClientToScreen(hwnd, &mut tl), 0);
            assert_ne!(win_ffi::ClientToScreen(hwnd, &mut br), 0);
            let client_screen = RECT {
                left: tl.x,
                top: tl.y,
                right: br.x,
                bottom: br.y,
            };
            *bounds_out.lock().unwrap() = Some((client_screen, mi.rc_work));

            // Also simulate Win10 `WS_THICKFRAME` negative-overhang `NCCALCSIZE_PARAMS` directly
            // to guarantee `WM_NCCALCSIZE` clamps all 4 edges even when `rgrc[0]` overhangs by 8px!
            let mut ncp = NCCALCSIZE_PARAMS {
                rgrc: [
                    RECT {
                        left: mi.rc_work.left - 8,
                        top: mi.rc_work.top - 8,
                        right: mi.rc_work.right + 8,
                        bottom: mi.rc_work.bottom + 8,
                    },
                    RECT::default(),
                    RECT::default(),
                ],
                lppos: ptr::null_mut(),
            };
            if win_ffi::IsZoomed(hwnd) != 0 {
                let ret = lava_wnd_proc(
                    hwnd,
                    WM_NCCALCSIZE,
                    1,
                    (&mut ncp as *mut NCCALCSIZE_PARAMS) as isize,
                );
                assert_eq!(ret, 0);
                assert_eq!(ncp.rgrc[0].left, mi.rc_work.left);
                assert_eq!(ncp.rgrc[0].top, mi.rc_work.top);
                assert_eq!(ncp.rgrc[0].right, mi.rc_work.right);
                assert_eq!(ncp.rgrc[0].bottom, mi.rc_work.bottom);
            }

            win_ffi::SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            win_ffi::DestroyWindow(hwnd);
        }
        let _ = eq_bg;
        drop(state);

        let (cs, work) = bounds_slot.lock().unwrap().expect("captured bounds");
        assert!(
            cs.left >= work.left
                && cs.top >= work.top
                && cs.right <= work.right
                && cs.bottom <= work.bottom,
            "borderless maximized client rect ({},{})..({},{}) must not overshoot work area ({},{})..({},{})",
            cs.left,
            cs.top,
            cs.right,
            cs.bottom,
            work.left,
            work.top,
            work.right,
            work.bottom,
        );
    }
}
