//! Neovide-style 4-corner cursor smear physics, smooth viewport split-clip scrolling,
//! GPU-cached `GdkMemoryTexture` + `GtkSnapshot` scene composition, and touchpad scroll accumulation.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use lava_proto::{
    AnimHints, CaretShape, ColorRgba, DrawCmd, RenderFrame, ScrollViewport, TransferOverlay,
};
use libc::{c_float, c_int};

use crate::ffi::{
    self, CairoSurfaceT, CairoT, GSK_FILL_RULE_WINDING, GdkRGBA, GdkTexture, GrapheneRect,
    GskRoundedRect, GtkSnapshot,
};
use crate::render::{
    GdkTextureHandle, OffscreenSurface, draw_caret, query_cairo_device_scale,
    render_commands_to_cairo, render_frame_to_cairo_with_opts,
    render_offscreen_incremental_with_opts, set_source_surface_nearest,
};

/// Leading-corner exponential decay time constant in seconds (~14 ms).
pub(crate) const TAU_CURSOR_LEAD: f64 = 0.014;
/// Trailing-corner exponential decay time constant for jumps > 2 cells (~44 ms).
pub(crate) const TAU_CURSOR_TRAIL: f64 = 0.044;
/// Crisp unified glide time constant for <= 2-cell adjacent typing/motion (~12 ms, zero smear tail).
pub(crate) const TAU_CURSOR_SHORT: f64 = 0.012;
/// Smooth viewport vertical scroll decay time constant in seconds (~42 ms).
pub(crate) const TAU_SCROLL_DECAY: f64 = 0.042;
/// Sub-pixel convergence threshold below which animations snap to target and stop VSync wakeups.
pub(crate) const SNAP_EPSILON_PX: f64 = 0.55;

/// Clockwise corner unit signs `[TL, TR, BR, BL]` around a cursor center.
const CORNER_SIGNS: [(f64, f64); 4] = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];

/// Target geometry and style of the primary active cursor caret in a [`RenderFrame`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CaretTarget {
    pub(crate) cmd_idx: usize,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) w: f64,
    pub(crate) h: f64,
    pub(crate) shape: CaretShape,
    pub(crate) color: ColorRgba,
}

impl CaretTarget {
    /// Computes the rendered rectangle `(x, y, w, h)` matching `render::draw_caret`.
    pub(crate) fn visual_rect(&self) -> (f64, f64, f64, f64) {
        let safe_w = self.w.max(1.0);
        let safe_h = self.h.max(1.0);
        match self.shape {
            CaretShape::Block | CaretShape::HollowBlock => (self.x, self.y, safe_w, safe_h),
            CaretShape::Beam => (self.x, self.y, safe_w.clamp(2.0, 2.5), safe_h),
            CaretShape::Underline => {
                let bar_h = (safe_h * 0.12).clamp(2.0, 3.0);
                (self.x, self.y + safe_h - bar_h, safe_w, bar_h)
            }
        }
    }

    /// Returns clockwise target corners `[TL, TR, BR, BL]` in logical pixels.
    pub(crate) fn target_corners(&self) -> [(f64, f64); 4] {
        let (rx, ry, rw, rh) = self.visual_rect();
        [(rx, ry), (rx + rw, ry), (rx + rw, ry + rh), (rx, ry + rh)]
    }
}

/// Locates the primary active caret (the last non-`HollowBlock` `DrawCmd::CursorCaret`).
pub(crate) fn find_primary_caret(frame: &RenderFrame) -> Option<CaretTarget> {
    for (cmd_idx, cmd) in frame.commands.iter().enumerate().rev() {
        if let DrawCmd::CursorCaret {
            x,
            y,
            w,
            h,
            shape,
            color,
        } = cmd
            && *shape != CaretShape::HollowBlock
            && x.is_finite()
            && y.is_finite()
            && w.is_finite()
            && h.is_finite()
        {
            return Some(CaretTarget {
                cmd_idx,
                x: *x,
                y: *y,
                w: *w,
                h: *h,
                shape: *shape,
                color: *color,
            });
        }
    }
    None
}

/// 4-corner independent exponential-spring cursor physics (`[TL, TR, BR, BL]`) modeled on Neovide.
#[derive(Debug, Clone)]
pub(crate) struct CursorPhysics {
    corners: [(f64, f64); 4],
    head_offset: (f64, f64),
    target: Option<CaretTarget>,
    move_dir: (f64, f64),
    short_typing_hop: bool,
    settled: bool,
}

impl Default for CursorPhysics {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorPhysics {
    pub(crate) fn new() -> Self {
        Self {
            corners: [(0.0, 0.0); 4],
            head_offset: (0.0, 0.0),
            target: None,
            move_dir: (1.0, 0.0),
            short_typing_hop: true,
            settled: true,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.settled && self.target.is_some()
    }

    #[cfg(test)]
    pub(crate) fn corners(&self) -> [(f64, f64); 4] {
        self.corners
    }

    fn current_center(&self) -> (f64, f64) {
        let sx = self.corners[0].0 + self.corners[1].0 + self.corners[2].0 + self.corners[3].0;
        let sy = self.corners[0].1 + self.corners[1].1 + self.corners[2].1 + self.corners[3].1;
        (sx * 0.25, sy * 0.25)
    }

    /// Updates the target caret geometry and triggers a smear transition if it moved.
    pub(crate) fn set_target(&mut self, next: Option<CaretTarget>, enabled: bool) {
        let Some(caret) = next else {
            self.target = None;
            self.head_offset = (0.0, 0.0);
            self.settled = true;
            return;
        };

        let next_corners = caret.target_corners();
        let Some(prev) = self.target else {
            self.corners = next_corners;
            self.head_offset = (0.0, 0.0);
            self.target = Some(caret);
            self.settled = true;
            return;
        };

        if !enabled {
            self.corners = next_corners;
            self.head_offset = (0.0, 0.0);
            self.target = Some(caret);
            self.settled = true;
            return;
        }

        let (nrx, nry, nrw, nrh) = caret.visual_rect();
        let (prx, pry, prw, prh) = prev.visual_rect();
        let next_center = (nrx + nrw * 0.5, nry + nrh * 0.5);
        let prev_center = (prx + prw * 0.5, pry + prh * 0.5);

        let target_delta = (next_center.0 - prev_center.0).hypot(next_center.1 - prev_center.1);
        let size_changed =
            (nrw - prw).abs() > SNAP_EPSILON_PX || (nrh - prh).abs() > SNAP_EPSILON_PX;
        if target_delta <= SNAP_EPSILON_PX && !size_changed {
            self.target = Some(caret);
            return;
        }

        let cur_center = self.current_center();
        let dx = next_center.0 - cur_center.0;
        let dy = next_center.1 - cur_center.1;
        let dist = dx.hypot(dy);
        if dist < SNAP_EPSILON_PX {
            self.corners = next_corners;
            self.head_offset = (0.0, 0.0);
            self.target = Some(caret);
            self.settled = true;
            return;
        }

        self.move_dir = (dx / dist, dy / dist);
        let step_dx = (next_center.0 - prev_center.0).abs();
        let step_dy = (next_center.1 - prev_center.1).abs();
        let cell_ref_w = caret.w.max(prev.w).max(6.0);
        self.short_typing_hop = step_dy < 1.0 && step_dx <= 2.25 * cell_ref_w;
        self.head_offset = (
            prev.x + self.head_offset.0 - caret.x,
            prev.y + self.head_offset.1 - caret.y,
        );
        self.target = Some(caret);
        self.settled = false;
    }

    /// Advances all 4 corners by `dt_sec` seconds and returns `true` while still unsettled.
    pub(crate) fn step(&mut self, dt_sec: f64) -> bool {
        if self.settled {
            return false;
        }
        let Some(caret) = self.target else {
            self.settled = true;
            return false;
        };

        let dt = if dt_sec.is_finite() && dt_sec >= 0.0 {
            dt_sec.min(0.1)
        } else {
            1.0 / 60.0
        };
        let target_corners = caret.target_corners();
        let lead_decay = if self.short_typing_hop {
            (-dt / TAU_CURSOR_SHORT).exp()
        } else {
            (-dt / TAU_CURSOR_LEAD).exp()
        };
        let trail_decay = if self.short_typing_hop {
            // Short 1-2 cell typing/motion moves all 4 corners uniformly without stretching a
            // translucent tail across neighboring characters, eliminating typing double-vision.
            lead_decay
        } else {
            (-dt / TAU_CURSOR_TRAIL).exp()
        };

        let mut max_err = 0.0_f64;
        for i in 0..4 {
            let alignment =
                CORNER_SIGNS[i].0 * self.move_dir.0 + CORNER_SIGNS[i].1 * self.move_dir.1;
            let decay = if alignment > 0.0 {
                lead_decay
            } else {
                trail_decay
            };
            self.corners[i].0 =
                target_corners[i].0 + (self.corners[i].0 - target_corners[i].0) * decay;
            self.corners[i].1 =
                target_corners[i].1 + (self.corners[i].1 - target_corners[i].1) * decay;
            let err = (self.corners[i].0 - target_corners[i].0)
                .hypot(self.corners[i].1 - target_corners[i].1);
            max_err = max_err.max(err);
        }

        self.head_offset.0 *= lead_decay;
        self.head_offset.1 *= lead_decay;
        max_err = max_err.max(self.head_offset.0.hypot(self.head_offset.1));

        if max_err < SNAP_EPSILON_PX {
            self.corners = target_corners;
            self.head_offset = (0.0, 0.0);
            self.settled = true;
            false
        } else {
            true
        }
    }

    /// Renders a single unified cursor shape onto `cr` (never double-drawing a smear polygon and
    /// a second overlapping caret rectangle on top of each other, which previously caused alpha
    /// compounding / ghosting artifacts).
    ///
    /// # Safety
    /// `cr` must be a valid non-null `*mut CairoT` owned on the current thread.
    pub(crate) unsafe fn draw(&self, cr: *mut CairoT) {
        let Some(caret) = self.target else {
            return;
        };
        if self.settled || caret.shape == CaretShape::HollowBlock {
            let gx = (caret.x + self.head_offset.0).round();
            let gy = (caret.y + self.head_offset.1).round();
            draw_caret(cr, gx, gy, caret.w, caret.h, caret.shape, caret.color);
            return;
        }

        let (_, _, target_w, target_h) = caret.visual_rect();
        let poly_w = (self.corners[1].0 - self.corners[0].0)
            .abs()
            .max((self.corners[2].0 - self.corners[3].0).abs());
        let poly_h = (self.corners[3].1 - self.corners[0].1)
            .abs()
            .max((self.corners[2].1 - self.corners[1].1).abs());
        let stretched = (poly_w - target_w).abs() > 0.85 || (poly_h - target_h).abs() > 0.85;

        if self.short_typing_hop || !stretched {
            let gx = (caret.x + self.head_offset.0).round();
            let gy = (caret.y + self.head_offset.1).round();
            draw_caret(cr, gx, gy, caret.w, caret.h, caret.shape, caret.color);
            return;
        }

        // Single unified 4-corner quad at exact `caret.color` — zero overlapping second layer!
        let (r, g, b, a) = caret.color.components_f64();
        // SAFETY: `cr` is valid per caller invariant; builds and fills one closed 4-corner polygon.
        unsafe {
            ffi::cairo_set_source_rgba(cr, r, g, b, a);
            ffi::cairo_new_path(cr);
            ffi::cairo_move_to(cr, self.corners[0].0, self.corners[0].1);
            ffi::cairo_line_to(cr, self.corners[1].0, self.corners[1].1);
            ffi::cairo_line_to(cr, self.corners[2].0, self.corners[2].1);
            ffi::cairo_line_to(cr, self.corners[3].0, self.corners[3].1);
            ffi::cairo_close_path(cr);
            ffi::cairo_fill(cr);
        }
    }

    /// Appends the active cursor caret or 4-corner smear polygon directly onto a `GtkSnapshot`
    /// using native GPU nodes (`GskColorNode` / `GskBorderNode` / `GskFillNode`) with zero Cairo.
    ///
    /// # Safety
    /// `snapshot` must be a valid non-null `*mut GtkSnapshot` on the GTK main thread.
    pub(crate) unsafe fn snapshot_draw(&self, snapshot: *mut GtkSnapshot) {
        let Some(caret) = self.target else {
            return;
        };
        let rgba = color_to_gdk_rgba(caret.color);

        let draw_rect_caret = |gx: f64, gy: f64| {
            let safe_w = caret.w.max(1.0);
            let safe_h = caret.h.max(1.0);
            match caret.shape {
                CaretShape::Block => {
                    let rect =
                        GrapheneRect::new(gx as f32, gy as f32, safe_w as f32, safe_h as f32);
                    // SAFETY: `snapshot`, `rgba`, and `rect` are valid stack/GTK pointers.
                    unsafe {
                        ffi::gtk_snapshot_append_color(snapshot, &rgba, &rect);
                    }
                }
                CaretShape::Beam => {
                    let bar_w = safe_w.clamp(2.0, 2.5);
                    let rect = GrapheneRect::new(gx as f32, gy as f32, bar_w as f32, safe_h as f32);
                    // SAFETY: `snapshot`, `rgba`, and `rect` are valid.
                    unsafe {
                        ffi::gtk_snapshot_append_color(snapshot, &rgba, &rect);
                    }
                }
                CaretShape::Underline => {
                    let bar_h = (safe_h * 0.12).clamp(2.0, 3.0);
                    let rect = GrapheneRect::new(
                        gx as f32,
                        (gy + safe_h - bar_h) as f32,
                        safe_w as f32,
                        bar_h as f32,
                    );
                    // SAFETY: `snapshot`, `rgba`, and `rect` are valid.
                    unsafe {
                        ffi::gtk_snapshot_append_color(snapshot, &rgba, &rect);
                    }
                }
                CaretShape::HollowBlock => {
                    let outline = GskRoundedRect::from_rect(GrapheneRect::new(
                        gx as f32,
                        gy as f32,
                        safe_w as f32,
                        safe_h as f32,
                    ));
                    let widths: [c_float; 4] = [1.5, 1.5, 1.5, 1.5];
                    let colors: [GdkRGBA; 4] = [rgba, rgba, rgba, rgba];
                    // SAFETY: `outline`, `widths[4]`, and `colors[4]` match GTK4's C ABI.
                    unsafe {
                        ffi::gtk_snapshot_append_border(
                            snapshot,
                            &outline,
                            widths.as_ptr(),
                            colors.as_ptr(),
                        );
                    }
                }
            }
        };

        if self.settled || caret.shape == CaretShape::HollowBlock {
            draw_rect_caret(
                (caret.x + self.head_offset.0).round(),
                (caret.y + self.head_offset.1).round(),
            );
            return;
        }

        let (_, _, target_w, target_h) = caret.visual_rect();
        let poly_w = (self.corners[1].0 - self.corners[0].0)
            .abs()
            .max((self.corners[2].0 - self.corners[3].0).abs());
        let poly_h = (self.corners[3].1 - self.corners[0].1)
            .abs()
            .max((self.corners[2].1 - self.corners[1].1).abs());
        let stretched = (poly_w - target_w).abs() > 0.85 || (poly_h - target_h).abs() > 0.85;

        if self.short_typing_hop || !stretched {
            draw_rect_caret(
                (caret.x + self.head_offset.0).round(),
                (caret.y + self.head_offset.1).round(),
            );
            return;
        }

        // SAFETY: Builds a 4-corner `GskPath` and emits one `GskFillNode` onto `snapshot`.
        unsafe {
            let builder = ffi::gsk_path_builder_new();
            if !builder.is_null() {
                ffi::gsk_path_builder_move_to(
                    builder,
                    self.corners[0].0 as c_float,
                    self.corners[0].1 as c_float,
                );
                ffi::gsk_path_builder_line_to(
                    builder,
                    self.corners[1].0 as c_float,
                    self.corners[1].1 as c_float,
                );
                ffi::gsk_path_builder_line_to(
                    builder,
                    self.corners[2].0 as c_float,
                    self.corners[2].1 as c_float,
                );
                ffi::gsk_path_builder_line_to(
                    builder,
                    self.corners[3].0 as c_float,
                    self.corners[3].1 as c_float,
                );
                ffi::gsk_path_builder_close(builder);
                let path = ffi::gsk_path_builder_free_to_path(builder);
                if !path.is_null() {
                    ffi::gtk_snapshot_append_fill(snapshot, path, GSK_FILL_RULE_WINDING, &rgba);
                    ffi::gsk_path_unref(path);
                }
            }
        }
    }
}

#[inline]
fn color_to_gdk_rgba(c: ColorRgba) -> GdkRGBA {
    let (r, g, b, a) = c.components_f64();
    GdkRGBA {
        red: r as c_float,
        green: g as c_float,
        blue: b as c_float,
        alpha: a as c_float,
    }
}

/// Immutable rendered base-frame layer pairing a CPU `OffscreenSurface` with a lazily uploaded,
/// GSK-cached `GdkMemoryTexture`.
struct SceneLayer {
    surface: OffscreenSurface,
    gpu_texture: RefCell<Option<GdkTextureHandle>>,
}

impl SceneLayer {
    fn new(surface: OffscreenSurface) -> Self {
        Self {
            surface,
            gpu_texture: RefCell::new(None),
        }
    }

