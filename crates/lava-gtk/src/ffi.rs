//! Minimal C ABI declarations for Cairo, Pango, PangoCairo, GLib, GObject, Graphene, GDK4, GSK4, and GTK4.

use libc::{c_char, c_double, c_float, c_int, c_uint, c_ulong, c_void, size_t};

pub(crate) type GType = size_t;

#[repr(C)]
pub(crate) struct CairoSurfaceT {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct CairoT {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct CairoPatternT {
    _private: [u8; 0],
}

pub(crate) const CAIRO_OPERATOR_SOURCE: c_int = 1;
pub(crate) const CAIRO_FILTER_NEAREST: c_int = 3;

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
pub(crate) struct GBytes {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GObject {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct GTypeQuery {
    pub(crate) type_: GType,
    pub(crate) type_name: *const c_char,
    pub(crate) class_size: c_uint,
    pub(crate) instance_size: c_uint,
}

impl Default for GTypeQuery {
    fn default() -> Self {
        Self {
            type_: 0,
            type_name: std::ptr::null(),
            class_size: 0,
            instance_size: 0,
        }
    }
}

#[repr(C)]
pub(crate) struct GObjectClass {
    pub(crate) g_type: GType,
    pub(crate) construct_properties: *mut c_void,
    pub(crate) constructor: *const c_void,
    pub(crate) set_property: *const c_void,
    pub(crate) get_property: *const c_void,
    pub(crate) dispose: Option<unsafe extern "C" fn(object: *mut GObject)>,
    pub(crate) finalize: Option<unsafe extern "C" fn(object: *mut GObject)>,
    pub(crate) dispatch_properties_changed: *const c_void,
    pub(crate) notify: *const c_void,
    pub(crate) constructed: *const c_void,
    pub(crate) flags: size_t,
    pub(crate) n_construct_properties: size_t,
    pub(crate) pspecs: *mut c_void,
    pub(crate) n_pspecs: size_t,
    pub(crate) pdummy: [*mut c_void; 3],
}

#[repr(C)]
pub(crate) struct GtkWidget {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GtkNative {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GdkSurface {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct PangoContext {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct CairoFontOptionsT {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GtkSnapshot {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GdkTexture {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GskPath {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct GskPathBuilder {
    _private: [u8; 0],
}

#[cfg(test)]
#[repr(C)]
pub(crate) struct GskRenderNode {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GdkRGBA {
    pub(crate) red: c_float,
    pub(crate) green: c_float,
    pub(crate) blue: c_float,
    pub(crate) alpha: c_float,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GraphenePoint {
    pub(crate) x: c_float,
    pub(crate) y: c_float,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GrapheneSize {
    pub(crate) width: c_float,
    pub(crate) height: c_float,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GrapheneRect {
    pub(crate) origin: GraphenePoint,
    pub(crate) size: GrapheneSize,
}

impl GrapheneRect {
    #[inline]
    pub(crate) const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            origin: GraphenePoint { x, y },
            size: GrapheneSize { width, height },
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GskRoundedRect {
    pub(crate) bounds: GrapheneRect,
    pub(crate) corner: [GrapheneSize; 4],
}

impl GskRoundedRect {
    #[inline]
    pub(crate) const fn from_rect(bounds: GrapheneRect) -> Self {
        Self {
            bounds,
            corner: [GrapheneSize {
                width: 0.0,
                height: 0.0,
            }; 4],
        }
    }
}

#[repr(C)]
pub(crate) struct GtkWidgetClass {
    pub(crate) parent_class: GObjectClass,
    pub(crate) show: *const c_void,
    pub(crate) hide: *const c_void,
    pub(crate) map: *const c_void,
    pub(crate) unmap: *const c_void,
    pub(crate) realize: *const c_void,
    pub(crate) unrealize: *const c_void,
    pub(crate) root: *const c_void,
    pub(crate) unroot: *const c_void,
    pub(crate) size_allocate: *const c_void,
    pub(crate) state_flags_changed: *const c_void,
    pub(crate) direction_changed: *const c_void,
    pub(crate) get_request_mode: *const c_void,
    pub(crate) measure: *const c_void,
    pub(crate) mnemonic_activate: *const c_void,
    pub(crate) grab_focus: *const c_void,
    pub(crate) focus: *const c_void,
    pub(crate) set_focus_child: *const c_void,
    pub(crate) move_focus: *const c_void,
    pub(crate) keynav_failed: *const c_void,
    pub(crate) query_tooltip: *const c_void,
    pub(crate) compute_expand: *const c_void,
    pub(crate) css_changed: *const c_void,
    pub(crate) system_setting_changed: *const c_void,
    pub(crate) snapshot:
        Option<unsafe extern "C" fn(widget: *mut GtkWidget, snapshot: *mut GtkSnapshot)>,
    pub(crate) contains: *const c_void,
    pub(crate) priv_: *mut c_void,
    pub(crate) padding: [*mut c_void; 8],
}

#[repr(C)]
pub(crate) struct GtkDrawingAreaClass {
    pub(crate) parent_class: GtkWidgetClass,
    pub(crate) resize: *const c_void,
    pub(crate) padding: [*mut c_void; 8],
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
pub(crate) const CAIRO_ANTIALIAS_GRAY: c_int = 2;
#[cfg(test)]
pub(crate) const CAIRO_ANTIALIAS_SUBPIXEL: c_int = 3;
pub(crate) const CAIRO_HINT_METRICS_OFF: c_int = 1;

#[cfg(target_endian = "little")]
pub(crate) const CAIRO_ARGB32_GDK_MEMORY_FORMAT: c_int = 0; // GDK_MEMORY_B8G8R8A8_PREMULTIPLIED
#[cfg(target_endian = "big")]
pub(crate) const CAIRO_ARGB32_GDK_MEMORY_FORMAT: c_int = 1; // GDK_MEMORY_A8R8G8B8_PREMULTIPLIED

pub(crate) const GSK_FILL_RULE_WINDING: c_int = 0;

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
pub(crate) type GClassInitFunc =
    Option<unsafe extern "C" fn(g_class: *mut c_void, class_data: *mut c_void)>;
pub(crate) type GInstanceInitFunc =
    Option<unsafe extern "C" fn(instance: *mut c_void, g_class: *mut c_void)>;
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
    pub(crate) fn cairo_image_surface_get_data(surface: *mut CairoSurfaceT) -> *mut u8;
    pub(crate) fn cairo_image_surface_get_stride(surface: *mut CairoSurfaceT) -> c_int;
    pub(crate) fn cairo_image_surface_get_width(surface: *mut CairoSurfaceT) -> c_int;
    pub(crate) fn cairo_image_surface_get_height(surface: *mut CairoSurfaceT) -> c_int;
    pub(crate) fn cairo_surface_reference(surface: *mut CairoSurfaceT) -> *mut CairoSurfaceT;
    pub(crate) fn cairo_surface_get_reference_count(surface: *mut CairoSurfaceT) -> c_uint;
    pub(crate) fn cairo_surface_destroy(surface: *mut CairoSurfaceT);
    pub(crate) fn cairo_surface_flush(surface: *mut CairoSurfaceT);
    pub(crate) fn cairo_surface_status(surface: *mut CairoSurfaceT) -> c_int;
    pub(crate) fn cairo_surface_get_device_scale(
        surface: *mut CairoSurfaceT,
        x_scale: *mut c_double,
        y_scale: *mut c_double,
    );
    pub(crate) fn cairo_surface_write_to_png(
        surface: *mut CairoSurfaceT,
        filename: *const c_char,
    ) -> c_int;
    pub(crate) fn cairo_create(target: *mut CairoSurfaceT) -> *mut CairoT;
    pub(crate) fn cairo_get_target(cr: *mut CairoT) -> *mut CairoSurfaceT;
    pub(crate) fn cairo_get_source(cr: *mut CairoT) -> *mut CairoPatternT;
    pub(crate) fn cairo_pattern_set_filter(pattern: *mut CairoPatternT, filter: c_int);
    pub(crate) fn cairo_get_matrix(cr: *mut CairoT, matrix: *mut CairoMatrix);
    pub(crate) fn cairo_destroy(cr: *mut CairoT);
    pub(crate) fn cairo_save(cr: *mut CairoT);
    pub(crate) fn cairo_restore(cr: *mut CairoT);
    pub(crate) fn cairo_scale(cr: *mut CairoT, sx: c_double, sy: c_double);
    pub(crate) fn cairo_identity_matrix(cr: *mut CairoT);
    pub(crate) fn cairo_font_options_create() -> *mut CairoFontOptionsT;
    pub(crate) fn cairo_font_options_destroy(options: *mut CairoFontOptionsT);
    pub(crate) fn cairo_font_options_set_antialias(
        options: *mut CairoFontOptionsT,
        antialias: c_int,
    );
    pub(crate) fn cairo_font_options_set_hint_metrics(
        options: *mut CairoFontOptionsT,
        hint_metrics: c_int,
    );
    pub(crate) fn cairo_set_font_options(cr: *mut CairoT, options: *const CairoFontOptionsT);
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
    pub(crate) fn pango_layout_get_context(layout: *mut PangoLayout) -> *mut PangoContext;
    pub(crate) fn pango_cairo_context_set_font_options(
        context: *mut PangoContext,
        options: *const CairoFontOptionsT,
    );
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
    pub(crate) fn g_bytes_new_with_free_func(
        data: *const c_void,
        size: size_t,
        free_func: GDestroyNotify,
        user_data: *mut c_void,
    ) -> *mut GBytes;
    pub(crate) fn g_bytes_unref(bytes: *mut GBytes);
    pub(crate) fn g_type_query(type_: GType, query: *mut GTypeQuery);
    pub(crate) fn g_type_register_static_simple(
        parent_type: GType,
        type_name: *const c_char,
        class_size: c_uint,
        class_init: GClassInitFunc,
        instance_size: c_uint,
        instance_init: GInstanceInitFunc,
        flags: c_uint,
    ) -> GType;
    pub(crate) fn g_type_class_peek_parent(g_class: *mut c_void) -> *mut c_void;
    pub(crate) fn g_object_new(
        object_type: GType,
        first_property_name: *const c_char,
        ...
    ) -> *mut c_void;
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

    // GDK4, GSK4 & GTK4
    pub(crate) fn gdk_memory_texture_new(
        width: c_int,
        height: c_int,
        format: c_int,
        bytes: *mut GBytes,
        stride: size_t,
    ) -> *mut GdkTexture;
    pub(crate) fn gsk_path_builder_new() -> *mut GskPathBuilder;
    pub(crate) fn gsk_path_builder_move_to(self_: *mut GskPathBuilder, x: c_float, y: c_float);
    pub(crate) fn gsk_path_builder_line_to(self_: *mut GskPathBuilder, x: c_float, y: c_float);
    pub(crate) fn gsk_path_builder_close(self_: *mut GskPathBuilder);
    pub(crate) fn gsk_path_builder_free_to_path(self_: *mut GskPathBuilder) -> *mut GskPath;
    pub(crate) fn gsk_path_unref(self_: *mut GskPath);

    pub(crate) fn gtk_init_check() -> c_int;
    pub(crate) fn gtk_widget_get_type() -> GType;
    pub(crate) fn gtk_drawing_area_get_type() -> GType;
    pub(crate) fn gtk_window_new() -> *mut GtkWidget;
    pub(crate) fn gtk_window_set_title(window: *mut GtkWindow, title: *const c_char);
    pub(crate) fn gtk_window_set_decorated(window: *mut GtkWindow, setting: c_int);
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
    pub(crate) fn gtk_widget_get_native(widget: *mut GtkWidget) -> *mut GtkNative;
    pub(crate) fn gtk_native_get_surface(self_: *mut GtkNative) -> *mut GdkSurface;
    pub(crate) fn gdk_surface_get_scale(surface: *mut GdkSurface) -> c_double;
    pub(crate) fn gtk_widget_get_width(widget: *mut GtkWidget) -> c_int;
    pub(crate) fn gtk_widget_get_height(widget: *mut GtkWidget) -> c_int;
    pub(crate) fn gtk_widget_get_frame_clock(widget: *mut GtkWidget) -> *mut GdkFrameClock;
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

    pub(crate) fn gtk_snapshot_push_clip(snapshot: *mut GtkSnapshot, bounds: *const GrapheneRect);
    pub(crate) fn gtk_snapshot_pop(snapshot: *mut GtkSnapshot);
    pub(crate) fn gtk_snapshot_save(snapshot: *mut GtkSnapshot);
    pub(crate) fn gtk_snapshot_restore(snapshot: *mut GtkSnapshot);
    pub(crate) fn gtk_snapshot_scale(
        snapshot: *mut GtkSnapshot,
        factor_x: c_float,
        factor_y: c_float,
    );
    pub(crate) fn gtk_snapshot_append_cairo(
        snapshot: *mut GtkSnapshot,
        bounds: *const GrapheneRect,
    ) -> *mut CairoT;
    pub(crate) fn gtk_snapshot_append_texture(
        snapshot: *mut GtkSnapshot,
        texture: *mut GdkTexture,
        bounds: *const GrapheneRect,
    );
    pub(crate) fn gtk_snapshot_append_color(
        snapshot: *mut GtkSnapshot,
        color: *const GdkRGBA,
        bounds: *const GrapheneRect,
    );
    pub(crate) fn gtk_snapshot_append_border(
        snapshot: *mut GtkSnapshot,
        outline: *const GskRoundedRect,
        border_width: *const c_float,
        border_color: *const GdkRGBA,
    );
    pub(crate) fn gtk_snapshot_append_fill(
        snapshot: *mut GtkSnapshot,
        path: *mut GskPath,
        fill_rule: c_int,
        color: *const GdkRGBA,
    );
    #[cfg(test)]
    pub(crate) fn gtk_snapshot_new() -> *mut GtkSnapshot;
    #[cfg(test)]
    pub(crate) fn gtk_snapshot_free_to_node(snapshot: *mut GtkSnapshot) -> *mut GskRenderNode;
    #[cfg(test)]
    pub(crate) fn gsk_render_node_draw(node: *mut GskRenderNode, cr: *mut CairoT);
    #[cfg(test)]
    pub(crate) fn gsk_render_node_unref(node: *mut GskRenderNode);

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
