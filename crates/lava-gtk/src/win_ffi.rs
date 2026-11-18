//! Win32 (`user32`, `gdi32`, `imm32`, `kernel32`) C-ABI definitions strictly backward-compatible
//! to Windows 10 version 1709 (RS3, Build 16299).

#![allow(dead_code, clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub(crate) type BOOL = i32;
pub(crate) type UINT = u32;
pub(crate) type DWORD = u32;
pub(crate) type WORD = u16;
pub(crate) type BYTE = u8;
pub(crate) type LONG = i32;
pub(crate) type WPARAM = usize;
pub(crate) type LPARAM = isize;
pub(crate) type LRESULT = isize;
pub(crate) type ATOM = u16;

pub(crate) type HANDLE = *mut c_void;
pub(crate) type HINSTANCE = *mut c_void;
pub(crate) type HMODULE = *mut c_void;
pub(crate) type HWND = *mut c_void;
pub(crate) type HDC = *mut c_void;
pub(crate) type HGDIOBJ = *mut c_void;
pub(crate) type HBITMAP = *mut c_void;
pub(crate) type HFONT = *mut c_void;
pub(crate) type HICON = *mut c_void;
pub(crate) type HCURSOR = *mut c_void;
pub(crate) type HBRUSH = *mut c_void;
pub(crate) type HMENU = *mut c_void;
pub(crate) type HIMC = *mut c_void;
pub(crate) type HMONITOR = *mut c_void;

pub(crate) type WNDPROC = Option<unsafe extern "system" fn(HWND, UINT, WPARAM, LPARAM) -> LRESULT>;

// Window Styles & Extended Styles
pub(crate) const WS_OVERLAPPED: DWORD = 0x0000_0000;
pub(crate) const WS_POPUP: DWORD = 0x8000_0000;
pub(crate) const WS_VISIBLE: DWORD = 0x1000_0000;
pub(crate) const WS_CLIPSIBLINGS: DWORD = 0x0400_0000;
pub(crate) const WS_CLIPCHILDREN: DWORD = 0x0200_0000;
pub(crate) const WS_CAPTION: DWORD = 0x00C0_0000;
pub(crate) const WS_SYSMENU: DWORD = 0x0008_0000;
pub(crate) const WS_THICKFRAME: DWORD = 0x0004_0000;
pub(crate) const WS_MINIMIZEBOX: DWORD = 0x0002_0000;
pub(crate) const WS_MAXIMIZEBOX: DWORD = 0x0001_0000;
pub(crate) const WS_OVERLAPPEDWINDOW: DWORD =
    WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;

pub(crate) const WS_EX_APPWINDOW: DWORD = 0x0004_0000;

pub(crate) const CS_HREDRAW: UINT = 0x0002;
pub(crate) const CS_VREDRAW: UINT = 0x0001;
pub(crate) const CS_DBLCLKS: UINT = 0x0008;

pub(crate) const SM_CXSCREEN: i32 = 0;
pub(crate) const SM_CYSCREEN: i32 = 1;
pub(crate) const CW_USEDEFAULT: i32 = 0x8000_0000_u32 as i32;
pub(crate) const SW_MAXIMIZE: i32 = 3;
pub(crate) const SW_SHOW: i32 = 5;
pub(crate) const SW_RESTORE: i32 = 9;

pub(crate) const MONITOR_DEFAULTTOPRIMARY: DWORD = 0x0000_0001;
pub(crate) const MONITOR_DEFAULTTONEAREST: DWORD = 0x0000_0002;

pub(crate) const GWLP_USERDATA: i32 = -21;
pub(crate) const GWL_STYLE: i32 = -16;

pub(crate) const SWP_NOSIZE: UINT = 0x0001;
pub(crate) const SWP_NOMOVE: UINT = 0x0002;
pub(crate) const SWP_NOZORDER: UINT = 0x0004;
pub(crate) const SWP_NOACTIVATE: UINT = 0x0010;
pub(crate) const SWP_FRAMECHANGED: UINT = 0x0020;

