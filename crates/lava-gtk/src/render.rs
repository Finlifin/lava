//! Cairo + PangoCairo 2D scene renderer, offscreen PNG export, and monospace font metrics.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::f64::consts::{FRAC_PI_2, PI};
use std::ffi::CString;
use std::hash::{Hash, Hasher};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use lava_proto::{CaretShape, ColorRgba, DrawCmd, FontMetrics, RenderFrame};
use libc::{c_int, c_void};

use crate::ffi::{
    self, CAIRO_ANTIALIAS_GRAY, CAIRO_ARGB32_GDK_MEMORY_FORMAT, CAIRO_FILTER_NEAREST,
    CAIRO_FORMAT_ARGB32, CAIRO_HINT_METRICS_OFF, CAIRO_STATUS_SUCCESS, CairoSurfaceT, CairoT,
    GdkTexture, PANGO_SCALE, PANGO_STYLE_ITALIC, PANGO_STYLE_NORMAL, PANGO_WEIGHT_BOLD,
    PANGO_WEIGHT_NORMAL, PangoFontDescription, PangoLayout, PangoRectangle,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FontKey {
    family: String,
    size_pango: c_int,
    bold: bool,
    italic: bool,
}

#[derive(Clone, Copy)]
struct CachedFont {
    desc: *mut PangoFontDescription,
    cell_width: f64,
    cell_height: f64,
    baseline_px: f64,
}

struct ThreadFontCache {
    fonts: HashMap<FontKey, CachedFont>,
}

impl ThreadFontCache {
    fn new() -> Self {
        Self {
            fonts: HashMap::new(),
        }
    }

    fn clear_all(&mut self) {
        for (_, cached) in self.fonts.drain() {
            if !cached.desc.is_null() {
                // SAFETY: Every `desc` stored in `ThreadFontCache` was allocated by
                // `pango_font_description_new` on this thread and is uniquely owned by the cache.
                unsafe {
                    ffi::pango_font_description_free(cached.desc);
                }
            }
        }
    }
}

impl Drop for ThreadFontCache {
    fn drop(&mut self) {
        self.clear_all();
    }
}

thread_local! {
    static THREAD_PANGO_FONTS: RefCell<ThreadFontCache> = RefCell::new(ThreadFontCache::new());
    static THREAD_ROW_RASTER_CACHE: RefCell<ThreadRowRasterCache> =
        RefCell::new(ThreadRowRasterCache::new());
}

const MAX_ROW_CACHE_ENTRIES: usize = 2_048;
const MAX_ROW_CACHE_BYTES: usize = 96 * 1024 * 1024;
const MAX_CACHED_ROW_TEXT_BYTES: usize = 1_024;
const MAX_CACHED_STRIP_WIDTH_PX: c_int = 4_096;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RowStyleKey {
    Single { color: ColorRgba, italic: bool },
    Spans(Vec<(u32, u32, ColorRgba, bool)>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RowRasterKey {
    family: String,
    size_pango: c_int,
    bold: bool,
    scale_x_milli: u32,
    scale_y_milli: u32,
    text: String,
    style: RowStyleKey,
}

struct RowStripSurface {
    surface: *mut CairoSurfaceT,
    phys_w: c_int,
    phys_h: c_int,
    left_pad_px: c_int,
    top_pad_px: c_int,
    byte_size: usize,
    last_used_seq: u64,
}

impl Drop for RowStripSurface {
    fn drop(&mut self) {
        if !self.surface.is_null() {
            // SAFETY: `self.surface` was created via `cairo_image_surface_create` and its temporary
            // `cairo_t` was destroyed immediately after rasterizing the glyph strip.
            unsafe {
                ffi::cairo_surface_destroy(self.surface);
            }
        }
    }
}

struct ThreadRowRasterCache {
    entries: HashMap<RowRasterKey, RowStripSurface>,
    access_seq: u64,
    total_bytes: usize,
    #[cfg(test)]
    hits: u64,
    #[cfg(test)]
    misses: u64,
}

impl ThreadRowRasterCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            access_seq: 0,
            total_bytes: 0,
            #[cfg(test)]
            hits: 0,
            #[cfg(test)]
            misses: 0,
        }
    }

    fn insert(&mut self, key: RowRasterKey, mut strip: RowStripSurface) {
        if strip.byte_size > MAX_ROW_CACHE_BYTES / 4 {
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(old.byte_size);
        }
        if self.entries.len() >= MAX_ROW_CACHE_ENTRIES
            || self.total_bytes + strip.byte_size > MAX_ROW_CACHE_BYTES
        {
            let target_entries = (MAX_ROW_CACHE_ENTRIES * 4) / 5;
            let target_bytes = ((MAX_ROW_CACHE_BYTES * 4) / 5).saturating_sub(strip.byte_size);
            let mut order: Vec<(u64, RowRasterKey)> = self
                .entries
                .iter()
                .map(|(k, v)| (v.last_used_seq, k.clone()))
                .collect();
            order.sort_unstable_by_key(|(seq, _)| *seq);
            for (_, victim_key) in order {
                if self.entries.len() <= target_entries && self.total_bytes <= target_bytes {
                    break;
                }
                if let Some(evicted) = self.entries.remove(&victim_key) {
                    self.total_bytes = self.total_bytes.saturating_sub(evicted.byte_size);
                }
            }
        }
        self.access_seq = self.access_seq.wrapping_add(1);
        strip.last_used_seq = self.access_seq;
        self.total_bytes += strip.byte_size;
        self.entries.insert(key, strip);
    }
}

#[cfg(test)]
pub(crate) fn row_raster_cache_stats() -> (u64, u64, usize) {
    THREAD_ROW_RASTER_CACHE.with(|cell| {
        let cache = cell.borrow();
        (cache.hits, cache.misses, cache.entries.len())
    })
}

#[cfg(test)]
pub(crate) fn reset_row_raster_cache_stats() {
    THREAD_ROW_RASTER_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        cache.entries.clear();
        cache.access_seq = 0;
        cache.total_bytes = 0;
        cache.hits = 0;
        cache.misses = 0;
    });
}

/// RAII scope owning a frame's `PangoLayout` while reusing persistent thread-local
/// `PangoFontDescription`s and monospace metrics across frames.
struct PangoFrameScope {
    layout: *mut PangoLayout,
    active_key: Option<FontKey>,
    active_metrics: (f64, f64, f64),
}

impl PangoFrameScope {
    fn new(cr: *mut CairoT) -> Self {
        // SAFETY: `cr` is a valid non-null Cairo context owned by the caller on the current thread;
        // `pango_cairo_create_layout` returns a newly owned `GObject` reference (or null on OOM).
        // Forces `CAIRO_ANTIALIAS_GRAY` + `CAIRO_HINT_METRICS_OFF` (matching GTK4's native GSK text
        // pipeline) so transparent strip premultiplication is exact (`R,G,B <= A`) and glyph
        // advances never quantize differently across `1.0x`, `1.25x`, `1.5x`, and `2.0x` scales.
        let layout = unsafe {
            let l = ffi::pango_cairo_create_layout(cr);
            if !l.is_null() {
                let opts = ffi::cairo_font_options_create();
                if !opts.is_null() {
                    ffi::cairo_font_options_set_antialias(opts, CAIRO_ANTIALIAS_GRAY);
                    ffi::cairo_font_options_set_hint_metrics(opts, CAIRO_HINT_METRICS_OFF);
                    ffi::cairo_set_font_options(cr, opts);
                    let pango_ctx = ffi::pango_layout_get_context(l);
                    if !pango_ctx.is_null() {
                        ffi::pango_cairo_context_set_font_options(pango_ctx, opts);
                    }
                    ffi::cairo_font_options_destroy(opts);
                }
            }
            l
        };
        Self {
            layout,
            active_key: None,
            active_metrics: (8.4, 19.0, 12.0),
        }
    }

    fn configure_layout_full(
        &mut self,
        family: &str,
        size_pt: f64,
        bold: bool,
        italic: bool,
    ) -> Option<(*mut PangoLayout, f64, f64, f64, c_int)> {
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

        if let Some(ref ak) = self.active_key
            && ak.size_pango == size_pango
            && ak.bold == bold
            && ak.italic == italic
            && ak.family == normalized_family
        {
            return Some((
                self.layout,
                self.active_metrics.0,
                self.active_metrics.1,
                self.active_metrics.2,
                size_pango,
            ));
        }

        let key = FontKey {
            family: normalized_family.to_string(),
            size_pango,
            bold,
            italic,
        };

        let cached_opt = THREAD_PANGO_FONTS.with(|cell| {
            let mut cache = cell.borrow_mut();
            if let Some(&cached) = cache.fonts.get(&key) {
                return Some(cached);
            }
            if cache.fonts.len() >= 128 {
                cache.clear_all();
            }
            let desc_ptr = create_font_description(normalized_family, size_pango, bold, italic)?;
            let mut logical = PangoRectangle::default();
            // SAFETY: `self.layout` and `desc_ptr` are valid non-null Pango objects on this thread;
            // `"M"` is a 1-byte static UTF-8 slice and `logical` is a valid stack out-pointer.
            let baseline_pango = unsafe {
                ffi::pango_layout_set_font_description(self.layout, desc_ptr);
                ffi::pango_layout_set_text(self.layout, b"M".as_ptr().cast(), 1);
                ffi::pango_layout_get_extents(self.layout, ptr::null_mut(), &mut logical);
                ffi::pango_layout_get_baseline(self.layout)
            };

            let scale = f64::from(PANGO_SCALE);
            let raw_cw = f64::from(logical.width) / scale;
            let cell_width = if raw_cw > 0.0 { raw_cw } else { 8.4 };
            let raw_ch = f64::from(logical.height) / scale;
            let cell_height = if raw_ch > 0.0 {
                raw_ch
            } else {
                clamped_pt * 1.4
            };
            let raw_base = f64::from(baseline_pango) / scale;
            let baseline_px = if raw_base > 0.0 { raw_base } else { clamped_pt };

            let entry = CachedFont {
                desc: desc_ptr,
                cell_width,
                cell_height,
                baseline_px,
            };
            cache.fonts.insert(key.clone(), entry);
            Some(entry)
        })?;

        // SAFETY: `self.layout` and `cached_opt.desc` are valid non-null Pango objects on this
        // thread; Pango copies/references the font description internally.
        unsafe {
            ffi::pango_layout_set_font_description(self.layout, cached_opt.desc);
        }
        self.active_key = Some(key);
        self.active_metrics = (
            cached_opt.cell_width,
            cached_opt.cell_height,
            cached_opt.baseline_px,
        );
        Some((
            self.layout,
            cached_opt.cell_width,
            cached_opt.cell_height,
            cached_opt.baseline_px,
            size_pango,
        ))
    }

    fn configure_layout(
        &mut self,
        family: &str,
        size_pt: f64,
        bold: bool,
        italic: bool,
    ) -> Option<(*mut PangoLayout, f64, f64)> {
        let (layout, cw, _ch, base, _sp) =
            self.configure_layout_full(family, size_pt, bold, italic)?;
        Some((layout, cw, base))
    }
}

impl Drop for PangoFrameScope {
    fn drop(&mut self) {
        if !self.layout.is_null() {
            // SAFETY: `self.layout` was returned by `pango_cairo_create_layout` with refcount 1
            // and has not been unreferenced elsewhere.
            unsafe {
                ffi::g_object_unref(self.layout.cast());
            }
        }
    }
}

