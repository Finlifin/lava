//! Native Win32 GDI + TrueType (`cmap` + `GetGlyphOutlineW`) 2D scene renderer, bundled
//! Nerd Font / Fira Code private font loader, smooth-scroll & 4-corner cursor smear compositor,
//! and zero-dependency PNG exporter for Windows 10 1709+.

use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;
use std::sync::OnceLock;

use lava_proto::{
    AnimHints, CaretShape, ColorRgba, DrawCmd, FontMetrics, RenderFrame, ScrollViewport,
    TransferOverlay,
};

use crate::win_ffi::{
    self, ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, FR_PRIVATE,
    FW_BOLD, FW_NORMAL, GDI_ERROR, GGI_MARK_NONEXISTING_GLYPHS, GGO_GLYPH_INDEX, GGO_GRAY8_BITMAP,
    GLYPHMETRICS, HDC, HFONT, MAT2, OUT_TT_PRECIS, SIZE, TEXTMETRICW, to_wide_nul,
};

const CMAP_TAG_LE: u32 = u32::from_le_bytes(*b"cmap");

static BUNDLED_FONTS_COUNT: OnceLock<usize> = OnceLock::new();

fn candidate_font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(explicit) = env::var("LAVA_FONTS_DIR")
        && !explicit.trim().is_empty()
    {
        dirs.push(PathBuf::from(explicit.trim()));
    }
    if let Ok(exe) = env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        dirs.push(exe_dir.join("fonts"));
        dirs.push(exe_dir.join("..").join("fonts"));
        dirs.push(exe_dir.join("..").join("share").join("lava").join("fonts"));
        let mut cur = Some(exe_dir);
        while let Some(d) = cur {
            dirs.push(d.join("dist").join("lava-windows-x86_64").join("fonts"));
            dirs.push(d.join("target").join("lava-bundled-fonts"));
            dirs.push(d.join("fonts"));
            cur = d.parent();
        }
    }
    if let Ok(cwd) = env::current_dir() {
        let mut cur = Some(cwd.as_path());
        while let Some(d) = cur {
            dirs.push(d.join("dist").join("lava-windows-x86_64").join("fonts"));
            dirs.push(d.join("target").join("lava-bundled-fonts"));
            dirs.push(d.join("fonts"));
            cur = d.parent();
        }
    }
    dirs
}

/// Registers all bundled `.ttf` / `.otf` font files (`Fira Code`, `FiraCode Nerd Font Mono`,
/// `Symbols Nerd Font Mono`) privately into the current process via `AddFontResourceExW(..., FR_PRIVATE, 0)`.
pub fn ensure_bundled_fonts_registered() -> usize {
    *BUNDLED_FONTS_COUNT.get_or_init(|| {
        let mut loaded_files = 0usize;
        let mut seen_names = std::collections::HashSet::new();
        for dir in candidate_font_dirs() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension().and_then(|x| x.to_str()).is_some_and(|ext| {
                            ext.eq_ignore_ascii_case("ttf") || ext.eq_ignore_ascii_case("otf")
                        })
                })
                .collect();
            paths.sort();
            for path in paths {
                let fname = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                if !seen_names.insert(fname) {
                    continue;
                }
                let wide = to_wide_nul(&path.to_string_lossy());
                // SAFETY: `wide` is NUL-terminated UTF-16; `FR_PRIVATE` registers the font file
                // exclusively for this process without modifying user/system registry state.
                let added = unsafe {
                    win_ffi::AddFontResourceExW(wide.as_ptr(), FR_PRIVATE, ptr::null_mut())
                };
                if added > 0 {
                    loaded_files += 1;
                }
            }
        }
        loaded_files
    })
}

// ---------------------------------------------------------------------------
// 1. OpenType `cmap` Parser (BMP Format 4 + Full 32-bit UCS-4 Format 12)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FontCmap {
    fmt12_groups: Vec<(u32, u32, u32)>,
    fmt4_raw: Vec<u8>,
    fmt4_sub_off: usize,
}

fn be_u16(buf: &[u8], off: usize) -> Option<u16> {
    let s = buf.get(off..off + 2)?;
    Some(u16::from_be_bytes([s[0], s[1]]))
}

fn be_i16(buf: &[u8], off: usize) -> Option<i16> {
    let s = buf.get(off..off + 2)?;
    Some(i16::from_be_bytes([s[0], s[1]]))
}

fn be_u32(buf: &[u8], off: usize) -> Option<u32> {
    let s = buf.get(off..off + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

impl FontCmap {
    fn from_hdc(hdc: HDC) -> Self {
        // SAFETY: `hdc` has an `HFONT` selected; querying `GetFontData` with `null` returns byte size.
        let size = unsafe { win_ffi::GetFontData(hdc, CMAP_TAG_LE, 0, ptr::null_mut(), 0) };
        if size == GDI_ERROR || !(12..=16 * 1024 * 1024).contains(&size) {
            return Self::default();
        }
        let mut raw = vec![0u8; size as usize];
        // SAFETY: `raw` has exact length `size`.
        let got =
            unsafe { win_ffi::GetFontData(hdc, CMAP_TAG_LE, 0, raw.as_mut_ptr().cast(), size) };
        if got != size {
            return Self::default();
        }
        Self::parse(raw)
    }

    fn parse(raw: Vec<u8>) -> Self {
        let Some(num_tables) = be_u16(&raw, 2) else {
            return Self::default();
        };
        let mut fmt12_off: Option<usize> = None;
        let mut fmt4_off: Option<usize> = None;

        for i in 0..usize::from(num_tables) {
            let rec = 4 + i * 8;
            let Some(plat) = be_u16(&raw, rec) else { break };
            let Some(enc) = be_u16(&raw, rec + 2) else {
                break;
            };
            let Some(sub_off_u32) = be_u32(&raw, rec + 4) else {
                break;
            };
            let sub_off = sub_off_u32 as usize;
            let Some(fmt) = be_u16(&raw, sub_off) else {
                continue;
            };

            let is_unicode = plat == 0 || (plat == 3 && (enc == 1 || enc == 10));
            if !is_unicode {
                continue;
            }
            if fmt == 12 && fmt12_off.is_none() {
                fmt12_off = Some(sub_off);
            } else if fmt == 4 && fmt4_off.is_none() {
                fmt4_off = Some(sub_off);
            }
        }

        let mut fmt12_groups = Vec::new();
        if let Some(off) = fmt12_off
            && let Some(n_groups) = be_u32(&raw, off + 12)
        {
            let cap = (n_groups as usize).min(200_000);
            fmt12_groups.reserve(cap);
            for g in 0..cap {
                let goff = off + 16 + g * 12;
                let (Some(s), Some(e), Some(gid)) = (
                    be_u32(&raw, goff),
                    be_u32(&raw, goff + 4),
                    be_u32(&raw, goff + 8),
                ) else {
                    break;
                };
                fmt12_groups.push((s, e, gid));
            }
        }

        let fmt4_sub_off = fmt4_off.unwrap_or(0);
        let fmt4_raw = if fmt4_off.is_some() { raw } else { Vec::new() };
        Self {
            fmt12_groups,
            fmt4_raw,
            fmt4_sub_off,
        }
    }

    fn lookup(&self, cp: u32) -> Option<u16> {
        if !self.fmt12_groups.is_empty() {
            let idx = self.fmt12_groups.partition_point(|&(s, _, _)| s <= cp);
            if idx > 0 {
                let (s, e, start_gid) = self.fmt12_groups[idx - 1];
                if cp >= s && cp <= e {
                    let gid = start_gid.wrapping_add(cp - s);
                    if gid != 0 && gid <= u32::from(u16::MAX) {
                        return Some(gid as u16);
                    }
                }
            }
        }
        if cp <= 0xFFFF && !self.fmt4_raw.is_empty() {
            return self.lookup_fmt4(cp as u16);
        }
        None
    }

    fn lookup_fmt4(&self, code: u16) -> Option<u16> {
        let off = self.fmt4_sub_off;
        let seg_count_x2 = usize::from(be_u16(&self.fmt4_raw, off + 6)?);
        let seg_count = seg_count_x2 / 2;
        if seg_count == 0 {
            return None;
        }
        let end_codes = off + 14;
        let start_codes = end_codes + seg_count_x2 + 2;
        let id_deltas = start_codes + seg_count_x2;
        let id_range_offsets = id_deltas + seg_count_x2;

        // Binary search `endCode` table
        let mut lo = 0usize;
        let mut hi = seg_count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let end_c = be_u16(&self.fmt4_raw, end_codes + mid * 2)?;
            if end_c < code {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo >= seg_count {
            return None;
        }
        let end_c = be_u16(&self.fmt4_raw, end_codes + lo * 2)?;
        let start_c = be_u16(&self.fmt4_raw, start_codes + lo * 2)?;
        if code < start_c || code > end_c {
            return None;
        }
        let delta = be_i16(&self.fmt4_raw, id_deltas + lo * 2)?;
        let ro_pos = id_range_offsets + lo * 2;
        let range_off = be_u16(&self.fmt4_raw, ro_pos)?;
        let gid = if range_off == 0 {
            (i32::from(code) + i32::from(delta)) as u16
        } else {
            let glyph_off = ro_pos + usize::from(range_off) + usize::from(code - start_c) * 2;
            let raw_gid = be_u16(&self.fmt4_raw, glyph_off)?;
            if raw_gid == 0 {
                0
            } else {
                (i32::from(raw_gid) + i32::from(delta)) as u16
            }
        };
        (gid != 0).then_some(gid)
    }
}

// ---------------------------------------------------------------------------
// 2. Unicode Cell Width Segmentation (matching `render.rs`)
// ---------------------------------------------------------------------------

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
            | 0x2600..=0x27BF
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
            | 0x1F000..=0x1FAFF
            | 0x20000..=0x3FFFD
    ) {
        return 2;
    }
    1
}

// ---------------------------------------------------------------------------
// 3. Thread-Local Win32 GDI Font Chain & Gray8 Glyph Raster Cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WinFontKey {
    family: String,
    px_height: i32,
    bold: bool,
    italic: bool,
}