    fn ensure_gpu_texture(&self) -> Option<*mut GdkTexture> {
        let mut slot = self.gpu_texture.borrow_mut();
        if let Some(ref handle) = *slot {
            return Some(handle.0);
        }
        let created = self.surface.create_gdk_texture()?;
        let ptr = created.0;
        *slot = Some(created);
        Some(ptr)
    }
}

const MAX_SCROLL_HISTORY_SLICES: usize = 4;

/// Single zero-copy historical layer slice participating in a multi-step smooth scroll transition.
struct ScrollHistorySlice {
    /// Cumulative signed pixel lag behind the newest `base_layer` (`> 0` when scrolling down,
    /// `< 0` when scrolling up).
    lag_px: f64,
    /// Signed step span contributed by this transition (`0 < |step_px| <= vp.h`).
    step_px: f64,
    layer: Rc<SceneLayer>,
}

struct PaneScrollState {
    vp: ScrollViewport,
    offset_y_px: f64,
    history: Vec<ScrollHistorySlice>,
}

impl PaneScrollState {
    fn is_active(&self) -> bool {
        !self.history.is_empty() && self.offset_y_px.abs() >= SNAP_EPSILON_PX
    }

    fn matches_geometry(&self, next: &ScrollViewport) -> bool {
        self.vp.doc_id == next.doc_id
            && (self.vp.x - next.x).abs() < 0.5
            && (self.vp.y - next.y).abs() < 0.5
            && (self.vp.w - next.w).abs() < 0.5
            && (self.vp.h - next.h).abs() < 0.5
    }

    fn clear_into_pool(&mut self, pool: &mut Vec<OffscreenSurface>) {
        self.offset_y_px = 0.0;
        for slice in self.history.drain(..) {
            AnimController::recycle_rc_layer(pool, slice.layer);
        }
    }

