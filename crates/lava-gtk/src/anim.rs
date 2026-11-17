//! Neovide-style 4-corner cursor smear physics, smooth viewport split-clip scrolling,
//! offscreen base-frame surface caching, and touchpad fractional scroll accumulation.

use std::collections::HashMap;
use std::rc::Rc;

use lava_proto::{AnimHints, CaretShape, ColorRgba, DrawCmd, RenderFrame, ScrollViewport};
use libc::c_int;

use crate::ffi::{self, CairoSurfaceT, CairoT};
use crate::render::{
    OffscreenSurface, draw_caret, query_cairo_device_scale, render_frame_to_cairo_with_opts,
};

/// Leading-corner exponential decay time constant in seconds (~24 ms).
pub(crate) const TAU_CURSOR_LEAD: f64 = 0.024;
/// Trailing-corner exponential decay time constant for jumps > 2 cells (~62 ms).
pub(crate) const TAU_CURSOR_TRAIL: f64 = 0.062;
/// Trailing-corner exponential decay time constant for <= 2-cell horizontal typing (~38 ms).
pub(crate) const TAU_CURSOR_TRAIL_TYPING: f64 = 0.038;
/// Smooth viewport vertical scroll decay time constant in seconds (~42 ms).
pub(crate) const TAU_SCROLL_DECAY: f64 = 0.042;
/// Sub-pixel convergence threshold below which animations snap to target and stop VSync wakeups.
pub(crate) const SNAP_EPSILON_PX: f64 = 0.2;

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
        let tau_trail = if self.short_typing_hop {
            TAU_CURSOR_TRAIL_TYPING
        } else {
            TAU_CURSOR_TRAIL
        };
        let lead_decay = (-dt / TAU_CURSOR_LEAD).exp();
        let trail_decay = (-dt / tau_trail).exp();

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

    /// Renders the 4-corner smear polygon + crisp cursor head onto `cr`.
    ///
    /// # Safety
    /// `cr` must be a valid non-null `*mut CairoT` owned on the current thread.
    pub(crate) unsafe fn draw(&self, cr: *mut CairoT) {
        let Some(caret) = self.target else {
            return;
        };
        if self.settled {
            draw_caret(
                cr,
                caret.x,
                caret.y,
                caret.w,
                caret.h,
                caret.shape,
                caret.color,
            );
            return;
        }

        let (r, g, b, a) = caret.color.components_f64();
        let smear_alpha = (a * 0.72).clamp(0.18, 0.88);
        // SAFETY: `cr` is valid per caller invariant; builds and fills the closed 4-corner polygon.
        unsafe {
            ffi::cairo_set_source_rgba(cr, r, g, b, smear_alpha);
            ffi::cairo_new_path(cr);
            ffi::cairo_move_to(cr, self.corners[0].0, self.corners[0].1);
            ffi::cairo_line_to(cr, self.corners[1].0, self.corners[1].1);
            ffi::cairo_line_to(cr, self.corners[2].0, self.corners[2].1);
            ffi::cairo_line_to(cr, self.corners[3].0, self.corners[3].1);
            ffi::cairo_close_path(cr);
            ffi::cairo_fill(cr);
        }

        draw_caret(
            cr,
            caret.x + self.head_offset.0,
            caret.y + self.head_offset.1,
            caret.w,
            caret.h,
            caret.shape,
            caret.color,
        );
    }
}

struct PaneScrollState {
    vp: ScrollViewport,
    offset_y_px: f64,
    jump_px: f64,
    outgoing: Option<Rc<OffscreenSurface>>,
}

impl PaneScrollState {
    fn is_active(&self) -> bool {
        self.outgoing.is_some() && self.offset_y_px.abs() >= SNAP_EPSILON_PX
    }

    fn matches_geometry(&self, next: &ScrollViewport) -> bool {
        self.vp.doc_id == next.doc_id
            && (self.vp.x - next.x).abs() < 0.5
            && (self.vp.y - next.y).abs() < 0.5
            && (self.vp.w - next.w).abs() < 0.5
            && (self.vp.h - next.h).abs() < 0.5
    }
}