// Window Messages
pub(crate) const WM_DESTROY: UINT = 0x0002;
pub(crate) const WM_SIZE: UINT = 0x0005;
pub(crate) const WM_SETFOCUS: UINT = 0x0007;
pub(crate) const WM_KILLFOCUS: UINT = 0x0008;
pub(crate) const WM_PAINT: UINT = 0x000F;
pub(crate) const WM_CLOSE: UINT = 0x0010;
pub(crate) const WM_QUIT: UINT = 0x0012;
pub(crate) const WM_ERASEBKGND: UINT = 0x0014;
pub(crate) const WM_SETCURSOR: UINT = 0x0020;
pub(crate) const WM_GETMINMAXINFO: UINT = 0x0024;
pub(crate) const WM_NCCALCSIZE: UINT = 0x0083;
pub(crate) const WM_NCHITTEST: UINT = 0x0084;
pub(crate) const WM_KEYDOWN: UINT = 0x0100;
pub(crate) const WM_KEYUP: UINT = 0x0101;
pub(crate) const WM_CHAR: UINT = 0x0102;
pub(crate) const WM_SYSKEYDOWN: UINT = 0x0104;
pub(crate) const WM_SYSKEYUP: UINT = 0x0105;
pub(crate) const WM_SYSCHAR: UINT = 0x0106;
pub(crate) const WM_UNICHAR: UINT = 0x0109;
pub(crate) const WM_IME_STARTCOMPOSITION: UINT = 0x010D;
pub(crate) const WM_TIMER: UINT = 0x0113;
pub(crate) const WM_MOUSEMOVE: UINT = 0x0200;
pub(crate) const WM_LBUTTONDOWN: UINT = 0x0201;
pub(crate) const WM_LBUTTONUP: UINT = 0x0202;
pub(crate) const WM_LBUTTONDBLCLK: UINT = 0x0203;
pub(crate) const WM_RBUTTONDOWN: UINT = 0x0204;
pub(crate) const WM_RBUTTONUP: UINT = 0x0205;
pub(crate) const WM_RBUTTONDBLCLK: UINT = 0x0206;
pub(crate) const WM_MBUTTONDOWN: UINT = 0x0207;
pub(crate) const WM_MBUTTONUP: UINT = 0x0208;
pub(crate) const WM_MBUTTONDBLCLK: UINT = 0x0209;
pub(crate) const WM_MOUSEWHEEL: UINT = 0x020A;
pub(crate) const WM_MOUSEHWHEEL: UINT = 0x020E;
pub(crate) const WM_DPICHANGED: UINT = 0x02E0;
pub(crate) const WM_APP: UINT = 0x8000;
pub(crate) const WM_LAVA_WAKE: UINT = WM_APP + 1;
pub(crate) const WM_LAVA_QUIT: UINT = WM_APP + 2;

pub(crate) const PM_REMOVE: UINT = 0x0001;
pub(crate) const UNICODE_NOCHAR: usize = 0xFFFF;

// Hit-test codes for borderless resize
pub(crate) const HTCLIENT: isize = 1;
pub(crate) const HTCAPTION: isize = 2;
pub(crate) const HTLEFT: isize = 10;
pub(crate) const HTRIGHT: isize = 11;
pub(crate) const HTTOP: isize = 12;
pub(crate) const HTTOPLEFT: isize = 13;
pub(crate) const HTTOPRIGHT: isize = 14;
pub(crate) const HTBOTTOM: isize = 15;
pub(crate) const HTBOTTOMLEFT: isize = 16;
pub(crate) const HTBOTTOMRIGHT: isize = 17;

// Standard Cursors (`MAKEINTRESOURCEW`)
pub(crate) const IDC_ARROW: *const u16 = 32512usize as *const u16;
pub(crate) const IDC_IBEAM: *const u16 = 32513usize as *const u16;
pub(crate) const IDC_SIZENS: *const u16 = 32645usize as *const u16;
pub(crate) const IDC_SIZEWE: *const u16 = 32644usize as *const u16;
pub(crate) const IDC_HAND: *const u16 = 32649usize as *const u16;