    fn prune_invisible_history(&mut self, pool: &mut Vec<OffscreenSurface>) {
        let abs_off = self.offset_y_px.abs();
        if abs_off < SNAP_EPSILON_PX {
            self.clear_into_pool(pool);
            return;
        }
        // `history` is ordered newest-first (`lag_px` increasing in magnitude). Once an older
        // slice's inner boundary (`prev_lag_abs`) reaches `>= abs_off`, it and all older slices
        // have scrolled completely out of the pane viewport and can be recycled immediately!
        let mut keep_len = 0usize;
        for slice in &self.history {
            let inner_edge = (slice.lag_px - slice.step_px).abs();
            if inner_edge < abs_off - 0.05 && keep_len < MAX_SCROLL_HISTORY_SLICES {
                keep_len += 1;
            } else {
                break;
            }
        }
        while self.history.len() > keep_len {
            if let Some(evicted) = self.history.pop() {
                AnimController::recycle_rc_layer(pool, evicted.layer);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ClipRect {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) w: f64,
    pub(crate) h: f64,
}

/// Subtracts `hole` from every overlapping rectangle in `rects`, partitioning the remaining region
/// into disjoint axis-aligned rectangles so static chrome + scrolling panes cover `1.0x` screen
/// area with zero triple-overdraw.
pub(crate) fn subtract_rect_from_list(rects: &mut Vec<ClipRect>, hole: ClipRect) {
    if hole.w <= 0.0 || hole.h <= 0.0 {
        return;
    }
    let hx0 = hole.x;
    let hy0 = hole.y;
    let hx1 = hole.x + hole.w;
    let hy1 = hole.y + hole.h;

    let mut next = Vec::with_capacity(rects.len() + 3);
    for r in rects.drain(..) {
        let rx0 = r.x;
        let ry0 = r.y;
        let rx1 = r.x + r.w;
        let ry1 = r.y + r.h;

        let ix0 = rx0.max(hx0);
        let iy0 = ry0.max(hy0);
        let ix1 = rx1.min(hx1);
        let iy1 = ry1.min(hy1);

        if ix1 <= ix0 + 1e-5 || iy1 <= iy0 + 1e-5 {
            next.push(r);
            continue;
        }
        // Top band `[rx0..rx1, ry0..iy0]`
        if iy0 > ry0 + 1e-5 {
            next.push(ClipRect {
                x: rx0,
                y: ry0,
                w: rx1 - rx0,
                h: iy0 - ry0,
            });
        }
        // Bottom band `[rx0..rx1, iy1..ry1]`
        if ry1 > iy1 + 1e-5 {
            next.push(ClipRect {
                x: rx0,
                y: iy1,
                w: rx1 - rx0,
                h: ry1 - iy1,
            });
        }
        // Left middle band `[rx0..ix0, iy0..iy1]`
        if ix0 > rx0 + 1e-5 {
            next.push(ClipRect {
                x: rx0,
                y: iy0,
                w: ix0 - rx0,
                h: iy1 - iy0,
            });
        }
        // Right middle band `[ix1..rx1, iy0..iy1]`
        if rx1 > ix1 + 1e-5 {
            next.push(ClipRect {
                x: ix1,
                y: iy0,
                w: rx1 - ix1,
                h: iy1 - iy0,
            });
        }
    }
    *rects = next;
}

/// Blits `src_surface` shifted vertically by `shift_y` inside the device-space `clip` rectangle,
/// snapping both the clip and vertical shift to device pixels with `CAIRO_OPERATOR_SOURCE` and
/// explicit `CAIRO_FILTER_NEAREST` so Cairo hits `pixman_blt` instead of `CAIRO_FILTER_GOOD` bilinear.
///
/// # Safety
/// `cr` and `src_surface` must be valid non-null Cairo pointers on the current thread.
unsafe fn blit_clipped_shifted(
    cr: *mut CairoT,
    src_surface: *mut CairoSurfaceT,
    clip: ClipRect,
    shift_y: f64,
    scale_x: f64,
    scale_y: f64,
) {
    if clip.w <= 0.0 || clip.h <= 0.0 {
        return;
    }
    let sx = if scale_x.is_finite() && scale_x > 0.25 {
        scale_x
    } else {
        1.0
    };
    let sy = if scale_y.is_finite() && scale_y > 0.25 {
        scale_y
    } else {
        1.0
    };
    let px0 = (clip.x * sx).floor();
    let py0 = (clip.y * sy).floor();
    let px1 = ((clip.x + clip.w) * sx).ceil();
    let py1 = ((clip.y + clip.h) * sy).ceil();
    if px1 <= px0 || py1 <= py0 {
        return;
    }
    let pshift_y = (shift_y * sy).round();

    // SAFETY: Saves Cairo state, resets to 1:1 physical pixel space, clips on integer physical
    // boundaries, sets `CAIRO_OPERATOR_SOURCE` + `CAIRO_FILTER_NEAREST`, paints, and restores.
    unsafe {
        let mut tsx = 1.0_f64;
        let mut tsy = 1.0_f64;
        let target = ffi::cairo_get_target(cr);
        if !target.is_null() {
            ffi::cairo_surface_get_device_scale(target, &mut tsx, &mut tsy);
        }
        ffi::cairo_save(cr);
        ffi::cairo_identity_matrix(cr);
        if (tsx - 1.0).abs() > 1e-6 || (tsy - 1.0).abs() > 1e-6 {
            ffi::cairo_scale(cr, 1.0 / tsx.max(0.25), 1.0 / tsy.max(0.25));
        }
        ffi::cairo_new_path(cr);
        ffi::cairo_rectangle(cr, px0, py0, px1 - px0, py1 - py0);
        ffi::cairo_clip(cr);
        ffi::cairo_set_operator(cr, ffi::CAIRO_OPERATOR_SOURCE);
        set_source_surface_nearest(cr, src_surface, 0.0, pshift_y);
        ffi::cairo_paint(cr);
        ffi::cairo_restore(cr);
    }
}

/// Appends `texture` (`(phys_w, phys_h)` physical texels) shifted vertically by `shift_y` inside
/// `clip` onto `snapshot` when `snapshot` has already been scaled to 1:1 physical device pixel
/// coordinates (`gtk_snapshot_scale(snapshot, 1.0 / scale, 1.0 / scale)`).
///
/// # Safety
/// `snapshot` and `texture` must be valid GTK4/GDK4 pointers on the GTK main thread.
#[inline]
unsafe fn snapshot_clipped_shifted_texture_phys(
    snapshot: *mut GtkSnapshot,
    texture: *mut GdkTexture,
    (phys_w, phys_h): (c_int, c_int),
    clip: ClipRect,
    shift_y: f64,
    scale: f64,
) {
    if clip.w <= 0.0 || clip.h <= 0.0 || phys_w <= 0 || phys_h <= 0 {
        return;
    }
    let px0 = (clip.x * scale).floor();
    let py0 = (clip.y * scale).floor();
    let px1 = ((clip.x + clip.w) * scale).ceil();
    let py1 = ((clip.y + clip.h) * scale).ceil();
    if px1 <= px0 || py1 <= py0 {
        return;
    }
    let pshift_y = (shift_y * scale).round();
    let clip_grect = GrapheneRect::new(
        px0 as f32,
        py0 as f32,
        (px1 - px0) as f32,
        (py1 - py0) as f32,
    );
    let tex_bounds = GrapheneRect::new(0.0, pshift_y as f32, phys_w as f32, phys_h as f32);
    // SAFETY: `snapshot` and `texture` are valid on the GTK main thread; `push_clip` and `pop` are balanced.
    unsafe {
        ffi::gtk_snapshot_push_clip(snapshot, &clip_grect);
        ffi::gtk_snapshot_append_texture(snapshot, texture, &tex_bounds);
        ffi::gtk_snapshot_pop(snapshot);
    }
}

/// Coordinates GPU-cached `SceneLayer` swapchains, zero-copy multi-slice smooth scrolling, and
/// 4-corner cursor animation.
pub(crate) struct AnimController {
    base_layer: Option<Rc<SceneLayer>>,
    surface_pool: Vec<OffscreenSurface>,
    cached_gen: u64,
    cached_skip_caret: Option<usize>,
    cursor: CursorPhysics,
    panes: HashMap<i64, PaneScrollState>,
    last_tick_sec: Option<f64>,
    nominal_dt_sec: f64,
    needs_snap_redraw: bool,
}

impl Default for AnimController {
    fn default() -> Self {
        Self::new()
    }
}

/// Target parameters for recording a single GTK4 `GtkWidgetClass.snapshot` frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SnapshotTarget {
    pub(crate) snapshot: *mut GtkSnapshot,
    pub(crate) width: c_int,
    pub(crate) height: c_int,
    pub(crate) scale: f64,
    pub(crate) frame_clock_sec: Option<f64>,
}

impl AnimController {
    pub(crate) fn new() -> Self {
        Self {
            base_layer: None,
            surface_pool: Vec::new(),
            cached_gen: 0,
            cached_skip_caret: None,
            cursor: CursorPhysics::new(),
            panes: HashMap::new(),
            last_tick_sec: None,
            nominal_dt_sec: 1.0 / 60.0,
            needs_snap_redraw: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn base_surface(&self) -> Option<&OffscreenSurface> {
        self.base_layer.as_deref().map(|l| &l.surface)
    }

    fn take_or_alloc_surface(
        &mut self,
        safe_w: c_int,
        safe_h: c_int,
        scale_x: f64,
        scale_y: f64,
    ) -> Result<OffscreenSurface, String> {
        if let Some(pos) = self.surface_pool.iter().position(|s| {
            s.width == safe_w
                && s.height == safe_h
                && (s.device_scale_x - scale_x).abs() <= 1e-4
                && (s.device_scale_y - scale_y).abs() <= 1e-4
                && s.is_uniquely_owned()
        }) {
            return Ok(self.surface_pool.swap_remove(pos));
        }
        // Drop any stale pool entries whose dimensions changed or exceed the 4-buffer swapchain cap.
        self.surface_pool.retain(|s| {
            s.width == safe_w
                && s.height == safe_h
                && (s.device_scale_x - scale_x).abs() <= 1e-4
                && (s.device_scale_y - scale_y).abs() <= 1e-4
        });
        while self.surface_pool.len() >= 4 {
            let _ = self.surface_pool.remove(0);
        }
        OffscreenSurface::new_scaled(safe_w, safe_h, scale_x, scale_y)
    }

    fn recycle_rc_layer(pool: &mut Vec<OffscreenSurface>, layer: Rc<SceneLayer>) {
        if let Ok(owned) = Rc::try_unwrap(layer) {
            // Release our `GdkTexture` reference first so if GSK is no longer holding `GBytes`,
            // `owned.surface.is_uniquely_owned()` immediately drops back to `true` (`refcount <= 2`).
            drop(owned.gpu_texture);
            if pool.len() < 4 {
                pool.push(owned.surface);
            }
        }
    }

    /// Returns `true` while cursor physics or any pane scroll animation is unsettled.
    pub(crate) fn is_active(&self) -> bool {
        self.cursor.is_active() || self.panes.values().any(PaneScrollState::is_active)
    }

    #[cfg(test)]
    pub(crate) fn pane_offset_y(&self, pane_id: i64) -> f64 {
        self.panes.get(&pane_id).map_or(0.0, |st| st.offset_y_px)
    }

    /// Seeds `last_tick_sec` from the widget's `GdkFrameClock` when an animation begins so the
    /// first VSync tick measures real elapsed frame time rather than a hardcoded `1/60s` jump.
    pub(crate) fn seed_frame_clock(&mut self, frame_time_sec: Option<f64>) {
        if self.is_active()
            && self.last_tick_sec.is_none()
            && let Some(t) = frame_time_sec
            && t.is_finite()
            && t > 0.0
        {
            self.last_tick_sec = Some(t);
        }
    }

    /// Advances all active animations to `now_sec`.
    ///
    /// Returns `(still_active, should_queue_draw)`. `should_queue_draw` is `true` while an
    /// animation is moving AND once more on the exact frame it snaps to rest, then `false` (0% CPU).
    pub(crate) fn tick_at(&mut self, now_sec: f64) -> (bool, bool) {
        if !self.is_active() {
            self.last_tick_sec = None;
            let snap = self.needs_snap_redraw;
            self.needs_snap_redraw = false;
            return (false, snap);
        }

        let dt = match self.last_tick_sec {
            Some(prev) if now_sec.is_finite() && now_sec >= prev => {
                let raw_dt = now_sec - prev;
                if raw_dt < 1e-4 {
                    // Duplicate tick within the same VSync interval: avoid redundant redraws.
                    return (true, false);
                }
                if (0.002..=0.025).contains(&raw_dt) {
                    self.nominal_dt_sec = self.nominal_dt_sec * 0.85 + raw_dt * 0.15;
                }
                let nom = self.nominal_dt_sec.clamp(1.0 / 240.0, 1.0 / 50.0);
                if raw_dt <= nom * 1.15 {
                    raw_dt
                } else {
                    // Smoothly compress dropped-frame `dt` spikes asymptotically toward `1.85 * nom`
                    // instead of hard-clamping at `0.05s`, preventing a sudden 70% single-frame lurch.
                    let excess = raw_dt - nom * 1.15;
                    nom * 1.15 + nom * 0.70 * (1.0 - (-excess / 0.045).exp())
                }
            }
            _ => self.nominal_dt_sec,
        };
        if now_sec.is_finite() {
            self.last_tick_sec = Some(now_sec);
        }

        self.step_dt(dt);
        let active_after = self.is_active();
        if !active_after {
            self.last_tick_sec = None;
            self.needs_snap_redraw = false;
        }
        // Queue draw for all in-flight steps AND the single final snapped step.
        (active_after, true)
    }

    /// Steps animations directly by `dt_sec` (used by VSync ticks and deterministic unit tests).
    pub(crate) fn step_dt(&mut self, dt_sec: f64) {
        let dt = if dt_sec.is_finite() && dt_sec >= 0.0 {
            dt_sec.min(0.1)
        } else {
            1.0 / 60.0
        };
        let _ = self.cursor.step(dt);

        let scroll_decay = (-dt / TAU_SCROLL_DECAY).exp();
        for pane in self.panes.values_mut() {
            if !pane.history.is_empty() {
                pane.offset_y_px *= scroll_decay;
                pane.prune_invisible_history(&mut self.surface_pool);
            }
        }
    }

    fn emit_pane_scroll_slices<F>(pane: &PaneScrollState, base_layer: &SceneLayer, mut emit: F)
    where
        F: FnMut(&SceneLayer, ClipRect, f64),
    {
        let offset_y = pane.offset_y_px;
        let vp = &pane.vp;
        if offset_y.abs() < SNAP_EPSILON_PX || vp.w <= 0.0 || vp.h <= 0.0 || pane.history.is_empty()
        {
            return;
        }

        if offset_y > 0.0 {
            // Scrolling down (`new_top > old_top`): content glides upward.
            // Bottom `[vp.y + split, vp.y + vp.h]` draws `base_layer` shifted by `+offset_y`.
            // Top `[vp.y, vp.y + split]` is partitioned across `pane.history` without overlap!
            let split = offset_y.clamp(0.0, vp.h);
            let bottom_h = (vp.h - split).max(0.0);
            if bottom_h > 0.0 {
                emit(
                    base_layer,
                    ClipRect {
                        x: vp.x,
                        y: vp.y + split,
                        w: vp.w,
                        h: bottom_h,
                    },
                    offset_y,
                );
            }
            let mut cur_hi = split;
            for (idx, slice) in pane.history.iter().enumerate() {
                if cur_hi <= 1e-4 {
                    break;
                }
                let is_last = idx + 1 == pane.history.len();
                let raw_lo = if is_last {
                    0.0
                } else {
                    (offset_y - slice.lag_px).max(0.0)
                };
                let c_lo = raw_lo.min(cur_hi);
                let band_h = cur_hi - c_lo;
                if band_h > 1e-4 {
                    emit(
                        &slice.layer,
                        ClipRect {
                            x: vp.x,
                            y: vp.y + c_lo,
                            w: vp.w,
                            h: band_h,
                        },
                        offset_y - slice.lag_px,
                    );
                }
                cur_hi = c_lo;
            }
        } else {
            // Scrolling up (`new_top < old_top`, `offset_y < 0`): content glides downward.
            // Top `[vp.y, vp.y + top_h]` draws `base_layer` shifted by `offset_y` (`< 0`).
            // Bottom `[vp.y + top_h, vp.y + vp.h]` is partitioned across `pane.history` without overlap!
            let mag = (-offset_y).clamp(0.0, vp.h);
            let top_h = (vp.h - mag).max(0.0);
            if top_h > 0.0 {
                emit(
                    base_layer,
                    ClipRect {
                        x: vp.x,
                        y: vp.y,
                        w: vp.w,
                        h: top_h,
                    },
                    offset_y,
                );
            }
            let mut cur_hi = mag;
            for (idx, slice) in pane.history.iter().enumerate() {
                if cur_hi <= 1e-4 {
                    break;
                }
                let is_last = idx + 1 == pane.history.len();
                let lag_mag = (-slice.lag_px).max(0.0);
                let raw_lo = if is_last {
                    0.0
                } else {
                    (mag - lag_mag).max(0.0)
                };
                let c_lo = raw_lo.min(cur_hi);
                let band_h = cur_hi - c_lo;
                if band_h > 1e-4 {
                    emit(
                        &slice.layer,
                        ClipRect {
                            x: vp.x,
                            y: vp.y + vp.h - cur_hi,
                            w: vp.w,
                            h: band_h,
                        },
                        offset_y - slice.lag_px,
                    );
                }
                cur_hi = c_lo;
            }
        }
    }

    fn static_complement_rects(&self, full_w: f64, full_h: f64) -> Vec<ClipRect> {
        let mut rects = vec![ClipRect {
            x: 0.0,
            y: 0.0,
            w: full_w,
            h: full_h,
        }];
        for pane in self.panes.values() {
            if pane.is_active() && pane.vp.w > 0.0 && pane.vp.h > 0.0 {
                subtract_rect_from_list(
                    &mut rects,
                    ClipRect {
                        x: pane.vp.x,
                        y: pane.vp.y,
                        w: pane.vp.w,
                        h: pane.vp.h,
                    },
                );
            }
        }
        rects
    }

    /// Composites `self.base_layer` plus any active pane split-clip scroll transitions onto `cr`
    /// using disjoint `1.0x` screen coverage and `CAIRO_FILTER_NEAREST`.
    ///
    /// # Safety
    /// `cr` must be a valid non-null `*mut CairoT` on the current thread.
    unsafe fn composite_scroll_layers(&self, cr: *mut CairoT, safe_w: c_int, safe_h: c_int) {
        let Some(base) = self.base_layer.as_deref() else {
            return;
        };
        // SAFETY: `cr` is valid per caller invariant.
        let (scale_x, scale_y) = unsafe { query_cairo_device_scale(cr) };
        let any_scroll = self.panes.values().any(PaneScrollState::is_active);

        if !any_scroll {
            // Fast single-blit path in 1:1 physical pixel space when no pane is mid-scroll.
            // SAFETY: `cr` and `base.surface.surface` are valid on this thread.
            unsafe {
                let mut tsx = 1.0_f64;
                let mut tsy = 1.0_f64;
                let target = ffi::cairo_get_target(cr);
                if !target.is_null() {
                    ffi::cairo_surface_get_device_scale(target, &mut tsx, &mut tsy);
                }
                ffi::cairo_save(cr);
                ffi::cairo_identity_matrix(cr);
                if (tsx - 1.0).abs() > 1e-6 || (tsy - 1.0).abs() > 1e-6 {
                    ffi::cairo_scale(cr, 1.0 / tsx.max(0.25), 1.0 / tsy.max(0.25));
                }
                ffi::cairo_set_operator(cr, ffi::CAIRO_OPERATOR_SOURCE);
                set_source_surface_nearest(cr, base.surface.surface, 0.0, 0.0);
                ffi::cairo_paint(cr);
                ffi::cairo_restore(cr);
            }
            return;
        }

        let full_w = f64::from(safe_w);
        let full_h = f64::from(safe_h);
        for static_rect in self.static_complement_rects(full_w, full_h) {
            // SAFETY: `cr` and `base.surface.surface` are valid on this thread.
            unsafe {
                blit_clipped_shifted(cr, base.surface.surface, static_rect, 0.0, scale_x, scale_y);
            }
        }

        for pane in self.panes.values() {
            if !pane.is_active() {
                continue;
            }
            Self::emit_pane_scroll_slices(pane, base, |layer, clip, shift_y| {
                // SAFETY: `cr` and `layer.surface.surface` are valid on this thread.
                unsafe {
                    blit_clipped_shifted(
                        cr,
                        layer.surface.surface,
                        clip,
                        shift_y,
                        scale_x,
                        scale_y,
                    );
                }
            });
        }
    }

    fn update_scroll_viewports(&mut self, hints: &AnimHints, resized: bool) {
        if !hints.smooth_scroll || resized {
            for (_, mut prev) in self.panes.drain() {
                prev.clear_into_pool(&mut self.surface_pool);
            }
            if resized {
                self.surface_pool.clear();
            }
            if hints.smooth_scroll {
                for vp in &hints.viewports {
                    self.panes.insert(
                        vp.pane_id,
                        PaneScrollState {
                            vp: *vp,
                            offset_y_px: 0.0,
                            history: Vec::new(),
                        },
                    );
                }
            }
            return;
        }

        let any_scrolled = self.base_layer.is_some()
            && hints.viewports.iter().any(|vp| {
                vp.w > 1.0
                    && vp.h > 1.0
                    && vp.cell_h > 1.0
                    && self.panes.get(&vp.pane_id).is_some_and(|prev| {
                        prev.matches_geometry(vp) && prev.vp.top_row != vp.top_row
                    })
            });

        // Zero-copy promotion: promote `self.base_layer` (`Rc<SceneLayer>`, whose `GdkTexture` is
        // ALREADY uploaded in GPU VRAM!) directly into scrolled panes' history without allocating
        // or blitting an intermediate full-screen `OffscreenSurface`!
        let outgoing_layer: Option<Rc<SceneLayer>> = if any_scrolled {
            self.base_layer.take()
        } else {
            None
        };

        let mut next_panes = HashMap::with_capacity(hints.viewports.len());
        for vp in &hints.viewports {
            if let Some(mut prev) = self.panes.remove(&vp.pane_id)
                && prev.matches_geometry(vp)
                && vp.w > 1.0
                && vp.h > 1.0
                && vp.cell_h > 1.0
            {
                if prev.vp.top_row != vp.top_row
                    && let Some(ref prev_scene) = outgoing_layer
                {
                    let delta_rows = (vp.top_row - prev.vp.top_row) as f64;
                    let raw_delta_px = delta_rows * vp.cell_h;
                    let max_off = (vp.h * 0.95).max(1.0);
                    let next_off = (prev.offset_y_px + raw_delta_px).clamp(-max_off, max_off);
                    prev.vp = *vp;

                    if next_off.abs() >= SNAP_EPSILON_PX {
                        let same_dir = prev.offset_y_px.abs() >= SNAP_EPSILON_PX
                            && prev.offset_y_px.signum() == next_off.signum()
                            && raw_delta_px.signum() == next_off.signum();
                        if same_dir {
                            let added_px = next_off - prev.offset_y_px;
                            if added_px.abs() >= 0.25 {
                                for existing in &mut prev.history {
                                    existing.lag_px += added_px;
                                }
                                prev.history.insert(
                                    0,
                                    ScrollHistorySlice {
                                        lag_px: added_px,
                                        step_px: added_px,
                                        layer: Rc::clone(prev_scene),
                                    },
                                );
                            }
                            prev.offset_y_px = next_off;
                            prev.prune_invisible_history(&mut self.surface_pool);
                        } else {
                            // Fresh scroll or direction reversal: replace history with `prev_scene`
                            // anchored at `lag_px = next_off` so `offset_y - lag_px == 0` at `t=0`.
                            prev.clear_into_pool(&mut self.surface_pool);
                            prev.offset_y_px = next_off;
                            prev.history.push(ScrollHistorySlice {
                                lag_px: next_off,
                                step_px: next_off,
                                layer: Rc::clone(prev_scene),
                            });
                        }
                    } else {
                        prev.clear_into_pool(&mut self.surface_pool);
                    }
                } else {
                    prev.vp = *vp;
                }
                next_panes.insert(vp.pane_id, prev);
                continue;
            }
            next_panes.insert(
                vp.pane_id,
                PaneScrollState {
                    vp: *vp,
                    offset_y_px: 0.0,
                    history: Vec::new(),
                },
            );
        }
        for (_, mut stale) in self.panes.drain() {
            stale.clear_into_pool(&mut self.surface_pool);
        }
        self.panes = next_panes;
        if let Some(unused) = outgoing_layer {
            Self::recycle_rc_layer(&mut self.surface_pool, unused);
        }
    }

    fn sync_base_layer(
        &mut self,
        safe_w: c_int,
        safe_h: c_int,
        scale_x: f64,
        scale_y: f64,
        frame: &RenderFrame,
        hints: &AnimHints,
    ) {
        let primary_caret = find_primary_caret(frame);
        let skip_caret_idx = if hints.cursor_anim {
            primary_caret.map(|c| c.cmd_idx)
        } else {
            None
        };

        let resized = self.base_layer.as_deref().is_some_and(|b| {
            b.surface.width != safe_w
                || b.surface.height != safe_h
                || (b.surface.device_scale_x - scale_x).abs() > 1e-4
                || (b.surface.device_scale_y - scale_y).abs() > 1e-4
        });
        let frame_changed = resized
            || self.base_layer.is_none()
            || frame.generation != self.cached_gen
            || skip_caret_idx != self.cached_skip_caret;

        if frame_changed {
            self.update_scroll_viewports(hints, resized);

            // Recycle the previous `base_layer` (if not already promoted into `pane.history` by
            // `update_scroll_viewports`) before popping a uniquely owned back-buffer surface.
            if let Some(prev_layer) = self.base_layer.take() {
                Self::recycle_rc_layer(&mut self.surface_pool, prev_layer);
            }

            if let Ok(mut surf) = self.take_or_alloc_surface(safe_w, safe_h, scale_x, scale_y) {
                // SAFETY: `surf` was verified `is_uniquely_owned()` (or freshly allocated) on this thread.
                unsafe {
                    render_offscreen_incremental_with_opts(&mut surf, frame, skip_caret_idx);
                }
                self.base_layer = Some(Rc::new(SceneLayer::new(surf)));
                self.cached_gen = frame.generation;
                self.cached_skip_caret = skip_caret_idx;
            }

            if hints.cursor_anim {
                self.cursor.set_target(primary_caret, true);
            } else {
                self.cursor.set_target(None, false);
            }
        } else {
            if !hints.smooth_scroll && !self.panes.is_empty() {
                for (_, mut p) in self.panes.drain() {
                    p.clear_into_pool(&mut self.surface_pool);
                }
            }
            if !hints.cursor_anim && self.cursor.is_active() {
                self.cursor.set_target(primary_caret, false);
            }
        }
    }

    /// Synchronizes GPU-cached `GdkMemoryTexture` layers for `(frame, hints)` and appends GSK
    /// texture, clip, and cursor nodes directly into GTK4's `GtkSnapshot` (zero `GskCairoNode`s
    /// during steady-state rendering, cursor movement, and smooth scroll!).
    ///
    /// # Safety
    /// `target.snapshot` must be a valid non-null `*mut GtkSnapshot` exclusively accessed on the GTK main thread.
    pub(crate) unsafe fn snapshot_to_gtk(
        &mut self,
        target: SnapshotTarget,
        frame: &RenderFrame,
        hints: &AnimHints,
        transfer_overlay: Option<&TransferOverlay>,
    ) {
        let snapshot = target.snapshot;
        if snapshot.is_null() {
            return;
        }
        let safe_w = target.width.max(frame.window.width).max(1);
        let safe_h = target.height.max(frame.window.height).max(1);
        let scale = if target.scale.is_finite() && target.scale > 0.25 {
            target.scale
        } else {
            1.0
        };

        self.sync_base_layer(safe_w, safe_h, scale, scale, frame, hints);
        self.seed_frame_clock(target.frame_clock_sec);

        let full_w = f64::from(safe_w);
        let full_h = f64::from(safe_h);
        let full_bounds = GrapheneRect::new(0.0, 0.0, full_w as f32, full_h as f32);

        if let Some(base) = self.base_layer.as_deref()
            && let Some(base_tex) = base.ensure_gpu_texture()
        {
            let any_scroll = self.panes.values().any(PaneScrollState::is_active);
            let inv_scale = (1.0 / scale) as f32;
            // Enter exact 1:1 physical device-pixel space (`scale * (1.0 / scale) == 1.0`) so
            // `GdkMemoryTexture` texels `(0..phys_w, 0..phys_h)` align 1:1 with Wayland physical
            // buffer pixels even on fractional scales (`1.25x`, `1.5x`, `1.75x`).
            // SAFETY: `snapshot` is valid on the GTK main thread; `save` and `restore` are balanced.
            unsafe {
                ffi::gtk_snapshot_save(snapshot);
                if (scale - 1.0).abs() > 1e-4 {
                    ffi::gtk_snapshot_scale(snapshot, inv_scale, inv_scale);
                }
            }
            if !any_scroll {
                let phys_bounds = GrapheneRect::new(
                    0.0,
                    0.0,
                    base.surface.phys_w as f32,
                    base.surface.phys_h as f32,
                );
                // Single GPU textured quad — zero CPU pixel work on VSync ticks!
                // SAFETY: `snapshot` and `base_tex` are valid on the GTK main thread.
                unsafe {
                    ffi::gtk_snapshot_append_texture(snapshot, base_tex, &phys_bounds);
                }
            } else {
                let base_phys = (base.surface.phys_w, base.surface.phys_h);
                for static_rect in self.static_complement_rects(full_w, full_h) {
                    // SAFETY: `snapshot` and `base_tex` are valid on the GTK main thread.
                    unsafe {
                        snapshot_clipped_shifted_texture_phys(
                            snapshot,
                            base_tex,
                            base_phys,
                            static_rect,
                            0.0,
                            scale,
                        );
                    }
                }
                for pane in self.panes.values() {
                    if !pane.is_active() {
                        continue;
                    }
                    Self::emit_pane_scroll_slices(pane, base, |layer, clip, shift_y| {
                        if let Some(tex) = layer.ensure_gpu_texture() {
                            // SAFETY: `snapshot` and `tex` are valid on the GTK main thread.
                            unsafe {
                                snapshot_clipped_shifted_texture_phys(
                                    snapshot,
                                    tex,
                                    (layer.surface.phys_w, layer.surface.phys_h),
                                    clip,
                                    shift_y,
                                    scale,
                                );
                            }
                        }
                    });
                }
            }
            // SAFETY: Matches `gtk_snapshot_save` above, restoring widget logical coordinates.
            unsafe {
                ffi::gtk_snapshot_restore(snapshot);
            }

            if hints.cursor_anim {
                // SAFETY: `snapshot` is valid on the GTK main thread.
                unsafe {
                    self.cursor.snapshot_draw(snapshot);
                }
            }
        } else {
            // Fallback via Cairo node only if `GdkMemoryTexture` allocation failed.
            // SAFETY: `snapshot` is valid; `cr` returned by `gtk_snapshot_append_cairo` is owned
            // by the caller and must be destroyed with `cairo_destroy`.
            unsafe {
                let cr = ffi::gtk_snapshot_append_cairo(snapshot, &full_bounds);
                if !cr.is_null() {
                    render_frame_to_cairo_with_opts(cr, full_w, full_h, frame, None);
                    ffi::cairo_destroy(cr);
                }
            }
        }

        if let Some(ov) = transfer_overlay
            && ov.active
        {
            let card_w = (full_w - 32.0).clamp(240.0, 440.0);
            let card_h = 68.0;
            let card_x = (full_w - card_w - 16.0).max(12.0).floor();
            let card_y = (full_h - card_h - 34.0).max(12.0).floor();
            let ov_bounds = GrapheneRect::new(
                (card_x - 2.0) as f32,
                (card_y - 2.0) as f32,
                (card_w + 4.0) as f32,
                (card_h + 4.0) as f32,
            );
            // SAFETY: Appends a small bounded Cairo overlay node only while file transfer is active.
            unsafe {
                let cr = ffi::gtk_snapshot_append_cairo(snapshot, &ov_bounds);
                if !cr.is_null() {
                    draw_transfer_progress_overlay(cr, full_w, full_h, ov);
                    ffi::cairo_destroy(cr);
                }
            }
        }
    }

    /// Synchronizes cached surfaces/targets for `(frame, hints)` and renders the composited scene.
    ///
    /// # Safety
    /// `cr` must be a valid non-null `*mut CairoT` exclusively accessed on the current thread.
    #[cfg(test)]
    pub(crate) unsafe fn render_to_cairo(
        &mut self,
        cr: *mut CairoT,
        width: c_int,
        height: c_int,
        frame: &RenderFrame,
        hints: &AnimHints,
    ) {
        // SAFETY: Caller guarantees `cr` is valid on this thread.
        unsafe {
            self.render_to_cairo_scaled(cr, width, height, 1.0, frame, hints);
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) unsafe fn render_to_cairo_scaled(
        &mut self,
        cr: *mut CairoT,
        width: c_int,
        height: c_int,
        min_scale: f64,
        frame: &RenderFrame,
        hints: &AnimHints,
    ) {
        // SAFETY: Forwards caller's `cr` invariant with no transfer overlay.
        unsafe {
            self.render_to_cairo_scaled_with_overlay(
                cr,
                (width, height),
                min_scale,
                frame,
                hints,
                None,
            );
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) unsafe fn render_to_cairo_scaled_with_overlay(
        &mut self,
        cr: *mut CairoT,
        (width, height): (c_int, c_int),
        min_scale: f64,
        frame: &RenderFrame,
        hints: &AnimHints,
        transfer_overlay: Option<&TransferOverlay>,
    ) {
        if cr.is_null() {
            return;
        }
        let safe_w = width.max(frame.window.width).max(1);
        let safe_h = height.max(frame.window.height).max(1);
        let floor_scale = if min_scale.is_finite() && min_scale >= 1.0 {
            min_scale
        } else {
            1.0
        };
        // SAFETY: `cr` is valid per caller invariant.
        let (cr_sx, cr_sy) = unsafe { query_cairo_device_scale(cr) };
        let scale_x = cr_sx.max(floor_scale);
        let scale_y = cr_sy.max(floor_scale);

        self.sync_base_layer(safe_w, safe_h, scale_x, scale_y, frame, hints);

        if self.base_layer.is_some() {
            // SAFETY: `cr` is valid per caller invariant.
            unsafe {
                self.composite_scroll_layers(cr, safe_w, safe_h);
                if hints.cursor_anim {
                    self.cursor.draw(cr);
                }
            }
        } else {
            // SAFETY: `cr` is valid per caller invariant.
            unsafe {
                render_frame_to_cairo_with_opts(
                    cr,
                    f64::from(safe_w),
                    f64::from(safe_h),
                    frame,
                    None,
                );
            }
        }

        if let Some(ov) = transfer_overlay
            && ov.active
        {
            // SAFETY: `cr` is valid per caller invariant.
            unsafe {
                draw_transfer_progress_overlay(cr, f64::from(safe_w), f64::from(safe_h), ov);
            }
        }
    }
}

fn format_transfer_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", (bytes as f64) / 1024.0)
    } else {
        format!("{:.2} MiB", (bytes as f64) / (1024.0 * 1024.0))
    }
}

unsafe fn draw_transfer_progress_overlay(
    cr: *mut CairoT,
    win_w: f64,
    win_h: f64,
    ov: &TransferOverlay,
) {
    let card_w = (win_w - 32.0).clamp(240.0, 440.0);
    let card_h = 68.0;
    let card_x = (win_w - card_w - 16.0).max(12.0).floor();
    let card_y = (win_h - card_h - 34.0).max(12.0).floor();
    let frac = if ov.bytes_total > 0 {
        (ov.bytes_done as f64 / ov.bytes_total as f64).clamp(0.0, 1.0)
    } else {
        0.5
    };
    let pct = (frac * 100.0).round() as u64;
    let badge = if ov.transport.eq_ignore_ascii_case("http") {
        "[HTTP]"
    } else {
        "[SSH]"
    };
    let title = format!("{badge} Fetching {}", ov.title);
    let detail = if ov.bytes_total > 0 {
        if ov.lines_loaded > 0 {
            format!(
                "{} / {}  ({pct}%)  •  {} lines",
                format_transfer_bytes(ov.bytes_done),
                format_transfer_bytes(ov.bytes_total),
                ov.lines_loaded
            )
        } else {
            format!(
                "{} / {}  ({pct}%)",
                format_transfer_bytes(ov.bytes_done),
                format_transfer_bytes(ov.bytes_total)
            )
        }
    } else {
        format!("{} received...", format_transfer_bytes(ov.bytes_done))
    };

    let bar_x = card_x + 12.0;
    let bar_y = card_y + 48.0;
    let bar_w = card_w - 24.0;
    let bar_h = 8.0;
    let (pill_x, pill_w) = if ov.bytes_total > 0 {
        (
            bar_x,
            (bar_w * frac).max(if ov.bytes_done > 0 { 4.0 } else { 0.0 }),
        )
    } else {
        let phase_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| (d.as_millis() % 1400) as u64);
        let t = phase_ms as f64 / 1400.0;
        let wave = 0.5 - 0.5 * (t * std::f64::consts::TAU).cos();
        let w = (bar_w * 0.28).max(24.0);
        (bar_x + (bar_w - w).max(0.0) * wave, w)
    };

    let cmds = [
        DrawCmd::ClipPush {
            x: card_x,
            y: card_y,
            w: card_w,
            h: card_h,
        },
        DrawCmd::FillRect {
            x: card_x,
            y: card_y,
            w: card_w,
            h: card_h,
            color: ColorRgba::from_rgba(36, 39, 58, 245),
            radius: 6.0,
        },
        DrawCmd::StrokeRect {
            x: card_x,
            y: card_y,
            w: card_w,
            h: card_h,
            color: ColorRgba::from_rgba(137, 180, 250, 255),
            line_width: 1.4,
            radius: 6.0,
        },
        DrawCmd::TextRun {
            x: card_x + 12.0,
            y: card_y + 6.0,
            text: title,
            font_family: "Monospace".to_string(),
            font_size_pt: 11.5,
            color: ColorRgba::from_rgba(137, 180, 250, 255),
            bold: false,
            italic: false,
            underline: false,
        },
        DrawCmd::TextRun {
            x: card_x + 12.0,
            y: card_y + 26.0,
            text: detail,
            font_family: "Monospace".to_string(),
            font_size_pt: 10.5,
            color: ColorRgba::from_rgba(205, 214, 244, 255),
            bold: false,
            italic: false,
            underline: false,
        },
        DrawCmd::FillRect {
            x: bar_x,
            y: bar_y,
            w: bar_w,
            h: bar_h,
            color: ColorRgba::from_rgba(24, 24, 37, 255),
            radius: 4.0,
        },
        DrawCmd::FillRect {
            x: pill_x,
            y: bar_y,
            w: pill_w,
            h: bar_h,
            color: ColorRgba::from_rgba(166, 227, 161, 255),
            radius: 4.0,
        },
        DrawCmd::ClipPop,
    ];
    // SAFETY: `cr` is valid per caller invariant.
    unsafe {
        render_commands_to_cairo(cr, &cmds, None);
    }
}

/// Accumulates high-frequency fractional touchpad `dx`/`dy` scroll deltas into clean discrete
/// units while passing standard mouse-wheel detents (`+-1.0`) immediately with zero lag.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScrollDeltaAccumulator {
    acc_x: f64,
    acc_y: f64,
    last_event_sec: Option<f64>,
}

impl ScrollDeltaAccumulator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn accumulate(&mut self, dx: f64, dy: f64, now_sec: f64) -> Option<(f64, f64)> {
        if !dx.is_finite() || !dy.is_finite() {
            return None;
        }
        if let Some(prev_sec) = self.last_event_sec
            && now_sec - prev_sec > 0.25
        {
            self.acc_x = 0.0;
            self.acc_y = 0.0;
        }
        self.last_event_sec = Some(now_sec);

        if dx != 0.0 && self.acc_x != 0.0 && dx.signum() != self.acc_x.signum() {
            self.acc_x = 0.0;
        }
        if dy != 0.0 && self.acc_y != 0.0 && dy.signum() != self.acc_y.signum() {
            self.acc_y = 0.0;
        }

        self.acc_x += dx;
        self.acc_y += dy;

        let emit_x = if self.acc_x.abs() >= 1.0 {
            let steps = self.acc_x.trunc();
            self.acc_x -= steps;
            steps
        } else {
            0.0
        };
        let emit_y = if self.acc_y.abs() >= 1.0 {
            let steps = self.acc_y.trunc();
            self.acc_y -= steps;
            steps
        } else {
            0.0
        };

        if emit_x != 0.0 || emit_y != 0.0 {
            Some((emit_x, emit_y))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use lava_proto::{
        AnimHints, CaretShape, ColorRgba, DrawCmd, RenderFrame, ScrollViewport, WindowConfig,
    };
    use libc::c_int;

    use super::{
        AnimController, CaretTarget, ClipRect, CursorPhysics, SNAP_EPSILON_PX,
        ScrollDeltaAccumulator, SnapshotTarget, find_primary_caret, subtract_rect_from_list,
    };
    use crate::ffi;
    use crate::render::OffscreenSurface;

    fn make_caret(x: f64, y: f64, w: f64, h: f64, shape: CaretShape) -> CaretTarget {
        CaretTarget {
            cmd_idx: 0,
            x,
            y,
            w,
            h,
            shape,
            color: ColorRgba::from_rgba(245, 224, 220, 255),
        }
    }

    #[test]
    fn four_corner_cursor_physics_leading_corners_advance_faster_than_trailing_and_snap() {
        let mut phys = CursorPhysics::new();
        let start = make_caret(10.0, 40.0, 9.0, 20.0, CaretShape::Block);
        phys.set_target(Some(start), true);
        assert!(!phys.is_active(), "initial placement must not smear");

        // Jump right by 100px (> 2 cells -> tau_lead = 0.024s, tau_trail = 0.062s).
        let target = make_caret(110.0, 40.0, 9.0, 20.0, CaretShape::Block);
        phys.set_target(Some(target), true);
        assert!(phys.is_active());

        assert!(phys.step(0.016));
        let c = phys.corners();
        // Moving right (+X): TR (1) and BR (2) are leading; TL (0) and BL (3) are trailing.
        let lead_progress = c[1].0 - 19.0; // started at 10 + 9 = 19, target = 119
        let trail_progress = c[0].0 - 10.0; // started at 10, target = 110
        assert!(
            lead_progress > trail_progress + 15.0,
            "expected leading corners ({lead_progress:.2}) way ahead of trailing ({trail_progress:.2})"
        );
        // Stretched width during rightward jump must exceed static 9px width.
        assert!(c[1].0 - c[0].0 > 25.0, "expected horizontal smear stretch");

        // Step until convergence (< 0.2px) and verify clean snap to exact target corners.
        let mut ticks = 0;
        while phys.step(0.016) {
            ticks += 1;
            assert!(ticks < 60, "cursor spring must converge within < 1s");
        }
        assert!(!phys.is_active());
        assert_eq!(phys.corners(), target.target_corners());
    }

    #[test]
    fn typing_short_hop_decays_faster_than_long_jump_and_hollow_block_is_ignored() {
        let mut short_phys = CursorPhysics::new();
        let mut long_phys = CursorPhysics::new();

        short_phys.set_target(
            Some(make_caret(10.0, 20.0, 9.0, 19.0, CaretShape::Beam)),
            true,
        );
        long_phys.set_target(
            Some(make_caret(10.0, 20.0, 9.0, 19.0, CaretShape::Beam)),
            true,
        );

        // 1-cell typing hop (9px <= 2.25 * 9px -> tau_trail = 0.038s) vs 10-cell jump (90px -> 0.062s).
        short_phys.set_target(
            Some(make_caret(19.0, 20.0, 9.0, 19.0, CaretShape::Beam)),
            true,
        );
        long_phys.set_target(
            Some(make_caret(100.0, 20.0, 9.0, 19.0, CaretShape::Beam)),
            true,
        );

        short_phys.step(0.020);
        long_phys.step(0.020);

        let short_rem_frac = (19.0 - short_phys.corners()[0].0) / 9.0;
        let long_rem_frac = (100.0 - long_phys.corners()[0].0) / 90.0;
        assert!(
            short_rem_frac < long_rem_frac - 0.10,
            "expected <=2-cell typing trail ({short_rem_frac:.3}) to decay faster than long jump ({long_rem_frac:.3})"
        );

        // Verify `find_primary_caret` picks the active Block/Beam caret and ignores trailing HollowBlock.
        let frame = RenderFrame {
            generation: 1,
            window: WindowConfig::default(),
            commands: vec![
                DrawCmd::CursorCaret {
                    x: 24.0,
                    y: 40.0,
                    w: 9.0,
                    h: 19.0,
                    shape: CaretShape::Block,
                    color: ColorRgba::from_rgba(255, 255, 255, 255),
                },
                DrawCmd::CursorCaret {
                    x: 420.0,
                    y: 40.0,
                    w: 9.0,
                    h: 19.0,
                    shape: CaretShape::HollowBlock,
                    color: ColorRgba::from_rgba(120, 120, 140, 255),
                },
            ],
        };
        let found = find_primary_caret(&frame).expect("should find primary non-hollow caret");
        assert_eq!(found.cmd_idx, 0);
        assert_eq!(found.shape, CaretShape::Block);
    }

    fn build_striped_scroll_frame(
        generation: u64,
        top_row: i64,
        vp_y: f64,
        vp_h: f64,
        cell_h: f64,
        header_color: ColorRgba,
        modeline_color: ColorRgba,
    ) -> RenderFrame {
        let mut cmds = Vec::new();
        // Fixed tab bar `[0, vp_y)`
        cmds.push(DrawCmd::FillRect {
            x: 0.0,
            y: 0.0,
            w: 200.0,
            h: vp_y,
            color: header_color,
            radius: 0.0,
        });
        // Buffer rows `[vp_y, vp_y + vp_h)` where row `r` has unique deterministic color `(r*7 % 200 + 30)`.
        let vis_rows = (vp_h / cell_h).round() as i64;
        for i in 0..vis_rows {
            let doc_row = top_row + i;
            let r_byte = ((doc_row.rem_euclid(200)) as u8).saturating_add(30);
            let g_byte = (((doc_row * 3).rem_euclid(180)) as u8).saturating_add(40);
            cmds.push(DrawCmd::FillRect {
                x: 0.0,
                y: vp_y + (i as f64) * cell_h,
                w: 200.0,
                h: cell_h,
                color: ColorRgba::from_rgba(r_byte, g_byte, 120, 255),
                radius: 0.0,
            });
        }
        // Fixed mode-line `[vp_y + vp_h, 260)`
        cmds.push(DrawCmd::FillRect {
            x: 0.0,
            y: vp_y + vp_h,
            w: 200.0,
            h: 260.0 - (vp_y + vp_h),
            color: modeline_color,
            radius: 0.0,
        });

        RenderFrame {
            generation,
            window: WindowConfig {
                title: "Scroll Test".to_string(),
                width: 200,
                height: 260,
                background: ColorRgba::from_rgba(0, 0, 0, 255), // pure black sentinel to detect any gap!
                ..WindowConfig::default()
            },
            commands: cmds,
        }
    }

    #[test]
    fn smooth_scroll_1_line_and_100_line_continuity_zero_gaps_and_fixed_modeline() {
        let mut ctrl = AnimController::new();
        let canvas = OffscreenSurface::new(200, 260).expect("offscreen surface");
        let vp_y = 20.0;
        let vp_h = 200.0;
        let cell_h = 20.0;
        let header_color = ColorRgba::from_rgba(17, 17, 27, 255);
        let modeline_color = ColorRgba::from_rgba(49, 50, 68, 255);

        let make_hints = |top_row: i64| AnimHints {
            smooth_scroll: true,
            cursor_anim: true,
            viewports: vec![ScrollViewport {
                pane_id: 1,
                doc_id: 10,
                x: 0.0,
                y: vp_y,
                w: 200.0,
                h: vp_h,
                top_row,
                left_col: 0,
                cell_w: 8.0,
                cell_h,
            }],
        };

        // Initial frame at top_row = 0.
        let f0 = build_striped_scroll_frame(1, 0, vp_y, vp_h, cell_h, header_color, modeline_color);
        // SAFETY: `canvas.cr` is a valid uniquely owned Cairo context.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 260, &f0, &make_hints(0));
        }
        assert!(
            !ctrl.is_active(),
            "idle initial frame must have 0 active animations"
        );
        let expected_header_px = canvas.read_argb_pixel(100, 10);
        let expected_modeline_px = canvas.read_argb_pixel(100, 240);
        let row1_orig_px = canvas.read_argb_pixel(100, 50); // row 1 at y=40..60

        // 1-line scroll down (`top_row`: 0 -> 1, initial offset = +20px).
        let f1 = build_striped_scroll_frame(2, 1, vp_y, vp_h, cell_h, header_color, modeline_color);
        // SAFETY: `canvas.cr` is valid.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 260, &f1, &make_hints(1));
        }
        assert!((ctrl.pane_offset_y(1) - 20.0).abs() < 1e-6);

        // Advance halfway (~29ms -> offset ~ 10px) and verify continuity across all 260 scanlines!
        ctrl.step_dt(0.029);
        let mid_off = ctrl.pane_offset_y(1);
        assert!(
            mid_off > 8.0 && mid_off < 12.0,
            "unexpected mid offset: {mid_off}"
        );
        // SAFETY: `canvas.cr` is valid.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 260, &f1, &make_hints(1));
        }
        assert_eq!(
            canvas.read_argb_pixel(100, 10),
            expected_header_px,
            "header must never shift during smooth scroll"
        );
        assert_eq!(
            canvas.read_argb_pixel(100, 240),
            expected_modeline_px,
            "mode-line must never shift during smooth scroll"
        );
        // At `mid_off ≈ 10px`, row 1 (which is at y=20..40 in f1) is shifted by +10px to y=30..50!
        assert_eq!(
            canvas.read_argb_pixel(100, 35),
            row1_orig_px,
            "row 1 must glide smoothly through mid-position y=35"
        );
        // Every scanline inside `[20, 220)` must have non-zero green/blue (never black 0xFF000000 gap!).
        for y in 20..220 {
            let px = canvas.read_argb_pixel(100, y);
            assert_ne!(
                px, 0xFF00_0000,
                "detected blank scroll gap at scanline y={y}"
            );
        }

        // Now test 100-line jump (`top_row`: 1 -> 101) AND reverse 100-line jump (`101 -> 1`).
        let f101 =
            build_striped_scroll_frame(3, 101, vp_y, vp_h, cell_h, header_color, modeline_color);
        // SAFETY: `canvas.cr` is valid.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 260, &f101, &make_hints(101));
        }
        assert!(
            (ctrl.pane_offset_y(1) - vp_h * 0.95).abs() < 1e-6,
            "100-line jump must clamp to +0.95 * vp.h"
        );
        ctrl.step_dt(0.030);
        // SAFETY: `canvas.cr` is valid.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 260, &f101, &make_hints(101));
        }
        for y in 20..220 {
            assert_ne!(
                canvas.read_argb_pixel(100, y),
                0xFF00_0000,
                "detected gap during 100-line downward jump at y={y}"
            );
        }
        assert_eq!(canvas.read_argb_pixel(100, 240), expected_modeline_px);