#[derive(Debug, Clone, Copy)]
struct ClipRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Blits `src_surface` shifted vertically by `shift_y` inside the device-space `clip` rectangle,
/// snapping both the clip and vertical shift to device pixels with `SOURCE` + `NEAREST` so Cairo
/// hits the fast `memcpy`/`pixman_blt` path instead of software bilinear resampling.
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
    let sx = if scale_x.is_finite() && scale_x > 0.0 {
        scale_x
    } else {
        1.0
    };
    let sy = if scale_y.is_finite() && scale_y > 0.0 {
        scale_y
    } else {
        1.0
    };
    let clip_x0 = (clip.x * sx).floor() / sx;
    let clip_y0 = (clip.y * sy).floor() / sy;
    let clip_x1 = ((clip.x + clip.w) * sx).ceil() / sx;
    let clip_y1 = ((clip.y + clip.h) * sy).ceil() / sy;
    let snapped_shift_y = (shift_y * sy).round() / sy;

    // SAFETY: Saves Cairo state, clips on device-pixel boundaries, blits `src_surface` with
    // `CAIRO_OPERATOR_SOURCE` + `CAIRO_FILTER_NEAREST` at device-snapped `snapped_shift_y`, and
    // restores balanced Cairo state.
    unsafe {
        ffi::cairo_save(cr);
        ffi::cairo_new_path(cr);
        ffi::cairo_rectangle(
            cr,
            clip_x0,
            clip_y0,
            (clip_x1 - clip_x0).max(0.0),
            (clip_y1 - clip_y0).max(0.0),
        );
        ffi::cairo_clip(cr);
        ffi::cairo_set_operator(cr, ffi::CAIRO_OPERATOR_SOURCE);
        ffi::cairo_set_source_surface(cr, src_surface, 0.0, snapped_shift_y);
        ffi::cairo_paint(cr);
        ffi::cairo_restore(cr);
    }
}

/// Coordinates base-surface caching, smooth per-pane scrolling, and 4-corner cursor animation.
pub(crate) struct AnimController {
    base_surface: Option<OffscreenSurface>,
    surface_pool: Vec<OffscreenSurface>,
    cached_gen: u64,
    cached_skip_caret: Option<usize>,
    cursor: CursorPhysics,
    panes: HashMap<i64, PaneScrollState>,
    last_tick_sec: Option<f64>,
    needs_snap_redraw: bool,
}

impl Default for AnimController {
    fn default() -> Self {
        Self::new()
    }
}

