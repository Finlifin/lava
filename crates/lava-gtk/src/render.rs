//! Cairo + PangoCairo 2D scene renderer, offscreen PNG export, and monospace font metrics.

use std::collections::HashMap;
use std::f64::consts::{FRAC_PI_2, PI};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use lava_proto::{CaretShape, ColorRgba, DrawCmd, FontMetrics, RenderFrame};
use libc::c_int;

use crate::ffi::{
    self, CAIRO_FORMAT_ARGB32, CAIRO_STATUS_SUCCESS, CairoSurfaceT, CairoT, PANGO_SCALE,
    PANGO_STYLE_ITALIC, PANGO_STYLE_NORMAL, PANGO_WEIGHT_BOLD, PANGO_WEIGHT_NORMAL,
    PangoFontDescription, PangoLayout, PangoRectangle,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FontKey {
    family: String,
    size_pango: c_int,
    bold: bool,
    italic: bool,
}

/// RAII scope owning a single `PangoLayout` and cached `PangoFontDescription` pointers for a frame.
struct PangoFrameScope {
    layout: *mut PangoLayout,
    descriptions: HashMap<FontKey, *mut PangoFontDescription>,
}

impl PangoFrameScope {
    fn new(cr: *mut CairoT) -> Self {
        // SAFETY: `cr` is a valid non-null Cairo context owned by the caller on the current thread;
        // `pango_cairo_create_layout` returns a newly owned `GObject` reference (or null on OOM).
        let layout = unsafe { ffi::pango_cairo_create_layout(cr) };
        Self {
            layout,
            descriptions: HashMap::new(),
        }
    }

    fn configure_layout(
        &mut self,
        family: &str,
        size_pt: f64,
        bold: bool,
        italic: bool,
    ) -> Option<*mut PangoLayout> {
        if self.layout.is_null() {
            return None;
        }

        let clamped_pt = if size_pt.is_finite() && size_pt > 0.0 {
            size_pt
        } else {
            12.0
        };
        let size_pango = (clamped_pt * f64::from(PANGO_SCALE))
            .round()
            .clamp(f64::from(PANGO_SCALE), f64::from(c_int::MAX / 2))
            as c_int;

        let normalized_family = if family.is_empty() || family.contains('\0') {
            "Monospace"
        } else {
            family
        };

        let key = FontKey {
            family: normalized_family.to_string(),
            size_pango,
            bold,
            italic,
        };

        let desc_ptr = if let Some(&cached) = self.descriptions.get(&key) {
            cached
        } else {
            let created = create_font_description(normalized_family, size_pango, bold, italic)?;
            self.descriptions.insert(key, created);
            created
        };

        // SAFETY: `self.layout` and `desc_ptr` are valid non-null Pango objects owned by `self`
        // on the current thread; Pango copies the font description internally.
        unsafe {
            ffi::pango_layout_set_font_description(self.layout, desc_ptr);
        }
        Some(self.layout)
    }
}

impl Drop for PangoFrameScope {
    fn drop(&mut self) {
        for (_, desc) in self.descriptions.drain() {
            if !desc.is_null() {
                // SAFETY: Every `desc` in `self.descriptions` was allocated by
                // `pango_font_description_new` and is uniquely owned by `self`.
                unsafe {
                    ffi::pango_font_description_free(desc);
                }
            }
        }
        if !self.layout.is_null() {
            // SAFETY: `self.layout` was returned by `pango_cairo_create_layout` with refcount 1
            // and has not been unreferenced elsewhere.
            unsafe {
                ffi::g_object_unref(self.layout.cast());
            }
        }
    }
}

/// RAII wrapper around an offscreen Cairo image surface and drawing context.
struct OffscreenSurface {
    surface: *mut CairoSurfaceT,
    cr: *mut CairoT,
}

impl OffscreenSurface {
    fn new(width: c_int, height: c_int) -> Result<Self, String> {
        let safe_w = width.max(1);
        let safe_h = height.max(1);
        // SAFETY: `cairo_image_surface_create` safely allocates an in-memory ARGB32 surface for
        // positive dimensions and returns a valid status-checked surface pointer even on failure.
        let surface =
            unsafe { ffi::cairo_image_surface_create(CAIRO_FORMAT_ARGB32, safe_w, safe_h) };
        if surface.is_null() {
            return Err("cairo_image_surface_create returned null".to_string());
        }

        // SAFETY: `surface` was verified non-null above.
        let status = unsafe { ffi::cairo_surface_status(surface) };
        if status != CAIRO_STATUS_SUCCESS {
            // SAFETY: `surface` is uniquely owned here and must be destroyed before returning.
            unsafe {
                ffi::cairo_surface_destroy(surface);
            }
            return Err(format!(
                "cairo_image_surface_create failed with status code {status}"
            ));
        }

        // SAFETY: `surface` is a valid Cairo surface owned on this thread.
        let cr = unsafe { ffi::cairo_create(surface) };
        if cr.is_null() {
            // SAFETY: `surface` is uniquely owned here and must be destroyed before returning.
            unsafe {
                ffi::cairo_surface_destroy(surface);
            }
            return Err("cairo_create returned null".to_string());
        }

        Ok(Self { surface, cr })
    }
}

impl Drop for OffscreenSurface {
    fn drop(&mut self) {
        if !self.cr.is_null() {
            // SAFETY: `self.cr` was created by `cairo_create` and is uniquely owned by `self`.
            unsafe {
                ffi::cairo_destroy(self.cr);
            }
        }
        if !self.surface.is_null() {
            // SAFETY: `self.surface` was created by `cairo_image_surface_create` and is uniquely owned.
            unsafe {
                ffi::cairo_surface_destroy(self.surface);
            }
        }
    }
}

fn create_font_description(
    family: &str,
    size_pango: c_int,
    bold: bool,
    italic: bool,
) -> Option<*mut PangoFontDescription> {
    let family_cstr = CString::new(family).ok()?;
    // SAFETY: `pango_font_description_new` takes no arguments and returns an owned heap allocation.
    let desc = unsafe { ffi::pango_font_description_new() };
    if desc.is_null() {
        return None;
    }

    let weight = if bold {
        PANGO_WEIGHT_BOLD
    } else {
        PANGO_WEIGHT_NORMAL
    };
    let style = if italic {
        PANGO_STYLE_ITALIC
    } else {
        PANGO_STYLE_NORMAL
    };

    // SAFETY: `desc` is non-null and uniquely owned; `family_cstr` is a valid NUL-terminated string
    // whose bytes Pango copies before `family_cstr` is dropped at the end of this function.
    unsafe {
        ffi::pango_font_description_set_family(desc, family_cstr.as_ptr());
        ffi::pango_font_description_set_size(desc, size_pango);
        ffi::pango_font_description_set_weight(desc, weight);
        ffi::pango_font_description_set_style(desc, style);
    }
    Some(desc)
}

fn set_source_color(cr: *mut CairoT, color: ColorRgba) {
    let (r, g, b, a) = color.components_f64();
    // SAFETY: `cr` is a valid Cairo context borrowed on the active rendering thread.
    unsafe {
        ffi::cairo_set_source_rgba(cr, r, g, b, a);
    }
}

fn append_rect_path(cr: *mut CairoT, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    // SAFETY: `cr` is a valid Cairo context borrowed on the active rendering thread, and all
    // coordinates are finite or standard IEEE-754 `f64` values handled safely by Cairo.
    unsafe {
        ffi::cairo_new_path(cr);
        if radius <= 0.0 {
            ffi::cairo_rectangle(cr, x, y, w, h);
        } else {
            let r = radius.min(w * 0.5).min(h * 0.5);
            ffi::cairo_new_sub_path(cr);
            ffi::cairo_arc(cr, x + w - r, y + r, r, -FRAC_PI_2, 0.0);
            ffi::cairo_arc(cr, x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
            ffi::cairo_arc(cr, x + r, y + h - r, r, FRAC_PI_2, PI);
            ffi::cairo_arc(cr, x + r, y + r, r, PI, PI + FRAC_PI_2);
            ffi::cairo_close_path(cr);
        }
    }
}

fn draw_caret(
    cr: *mut CairoT,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    shape: CaretShape,
    color: ColorRgba,
) {
    if !x.is_finite() || !y.is_finite() || !w.is_finite() || !h.is_finite() {
        return;
    }
    let safe_w = w.max(1.0);
    let safe_h = h.max(1.0);
    set_source_color(cr, color);

    match shape {
        CaretShape::Block => {
            append_rect_path(cr, x, y, safe_w, safe_h, 0.0);
            // SAFETY: `cr` is a valid Cairo context with an active path set above.
            unsafe {
                ffi::cairo_fill(cr);
            }
        }
        CaretShape::Beam => {
            let bar_w = safe_w.clamp(2.0, 2.5);
            append_rect_path(cr, x, y, bar_w, safe_h, 0.0);
            // SAFETY: `cr` is a valid Cairo context with an active path set above.
            unsafe {
                ffi::cairo_fill(cr);
            }
        }
        CaretShape::Underline => {
            let bar_h = (safe_h * 0.12).clamp(2.0, 3.0);
            append_rect_path(cr, x, y + safe_h - bar_h, safe_w, bar_h, 0.0);
            // SAFETY: `cr` is a valid Cairo context with an active path set above.
            unsafe {
                ffi::cairo_fill(cr);
            }
        }
        CaretShape::HollowBlock => {
            let stroke_w = 1.5;
            let inner_w = (safe_w - stroke_w).max(1.0);
            let inner_h = (safe_h - stroke_w).max(1.0);
            append_rect_path(cr, x + 0.75, y + 0.75, inner_w, inner_h, 0.0);
            // SAFETY: `cr` is a valid Cairo context with an active path set above.
            unsafe {
                ffi::cairo_set_line_width(cr, stroke_w);
                ffi::cairo_stroke(cr);
            }
        }
    }
}

/// Executes all drawing commands in `frame` onto `cr`, guaranteeing balanced Cairo state saves.
///
/// # Safety
/// `cr` must be a valid, non-null `*mut CairoT` exclusively accessed on the current thread for
/// the duration of this call.
pub(crate) unsafe fn render_frame_to_cairo(
    cr: *mut CairoT,
    viewport_width: f64,
    viewport_height: f64,
    frame: &RenderFrame,
) {
    if cr.is_null() {
        return;
    }

    let bg_w = viewport_width.max(f64::from(frame.window.width)).max(1.0);
    let bg_h = viewport_height.max(f64::from(frame.window.height)).max(1.0);

    set_source_color(cr, frame.window.background);
    append_rect_path(cr, 0.0, 0.0, bg_w, bg_h, 0.0);
    // SAFETY: `cr` is non-null and verified by caller invariant; fills the background rectangle.
    unsafe {
        ffi::cairo_fill(cr);
    }

    let mut pango_scope = PangoFrameScope::new(cr);
    let mut clip_depth: usize = 0;

    for cmd in &frame.commands {
        match cmd {
            DrawCmd::ClipPush { x, y, w, h } => {
                // SAFETY: `cr` is valid; every `cairo_save` increments `clip_depth` so all saves
                // are paired with `cairo_restore` even if `ClipPop` commands are missing.
                unsafe {
                    ffi::cairo_save(cr);
                    ffi::cairo_new_path(cr);
                    ffi::cairo_rectangle(cr, *x, *y, w.max(0.0), h.max(0.0));
                    ffi::cairo_clip(cr);
                }
                clip_depth += 1;
            }
            DrawCmd::ClipPop => {
                if clip_depth > 0 {
                    // SAFETY: `clip_depth > 0` guarantees a matching preceding `cairo_save` in this frame.
                    unsafe {
                        ffi::cairo_restore(cr);
                    }
                    clip_depth -= 1;
                }
            }
            DrawCmd::FillRect {
                x,
                y,
                w,
                h,
                color,
                radius,
            } => {
                if *w > 0.0 && *h > 0.0 {
                    set_source_color(cr, *color);
                    append_rect_path(cr, *x, *y, *w, *h, *radius);
                    // SAFETY: `cr` is valid and has the rectangle path constructed above.
                    unsafe {
                        ffi::cairo_fill(cr);
                    }
                }
            }
            DrawCmd::StrokeRect {
                x,
                y,
                w,
                h,
                color,
                line_width,
                radius,
            } => {
                if *w > 0.0 && *h > 0.0 && *line_width > 0.0 {
                    set_source_color(cr, *color);
                    append_rect_path(cr, *x, *y, *w, *h, *radius);
                    // SAFETY: `cr` is valid and has the rectangle path constructed above.
                    unsafe {
                        ffi::cairo_set_line_width(cr, *line_width);
                        ffi::cairo_stroke(cr);
                    }
                }
            }
            DrawCmd::Line {
                x1,
                y1,
                x2,
                y2,
                color,
                line_width,
            } => {
                if *line_width > 0.0 {
                    set_source_color(cr, *color);
                    // SAFETY: `cr` is valid; constructs and strokes a two-point line segment.
                    unsafe {
                        ffi::cairo_set_line_width(cr, *line_width);
                        ffi::cairo_new_path(cr);
                        ffi::cairo_move_to(cr, *x1, *y1);
                        ffi::cairo_line_to(cr, *x2, *y2);
                        ffi::cairo_stroke(cr);
                    }
                }
            }
            DrawCmd::TextRun {
                x,
                y,
                text,
                font_family,
                font_size_pt,
                color,
                bold,
                italic,
                underline,
            } => {
                if text.is_empty() {
                    continue;
                }
                let Some(layout) =
                    pango_scope.configure_layout(font_family, *font_size_pt, *bold, *italic)
                else {
                    continue;
                };
                let byte_len = c_int::try_from(text.len()).unwrap_or(c_int::MAX);
                set_source_color(cr, *color);

                // SAFETY: `layout` and `cr` are valid non-null objects on this thread, and `text`
                // points to `byte_len` valid UTF-8 bytes alive for the duration of `pango_layout_set_text`.
                unsafe {
                    ffi::pango_layout_set_text(layout, text.as_ptr().cast(), byte_len);
                    ffi::cairo_new_path(cr);
                    ffi::cairo_move_to(cr, *x, *y);
                    ffi::pango_cairo_show_layout(cr, layout);
                }

                if *underline {
                    let mut logical_px = PangoRectangle::default();
                    // SAFETY: `layout` is valid and `logical_px` is a valid stack out-pointer;
                    // `ink_rect` may be null per Pango documentation.
                    let baseline_pango = unsafe {
                        ffi::pango_layout_get_pixel_extents(
                            layout,
                            ptr::null_mut(),
                            &mut logical_px,
                        );
                        ffi::pango_layout_get_baseline(layout)
                    };
                    if logical_px.width > 0 {
                        let baseline_px = f64::from(baseline_pango) / f64::from(PANGO_SCALE);
                        let uy = (*y + baseline_px + 1.5).floor() + 0.5;
                        let ux_end = *x + f64::from(logical_px.width);
                        // SAFETY: `cr` is valid; draws a crisp 1px horizontal underline segment.
                        unsafe {
                            ffi::cairo_set_line_width(cr, 1.0);
                            ffi::cairo_new_path(cr);
                            ffi::cairo_move_to(cr, *x, uy);
                            ffi::cairo_line_to(cr, ux_end, uy);
                            ffi::cairo_stroke(cr);
                        }
                    }
                }
            }
            DrawCmd::CursorCaret {
                x,
                y,
                w,
                h,
                shape,
                color,
            } => {
                draw_caret(cr, *x, *y, *w, *h, *shape, *color);
            }
        }
    }

    while clip_depth > 0 {
        // SAFETY: Restores any unbalanced `ClipPush` saves opened during this frame.
        unsafe {
            ffi::cairo_restore(cr);
        }
        clip_depth -= 1;
    }
}

/// Measures monospace glyph `"M"` metrics in logical pixels on an offscreen PangoCairo surface.
///
/// Falls back to [`FontMetrics::default`] if measurement fails or produces non-positive dimensions.
pub fn measure_monospace_font(family: &str, size_pt: f64) -> FontMetrics {
    if !size_pt.is_finite() || size_pt <= 0.0 {
        return FontMetrics::default();
    }

    let Ok(offscreen) = OffscreenSurface::new(1, 1) else {
        return FontMetrics::default();
    };
    let mut scope = PangoFrameScope::new(offscreen.cr);
    let Some(layout) = scope.configure_layout(family, size_pt, false, false) else {
        return FontMetrics::default();
    };

    let mut logical = PangoRectangle::default();
    // SAFETY: `layout` is a valid PangoLayout owned by `scope`, `"M"` is a 1-byte static UTF-8
    // slice, and `logical` is a valid stack out-pointer.
    let baseline_pango = unsafe {
        ffi::pango_layout_set_text(layout, b"M".as_ptr().cast(), 1);
        ffi::pango_layout_get_extents(layout, ptr::null_mut(), &mut logical);
        ffi::pango_layout_get_baseline(layout)
    };

    let scale = f64::from(PANGO_SCALE);
    let cell_width = f64::from(logical.width) / scale;
    let cell_height = f64::from(logical.height) / scale;
    let ascent = f64::from(baseline_pango) / scale;
    let descent = (cell_height - ascent).max(0.0);

    if cell_width > 0.0 && cell_height > 0.0 && ascent > 0.0 {
        FontMetrics {
            cell_width,
            cell_height,
            ascent,
            descent,
        }
    } else {
        FontMetrics::default()
    }
}

/// Renders `frame` onto an offscreen ARGB32 Cairo image surface and writes a PNG file to `out_path`.
pub fn render_frame_to_png(frame: &RenderFrame, out_path: &Path) -> Result<(), String> {
    let width = frame.window.width.max(1);
    let height = frame.window.height.max(1);

    let out_cstr = CString::new(out_path.as_os_str().as_bytes())
        .map_err(|_| "output PNG path contains an interior NUL byte".to_string())?;

    let offscreen = OffscreenSurface::new(width, height)?;

    // SAFETY: `offscreen.cr` is a valid non-null Cairo context uniquely owned on this thread.
    unsafe {
        render_frame_to_cairo(offscreen.cr, f64::from(width), f64::from(height), frame);
        ffi::cairo_surface_flush(offscreen.surface);
    }

    // SAFETY: `offscreen.surface` is valid and flushed; `out_cstr` is a valid NUL-terminated path.
    let status = unsafe { ffi::cairo_surface_write_to_png(offscreen.surface, out_cstr.as_ptr()) };
    if status != CAIRO_STATUS_SUCCESS {
        return Err(format!(
            "cairo_surface_write_to_png failed for `{}` with status code {status}",
            out_path.display()
        ));
    }

    let meta = std::fs::metadata(out_path).map_err(|err| {
        format!(
            "failed to stat rendered PNG `{}`: {err}",
            out_path.display()
        )
    })?;
    if !meta.is_file() || meta.len() == 0 {
        return Err(format!(
            "rendered PNG `{}` is missing or empty",
            out_path.display()
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use lava_proto::{CaretShape, ColorRgba, DrawCmd, FontMetrics, RenderFrame, WindowConfig};

    use super::{measure_monospace_font, render_frame_to_png};

    static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(1);

    struct TempFileGuard(PathBuf);

    impl TempFileGuard {
        fn new(suffix: &str) -> Self {
            let pid = std::process::id();
            let seq = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = PathBuf::from("/tmp").join(format!("lava_gtk_{pid}_{seq}_{suffix}"));
            Self(path)
        }
    }

    impl Drop for TempFileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn offscreen_font_measurement_returns_positive_metrics_and_handles_invalid_size() {
        let m = measure_monospace_font("Monospace", 13.0);
        assert!(
            m.cell_width > 2.0,
            "unexpected cell_width: {}",
            m.cell_width
        );
        assert!(
            m.cell_height > m.cell_width,
            "expected monospace cell_height ({}) > cell_width ({})",
            m.cell_height,
            m.cell_width
        );
        assert!(m.ascent > 0.0, "unexpected ascent: {}", m.ascent);
        assert!(m.descent >= 0.0, "unexpected descent: {}", m.descent);

        let fallback = measure_monospace_font("Monospace", 0.0);
        assert_eq!(fallback, FontMetrics::default());
    }

    #[test]
    fn offscreen_render_frame_to_png_renders_all_draw_cmd_variants_and_balances_clips() {
        let guard = TempFileGuard::new("all_variants.png");

        let frame = RenderFrame {
            generation: 1,
            window: WindowConfig {
                title: "Lava Offscreen Test".to_string(),
                width: 320,
                height: 200,
                background: ColorRgba::from_rgba(24, 24, 37, 255),
                ..WindowConfig::default()
            },
            commands: vec![
                // Extra ClipPop when stack is empty must be safely ignored.
                DrawCmd::ClipPop,
                DrawCmd::ClipPush {
                    x: 8.0,
                    y: 8.0,
                    w: 304.0,
                    h: 184.0,
                },
                DrawCmd::FillRect {
                    x: 12.0,
                    y: 12.0,
                    w: 140.0,
                    h: 60.0,
                    color: ColorRgba::from_rgba(49, 50, 68, 255),
                    radius: 0.0,
                },
                DrawCmd::FillRect {
                    x: 164.0,
                    y: 12.0,
                    w: 140.0,
                    h: 60.0,
                    color: ColorRgba::from_rgba(69, 71, 90, 255),
                    radius: 10.0,
                },
                DrawCmd::StrokeRect {
                    x: 12.0,
                    y: 80.0,
                    w: 140.0,
                    h: 48.0,
                    color: ColorRgba::from_rgba(137, 180, 250, 255),
                    line_width: 2.0,
                    radius: 0.0,
                },
                DrawCmd::StrokeRect {
                    x: 164.0,
                    y: 80.0,
                    w: 140.0,
                    h: 48.0,
                    color: ColorRgba::from_rgba(166, 227, 161, 255),
                    line_width: 2.0,
                    radius: 8.0,
                },
                DrawCmd::Line {
                    x1: 12.0,
                    y1: 136.0,
                    x2: 304.0,
                    y2: 136.0,
                    color: ColorRgba::from_rgba(249, 226, 175, 255),
                    line_width: 1.5,
                },
                DrawCmd::TextRun {
                    x: 16.0,
                    y: 18.0,
                    text: "fn main() -> λ 中文".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 12.0,
                    color: ColorRgba::from_rgba(205, 214, 244, 255),
                    bold: true,
                    italic: true,
                    underline: true,
                },
                DrawCmd::CursorCaret {
                    x: 20.0,
                    y: 150.0,
                    w: 9.0,
                    h: 20.0,
                    shape: CaretShape::Block,
                    color: ColorRgba::from_rgba(245, 224, 220, 192),
                },
                DrawCmd::CursorCaret {
                    x: 50.0,
                    y: 150.0,
                    w: 9.0,
                    h: 20.0,
                    shape: CaretShape::Beam,
                    color: ColorRgba::from_rgba(245, 224, 220, 255),
                },
                DrawCmd::CursorCaret {
                    x: 80.0,
                    y: 150.0,
                    w: 9.0,
                    h: 20.0,
                    shape: CaretShape::Underline,
                    color: ColorRgba::from_rgba(245, 224, 220, 255),
                },
                DrawCmd::CursorCaret {
                    x: 110.0,
                    y: 150.0,
                    w: 9.0,
                    h: 20.0,
                    shape: CaretShape::HollowBlock,
                    color: ColorRgba::from_rgba(245, 224, 220, 255),
                },
                // Intentionally leave nested ClipPush unpopped to verify automatic frame-end cleanup.
                DrawCmd::ClipPush {
                    x: 16.0,
                    y: 145.0,
                    w: 200.0,
                    h: 40.0,
                },
            ],
        };

        render_frame_to_png(&frame, &guard.0).expect("PNG export should succeed");
        let bytes = std::fs::read(&guard.0).expect("PNG file should be readable");
        assert!(
            bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "missing PNG signature"
        );
        // Verify IHDR chunk dimensions (bytes 16..24 big-endian width & height).
        assert!(
            bytes.len() > 1024,
            "expected non-trivial compressed PNG size, got {}",
            bytes.len()
        );
        let png_w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let png_h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        assert_eq!(png_w, 320);
        assert_eq!(png_h, 200);
    }
}