#[derive(Clone)]
struct RasterGlyph {
    offset_x: i32,
    offset_y_from_baseline: i32,
    width: usize,
    height: usize,
    alpha: Vec<u8>,
}

struct FaceEntry {
    hfont: HFONT,
    cmap: FontCmap,
}

impl Drop for FaceEntry {
    fn drop(&mut self) {
        if !self.hfont.is_null() {
            // SAFETY: `self.hfont` is uniquely owned by `FaceEntry` and deselected before drop.
            unsafe {
                win_ffi::DeleteObject(self.hfont);
            }
        }
    }
}

struct FontChain {
    faces: Vec<FaceEntry>,
    metrics: FontMetrics,
    glyphs: HashMap<char, Rc<RasterGlyph>>,
}

struct WinGdiFontEngine {
    hdc: HDC,
    chains: HashMap<WinFontKey, FontChain>,
}

impl WinGdiFontEngine {
    fn new() -> Self {
        ensure_bundled_fonts_registered();
        // SAFETY: `CreateCompatibleDC(null)` creates an in-memory GDI device context independent
        // of any physical window, working identically in desktop sessions and headless Wine.
        let hdc = unsafe { win_ffi::CreateCompatibleDC(ptr::null_mut()) };
        Self {
            hdc,
            chains: HashMap::new(),
        }
    }

    fn build_face_names(requested: &str) -> Vec<String> {
        let mut names: Vec<String> = Vec::with_capacity(8);
        let clean = requested.trim();
        let mut push_unique = |name: &str| {
            let t = name.trim();
            if !t.is_empty()
                && !names
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(t))
            {
                names.push(t.to_string());
            }
        };

        if !clean.is_empty() && !clean.eq_ignore_ascii_case("Monospace") {
            for part in clean.split(',') {
                push_unique(part);
            }
        }
        push_unique("FiraCode Nerd Font Mono");
        push_unique("Fira Code");
        push_unique("Symbols Nerd Font Mono");
        push_unique("Symbols Nerd Font");
        push_unique("Consolas");
        push_unique("Microsoft YaHei UI");
        push_unique("Microsoft YaHei");
        push_unique("SimHei");
        push_unique("Segoe UI Symbol");
        names
    }

    fn create_face(
        &self,
        face_name: &str,
        px_height: i32,
        bold: bool,
        italic: bool,
    ) -> Option<FaceEntry> {
        if self.hdc.is_null() {
            return None;
        }
        let wide = to_wide_nul(face_name);
        let weight = if bold { FW_BOLD } else { FW_NORMAL };
        // SAFETY: `CreateFontW` allocates an `HFONT` for the requested negative character height.
        let hfont = unsafe {
            win_ffi::CreateFontW(
                -px_height.max(6),
                0,
                0,
                0,
                weight,
                u32::from(italic),
                0,
                0,
                DEFAULT_CHARSET,
                OUT_TT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                DEFAULT_PITCH,
                wide.as_ptr(),
            )
        };
        if hfont.is_null() {
            return None;
        }
        // SAFETY: Temporarily select `hfont` into `self.hdc` to inspect `TEXTMETRICW` and `cmap`.
        let prev = unsafe { win_ffi::SelectObject(self.hdc, hfont) };
        let mut tm = TEXTMETRICW::default();
        // SAFETY: `tm` is a valid stack `TEXTMETRICW`.
        let ok = unsafe { win_ffi::GetTextMetricsW(self.hdc, &mut tm) } != 0;
        let cmap = if ok {
            FontCmap::from_hdc(self.hdc)
        } else {
            FontCmap::default()
        };
        // SAFETY: Restore previous GDI object in `self.hdc`.
        unsafe {
            win_ffi::SelectObject(self.hdc, prev);
        }
        if !ok {
            // SAFETY: `hfont` was deselected above.
            unsafe {
                win_ffi::DeleteObject(hfont);
            }
            return None;
        }
        Some(FaceEntry { hfont, cmap })
    }

    fn ensure_chain(
        &mut self,
        family: &str,
        size_pt: f64,
        scale: f64,
        bold: bool,
        italic: bool,
    ) -> &mut FontChain {
        let safe_pt = if size_pt.is_finite() && size_pt > 0.0 {
            size_pt
        } else {
            13.0
        };
        let safe_scale = if scale.is_finite() && scale >= 1.0 {
            scale
        } else {
            1.0
        };
        // 96 DPI standard logical pixel conversion: `pt * 96 / 72 == pt * 4 / 3`.
        let px_height = ((safe_pt * (96.0 / 72.0)) * safe_scale)
            .round()
            .clamp(8.0, 256.0) as i32;
        let key = WinFontKey {
            family: family.trim().to_string(),
            px_height,
            bold,
            italic,
        };
        if !self.chains.contains_key(&key) {
            if self.chains.len() >= 64 {
                self.chains.clear();
            }
            let face_names = Self::build_face_names(family);
            let mut faces = Vec::with_capacity(face_names.len());
            for name in &face_names {
                if let Some(entry) = self.create_face(name, px_height, bold, italic) {
                    faces.push(entry);
                }
            }
            let metrics = self.compute_primary_metrics(&faces, px_height, safe_scale);
            self.chains.insert(
                key.clone(),
                FontChain {
                    faces,
                    metrics,
                    glyphs: HashMap::with_capacity(256),
                },
            );
        }
        self.chains.get_mut(&key).expect("font chain inserted")
    }

    fn compute_primary_metrics(
        &self,
        faces: &[FaceEntry],
        px_height: i32,
        scale: f64,
    ) -> FontMetrics {
        let Some(primary) = faces.first() else {
            return FontMetrics::default();
        };
        if self.hdc.is_null() {
            return FontMetrics::default();
        }
        // SAFETY: Select `primary.hfont` temporarily to measure `'M'` advance and vertical metrics.
        unsafe {
            let prev = win_ffi::SelectObject(self.hdc, primary.hfont);
            let mut tm = TEXTMETRICW::default();
            let _ = win_ffi::GetTextMetricsW(self.hdc, &mut tm);
            let sample = [b'M' as u16; 10];
            let mut sz = SIZE::default();
            let has_ext =
                win_ffi::GetTextExtentPoint32W(self.hdc, sample.as_ptr(), 10, &mut sz) != 0;
            win_ffi::SelectObject(self.hdc, prev);

            let raw_cw = if has_ext && sz.cx > 0 {
                (f64::from(sz.cx) / 10.0) / scale
            } else {
                (f64::from(px_height) * 0.6) / scale
            };
            let ascent = (f64::from(tm.tm_ascent.max(1)) / scale).round().max(1.0);
            let descent = (f64::from(tm.tm_descent.max(1)) / scale).round().max(1.0);
            let leading = (f64::from(tm.tm_external_leading.max(0)) / scale).round();
            let cell_height = (ascent + descent + leading).max(ascent + 2.0);
            FontMetrics {
                cell_width: if raw_cw > 1.0 { raw_cw } else { 8.4 },
                cell_height,
                ascent,
                descent,
            }
        }
    }

    fn rasterize_char(hdc: HDC, chain: &mut FontChain, ch: char) -> Rc<RasterGlyph> {
        if let Some(existing) = chain.glyphs.get(&ch) {
            return Rc::clone(existing);
        }
        if chain.glyphs.len() >= 4096 {
            chain.glyphs.clear();
        }

        let empty = Rc::new(RasterGlyph {
            offset_x: 0,
            offset_y_from_baseline: 0,
            width: 0,
            height: 0,
            alpha: Vec::new(),
        });
        if hdc.is_null() || ch == ' ' || ch.is_control() {
            chain.glyphs.insert(ch, Rc::clone(&empty));
            return empty;
        }

        let cp = ch as u32;
        let mat = MAT2::identity();

        for face in &chain.faces {
            // 1. Prefer exact OpenType `cmap` lookup (supports full 32-bit UCS-4 including SMP Nerd Font!).
            let mut glyph_index = face.cmap.lookup(cp);

            // 2. Fallback to `GetGlyphIndicesW` for BMP characters if font had a non-Unicode cmap.
            if glyph_index.is_none() && cp <= 0xFFFF {
                // SAFETY: `hdc` and `face.hfont` are valid on this thread.
                unsafe {
                    let prev = win_ffi::SelectObject(hdc, face.hfont);
                    let wc = cp as u16;
                    let mut gi: u16 = 0xFFFF;
                    let res = win_ffi::GetGlyphIndicesW(
                        hdc,
                        &wc,
                        1,
                        &mut gi,
                        GGI_MARK_NONEXISTING_GLYPHS,
                    );
                    win_ffi::SelectObject(hdc, prev);
                    if res != GDI_ERROR && gi != 0xFFFF && gi != 0 {
                        glyph_index = Some(gi);
                    }
                }
            }

            let Some(gid) = glyph_index else {
                continue;
            };

            // SAFETY: Select `face.hfont` and query `GGO_GRAY8_BITMAP | GGO_GLYPH_INDEX` size & pixels.
            unsafe {
                let prev = win_ffi::SelectObject(hdc, face.hfont);
                let mut gm = GLYPHMETRICS::default();
                let fmt = GGO_GRAY8_BITMAP | GGO_GLYPH_INDEX;
                let buf_size = win_ffi::GetGlyphOutlineW(
                    hdc,
                    u32::from(gid),
                    fmt,
                    &mut gm,
                    0,
                    ptr::null_mut(),
                    &mat,
                );
                if buf_size == GDI_ERROR {
                    win_ffi::SelectObject(hdc, prev);
                    continue;
                }
                let w = gm.gm_black_box_x as usize;
                let h = gm.gm_black_box_y as usize;
                if buf_size == 0 || w == 0 || h == 0 {
                    win_ffi::SelectObject(hdc, prev);
                    chain.glyphs.insert(ch, Rc::clone(&empty));
                    return empty;
                }
                let mut raw = vec![0u8; buf_size as usize];
                let got = win_ffi::GetGlyphOutlineW(
                    hdc,
                    u32::from(gid),
                    fmt,
                    &mut gm,
                    buf_size,
                    raw.as_mut_ptr().cast(),
                    &mat,
                );
                win_ffi::SelectObject(hdc, prev);
                if got == GDI_ERROR {
                    continue;
                }

                // `GGO_GRAY8_BITMAP` rows are DWORD-aligned (4-byte pitch) with coverage `0..=64`.
                let pitch = (w + 3) & !3;
                let mut alpha = vec![0u8; w * h];
                for row in 0..h {
                    let row_base = row * pitch;
                    for col in 0..w {
                        let cov = *raw.get(row_base + col).unwrap_or(&0);
                        alpha[row * w + col] = if cov >= 64 {
                            255
                        } else {
                            ((u32::from(cov) * 255 + 32) / 64) as u8
                        };
                    }
                }

                let glyph = Rc::new(RasterGlyph {
                    offset_x: gm.gm_pt_glyph_origin.x,
                    offset_y_from_baseline: -gm.gm_pt_glyph_origin.y,
                    width: w,
                    height: h,
                    alpha,
                });
                chain.glyphs.insert(ch, Rc::clone(&glyph));
                return glyph;
            }
        }

        chain.glyphs.insert(ch, Rc::clone(&empty));
        empty
    }
}