// Clipboard & Global Memory
pub(crate) const CF_UNICODETEXT: UINT = 13;
pub(crate) const GMEM_MOVEABLE: UINT = 0x0002;

// GDI & Font Constants
pub(crate) const BI_RGB: DWORD = 0;
pub(crate) const DIB_RGB_COLORS: UINT = 0;
pub(crate) const SRCCOPY: DWORD = 0x00CC_0020;
pub(crate) const LOGPIXELSX: i32 = 88;
pub(crate) const LOGPIXELSY: i32 = 90;

pub(crate) const FR_PRIVATE: DWORD = 0x10;
pub(crate) const DEFAULT_CHARSET: DWORD = 1;
pub(crate) const OUT_TT_PRECIS: DWORD = 4;
pub(crate) const CLIP_DEFAULT_PRECIS: DWORD = 0;
pub(crate) const ANTIALIASED_QUALITY: DWORD = 4;
pub(crate) const CLEARTYPE_QUALITY: DWORD = 5;
pub(crate) const DEFAULT_PITCH: DWORD = 0;
pub(crate) const FIXED_PITCH: DWORD = 1;
pub(crate) const FF_MODERN: DWORD = 3 << 4;
pub(crate) const FW_NORMAL: i32 = 400;
pub(crate) const FW_BOLD: i32 = 700;

pub(crate) const GGI_MARK_NONEXISTING_GLYPHS: DWORD = 0x0001;
pub(crate) const GGO_METRICS: UINT = 0;
pub(crate) const GGO_GRAY8_BITMAP: UINT = 6;
pub(crate) const GGO_GLYPH_INDEX: UINT = 0x0080;
pub(crate) const GDI_ERROR: DWORD = 0xFFFF_FFFF;