/// Queries the device scale `(sx, sy)` of `cr`'s target surface, clamped to `>= 1.0`.
///
/// # Safety
/// `cr` must be a valid non-null `*mut CairoT` on the current thread.
pub(crate) unsafe fn query_cairo_device_scale(cr: *mut CairoT) -> (f64, f64) {
    if cr.is_null() {
        return (1.0, 1.0);
    }
    let mut sx: f64 = 1.0;
    let mut sy: f64 = 1.0;
    // SAFETY: Caller guarantees `cr` is a valid Cairo context on the current thread.
    let target = unsafe { ffi::cairo_get_target(cr) };
    if !target.is_null() {
        // SAFETY: `target` is a non-null Cairo surface owned by `cr`.
        unsafe { ffi::cairo_surface_get_device_scale(target, &mut sx, &mut sy) };
    }
    let mut mat = ffi::CairoMatrix::default();
    // SAFETY: `cr` is valid and `mat` is a stack-allocated `CairoMatrix`.
    unsafe { ffi::cairo_get_matrix(cr, &mut mat) };
    let ctm_x = mat.xx.hypot(mat.yx);
    let ctm_y = mat.xy.hypot(mat.yy);
    let clean_surf_x = if sx.is_finite() && sx >= 1.0 { sx } else { 1.0 };
    let clean_surf_y = if sy.is_finite() && sy >= 1.0 { sy } else { 1.0 };
    let clean_ctm_x = if ctm_x.is_finite() && ctm_x >= 1.0 {
        ctm_x
    } else {
        1.0
    };
    let clean_ctm_y = if ctm_y.is_finite() && ctm_y >= 1.0 {
        ctm_y
    } else {
        1.0
    };
    (clean_surf_x.max(clean_ctm_x), clean_surf_y.max(clean_ctm_y))
}

/// Sets `surface` as source on `cr` at `(x, y)` and immediately forces `CAIRO_FILTER_NEAREST` on
/// the resulting source pattern so Cairo/Pixman executes direct integer pixel copies instead of
/// `CAIRO_FILTER_GOOD` bilinear resampling.
///
/// # Safety
/// `cr` and `surface` must be valid non-null Cairo pointers on the current thread.
#[inline]
pub(crate) unsafe fn set_source_surface_nearest(
    cr: *mut CairoT,
    surface: *mut CairoSurfaceT,
    x: f64,
    y: f64,
) {
    // SAFETY: Caller guarantees `cr` and `surface` are valid on the current thread.
    unsafe {
        ffi::cairo_set_source_surface(cr, surface, x, y);
        let pat = ffi::cairo_get_source(cr);
        if !pat.is_null() {
            ffi::cairo_pattern_set_filter(pat, CAIRO_FILTER_NEAREST);
        }
    }
}

unsafe extern "C" fn release_cairo_surface_gbytes(data: *mut c_void) {
    if !data.is_null() {
        // SAFETY: `data` is an owned `cairo_surface_t*` reference acquired via
        // `cairo_surface_reference` in `OffscreenSurface::create_gdk_texture` and released once
        // when `GBytes` finalizes.
        unsafe {
            ffi::cairo_surface_destroy(data.cast::<CairoSurfaceT>());
        }
    }
}

/// RAII handle owning a `GdkTexture*` (`GdkMemoryTexture`) uploaded and cached on the GPU by GSK.
pub(crate) struct GdkTextureHandle(pub(crate) *mut GdkTexture);

impl Drop for GdkTextureHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was returned with `(transfer full)` ownership by `gdk_memory_texture_new`.
            unsafe {
                ffi::g_object_unref(self.0.cast());
            }
        }
    }
}

/// RAII wrapper around an offscreen Cairo image surface, drawing context, and per-band dirty hashes.
pub(crate) struct OffscreenSurface {
    pub(crate) surface: *mut CairoSurfaceT,
    pub(crate) cr: *mut CairoT,
    base_refcount: libc::c_uint,
    pub(crate) width: c_int,
    pub(crate) height: c_int,
    pub(crate) phys_w: c_int,
    pub(crate) phys_h: c_int,
    pub(crate) device_scale_x: f64,
    pub(crate) device_scale_y: f64,
    pub(crate) band_hashes: Vec<u64>,
    pub(crate) cached_bg: Option<ColorRgba>,
}

impl OffscreenSurface {
    pub(crate) fn new(width: c_int, height: c_int) -> Result<Self, String> {
        Self::new_scaled(width, height, 1.0, 1.0)
    }

    pub(crate) fn new_scaled(
        width: c_int,
        height: c_int,
        scale_x: f64,
        scale_y: f64,
    ) -> Result<Self, String> {
        let safe_w = width.max(1);
        let safe_h = height.max(1);
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
        let phys_w = (f64::from(safe_w) * sx).ceil().max(1.0) as c_int;
        let phys_h = (f64::from(safe_h) * sy).ceil().max(1.0) as c_int;
        // SAFETY: `cairo_image_surface_create` safely allocates an in-memory ARGB32 surface for
        // positive dimensions and returns a valid status-checked surface pointer even on failure.
        let surface =
            unsafe { ffi::cairo_image_surface_create(CAIRO_FORMAT_ARGB32, phys_w, phys_h) };
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

        // SAFETY: `surface` is a valid Cairo surface owned on this thread. Keep `surface`'s own
        // `device_scale` at `(1.0, 1.0)` and apply `(sx, sy)` via `cairo_scale(cr, sx, sy)` so
        // `cairo_identity_matrix(cr)` drops straight into exact 1:1 integer physical pixel space
        // without IEEE-754 `sx * (1.0 / sx)` fixed-point drift at fractional scales (e.g. `1.5x`).
        let cr = unsafe { ffi::cairo_create(surface) };
        if cr.is_null() {
            // SAFETY: `surface` is uniquely owned here and must be destroyed before returning.
            unsafe {
                ffi::cairo_surface_destroy(surface);
            }
            return Err("cairo_create returned null".to_string());
        }
        if (sx - 1.0).abs() > 1e-4 || (sy - 1.0).abs() > 1e-4 {
            // SAFETY: `cr` is valid and uniquely owned on this thread.
            unsafe { ffi::cairo_scale(cr, sx, sy) };
        }

        // SAFETY: `surface` is valid and owned by `(surface, cr)` on this thread; record the
        // baseline Cairo reference count (e.g. `surface` + `cr`'s `target`/`original_target` refs)
        // so `is_uniquely_owned` detects any external `GBytes`/`GdkMemoryTexture` reference.
        let base_refcount = unsafe { ffi::cairo_surface_get_reference_count(surface) };

        Ok(Self {
            surface,
            cr,
            base_refcount,
            width: safe_w,
            height: safe_h,
            phys_w,
            phys_h,
            device_scale_x: sx,
            device_scale_y: sy,
            band_hashes: Vec::new(),
            cached_bg: None,
        })
    }

    /// Returns `true` when no external `GBytes` / `GdkMemoryTexture` holds a reference to the
    /// backing pixel buffer (`refcount <= self.base_refcount`), making it safe to mutate in-place
    /// for a new frame.
    pub(crate) fn is_uniquely_owned(&self) -> bool {
        if self.surface.is_null() {
            return false;
        }
        // SAFETY: `self.surface` is a valid Cairo surface owned by `self`.
        unsafe { ffi::cairo_surface_get_reference_count(self.surface) <= self.base_refcount }
    }

