//! Minimal C ABI declarations for Cairo, Pango, PangoCairo, GLib, GObject, GDK4, and GTK4.

use libc::{c_char, c_double, c_int, c_uint, c_ulong, c_void};

#[repr(C)]
pub(crate) struct CairoSurfaceT {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct CairoT {
    _private: [u8; 0],
}

pub(crate) const CAIRO_OPERATOR_SOURCE: c_int = 1;

#[repr(C)]
pub(crate) struct PangoLayout {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct PangoFontDescription {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct PangoAttrList {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct PangoAttribute {
    pub(crate) klass: *const c_void,
    pub(crate) start_index: c_uint,
    pub(crate) end_index: c_uint,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PangoRectangle {
    pub(crate) x: c_int,
    pub(crate) y: c_int,
    pub(crate) width: c_int,
    pub(crate) height: c_int,
}

#[repr(C)]
pub(crate) struct GMainContext {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GClosure {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GdkDisplay {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GdkClipboard {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CairoMatrix {
    pub(crate) xx: c_double,
    pub(crate) yx: c_double,
    pub(crate) xy: c_double,
    pub(crate) yy: c_double,
    pub(crate) x0: c_double,
    pub(crate) y0: c_double,
}

#[repr(C)]
pub(crate) struct GdkFrameClock {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GtkWidget {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GtkWindow {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GtkDrawingArea {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GtkEventController {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GtkGesture {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GtkGestureSingle {
    _private: [u8; 0],
}

pub(crate) const CAIRO_FORMAT_ARGB32: c_int = 0;
pub(crate) const CAIRO_STATUS_SUCCESS: c_int = 0;

pub(crate) const PANGO_SCALE: c_int = 1024;
pub(crate) const PANGO_WEIGHT_NORMAL: c_int = 400;
pub(crate) const PANGO_WEIGHT_BOLD: c_int = 700;
pub(crate) const PANGO_STYLE_NORMAL: c_int = 0;
pub(crate) const PANGO_STYLE_ITALIC: c_int = 2;

pub(crate) const G_SOURCE_REMOVE: c_int = 0;
pub(crate) const G_SOURCE_CONTINUE: c_int = 1;
pub(crate) const G_PRIORITY_DEFAULT: c_int = 0;
pub(crate) const G_PRIORITY_DEFAULT_IDLE: c_int = 200;

pub(crate) const GTK_EVENT_CONTROLLER_SCROLL_BOTH_AXES: c_uint = (1 << 0) | (1 << 1);

pub(crate) const GDK_SHIFT_MASK: c_uint = 1 << 0;
pub(crate) const GDK_CONTROL_MASK: c_uint = 1 << 2;
pub(crate) const GDK_ALT_MASK: c_uint = 1 << 3;
pub(crate) const GDK_SUPER_MASK: c_uint = 1 << 26;
pub(crate) const GDK_META_MASK: c_uint = 1 << 28;

pub(crate) type GSourceFunc = Option<unsafe extern "C" fn(user_data: *mut c_void) -> c_int>;
pub(crate) type GDestroyNotify = Option<unsafe extern "C" fn(data: *mut c_void)>;
pub(crate) type GClosureNotify =
    Option<unsafe extern "C" fn(data: *mut c_void, closure: *mut GClosure)>;
pub(crate) type GCallback = Option<unsafe extern "C" fn()>;
pub(crate) type GtkDrawingAreaDrawFunc = Option<
    unsafe extern "C" fn(
        drawing_area: *mut GtkDrawingArea,
        cr: *mut CairoT,
        width: c_int,
        height: c_int,
        user_data: *mut c_void,
    ),
>;
pub(crate) type GtkTickCallback = Option<
    unsafe extern "C" fn(
        widget: *mut GtkWidget,
        frame_clock: *mut GdkFrameClock,
        user_data: *mut c_void,
    ) -> c_int,
>;

unsafe extern "C" {
    // Cairo
    pub(crate) fn cairo_image_surface_create(
        format: c_int,
        width: c_int,
        height: c_int,
    ) -> *mut CairoSurfaceT;
    #[cfg(test)]
    pub(crate) fn cairo_image_surface_get_data(surface: *mut CairoSurfaceT) -> *mut u8;
    #[cfg(test)]
    pub(crate) fn cairo_image_surface_get_stride(surface: *mut CairoSurfaceT) -> c_int;
    pub(crate) fn cairo_surface_destroy(surface: *mut CairoSurfaceT);
    pub(crate) fn cairo_surface_flush(surface: *mut CairoSurfaceT);
    pub(crate) fn cairo_surface_status(surface: *mut CairoSurfaceT) -> c_int;
    pub(crate) fn cairo_surface_get_device_scale(
        surface: *mut CairoSurfaceT,
        x_scale: *mut c_double,
        y_scale: *mut c_double,
    );
    pub(crate) fn cairo_surface_set_device_scale(
        surface: *mut CairoSurfaceT,
        x_scale: c_double,
        y_scale: c_double,
    );
    pub(crate) fn cairo_surface_write_to_png(
        surface: *mut CairoSurfaceT,
        filename: *const c_char,
    ) -> c_int;
    pub(crate) fn cairo_create(target: *mut CairoSurfaceT) -> *mut CairoT;
    pub(crate) fn cairo_get_target(cr: *mut CairoT) -> *mut CairoSurfaceT;
    pub(crate) fn cairo_get_matrix(cr: *mut CairoT, matrix: *mut CairoMatrix);
    pub(crate) fn cairo_destroy(cr: *mut CairoT);
    pub(crate) fn cairo_save(cr: *mut CairoT);
    pub(crate) fn cairo_restore(cr: *mut CairoT);
    pub(crate) fn cairo_rectangle(
        cr: *mut CairoT,
        x: c_double,
        y: c_double,
        width: c_double,
        height: c_double,
    );
    pub(crate) fn cairo_clip(cr: *mut CairoT);
    pub(crate) fn cairo_new_path(cr: *mut CairoT);
    pub(crate) fn cairo_new_sub_path(cr: *mut CairoT);
    pub(crate) fn cairo_arc(
        cr: *mut CairoT,
        xc: c_double,
        yc: c_double,
        radius: c_double,
        angle1: c_double,
        angle2: c_double,
    );
    pub(crate) fn cairo_move_to(cr: *mut CairoT, x: c_double, y: c_double);
    pub(crate) fn cairo_line_to(cr: *mut CairoT, x: c_double, y: c_double);
    pub(crate) fn cairo_close_path(cr: *mut CairoT);
    pub(crate) fn cairo_set_source_rgba(
        cr: *mut CairoT,
        red: c_double,
        green: c_double,
        blue: c_double,
        alpha: c_double,
    );
    pub(crate) fn cairo_set_source_surface(
        cr: *mut CairoT,
        surface: *mut CairoSurfaceT,
        x: c_double,
        y: c_double,
    );
    pub(crate) fn cairo_set_operator(cr: *mut CairoT, op: c_int);
    pub(crate) fn cairo_set_line_width(cr: *mut CairoT, width: c_double);
    pub(crate) fn cairo_fill(cr: *mut CairoT);
    pub(crate) fn cairo_stroke(cr: *mut CairoT);
    pub(crate) fn cairo_paint(cr: *mut CairoT);

    // Pango & PangoCairo
    pub(crate) fn pango_cairo_create_layout(cr: *mut CairoT) -> *mut PangoLayout;
    pub(crate) fn pango_cairo_show_layout(cr: *mut CairoT, layout: *mut PangoLayout);
    pub(crate) fn pango_font_description_new() -> *mut PangoFontDescription;
    pub(crate) fn pango_font_description_set_family(
        desc: *mut PangoFontDescription,
        family: *const c_char,
    );
    pub(crate) fn pango_font_description_set_size(desc: *mut PangoFontDescription, size: c_int);
    pub(crate) fn pango_font_description_set_weight(desc: *mut PangoFontDescription, weight: c_int);
    pub(crate) fn pango_font_description_set_style(desc: *mut PangoFontDescription, style: c_int);
    pub(crate) fn pango_font_description_free(desc: *mut PangoFontDescription);
    pub(crate) fn pango_layout_set_font_description(
        layout: *mut PangoLayout,
        desc: *const PangoFontDescription,
    );
    pub(crate) fn pango_layout_set_text(
        layout: *mut PangoLayout,
        text: *const c_char,
        length: c_int,
    );
    pub(crate) fn pango_layout_get_extents(
        layout: *mut PangoLayout,
        ink_rect: *mut PangoRectangle,
        logical_rect: *mut PangoRectangle,
    );
    pub(crate) fn pango_layout_get_baseline(layout: *mut PangoLayout) -> c_int;
    pub(crate) fn pango_attr_list_new() -> *mut PangoAttrList;
    pub(crate) fn pango_attr_list_unref(list: *mut PangoAttrList);
    pub(crate) fn pango_attr_list_insert(list: *mut PangoAttrList, attr: *mut PangoAttribute);
    pub(crate) fn pango_attr_foreground_new(red: u16, green: u16, blue: u16)
    -> *mut PangoAttribute;
    pub(crate) fn pango_attr_style_new(style: c_int) -> *mut PangoAttribute;
    pub(crate) fn pango_layout_set_attributes(layout: *mut PangoLayout, attrs: *mut PangoAttrList);
    pub(crate) fn g_object_unref(object: *mut c_void);

    // GLib & GObject
    pub(crate) fn g_signal_connect_data(
        instance: *mut c_void,
        detailed_signal: *const c_char,
        c_handler: GCallback,
        data: *mut c_void,
        destroy_data: GClosureNotify,
        connect_flags: c_uint,
    ) -> c_ulong;
    pub(crate) fn g_idle_add_full(
        priority: c_int,
        function: GSourceFunc,
        data: *mut c_void,
        notify: GDestroyNotify,
    ) -> c_uint;
    pub(crate) fn g_main_context_default() -> *mut GMainContext;
    pub(crate) fn g_main_context_iteration(context: *mut GMainContext, may_block: c_int) -> c_int;

    // GDK4 & GTK4
    pub(crate) fn gtk_init_check() -> c_int;
    pub(crate) fn gtk_window_new() -> *mut GtkWidget;
    pub(crate) fn gtk_window_set_title(window: *mut GtkWindow, title: *const c_char);
    pub(crate) fn gtk_window_set_default_size(window: *mut GtkWindow, width: c_int, height: c_int);
    pub(crate) fn gtk_window_set_child(window: *mut GtkWindow, child: *mut GtkWidget);
    pub(crate) fn gtk_window_present(window: *mut GtkWindow);
    pub(crate) fn gtk_window_destroy(window: *mut GtkWindow);

    pub(crate) fn gtk_widget_queue_draw(widget: *mut GtkWidget);
    pub(crate) fn gtk_widget_set_focusable(widget: *mut GtkWidget, focusable: c_int);
    pub(crate) fn gtk_widget_set_can_focus(widget: *mut GtkWidget, can_focus: c_int);
    pub(crate) fn gtk_widget_grab_focus(widget: *mut GtkWidget) -> c_int;
    pub(crate) fn gtk_widget_set_hexpand(widget: *mut GtkWidget, expand: c_int);
    pub(crate) fn gtk_widget_set_vexpand(widget: *mut GtkWidget, expand: c_int);
    pub(crate) fn gtk_widget_get_scale_factor(widget: *mut GtkWidget) -> c_int;
    pub(crate) fn gtk_widget_get_width(widget: *mut GtkWidget) -> c_int;
    pub(crate) fn gtk_widget_get_height(widget: *mut GtkWidget) -> c_int;
    pub(crate) fn gtk_widget_set_cursor_from_name(widget: *mut GtkWidget, name: *const c_char);
    pub(crate) fn gtk_widget_add_controller(
        widget: *mut GtkWidget,
        controller: *mut GtkEventController,
    );
    pub(crate) fn gtk_widget_add_tick_callback(
        widget: *mut GtkWidget,
        callback: GtkTickCallback,
        user_data: *mut c_void,
        notify: GDestroyNotify,
    ) -> c_uint;
    pub(crate) fn gdk_frame_clock_get_frame_time(frame_clock: *mut GdkFrameClock) -> i64;

    pub(crate) fn gtk_drawing_area_new() -> *mut GtkWidget;
    pub(crate) fn gtk_drawing_area_set_draw_func(
        self_: *mut GtkDrawingArea,
        draw_func: GtkDrawingAreaDrawFunc,
        user_data: *mut c_void,
        destroy: GDestroyNotify,
    );

    pub(crate) fn gtk_event_controller_key_new() -> *mut GtkEventController;
    pub(crate) fn gtk_gesture_click_new() -> *mut GtkGesture;
    pub(crate) fn gtk_gesture_single_set_button(gesture: *mut GtkGestureSingle, button: c_uint);
    pub(crate) fn gtk_gesture_single_get_current_button(gesture: *mut GtkGestureSingle) -> c_uint;
    pub(crate) fn gtk_event_controller_motion_new() -> *mut GtkEventController;
    pub(crate) fn gtk_event_controller_scroll_new(flags: c_uint) -> *mut GtkEventController;
    pub(crate) fn gtk_event_controller_focus_new() -> *mut GtkEventController;
    pub(crate) fn gtk_event_controller_get_current_event_state(
        controller: *mut GtkEventController,
    ) -> c_uint;

    pub(crate) fn gdk_keyval_to_unicode(keyval: c_uint) -> u32;
    pub(crate) fn gdk_keyval_name(keyval: c_uint) -> *const c_char;
    pub(crate) fn gdk_display_get_default() -> *mut GdkDisplay;
    pub(crate) fn gdk_display_get_clipboard(display: *mut GdkDisplay) -> *mut GdkClipboard;
    pub(crate) fn gdk_clipboard_set_text(clipboard: *mut GdkClipboard, text: *const c_char);
}