impl Drop for WinGdiFontEngine {
    fn drop(&mut self) {
        self.chains.clear();
        if !self.hdc.is_null() {
            // SAFETY: `self.hdc` was created by `CreateCompatibleDC` and all `HFONT`s were dropped.
            unsafe {
                win_ffi::DeleteDC(self.hdc);
            }
        }
    }
}

thread_local! {
    static THREAD_WIN_FONTS: RefCell<WinGdiFontEngine> = RefCell::new(WinGdiFontEngine::new());
}

/// Measures monospace cell width, line height, ascent, and descent in logical pixels.
pub fn measure_monospace_font(family: &str, size_pt: f64) -> FontMetrics {
    THREAD_WIN_FONTS.with(|cell| {
        let mut engine = cell.borrow_mut();
        engine
            .ensure_chain(family, size_pt, 1.0, false, false)
            .metrics
    })
}

// ---------------------------------------------------------------------------
// 4. 32-bit BGRA/ARGB Pixel Canvas (`0xAARRGGBB` = Win32 `BI_RGB` Top-Down DIB)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub(crate) struct IntClip {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl IntClip {
    fn intersect(self, other: Self) -> Self {
        Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    #[inline]
    fn is_empty(self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }
}

#[derive(Clone)]
pub(crate) struct PixelCanvas {
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    pub pixels: Vec<u32>,
    clip_stack: Vec<IntClip>,
}

#[inline]
fn unpack_rgba(color: ColorRgba) -> (u8, u8, u8, u8) {
    let p = color.0;
    (
        ((p >> 24) & 0xFF) as u8,
        ((p >> 16) & 0xFF) as u8,
        ((p >> 8) & 0xFF) as u8,
        (p & 0xFF) as u8,
    )
}

#[inline]
fn pack_argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

#[inline]
fn blend_over_argb(dst: u32, r: u8, g: u8, b: u8, a: u8) -> u32 {
    if a == 255 {
        return pack_argb(255, r, g, b);
    }
    if a == 0 {
        return dst;
    }
    let sa = u32::from(a);
    let inv = 255 - sa;
    let da = (dst >> 24) & 0xFF;
    let dr = (dst >> 16) & 0xFF;
    let dg = (dst >> 8) & 0xFF;
    let db = dst & 0xFF;

    let out_r = (u32::from(r) * sa + dr * inv + 127) / 255;
    let out_g = (u32::from(g) * sa + dg * inv + 127) / 255;
    let out_b = (u32::from(b) * sa + db * inv + 127) / 255;
    let out_a = sa + (da * inv + 127) / 255;
    pack_argb(out_a.min(255) as u8, out_r as u8, out_g as u8, out_b as u8)
}

#[inline]
fn sdf_rounded_box(
    px: f64,
    py: f64,
    cx: f64,
    cy: f64,
    half_w: f64,
    half_h: f64,
    radius: f64,
) -> f64 {
    let r = radius.clamp(0.0, half_w.min(half_h));
    let qx = (px - cx).abs() - (half_w - r);
    let qy = (py - cy).abs() - (half_h - r);
    let out_dist = qx.max(0.0).hypot(qy.max(0.0));
    let in_dist = qx.max(qy).min(0.0);
    out_dist + in_dist - r
}

impl PixelCanvas {
    pub(crate) fn new_scaled(logical_w: i32, logical_h: i32, scale: f64) -> Self {
        let safe_scale = if scale.is_finite() && scale >= 1.0 {
            scale
        } else {
            1.0
        };
        let w = ((f64::from(logical_w.max(1)) * safe_scale).ceil() as i32).max(1);
        let h = ((f64::from(logical_h.max(1)) * safe_scale).ceil() as i32).max(1);
        let root_clip = IntClip {
            x0: 0,
            y0: 0,
            x1: w,
            y1: h,
        };
        Self {
            width: w,
            height: h,
            scale: safe_scale,
            pixels: vec![0xFF1E_1E2E; (w as usize) * (h as usize)],
            clip_stack: vec![root_clip],
        }
    }

    pub(crate) fn reset_and_clear(&mut self, bg: ColorRgba) {
        self.clip_stack.truncate(1);
        if self.clip_stack.is_empty() {
            self.clip_stack.push(IntClip {
                x0: 0,
                y0: 0,
                x1: self.width,
                y1: self.height,
            });
        } else {
            self.clip_stack[0] = IntClip {
                x0: 0,
                y0: 0,
                x1: self.width,
                y1: self.height,
            };
        }
        let (r, g, b, a) = unpack_rgba(bg);
        let fill = pack_argb(a, r, g, b);
        self.pixels.fill(fill);
    }

    #[inline]
    fn current_clip(&self) -> IntClip {
        self.clip_stack.last().copied().unwrap_or(IntClip {
            x0: 0,
            y0: 0,
            x1: self.width,
            y1: self.height,
        })
    }