impl AnimController {
    pub(crate) fn new() -> Self {
        Self {
            base_surface: None,
            surface_pool: Vec::new(),
            cached_gen: 0,
            cached_skip_caret: None,
            cursor: CursorPhysics::new(),
            panes: HashMap::new(),
            last_tick_sec: None,
            needs_snap_redraw: false,
        }
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
        }) {
            return Ok(self.surface_pool.swap_remove(pos));
        }
        OffscreenSurface::new_scaled(safe_w, safe_h, scale_x, scale_y)
    }

    fn recycle_rc_surface(pool: &mut Vec<OffscreenSurface>, surf: Rc<OffscreenSurface>) {
        if let Ok(owned) = Rc::try_unwrap(surf)
            && pool.len() < 3
        {
            pool.push(owned);
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
            Some(prev) if now_sec >= prev => (now_sec - prev).clamp(1.0 / 240.0, 0.05),
            _ => 1.0 / 60.0,
        };
        self.last_tick_sec = Some(now_sec);

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
            if pane.outgoing.is_some() {
                pane.offset_y_px *= scroll_decay;
                if pane.offset_y_px.abs() < SNAP_EPSILON_PX {
                    pane.offset_y_px = 0.0;
                    pane.jump_px = 0.0;
                    if let Some(old) = pane.outgoing.take() {
                        Self::recycle_rc_surface(&mut self.surface_pool, old);
                    }
                }
            }
        }
    }

    /// Composites `self.base_surface` plus any active pane split-clip scroll transitions onto `cr`.
    ///
    /// # Safety
    /// `cr` must be a valid non-null `*mut CairoT` on the current thread.
    unsafe fn composite_scroll_layers(&self, cr: *mut CairoT) {
        let Some(base) = self.base_surface.as_ref() else {
            return;
        };
        // SAFETY: `cr` is valid per caller invariant.
        let (scale_x, scale_y) = unsafe { query_cairo_device_scale(cr) };
        // SAFETY: `cr` and `base.surface` are valid on this thread; paints the full base frame
        // (including static tab bar, borders, status/mode-lines, and inactive panes) using
        // `CAIRO_OPERATOR_SOURCE` + `CAIRO_FILTER_NEAREST` for direct blit speed.
        unsafe {
            ffi::cairo_save(cr);
            ffi::cairo_set_operator(cr, ffi::CAIRO_OPERATOR_SOURCE);
            ffi::cairo_set_source_surface(cr, base.surface, 0.0, 0.0);
            ffi::cairo_paint(cr);
            ffi::cairo_restore(cr);
        }

        for pane in self.panes.values() {
            let Some(outgoing) = pane.outgoing.as_ref() else {
                continue;
            };
            let offset_y = pane.offset_y_px;
            let vp = &pane.vp;
            if offset_y.abs() < SNAP_EPSILON_PX || vp.w <= 0.0 || vp.h <= 0.0 {
                continue;
            }

            if offset_y > 0.0 {
                // Scrolling down (`new_top > old_top`): content glides upward.
                // Top `[vp.y, vp.y + split]` blits outgoing snapshot shifted by `offset_y - jump_px`;
                // Bottom `[vp.y + split, vp.y + vp.h]` blits new base surface shifted by `+offset_y`.
                let split_clip = offset_y.ceil().clamp(0.0, vp.h);
                let bottom_h = (vp.h - split_clip).max(0.0);
                // SAFETY: `cr`, `outgoing.surface`, and `base.surface` are valid on this thread.
                unsafe {
                    blit_clipped_shifted(
                        cr,
                        outgoing.surface,
                        ClipRect {
                            x: vp.x,
                            y: vp.y,
                            w: vp.w,
                            h: split_clip,
                        },
                        offset_y - pane.jump_px,
                        scale_x,
                        scale_y,
                    );
                    blit_clipped_shifted(
                        cr,
                        base.surface,
                        ClipRect {
                            x: vp.x,
                            y: vp.y + split_clip,
                            w: vp.w,
                            h: bottom_h,
                        },
                        offset_y,
                        scale_x,
                        scale_y,
                    );
                }
            } else {
                // Scrolling up (`new_top < old_top`, `offset_y < 0`): content glides downward.
                // Top `[vp.y, vp.y + top_h]` blits new base surface shifted by `+offset_y`;
                // Bottom `[vp.y + top_h, vp.y + vp.h]` blits outgoing snapshot shifted by `offset_y - jump_px`.
                let bottom_clip = (-offset_y).ceil().clamp(0.0, vp.h);
                let top_h = (vp.h - bottom_clip).max(0.0);
                // SAFETY: `cr`, `outgoing.surface`, and `base.surface` are valid on this thread.
                unsafe {
                    blit_clipped_shifted(
                        cr,
                        base.surface,
                        ClipRect {
                            x: vp.x,
                            y: vp.y,
                            w: vp.w,
                            h: top_h,
                        },
                        offset_y,
                        scale_x,
                        scale_y,
                    );
                    blit_clipped_shifted(
                        cr,
                        outgoing.surface,
                        ClipRect {
                            x: vp.x,
                            y: vp.y + top_h,
                            w: vp.w,
                            h: bottom_clip,
                        },
                        offset_y - pane.jump_px,
                        scale_x,
                        scale_y,
                    );
                }
            }
        }
    }

    fn update_scroll_viewports(
        &mut self,
        safe_w: c_int,
        safe_h: c_int,
        scale_x: f64,
        scale_y: f64,
        hints: &AnimHints,
        resized: bool,
    ) {
        if !hints.smooth_scroll || resized {
            for (_, mut prev) in self.panes.drain() {
                if let Some(old) = prev.outgoing.take() {
                    Self::recycle_rc_surface(&mut self.surface_pool, old);
                }
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
                            jump_px: 0.0,
                            outgoing: None,
                        },
                    );
                }
            }
            return;
        }

        // Capture a single composited snapshot of the outgoing visual scene only if at least one
        // pane viewport scrolled on the same document & dimensions.
        let any_scrolled = self.base_surface.is_some()
            && hints.viewports.iter().any(|vp| {
                vp.w > 1.0
                    && vp.h > 1.0
                    && vp.cell_h > 1.0
                    && self.panes.get(&vp.pane_id).is_some_and(|prev| {
                        prev.matches_geometry(vp) && prev.vp.top_row != vp.top_row
                    })
            });

        let outgoing_scene: Option<Rc<OffscreenSurface>> = if any_scrolled {
            let any_active_scroll = self.panes.values().any(PaneScrollState::is_active);
            if !any_active_scroll {
                // Fast path: no pane is mid-scroll, so `self.base_surface` already holds the
                // exact outgoing scene. Promote it directly without allocating or copying!
                self.base_surface.take().map(Rc::new)
            } else if let Ok(snap) = self.take_or_alloc_surface(safe_w, safe_h, scale_x, scale_y) {
                // SAFETY: `snap.cr` is a valid uniquely owned offscreen Cairo context.
                unsafe {
                    self.composite_scroll_layers(snap.cr);
                    ffi::cairo_surface_flush(snap.surface);
                }
                Some(Rc::new(snap))
            } else {
                None
            }
        } else {
            None
        };
        let shared_snapshot_needed = outgoing_scene.is_some();

        let mut next_panes = HashMap::with_capacity(hints.viewports.len());
        for vp in &hints.viewports {
            if let Some(mut prev) = self.panes.remove(&vp.pane_id)
                && prev.matches_geometry(vp)
                && vp.w > 1.0
                && vp.h > 1.0
                && vp.cell_h > 1.0
            {
                if prev.vp.top_row != vp.top_row && shared_snapshot_needed {
                    let delta_rows = (vp.top_row - prev.vp.top_row) as f64;
                    let max_off = (vp.h * 0.95).max(1.0);
                    let next_off =
                        (prev.offset_y_px + delta_rows * vp.cell_h).clamp(-max_off, max_off);
                    if let Some(old) = prev.outgoing.take() {
                        Self::recycle_rc_surface(&mut self.surface_pool, old);
                    }
                    prev.vp = *vp;
                    if next_off.abs() >= SNAP_EPSILON_PX {
                        prev.offset_y_px = next_off;
                        prev.jump_px = next_off;
                        prev.outgoing = outgoing_scene.as_ref().map(Rc::clone);
                    } else {
                        prev.offset_y_px = 0.0;
                        prev.jump_px = 0.0;
                    }
                } else {
                    // Same `top_row`, `doc_id`, and dimensions: preserve any active decay!
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
                    jump_px: 0.0,
                    outgoing: None,
                },
            );
        }
        for (_, mut stale) in self.panes.drain() {
            if let Some(old) = stale.outgoing.take() {
                Self::recycle_rc_surface(&mut self.surface_pool, old);
            }
        }
        self.panes = next_panes;
        if let Some(scene) = outgoing_scene {
            Self::recycle_rc_surface(&mut self.surface_pool, scene);
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

    pub(crate) unsafe fn render_to_cairo_scaled(
        &mut self,
        cr: *mut CairoT,
        width: c_int,
        height: c_int,
        min_scale: f64,
        frame: &RenderFrame,
        hints: &AnimHints,
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

        let primary_caret = find_primary_caret(frame);
        let skip_caret_idx = if hints.cursor_anim {
            primary_caret.map(|c| c.cmd_idx)
        } else {
            None
        };

        let resized = self.base_surface.as_ref().is_some_and(|b| {
            b.width != safe_w
                || b.height != safe_h
                || (b.device_scale_x - scale_x).abs() > 1e-4
                || (b.device_scale_y - scale_y).abs() > 1e-4
        });
        let frame_changed = resized
            || self.base_surface.is_none()
            || frame.generation != self.cached_gen
            || skip_caret_idx != self.cached_skip_caret;

        if frame_changed {
            self.update_scroll_viewports(safe_w, safe_h, scale_x, scale_y, hints, resized);

            let surface_ready = if resized || self.base_surface.is_none() {
                if resized {
                    self.base_surface = None;
                }
                match self.take_or_alloc_surface(safe_w, safe_h, scale_x, scale_y) {
                    Ok(surf) => {
                        self.base_surface = Some(surf);
                        true
                    }
                    Err(_) => {
                        self.base_surface = None;
                        false
                    }
                }
            } else {
                true
            };

            if surface_ready && let Some(base) = self.base_surface.as_ref() {
                // SAFETY: `base.cr` and `base.surface` are uniquely owned on this thread.
                unsafe {
                    render_frame_to_cairo_with_opts(
                        base.cr,
                        f64::from(safe_w),
                        f64::from(safe_h),
                        frame,
                        skip_caret_idx,
                    );
                    ffi::cairo_surface_flush(base.surface);
                }
                self.cached_gen = frame.generation;
                self.cached_skip_caret = skip_caret_idx;
            }

            if hints.cursor_anim {
                self.cursor.set_target(primary_caret, true);
            } else {
                self.cursor.set_target(None, false);
            }
        } else {
            // Even if `frame.generation` was unchanged, respect live `AnimHints` toggles.
            if !hints.smooth_scroll && !self.panes.is_empty() {
                self.panes.clear();
            }
            if !hints.cursor_anim && self.cursor.is_active() {
                self.cursor.set_target(primary_caret, false);
            }
        }

        if self.base_surface.is_some() {
            // SAFETY: `cr` is valid per caller invariant.
            unsafe {
                self.composite_scroll_layers(cr);
                if hints.cursor_anim {
                    self.cursor.draw(cr);
                }
            }
        } else {
            // Fallback direct render if offscreen allocation failed.
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
    use lava_proto::{
        AnimHints, CaretShape, ColorRgba, DrawCmd, RenderFrame, ScrollViewport, WindowConfig,
    };

    use super::{
        AnimController, CaretTarget, CursorPhysics, SNAP_EPSILON_PX, ScrollDeltaAccumulator,
        find_primary_caret,
    };
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
        let base = ctrl.base_surface.as_ref().expect("base_surface allocated");
        assert_eq!(base.width, 200);
        assert_eq!(base.height, 160);
        assert!((base.device_scale_x - 2.0).abs() < 1e-4);
        assert!((base.device_scale_y - 2.0).abs() < 1e-4);
    }
}