        // Reverse 100-line upward jump (`101 -> 1`): `offset_y < 0`.
        let f_back =
            build_striped_scroll_frame(4, 1, vp_y, vp_h, cell_h, header_color, modeline_color);
        // SAFETY: `canvas.cr` is valid.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 260, &f_back, &make_hints(1));
        }
        assert!(ctrl.pane_offset_y(1) < -100.0);
        ctrl.step_dt(0.035);
        // SAFETY: `canvas.cr` is valid.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 260, &f_back, &make_hints(1));
        }
        for y in 20..220 {
            assert_ne!(
                canvas.read_argb_pixel(100, y),
                0xFF00_0000,
                "detected gap during upward jump at y={y}"
            );
        }
        assert_eq!(canvas.read_argb_pixel(100, 10), expected_header_px);
        assert_eq!(canvas.read_argb_pixel(100, 240), expected_modeline_px);

        // Step to completion and verify 0% idle wakeup behavior (`tick_at` returns `(false, false)`).
        let mut t = 1.0;
        let mut steps = 0;
        loop {
            let (active, redraw) = ctrl.tick_at(t);
            t += 0.016;
            steps += 1;
            if !active {
                assert!(redraw, "final snap frame must request one last redraw");
                break;
            }
            assert!(steps < 60, "scroll decay must settle within < 1s");
        }
        assert!(ctrl.pane_offset_y(1).abs() < SNAP_EPSILON_PX);
        assert_eq!(
            ctrl.tick_at(t + 0.016),
            (false, false),
            "idle ticks must never request redraws"
        );
    }

    #[test]
    fn touchpad_fractional_scroll_accumulator_coalesces_and_reverses_cleanly() {
        let mut acc = ScrollDeltaAccumulator::new();
        // Fractional touchpad events accumulate until >= 1.0 unit.
        assert_eq!(acc.accumulate(0.0, 0.30, 1.00), None);
        assert_eq!(acc.accumulate(0.0, 0.35, 1.01), None);
        assert_eq!(acc.accumulate(0.0, 0.40, 1.02), Some((0.0, 1.0)));

        // Direction reversal clears opposite remainder immediately.
        assert_eq!(acc.accumulate(0.0, 0.60, 1.03), None);
        assert_eq!(acc.accumulate(0.0, -0.55, 1.04), None);
        assert_eq!(acc.accumulate(0.0, -0.50, 1.05), Some((0.0, -1.0)));

        // Discrete mouse wheel detent passes through immediately.
        assert_eq!(acc.accumulate(0.0, 1.0, 2.00), Some((0.0, 1.0)));
    }

    #[test]
    fn diagonal_cursor_smear_polygon_rasterizes_trail_and_clears_after_snap() {
        let mut ctrl = AnimController::new();
        let canvas = OffscreenSurface::new(200, 160).expect("surface");
        let bg = ColorRgba::from_rgba(20, 20, 30, 255);
        let caret_color = ColorRgba::from_rgba(250, 180, 100, 255);
        let hints = AnimHints::default();

        let make_frame = |generation: u64, x: f64, y: f64| RenderFrame {
            generation,
            window: WindowConfig {
                title: "Smear Test".to_string(),
                width: 200,
                height: 160,
                background: bg,
                ..WindowConfig::default()
            },
            commands: vec![DrawCmd::CursorCaret {
                x,
                y,
                w: 10.0,
                h: 20.0,
                shape: CaretShape::Block,
                color: caret_color,
            }],
        };

        let f1 = make_frame(1, 20.0, 20.0);
        // SAFETY: `canvas.cr` is valid.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 160, &f1, &hints);
        }
        let bg_px = canvas.read_argb_pixel(100, 80);

        // Diagonal jump `(20, 20) -> (140, 110)`
        let f2 = make_frame(2, 140.0, 110.0);
        // SAFETY: `canvas.cr` is valid.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 160, &f2, &hints);
        }
        ctrl.step_dt(0.024);
        // SAFETY: `canvas.cr` is valid.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 160, &f2, &hints);
        }

        // Midpoint region around (85, 70) must contain painted smear polygon pixels!
        let mut smear_pixels = 0usize;
        for py in 55..95 {
            for px in 65..115 {
                if canvas.read_argb_pixel(px, py) != bg_px {
                    smear_pixels += 1;
                }
            }
        }
        assert!(
            smear_pixels > 40,
            "expected diagonal 4-corner smear polygon pixels along path, got {smear_pixels}"
        );

        // Converge to rest and verify midpoint trail cleans up completely while final caret is crisp.
        for _ in 0..45 {
            ctrl.step_dt(0.016);
        }
        assert!(!ctrl.is_active());
        // SAFETY: `canvas.cr` is valid.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 160, &f2, &hints);
        }
        assert_eq!(canvas.read_argb_pixel(85, 70), bg_px);
        assert_ne!(canvas.read_argb_pixel(145, 120), bg_px);
    }

    #[test]
    fn anim_controller_preserves_hidpi_device_scale_on_offscreen_base_surface() {
        let mut ctrl = AnimController::new();
        let canvas = OffscreenSurface::new_scaled(200, 160, 2.0, 2.0).expect("2x surface");
        let frame = RenderFrame {
            generation: 1,
            window: WindowConfig {
                title: "HiDPI".to_string(),
                width: 200,
                height: 160,
                ..WindowConfig::default()
            },
            commands: vec![DrawCmd::FillRect {
                x: 0.0,
                y: 0.0,
                w: 200.0,
                h: 160.0,
                color: ColorRgba::from_rgba(30, 30, 46, 255),
                radius: 0.0,
            }],
        };
        let hints = AnimHints::default();
        // SAFETY: `canvas.cr` is a valid 2x-scaled Cairo context.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 160, &frame, &hints);
        }
        let base = ctrl.base_surface().expect("base_surface allocated");
        assert_eq!(base.width, 200);
        assert_eq!(base.height, 160);
        assert!((base.device_scale_x - 2.0).abs() < 1e-4);
        assert!((base.device_scale_y - 2.0).abs() < 1e-4);
    }

    #[test]
    fn multi_step_mid_scroll_chaining_is_gap_free_and_disjoint_complement_covers_exact_area() {
        // 1. Disjoint static complement partitioning check (`1.0x` total area, zero overlap)
        let mut rects = vec![ClipRect {
            x: 0.0,
            y: 0.0,
            w: 3405.0,
            h: 1324.0,
        }];
        let hole = ClipRect {
            x: 220.0,
            y: 28.0,
            w: 3185.0,
            h: 1250.0,
        };
        subtract_rect_from_list(&mut rects, hole);
        let static_area: f64 = rects.iter().map(|r| r.w * r.h).sum();
        let expected_area = 3405.0 * 1324.0 - hole.w * hole.h;
        assert!(
            (static_area - expected_area).abs() < 1e-3,
            "expected disjoint complement area {expected_area}, got {static_area}"
        );

        // 2. Rapid continuous scrolling (`0 -> 1 -> 2 -> 3` while mid-decay!)
        let mut ctrl = AnimController::new();
        let canvas = OffscreenSurface::new(200, 260).expect("offscreen surface");
        let vp_y = 20.0;
        let vp_h = 200.0;
        let cell_h = 20.0;
        let header_color = ColorRgba::from_rgba(17, 17, 27, 255);
        let modeline_color = ColorRgba::from_rgba(49, 50, 68, 255);

        let make_hints = |top_row: i64| AnimHints {
            smooth_scroll: true,
            cursor_anim: true,
            viewports: vec![ScrollViewport {
                pane_id: 1,
                doc_id: 10,
                x: 0.0,
                y: vp_y,
                w: 200.0,
                h: vp_h,
                top_row,
                left_col: 0,
                cell_w: 8.0,
                cell_h,
            }],
        };

        let f0 = build_striped_scroll_frame(1, 0, vp_y, vp_h, cell_h, header_color, modeline_color);
        // SAFETY: `canvas.cr` is valid.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 260, &f0, &make_hints(0));
        }
        let expected_header_px = canvas.read_argb_pixel(100, 10);
        let expected_modeline_px = canvas.read_argb_pixel(100, 240);

        for (gen_id, top_row) in [(2, 1), (3, 2), (4, 3)] {
            let f_step = build_striped_scroll_frame(
                gen_id,
                top_row,
                vp_y,
                vp_h,
                cell_h,
                header_color,
                modeline_color,
            );
            // SAFETY: `canvas.cr` is valid.
            unsafe {
                ctrl.render_to_cairo(canvas.cr, 200, 260, &f_step, &make_hints(top_row));
            }
            ctrl.step_dt(0.012);
            // SAFETY: `canvas.cr` is valid.
            unsafe {
                ctrl.render_to_cairo(canvas.cr, 200, 260, &f_step, &make_hints(top_row));
            }
            assert_eq!(canvas.read_argb_pixel(100, 10), expected_header_px);
            assert_eq!(canvas.read_argb_pixel(100, 240), expected_modeline_px);
            for y in 20..220 {
                assert_ne!(
                    canvas.read_argb_pixel(100, y),
                    0xFF00_0000,
                    "detected gap during rapid chained scroll at top_row={top_row}, y={y}"
                );
            }
        }
    }

    #[test]
    fn seeded_frame_clock_compresses_dropped_frame_spike_without_lurch() {
        let mut ctrl = AnimController::new();
        let canvas = OffscreenSurface::new(200, 260).expect("surface");
        let hints0 = AnimHints {
            smooth_scroll: true,
            cursor_anim: true,
            viewports: vec![ScrollViewport {
                pane_id: 1,
                doc_id: 10,
                x: 0.0,
                y: 20.0,
                w: 200.0,
                h: 200.0,
                top_row: 0,
                left_col: 0,
                cell_w: 8.0,
                cell_h: 20.0,
            }],
        };
        let mut hints3 = hints0.clone();
        hints3.viewports[0].top_row = 3;
        let bg = ColorRgba::from_rgba(30, 30, 46, 255);
        let f0 = build_striped_scroll_frame(1, 0, 20.0, 200.0, 20.0, bg, bg);
        let f3 = build_striped_scroll_frame(2, 3, 20.0, 200.0, 20.0, bg, bg);
        // SAFETY: `canvas.cr` is valid.
        unsafe {
            ctrl.render_to_cairo(canvas.cr, 200, 260, &f0, &hints0);
            ctrl.render_to_cairo(canvas.cr, 200, 260, &f3, &hints3);
        }
        ctrl.seed_frame_clock(Some(10.000));
        let start_off = ctrl.pane_offset_y(1);
        assert!((start_off - 60.0).abs() < 1e-6);

        // Simulate a 65ms frame hitch: soft compression must keep single-tick progress bounded
        // (< 55% of remaining distance rather than jumping > 78% in one lurch).
        let (active, redraw) = ctrl.tick_at(10.065);
        assert!(active && redraw);
        let after_spike = ctrl.pane_offset_y(1);
        let spike_step = start_off - after_spike;
        assert!(
            spike_step > 12.0 && spike_step < 34.0,
            "expected smooth compressed step on 65ms spike, got {spike_step:.2}px"
        );
    }

    #[test]
    fn gtk_snapshot_swapchain_preserves_inflight_render_node_and_draws_cleanly() {
        let mut ctrl = AnimController::new();
        let header_col = ColorRgba::from_rgba(137, 180, 250, 255);
        let modeline_col = ColorRgba::from_rgba(166, 227, 161, 255);

        let hints0 = AnimHints {
            smooth_scroll: true,
            cursor_anim: true,
            viewports: vec![ScrollViewport {
                pane_id: 1,
                doc_id: 10,
                x: 0.0,
                y: 20.0,
                w: 200.0,
                h: 200.0,
                top_row: 0,
                left_col: 0,
                cell_w: 8.0,
                cell_h: 20.0,
            }],
        };
        let mut hints2 = hints0.clone();
        hints2.viewports[0].top_row = 2;

        let mut f0 = build_striped_scroll_frame(1, 0, 20.0, 200.0, 20.0, header_col, modeline_col);
        f0.commands.push(DrawCmd::CursorCaret {
            x: 16.0,
            y: 40.0,
            w: 8.0,
            h: 20.0,
            shape: CaretShape::Block,
            color: ColorRgba::from_rgba(245, 194, 231, 255),
        });
        let mut f2 = build_striped_scroll_frame(2, 2, 20.0, 200.0, 20.0, header_col, modeline_col);
        f2.commands.push(DrawCmd::CursorCaret {
            x: 80.0,
            y: 80.0,
            w: 8.0,
            h: 20.0,
            shape: CaretShape::Block,
            color: ColorRgba::from_rgba(245, 194, 231, 255),
        });

        // Frame 1: build real `GskRenderNode` while keeping it alive (simulating GTK4 retaining
        // the previous frame's render node while recording the next frame's snapshot).
        // SAFETY: `gtk_snapshot_new` and `gtk_snapshot_free_to_node` are valid headless GSK/GTK4 calls.
        let node0 = unsafe {
            let snap0 = ffi::gtk_snapshot_new();
            assert!(!snap0.is_null());
            ctrl.snapshot_to_gtk(
                SnapshotTarget {
                    snapshot: snap0,
                    width: 200,
                    height: 260,
                    scale: 1.0,
                    frame_clock_sec: Some(1.000),
                },
                &f0,
                &hints0,
                None,
            );
            ffi::gtk_snapshot_free_to_node(snap0)
        };
        assert!(
            !node0.is_null(),
            "expected non-null GskRenderNode for frame 0"
        );
        let before0 = OffscreenSurface::new(200, 260).expect("before0");
        // SAFETY: `node0` and `before0.cr` are valid.
        unsafe {
            ffi::gsk_render_node_draw(node0, before0.cr);
        }

        // Frame 2: scroll + cursor diagonal smear while `node0` is STILL alive! Swapchain must not
        // corrupt `node0` AND must produce a valid composite `GskRenderNode` with texture + smear nodes.
        // SAFETY: `gtk_snapshot_new` and `gtk_snapshot_free_to_node` are valid headless GSK/GTK4 calls.
        let node1 = unsafe {
            let snap1 = ffi::gtk_snapshot_new();
            assert!(!snap1.is_null());
            ctrl.snapshot_to_gtk(
                SnapshotTarget {
                    snapshot: snap1,
                    width: 200,
                    height: 260,
                    scale: 1.0,
                    frame_clock_sec: Some(1.016),
                },
                &f2,
                &hints2,
                None,
            );
            ffi::gtk_snapshot_free_to_node(snap1)
        };
        assert!(
            !node1.is_null(),
            "expected non-null GskRenderNode for scrolled frame 1"
        );

        let after0 = OffscreenSurface::new(200, 260).expect("after0");
        let check1 = OffscreenSurface::new(200, 260).expect("check1");
        // SAFETY: `node0`, `node1`, `after0.cr`, and `check1.cr` are valid; `gsk_render_node_unref` frees both.
        unsafe {
            ffi::gsk_render_node_draw(node0, after0.cr);
            ffi::gsk_render_node_draw(node1, check1.cr);
            ffi::gsk_render_node_unref(node0);
            ffi::gsk_render_node_unref(node1);
        }
        for py in (0..260).step_by(5) {
            for px in (0..200).step_by(7) {
                assert_eq!(
                    before0.read_argb_pixel(px, py),
                    after0.read_argb_pixel(px, py),
                    "in-flight GskRenderNode pixel ({px}, {py}) was mutated by next frame"
                );
            }
        }
        // `node1` static header (`y = 10`) and modeline (`y = 240`) must match `before0` (`1.0x` disjoint chrome)!
        assert_eq!(
            check1.read_argb_pixel(100, 10),
            before0.read_argb_pixel(100, 10)
        );
        assert_eq!(
            check1.read_argb_pixel(100, 240),
            before0.read_argb_pixel(100, 240)
        );
    }

    #[test]
    #[ignore = "manual --release ultrawide 3405x1324 1x/2x HiDPI GTK4 snapshot latency benchmark"]
    fn profile_ultrawide_3405x1324_gtk_snapshot_pipeline() {
        use std::time::Instant;

        let metrics = crate::render::measure_monospace_font("Monospace", 13.0);
        let cw = metrics.cell_width;
        let ch = metrics.cell_height.ceil().max(20.0);
        let win_w: c_int = 3405;
        let win_h: c_int = 1324;
        let visible_rows: usize = 62;
        let pane_y = 28.0;
        let pane_h = (visible_rows as f64) * ch;

        let bg = ColorRgba::from_rgba(30, 30, 46, 255);
        let gutter_fg = ColorRgba::from_rgba(108, 112, 134, 255);
        let kw = ColorRgba::from_rgba(203, 166, 247, 255);
        let ty = ColorRgba::from_rgba(249, 226, 175, 255);
        let ident = ColorRgba::from_rgba(137, 180, 250, 255);
        let op = ColorRgba::from_rgba(137, 220, 235, 255);
        let str_col = ColorRgba::from_rgba(166, 227, 161, 255);
        let cmt = ColorRgba::from_rgba(147, 153, 178, 255);
        let modeline_bg = ColorRgba::from_rgba(24, 24, 37, 255);

        let build_ultrawide_frame = |generation: u64, top_row: usize, edit_col: usize| {
            let mut cmds = Vec::with_capacity(visible_rows * 10 + 16);
            // Top tabbar
            cmds.push(DrawCmd::FillRect {
                x: 0.0,
                y: 0.0,
                w: f64::from(win_w),
                h: pane_y,
                color: modeline_bg,
                radius: 0.0,
            });
            cmds.push(DrawCmd::ClipPush {
                x: 0.0,
                y: pane_y,
                w: f64::from(win_w),
                h: pane_h,
            });
            for vis in 0..visible_rows {
                let doc_row = top_row + vis;
                let y = pane_y + (vis as f64) * ch;
                let ln_str = format!("{:>5} ", doc_row + 1);
                cmds.push(DrawCmd::TextRun {
                    x: 8.0,
                    y,
                    text: ln_str,
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: gutter_fg,
                    bold: false,
                    italic: false,
                    underline: false,
                });
                let x0 = 8.0 + 7.0 * cw;
                cmds.push(DrawCmd::TextRun {
                    x: x0,
                    y,
                    text: "pub fn ".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: kw,
                    bold: true,
                    italic: false,
                    underline: false,
                });
                let fn_name = if doc_row == top_row + 12 {
                    format!("dispatch_key_seq_{doc_row}_{edit_col}")
                } else {
                    format!("render_viewport_row_{doc_row:04}")
                };
                let fn_len = fn_name.len() as f64;
                cmds.push(DrawCmd::TextRun {
                    x: x0 + 7.0 * cw,
                    y,
                    text: fn_name,
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: ident,
                    bold: false,
                    italic: false,
                    underline: false,
                });
                let sig = "(ctx: &mut RenderCtx, row_idx: Int) -> Result[Bool, UiError] { ";
                let sig_len = sig.len() as f64;
                cmds.push(DrawCmd::TextRun {
                    x: x0 + (7.0 + fn_len) * cw,
                    y,
                    text: sig.to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: ty,
                    bold: false,
                    italic: false,
                    underline: false,
                });
                let body = "let ok = ctx.flush_gpu_slice(row_idx * 2 + 1); ";
                let body_len = body.len() as f64;
                cmds.push(DrawCmd::TextRun {
                    x: x0 + (7.0 + fn_len + sig_len) * cw,
                    y,
                    text: body.to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: op,
                    bold: false,
                    italic: false,
                    underline: false,
                });
                let lit = "\"ultrawide_fast_path\" ";
                let lit_len = lit.len() as f64;
                cmds.push(DrawCmd::TextRun {
                    x: x0 + (7.0 + fn_len + sig_len + body_len) * cw,
                    y,
                    text: lit.to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: str_col,
                    bold: false,
                    italic: false,
                    underline: false,
                });
                cmds.push(DrawCmd::TextRun {
                    x: x0 + (7.0 + fn_len + sig_len + body_len + lit_len) * cw,
                    y,
                    text: format!("// 渲染缓存行 #{doc_row}"),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: cmt,
                    bold: false,
                    italic: true,
                    underline: false,
                });
            }
            cmds.push(DrawCmd::ClipPop);
            // Bottom modeline
            cmds.push(DrawCmd::FillRect {
                x: 0.0,
                y: pane_y + pane_h,
                w: f64::from(win_w),
                h: f64::from(win_h) - (pane_y + pane_h),
                color: modeline_bg,
                radius: 0.0,
            });
            cmds.push(DrawCmd::CursorCaret {
                x: 8.0 + (22.0 + (edit_col % 40) as f64) * cw,
                y: pane_y + 12.0 * ch,
                w: 2.0,
                h: ch,
                shape: CaretShape::Beam,
                color: ColorRgba::from_rgba(245, 194, 231, 255),
            });

            let hints = AnimHints {
                smooth_scroll: true,
                cursor_anim: true,
                viewports: vec![ScrollViewport {
                    pane_id: 1,
                    doc_id: 42,
                    x: 0.0,
                    y: pane_y,
                    w: f64::from(win_w),
                    h: pane_h,
                    top_row: top_row as i64,
                    left_col: 0,
                    cell_w: cw,
                    cell_h: ch,
                }],
            };
            (
                RenderFrame {
                    generation,
                    window: WindowConfig {
                        title: "Ultrawide Profile".to_string(),
                        width: win_w,
                        height: win_h,
                        background: bg,
                        ..WindowConfig::default()
                    },
                    commands: cmds,
                },
                hints,
            )
        };

        for scale in [1.0_f64, 1.5_f64, 2.0_f64] {
            crate::render::reset_row_raster_cache_stats();
            let mut ctrl = AnimController::new();
            let mut held_prev_node: *mut ffi::GskRenderNode = ptr::null_mut();
            let run_snap = |ctrl: &mut AnimController,
                            held_prev_node: &mut *mut ffi::GskRenderNode,
                            frame: &RenderFrame,
                            hints: &AnimHints,
                            t_sec: f64|
             -> std::time::Duration {
                let t0 = Instant::now();
                // SAFETY: Headless `GtkSnapshot` -> `GskRenderNode` lifecycle simulating GTK4
                // holding the previous frame's render node while recording the new snapshot.
                let new_node = unsafe {
                    let snap = ffi::gtk_snapshot_new();
                    ctrl.snapshot_to_gtk(
                        SnapshotTarget {
                            snapshot: snap,
                            width: win_w,
                            height: win_h,
                            scale,
                            frame_clock_sec: Some(t_sec),
                        },
                        frame,
                        hints,
                        None,
                    );
                    ffi::gtk_snapshot_free_to_node(snap)
                };
                let dt = t0.elapsed();
                if !(*held_prev_node).is_null() {
                    // SAFETY: `*held_prev_node` was returned by `gtk_snapshot_free_to_node`.
                    unsafe {
                        ffi::gsk_render_node_unref(*held_prev_node);
                    }
                }
                *held_prev_node = new_node;
                dt
            };

            let mut frame_gen: u64 = 1;
            let mut now = 1.000_f64;
            let (f_cold, h_cold) = build_ultrawide_frame(frame_gen, 0, 0);
            let cold_ms = run_snap(&mut ctrl, &mut held_prev_node, &f_cold, &h_cold, now)
                .as_secs_f64()
                * 1000.0;

            // 1. Rapid continuous smooth scrolling (30 consecutive 1-line scrolls, mid-scroll chaining!)
            let mut scroll_new_frame_ms = Vec::with_capacity(30);
            let mut scroll_vsync_tick_ms = Vec::with_capacity(30);
            for step in 1..=30usize {
                frame_gen += 1;
                now += 0.008;
                let (f_sc, h_sc) = build_ultrawide_frame(frame_gen, step, 0);
                scroll_new_frame_ms.push(
                    run_snap(&mut ctrl, &mut held_prev_node, &f_sc, &h_sc, now).as_secs_f64()
                        * 1000.0,
                );

                // Intermediate VSync tick (no new Nessa frame, pure GPU slice shift + cursor smear)
                now += 0.008;
                let t_tick = Instant::now();
                let _ = ctrl.tick_at(now);
                let tick_dt =
                    t_tick.elapsed() + run_snap(&mut ctrl, &mut held_prev_node, &f_sc, &h_sc, now);
                scroll_vsync_tick_ms.push(tick_dt.as_secs_f64() * 1000.0);
            }

            // Settle scroll before measuring Insert-mode typing
            for _ in 0..30 {
                now += 0.016;
                let (active, _) = ctrl.tick_at(now);
                if !active {
                    break;
                }
            }
            let (f_settled, h_settled) = build_ultrawide_frame(frame_gen, 30, 0);
            let _ = run_snap(&mut ctrl, &mut held_prev_node, &f_settled, &h_settled, now);

            // 2. Rapid Insert-mode typing (30 keystrokes @ 25 keys/s on ultrawide screen)
            let mut insert_frame_ms = Vec::with_capacity(30);
            let mut smear_vsync_ms = Vec::with_capacity(30);
            for k in 1..=30usize {
                frame_gen += 1;
                now += 0.008;
                let (f_ed, h_ed) = build_ultrawide_frame(frame_gen, 30, k);
                insert_frame_ms.push(
                    run_snap(&mut ctrl, &mut held_prev_node, &f_ed, &h_ed, now).as_secs_f64()
                        * 1000.0,
                );

                now += 0.008;
                let t_tick = Instant::now();
                let _ = ctrl.tick_at(now);
                let smear_dt =
                    t_tick.elapsed() + run_snap(&mut ctrl, &mut held_prev_node, &f_ed, &h_ed, now);
                smear_vsync_ms.push(smear_dt.as_secs_f64() * 1000.0);
            }

            if !held_prev_node.is_null() {
                // SAFETY: Release final retained `GskRenderNode`.
                unsafe {
                    ffi::gsk_render_node_unref(held_prev_node);
                }
            }

            let avg = |xs: &[f64]| xs.iter().sum::<f64>() / (xs.len() as f64);
            let p95 = |xs: &[f64]| {
                let mut v = xs.to_vec();
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                v[((v.len() * 95) / 100).min(v.len() - 1)]
            };
            let (hits, misses, cached_entries) = crate::render::row_raster_cache_stats();
            println!(
                "[ULTRAWIDE {}x{} @{:.0}x] cold={:.2}ms | scroll_step avg={:.2}ms p95={:.2}ms | scroll_vsync_tick avg={:.3}ms p95={:.3}ms | insert_edit avg={:.2}ms p95={:.2}ms | cursor_vsync_tick avg={:.3}ms p95={:.3}ms | RowCache hits={} misses={} entries={}",
                win_w,
                win_h,
                scale,
                cold_ms,
                avg(&scroll_new_frame_ms),
                p95(&scroll_new_frame_ms),
                avg(&scroll_vsync_tick_ms),
                p95(&scroll_vsync_tick_ms),
                avg(&insert_frame_ms),
                p95(&insert_frame_ms),
                avg(&smear_vsync_ms),
                p95(&smear_vsync_ms),
                hits,
                misses,
                cached_entries
            );

            // Hard regression gates even at 3405x1324 @2x HiDPI (6810x2648 = 72 MB framebuffer!):
            assert!(
                avg(&scroll_vsync_tick_ms) < 0.5,
                "VSync scroll tick must stay sub-0.5ms (zero bitmap copy), got {:.3}ms",
                avg(&scroll_vsync_tick_ms)
            );
            assert!(
                avg(&smear_vsync_ms) < 0.5,
                "VSync cursor smear tick must stay sub-0.5ms, got {:.3}ms",
                avg(&smear_vsync_ms)
            );
        }
    }
}