    pub(crate) fn clip_push(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let s = self.scale;
        let next = if w > 0.0 && h > 0.0 && x.is_finite() && y.is_finite() {
            IntClip {
                x0: (x * s).floor() as i32,
                y0: (y * s).floor() as i32,
                x1: ((x + w) * s).ceil() as i32,
                y1: ((y + h) * s).ceil() as i32,
            }
        } else {
            IntClip {
                x0: 0,
                y0: 0,
                x1: 0,
                y1: 0,
            }
        };
        let merged = self.current_clip().intersect(next);
        self.clip_stack.push(merged);
    }

    pub(crate) fn clip_pop(&mut self) {
        if self.clip_stack.len() > 1 {
            self.clip_stack.pop();
        }
    }

    pub(crate) fn fill_rect(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        color: ColorRgba,
        radius: f64,
    ) {
        if w <= 0.0 || h <= 0.0 || !x.is_finite() || !y.is_finite() {
            return;
        }
        let (r, g, b, a) = unpack_rgba(color);
        if a == 0 {
            return;
        }
        let s = self.scale;
        let px0 = x * s;
        let py0 = y * s;
        let pw = w * s;
        let ph = h * s;
        let rad = (radius.max(0.0) * s).min(pw.min(ph) * 0.5);
        let clip = self.current_clip();

        if rad <= 0.35 {
            let ix0 = (px0.round() as i32).max(clip.x0);
            let iy0 = (py0.round() as i32).max(clip.y0);
            let ix1 = ((px0 + pw).round() as i32).min(clip.x1);
            let iy1 = ((py0 + ph).round() as i32).min(clip.y1);
            if ix0 >= ix1 || iy0 >= iy1 {
                return;
            }
            let stride = self.width as usize;
            if a == 255 {
                let packed = pack_argb(255, r, g, b);
                for py in iy0..iy1 {
                    let row_off = (py as usize) * stride;
                    self.pixels[row_off + (ix0 as usize)..row_off + (ix1 as usize)].fill(packed);
                }
            } else {
                for py in iy0..iy1 {
                    let row_off = (py as usize) * stride;
                    for px in ix0..ix1 {
                        let idx = row_off + (px as usize);
                        self.pixels[idx] = blend_over_argb(self.pixels[idx], r, g, b, a);
                    }
                }
            }
            return;
        }

        let ix0 = (px0.floor() as i32 - 1).max(clip.x0);
        let iy0 = (py0.floor() as i32 - 1).max(clip.y0);
        let ix1 = ((px0 + pw).ceil() as i32 + 1).min(clip.x1);
        let iy1 = ((py0 + ph).ceil() as i32 + 1).min(clip.y1);
        if ix0 >= ix1 || iy0 >= iy1 {
            return;
        }
        let cx = px0 + pw * 0.5;
        let cy = py0 + ph * 0.5;
        let hw = pw * 0.5;
        let hh = ph * 0.5;
        let stride = self.width as usize;

        for py in iy0..iy1 {
            let sample_y = f64::from(py) + 0.5;
            let row_off = (py as usize) * stride;
            for px in ix0..ix1 {
                let d = sdf_rounded_box(f64::from(px) + 0.5, sample_y, cx, cy, hw, hh, rad);
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let eff_a = (f64::from(a) * cov).round() as u8;
                    let idx = row_off + (px as usize);
                    self.pixels[idx] = blend_over_argb(self.pixels[idx], r, g, b, eff_a);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn stroke_rect(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        color: ColorRgba,
        line_width: f64,
        radius: f64,
    ) {
        if w <= 0.0 || h <= 0.0 || !x.is_finite() || !y.is_finite() {
            return;
        }
        let (r, g, b, a) = unpack_rgba(color);
        if a == 0 {
            return;
        }
        let s = self.scale;
        let lw = (line_width.max(1.0) * s).max(1.0);
        let half_lw = lw * 0.5;
        let px0 = x * s;
        let py0 = y * s;
        let pw = w * s;
        let ph = h * s;
        let rad = (radius.max(0.0) * s).min(pw.min(ph) * 0.5);
        let clip = self.current_clip();

        let ix0 = ((px0 - half_lw - 1.0).floor() as i32).max(clip.x0);
        let iy0 = ((py0 - half_lw - 1.0).floor() as i32).max(clip.y0);
        let ix1 = ((px0 + pw + half_lw + 1.0).ceil() as i32).min(clip.x1);
        let iy1 = ((py0 + ph + half_lw + 1.0).ceil() as i32).min(clip.y1);
        if ix0 >= ix1 || iy0 >= iy1 {
            return;
        }

        let cx = px0 + pw * 0.5;
        let cy = py0 + ph * 0.5;
        let hw = pw * 0.5;
        let hh = ph * 0.5;
        let stride = self.width as usize;

        for py in iy0..iy1 {
            let sample_y = f64::from(py) + 0.5;
            let row_off = (py as usize) * stride;
            // Fast skip interior rows when far from top/bottom borders
            let dy_center = (sample_y - cy).abs();
            let in_vert_band = dy_center >= (hh - rad - half_lw - 1.5);
            for px in ix0..ix1 {
                let sample_x = f64::from(px) + 0.5;
                if !in_vert_band && (sample_x - cx).abs() < (hw - half_lw - 1.5) {
                    continue;
                }
                let d = sdf_rounded_box(sample_x, sample_y, cx, cy, hw, hh, rad);
                let stroke_d = d.abs() - half_lw;
                let cov = (0.5 - stroke_d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let eff_a = (f64::from(a) * cov).round() as u8;
                    let idx = row_off + (px as usize);
                    self.pixels[idx] = blend_over_argb(self.pixels[idx], r, g, b, eff_a);
                }
            }
        }
    }

    pub(crate) fn draw_line(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        color: ColorRgba,
        line_width: f64,
    ) {
        if !x1.is_finite() || !y1.is_finite() || !x2.is_finite() || !y2.is_finite() {
            return;
        }
        let lw = line_width.max(1.0);
        // Fast path for axis-aligned horizontal/vertical UI dividers and underlines!
        if (y1 - y2).abs() < 0.05 {
            let lx = x1.min(x2);
            let w = (x2 - x1).abs();
            self.fill_rect(lx, y1 - lw * 0.5, w, lw, color, 0.0);
            return;
        }
        if (x1 - x2).abs() < 0.05 {
            let ty = y1.min(y2);
            let h = (y2 - y1).abs();
            self.fill_rect(x1 - lw * 0.5, ty, lw, h, color, 0.0);
            return;
        }

        let (r, g, b, a) = unpack_rgba(color);
        if a == 0 {
            return;
        }
        let s = self.scale;
        let ax = x1 * s;
        let ay = y1 * s;
        let bx = x2 * s;
        let by = y2 * s;
        let half_w = (lw * s) * 0.5;
        let clip = self.current_clip();

        let ix0 = ((ax.min(bx) - half_w - 1.0).floor() as i32).max(clip.x0);
        let iy0 = ((ay.min(by) - half_w - 1.0).floor() as i32).max(clip.y0);
        let ix1 = ((ax.max(bx) + half_w + 1.0).ceil() as i32).min(clip.x1);
        let iy1 = ((ay.max(by) + half_w + 1.0).ceil() as i32).min(clip.y1);
        if ix0 >= ix1 || iy0 >= iy1 {
            return;
        }

        let vx = bx - ax;
        let vy = by - ay;
        let len_sq = (vx * vx + vy * vy).max(1e-6);
        let stride = self.width as usize;

        for py in iy0..iy1 {
            let sy = f64::from(py) + 0.5;
            let row_off = (py as usize) * stride;
            for px in ix0..ix1 {
                let sx = f64::from(px) + 0.5;
                let t = (((sx - ax) * vx + (sy - ay) * vy) / len_sq).clamp(0.0, 1.0);
                let dist = (sx - (ax + t * vx)).hypot(sy - (ay + t * vy)) - half_w;
                let cov = (0.5 - dist).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let eff_a = (f64::from(a) * cov).round() as u8;
                    let idx = row_off + (px as usize);
                    self.pixels[idx] = blend_over_argb(self.pixels[idx], r, g, b, eff_a);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_text_run(
        &mut self,
        x: f64,
        y: f64,
        text: &str,
        font_family: &str,
        font_size_pt: f64,
        color: ColorRgba,
        bold: bool,
        italic: bool,
        underline: bool,
    ) {
        if text.is_empty() || !x.is_finite() || !y.is_finite() {
            return;
        }
        let (r, g, b, a) = unpack_rgba(color);
        if a == 0 {
            return;
        }
        let clip = self.current_clip();
        if clip.is_empty() {
            return;
        }
        let s = self.scale;

        THREAD_WIN_FONTS.with(|cell| {
            let mut engine = cell.borrow_mut();
            let hdc = engine.hdc;
            let chain = engine.ensure_chain(font_family, font_size_pt, s, bold, italic);
            let cell_w_px = chain.metrics.cell_width * s;
            let baseline_y_px = (y * s + chain.metrics.ascent * s).round() as i32;

            // Quick vertical reject if entire line is outside clip rect
            let max_glyph_h = (chain.metrics.cell_height * s).ceil() as i32 + 6;
            if baseline_y_px + max_glyph_h < clip.y0 || baseline_y_px - max_glyph_h > clip.y1 {
                return;
            }

            let stride = self.width as usize;
            let mut col_cells = 0usize;

            for ch in text.chars() {
                let cw = codepoint_cell_width(ch);
                let pen_x = (x * s + (col_cells as f64) * cell_w_px).round() as i32;
                col_cells += cw;

                if ch == ' ' || cw == 0 {
                    continue;
                }
                if pen_x > clip.x1 + 32
                    || pen_x + (cw as i32 + 2) * (cell_w_px.ceil() as i32) < clip.x0
                {
                    continue;
                }

                let glyph = WinGdiFontEngine::rasterize_char(hdc, chain, ch);
                if glyph.width == 0 || glyph.height == 0 {
                    continue;
                }

                let gx0 = pen_x + glyph.offset_x;
                let gy0 = baseline_y_px + glyph.offset_y_from_baseline;
                let gx1 = gx0 + glyph.width as i32;
                let gy1 = gy0 + glyph.height as i32;

                let draw_x0 = gx0.max(clip.x0);
                let draw_y0 = gy0.max(clip.y0);
                let draw_x1 = gx1.min(clip.x1);
                let draw_y1 = gy1.min(clip.y1);
                if draw_x0 >= draw_x1 || draw_y0 >= draw_y1 {
                    continue;
                }

                for py in draw_y0..draw_y1 {
                    let src_r = (py - gy0) as usize;
                    let dst_row_off = (py as usize) * stride;
                    let src_row_off = src_r * glyph.width;
                    for px in draw_x0..draw_x1 {
                        let src_c = (px - gx0) as usize;
                        let cov = glyph.alpha[src_row_off + src_c];
                        if cov > 0 {
                            let eff_a = if a == 255 {
                                cov
                            } else {
                                ((u32::from(cov) * u32::from(a) + 127) / 255) as u8
                            };
                            let idx = dst_row_off + (px as usize);
                            self.pixels[idx] = blend_over_argb(self.pixels[idx], r, g, b, eff_a);
                        }
                    }
                }
            }

            if underline && col_cells > 0 {
                let total_w_logical = (col_cells as f64) * chain.metrics.cell_width;
                let ul_y = y + chain.metrics.ascent + 1.5;
                self.fill_rect(x, ul_y, total_w_logical, 1.0, color, 0.0);
            }
        });
    }

    pub(crate) fn draw_caret(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        shape: CaretShape,
        color: ColorRgba,
    ) {
        let safe_w = w.max(1.0);
        let safe_h = h.max(1.0);
        match shape {
            CaretShape::Block => self.fill_rect(x, y, safe_w, safe_h, color, 0.0),
            CaretShape::Beam => {
                let beam_w = safe_w.clamp(2.0, 2.5);
                self.fill_rect(x, y, beam_w, safe_h, color, 0.0);
            }
            CaretShape::Underline => {
                let bar_h = (safe_h * 0.12).clamp(2.0, 3.0);
                self.fill_rect(x, y + safe_h - bar_h, safe_w, bar_h, color, 0.0);
            }
            CaretShape::HollowBlock => {
                self.stroke_rect(
                    x + 0.5,
                    y + 0.5,
                    (safe_w - 1.0).max(1.0),
                    (safe_h - 1.0).max(1.0),
                    color,
                    1.0,
                    0.0,
                );
            }
        }
    }

    /// Fills a convex 4-corner polygon (`corners` in logical coordinates) merged with the leading
    /// cursor head rectangle in a single pass so translucent cursor smear never double-blends!
    pub(crate) fn fill_smear_quad_and_head(
        &mut self,
        corners: [(f64, f64); 4],
        head_rect: (f64, f64, f64, f64),
        color: ColorRgba,
    ) {
        let (r, g, b, a) = unpack_rgba(color);
        if a == 0 {
            return;
        }
        let s = self.scale;
        let pts: [(f64, f64); 4] = corners.map(|(cx, cy)| (cx * s, cy * s));
        let hx0 = head_rect.0 * s;
        let hy0 = head_rect.1 * s;
        let hx1 = (head_rect.0 + head_rect.2) * s;
        let hy1 = (head_rect.1 + head_rect.3) * s;

        let mut min_x = hx0;
        let mut min_y = hy0;
        let mut max_x = hx1;
        let mut max_y = hy1;
        for &(px, py) in &pts {
            min_x = min_x.min(px);
            min_y = min_y.min(py);
            max_x = max_x.max(px);
            max_y = max_y.max(py);
        }

        let clip = self.current_clip();
        let ix0 = (min_x.floor() as i32).max(clip.x0);
        let iy0 = (min_y.floor() as i32).max(clip.y0);
        let ix1 = (max_x.ceil() as i32).min(clip.x1);
        let iy1 = (max_y.ceil() as i32).min(clip.y1);
        if ix0 >= ix1 || iy0 >= iy1 {
            return;
        }

        let stride = self.width as usize;
        for py in iy0..iy1 {
            let sy = f64::from(py) + 0.5;
            // Compute scanline intersection `[qx0, qx1]` with the 4-edge polygon
            let mut xs = [0.0_f64; 4];
            let mut n_xs = 0usize;
            for i in 0..4 {
                let (x0, y0) = pts[i];
                let (x1, y1) = pts[(i + 1) & 3];
                if (y0 <= sy && y1 > sy) || (y1 <= sy && y0 > sy) {
                    let t = (sy - y0) / (y1 - y0);
                    xs[n_xs] = x0 + t * (x1 - x0);
                    n_xs += 1;
                }
            }
            let (quad_lo, quad_hi) = if n_xs >= 2 {
                let mut lo = xs[0];
                let mut hi = xs[0];
                for &v in &xs[1..n_xs] {
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
                (lo, hi)
            } else {
                (f64::INFINITY, f64::NEG_INFINITY)
            };
            let in_head_y = sy >= hy0 && sy <= hy1;
            let row_off = (py as usize) * stride;

            for px in ix0..ix1 {
                let sx = f64::from(px) + 0.5;
                let inside =
                    (in_head_y && sx >= hx0 && sx <= hx1) || (sx >= quad_lo && sx <= quad_hi);
                if inside {
                    let idx = row_off + (px as usize);
                    self.pixels[idx] = blend_over_argb(self.pixels[idx], r, g, b, a);
                }
            }
        }
    }

    pub(crate) fn blit_shifted_region(
        &mut self,
        src: &PixelCanvas,
        clip_logical: (f64, f64, f64, f64),
        shift_y_logical: f64,
    ) {
        if src.width != self.width || src.height != self.height {
            return;
        }
        let s = self.scale;
        let cx0 = ((clip_logical.0 * s).round() as i32).clamp(0, self.width);
        let cy0 = ((clip_logical.1 * s).round() as i32).clamp(0, self.height);
        let cx1 = (((clip_logical.0 + clip_logical.2) * s).round() as i32).clamp(0, self.width);
        let cy1 = (((clip_logical.1 + clip_logical.3) * s).round() as i32).clamp(0, self.height);
        if cx0 >= cx1 || cy0 >= cy1 {
            return;
        }
        let dy_px = (shift_y_logical * s).round() as i32;
        let stride = self.width as usize;

        for dst_y in cy0..cy1 {
            let src_y = (dst_y - dy_px).clamp(0, self.height - 1);
            let dst_off = (dst_y as usize) * stride;
            let src_off = (src_y as usize) * stride;
            self.pixels[dst_off + (cx0 as usize)..dst_off + (cx1 as usize)]
                .copy_from_slice(&src.pixels[src_off + (cx0 as usize)..src_off + (cx1 as usize)]);
        }
    }
}

pub(crate) fn render_frame_to_canvas(
    canvas: &mut PixelCanvas,
    frame: &RenderFrame,
    skip_caret_idx: Option<usize>,
) {
    canvas.reset_and_clear(frame.window.background);
    for (idx, cmd) in frame.commands.iter().enumerate() {
        if skip_caret_idx == Some(idx) {
            continue;
        }
        match cmd {
            DrawCmd::ClipPush { x, y, w, h } => canvas.clip_push(*x, *y, *w, *h),
            DrawCmd::ClipPop => canvas.clip_pop(),
            DrawCmd::FillRect {
                x,
                y,
                w,
                h,
                color,
                radius,
            } => canvas.fill_rect(*x, *y, *w, *h, *color, *radius),
            DrawCmd::StrokeRect {
                x,
                y,
                w,
                h,
                color,
                line_width,
                radius,
            } => canvas.stroke_rect(*x, *y, *w, *h, *color, *line_width, *radius),
            DrawCmd::Line {
                x1,
                y1,
                x2,
                y2,
                color,
                line_width,
            } => canvas.draw_line(*x1, *y1, *x2, *y2, *color, *line_width),
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
            } => canvas.draw_text_run(
                *x,
                *y,
                text,
                font_family,
                *font_size_pt,
                *color,
                *bold,
                *italic,
                *underline,
            ),
            DrawCmd::CursorCaret {
                x,
                y,
                w,
                h,
                shape,
                color,
            } => canvas.draw_caret(*x, *y, *w, *h, *shape, *color),
        }
    }
}

fn format_bytes_human(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.2} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

pub(crate) fn draw_transfer_progress_overlay(
    canvas: &mut PixelCanvas,
    logical_w: f64,
    logical_h: f64,
    ov: &TransferOverlay,
) {
    if !ov.active || logical_w < 120.0 || logical_h < 60.0 {
        return;
    }
    let card_w = (logical_w * 0.44).clamp(320.0, 520.0);
    let card_h = 68.0;
    let card_x = (logical_w - card_w - 20.0).max(8.0);
    let card_y = (logical_h - card_h - 38.0).max(8.0);

    canvas.fill_rect(
        card_x,
        card_y,
        card_w,
        card_h,
        ColorRgba::from_rgba(24, 24, 37, 242),
        8.0,
    );
    canvas.stroke_rect(
        card_x,
        card_y,
        card_w,
        card_h,
        ColorRgba::from_rgba(137, 180, 250, 215),
        1.2,
        8.0,
    );

    let frac = if ov.bytes_total > 0 {
        (ov.bytes_done as f64 / ov.bytes_total as f64).clamp(0.0, 1.0)
    } else {
        0.35
    };
    let pct = (frac * 100.0).round() as u64;
    let badge = if ov.transport.is_empty() {
        "SSH"
    } else {
        ov.transport.as_str()
    };
    let header = format!("[{badge}] {}", ov.title);
    let stats = if ov.bytes_total > 0 {
        format!(
            "{} / {} ({pct}%)  {} lines",
            format_bytes_human(ov.bytes_done),
            format_bytes_human(ov.bytes_total),
            ov.lines_loaded
        )
    } else {
        format!(
            "{}  {} lines",
            format_bytes_human(ov.bytes_done),
            ov.lines_loaded
        )
    };

    canvas.draw_text_run(
        card_x + 12.0,
        card_y + 8.0,
        &header,
        "Fira Code",
        10.5,
        ColorRgba::from_rgba(205, 214, 244, 255),
        true,
        false,
        false,
    );
    canvas.draw_text_run(
        card_x + 12.0,
        card_y + 26.0,
        &stats,
        "Fira Code",
        9.5,
        ColorRgba::from_rgba(166, 173, 200, 255),
        false,
        false,
        false,
    );

    let bar_x = card_x + 12.0;
    let bar_y = card_y + 48.0;
    let bar_w = (card_w - 24.0).max(40.0);
    let bar_h = 8.0;
    canvas.fill_rect(
        bar_x,
        bar_y,
        bar_w,
        bar_h,
        ColorRgba::from_rgba(49, 50, 68, 255),
        4.0,
    );
    let fill_w = (bar_w * frac).max(if ov.bytes_done > 0 { 6.0 } else { 0.0 });
    if fill_w > 0.0 {
        canvas.fill_rect(
            bar_x,
            bar_y,
            fill_w,
            bar_h,
            ColorRgba::from_rgba(137, 180, 250, 255),
            4.0,
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Smooth-Scroll & 4-Corner Cursor Smear Compositor (`WinAnimController`)
// ---------------------------------------------------------------------------

const TAU_CURSOR_LEAD: f64 = 0.014;
const TAU_CURSOR_TRAIL: f64 = 0.044;
const TAU_CURSOR_SHORT: f64 = 0.012;
const TAU_SCROLL_DECAY: f64 = 0.042;
const SNAP_EPSILON_PX: f64 = 0.55;
const CORNER_SIGNS: [(f64, f64); 4] = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];

#[derive(Clone, Copy, PartialEq)]
struct CaretTarget {
    cmd_idx: usize,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    shape: CaretShape,
    color: ColorRgba,
}

impl CaretTarget {
    fn visual_rect(&self) -> (f64, f64, f64, f64) {
        let sw = self.w.max(1.0);
        let sh = self.h.max(1.0);
        match self.shape {
            CaretShape::Block | CaretShape::HollowBlock => (self.x, self.y, sw, sh),
            CaretShape::Beam => (self.x, self.y, sw.clamp(2.0, 2.5), sh),
            CaretShape::Underline => {
                let bh = (sh * 0.12).clamp(2.0, 3.0);
                (self.x, self.y + sh - bh, sw, bh)
            }
        }
    }

    fn target_corners(&self) -> [(f64, f64); 4] {
        let (rx, ry, rw, rh) = self.visual_rect();
        [(rx, ry), (rx + rw, ry), (rx + rw, ry + rh), (rx, ry + rh)]
    }
}

fn find_primary_caret(frame: &RenderFrame) -> Option<CaretTarget> {
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

struct WinPaneScroll {
    vp: ScrollViewport,
    offset_y_px: f64,
    jump_px: f64,
    outgoing: Option<Rc<PixelCanvas>>,
}

impl WinPaneScroll {
    fn matches_geometry(&self, next: &ScrollViewport) -> bool {
        self.vp.pane_id == next.pane_id
            && self.vp.doc_id == next.doc_id
            && self.vp.left_col == next.left_col
            && (self.vp.x - next.x).abs() < 0.5
            && (self.vp.y - next.y).abs() < 0.5
            && (self.vp.w - next.w).abs() < 0.5
            && (self.vp.h - next.h).abs() < 0.5
    }

    fn is_active(&self) -> bool {
        self.offset_y_px.abs() >= SNAP_EPSILON_PX
    }
}

pub(crate) struct WinAnimController {
    base_canvas: Option<PixelCanvas>,
    cached_gen: u64,
    cached_skip_caret: Option<usize>,
    panes: HashMap<i64, WinPaneScroll>,
    corners: [(f64, f64); 4],
    head_offset: (f64, f64),
    caret_target: Option<CaretTarget>,
    move_dir: (f64, f64),
    short_hop: bool,
    cursor_settled: bool,
}

impl WinAnimController {
    pub(crate) fn new() -> Self {
        Self {
            base_canvas: None,
            cached_gen: 0,
            cached_skip_caret: None,
            panes: HashMap::new(),
            corners: [(0.0, 0.0); 4],
            head_offset: (0.0, 0.0),
            caret_target: None,
            move_dir: (1.0, 0.0),
            short_hop: true,
            cursor_settled: true,
        }
    }

    pub(crate) fn is_animating(&self) -> bool {
        (!self.cursor_settled && self.caret_target.is_some())
            || self.panes.values().any(WinPaneScroll::is_active)
    }

    pub(crate) fn step(&mut self, dt_sec: f64) -> bool {
        let dt = dt_sec.clamp(0.001, 0.1);
        let mut active = false;

        if !self.cursor_settled {
            if let Some(target) = self.caret_target {
                let tc = target.target_corners();
                let lead_decay = if self.short_hop {
                    (-dt / TAU_CURSOR_SHORT).exp()
                } else {
                    (-dt / TAU_CURSOR_LEAD).exp()
                };
                let trail_decay = if self.short_hop {
                    lead_decay
                } else {
                    (-dt / TAU_CURSOR_TRAIL).exp()
                };
                let mut max_err = 0.0_f64;
                for i in 0..4 {
                    let align =
                        CORNER_SIGNS[i].0 * self.move_dir.0 + CORNER_SIGNS[i].1 * self.move_dir.1;
                    let dec = if align > 0.0 { lead_decay } else { trail_decay };
                    self.corners[i].0 = tc[i].0 + (self.corners[i].0 - tc[i].0) * dec;
                    self.corners[i].1 = tc[i].1 + (self.corners[i].1 - tc[i].1) * dec;
                    max_err = max_err
                        .max((self.corners[i].0 - tc[i].0).hypot(self.corners[i].1 - tc[i].1));
                }
                self.head_offset.0 *= lead_decay;
                self.head_offset.1 *= lead_decay;
                max_err = max_err.max(self.head_offset.0.hypot(self.head_offset.1));
                if max_err < SNAP_EPSILON_PX {
                    self.corners = tc;
                    self.head_offset = (0.0, 0.0);
                    self.cursor_settled = true;
                } else {
                    active = true;
                }
            } else {
                self.cursor_settled = true;
            }
        }

        let scroll_decay = (-dt / TAU_SCROLL_DECAY).exp();
        for pane in self.panes.values_mut() {
            if pane.is_active() {
                pane.offset_y_px *= scroll_decay;
                if pane.offset_y_px.abs() < SNAP_EPSILON_PX {
                    pane.offset_y_px = 0.0;
                    pane.jump_px = 0.0;
                    pane.outgoing = None;
                } else {
                    active = true;
                }
            }
        }
        active
    }

    fn update_cursor_target(&mut self, next: Option<CaretTarget>, enabled: bool) {
        let Some(caret) = next else {
            self.caret_target = None;
            self.head_offset = (0.0, 0.0);
            self.cursor_settled = true;
            return;
        };
        let next_corners = caret.target_corners();
        let Some(prev) = self.caret_target else {
            self.corners = next_corners;
            self.head_offset = (0.0, 0.0);
            self.caret_target = Some(caret);
            self.cursor_settled = true;
            return;
        };
        if !enabled {
            self.corners = next_corners;
            self.head_offset = (0.0, 0.0);
            self.caret_target = Some(caret);
            self.cursor_settled = true;
            return;
        }
        let (nrx, nry, nrw, nrh) = caret.visual_rect();
        let (prx, pry, prw, prh) = prev.visual_rect();
        let nc = (nrx + nrw * 0.5, nry + nrh * 0.5);
        let pc = (prx + prw * 0.5, pry + prh * 0.5);
        if (nc.0 - pc.0).hypot(nc.1 - pc.1) <= SNAP_EPSILON_PX
            && (nrw - prw).abs() <= SNAP_EPSILON_PX
            && (nrh - prh).abs() <= SNAP_EPSILON_PX
        {
            self.caret_target = Some(caret);
            return;
        }
        let cur_cx =
            (self.corners[0].0 + self.corners[1].0 + self.corners[2].0 + self.corners[3].0) * 0.25;
        let cur_cy =
            (self.corners[0].1 + self.corners[1].1 + self.corners[2].1 + self.corners[3].1) * 0.25;
        let dx = nc.0 - cur_cx;
        let dy = nc.1 - cur_cy;
        let dist = dx.hypot(dy);
        if dist < SNAP_EPSILON_PX {
            self.corners = next_corners;
            self.head_offset = (0.0, 0.0);
            self.caret_target = Some(caret);
            self.cursor_settled = true;
            return;
        }
        self.move_dir = (dx / dist, dy / dist);
        let cell_ref_w = caret.w.max(prev.w).max(6.0);
        self.short_hop = (nc.1 - pc.1).abs() < 1.0 && (nc.0 - pc.0).abs() <= 2.25 * cell_ref_w;
        self.head_offset = (
            prev.x + self.head_offset.0 - caret.x,
            prev.y + self.head_offset.1 - caret.y,
        );
        self.caret_target = Some(caret);
        self.cursor_settled = false;
    }

    pub(crate) fn render_composited(
        &mut self,
        out: &mut PixelCanvas,
        logical_w: i32,
        logical_h: i32,
        frame: &RenderFrame,
        hints: &AnimHints,
        transfer_overlay: Option<&TransferOverlay>,
    ) {
        let safe_w = logical_w.max(1);
        let safe_h = logical_h.max(1);
        let scale = out.scale;

        let primary_caret = find_primary_caret(frame);
        let skip_caret_idx = if hints.cursor_anim {
            primary_caret.map(|c| c.cmd_idx)
        } else {
            None
        };

        let resized = self.base_canvas.as_ref().is_some_and(|b| {
            b.width != out.width || b.height != out.height || (b.scale - scale).abs() > 1e-4
        });
        let changed = resized
            || self.base_canvas.is_none()
            || frame.generation != self.cached_gen
            || skip_caret_idx != self.cached_skip_caret;

        if changed {
            self.update_scroll_viewports(hints, resized);
            let base = self
                .base_canvas
                .get_or_insert_with(|| PixelCanvas::new_scaled(safe_w, safe_h, scale));
            if base.width != out.width || base.height != out.height {
                *base = PixelCanvas::new_scaled(safe_w, safe_h, scale);
            }
            render_frame_to_canvas(base, frame, skip_caret_idx);
            self.cached_gen = frame.generation;
            self.cached_skip_caret = skip_caret_idx;
            self.update_cursor_target(primary_caret, hints.cursor_anim);
        } else if !hints.cursor_anim && !self.cursor_settled {
            self.update_cursor_target(primary_caret, false);
        }

        if let Some(base) = &self.base_canvas
            && base.pixels.len() == out.pixels.len()
        {
            out.pixels.copy_from_slice(&base.pixels);
            for pane in self.panes.values() {
                if !pane.is_active() {
                    continue;
                }
                let vp = pane.vp;
                let off = pane.offset_y_px;
                if off > 0.0 {
                    let old_h = if pane.outgoing.is_some() && pane.jump_px > 0.5 {
                        off.min(pane.jump_px).min(vp.h)
                    } else {
                        0.0
                    };
                    if let Some(outgoing) = &pane.outgoing
                        && old_h > 0.5
                    {
                        out.blit_shifted_region(
                            outgoing,
                            (vp.x, vp.y, vp.w, old_h),
                            off - pane.jump_px,
                        );
                    }
                    let new_h = (vp.h - old_h).max(0.0);
                    if new_h > 0.5 {
                        out.blit_shifted_region(base, (vp.x, vp.y + old_h, vp.w, new_h), off);
                    }
                } else {
                    let reveal_h = (-off).min(vp.h);
                    let old_h = if pane.outgoing.is_some() && pane.jump_px < -0.5 {
                        reveal_h.min(-pane.jump_px)
                    } else {
                        0.0
                    };
                    let top_h = (vp.h - old_h).max(0.0);
                    if top_h > 0.5 {
                        out.blit_shifted_region(base, (vp.x, vp.y, vp.w, top_h), off);
                    }
                    if let Some(outgoing) = &pane.outgoing
                        && old_h > 0.5
                    {
                        out.blit_shifted_region(
                            outgoing,
                            (vp.x, vp.y + top_h, vp.w, old_h),
                            off - pane.jump_px,
                        );
                    }
                }
            }
        } else {
            render_frame_to_canvas(out, frame, skip_caret_idx);
        }

        if hints.cursor_anim
            && let Some(caret) = self.caret_target
        {
            let mut scroll_dy = 0.0;
            let (rx, ry, rw, rh) = caret.visual_rect();
            let cx = rx + rw * 0.5;
            let cy = ry + rh * 0.5;
            for p in self.panes.values() {
                if p.is_active()
                    && cx >= p.vp.x
                    && cx <= p.vp.x + p.vp.w
                    && cy >= p.vp.y
                    && cy <= p.vp.y + p.vp.h
                {
                    scroll_dy = p.offset_y_px;
                    break;
                }
            }
            if self.cursor_settled {
                out.draw_caret(
                    caret.x,
                    caret.y + scroll_dy,
                    caret.w,
                    caret.h,
                    caret.shape,
                    caret.color,
                );
            } else if self.short_hop {
                out.draw_caret(
                    caret.x + self.head_offset.0,
                    caret.y + self.head_offset.1 + scroll_dy,
                    caret.w,
                    caret.h,
                    caret.shape,
                    caret.color,
                );
            } else {
                let shifted_corners = self.corners.map(|(px, py)| (px, py + scroll_dy));
                out.fill_smear_quad_and_head(
                    shifted_corners,
                    (
                        rx + self.head_offset.0,
                        ry + self.head_offset.1 + scroll_dy,
                        rw,
                        rh,
                    ),
                    caret.color,
                );
            }
        }

        if let Some(ov) = transfer_overlay {
            draw_transfer_progress_overlay(out, f64::from(safe_w), f64::from(safe_h), ov);
        }
    }

    fn update_scroll_viewports(&mut self, hints: &AnimHints, resized: bool) {
        if !hints.smooth_scroll || resized {
            self.panes.clear();
            if hints.smooth_scroll {
                for vp in &hints.viewports {
                    self.panes.insert(
                        vp.pane_id,
                        WinPaneScroll {
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

        let any_scrolled = self.base_canvas.is_some()
            && hints.viewports.iter().any(|vp| {
                vp.w > 1.0
                    && vp.h > 1.0
                    && vp.cell_h > 1.0
                    && self
                        .panes
                        .get(&vp.pane_id)
                        .is_some_and(|p| p.matches_geometry(vp) && p.vp.top_row != vp.top_row)
            });

        let outgoing = if any_scrolled {
            self.base_canvas.as_ref().map(|b| Rc::new(b.clone()))
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
                if prev.vp.top_row != vp.top_row && outgoing.is_some() {
                    let delta_rows = (vp.top_row - prev.vp.top_row) as f64;
                    let max_off = (vp.h * 0.95).max(1.0);
                    let next_off =
                        (prev.offset_y_px + delta_rows * vp.cell_h).clamp(-max_off, max_off);
                    prev.vp = *vp;
                    if next_off.abs() >= SNAP_EPSILON_PX {
                        prev.offset_y_px = next_off;
                        prev.jump_px = next_off;
                        prev.outgoing = outgoing.as_ref().map(Rc::clone);
                    } else {
                        prev.offset_y_px = 0.0;
                        prev.jump_px = 0.0;
                        prev.outgoing = None;
                    }
                } else {
                    prev.vp = *vp;
                }
                next_panes.insert(vp.pane_id, prev);
                continue;
            }
            next_panes.insert(
                vp.pane_id,
                WinPaneScroll {
                    vp: *vp,
                    offset_y_px: 0.0,
                    jump_px: 0.0,
                    outgoing: None,
                },
            );
        }
        self.panes = next_panes;
    }
}

// ---------------------------------------------------------------------------
// 6. Zero-Dependency PNG Exporter (`render_frame_to_png`)
// ---------------------------------------------------------------------------

fn crc32_ieee(chunks: &[&[u8]]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for slice in chunks {
        for &b in *slice {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg() & 0xEDB8_8320;
                crc = (crc >> 1) ^ mask;
            }
        }
    }
    !crc
}

fn write_png_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(data);
    let crc = crc32_ieee(&[chunk_type, data]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn encode_argb_canvas_to_png(canvas: &PixelCanvas) -> Vec<u8> {
    let w = canvas.width.max(1) as usize;
    let h = canvas.height.max(1) as usize;
    // Each scanline: 1 filter byte (`0` = None) + `w * 4` RGBA bytes
    let row_bytes = 1 + w * 4;
    let raw_len = h * row_bytes;
    let mut raw_scanlines = Vec::with_capacity(raw_len);
    for y in 0..h {
        raw_scanlines.push(0u8);
        let row_off = y * w;
        for x in 0..w {
            let px = canvas.pixels[row_off + x];
            let a = ((px >> 24) & 0xFF) as u8;
            let r = ((px >> 16) & 0xFF) as u8;
            let g = ((px >> 8) & 0xFF) as u8;
            let b = (px & 0xFF) as u8;
            raw_scanlines.extend_from_slice(&[r, g, b, a]);
        }
    }

    // Wrap scanlines in standard RFC 1950 zlib stream with stored RFC 1951 blocks + Adler-32.
    let mut zlib = Vec::with_capacity(raw_len + (raw_len / 65535 + 2) * 5 + 6);
    zlib.extend_from_slice(&[0x78, 0x01]); // CMF=DEFLATE 32K, FLG=no-dict check bits

    let mut adler_a: u32 = 1;
    let mut adler_b: u32 = 0;
    for &byte in &raw_scanlines {
        adler_a = (adler_a + u32::from(byte)) % 65_521;
        adler_b = (adler_b + adler_a) % 65_521;
    }

    let mut pos = 0usize;
    while pos < raw_scanlines.len() {
        let take = (raw_scanlines.len() - pos).min(65_535);
        let is_final = pos + take >= raw_scanlines.len();
        zlib.push(u8::from(is_final));
        let len_u16 = take as u16;
        let nlen_u16 = !len_u16;
        zlib.extend_from_slice(&len_u16.to_le_bytes());
        zlib.extend_from_slice(&nlen_u16.to_le_bytes());
        zlib.extend_from_slice(&raw_scanlines[pos..pos + take]);
        pos += take;
    }
    let adler = (adler_b << 16) | adler_a;
    zlib.extend_from_slice(&adler.to_be_bytes());

    let mut png = Vec::with_capacity(zlib.len() + 64);
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");

    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&(w as u32).to_be_bytes());
    ihdr[4..8].copy_from_slice(&(h as u32).to_be_bytes());
    ihdr[8] = 8; // 8 bits per channel
    ihdr[9] = 6; // RGBA truecolor with alpha
    write_png_chunk(&mut png, b"IHDR", &ihdr);
    write_png_chunk(&mut png, b"IDAT", &zlib);
    write_png_chunk(&mut png, b"IEND", &[]);
    png
}

/// Renders an immutable [`RenderFrame`] into a PNG file at `output_path` using Win32 GDI +
/// bundled Fira Code / Nerd Font glyph rasterization.
pub fn render_frame_to_png(frame: &RenderFrame, output_path: &Path) -> Result<(), String> {
    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create PNG parent directory: {err}"))?;
    }
    let w = frame.window.width.max(1);
    let h = frame.window.height.max(1);
    let mut canvas = PixelCanvas::new_scaled(w, h, 1.0);
    render_frame_to_canvas(&mut canvas, frame, None);
    let png_bytes = encode_argb_canvas_to_png(&canvas);
    fs::write(output_path, png_bytes)
        .map_err(|err| format!("failed to write PNG `{}`: {err}", output_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lava_proto::WindowConfig;

    #[test]
    fn win32_font_metrics_and_offscreen_png_render_all_draw_cmds() {
        let metrics = measure_monospace_font("Fira Code", 13.0);
        assert!(metrics.cell_width > 2.0);
        assert!(metrics.cell_height >= metrics.ascent);

        let frame = RenderFrame {
            generation: 1,
            window: WindowConfig {
                title: "Lava Win10 1709".to_string(),
                width: 480,
                height: 240,
                background: ColorRgba::from_rgba(30, 30, 46, 255),
                ..WindowConfig::default()
            },
            commands: vec![
                DrawCmd::ClipPush {
                    x: 8.0,
                    y: 8.0,
                    w: 464.0,
                    h: 224.0,
                },
                DrawCmd::FillRect {
                    x: 12.0,
                    y: 12.0,
                    w: 440.0,
                    h: 44.0,
                    color: ColorRgba::from_rgba(49, 50, 68, 255),
                    radius: 6.0,
                },
                DrawCmd::StrokeRect {
                    x: 12.0,
                    y: 12.0,
                    w: 440.0,
                    h: 44.0,
                    color: ColorRgba::from_rgba(137, 180, 250, 255),
                    line_width: 1.5,
                    radius: 6.0,
                },
                DrawCmd::TextRun {
                    x: 20.0,
                    y: 22.0,
                    text: " Lava Windows 10 1709 你好 fn main()".to_string(),
                    font_family: "Fira Code".to_string(),
                    font_size_pt: 13.0,
                    color: ColorRgba::from_rgba(205, 214, 244, 255),
                    bold: true,
                    italic: false,
                    underline: true,
                },
                DrawCmd::CursorCaret {
                    x: 20.0,
                    y: 80.0,
                    w: 8.4,
                    h: 19.0,
                    shape: CaretShape::Block,
                    color: ColorRgba::from_rgba(245, 224, 220, 255),
                },
                DrawCmd::ClipPop,
            ],
        };

        let out_png = env::temp_dir().join(format!("lava_win1709_test_{}.png", std::process::id()));
        render_frame_to_png(&frame, &out_png).expect("render_frame_to_png must succeed");
        let bytes = fs::read(&out_png).expect("read written PNG");
        let _ = fs::remove_file(&out_png);
        assert_eq!(&bytes[0..8], b"\x89PNG\r\n\x1a\n");
        assert!(bytes.len() > 1024);
    }

    #[test]
    fn bundled_fira_code_and_nerd_font_rasterize_bmp_and_smp_icons() {
        let count = ensure_bundled_fonts_registered();
        assert!(
            count >= 4,
            "expected bundled Fira Code + Nerd Font TTFs to load via AddFontResourceExW, got {count}"
        );

        THREAD_WIN_FONTS.with(|cell| {
            let mut engine = cell.borrow_mut();
            let hdc = engine.hdc;
            let chain = engine.ensure_chain("Fira Code", 14.0, 1.0, false, false);

            // ASCII 'M' from Fira Code
            let g_ascii = WinGdiFontEngine::rasterize_char(hdc, chain, 'M');
            assert!(g_ascii.width > 0 && g_ascii.height > 0);
            assert!(g_ascii.alpha.iter().any(|&a| a > 0));

            // BMP Nerd Font folder icon `\u{f07c}` (``)
            let g_folder = WinGdiFontEngine::rasterize_char(hdc, chain, '\u{f07c}');
            assert!(
                g_folder.width > 0 && g_folder.alpha.iter().any(|&a| a > 0),
                "expected BMP Nerd Font folder icon to rasterize via fallback chain"
            );

            // SMP Nerd Font v3 Material Design icon `\u{f0219}` (folder-outline in PUA-A)
            let g_mdi = WinGdiFontEngine::rasterize_char(hdc, chain, '\u{f0219}');
            assert!(
                g_mdi.width > 0 && g_mdi.alpha.iter().any(|&a| a > 0),
                "expected 32-bit SMP Nerd Font v3 icon (U+F0219) to rasterize via cmap Format 12"
            );
        });
    }
}