// IME Composition Window
pub(crate) const CFS_POINT: DWORD = 0x0002;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct POINT {
    pub x: LONG,
    pub y: LONG,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct RECT {
    pub left: LONG,
    pub top: LONG,
    pub right: LONG,
    pub bottom: LONG,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct MSG {
    pub hwnd: HWND,
    pub message: UINT,
    pub w_param: WPARAM,
    pub l_param: LPARAM,
    pub time: DWORD,
    pub pt: POINT,
}

impl Default for MSG {
    fn default() -> Self {
        Self {
            hwnd: std::ptr::null_mut(),
            message: 0,
            w_param: 0,
            l_param: 0,
            time: 0,
            pt: POINT::default(),
        }
    }
}

#[repr(C)]
pub(crate) struct WNDCLASSEXW {
    pub cb_size: UINT,
    pub style: UINT,
    pub lpfn_wnd_proc: WNDPROC,
    pub cb_cls_extra: i32,
    pub cb_wnd_extra: i32,
    pub h_instance: HINSTANCE,
    pub h_icon: HICON,
    pub h_cursor: HCURSOR,
    pub hbr_background: HBRUSH,
    pub lpsz_menu_name: *const u16,
    pub lpsz_class_name: *const u16,
    pub h_icon_sm: HICON,
}

#[repr(C)]
pub(crate) struct PAINTSTRUCT {
    pub hdc: HDC,
    pub f_erase: BOOL,
    pub rc_paint: RECT,
    pub f_restore: BOOL,
    pub f_inc_update: BOOL,
    pub rgb_reserved: [BYTE; 32],
}

impl Default for PAINTSTRUCT {
    fn default() -> Self {
        Self {
            hdc: std::ptr::null_mut(),
            f_erase: 0,
            rc_paint: RECT::default(),
            f_restore: 0,
            f_inc_update: 0,
            rgb_reserved: [0; 32],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct MINMAXINFO {
    pub pt_reserved: POINT,
    pub pt_max_size: POINT,
    pub pt_max_position: POINT,
    pub pt_min_track_size: POINT,
    pub pt_max_track_size: POINT,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct MONITORINFO {
    pub cb_size: DWORD,
    pub rc_monitor: RECT,
    pub rc_work: RECT,
    pub dw_flags: DWORD,
}

#[repr(C)]
pub(crate) struct NCCALCSIZE_PARAMS {
    pub rgrc: [RECT; 3],
    pub lppos: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct BITMAPINFOHEADER {
    pub bi_size: DWORD,
    pub bi_width: LONG,
    pub bi_height: LONG,
    pub bi_planes: WORD,
    pub bi_bit_count: WORD,
    pub bi_compression: DWORD,
    pub bi_size_image: DWORD,
    pub bi_x_pels_per_meter: LONG,
    pub bi_y_pels_per_meter: LONG,
    pub bi_clr_used: DWORD,
    pub bi_clr_important: DWORD,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct RGBQUAD {
    pub rgb_blue: BYTE,
    pub rgb_green: BYTE,
    pub rgb_red: BYTE,
    pub rgb_reserved: BYTE,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct BITMAPINFO {
    pub bmi_header: BITMAPINFOHEADER,
    pub bmi_colors: [RGBQUAD; 1],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct TEXTMETRICW {
    pub tm_height: LONG,
    pub tm_ascent: LONG,
    pub tm_descent: LONG,
    pub tm_internal_leading: LONG,
    pub tm_external_leading: LONG,
    pub tm_ave_char_width: LONG,
    pub tm_max_char_width: LONG,
    pub tm_weight: LONG,
    pub tm_overhang: LONG,
    pub tm_digitized_aspect_x: LONG,
    pub tm_digitized_aspect_y: LONG,
    pub tm_first_char: u16,
    pub tm_last_char: u16,
    pub tm_default_char: u16,
    pub tm_break_char: u16,
    pub tm_italic: BYTE,
    pub tm_underlined: BYTE,
    pub tm_struck_out: BYTE,
    pub tm_pitch_and_family: BYTE,
    pub tm_char_set: BYTE,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct SIZE {
    pub cx: LONG,
    pub cy: LONG,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct FIXED {
    pub fract: WORD,
    pub value: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct MAT2 {
    pub e_m11: FIXED,
    pub e_m12: FIXED,
    pub e_m21: FIXED,
    pub e_m22: FIXED,
}

impl MAT2 {
    pub(crate) const fn identity() -> Self {
        Self {
            e_m11: FIXED { fract: 0, value: 1 },
            e_m12: FIXED { fract: 0, value: 0 },
            e_m21: FIXED { fract: 0, value: 0 },
            e_m22: FIXED { fract: 0, value: 1 },
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct GLYPHMETRICS {
    pub gm_black_box_x: UINT,
    pub gm_black_box_y: UINT,
    pub gm_pt_glyph_origin: POINT,
    pub gm_cell_inc_x: i16,
    pub gm_cell_inc_y: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct COMPOSITIONFORM {
    pub dw_style: DWORD,
    pub pt_current_pos: POINT,
    pub rc_area: RECT,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub(crate) fn GetModuleHandleW(lp_module_name: *const u16) -> HMODULE;
    pub(crate) fn GetProcAddress(h_module: HMODULE, lp_proc_name: *const u8) -> *mut c_void;
    pub(crate) fn GlobalAlloc(u_flags: UINT, dw_bytes: usize) -> HANDLE;
    pub(crate) fn GlobalLock(h_mem: HANDLE) -> *mut c_void;
    pub(crate) fn GlobalUnlock(h_mem: HANDLE) -> BOOL;
    pub(crate) fn GlobalFree(h_mem: HANDLE) -> HANDLE;
}

#[link(name = "user32")]
unsafe extern "system" {
    pub(crate) fn RegisterClassExW(lpwcx: *const WNDCLASSEXW) -> ATOM;
    pub(crate) fn CreateWindowExW(
        dw_ex_style: DWORD,
        lp_class_name: *const u16,
        lp_window_name: *const u16,
        dw_style: DWORD,
        x: i32,
        y: i32,
        n_width: i32,
        n_height: i32,
        h_wnd_parent: HWND,
        h_menu: HMENU,
        h_instance: HINSTANCE,
        lp_param: *mut c_void,
    ) -> HWND;
    pub(crate) fn DestroyWindow(h_wnd: HWND) -> BOOL;
    pub(crate) fn ShowWindow(h_wnd: HWND, n_cmd_show: i32) -> BOOL;
    pub(crate) fn UpdateWindow(h_wnd: HWND) -> BOOL;
    pub(crate) fn DefWindowProcW(
        h_wnd: HWND,
        msg: UINT,
        w_param: WPARAM,
        l_param: LPARAM,
    ) -> LRESULT;
    pub(crate) fn GetMessageW(
        lp_msg: *mut MSG,
        h_wnd: HWND,
        w_msg_filter_min: UINT,
        w_msg_filter_max: UINT,
    ) -> BOOL;
    pub(crate) fn PeekMessageW(
        lp_msg: *mut MSG,
        h_wnd: HWND,
        w_msg_filter_min: UINT,
        w_msg_filter_max: UINT,
        w_remove_msg: UINT,
    ) -> BOOL;
    pub(crate) fn TranslateMessage(lp_msg: *const MSG) -> BOOL;
    pub(crate) fn DispatchMessageW(lp_msg: *const MSG) -> LRESULT;
    pub(crate) fn PostMessageW(h_wnd: HWND, msg: UINT, w_param: WPARAM, l_param: LPARAM) -> BOOL;
    pub(crate) fn PostQuitMessage(n_exit_code: i32);
    pub(crate) fn InvalidateRect(h_wnd: HWND, lp_rect: *const RECT, b_erase: BOOL) -> BOOL;
    pub(crate) fn BeginPaint(h_wnd: HWND, lp_paint: *mut PAINTSTRUCT) -> HDC;
    pub(crate) fn EndPaint(h_wnd: HWND, lp_paint: *const PAINTSTRUCT) -> BOOL;
    pub(crate) fn GetClientRect(h_wnd: HWND, lp_rect: *mut RECT) -> BOOL;
    pub(crate) fn GetWindowRect(h_wnd: HWND, lp_rect: *mut RECT) -> BOOL;
    pub(crate) fn AdjustWindowRectEx(
        lp_rect: *mut RECT,
        dw_style: DWORD,
        b_menu: BOOL,
        dw_ex_style: DWORD,
    ) -> BOOL;
    pub(crate) fn SetWindowTextW(h_wnd: HWND, lp_string: *const u16) -> BOOL;
    pub(crate) fn SetWindowLongPtrW(h_wnd: HWND, n_index: i32, dw_new_long: isize) -> isize;
    pub(crate) fn GetWindowLongPtrW(h_wnd: HWND, n_index: i32) -> isize;
    pub(crate) fn SetWindowPos(
        h_wnd: HWND,
        h_wnd_insert_after: HWND,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        u_flags: UINT,
    ) -> BOOL;
    pub(crate) fn SetTimer(
        h_wnd: HWND,
        n_id_event: usize,
        u_elapse: UINT,
        lp_timer_func: *mut c_void,
    ) -> usize;
    pub(crate) fn KillTimer(h_wnd: HWND, u_id_event: usize) -> BOOL;
    pub(crate) fn LoadCursorW(h_instance: HINSTANCE, lp_cursor_name: *const u16) -> HCURSOR;
    pub(crate) fn SetCursor(h_cursor: HCURSOR) -> HCURSOR;
    pub(crate) fn SetCapture(h_wnd: HWND) -> HWND;
    pub(crate) fn ReleaseCapture() -> BOOL;
    pub(crate) fn GetKeyState(n_virt_key: i32) -> i16;
    pub(crate) fn MapVirtualKeyW(u_code: UINT, u_map_type: UINT) -> UINT;
    pub(crate) fn OpenClipboard(h_wnd_new_owner: HWND) -> BOOL;
    pub(crate) fn CloseClipboard() -> BOOL;
    pub(crate) fn EmptyClipboard() -> BOOL;
    pub(crate) fn GetClipboardData(u_format: UINT) -> HANDLE;
    pub(crate) fn SetClipboardData(u_format: UINT, h_mem: HANDLE) -> HANDLE;
    pub(crate) fn GetDC(h_wnd: HWND) -> HDC;
    pub(crate) fn ReleaseDC(h_wnd: HWND, h_dc: HDC) -> i32;
    pub(crate) fn GetSystemMetrics(n_index: i32) -> i32;
    pub(crate) fn IsZoomed(h_wnd: HWND) -> BOOL;
    pub(crate) fn ClientToScreen(h_wnd: HWND, lp_point: *mut POINT) -> BOOL;
    pub(crate) fn MonitorFromWindow(h_wnd: HWND, dw_flags: DWORD) -> HMONITOR;
    pub(crate) fn GetMonitorInfoW(h_monitor: HMONITOR, lpmi: *mut MONITORINFO) -> BOOL;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    pub(crate) fn CreateCompatibleDC(hdc: HDC) -> HDC;
    pub(crate) fn DeleteDC(hdc: HDC) -> BOOL;
    pub(crate) fn SelectObject(hdc: HDC, h: HGDIOBJ) -> HGDIOBJ;
    pub(crate) fn DeleteObject(ho: HGDIOBJ) -> BOOL;
    pub(crate) fn GetDeviceCaps(hdc: HDC, index: i32) -> i32;
    pub(crate) fn AddFontResourceExW(name: *const u16, fl: DWORD, res: *mut c_void) -> i32;
    pub(crate) fn CreateFontW(
        c_height: i32,
        c_width: i32,
        c_escapement: i32,
        c_orientation: i32,
        c_weight: i32,
        b_italic: DWORD,
        b_underline: DWORD,
        b_strike_out: DWORD,
        i_char_set: DWORD,
        i_out_precision: DWORD,
        i_clip_precision: DWORD,
        i_quality: DWORD,
        i_pitch_and_family: DWORD,
        psz_face_name: *const u16,
    ) -> HFONT;
    pub(crate) fn GetTextMetricsW(hdc: HDC, lptm: *mut TEXTMETRICW) -> BOOL;
    pub(crate) fn GetTextExtentPoint32W(
        hdc: HDC,
        lp_string: *const u16,
        c: i32,
        psizl: *mut SIZE,
    ) -> BOOL;
    pub(crate) fn GetGlyphIndicesW(
        hdc: HDC,
        lpstr: *const u16,
        c: i32,
        pgi: *mut WORD,
        fl: DWORD,
    ) -> DWORD;
    pub(crate) fn GetFontData(
        hdc: HDC,
        dw_table: DWORD,
        dw_offset: DWORD,
        pv_buffer: *mut c_void,
        cj_buffer: DWORD,
    ) -> DWORD;
    pub(crate) fn GetGlyphOutlineW(
        hdc: HDC,
        u_char: UINT,
        fu_format: UINT,
        lpgm: *mut GLYPHMETRICS,
        cj_buffer: DWORD,
        pv_buffer: *mut c_void,
        lpmat2: *const MAT2,
    ) -> DWORD;
    pub(crate) fn CreateDIBSection(
        hdc: HDC,
        pbmi: *const BITMAPINFO,
        usage: UINT,
        ppv_bits: *mut *mut c_void,
        h_section: HANDLE,
        offset: DWORD,
    ) -> HBITMAP;
    pub(crate) fn BitBlt(
        hdc: HDC,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        hdc_src: HDC,
        x1: i32,
        y1: i32,
        rop: DWORD,
    ) -> BOOL;
    pub(crate) fn GdiFlush() -> BOOL;
}

#[link(name = "imm32")]
unsafe extern "system" {
    pub(crate) fn ImmGetContext(h_wnd: HWND) -> HIMC;
    pub(crate) fn ImmSetCompositionWindow(himc: HIMC, lp_comp_form: *const COMPOSITIONFORM)
    -> BOOL;
    pub(crate) fn ImmReleaseContext(h_wnd: HWND, himc: HIMC) -> BOOL;
}

pub(crate) fn to_wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