    /// Wraps this flushed ARGB32 surface zero-copy into an immutable `GdkMemoryTexture` whose
    /// `GBytes` holds a `cairo_surface_reference` until GSK releases the texture.
    pub(crate) fn create_gdk_texture(&self) -> Option<GdkTextureHandle> {
        if self.surface.is_null() {
            return None;
        }
        // SAFETY: `self.surface` is a valid ARGB32 image surface; flushing finalizes all pixel
        // writes, and `cairo_surface_reference` keeps the pixel buffer alive and immutable (because
        // `is_uniquely_owned` prevents reuse while `refcount > 2`) until `release_cairo_surface_gbytes` runs.
        unsafe {
            ffi::cairo_surface_flush(self.surface);
            let phys_w = ffi::cairo_image_surface_get_width(self.surface);
            let phys_h = ffi::cairo_image_surface_get_height(self.surface);
            let stride = ffi::cairo_image_surface_get_stride(self.surface);
            let data = ffi::cairo_image_surface_get_data(self.surface);
            if phys_w <= 0 || phys_h <= 0 || stride < phys_w * 4 || data.is_null() {
                return None;
            }
            let byte_len = (stride as usize).checked_mul(phys_h as usize)?;
            let retained = ffi::cairo_surface_reference(self.surface);
            let gbytes = ffi::g_bytes_new_with_free_func(
                data.cast::<c_void>(),
                byte_len,
                Some(release_cairo_surface_gbytes),
                retained.cast::<c_void>(),
            );
            if gbytes.is_null() {
                ffi::cairo_surface_destroy(retained);
                return None;
            }
            let tex = ffi::gdk_memory_texture_new(
                phys_w,
                phys_h,
                CAIRO_ARGB32_GDK_MEMORY_FORMAT,
                gbytes,
                stride as usize,
            );
            ffi::g_bytes_unref(gbytes);
            if tex.is_null() {
                None
            } else {
                Some(GdkTextureHandle(tex))
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn read_argb_pixel(&self, x: i32, y: i32) -> u32 {
        assert!(
            x >= 0 && x < self.width && y >= 0 && y < self.height,
            "pixel ({x}, {y}) out of surface bounds ({}, {})",
            self.width,
            self.height
        );
        // SAFETY: `self.surface` is a valid ARGB32 Cairo image surface; flushing ensures all
        // pending Cairo operations have written to the backing pixel buffer, and bounds were
        // verified above (`stride` is in bytes, each pixel is 4-byte native-endian `0xAARRGGBB`).
        unsafe {
            ffi::cairo_surface_flush(self.surface);
            let data = ffi::cairo_image_surface_get_data(self.surface);
            let stride = ffi::cairo_image_surface_get_stride(self.surface) as usize;
            assert!(!data.is_null() && stride >= (self.width as usize) * 4);
            let row_ptr = data.add((y as usize) * stride);
            let px_ptr = row_ptr.add((x as usize) * 4).cast::<u32>();
            px_ptr.read_unaligned()
        }
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

pub(crate) fn build_font_family_chain(family: &str) -> Cow<'_, str> {
    if !family.contains("Symbols Nerd Font") && !family.contains("Nerd Font") {
        Cow::Owned(format!(
            "{family}, Symbols Nerd Font Mono, Symbols Nerd Font, Noto Sans Mono CJK SC, Monospace"
        ))
    } else {
        Cow::Borrowed(family)
    }
}

fn create_font_description(
    family: &str,
    size_pango: c_int,
    bold: bool,
    italic: bool,
) -> Option<*mut PangoFontDescription> {
    let family_chain = build_font_family_chain(family);
    let family_cstr = CString::new(family_chain.as_ref()).ok()?;
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

#[inline]
fn is_zero_width_or_combining(cp: u32) -> bool {
    matches!(
        cp,
        0x0000..=0x001F
            | 0x007F..=0x009F
            | 0x0300..=0x036F
            | 0x0483..=0x0489
            | 0x0591..=0x05BD
            | 0x05BF
            | 0x05C1..=0x05C2
            | 0x05C4..=0x05C5
            | 0x05C7
            | 0x0610..=0x061A
            | 0x064B..=0x065F
            | 0x0670
            | 0x06D6..=0x06DC
            | 0x06DF..=0x06E4
            | 0x06E7..=0x06E8
            | 0x06EA..=0x06ED
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x200B..=0x200F
            | 0x2060..=0x2064
            | 0x20D0..=0x20FF
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
            | 0xFEFF
            | 0xE0100..=0xE01EF
    )
}

#[inline]
fn is_cluster_continuation(ch: char) -> bool {
    let cp = ch as u32;
    (0x1F3FB..=0x1F3FF).contains(&cp) || (cp >= 0x0300 && is_zero_width_or_combining(cp))
}

/// Returns monospace cell width (`0`, `1`, or `2`) for a Unicode scalar value.
///
/// Nerd Font PUA codepoints (`0xE000..=0xF8FF`, `0xF0000..=0xFFFFD`, `0x100000..=0x10FFFD`) and
/// box-drawing / 1-cell UI symbols evaluate to `1`, while CJK / Fullwidth / Emoji and common
/// dingbats (`0x231A..0x27BF` such as `✅❌✨⚡` plus `⭐` and `0x1F300..0x1FAFF`) evaluate to `2`.
pub(crate) fn codepoint_cell_width(ch: char) -> usize {
    let cp = ch as u32;
    if cp < 0x0300 {
        return usize::from(cp >= 0x20 && !(0x7F..=0x9F).contains(&cp));
    }
    if is_zero_width_or_combining(cp) {
        return 0;
    }
    if matches!(
        cp,
        0x1100..=0x115F
            | 0x231A..=0x231B
            | 0x2329..=0x232A
            | 0x23E9..=0x23F3
            | 0x23F8..=0x23FA
            | 0x25FD..=0x25FE
            | 0x2614..=0x2615
            | 0x2648..=0x2653
            | 0x267F
            | 0x2693
            | 0x26A1
            | 0x26AA..=0x26AB
            | 0x26BD..=0x26BE
            | 0x26C4..=0x26C5
            | 0x26CE
            | 0x26D4
            | 0x26EA
            | 0x26F2..=0x26F3
            | 0x26F5
            | 0x26FA
            | 0x26FD
            | 0x2705
            | 0x270A..=0x270B
            | 0x2728
            | 0x274C
            | 0x274E
            | 0x2753..=0x2755
            | 0x2757
            | 0x2795..=0x2797
            | 0x27B0
            | 0x27BF
            | 0x2B1B..=0x2B1C
            | 0x2B50
            | 0x2B55
            | 0x2E80..=0x303E
            | 0x3040..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0xA4CF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF01..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F004
            | 0x1F0CF
            | 0x1F18E
            | 0x1F191..=0x1F19A
            | 0x1F200..=0x1F251
            | 0x1F300..=0x1FAFF
            | 0x20000..=0x3FFFD
    ) {
        return 2;
    }
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextSegment<'a> {
    Ascii(&'a str),
    Cluster { text: &'a str, cells: usize },
}

/// Segments a mixed UTF-8 string into contiguous pure-ASCII runs and non-ASCII base+combining
/// clusters with exact monospace cell counts (skin-tones `0x1F3FB..=0x1F3FF` and ZWJ sequences
/// attach to their base emoji cluster with `0` extra cell advance).
pub(crate) fn for_each_text_segment<'a>(text: &'a str, mut emit: impl FnMut(TextSegment<'a>)) {
    let mut iter = text.char_indices().peekable();
    while let Some((start_idx, ch)) = iter.next() {
        if ch.is_ascii() {
            let next_attaches = iter
                .peek()
                .is_some_and(|&(_, next_c)| !next_c.is_ascii() && is_cluster_continuation(next_c));
            if !next_attaches {
                let mut end_idx = start_idx + 1;
                while let Some(&(idx, c)) = iter.peek() {
                    if !c.is_ascii() {
                        break;
                    }
                    iter.next();
                    if iter.peek().is_some_and(|&(_, after)| {
                        !after.is_ascii() && is_cluster_continuation(after)
                    }) {
                        emit(TextSegment::Ascii(&text[start_idx..end_idx]));
                        let mut cluster_end = idx + 1;
                        let mut prev_zwj = false;
                        while let Some(&(_, cont_c)) = iter.peek() {
                            if prev_zwj || is_cluster_continuation(cont_c) {
                                iter.next();
                                cluster_end += cont_c.len_utf8();
                                prev_zwj = cont_c == '\u{200D}';
                            } else {
                                break;
                            }
                        }
                        let cells = usize::from(!c.is_ascii_control());
                        emit(TextSegment::Cluster {
                            text: &text[idx..cluster_end],
                            cells,
                        });
                        end_idx = 0;
                        break;
                    }
                    end_idx = idx + 1;
                }
                if end_idx > start_idx {
                    emit(TextSegment::Ascii(&text[start_idx..end_idx]));
                }
                continue;
            }
        }

        let cells = codepoint_cell_width(ch);
        let mut prev_zwj = ch == '\u{200D}';
        let mut end_idx = start_idx + ch.len_utf8();
        while let Some(&(_, next_c)) = iter.peek() {
            if prev_zwj || is_cluster_continuation(next_c) {
                iter.next();
                end_idx += next_c.len_utf8();
                prev_zwj = next_c == '\u{200D}';
            } else {
                break;
            }
        }
        emit(TextSegment::Cluster {
            text: &text[start_idx..end_idx],
            cells,
        });
    }
}

pub(crate) fn set_source_color(cr: *mut CairoT, color: ColorRgba) {
    let (r, g, b, a) = color.components_f64();
    // SAFETY: `cr` is a valid Cairo context borrowed on the active rendering thread.
    unsafe {
        ffi::cairo_set_source_rgba(cr, r, g, b, a);
    }
}

pub(crate) fn append_rect_path(cr: *mut CairoT, x: f64, y: f64, w: f64, h: f64, radius: f64) {
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

pub(crate) fn draw_caret(
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
    // SAFETY: Caller upholds exclusive validity of `cr` on the current thread.
    unsafe {
        render_frame_to_cairo_with_opts(cr, viewport_width, viewport_height, frame, None);
    }
}

const DIRTY_BAND_HEIGHT_PX: f64 = 32.0;

#[inline]
fn mix_band_hash(acc: u64, val: u64) -> u64 {
    (acc ^ val)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .rotate_left(17)
}

#[inline]
fn quantize_coord(v: f64) -> u64 {
    if v.is_finite() {
        ((v * 64.0).round() as i64) as u64
    } else {
        0
    }
}

fn hash_draw_cmd_for_band(cmd: &DrawCmd, active_clip: (f64, f64, f64, f64)) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    quantize_coord(active_clip.0).hash(&mut h);
    quantize_coord(active_clip.1).hash(&mut h);
    quantize_coord(active_clip.2).hash(&mut h);
    quantize_coord(active_clip.3).hash(&mut h);
    match cmd {
        DrawCmd::ClipPush { .. } | DrawCmd::ClipPop => {}
        DrawCmd::FillRect {
            x,
            y,
            w,
            h: rh,
            color,
            radius,
        } => {
            1u8.hash(&mut h);
            quantize_coord(*x).hash(&mut h);
            quantize_coord(*y).hash(&mut h);
            quantize_coord(*w).hash(&mut h);
            quantize_coord(*rh).hash(&mut h);
            color.0.hash(&mut h);
            quantize_coord(*radius).hash(&mut h);
        }
        DrawCmd::StrokeRect {
            x,
            y,
            w,
            h: rh,
            color,
            line_width,
            radius,
        } => {
            2u8.hash(&mut h);
            quantize_coord(*x).hash(&mut h);
            quantize_coord(*y).hash(&mut h);
            quantize_coord(*w).hash(&mut h);
            quantize_coord(*rh).hash(&mut h);
            color.0.hash(&mut h);
            quantize_coord(*line_width).hash(&mut h);
            quantize_coord(*radius).hash(&mut h);
        }
        DrawCmd::Line {
            x1,
            y1,
            x2,
            y2,
            color,
            line_width,
        } => {
            3u8.hash(&mut h);
            quantize_coord(*x1).hash(&mut h);
            quantize_coord(*y1).hash(&mut h);
            quantize_coord(*x2).hash(&mut h);
            quantize_coord(*y2).hash(&mut h);
            color.0.hash(&mut h);
            quantize_coord(*line_width).hash(&mut h);
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
            4u8.hash(&mut h);
            quantize_coord(*x).hash(&mut h);
            quantize_coord(*y).hash(&mut h);
            text.hash(&mut h);
            font_family.hash(&mut h);
            quantize_coord(*font_size_pt).hash(&mut h);
            color.0.hash(&mut h);
            bold.hash(&mut h);
            italic.hash(&mut h);
            underline.hash(&mut h);
        }
        DrawCmd::CursorCaret {
            x,
            y,
            w,
            h: ch,
            shape,
            color,
        } => {
            5u8.hash(&mut h);
            quantize_coord(*x).hash(&mut h);
            quantize_coord(*y).hash(&mut h);
            quantize_coord(*w).hash(&mut h);
            quantize_coord(*ch).hash(&mut h);
            (*shape as u8).hash(&mut h);
            color.0.hash(&mut h);
        }
    }
    h.finish()
}

#[inline]
fn cmd_vertical_bounds(cmd: &DrawCmd) -> Option<(f64, f64)> {
    match cmd {
        DrawCmd::ClipPush { .. } | DrawCmd::ClipPop => None,
        DrawCmd::FillRect { y, h, .. } => {
            if *h <= 0.0 || !y.is_finite() || !h.is_finite() {
                None
            } else {
                Some((*y - 1.0, *y + *h + 1.0))
            }
        }
        DrawCmd::StrokeRect {
            y, h, line_width, ..
        } => {
            if *h <= 0.0 || *line_width <= 0.0 || !y.is_finite() || !h.is_finite() {
                None
            } else {
                let pad = line_width.max(1.0) + 1.0;
                Some((*y - pad, *y + *h + pad))
            }
        }
        DrawCmd::Line {
            y1, y2, line_width, ..
        } => {
            if *line_width <= 0.0 || !y1.is_finite() || !y2.is_finite() {
                None
            } else {
                let pad = line_width.max(1.0) + 1.0;
                Some((y1.min(*y2) - pad, y1.max(*y2) + pad))
            }
        }
        DrawCmd::TextRun {
            y,
            text,
            font_size_pt,
            ..
        } => {
            if text.is_empty() || !y.is_finite() {
                None
            } else {
                let est_h = if font_size_pt.is_finite() && *font_size_pt > 0.0 {
                    (*font_size_pt * 2.2).max(28.0)
                } else {
                    32.0
                };
                Some((*y - 6.0, *y + est_h + 6.0))
            }
        }
        DrawCmd::CursorCaret { y, h, .. } => {
            if !y.is_finite() || !h.is_finite() {
                None
            } else {
                Some((*y - 2.0, *y + h.max(1.0) + 2.0))
            }
        }
    }
}

fn compute_frame_band_hashes(
    bg_w: f64,
    bg_h: f64,
    frame: &RenderFrame,
    skip_caret_idx: Option<usize>,
) -> Vec<u64> {
    let num_bands = ((bg_h / DIRTY_BAND_HEIGHT_PX).ceil().max(1.0)) as usize;
    let seed = u64::from(frame.window.background.0) ^ 0xcbf2_9ce4_8422_2325;
    let mut hashes = vec![seed; num_bands];
    let mut clip_stack: Vec<(f64, f64, f64, f64)> = Vec::with_capacity(8);
    let root_clip = (0.0, 0.0, bg_w, bg_h);

    for (cmd_idx, cmd) in frame.commands.iter().enumerate() {
        if skip_caret_idx == Some(cmd_idx) && matches!(cmd, DrawCmd::CursorCaret { .. }) {
            continue;
        }
        match cmd {
            DrawCmd::ClipPush { x, y, w, h } => {
                let cur = clip_stack.last().copied().unwrap_or(root_clip);
                let nx0 = x.max(cur.0);
                let ny0 = y.max(cur.1);
                let nx1 = (*x + w.max(0.0)).min(cur.0 + cur.2);
                let ny1 = (*y + h.max(0.0)).min(cur.1 + cur.3);
                clip_stack.push((nx0, ny0, (nx1 - nx0).max(0.0), (ny1 - ny0).max(0.0)));
                continue;
            }
            DrawCmd::ClipPop => {
                let _ = clip_stack.pop();
                continue;
            }
            _ => {}
        }

        let Some((raw_y0, raw_y1)) = cmd_vertical_bounds(cmd) else {
            continue;
        };
        let cur_clip = clip_stack.last().copied().unwrap_or(root_clip);
        if cur_clip.2 <= 0.0 || cur_clip.3 <= 0.0 {
            continue;
        }
        let y0 = raw_y0.max(cur_clip.1).max(0.0);
        let y1 = raw_y1.min(cur_clip.1 + cur_clip.3).min(bg_h);
        if y1 <= y0 {
            continue;
        }
        let b_lo = ((y0 / DIRTY_BAND_HEIGHT_PX).floor().max(0.0) as usize).min(num_bands - 1);
        let b_hi = (((y1 / DIRTY_BAND_HEIGHT_PX).ceil().max(1.0) as usize) - 1).min(num_bands - 1);
        let cmd_h = hash_draw_cmd_for_band(cmd, cur_clip);
        for slot in &mut hashes[b_lo..=b_hi] {
            *slot = mix_band_hash(*slot, cmd_h);
        }
    }
    hashes
}

#[inline]
fn range_overlaps_dirty_bands(y0: f64, y1: f64, dirty: &[bool]) -> bool {
    if dirty.is_empty() || y1 < 0.0 {
        return false;
    }
    let lo = ((y0.max(0.0) / DIRTY_BAND_HEIGHT_PX).floor() as usize).min(dirty.len() - 1);
    let hi = (((y1.max(0.0) / DIRTY_BAND_HEIGHT_PX).ceil() as usize).saturating_sub(1))
        .min(dirty.len() - 1);
    if lo > hi {
        return dirty[lo];
    }
    dirty[lo..=hi].iter().any(|&d| d)
}

/// Incrementally updates `target` (`OffscreenSurface`) for `frame` by hashing `32px` horizontal
/// scene bands and clipping/executing only bands whose ordered `DrawCmd` sequence changed since the
/// last frame rendered into `target`.
///
/// # Safety
/// `target` must be uniquely owned on the current thread (`target.is_uniquely_owned()`).
pub(crate) unsafe fn render_offscreen_incremental_with_opts(
    target: &mut OffscreenSurface,
    frame: &RenderFrame,
    skip_caret_idx: Option<usize>,
) {
    if target.cr.is_null() || target.surface.is_null() {
        return;
    }
    let bg_w = f64::from(target.width)
        .max(f64::from(frame.window.width))
        .max(1.0);
    let bg_h = f64::from(target.height)
        .max(f64::from(frame.window.height))
        .max(1.0);
    let new_hashes = compute_frame_band_hashes(bg_w, bg_h, frame, skip_caret_idx);

    let can_diff = target.cached_bg == Some(frame.window.background)
        && target.band_hashes.len() == new_hashes.len()
        && !new_hashes.is_empty();

    if can_diff {
        let mut dirty_mask = vec![false; new_hashes.len()];
        let mut dirty_count = 0usize;
        for (i, (&old_h, &new_h)) in target.band_hashes.iter().zip(&new_hashes).enumerate() {
            if old_h != new_h {
                dirty_mask[i] = true;
                dirty_count += 1;
            }
        }
        if dirty_count == 0 {
            return;
        }
        if dirty_count < new_hashes.len() {
            // SAFETY: `target.cr` and `target.surface` are uniquely owned on this thread; clips to
            // exact coalesced dirty bands, clears background within the dirty clip, replays only
            // overlapping `DrawCmd`s, restores balanced Cairo state, and flushes the surface.
            unsafe {
                ffi::cairo_save(target.cr);
                ffi::cairo_new_path(target.cr);
                let mut b = 0usize;
                while b < dirty_mask.len() {
                    if !dirty_mask[b] {
                        b += 1;
                        continue;
                    }
                    let start_b = b;
                    while b < dirty_mask.len() && dirty_mask[b] {
                        b += 1;
                    }
                    let raw_y0 = (start_b as f64) * DIRTY_BAND_HEIGHT_PX;
                    let raw_y1 = ((b as f64) * DIRTY_BAND_HEIGHT_PX).min(bg_h);
                    // Snap dirty clip boundaries to exact integer physical pixels so fractional
                    // Wayland scales (`1.25x`, `1.5x`) never create antialiased clip seam masks.
                    let sy = target.device_scale_y.max(1.0);
                    let y0 = (raw_y0 * sy).floor() / sy;
                    let y1 = ((raw_y1 * sy).ceil() / sy).min(bg_h);
                    if y1 > y0 {
                        ffi::cairo_rectangle(target.cr, 0.0, y0, bg_w, y1 - y0);
                    }
                }
                ffi::cairo_clip(target.cr);
                set_source_color(target.cr, frame.window.background);
                append_rect_path(target.cr, 0.0, 0.0, bg_w, bg_h, 0.0);
                ffi::cairo_fill(target.cr);
                render_commands_to_cairo_filtered(
                    target.cr,
                    &frame.commands,
                    skip_caret_idx,
                    Some(&dirty_mask),
                );
                ffi::cairo_restore(target.cr);
                ffi::cairo_surface_flush(target.surface);
            }
            target.band_hashes = new_hashes;
            target.cached_bg = Some(frame.window.background);
            return;
        }
    }

    // Full render path (initial frame, resize, theme background change, or 100% dirty viewport).
    // SAFETY: `target.cr` and `target.surface` are uniquely owned on this thread.
    unsafe {
        render_frame_to_cairo_with_opts(target.cr, bg_w, bg_h, frame, skip_caret_idx);
        ffi::cairo_surface_flush(target.surface);
    }
    target.band_hashes = new_hashes;
    target.cached_bg = Some(frame.window.background);
}

/// Executes all drawing commands in `frame` onto `cr`, optionally skipping the `CursorCaret` at
/// command index `skip_caret_idx` so the live animation compositor can render a smooth caret trail
/// on top of a cached static base surface.
///
/// # Safety
/// `cr` must be a valid, non-null `*mut CairoT` exclusively accessed on the current thread for
/// the duration of this call.
pub(crate) unsafe fn render_frame_to_cairo_with_opts(
    cr: *mut CairoT,
    viewport_width: f64,
    viewport_height: f64,
    frame: &RenderFrame,
    skip_caret_idx: Option<usize>,
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
        render_commands_to_cairo(cr, &frame.commands, skip_caret_idx);
    }
}

fn count_text_cells(text: &str) -> usize {
    let mut total = 0usize;
    for_each_text_segment(text, |seg| match seg {
        TextSegment::Ascii(slice) => {
            total += slice.bytes().filter(|b| *b >= 0x20 && *b != 0x7F).count();
        }
        TextSegment::Cluster { cells, .. } => {
            total += cells;
        }
    });
    total
}

/// Paints a cached premultiplied `RowStripSurface` onto `cr` in exact 1:1 physical pixel space so
/// fractional scales (`1.25x`, `1.5x`, `1.75x`) never suffer 16.16 fixed-point Pixman matrix drift.
///
/// # Safety
/// `cr` and `strip.surface` must be valid Cairo objects on the current thread.
#[inline]
unsafe fn blit_row_strip(
    cr: *mut CairoT,
    strip: &RowStripSurface,
    draw_x: f64,
    draw_y: f64,
    scale_x: f64,
    scale_y: f64,
) {
    let dst_px = (draw_x * scale_x).round() as c_int - strip.left_pad_px;
    let dst_py = (draw_y * scale_y).round() as c_int - strip.top_pad_px;
    // SAFETY: `cr` and `strip.surface` are valid on this thread. Active Cairo clips are stored in
    // device space and remain intact across `cairo_identity_matrix`; `cairo_restore` restores CTM.
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
        set_source_surface_nearest(cr, strip.surface, f64::from(dst_px), f64::from(dst_py));
        ffi::cairo_new_path(cr);
        ffi::cairo_rectangle(
            cr,
            f64::from(dst_px),
            f64::from(dst_py),
            f64::from(strip.phys_w),
            f64::from(strip.phys_h),
        );
        ffi::cairo_fill(cr);
        ffi::cairo_restore(cr);
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_ascii_spans_direct(
    cr: *mut CairoT,
    batch_layout: *mut PangoLayout,
    row_buf: &str,
    row_spans: &[(u32, u32, ColorRgba, bool)],
    draw_x: f64,
    draw_y: f64,
) {
    let byte_len = c_int::try_from(row_buf.len()).unwrap_or(c_int::MAX);
    // SAFETY: `batch_layout` and `cr` are valid PangoCairo objects on this thread; `attr_list`
    // owns inserted attributes and is unreferenced immediately after `pango_cairo_show_layout`.
    unsafe {
        let attr_list = ffi::pango_attr_list_new();
        if !attr_list.is_null() {
            for &(start_idx, end_idx, col, is_it) in row_spans {
                let fg = ffi::pango_attr_foreground_new(
                    ((col.0 >> 24) & 0xff) as u16 * 257,
                    ((col.0 >> 16) & 0xff) as u16 * 257,
                    ((col.0 >> 8) & 0xff) as u16 * 257,
                );
                if !fg.is_null() {
                    (*fg).start_index = start_idx;
                    (*fg).end_index = end_idx;
                    ffi::pango_attr_list_insert(attr_list, fg);
                }
                if is_it {
                    let st = ffi::pango_attr_style_new(PANGO_STYLE_ITALIC);
                    if !st.is_null() {
                        (*st).start_index = start_idx;
                        (*st).end_index = end_idx;
                        ffi::pango_attr_list_insert(attr_list, st);
                    }
                }
            }
            ffi::pango_layout_set_attributes(batch_layout, attr_list);
        }
        ffi::pango_layout_set_text(batch_layout, row_buf.as_ptr().cast(), byte_len);
        ffi::cairo_new_path(cr);
        ffi::cairo_move_to(cr, draw_x, draw_y);
        ffi::pango_cairo_show_layout(cr, batch_layout);
        if !attr_list.is_null() {
            ffi::pango_layout_set_attributes(batch_layout, ptr::null_mut());
            ffi::pango_attr_list_unref(attr_list);
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_mixed_text_direct(
    cr: *mut CairoT,
    layout: *mut PangoLayout,
    text: &str,
    origin_x: f64,
    origin_y: f64,
    cell_w: f64,
    baseline_px: f64,
    scale_x: f64,
    scale_y: f64,
) -> usize {
    let snap_x = |vx: f64| -> f64 { (vx * scale_x).round() / scale_x };
    let snap_y = |vy: f64| -> f64 { (vy * scale_y).round() / scale_y };
    let draw_y = snap_y(origin_y);

    if text.is_ascii() {
        let cells = text.bytes().filter(|b| *b >= 0x20 && *b != 0x7F).count();
        if text.bytes().any(|b| b > b' ' && b != 0x7F) {
            let byte_len = c_int::try_from(text.len()).unwrap_or(c_int::MAX);
            // SAFETY: `layout` and `cr` are valid on this thread; `text` is valid ASCII UTF-8.
            unsafe {
                ffi::pango_layout_set_text(layout, text.as_ptr().cast(), byte_len);
                ffi::cairo_new_path(cr);
                ffi::cairo_move_to(cr, snap_x(origin_x), draw_y);
                ffi::pango_cairo_show_layout(cr, layout);
            }
        }
        return cells;
    }

    let pango_scale = f64::from(PANGO_SCALE);
    let mut cur_x = origin_x;
    let mut acc_cells: usize = 0;
    for_each_text_segment(text, |seg| match seg {
        TextSegment::Ascii(slice) => {
            let cells = slice.bytes().filter(|b| *b >= 0x20 && *b != 0x7F).count();
            if slice.bytes().any(|b| b > b' ' && b != 0x7F) {
                let byte_len = c_int::try_from(slice.len()).unwrap_or(c_int::MAX);
                // SAFETY: `layout` and `cr` are valid on this thread; `slice` is valid UTF-8.
                unsafe {
                    ffi::pango_layout_set_text(layout, slice.as_ptr().cast(), byte_len);
                    ffi::cairo_new_path(cr);
                    ffi::cairo_move_to(cr, snap_x(cur_x), draw_y);
                    ffi::pango_cairo_show_layout(cr, layout);
                }
            }
            cur_x += (cells as f64) * cell_w;
            acc_cells += cells;
        }
        TextSegment::Cluster {
            text: cluster,
            cells,
        } => {
            if cells > 0 {
                let slot_w = (cells as f64) * cell_w;
                let byte_len = c_int::try_from(cluster.len()).unwrap_or(c_int::MAX);
                let mut logical = PangoRectangle::default();
                // SAFETY: `layout` and `cr` are valid on this thread; `cluster` is valid UTF-8.
                let cluster_baseline_pango = unsafe {
                    ffi::pango_layout_set_text(layout, cluster.as_ptr().cast(), byte_len);
                    ffi::pango_layout_get_extents(layout, ptr::null_mut(), &mut logical);
                    ffi::pango_layout_get_baseline(layout)
                };
                let glyph_w = f64::from(logical.width) / pango_scale;
                let offset_x = if (slot_w - glyph_w).abs() > 0.25 {
                    ((slot_w - glyph_w) * 0.5).clamp(-cell_w * 0.25, slot_w * 0.5)
                } else {
                    0.0
                };
                let cluster_baseline = f64::from(cluster_baseline_pango) / pango_scale;
                let offset_y = if cluster_baseline > 0.0 {
                    baseline_px - cluster_baseline
                } else {
                    0.0
                };
                // SAFETY: `cr` and `layout` are valid; renders cluster centered in slot.
                unsafe {
                    ffi::cairo_new_path(cr);
                    ffi::cairo_move_to(cr, snap_x(cur_x + offset_x), snap_y(origin_y + offset_y));
                    ffi::pango_cairo_show_layout(cr, layout);
                }
                cur_x += slot_w;
                acc_cells += cells;
            }
        }
    });
    acc_cells
}

#[allow(clippy::too_many_arguments)]
fn rasterize_row_strip(
    family: &str,
    font_size_pt: f64,
    bold: bool,
    cell_w: f64,
    cell_h: f64,
    baseline_px: f64,
    total_cells: usize,
    scale_x: f64,
    scale_y: f64,
    text: &str,
    style: &RowStyleKey,
) -> Option<RowStripSurface> {
    if total_cells == 0 {
        return None;
    }
    let left_pad_px = ((cell_w * 0.5).ceil().max(3.0) * scale_x).ceil() as c_int;
    let top_pad_px = (4.0 * scale_y).ceil() as c_int;
    let left_pad = f64::from(left_pad_px) / scale_x;
    let top_pad = f64::from(top_pad_px) / scale_y;
    let logical_w = (left_pad + (total_cells as f64) * cell_w + cell_w + 4.0).ceil();
    let logical_h = (top_pad + cell_h.max(baseline_px + 6.0) + 6.0).ceil();
    if logical_w <= 0.0 || logical_w > f64::from(MAX_CACHED_STRIP_WIDTH_PX) || logical_h <= 0.0 {
        return None;
    }
    let phys_w = (logical_w * scale_x).ceil().max(1.0) as c_int;
    let phys_h = (logical_h * scale_y).ceil().max(1.0) as c_int;

    // SAFETY: Allocates an ARGB32 transparent strip surface with native `(1.0, 1.0)` device scale,
    // scales `strip_cr` by `(scale_x, scale_y)`, forces `CAIRO_ANTIALIAS_GRAY` on both `strip_cr`
    // and the `PangoContext` (preventing 3-channel LCD component-alpha overflow `R,G,B > A` onto a
    // transparent 1-alpha `ARGB32` buffer when KDE/GTK `gtk-xft-rgba=rgb` is active), destroys
    // `strip_cr` + `font_opts`, and flushes `surface` with refcount 1.
    unsafe {
        let surface = ffi::cairo_image_surface_create(CAIRO_FORMAT_ARGB32, phys_w, phys_h);
        if surface.is_null() {
            return None;
        }
        if ffi::cairo_surface_status(surface) != CAIRO_STATUS_SUCCESS {
            ffi::cairo_surface_destroy(surface);
            return None;
        }
        let strip_cr = ffi::cairo_create(surface);
        if strip_cr.is_null() {
            ffi::cairo_surface_destroy(surface);
            return None;
        }
        if (scale_x - 1.0).abs() > 1e-4 || (scale_y - 1.0).abs() > 1e-4 {
            ffi::cairo_scale(strip_cr, scale_x, scale_y);
        }
        {
            let mut strip_scope = PangoFrameScope::new(strip_cr);
            match style {
                RowStyleKey::Spans(spans) => {
                    if let Some((strip_layout, _, _)) =
                        strip_scope.configure_layout(family, font_size_pt, bold, false)
                    {
                        set_source_color(strip_cr, ColorRgba::from_rgba(255, 255, 255, 255));
                        draw_ascii_spans_direct(
                            strip_cr,
                            strip_layout,
                            text,
                            spans,
                            left_pad,
                            top_pad,
                        );
                    }
                }
                RowStyleKey::Single { color, italic } => {
                    if let Some((strip_layout, scw, sbase)) =
                        strip_scope.configure_layout(family, font_size_pt, bold, *italic)
                    {
                        set_source_color(strip_cr, *color);
                        let _ = draw_mixed_text_direct(
                            strip_cr,
                            strip_layout,
                            text,
                            left_pad,
                            top_pad,
                            scw,
                            sbase,
                            scale_x,
                            scale_y,
                        );
                    }
                }
            }
        }
        ffi::cairo_destroy(strip_cr);
        ffi::cairo_surface_flush(surface);
        let stride = ffi::cairo_image_surface_get_stride(surface).max(0) as usize;
        let byte_size = stride.saturating_mul(phys_h as usize);
        Some(RowStripSurface {
            surface,
            phys_w,
            phys_h,
            left_pad_px,
            top_pad_px,
            byte_size,
            last_used_seq: 0,
        })
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn try_blit_or_cache_row_strip(
    cr: *mut CairoT,
    key: RowRasterKey,
    font_family: &str,
    font_size_pt: f64,
    bold: bool,
    cell_w: f64,
    cell_h: f64,
    baseline_px: f64,
    total_cells: usize,
    scale_x: f64,
    scale_y: f64,
    draw_x: f64,
    draw_y: f64,
) -> bool {
    THREAD_ROW_RASTER_CACHE.with(|cell| {
        {
            let mut cache = cell.borrow_mut();
            let next_seq = cache.access_seq.wrapping_add(1);
            let hit = if let Some(strip) = cache.entries.get_mut(&key) {
                strip.last_used_seq = next_seq;
                // SAFETY: `cr` and `strip` are valid on this thread.
                unsafe {
                    blit_row_strip(cr, strip, draw_x, draw_y, scale_x, scale_y);
                }
                true
            } else {
                false
            };
            if hit {
                cache.access_seq = next_seq;
                #[cfg(test)]
                {
                    cache.hits += 1;
                }
                return true;
            }
            #[cfg(test)]
            {
                cache.misses += 1;
            }
        }
        if let Some(strip) = rasterize_row_strip(
            font_family,
            font_size_pt,
            bold,
            cell_w,
            cell_h,
            baseline_px,
            total_cells,
            scale_x,
            scale_y,
            &key.text,
            &key.style,
        ) {
            // SAFETY: `cr` and `strip` are valid on this thread.
            unsafe {
                blit_row_strip(cr, &strip, draw_x, draw_y, scale_x, scale_y);
            }
            cell.borrow_mut().insert(key, strip);
            true
        } else {
            false
        }
    })
}

/// Renders a slice of [`DrawCmd`]s directly onto `cr` without clearing the background surface.
///
/// # Safety
/// `cr` must be a valid, non-null `*mut CairoT` exclusively accessed on the current thread.
pub(crate) unsafe fn render_commands_to_cairo(
    cr: *mut CairoT,
    commands: &[DrawCmd],
    skip_caret_idx: Option<usize>,
) {
    // SAFETY: Forwards caller's `cr` invariant with no dirty-band filter.
    unsafe {
        render_commands_to_cairo_filtered(cr, commands, skip_caret_idx, None);
    }
}

unsafe fn render_commands_to_cairo_filtered(
    cr: *mut CairoT,
    commands: &[DrawCmd],
    skip_caret_idx: Option<usize>,
    dirty_bands: Option<&[bool]>,
) {
    if cr.is_null() {
        return;
    }
    let mut pango_scope = PangoFrameScope::new(cr);
    let mut clip_depth: usize = 0;
    // SAFETY: `cr` is valid per caller invariant.
    let (frame_scale_x, frame_scale_y) = unsafe { query_cairo_device_scale(cr) };
    let scale_x_milli = (frame_scale_x * 1000.0).round() as u32;
    let scale_y_milli = (frame_scale_y * 1000.0).round() as u32;
    let cache_scale_eligible =
        (1.0..=4.0).contains(&frame_scale_x) && (1.0..=4.0).contains(&frame_scale_y);

    let mut skip_until_idx: usize = 0;
    let mut row_buf = String::with_capacity(256);
    let mut row_spans: Vec<(u32, u32, ColorRgba, bool)> = Vec::with_capacity(64);
    let mut batch_underlines: Vec<(f64, usize, ColorRgba)> = Vec::with_capacity(8);

    for (cmd_idx, cmd) in commands.iter().enumerate() {
        if cmd_idx < skip_until_idx {
            continue;
        }
        if let Some(mask) = dirty_bands
            && !matches!(cmd, DrawCmd::ClipPush { .. } | DrawCmd::ClipPop)
        {
            let Some((y0, y1)) = cmd_vertical_bounds(cmd) else {
                continue;
            };
            if !range_overlaps_dirty_bands(y0 - 1.0, y1 + 1.0, mask) {
                continue;
            }
        }
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
                let Some((layout, cell_w, cell_h, baseline_px, size_pango)) =
                    pango_scope.configure_layout_full(font_family, *font_size_pt, *bold, *italic)
                else {
                    continue;
                };
                let norm_fam = if font_family.is_empty() || font_family.contains('\0') {
                    "Monospace"
                } else {
                    font_family.as_str()
                };

                let (scale_x, scale_y) = (frame_scale_x, frame_scale_y);
                let snap_x = |vx: f64| -> f64 { (vx * scale_x).round() / scale_x };
                let snap_y = |vy: f64| -> f64 { (vy * scale_y).round() / scale_y };
                let draw_x = snap_x(*x);
                let draw_y = snap_y(*y);

                let mut batch_handled_underlines = false;
                let total_cells: usize = if text.is_ascii() {
                    let first_cells = text.bytes().filter(|b| *b >= 0x20 && *b != 0x7F).count();
                    let can_try_batch = (color.0 & 0xff) == 0xff;
                    let has_mergeable_next = can_try_batch
                        && matches!(
                            commands.get(cmd_idx + 1),
                            Some(DrawCmd::TextRun {
                                x: nx,
                                y: ny,
                                text: ntext,
                                font_family: nfam,
                                font_size_pt: nsz,
                                color: ncol,
                                bold: nbold,
                                ..
                            }) if (ncol.0 & 0xff) == 0xff
                                && ntext.is_ascii()
                                && (*ny - *y).abs() < 0.01
                                && nfam == font_family
                                && (*nsz - *font_size_pt).abs() < 1e-4
                                && *nbold == *bold
                                && {
                                    let expected_x = *x + (first_cells as f64) * cell_w;
                                    let gap = ((*nx - expected_x) / cell_w).round();
                                    (0.0..=32.0).contains(&gap)
                                        && (*nx - (expected_x + gap * cell_w)).abs() < 0.35
                                }
                        );

                    if has_mergeable_next {
                        batch_handled_underlines = true;
                        row_buf.clear();
                        row_spans.clear();
                        batch_underlines.clear();
                        row_buf.push_str(text);
                        if text.bytes().any(|b| b > b' ' && b != 0x7F)
                            && let Ok(end) = u32::try_from(row_buf.len())
                        {
                            row_spans.push((0, end, *color, *italic));
                        }
                        if *underline && first_cells > 0 {
                            batch_underlines.push((*x, first_cells, *color));
                        }
                        let mut batch_cells = first_cells;
                        let mut expected_x = *x + (first_cells as f64) * cell_w;
                        let mut lookahead = cmd_idx + 1;
                        while let Some(DrawCmd::TextRun {
                            x: nx,
                            y: ny,
                            text: ntext,
                            font_family: nfam,
                            font_size_pt: nsz,
                            color: ncol,
                            bold: nbold,
                            italic: nitalic,
                            underline: nul,
                        }) = commands.get(lookahead)
                        {
                            if (ncol.0 & 0xff) != 0xff
                                || !ntext.is_ascii()
                                || (*ny - *y).abs() >= 0.01
                                || nfam != font_family
                                || (*nsz - *font_size_pt).abs() >= 1e-4
                                || *nbold != *bold
                            {
                                break;
                            }
                            let gap_f = ((*nx - expected_x) / cell_w).round();
                            if !(0.0..=32.0).contains(&gap_f)
                                || (*nx - (expected_x + gap_f * cell_w)).abs() >= 0.35
                            {
                                break;
                            }
                            let gap_cells = gap_f as usize;
                            for _ in 0..gap_cells {
                                row_buf.push(' ');
                            }
                            let start_u32 = u32::try_from(row_buf.len()).unwrap_or(u32::MAX);
                            row_buf.push_str(ntext);
                            let end_u32 = u32::try_from(row_buf.len()).unwrap_or(u32::MAX);
                            if start_u32 < end_u32 && ntext.bytes().any(|b| b > b' ' && b != 0x7F) {
                                row_spans.push((start_u32, end_u32, *ncol, *nitalic));
                            }
                            let ncells = ntext.bytes().filter(|b| *b >= 0x20 && *b != 0x7F).count();
                            if *nul && ncells > 0 {
                                batch_underlines.push((*nx, ncells, *ncol));
                            }
                            batch_cells += gap_cells + ncells;
                            expected_x += ((gap_cells + ncells) as f64) * cell_w;
                            lookahead += 1;
                        }
                        skip_until_idx = lookahead;
                        if !row_spans.is_empty() {
                            let use_cache =
                                cache_scale_eligible && row_buf.len() <= MAX_CACHED_ROW_TEXT_BYTES;
                            let mut blitted_from_cache = false;
                            if use_cache {
                                let key = RowRasterKey {
                                    family: norm_fam.to_string(),
                                    size_pango,
                                    bold: *bold,
                                    scale_x_milli,
                                    scale_y_milli,
                                    text: row_buf.clone(),
                                    style: RowStyleKey::Spans(row_spans.clone()),
                                };
                                // SAFETY: `cr` is valid on this thread.
                                blitted_from_cache = unsafe {
                                    try_blit_or_cache_row_strip(
                                        cr,
                                        key,
                                        norm_fam,
                                        *font_size_pt,
                                        *bold,
                                        cell_w,
                                        cell_h,
                                        baseline_px,
                                        batch_cells,
                                        scale_x,
                                        scale_y,
                                        draw_x,
                                        draw_y,
                                    )
                                };
                            }
                            if !blitted_from_cache {
                                let batch_layout = if *italic {
                                    pango_scope
                                        .configure_layout(font_family, *font_size_pt, *bold, false)
                                        .map(|(l, _, _)| l)
                                        .unwrap_or(layout)
                                } else {
                                    layout
                                };
                                set_source_color(cr, *color);
                                // SAFETY: `cr` and `batch_layout` are valid on this thread.
                                unsafe {
                                    draw_ascii_spans_direct(
                                        cr,
                                        batch_layout,
                                        &row_buf,
                                        &row_spans,
                                        draw_x,
                                        draw_y,
                                    );
                                }
                            }
                        }
                        if !batch_underlines.is_empty() {
                            let uy = (*y + baseline_px + 1.5).floor() + 0.5;
                            // SAFETY: `cr` is valid; strokes crisp 1px underlines over each underlined span.
                            unsafe {
                                ffi::cairo_set_line_width(cr, 1.0);
                                for &(ux, ucells, ucol) in &batch_underlines {
                                    set_source_color(cr, ucol);
                                    ffi::cairo_new_path(cr);
                                    ffi::cairo_move_to(cr, ux, uy);
                                    ffi::cairo_line_to(cr, ux + (ucells as f64) * cell_w, uy);
                                    ffi::cairo_stroke(cr);
                                }
                            }
                        }
                        batch_cells
                    } else {
                        if text.bytes().any(|b| b > b' ' && b != 0x7F) {
                            let use_cache = cache_scale_eligible
                                && text.len() >= 2
                                && text.len() <= MAX_CACHED_ROW_TEXT_BYTES;
                            let mut blitted_from_cache = false;
                            if use_cache {
                                let style = RowStyleKey::Single {
                                    color: *color,
                                    italic: *italic,
                                };
                                let key = RowRasterKey {
                                    family: norm_fam.to_string(),
                                    size_pango,
                                    bold: *bold,
                                    scale_x_milli,
                                    scale_y_milli,
                                    text: text.clone(),
                                    style,
                                };
                                // SAFETY: `cr` is valid on this thread.
                                blitted_from_cache = unsafe {
                                    try_blit_or_cache_row_strip(
                                        cr,
                                        key,
                                        norm_fam,
                                        *font_size_pt,
                                        *bold,
                                        cell_w,
                                        cell_h,
                                        baseline_px,
                                        first_cells,
                                        scale_x,
                                        scale_y,
                                        draw_x,
                                        draw_y,
                                    )
                                };
                            }
                            if !blitted_from_cache {
                                set_source_color(cr, *color);
                                let byte_len = c_int::try_from(text.len()).unwrap_or(c_int::MAX);
                                // SAFETY: `layout` and `cr` are valid non-null objects on this thread.
                                unsafe {
                                    ffi::pango_layout_set_text(
                                        layout,
                                        text.as_ptr().cast(),
                                        byte_len,
                                    );
                                    ffi::cairo_new_path(cr);
                                    ffi::cairo_move_to(cr, draw_x, draw_y);
                                    ffi::pango_cairo_show_layout(cr, layout);
                                }
                            }
                        }
                        first_cells
                    }
                } else {
                    let cells = count_text_cells(text);
                    let use_cache = cache_scale_eligible
                        && cells > 0
                        && text.len() <= MAX_CACHED_ROW_TEXT_BYTES;
                    let mut blitted_from_cache = false;
                    if use_cache {
                        let style = RowStyleKey::Single {
                            color: *color,
                            italic: *italic,
                        };
                        let key = RowRasterKey {
                            family: norm_fam.to_string(),
                            size_pango,
                            bold: *bold,
                            scale_x_milli,
                            scale_y_milli,
                            text: text.clone(),
                            style,
                        };
                        // SAFETY: `cr` is valid on this thread.
                        blitted_from_cache = unsafe {
                            try_blit_or_cache_row_strip(
                                cr,
                                key,
                                norm_fam,
                                *font_size_pt,
                                *bold,
                                cell_w,
                                cell_h,
                                baseline_px,
                                cells,
                                scale_x,
                                scale_y,
                                draw_x,
                                draw_y,
                            )
                        };
                    }
                    if !blitted_from_cache {
                        set_source_color(cr, *color);
                        // SAFETY: `cr` and `layout` are valid on this thread.
                        unsafe {
                            let _ = draw_mixed_text_direct(
                                cr,
                                layout,
                                text,
                                *x,
                                *y,
                                cell_w,
                                baseline_px,
                                scale_x,
                                scale_y,
                            );
                        }
                    }
                    cells
                };

                if !batch_handled_underlines && *underline && total_cells > 0 {
                    set_source_color(cr, *color);
                    let uy = (*y + baseline_px + 1.5).floor() + 0.5;
                    let ux_end = *x + (total_cells as f64) * cell_w;
                    // SAFETY: `cr` is valid; draws a crisp 1px horizontal underline over `total_cells * cell_w`.
                    unsafe {
                        ffi::cairo_set_line_width(cr, 1.0);
                        ffi::cairo_new_path(cr);
                        ffi::cairo_move_to(cr, *x, uy);
                        ffi::cairo_line_to(cr, ux_end, uy);
                        ffi::cairo_stroke(cr);
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
                if skip_caret_idx == Some(cmd_idx) {
                    continue;
                }
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
    let Some((layout, _, _)) = scope.configure_layout(family, size_pt, false, false) else {
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

    use super::{
        OffscreenSurface, THREAD_ROW_RASTER_CACHE, TextSegment, build_font_family_chain,
        codepoint_cell_width, for_each_text_segment, measure_monospace_font,
        render_frame_to_cairo_with_opts, render_frame_to_png,
        render_offscreen_incremental_with_opts, reset_row_raster_cache_stats,
        row_raster_cache_stats,
    };
    use crate::ffi;

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

    #[test]
    fn nerd_font_fallback_chain_and_cell_width_segmentation_align_cjk_emoji_and_pua() {
        assert_eq!(
            build_font_family_chain("Fira Code").as_ref(),
            "Fira Code, Symbols Nerd Font Mono, Symbols Nerd Font, Noto Sans Mono CJK SC, Monospace"
        );
        assert_eq!(
            build_font_family_chain("FiraCode Nerd Font Mono").as_ref(),
            "FiraCode Nerd Font Mono"
        );
        assert_eq!(
            build_font_family_chain("Symbols Nerd Font Mono").as_ref(),
            "Symbols Nerd Font Mono"
        );

        // Verify cell width rules: 0 combining/ZW, 2 CJK/fullwidth/dingbats/emoji, 1 PUA & box-drawing.
        assert_eq!(codepoint_cell_width('\u{0301}'), 0);
        assert_eq!(codepoint_cell_width('\u{200D}'), 0);
        assert_eq!(codepoint_cell_width('中'), 2);
        assert_eq!(codepoint_cell_width('Ａ'), 2);
        assert_eq!(codepoint_cell_width('✅'), 2);
        assert_eq!(codepoint_cell_width('❌'), 2);
        assert_eq!(codepoint_cell_width('✨'), 2);
        assert_eq!(codepoint_cell_width('⚡'), 2);
        assert_eq!(codepoint_cell_width('⭐'), 2);
        assert_eq!(codepoint_cell_width('🚀'), 2);
        assert_eq!(codepoint_cell_width('\u{E0A0}'), 1); // Powerline PUA
        assert_eq!(codepoint_cell_width('\u{E7A8}'), 1); // Nerd Font Devicon PUA
        assert_eq!(codepoint_cell_width('\u{F0219}'), 1); // Nerd Font v3 Material PUA-A
        assert_eq!(codepoint_cell_width('─'), 1);
        assert_eq!(codepoint_cell_width('▸'), 1);

        let mixed = "ab\u{E7A8}中文👋\u{1F3FD}✅|";
        let mut segs = Vec::new();
        for_each_text_segment(mixed, |s| segs.push(s));
        assert_eq!(
            segs,
            vec![
                TextSegment::Ascii("ab"),
                TextSegment::Cluster {
                    text: "\u{E7A8}",
                    cells: 1,
                },
                TextSegment::Cluster {
                    text: "中",
                    cells: 2,
                },
                TextSegment::Cluster {
                    text: "文",
                    cells: 2,
                },
                TextSegment::Cluster {
                    text: "👋\u{1F3FD}",
                    cells: 2, // skin-tone modifier adds 0 extra cells!
                },
                TextSegment::Cluster {
                    text: "✅",
                    cells: 2,
                },
                TextSegment::Ascii("|"),
            ]
        );
    }

    #[test]
    fn fira_code_renders_nerd_font_pua_cjk_underline_on_grid_and_skip_caret_works() {
        let metrics = measure_monospace_font("Fira Code", 14.0);
        assert!(metrics.cell_width > 4.0 && metrics.cell_height > 8.0);
        let cw = metrics.cell_width;

        // Place Nerd Font PUA `\u{E7A8}` (col 0, 1 cell) + `中文` (cols 1..5, 4 cells) + `M` (col 5, 1 cell) = 6 cells.
        let bg = ColorRgba::from_rgba(20, 20, 30, 255);
        let fg = ColorRgba::from_rgba(240, 240, 250, 255);
        let caret_col = ColorRgba::from_rgba(255, 64, 64, 255);
        let frame = RenderFrame {
            generation: 7,
            window: WindowConfig {
                title: "Grid Align".to_string(),
                width: 240,
                height: 80,
                background: bg,
                ..WindowConfig::default()
            },
            commands: vec![
                DrawCmd::TextRun {
                    x: 10.0,
                    y: 10.0,
                    text: "\u{E7A8}中文M".to_string(),
                    font_family: "Fira Code".to_string(),
                    font_size_pt: 14.0,
                    color: fg,
                    bold: false,
                    italic: false,
                    underline: true,
                },
                DrawCmd::CursorCaret {
                    x: 180.0,
                    y: 10.0,
                    w: 10.0,
                    h: 20.0,
                    shape: CaretShape::Block,
                    color: caret_col,
                },
            ],
        };

        let surf_full = OffscreenSurface::new(240, 80).expect("surface");
        let surf_skip = OffscreenSurface::new(240, 80).expect("surface");
        // SAFETY: Both offscreen Cairo contexts are valid and uniquely owned.
        unsafe {
            render_frame_to_cairo_with_opts(surf_full.cr, 240.0, 80.0, &frame, None);
            render_frame_to_cairo_with_opts(surf_skip.cr, 240.0, 80.0, &frame, Some(1));
        }

        let bg_px = surf_full.read_argb_pixel(2, 2);
        // 1. Nerd Font PUA icon at col 0 (`x = 10 .. 10 + cw`) must have rendered non-background ink!
        let mut pua_ink = 0usize;
        for py in 10..34 {
            for px in 10..(10.0 + cw).ceil() as i32 {
                if surf_full.read_argb_pixel(px, py) != bg_px {
                    pua_ink += 1;
                }
            }
        }
        assert!(
            pua_ink > 4,
            "expected Nerd Font PUA glyph ink in col 0, got {pua_ink} px"
        );

        // 2. Trailing `'M'` at exact col 5 (`x = 10 + 5*cw .. 10 + 6*cw`) must have rendered ink,
        //    and nothing should spill past `10 + 6*cw + 2`!
        let col5_start = (10.0 + 5.0 * cw).floor() as i32;
        let col6_end = (10.0 + 6.0 * cw).ceil() as i32;
        let mut m_ink = 0usize;
        for py in 10..34 {
            for px in col5_start..col6_end {
                if surf_full.read_argb_pixel(px, py) != bg_px {
                    m_ink += 1;
                }
            }
        }
        assert!(m_ink > 6, "expected trailing 'M' ink aligned at col 5..6");
        for py in 10..40 {
            assert_eq!(
                surf_full.read_argb_pixel(col6_end + 3, py),
                bg_px,
                "underline or glyph must stop cleanly at total_cells * cell_w"
            );
        }

        // 3. `skip_caret_idx = Some(1)` omits primary caret while `None` (and PNG export) draws it!
        assert_ne!(surf_full.read_argb_pixel(185, 20), bg_px);
        assert_eq!(surf_skip.read_argb_pixel(185, 20), bg_px);
    }

    #[test]
    fn cjk_vertical_alignment_matches_ascii_baseline() {
        let metrics = measure_monospace_font("Fira Code", 14.0);
        let cw = metrics.cell_width;
        let bg = ColorRgba::from_rgba(20, 20, 30, 255);
        let fg = ColorRgba::from_rgba(240, 240, 250, 255);
        let frame = RenderFrame {
            generation: 8,
            window: WindowConfig {
                title: "CJK Baseline".to_string(),
                width: 160,
                height: 80,
                background: bg,
                ..WindowConfig::default()
            },
            commands: vec![DrawCmd::TextRun {
                x: 10.0,
                y: 20.0,
                text: "H中M".to_string(),
                font_family: "Fira Code".to_string(),
                font_size_pt: 14.0,
                color: fg,
                bold: false,
                italic: false,
                underline: false,
            }],
        };

        let surf = OffscreenSurface::new(160, 80).expect("surface");
        // SAFETY: `surf.cr` is a valid, uniquely owned offscreen Cairo context.
        unsafe {
            render_frame_to_cairo_with_opts(surf.cr, 160.0, 80.0, &frame, None);
        }

        let bg_px = surf.read_argb_pixel(2, 2);
        let vertical_bounds = |x_start: i32, x_end: i32| -> (i32, i32) {
            let mut min_y = i32::MAX;
            let mut max_y = i32::MIN;
            for py in 10..60 {
                for px in x_start..x_end {
                    if surf.read_argb_pixel(px, py) != bg_px {
                        min_y = min_y.min(py);
                        max_y = max_y.max(py);
                    }
                }
            }
            (min_y, max_y)
        };

        let (h_min, h_max) = vertical_bounds(10, (10.0 + cw).ceil() as i32);
        let (cjk_min, cjk_max) =
            vertical_bounds((10.0 + cw).floor() as i32, (10.0 + 3.0 * cw).ceil() as i32);
        let (m_min, m_max) = vertical_bounds(
            (10.0 + 3.0 * cw).floor() as i32,
            (10.0 + 4.0 * cw).ceil() as i32,
        );

        assert!(h_min < h_max, "expected 'H' ink");
        assert!(cjk_min < cjk_max, "expected '中' ink");
        assert!(m_min < m_max, "expected 'M' ink");

        let ascii_mid = f64::from(h_min + h_max + m_min + m_max) / 4.0;
        let cjk_mid = f64::from(cjk_min + cjk_max) / 2.0;
        assert!(
            (cjk_mid - ascii_mid).abs() <= 2.5,
            "CJK vertical midpoint ({cjk_mid:.2}) drifted from ASCII midpoint ({ascii_mid:.2}); bounds H=({h_min}..={h_max}) CJK=({cjk_min}..={h_max}) M=({m_min}..={m_max})"
        );
    }

    #[test]
    fn row_raster_cache_hits_across_scrolled_y_and_gdk_texture_zero_copy_tracks_refcount() {
        reset_row_raster_cache_stats();
        let metrics = measure_monospace_font("Monospace", 13.0);
        let cw = metrics.cell_width;
        let bg = ColorRgba::from_rgba(30, 30, 46, 255);
        let kw = ColorRgba::from_rgba(203, 166, 247, 255);
        let fg = ColorRgba::from_rgba(205, 214, 244, 255);

        let op = ColorRgba::from_rgba(137, 220, 235, 255);

        let build_scroll_frame = |generation: u64, top_row: usize, edit_row5: &str| {
            let mut cmds = Vec::new();
            for vis in 0..20usize {
                let doc_row = top_row + vis;
                let y = 24.0 + (vis as f64) * 20.0;
                let rhs = if doc_row == 5 {
                    edit_row5.to_string()
                } else {
                    format!("row_{doc_row}_identifier(x + 1) // 注释 {doc_row}")
                };
                cmds.push(DrawCmd::TextRun {
                    x: 16.0,
                    y,
                    text: "let ".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: kw,
                    bold: false,
                    italic: false,
                    underline: false,
                });
                cmds.push(DrawCmd::TextRun {
                    x: 16.0 + 4.0 * cw,
                    y,
                    text: "x = ".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: op,
                    bold: false,
                    italic: false,
                    underline: false,
                });
                cmds.push(DrawCmd::TextRun {
                    x: 16.0 + 8.0 * cw,
                    y,
                    text: rhs,
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: fg,
                    bold: false,
                    italic: false,
                    underline: false,
                });
            }
            RenderFrame {
                generation,
                window: WindowConfig {
                    title: "Cache Test".to_string(),
                    width: 640,
                    height: 460,
                    background: bg,
                    ..WindowConfig::default()
                },
                commands: cmds,
            }
        };

        let mut surf = OffscreenSurface::new(640, 460).expect("surface");
        assert!(surf.is_uniquely_owned());

        // Zero-copy GdkMemoryTexture holds a cairo_surface_reference while alive, then releases it.
        let tex = surf.create_gdk_texture().expect("gdk memory texture");
        assert!(
            !surf.is_uniquely_owned(),
            "surface must report shared ownership while GdkMemoryTexture is alive"
        );
        drop(tex);
        assert!(
            surf.is_uniquely_owned(),
            "dropping GdkMemoryTexture must restore unique surface ownership"
        );

        let f0 = build_scroll_frame(1, 0, "original_value");
        // SAFETY: `surf` is uniquely owned on this thread.
        unsafe {
            render_offscreen_incremental_with_opts(&mut surf, &f0, None);
        }
        let (hits0, misses0, cached_cnt0) = row_raster_cache_stats();
        // 19 CJK-commented rows share the batched ASCII prefix `"let x = "` (1 miss + 18 hits) plus
        // 19 distinct mixed-UTF8 strips, and `doc_row == 5` forms 1 merged 3-span ASCII strip.
        assert_eq!(hits0, 18);
        assert_eq!(misses0, 21);
        assert_eq!(cached_cnt0, 21);

        // Scroll down by 1 row (`top_row = 1`): all 19 surviving rows moved to a NEW `y` and MUST
        // hit `THREAD_ROW_RASTER_CACHE` (both batched ASCII prefixes and mixed CJK strips), with
        // ONLY the 1 newly revealed row (`doc_row == 20`) rasterizing its new `rhs` strip!
        let f1 = build_scroll_frame(2, 1, "original_value");
        // SAFETY: `surf` is uniquely owned on this thread.
        unsafe {
            render_offscreen_incremental_with_opts(&mut surf, &f1, None);
        }
        let (hits1, misses1, _) = row_raster_cache_stats();
        assert_eq!(
            hits1 - hits0,
            38,
            "expected all scrolled strips at new Y coordinates to hit RowRasterCache"
        );
        assert_eq!(
            misses1 - misses0,
            1,
            "expected only the 1 newly revealed row to miss RowRasterCache"
        );

        // Edit a single row (`doc_row == 5`) without scrolling (`top_row = 1`): incremental dirty
        // band clipping must match a full scratch render pixel-for-pixel AND skip clean bands!
        let f2 = build_scroll_frame(3, 1, "edited_value_xyz");
        // SAFETY: `surf` is uniquely owned on this thread.
        unsafe {
            render_offscreen_incremental_with_opts(&mut surf, &f2, None);
        }
        let (hits2, misses2, _) = row_raster_cache_stats();
        assert_eq!(
            misses2 - misses1,
            1,
            "editing 1 line must rasterize only 1 new strip"
        );
        assert!(
            hits2 - hits1 <= 12,
            "incremental dirty-band clip must skip clean rows outside the edited band (got {} hits out of 38)",
            hits2 - hits1
        );

        let scratch = OffscreenSurface::new(640, 460).expect("scratch");
        // SAFETY: `scratch.cr` is valid.
        unsafe {
            render_frame_to_cairo_with_opts(scratch.cr, 640.0, 460.0, &f2, None);
        }
        for py in (0..460).step_by(7) {
            for px in (0..640).step_by(11) {
                assert_eq!(
                    surf.read_argb_pixel(px, py),
                    scratch.read_argb_pixel(px, py),
                    "incremental render diverged from full render at ({px}, {py})"
                );
            }
        }
    }

    #[test]
    fn fractional_scale_and_subpixel_aa_row_strips_preserve_valid_premultiplied_alpha_without_tearing()
     {
        for scale in [1.25_f64, 1.5_f64, 1.75_f64] {
            reset_row_raster_cache_stats();
            let mut surf = OffscreenSurface::new_scaled(600, 120, scale, scale).expect("frac surf");
            // Simulate KDE/GTK `gtk-xft-rgba=rgb` subpixel AA on the parent context.
            // SAFETY: `surf.cr` is valid; `opts` is destroyed immediately after setting.
            unsafe {
                let opts = ffi::cairo_font_options_create();
                if !opts.is_null() {
                    ffi::cairo_font_options_set_antialias(opts, ffi::CAIRO_ANTIALIAS_SUBPIXEL);
                    ffi::cairo_set_font_options(surf.cr, opts);
                    ffi::cairo_font_options_destroy(opts);
                }
            }
            let bg = ColorRgba::from_rgba(88, 91, 112, 255);
            let fg = ColorRgba::from_rgba(240, 244, 255, 255);
            let frame = RenderFrame {
                generation: 1,
                window: WindowConfig {
                    title: "Fractional Scale".to_string(),
                    width: 600,
                    height: 120,
                    background: bg,
                    ..WindowConfig::default()
                },
                commands: vec![
                    DrawCmd::TextRun {
                        x: 13.3,
                        y: 18.7,
                        text: "pub fn render_glyph_strip(scale: Float) -> Bool { true }"
                            .to_string(),
                        font_family: "Monospace".to_string(),
                        font_size_pt: 13.0,
                        color: fg,
                        bold: false,
                        italic: false,
                        underline: false,
                    },
                    DrawCmd::TextRun {
                        x: 13.3,
                        y: 46.7,
                        text: "// 分数缩放 125% / 150% 清晰无破损测试".to_string(),
                        font_family: "Monospace".to_string(),
                        font_size_pt: 13.0,
                        color: fg,
                        bold: false,
                        italic: true,
                        underline: false,
                    },
                ],
            };
            // SAFETY: `surf` is uniquely owned on this thread.
            unsafe {
                render_offscreen_incremental_with_opts(&mut surf, &frame, None);
            }
            // Verify every cached strip pixel obeys Cairo's premultiplied invariant `R,G,B <= A`.
            THREAD_ROW_RASTER_CACHE.with(|cell| {
                let cache = cell.borrow();
                assert!(!cache.entries.is_empty());
                for strip in cache.entries.values() {
                    // SAFETY: `strip.surface` is flushed and valid.
                    unsafe {
                        let data = ffi::cairo_image_surface_get_data(strip.surface);
                        let stride = ffi::cairo_image_surface_get_stride(strip.surface) as usize;
                        for py in 0..(strip.phys_h as usize) {
                            let row = data.add(py * stride);
                            for px in 0..(strip.phys_w as usize) {
                                let argb = row.add(px * 4).cast::<u32>().read_unaligned();
                                let a = (argb >> 24) & 0xff;
                                let r = (argb >> 16) & 0xff;
                                let g = (argb >> 8) & 0xff;
                                let b = argb & 0xff;
                                assert!(
                                    r <= a && g <= a && b <= a,
                                    "broken premultiplied alpha ({r},{g},{b} > {a}) at ({px},{py}) scale={scale}"
                                );
                            }
                        }
                    }
                }
            });
            // Verify `surf` has zero dark wraparound pixels (`channel < 80` on `[88,91,112]` -> `[240,244,255]` blend!).
            // SAFETY: `surf.surface` is valid.
            unsafe {
                ffi::cairo_surface_flush(surf.surface);
                let data = ffi::cairo_image_surface_get_data(surf.surface);
                let stride = ffi::cairo_image_surface_get_stride(surf.surface) as usize;
                for py in 0..(surf.phys_h as usize) {
                    let row = data.add(py * stride);
                    for px in 0..(surf.phys_w as usize) {
                        let argb = row.add(px * 4).cast::<u32>().read_unaligned();
                        let r = (argb >> 16) & 0xff;
                        let g = (argb >> 8) & 0xff;
                        let b = argb & 0xff;
                        assert!(
                            r >= 86 && g >= 89 && b >= 110,
                            "detected dark glyph corruption ({r},{g},{b}) at ({px},{py}) scale={scale}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn word_underline_never_shifts_preceding_or_following_tokens_across_scales() {
        let metrics = measure_monospace_font("Monospace", 13.0);
        let cw = metrics.cell_width;
        let bg = ColorRgba::from_rgba(30, 30, 46, 255);
        let kw = ColorRgba::from_rgba(203, 166, 247, 255);
        let ident = ColorRgba::from_rgba(205, 214, 244, 255);
        let accent = ColorRgba::from_rgba(137, 180, 250, 255);
        let op = ColorRgba::from_rgba(148, 226, 213, 255);

        let build_frame = |generation: u64, underline_word: bool| {
            let word_col = if underline_word { accent } else { ident };
            let mut cmds = vec![
                // Row 0: multi-token syntax line `"    let target_word = compute_result(alpha, beta);"`
                DrawCmd::TextRun {
                    x: 16.0 + 4.0 * cw,
                    y: 20.0,
                    text: "let ".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: kw,
                    bold: false,
                    italic: false,
                    underline: false,
                },
                DrawCmd::TextRun {
                    x: 16.0 + 8.0 * cw,
                    y: 20.0,
                    text: "target_word".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: word_col,
                    bold: false,
                    italic: false,
                    underline: underline_word,
                },
                DrawCmd::TextRun {
                    x: 16.0 + 19.0 * cw,
                    y: 20.0,
                    text: " = ".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: op,
                    bold: false,
                    italic: false,
                    underline: false,
                },
                DrawCmd::TextRun {
                    x: 16.0 + 22.0 * cw,
                    y: 20.0,
                    text: "compute_result(alpha, beta, gamma);".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: ident,
                    bold: false,
                    italic: false,
                    underline: false,
                },
            ];
            // Row 1: single comment run split into 3 runs when `underline_word` is active
            if !underline_word {
                cmds.push(DrawCmd::TextRun {
                    x: 16.0,
                    y: 50.0,
                    text: "// verify target_word keeps trailing_tokens_exact_here".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: ident,
                    bold: false,
                    italic: true,
                    underline: false,
                });
            } else {
                cmds.push(DrawCmd::TextRun {
                    x: 16.0,
                    y: 50.0,
                    text: "// verify ".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: ident,
                    bold: false,
                    italic: true,
                    underline: false,
                });
                cmds.push(DrawCmd::TextRun {
                    x: 16.0 + 10.0 * cw,
                    y: 50.0,
                    text: "target_word".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: accent,
                    bold: false,
                    italic: true,
                    underline: true,
                });
                cmds.push(DrawCmd::TextRun {
                    x: 16.0 + 21.0 * cw,
                    y: 50.0,
                    text: " keeps trailing_tokens_exact_here".to_string(),
                    font_family: "Monospace".to_string(),
                    font_size_pt: 13.0,
                    color: ident,
                    bold: false,
                    italic: true,
                    underline: false,
                });
            }
            // Row 2: bold + CJK mixed line where runs cross non-merged boundaries
            cmds.push(DrawCmd::TextRun {
                x: 16.0,
                y: 76.0,
                text: "fn ".to_string(),
                font_family: "Monospace".to_string(),
                font_size_pt: 13.0,
                color: kw,
                bold: true,
                italic: false,
                underline: false,
            });
            cmds.push(DrawCmd::TextRun {
                x: 16.0 + 3.0 * cw,
                y: 76.0,
                text: "target_word".to_string(),
                font_family: "Monospace".to_string(),
                font_size_pt: 13.0,
                color: word_col,
                bold: false,
                italic: false,
                underline: underline_word,
            });
            cmds.push(DrawCmd::TextRun {
                x: 16.0 + 14.0 * cw,
                y: 76.0,
                text: "(x: Int) // 对齐测试 tail".to_string(),
                font_family: "Monospace".to_string(),
                font_size_pt: 13.0,
                color: ident,
                bold: false,
                italic: false,
                underline: false,
            });
            RenderFrame {
                generation,
                window: WindowConfig {
                    title: "Underline Invariance".to_string(),
                    width: 640,
                    height: 100,
                    background: bg,
                    ..WindowConfig::default()
                },
                commands: cmds,
            }
        };

        for scale in [1.0_f64, 1.25_f64, 1.5_f64, 2.0_f64] {
            reset_row_raster_cache_stats();
            let mut plain = OffscreenSurface::new_scaled(640, 100, scale, scale).expect("plain");
            let mut ul = OffscreenSurface::new_scaled(640, 100, scale, scale).expect("ul");
            let f_plain = build_frame(1, false);
            let f_ul = build_frame(2, true);
            // SAFETY: Both surfaces are uniquely owned on this thread.
            unsafe {
                render_offscreen_incremental_with_opts(&mut plain, &f_plain, None);
                render_offscreen_incremental_with_opts(&mut ul, &f_ul, None);
                ffi::cairo_surface_flush(plain.surface);
                ffi::cairo_surface_flush(ul.surface);
                let data_p = ffi::cairo_image_surface_get_data(plain.surface);
                let data_u = ffi::cairo_image_surface_get_data(ul.surface);
                let stride = ffi::cairo_image_surface_get_stride(plain.surface) as usize;
                // Check all columns strictly after `target_word` (`x >= 16 + 21.5 * cw`):
                // EVERY SINGLE PHYSICAL PIXEL on BOTH rows must be 100% bit-for-bit identical!
                let check_px_start = ((16.0 + 21.5 * cw) * scale).ceil() as usize;
                for py in 0..(plain.phys_h as usize) {
                    let rp = data_p.add(py * stride);
                    let ru = data_u.add(py * stride);
                    for px in check_px_start..(plain.phys_w as usize) {
                        let vp = rp.add(px * 4).cast::<u32>().read_unaligned();
                        let vu = ru.add(px * 4).cast::<u32>().read_unaligned();
                        assert_eq!(
                            vp, vu,
                            "trailing text pixel shifted at physical ({px}, {py}) scale={scale}"
                        );
                    }
                }
            }
        }
    }
}
