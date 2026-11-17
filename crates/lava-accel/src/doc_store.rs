//! Native Copy-on-Write chunked rope document store (`NativeDocStore`) and
//! single-pass O(N) IDE buffer accelerators operating directly on `doc_id`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::{DiffHunk, TodoMatch, diff_line_hunks, git_head_file_lines};

pub const CHUNK_TARGET: usize = 256;
pub const CHUNK_MAX: usize = 512;
pub const CHUNK_MIN: usize = 128;

/// Safe byte cap below Nessa VM's 512 KiB (`524,272`) single-object limit.
pub const MAX_NESSA_STR_BYTES: usize = 490_000;

/// Safe element cap below Nessa VM's `65,535` single-list limit.
pub const MAX_NESSA_LIST_ITEMS: usize = 60_000;

/// Truncate an owned `String` cleanly on a UTF-8 scalar boundary at most `max_bytes`.
#[inline]
pub fn truncate_utf8_safe(mut s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s
}

/// Borrowed `&str` counterpart of [`truncate_utf8_safe`].
#[inline]
pub fn truncate_str_utf8_safe(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Fast O(1)-for-ASCII Unicode scalar length of a line.
#[inline]
pub fn char_len_fast(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.chars().count()
    }
}

/// Fast O(1)-for-ASCII conversion from 0-based `char_col` to byte offset in `s`.
#[inline]
pub fn char_to_byte_idx(s: &str, char_col: usize) -> usize {
    if s.is_ascii() {
        return char_col.min(s.len());
    }
    if char_col == 0 {
        return 0;
    }
    match s.char_indices().nth(char_col) {
        Some((b, _)) => b,
        None => s.len(),
    }
}

/// Fast O(1)-for-ASCII conversion from a valid UTF-8 byte offset in `s` to 0-based `char_col`.
#[inline]
pub fn byte_to_char_idx(s: &str, byte_idx: usize) -> usize {
    let clamped = byte_idx.min(s.len());
    if s.is_ascii() {
        clamped
    } else {
        s[..clamped].chars().count()
    }
}

#[derive(Debug, Clone)]
struct RopeDoc {
    chunks: Vec<Arc<Vec<String>>>,
    prefix_ends: Vec<usize>,
    total_lines: usize,
    version: i64,
}

#[derive(Debug, Clone)]
struct DocSnapshot {
    chunks: Vec<Arc<Vec<String>>>,
    total_lines: usize,
    owner_doc_id: i64,
}

struct DocStoreInner {
    next_doc_id: i64,
    next_snap_id: i64,
    docs: HashMap<i64, RopeDoc>,
    snapshots: HashMap<i64, DocSnapshot>,
}

impl DocStoreInner {
    fn new() -> Self {
        Self {
            next_doc_id: 1,
            next_snap_id: 1,
            docs: HashMap::new(),
            snapshots: HashMap::new(),
        }
    }
}

static GLOBAL_DOC_STORE: OnceLock<Mutex<DocStoreInner>> = OnceLock::new();

fn with_store<R>(f: impl FnOnce(&mut DocStoreInner) -> R) -> R {
    let mtx = GLOBAL_DOC_STORE.get_or_init(|| Mutex::new(DocStoreInner::new()));
    let mut guard = mtx.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

fn pack_lines_into_chunks(lines: Vec<String>) -> Vec<Arc<Vec<String>>> {
    if lines.is_empty() {
        return vec![Arc::new(vec![String::new()])];
    }
    let mut chunks = Vec::with_capacity(lines.len().div_ceil(CHUNK_TARGET));
    let mut cur = Vec::with_capacity(CHUNK_TARGET);
    for line in lines {
        cur.push(line);
        if cur.len() >= CHUNK_TARGET {
            chunks.push(Arc::new(cur));
            cur = Vec::with_capacity(CHUNK_TARGET);
        }
    }
    if !cur.is_empty() {
        chunks.push(Arc::new(cur));
    }
    chunks
}

impl RopeDoc {
    fn from_chunks(chunks: Vec<Arc<Vec<String>>>, version: i64) -> Self {
        let mut doc = Self {
            chunks,
            prefix_ends: Vec::new(),
            total_lines: 0,
            version,
        };
        doc.rebuild_index();
        doc
    }

    fn from_lines(lines: Vec<String>) -> Self {
        Self::from_chunks(pack_lines_into_chunks(lines), 1)
    }

    fn rebuild_index(&mut self) {
        self.chunks.retain(|c| !c.is_empty());
        if self.chunks.is_empty() {
            self.chunks.push(Arc::new(vec![String::new()]));
        }
        self.prefix_ends.clear();
        self.prefix_ends.reserve(self.chunks.len());
        let mut sum = 0usize;
        for c in &self.chunks {
            sum += c.len();
            self.prefix_ends.push(sum);
        }
        self.total_lines = sum;
    }

    #[inline]
    fn locate(&self, row: usize) -> (usize, usize) {
        let ci = self.prefix_ends.partition_point(|&end| end <= row);
        let prev_end = if ci == 0 { 0 } else { self.prefix_ends[ci - 1] };
        (ci, row - prev_end)
    }

    #[inline]
    fn line_ref(&self, row: usize) -> &str {
        let (ci, off) = self.locate(row);
        &self.chunks[ci][off]
    }

    fn iter_lines(&self) -> impl Iterator<Item = &str> {
        self.chunks
            .iter()
            .flat_map(|c| c.iter().map(String::as_str))
    }

    fn iter_range(&self, start_row: usize, end_row_exclusive: usize) -> RopeLineIter<'_> {
        let end = end_row_exclusive.min(self.total_lines);
        let start = start_row.min(end);
        if start >= end {
            return RopeLineIter {
                chunks: &self.chunks,
                ci: 0,
                off: 0,
                remaining: 0,
            };
        }
        let (ci, off) = self.locate(start);
        RopeLineIter {
            chunks: &self.chunks,
            ci,
            off,
            remaining: end - start,
        }
    }

    fn rebalance_chunk(&mut self, ci: usize) {
        if ci >= self.chunks.len() {
            self.rebuild_index();
            return;
        }
        if self.chunks[ci].is_empty() {
            self.chunks.remove(ci);
            self.rebuild_index();
            return;
        }
        if self.chunks[ci].len() > CHUNK_MAX {
            let vec_mut = Arc::make_mut(&mut self.chunks[ci]);
            if vec_mut.len() <= CHUNK_MAX * 2 {
                let mid = vec_mut.len() / 2;
                let right = vec_mut.split_off(mid);
                self.chunks.insert(ci + 1, Arc::new(right));
            } else {
                let drained = std::mem::take(vec_mut);
                let packed = pack_lines_into_chunks(drained);
                self.chunks.splice(ci..=ci, packed);
            }
            self.rebuild_index();
            return;
        }
        if self.chunks[ci].len() < CHUNK_MIN && self.chunks.len() > 1 {
            let ni = if ci + 1 < self.chunks.len() {
                ci + 1
            } else {
                ci - 1
            };
            let (lo_i, hi_i) = if ci < ni { (ci, ni) } else { (ni, ci) };
            let combined_len = self.chunks[lo_i].len() + self.chunks[hi_i].len();
            if combined_len <= CHUNK_MAX {
                let right_arc = self.chunks.remove(hi_i);
                let left_mut = Arc::make_mut(&mut self.chunks[lo_i]);
                match Arc::try_unwrap(right_arc) {
                    Ok(mut right_vec) => left_mut.append(&mut right_vec),
                    Err(shared) => left_mut.extend(shared.iter().cloned()),
                }
            } else {
                let half = combined_len / 2;
                let mut combined = Vec::with_capacity(combined_len);
                combined.extend(self.chunks[lo_i].iter().cloned());
                combined.extend(self.chunks[hi_i].iter().cloned());
                let right = combined.split_off(half);
                self.chunks[lo_i] = Arc::new(combined);
                self.chunks[hi_i] = Arc::new(right);
            }
        }
        self.rebuild_index();
    }

    fn set_line(&mut self, row: usize, text: String) -> bool {
        if row >= self.total_lines {
            return false;
        }
        let (ci, off) = self.locate(row);
        Arc::make_mut(&mut self.chunks[ci])[off] = text;
        self.version += 1;
        true
    }

    fn insert_line(&mut self, row: usize, text: String) {
        let idx = row.min(self.total_lines);
        let (ci, off) = if idx == self.total_lines {
            let last = self.chunks.len() - 1;
            (last, self.chunks[last].len())
        } else {
            self.locate(idx)
        };
        Arc::make_mut(&mut self.chunks[ci]).insert(off, text);
        self.rebalance_chunk(ci);
        self.version += 1;
    }

    fn insert_lines(&mut self, row: usize, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        let idx = row.min(self.total_lines);
        let (ci, off) = if idx == self.total_lines {
            let last = self.chunks.len() - 1;
            (last, self.chunks[last].len())
        } else {
            self.locate(idx)
        };
        if self.chunks[ci].len() + lines.len() <= CHUNK_MAX {
            Arc::make_mut(&mut self.chunks[ci]).splice(off..off, lines);
            self.rebuild_index();
        } else {
            let cur_mut = Arc::make_mut(&mut self.chunks[ci]);
            let tail = cur_mut.split_off(off);
            let mut combined = Vec::with_capacity(cur_mut.len() + lines.len() + tail.len());
            combined.append(cur_mut);
            combined.extend(lines);
            combined.extend(tail);
            let packed = pack_lines_into_chunks(combined);
            self.chunks.splice(ci..=ci, packed);
            self.rebuild_index();
        }
        self.version += 1;
    }

    fn remove_line(&mut self, row: usize) -> String {
        if row >= self.total_lines {
            return String::new();
        }
        if self.total_lines == 1 {
            let slot = &mut Arc::make_mut(&mut self.chunks[0])[0];
            let old = std::mem::take(slot);
            self.version += 1;
            return truncate_utf8_safe(old, MAX_NESSA_STR_BYTES);
        }
        let (ci, off) = self.locate(row);
        let old = Arc::make_mut(&mut self.chunks[ci]).remove(off);
        self.rebalance_chunk(ci);
        self.version += 1;
        truncate_utf8_safe(old, MAX_NESSA_STR_BYTES)
    }

    fn drop_row_range(&mut self, lo: usize, hi_inclusive: usize) {
        if lo == 0 && hi_inclusive + 1 >= self.total_lines {
            self.chunks = vec![Arc::new(vec![String::new()])];
            self.rebuild_index();
            return;
        }
        let (c_start, off_start) = self.locate(lo);
        let (c_end, off_end) = self.locate(hi_inclusive);
        if c_start == c_end {
            Arc::make_mut(&mut self.chunks[c_start]).drain(off_start..=off_end);
            self.rebalance_chunk(c_start);
        } else {
            let prefix_slice = &self.chunks[c_start][..off_start];
            let suffix_slice = &self.chunks[c_end][(off_end + 1)..];
            let mut merged = Vec::with_capacity(prefix_slice.len() + suffix_slice.len());
            merged.extend(prefix_slice.iter().cloned());
            merged.extend(suffix_slice.iter().cloned());
            if merged.is_empty() {
                self.chunks.drain(c_start..=c_end);
                let check_ci = c_start.min(self.chunks.len().saturating_sub(1));
                self.rebalance_chunk(check_ci);
            } else {
                let packed = pack_lines_into_chunks(merged);
                self.chunks.splice(c_start..=c_end, packed);
                self.rebalance_chunk(c_start);
            }
        }
    }

    fn delete_lines(&mut self, start_row: i64, end_row_inclusive: i64) -> String {
        let max_r = self.total_lines as i64 - 1;
        let lo = start_row.max(0);
        let hi = end_row_inclusive.min(max_r);
        if hi < lo {
            return String::new();
        }
        let lo_u = lo as usize;
        let hi_u = hi as usize;
        let cap_target = MAX_NESSA_STR_BYTES + 16;
        let mut removed = String::new();
        for line in self.iter_range(lo_u, hi_u + 1) {
            if removed.len() < cap_target {
                let take = (cap_target - removed.len()).min(line.len());
                removed.push_str(&line[..floor_char_boundary_str(line, take)]);
                if removed.len() < cap_target {
                    removed.push('\n');
                }
            } else {
                break;
            }
        }
        self.drop_row_range(lo_u, hi_u);
        self.version += 1;
        truncate_utf8_safe(removed, MAX_NESSA_STR_BYTES)
    }

    fn clamp_pos(&self, row: i64, col: i64) -> (usize, usize) {
        let r = row.clamp(0, self.total_lines as i64 - 1) as usize;
        let llen = char_len_fast(self.line_ref(r));
        let c = col.clamp(0, llen as i64) as usize;
        (r, c)
    }

    fn normalize_span(
        &self,
        s_row: i64,
        s_col: i64,
        e_row: i64,
        e_col: i64,
    ) -> ((usize, usize), (usize, usize)) {
        let a = self.clamp_pos(s_row, s_col);
        let b = self.clamp_pos(e_row, e_col);
        if a <= b { (a, b) } else { (b, a) }
    }

    fn extract_span_raw(&self, s: (usize, usize), e: (usize, usize)) -> String {
        if s == e {
            return String::new();
        }
        if s.0 == e.0 {
            let line = self.line_ref(s.0);
            let sb = char_to_byte_idx(line, s.1);
            let eb = char_to_byte_idx(line, e.1);
            return truncate_str_utf8_safe(&line[sb..eb], MAX_NESSA_STR_BYTES);
        }
        let cap_target = MAX_NESSA_STR_BYTES + 16;
        let mut out = String::new();

        let first = self.line_ref(s.0);
        let first_sb = char_to_byte_idx(first, s.1);
        let first_tail = &first[first_sb..];
        let take_first = first_tail.len().min(cap_target);
        out.push_str(&first_tail[..floor_char_boundary_str(first_tail, take_first)]);
        if out.len() < cap_target {
            out.push('\n');
        }

        if out.len() < cap_target && s.0 + 1 < e.0 {
            for mid in self.iter_range(s.0 + 1, e.0) {
                let rem = cap_target - out.len();
                let take = mid.len().min(rem);
                out.push_str(&mid[..floor_char_boundary_str(mid, take)]);
                if out.len() < cap_target {
                    out.push('\n');
                } else {
                    break;
                }
            }
        }

        if out.len() < cap_target {
            let last = self.line_ref(e.0);
            let last_eb = char_to_byte_idx(last, e.1);
            let last_head = &last[..last_eb];
            let rem = cap_target - out.len();
            let take = last_head.len().min(rem);
            out.push_str(&last_head[..floor_char_boundary_str(last_head, take)]);
        }

        truncate_utf8_safe(out, MAX_NESSA_STR_BYTES)
    }

    fn insert_text(&mut self, row: i64, col: i64, text: &str) -> (i64, i64) {
        let (r, c) = self.clamp_pos(row, col);
        if text.is_empty() {
            return (r as i64, c as i64);
        }
        let cur_line = self.line_ref(r);
        let split_b = char_to_byte_idx(cur_line, c);
        let prefix = cur_line[..split_b].to_string();
        let suffix = cur_line[split_b..].to_string();

        if !text.contains('\n') && !text.contains('\r') {
            let mut updated = String::with_capacity(prefix.len() + text.len() + suffix.len());
            updated.push_str(&prefix);
            updated.push_str(text);
            updated.push_str(&suffix);
            let (ci, off) = self.locate(r);
            Arc::make_mut(&mut self.chunks[ci])[off] = updated;
            self.version += 1;
            return (r as i64, (c + char_len_fast(text)) as i64);
        }

        let normalized: std::borrow::Cow<'_, str> = if text.contains('\r') {
            std::borrow::Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
        } else {
            std::borrow::Cow::Borrowed(text)
        };

        let parts: Vec<&str> = normalized.split('\n').collect();
        if parts.len() == 1 {
            let only = parts[0];
            let mut updated = String::with_capacity(prefix.len() + only.len() + suffix.len());
            updated.push_str(&prefix);
            updated.push_str(only);
            updated.push_str(&suffix);
            let (ci, off) = self.locate(r);
            Arc::make_mut(&mut self.chunks[ci])[off] = updated;
            self.version += 1;
            return (r as i64, (c + char_len_fast(only)) as i64);
        }

        let mut first_line = String::with_capacity(prefix.len() + parts[0].len());
        first_line.push_str(&prefix);
        first_line.push_str(parts[0]);
        let (ci, off) = self.locate(r);
        Arc::make_mut(&mut self.chunks[ci])[off] = first_line;

        let last_part = parts[parts.len() - 1];
        let last_part_chars = char_len_fast(last_part);
        let mut extra = Vec::with_capacity(parts.len() - 1);
        for mid in &parts[1..parts.len() - 1] {
            extra.push((*mid).to_string());
        }
        let mut final_line = String::with_capacity(last_part.len() + suffix.len());
        final_line.push_str(last_part);
        final_line.push_str(&suffix);
        extra.push(final_line);

        let ins_count = extra.len();
        // Insert `extra` right after `r` (note: `insert_lines` bumps version once, so reuse helper without double-bumping)
        let prev_ver = self.version;
        self.insert_lines(r + 1, extra);
        self.version = prev_ver + 1;

        ((r + ins_count) as i64, last_part_chars as i64)
    }

    fn delete_span(&mut self, s_row: i64, s_col: i64, e_row: i64, e_col: i64) -> String {
        let (s, e) = self.normalize_span(s_row, s_col, e_row, e_col);
        if s == e {
            return String::new();
        }
        let removed = self.extract_span_raw(s, e);
        if s.0 == e.0 {
            let line = self.line_ref(s.0);
            let sb = char_to_byte_idx(line, s.1);
            let eb = char_to_byte_idx(line, e.1);
            let mut updated = String::with_capacity(sb + (line.len() - eb));
            updated.push_str(&line[..sb]);
            updated.push_str(&line[eb..]);
            let (ci, off) = self.locate(s.0);
            Arc::make_mut(&mut self.chunks[ci])[off] = updated;
            self.version += 1;
            return removed;
        }

        let first = self.line_ref(s.0);
        let sb = char_to_byte_idx(first, s.1);
        let first_prefix = first[..sb].to_string();

        let last = self.line_ref(e.0);
        let eb = char_to_byte_idx(last, e.1);
        let last_suffix = &last[eb..];

        let mut merged_line = String::with_capacity(first_prefix.len() + last_suffix.len());
        merged_line.push_str(&first_prefix);
        merged_line.push_str(last_suffix);

        let (ci, off) = self.locate(s.0);
        Arc::make_mut(&mut self.chunks[ci])[off] = merged_line;
        self.drop_row_range(s.0 + 1, e.0);
        self.version += 1;
        removed
    }
}

struct RopeLineIter<'a> {
    chunks: &'a [Arc<Vec<String>>],
    ci: usize,
    off: usize,
    remaining: usize,
}

impl<'a> Iterator for RopeLineIter<'a> {
    type Item = &'a str;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 || self.ci >= self.chunks.len() {
            return None;
        }
        let chunk = &self.chunks[self.ci];
        let s = chunk[self.off].as_str();
        self.remaining -= 1;
        self.off += 1;
        if self.off >= chunk.len() {
            self.ci += 1;
            self.off = 0;
        }
        Some(s)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for RopeLineIter<'_> {}

#[inline]
fn floor_char_boundary_str(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

// ---------------------------------------------------------------------------
// Public Rope Document Store Operations
// ---------------------------------------------------------------------------

pub fn doc_new(initial_lines: Vec<String>) -> i64 {
    let doc = RopeDoc::from_lines(initial_lines);
    with_store(|store| {
        let id = store.next_doc_id;
        store.next_doc_id += 1;
        store.docs.insert(id, doc);
        id
    })
}

/// Fast single-syscall file reader into a `CHUNK_TARGET` Copy-on-Write rope.
/// Preserves exact newline roundtrip semantics of `host_read_file_lines`.
pub fn doc_open_file(file_path: &str) -> Result<(i64, i64, bool), String> {
    let bytes =
        fs::read(file_path).map_err(|err| format!("cannot open file '{file_path}': {err}"))?;
    if bytes.is_empty() {
        let doc = RopeDoc::from_lines(vec![String::new()]);
        let id = with_store(|store| {
            let id = store.next_doc_id;
            store.next_doc_id += 1;
            store.docs.insert(id, doc);
            id
        });
        return Ok((id, 1, false));
    }

    let has_crlf = bytes.windows(2).any(|w| w == b"\r\n");
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(err) => String::from_utf8_lossy(&err.into_bytes()).into_owned(),
    };

    let body = text.strip_suffix('\n').unwrap_or(&text);
    let mut chunks = Vec::with_capacity((body.len() / 8192).max(1));
    let mut cur = Vec::with_capacity(CHUNK_TARGET);
    let mut total_lines = 0usize;
    for raw_line in body.split('\n') {
        let clean = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        cur.push(clean.to_string());
        total_lines += 1;
        if cur.len() >= CHUNK_TARGET {
            chunks.push(Arc::new(cur));
            cur = Vec::with_capacity(CHUNK_TARGET);
        }
    }
    if !cur.is_empty() {
        chunks.push(Arc::new(cur));
    }
    if chunks.is_empty() {
        chunks.push(Arc::new(vec![String::new()]));
        total_lines = 1;
    }

    let doc = RopeDoc::from_chunks(chunks, 1);
    debug_assert_eq!(doc.total_lines, total_lines);
    let line_count = doc.total_lines as i64;
    let id = with_store(|store| {
        let id = store.next_doc_id;
        store.next_doc_id += 1;
        store.docs.insert(id, doc);
        id
    });
    Ok((id, line_count, has_crlf))
}

/// Stream untruncated rope chunks to disk via `BufWriter<File>`, matching `host_write_file_lines(..., true)`.
pub fn doc_save_file(doc_id: i64, file_path: &str, crlf: bool) -> Result<i64, String> {
    let chunks = with_store(|store| store.docs.get(&doc_id).map(|d| d.chunks.clone()))
        .ok_or_else(|| format!("unknown document id {doc_id}"))?;

    let file = File::create(file_path)
        .map_err(|err| format!("cannot open file '{file_path}' for write: {err}"))?;
    let mut writer = BufWriter::with_capacity(64 * 1024, file);
    let sep: &[u8] = if crlf { b"\r\n" } else { b"\n" };
    let mut bytes_written: i64 = 0;

    for chunk in &chunks {
        for line in chunk.iter() {
            writer
                .write_all(line.as_bytes())
                .map_err(|err| format!("write failed on '{file_path}': {err}"))?;
            writer
                .write_all(sep)
                .map_err(|err| format!("write newline failed on '{file_path}': {err}"))?;
            bytes_written = bytes_written
                .saturating_add(line.len() as i64)
                .saturating_add(sep.len() as i64);
        }
    }
    writer
        .flush()
        .map_err(|err| format!("flush failed on '{file_path}': {err}"))?;

    Ok(bytes_written)
}

pub fn doc_clone_from_lines(doc_id: i64, lines: Vec<String>) {
    let new_chunks = pack_lines_into_chunks(lines);
    with_store(|store| {
        if let Some(doc) = store.docs.get_mut(&doc_id) {
            doc.chunks = new_chunks;
            doc.rebuild_index();
            doc.version += 1;
        } else if doc_id > 0 {
            let doc = RopeDoc::from_chunks(new_chunks, 1);
            if doc_id >= store.next_doc_id {
                store.next_doc_id = doc_id + 1;
            }
            store.docs.insert(doc_id, doc);
        }
    });
}

pub fn doc_free(doc_id: i64) {
    with_store(|store| {
        store.docs.remove(&doc_id);
        store
            .snapshots
            .retain(|_, snap| snap.owner_doc_id != doc_id);
    });
}

pub fn doc_line_count(doc_id: i64) -> i64 {
    with_store(|store| store.docs.get(&doc_id).map_or(0, |d| d.total_lines as i64))
}

pub fn doc_version(doc_id: i64) -> i64 {
    with_store(|store| store.docs.get(&doc_id).map_or(0, |d| d.version))
}

pub(crate) fn doc_version_and_line_count(doc_id: i64) -> Option<(i64, usize)> {
    with_store(|store| {
        let d = store.docs.get(&doc_id)?;
        Some((d.version, d.total_lines))
    })
}

pub(crate) type DocChunkList = Vec<Arc<Vec<String>>>;
pub(crate) type DocChunkSnapshot = (i64, DocChunkList, usize);

pub(crate) fn doc_snapshot_source_and_offsets(doc_id: i64) -> Option<DocChunkSnapshot> {
    with_store(|store| {
        let d = store.docs.get(&doc_id)?;
        Some((d.version, d.chunks.clone(), d.total_lines))
    })
}

pub fn doc_line_at(doc_id: i64, row: i64) -> String {
    if row < 0 {
        return String::new();
    }
    with_store(|store| {
        let Some(doc) = store.docs.get(&doc_id) else {
            return String::new();
        };
        let r = row as usize;
        if r >= doc.total_lines {
            return String::new();
        }
        truncate_str_utf8_safe(doc.line_ref(r), MAX_NESSA_STR_BYTES)
    })
}

pub fn doc_line_char_len(doc_id: i64, row: i64) -> i64 {
    if row < 0 {
        return 0;
    }
    with_store(|store| {
        let Some(doc) = store.docs.get(&doc_id) else {
            return 0;
        };
        let r = row as usize;
        if r >= doc.total_lines {
            return 0;
        }
        char_len_fast(doc.line_ref(r)) as i64
    })
}

pub fn doc_slice_lines(doc_id: i64, start_row: i64, end_row_exclusive: i64) -> Vec<String> {
    with_store(|store| {
        let Some(doc) = store.docs.get(&doc_id) else {
            return Vec::new();
        };
        let start = start_row.max(0) as usize;
        let end = end_row_exclusive.max(0) as usize;
        let clamped_start = start.min(doc.total_lines);
        let clamped_end = end
            .min(doc.total_lines)
            .max(clamped_start)
            .min(clamped_start.saturating_add(MAX_NESSA_LIST_ITEMS));
        let mut out = Vec::with_capacity(clamped_end - clamped_start);
        for line in doc.iter_range(clamped_start, clamped_end) {
            out.push(truncate_str_utf8_safe(line, MAX_NESSA_STR_BYTES));
        }
        out
    })
}

pub fn doc_set_line(doc_id: i64, row: i64, text: String) {
    if row < 0 {
        return;
    }
    with_store(|store| {
        if let Some(doc) = store.docs.get_mut(&doc_id) {
            doc.set_line(row as usize, text);
        }
    });
}

pub fn doc_insert_line(doc_id: i64, row: i64, text: String) {
    with_store(|store| {
        if let Some(doc) = store.docs.get_mut(&doc_id) {
            let idx = row.max(0) as usize;
            doc.insert_line(idx, text);
        }
    });
}

pub fn doc_insert_lines(doc_id: i64, row: i64, lines: Vec<String>) {
    if lines.is_empty() {
        return;
    }
    with_store(|store| {
        if let Some(doc) = store.docs.get_mut(&doc_id) {
            let idx = row.max(0) as usize;
            doc.insert_lines(idx, lines);
        }
    });
}

pub fn doc_remove_line(doc_id: i64, row: i64) -> String {
    if row < 0 {
        return String::new();
    }
    with_store(|store| {
        let Some(doc) = store.docs.get_mut(&doc_id) else {
            return String::new();
        };
        doc.remove_line(row as usize)
    })
}

pub fn doc_delete_lines(doc_id: i64, start_row: i64, end_row_inclusive: i64) -> String {
    with_store(|store| {
        let Some(doc) = store.docs.get_mut(&doc_id) else {
            return String::new();
        };
        doc.delete_lines(start_row, end_row_inclusive)
    })
}

pub fn doc_insert_text(doc_id: i64, row: i64, col: i64, text: &str) -> (i64, i64) {
    with_store(|store| {
        let Some(doc) = store.docs.get_mut(&doc_id) else {
            return (0, 0);
        };
        doc.insert_text(row, col, text)
    })
}

pub fn doc_delete_span(doc_id: i64, s_row: i64, s_col: i64, e_row: i64, e_col: i64) -> String {
    with_store(|store| {
        let Some(doc) = store.docs.get_mut(&doc_id) else {
            return String::new();
        };
        doc.delete_span(s_row, s_col, e_row, e_col)
    })
}

pub fn doc_extract_span(doc_id: i64, s_row: i64, s_col: i64, e_row: i64, e_col: i64) -> String {
    with_store(|store| {
        let Some(doc) = store.docs.get(&doc_id) else {
            return String::new();
        };
        let (s, e) = doc.normalize_span(s_row, s_col, e_row, e_col);
        doc.extract_span_raw(s, e)
    })
}

pub fn doc_to_text_capped(doc_id: i64, crlf: bool) -> String {
    let sep = if crlf { "\r\n" } else { "\n" };
    let cap_target = MAX_NESSA_STR_BYTES + 16;
    with_store(|store| {
        let Some(doc) = store.docs.get(&doc_id) else {
            return String::new();
        };
        let mut out = String::new();
        for (idx, line) in doc.iter_lines().enumerate() {
            if idx > 0 {
                if out.len() + sep.len() <= cap_target {
                    out.push_str(sep);
                } else {
                    break;
                }
            }
            if out.len() >= cap_target {
                break;
            }
            let rem = cap_target - out.len();
            let take = line.len().min(rem);
            out.push_str(&line[..floor_char_boundary_str(line, take)]);
        }
        truncate_utf8_safe(out, MAX_NESSA_STR_BYTES)
    })
}

/// Untruncated full document text for Rust-side LSP synchronization (`didOpen`/`didChange`/`didSave`).
pub fn doc_full_text(doc_id: i64, crlf: bool) -> Option<String> {
    let sep = if crlf { "\r\n" } else { "\n" };
    let (chunks, total_lines) = with_store(|store| {
        let d = store.docs.get(&doc_id)?;
        Some((d.chunks.clone(), d.total_lines))
    })?;
    let total_bytes: usize = chunks
        .iter()
        .map(|c| c.iter().map(String::len).sum::<usize>())
        .sum::<usize>()
        + total_lines.saturating_sub(1) * sep.len();
    let mut out = String::with_capacity(total_bytes);
    let mut first = true;
    for chunk in &chunks {
        for line in chunk.iter() {
            if !first {
                out.push_str(sep);
            }
            first = false;
            out.push_str(line);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Copy-on-Write Snapshots (O(chunks) Arc clones, < 1 µs equality)
// ---------------------------------------------------------------------------

pub fn doc_snapshot_take(doc_id: i64) -> i64 {
    with_store(|store| {
        let Some(doc) = store.docs.get(&doc_id) else {
            return 0;
        };
        let snap = DocSnapshot {
            chunks: doc.chunks.clone(),
            total_lines: doc.total_lines,
            owner_doc_id: doc_id,
        };
        let id = store.next_snap_id;
        store.next_snap_id += 1;
        store.snapshots.insert(id, snap);
        id
    })
}

pub fn doc_snapshot_matches(doc_id: i64, snap_id: i64) -> bool {
    with_store(|store| {
        let (Some(doc), Some(snap)) = (store.docs.get(&doc_id), store.snapshots.get(&snap_id))
        else {
            return false;
        };
        if doc.total_lines != snap.total_lines {
            return false;
        }
        if doc.chunks.len() == snap.chunks.len()
            && doc
                .chunks
                .iter()
                .zip(snap.chunks.iter())
                .all(|(a, b)| Arc::ptr_eq(a, b) || a == b)
        {
            return true;
        }
        let snap_iter = snap
            .chunks
            .iter()
            .flat_map(|c| c.iter().map(String::as_str));
        doc.iter_lines().eq(snap_iter)
    })
}

pub fn doc_snapshot_restore(doc_id: i64, snap_id: i64) -> i64 {
    with_store(|store| {
        let Some(snap_chunks) = store.snapshots.get(&snap_id).map(|s| s.chunks.clone()) else {
            return 0;
        };
        let Some(doc) = store.docs.get_mut(&doc_id) else {
            return 0;
        };
        doc.chunks = snap_chunks;
        doc.rebuild_index();
        doc.version += 1;
        doc.version
    })
}

// ---------------------------------------------------------------------------
// Native Search & Bounded Bracket Matching
// ---------------------------------------------------------------------------

struct PreparedNeedle<'a> {
    raw: &'a str,
    lower: String,
    ignore_case: bool,
    ascii_fast: bool,
}

impl<'a> PreparedNeedle<'a> {
    fn new(needle: &'a str, ignore_case: bool) -> Self {
        let lower = if ignore_case {
            needle.to_lowercase()
        } else {
            String::new()
        };
        let ascii_fast = ignore_case && needle.is_ascii();
        Self {
            raw: needle,
            lower,
            ignore_case,
            ascii_fast,
        }
    }

    /// Find first occurrence in `line` at a character column `>= min_char_col`.
    fn find_first_from(&self, line: &str, min_char_col: usize) -> Option<usize> {
        if !self.ignore_case {
            let start_b = char_to_byte_idx(line, min_char_col);
            if start_b > line.len() {
                return None;
            }
            let rel_b = line[start_b..].find(self.raw)?;
            return Some(min_char_col + byte_to_char_idx(&line[start_b..], rel_b));
        }

        if self.ascii_fast && line.is_ascii() {
            let lb = line.as_bytes();
            let nb = self.lower.as_bytes();
            let start = min_char_col.min(lb.len());
            if nb.is_empty() || start + nb.len() > lb.len() {
                return None;
            }
            let first_lo = nb[0];
            let first_hi = first_lo.to_ascii_uppercase();
            let end_i = lb.len() - nb.len();
            let mut i = start;
            while i <= end_i {
                let b = lb[i];
                if (b == first_lo || b == first_hi) && lb[i..i + nb.len()].eq_ignore_ascii_case(nb)
                {
                    return Some(i);
                }
                i += 1;
            }
            return None;
        }

        // General Unicode case-insensitive fallback: compare lowercased slice at char offsets
        let start_b = char_to_byte_idx(line, min_char_col);
        if start_b >= line.len() {
            return None;
        }
        for (char_delta, (rel_b, _)) in line[start_b..].char_indices().enumerate() {
            let tail_low = line[start_b + rel_b..].to_lowercase();
            if tail_low.starts_with(&self.lower) {
                return Some(min_char_col + char_delta);
            }
        }
        None
    }

    /// Find last occurrence in `line` at a character column `<= max_char_col`.
    fn find_last_up_to(&self, line: &str, max_char_col: i64) -> Option<usize> {
        if max_char_col < 0 {
            return None;
        }
        let max_c = max_char_col as usize;
        if !self.ignore_case {
            if line.is_ascii() {
                let lb = line.as_bytes();
                let nb = self.raw.as_bytes();
                if nb.is_empty() || nb.len() > lb.len() {
                    return None;
                }
                let hi_end = max_c.saturating_add(nb.len()).min(lb.len());
                return line[..hi_end].rfind(self.raw);
            }
            let mut best: Option<usize> = None;
            let mut search_from = 0usize;
            while search_from <= line.len() {
                let Some(rel) = line[search_from..].find(self.raw) else {
                    break;
                };
                let abs_b = search_from + rel;
                let cc = byte_to_char_idx(line, abs_b);
                if cc <= max_c {
                    best = Some(cc);
                    let step = line[abs_b..].chars().next().map_or(1, char::len_utf8);
                    search_from = abs_b + step;
                } else {
                    break;
                }
            }
            return best;
        }

        if self.ascii_fast && line.is_ascii() {
            let lb = line.as_bytes();
            let nb = self.lower.as_bytes();
            if nb.is_empty() || nb.len() > lb.len() {
                return None;
            }
            let last_start = max_c.min(lb.len() - nb.len());
            let first_lo = nb[0];
            let first_hi = first_lo.to_ascii_uppercase();
            let mut i = last_start + 1;
            while i > 0 {
                i -= 1;
                let b = lb[i];
                if (b == first_lo || b == first_hi) && lb[i..i + nb.len()].eq_ignore_ascii_case(nb)
                {
                    return Some(i);
                }
            }
            return None;
        }

        let mut best = None;
        for (cc, (b_idx, _)) in line.char_indices().enumerate() {
            if cc > max_c {
                break;
            }
            if line[b_idx..].to_lowercase().starts_with(&self.lower) {
                best = Some(cc);
            }
        }
        best
    }

    /// Count non-overlapping occurrences in `line` and return `(count, first_char_col)`.
    fn count_and_first_col(&self, line: &str) -> (usize, usize) {
        if !self.ignore_case {
            if self.raw.is_empty() || line.len() < self.raw.len() {
                return (0, 0);
            }
            let Some(first_b) = line.find(self.raw) else {
                return (0, 0);
            };
            let first_col = byte_to_char_idx(line, first_b);
            let mut count = 1usize;
            let mut rest = &line[first_b + self.raw.len()..];
            while let Some(rel) = rest.find(self.raw) {
                count += 1;
                rest = &rest[rel + self.raw.len()..];
            }
            return (count, first_col);
        }

        if self.ascii_fast && line.is_ascii() {
            let lb = line.as_bytes();
            let nb = self.lower.as_bytes();
            if nb.is_empty() || lb.len() < nb.len() {
                return (0, 0);
            }
            let first_lo = nb[0];
            let first_hi = first_lo.to_ascii_uppercase();
            let end_i = lb.len() - nb.len();
            let mut i = 0usize;
            let mut count = 0usize;
            let mut first_col = 0usize;
            while i <= end_i {
                let b = lb[i];
                if (b == first_lo || b == first_hi) && lb[i..i + nb.len()].eq_ignore_ascii_case(nb)
                {
                    if count == 0 {
                        first_col = i;
                    }
                    count += 1;
                    i += nb.len();
                } else {
                    i += 1;
                }
            }
            return (count, first_col);
        }

        let low = line.to_lowercase();
        let pat = self.lower.as_str();
        if pat.is_empty() || low.len() < pat.len() {
            return (0, 0);
        }
        let Some(first_b) = low.find(pat) else {
            return (0, 0);
        };
        let first_col = byte_to_char_idx(&low, first_b);
        let count = low.matches(pat).count();
        (count, first_col)
    }
}

pub fn doc_search_preview(
    doc_id: i64,
    needle: &str,
    min_row: i64,
    max_row: i64,
    ignore_case: bool,
    max_previews: usize,
) -> (i64, Vec<(i64, i64, String)>) {
    if needle.is_empty() {
        return (0, Vec::new());
    }
    let Some(chunks) = with_store(|store| {
        store
            .docs
            .get(&doc_id)
            .map(|d| (d.chunks.clone(), d.prefix_ends.clone(), d.total_lines))
    }) else {
        return (0, Vec::new());
    };
    let doc = RopeDoc {
        chunks: chunks.0,
        prefix_ends: chunks.1,
        total_lines: chunks.2,
        version: 0,
    };
    if doc.total_lines == 0 {
        return (0, Vec::new());
    }
    let max_valid = doc.total_lines as i64 - 1;
    let lo = min_row.clamp(0, max_valid) as usize;
    let hi = max_row.clamp(lo as i64, max_valid) as usize;
    let preview_cap = max_previews.min(64);

    let prep = PreparedNeedle::new(needle, ignore_case);
    let mut total_hits: i64 = 0;
    let mut previews = Vec::with_capacity(preview_cap);

    // Cap match counting at 99_999 on huge 1-character queries once previews are full
    // so `/f` on a 250k-line amalgamation stays sub-millisecond.
    let single_char_huge = hi.saturating_sub(lo) > 25_000 && needle.len() < 2;

    for (offset, line) in doc.iter_range(lo, hi + 1).enumerate() {
        let (cnt, first_col) = prep.count_and_first_col(line);
        if cnt > 0 {
            total_hits = total_hits.saturating_add(cnt as i64);
            if previews.len() < preview_cap {
                let trimmed = line.trim();
                let clipped = if trimmed.len() <= 60 {
                    trimmed.to_string()
                } else {
                    let mut chars = trimmed.chars();
                    let head: String = chars.by_ref().take(60).collect();
                    if chars.next().is_some() {
                        format!("{head}...")
                    } else {
                        head
                    }
                };
                previews.push(((lo + offset) as i64, first_col as i64, clipped));
            } else if single_char_huge && total_hits >= 10_000 {
                break;
            }
        }
    }

    (total_hits, previews)
}

#[allow(clippy::too_many_arguments)]
pub fn doc_find_forward(
    doc_id: i64,
    needle: &str,
    from_row: i64,
    from_col: i64,
    min_row: i64,
    max_row: i64,
    ignore_case: bool,
    wrap: bool,
) -> Option<(i64, i64)> {
    if needle.is_empty() {
        return None;
    }
    let chunks = with_store(|store| {
        store
            .docs
            .get(&doc_id)
            .map(|d| (d.chunks.clone(), d.prefix_ends.clone(), d.total_lines))
    })?;
    let doc = RopeDoc {
        chunks: chunks.0,
        prefix_ends: chunks.1,
        total_lines: chunks.2,
        version: 0,
    };
    if doc.total_lines == 0 {
        return None;
    }
    let max_valid = doc.total_lines as i64 - 1;
    let lo = min_row.clamp(0, max_valid) as usize;
    let hi = max_row.clamp(lo as i64, max_valid) as usize;
    let fr = from_row.clamp(lo as i64, hi as i64) as usize;

    let prep = PreparedNeedle::new(needle, ignore_case);

    // First line (`fr`): strictly after `from_col` (`sc > from_col`)
    let min_first_col = if from_col < 0 {
        0usize
    } else {
        (from_col as usize).saturating_add(1)
    };
    if let Some(sc) = prep.find_first_from(doc.line_ref(fr), min_first_col) {
        return Some((fr as i64, sc as i64));
    }

    // Subsequent lines `(fr + 1)..=hi`
    if fr < hi {
        for (offset, line) in doc.iter_range(fr + 1, hi + 1).enumerate() {
            if let Some(sc) = prep.find_first_from(line, 0) {
                return Some(((fr + 1 + offset) as i64, sc as i64));
            }
        }
    }

    if wrap {
        for (offset, line) in doc.iter_range(lo, fr + 1).enumerate() {
            let wr = lo + offset;
            if let Some(sc) = prep.find_first_from(line, 0)
                && (wr < fr || (sc as i64) <= from_col)
            {
                return Some((wr as i64, sc as i64));
            }
        }
    }

    None
}

#[allow(clippy::too_many_arguments)]
pub fn doc_find_backward(
    doc_id: i64,
    needle: &str,
    from_row: i64,
    from_col: i64,
    min_row: i64,
    max_row: i64,
    ignore_case: bool,
    wrap: bool,
) -> Option<(i64, i64)> {
    if needle.is_empty() {
        return None;
    }
    let chunks = with_store(|store| {
        store
            .docs
            .get(&doc_id)
            .map(|d| (d.chunks.clone(), d.prefix_ends.clone(), d.total_lines))
    })?;
    let doc = RopeDoc {
        chunks: chunks.0,
        prefix_ends: chunks.1,
        total_lines: chunks.2,
        version: 0,
    };
    if doc.total_lines == 0 {
        return None;
    }
    let max_valid = doc.total_lines as i64 - 1;
    let lo = min_row.clamp(0, max_valid) as usize;
    let hi = max_row.clamp(lo as i64, max_valid) as usize;
    let fr = from_row.clamp(lo as i64, hi as i64) as usize;

    let prep = PreparedNeedle::new(needle, ignore_case);

    // Current line (`fr`): strictly before `from_col` (`sc < from_col` <=> `sc <= from_col - 1`)
    if let Some(sc) = prep.find_last_up_to(doc.line_ref(fr), from_col - 1) {
        return Some((fr as i64, sc as i64));
    }

    // Preceding lines `(lo..fr).rev()`
    for r in (lo..fr).rev() {
        let line = doc.line_ref(r);
        let max_c = char_len_fast(line) as i64;
        if let Some(sc) = prep.find_last_up_to(line, max_c) {
            return Some((r as i64, sc as i64));
        }
    }

    if wrap {
        for wr in (fr..=hi).rev() {
            let line = doc.line_ref(wr);
            let max_c = char_len_fast(line) as i64;
            if let Some(sc) = prep.find_last_up_to(line, max_c)
                && (wr > fr || (sc as i64) >= from_col)
            {
                return Some((wr as i64, sc as i64));
            }
        }
    }

    None
}

pub fn doc_find_matching_bracket(
    doc_id: i64,
    row: i64,
    col: i64,
    min_row: i64,
    max_row: i64,
    max_scan_lines: i64,
) -> Option<(i64, i64)> {
    let chunks = with_store(|store| {
        store
            .docs
            .get(&doc_id)
            .map(|d| (d.chunks.clone(), d.prefix_ends.clone(), d.total_lines))
    })?;
    let doc = RopeDoc {
        chunks: chunks.0,
        prefix_ends: chunks.1,
        total_lines: chunks.2,
        version: 0,
    };
    if row < 0 || row >= doc.total_lines as i64 {
        return None;
    }
    let r0 = row as usize;
    let max_valid = doc.total_lines as i64 - 1;
    let lo_base = min_row.clamp(0, max_valid);
    let hi_base = max_row.clamp(lo_base, max_valid);
    if row < lo_base || row > hi_base {
        return None;
    }

    let line0 = doc.line_ref(r0);
    let llen = char_len_fast(line0) as i64;
    if llen == 0 {
        return None;
    }
    let c0 = if col >= llen { llen - 1 } else { col };
    if c0 < 0 {
        return None;
    }
    let c0_u = c0 as usize;
    let ch_b = char_to_byte_idx(line0, c0_u);
    let ch = line0[ch_b..].chars().next()?;

    let (open_ch, close_ch, forward) = match ch {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        '}' => ('{', '}', false),
        _ => return None,
    };

    let (lo_r, hi_r) = if max_scan_lines > 0 {
        (
            lo_base.max(row.saturating_sub(max_scan_lines)) as usize,
            hi_base.min(row.saturating_add(max_scan_lines)) as usize,
        )
    } else {
        (lo_base as usize, hi_base as usize)
    };

    let mut depth = 0i64;
    if forward {
        for (offset, l) in doc.iter_range(r0, hi_r + 1).enumerate() {
            let r = r0 + offset;
            let start_c = if r == r0 { c0_u } else { 0 };
            let start_b = if start_c == 0 {
                0
            } else {
                char_to_byte_idx(l, start_c)
            };
            for (delta_c, cur) in l[start_b..].chars().enumerate() {
                if cur == open_ch {
                    depth += 1;
                } else if cur == close_ch {
                    depth -= 1;
                    if depth == 0 {
                        return Some((r as i64, (start_c + delta_c) as i64));
                    }
                }
            }
        }
    } else {
        for r in (lo_r..=r0).rev() {
            let l = doc.line_ref(r);
            if l.is_ascii() {
                let bytes = l.as_bytes();
                let end_excl = if r == r0 {
                    (c0_u + 1).min(bytes.len())
                } else {
                    bytes.len()
                };
                for c in (0..end_excl).rev() {
                    let cur = bytes[c] as char;
                    if cur == close_ch {
                        depth += 1;
                    } else if cur == open_ch {
                        depth -= 1;
                        if depth == 0 {
                            return Some((r as i64, c as i64));
                        }
                    }
                }
            } else {
                let chars: Vec<char> = l.chars().collect();
                let end_excl = if r == r0 {
                    (c0_u + 1).min(chars.len())
                } else {
                    chars.len()
                };
                for c in (0..end_excl).rev() {
                    let cur = chars[c];
                    if cur == close_ch {
                        depth += 1;
                    } else if cur == open_ch {
                        depth -= 1;
                        if depth == 0 {
                            return Some((r as i64, c as i64));
                        }
                    }
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Native Single-Pass IDE Buffer Accelerators on `doc_id`
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeDiagHit {
    pub row: i64,
    pub col: i64,
    pub end_col: i64,
    pub severity: String,
    pub source: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolOutlineSpan {
    pub name: String,
    pub kind: String,
    pub row: i64,
    pub col: i64,
    pub level: i64,
    pub end_row: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeConflictHit {
    pub start_row: i64,
    pub base_row: i64,
    pub sep_row: i64,
    pub end_row: i64,
    pub ours_label: String,
    pub theirs_label: String,
}

fn snapshot_doc_view(doc_id: i64) -> Option<RopeDoc> {
    with_store(|store| {
        let d = store.docs.get(&doc_id)?;
        Some(RopeDoc {
            chunks: d.chunks.clone(),
            prefix_ends: d.prefix_ends.clone(),
            total_lines: d.total_lines,
            version: d.version,
        })
    })
}

#[inline]
fn resolve_cap(max_items: i64, default_cap: usize) -> usize {
    if max_items <= 0 {
        default_cap
    } else {
        (max_items as usize).min(MAX_NESSA_LIST_ITEMS)
    }
}

fn is_code_mode(mode: &str) -> bool {
    !matches!(
        mode,
        "text" | "markdown" | "org" | "diff" | "messages" | "trouble" | "magit" | "dired"
    )
}

fn mode_line_comment_prefix(mode: &str) -> &'static str {
    match mode {
        "nessa" | "lua" | "sql" | "haskell" => "--",
        "rust" | "c" | "cpp" | "go" | "javascript" | "typescript" | "java" | "css" | "scss"
        | "jsonc" => "//",
        "python" | "sh" | "shell" | "toml" | "yaml" | "makefile" | "conf" => "#",
        "lisp" | "elisp" | "scheme" => ";",
        _ => "",
    }
}

fn mode_block_comment_delims(mode: &str) -> (&'static str, &'static str) {
    match mode {
        "nessa" | "lua" => ("--[[", "]]"),
        "rust" | "c" | "cpp" | "go" | "javascript" | "typescript" | "java" | "css" | "scss"
        | "jsonc" => ("/*", "*/"),
        _ => ("", ""),
    }
}

fn uses_single_quote_strings(mode: &str) -> bool {
    matches!(
        mode,
        "python" | "javascript" | "typescript" | "sh" | "shell" | "yaml" | "toml" | "css" | "scss"
    )
}

fn conflict_marker_kind(line: &str) -> Option<&'static str> {
    if line.starts_with("<<<<<<<") {
        Some("<<<<<<<")
    } else if line == "=======" || line.starts_with("======= ") {
        Some("=======")
    } else if line.starts_with(">>>>>>>") {
        Some(">>>>>>>")
    } else if line.starts_with("|||||||") {
        Some("|||||||")
    } else {
        None
    }
}

fn matching_open_for_close(close_ch: char) -> char {
    match close_ch {
        ')' => '(',
        ']' => '[',
        '}' => '{',
        _ => ' ',
    }
}

fn expected_close_for_open(open_ch: char) -> char {
    match open_ch {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        _ => ' ',
    }
}

#[derive(Clone, Copy)]
struct OpenBracket {
    ch: char,
    row: i64,
    col: i64,
}

/// Single-pass buffer diagnostics matching `ext_trouble` rules, returning
/// `(capped_sorted_items, total_err_count, total_warn_count)`.
pub fn doc_analyze_diagnostics(
    doc_id: i64,
    major_mode: &str,
    max_items: i64,
) -> (Vec<NativeDiagHit>, i64, i64) {
    let Some(doc) = snapshot_doc_view(doc_id) else {
        return (Vec::new(), 0, 0);
    };
    let cap = resolve_cap(max_items, 1_000);
    let mode = major_mode.to_ascii_lowercase();
    let code_mode = is_code_mode(&mode);
    let line_cmt = mode_line_comment_prefix(&mode);
    let (blk_open, blk_close) = mode_block_comment_delims(&mode);
    let single_quote_str = uses_single_quote_strings(&mode);

    let mut diags: Vec<NativeDiagHit> = Vec::new();
    let mut err_count: i64 = 0;
    let mut warn_count: i64 = 0;
    let mut bracket_stack: Vec<OpenBracket> = Vec::new();
    let mut in_block_cmt = false;

    for (r_usize, line) in doc.iter_lines().enumerate() {
        let r = r_usize as i64;
        let llen = char_len_fast(line) as i64;

        // 1) Unresolved git merge conflict markers
        if let Some(marker) = conflict_marker_kind(line) {
            err_count += 1;
            diags.push(NativeDiagHit {
                row: r,
                col: 0,
                end_col: llen.max(1),
                severity: "ERROR".to_string(),
                source: "git".to_string(),
                message: format!("Unresolved git merge conflict marker '{marker}'"),
            });
            continue;
        }

        // 2) Mixed tab + space leading indentation
        let mut saw_space = false;
        let mut saw_tab = false;
        let mut ws_end: i64 = 0;
        for ch in line.chars() {
            if ch == ' ' {
                saw_space = true;
                ws_end += 1;
            } else if ch == '\t' {
                saw_tab = true;
                ws_end += 1;
            } else {
                break;
            }
        }
        if saw_space && saw_tab {
            warn_count += 1;
            diags.push(NativeDiagHit {
                row: r,
                col: 0,
                end_col: ws_end.max(1),
                severity: "WARN".to_string(),
                source: "style".to_string(),
                message: "Mixed tab and space indentation".to_string(),
            });
        }

        // 3) Trailing whitespace in code buffers
        if code_mode {
            let trimmed_end = line.trim_end();
            let tlen = char_len_fast(trimmed_end) as i64;
            if tlen < llen {
                warn_count += 1;
                diags.push(NativeDiagHit {
                    row: r,
                    col: tlen,
                    end_col: llen,
                    severity: "WARN".to_string(),
                    source: "style".to_string(),
                    message: "Trailing whitespace".to_string(),
                });
            }
        }

        // 4) Balanced delimiter & string literal checks in code modes
        if code_mode && (in_block_cmt || !line.is_empty()) {
            let bytes = line.as_bytes();
            let mut b_idx = 0usize;
            let mut char_col = 0i64;

            while b_idx < bytes.len() {
                if in_block_cmt {
                    if !blk_close.is_empty() && line[b_idx..].starts_with(blk_close) {
                        in_block_cmt = false;
                        b_idx += blk_close.len();
                        char_col += blk_close.chars().count() as i64;
                    } else {
                        let ch = line[b_idx..].chars().next().unwrap_or(' ');
                        b_idx += ch.len_utf8();
                        char_col += 1;
                    }
                    continue;
                }

                if !blk_open.is_empty() && line[b_idx..].starts_with(blk_open) {
                    in_block_cmt = true;
                    b_idx += blk_open.len();
                    char_col += blk_open.chars().count() as i64;
                    continue;
                }

                if !line_cmt.is_empty() && line[b_idx..].starts_with(line_cmt) {
                    break;
                }

                let ch = line[b_idx..].chars().next().unwrap_or(' ');
                if ch == '"' || (single_quote_str && ch == '\'') {
                    let quote = ch;
                    let start_c = char_col;
                    b_idx += quote.len_utf8();
                    char_col += 1;
                    let mut closed = false;
                    while b_idx < bytes.len() {
                        let cur = line[b_idx..].chars().next().unwrap_or(' ');
                        if cur == '\\' {
                            b_idx += 1;
                            char_col += 1;
                            if b_idx < bytes.len() {
                                let esc = line[b_idx..].chars().next().unwrap_or(' ');
                                b_idx += esc.len_utf8();
                                char_col += 1;
                            }
                        } else if cur == quote {
                            closed = true;
                            b_idx += quote.len_utf8();
                            char_col += 1;
                            break;
                        } else {
                            b_idx += cur.len_utf8();
                            char_col += 1;
                        }
                    }
                    if !closed {
                        err_count += 1;
                        diags.push(NativeDiagHit {
                            row: r,
                            col: start_c,
                            end_col: llen.max(start_c + 1),
                            severity: "ERROR".to_string(),
                            source: "syntax".to_string(),
                            message: "Unterminated string literal".to_string(),
                        });
                    }
                    continue;
                }

                // Skip short character literals `'x'` or `'\x'` in Rust/C/Nessa/Java/Go
                if !single_quote_str && ch == '\'' {
                    let after1 = b_idx + 1;
                    if after1 < bytes.len() {
                        let c1 = line[after1..].chars().next().unwrap_or(' ');
                        if c1 == '\\' {
                            let after2 = after1 + 1;
                            if after2 < bytes.len() {
                                let c2 = line[after2..].chars().next().unwrap_or(' ');
                                let after3 = after2 + c2.len_utf8();
                                if after3 < bytes.len() && bytes[after3] == b'\'' {
                                    b_idx = after3 + 1;
                                    char_col += 4;
                                    continue;
                                }
                            }
                        } else {
                            let after2 = after1 + c1.len_utf8();
                            if after2 < bytes.len() && bytes[after2] == b'\'' {
                                b_idx = after2 + 1;
                                char_col += 3;
                                continue;
                            }
                        }
                    }
                }

                if matches!(ch, '(' | '[' | '{') {
                    bracket_stack.push(OpenBracket {
                        ch,
                        row: r,
                        col: char_col,
                    });
                } else if matches!(ch, ')' | ']' | '}') {
                    let want = matching_open_for_close(ch);
                    match bracket_stack.pop() {
                        None => {
                            err_count += 1;
                            diags.push(NativeDiagHit {
                                row: r,
                                col: char_col,
                                end_col: char_col + 1,
                                severity: "ERROR".to_string(),
                                source: "syntax".to_string(),
                                message: format!("Unmatched closing bracket '{ch}'"),
                            });
                        }
                        Some(top) if top.ch != want => {
                            let exp = expected_close_for_open(top.ch);
                            err_count += 1;
                            diags.push(NativeDiagHit {
                                row: r,
                                col: char_col,
                                end_col: char_col + 1,
                                severity: "ERROR".to_string(),
                                source: "syntax".to_string(),
                                message: format!(
                                    "Mismatched bracket: expected '{exp}' for '{}' at line {}, found '{ch}'",
                                    top.ch,
                                    top.row + 1
                                ),
                            });
                        }
                        Some(_) => {}
                    }
                }

                b_idx += ch.len_utf8();
                char_col += 1;
            }
        }
    }

    for open in bracket_stack {
        err_count += 1;
        diags.push(NativeDiagHit {
            row: open.row,
            col: open.col,
            end_col: open.col + 1,
            severity: "ERROR".to_string(),
            source: "syntax".to_string(),
            message: format!("Unclosed bracket '{}'", open.ch),
        });
    }

    diags.sort_by(|a, b| {
        let ra = i32::from(a.severity != "ERROR");
        let rb = i32::from(b.severity != "ERROR");
        a.row
            .cmp(&b.row)
            .then_with(|| a.col.cmp(&b.col))
            .then_with(|| ra.cmp(&rb))
    });
    if diags.len() > cap {
        diags.truncate(cap);
    }

    (diags, err_count, warn_count)
}

const TODO_TAGS: &[&str] = &["TODO", "FIXME", "HACK", "NOTE", "PERF", "WARN", "BUG"];

/// Scan a single line for the leftmost valid boundary-verified TODO/FIXME/etc. tag,
/// even when preceded by a longer identifier like `TODOS`.
pub(crate) fn scan_todo_in_line(line: &str, row: i64, file_path: &str) -> Option<TodoMatch> {
    // Fast-path byte filter: every tag starts with T, F, H, N, P, W, or B
    if !line
        .bytes()
        .any(|b| matches!(b, b'T' | b'F' | b'H' | b'N' | b'P' | b'W' | b'B'))
    {
        return None;
    }

    let mut best: Option<(usize, &'static str)> = None;
    for &tag in TODO_TAGS {
        let mut from_b = 0usize;
        while from_b + tag.len() <= line.len() {
            let Some(rel) = line[from_b..].find(tag) else {
                break;
            };
            let b_pos = from_b + rel;
            if let Some((best_b, _)) = best
                && b_pos >= best_b
            {
                break;
            }
            let before_ok = b_pos == 0
                || !line[..b_pos]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            let after_b = b_pos + tag.len();
            let after_ok = after_b >= line.len()
                || matches!(
                    line[after_b..].chars().next(),
                    Some(':' | '(' | ' ' | '\t' | '!')
                );
            if before_ok && after_ok {
                best = Some((b_pos, tag));
                break;
            }
            from_b = b_pos + 1;
            while from_b < line.len() && !line.is_char_boundary(from_b) {
                from_b += 1;
            }
        }
    }

    let (best_b, tag) = best?;
    let col = byte_to_char_idx(line, best_b) as i64;
    let text = truncate_str_utf8_safe(line[best_b..].trim(), 4_096);
    Some(TodoMatch {
        file_path: file_path.to_string(),
        row,
        col,
        tag: tag.to_string(),
        text,
    })
}

/// 2. Scan `doc_id` for `TODO`/`FIXME`/`BUG`/`HACK`/`WARN`/`NOTE`/`PERF` at valid identifier boundaries.
pub fn doc_scan_todos(doc_id: i64, file_path: &str, max_items: i64) -> Vec<TodoMatch> {
    let Some(doc) = snapshot_doc_view(doc_id) else {
        return Vec::new();
    };
    let cap = resolve_cap(max_items, 500);
    let mut out = Vec::new();
    for (row, line) in doc.iter_lines().enumerate() {
        if let Some(hit) = scan_todo_in_line(line, row as i64, file_path) {
            out.push(hit);
            if out.len() >= cap {
                break;
            }
        }
    }
    out
}

fn extract_ident_after(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    let ident: String = trimmed
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if ident.is_empty() { None } else { Some(ident) }
}

#[inline]
pub(crate) fn is_markdown_mode_name(mode_lower: &str) -> bool {
    matches!(
        mode_lower,
        "markdown" | "md" | "gfm" | "markdown-inline" | "markdown_inline"
    )
}

/// If row 0 opens a YAML frontmatter block (`---`) and a matching closing `---` or `...` line
/// exists later in the document, return the 0-based row index of that closing delimiter.
pub(crate) fn markdown_frontmatter_end_row<'a, I>(mut lines: I) -> Option<usize>
where
    I: Iterator<Item = &'a str>,
{
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    for (offset, line) in lines.enumerate() {
        let row = offset + 1;
        let trimmed = line.trim_end();
        if trimmed == "---" || trimmed == "..." {
            return Some(row);
        }
    }
    None
}

fn strip_blockquote_and_fence_indent(line: &str) -> Option<&str> {
    let mut rest = line.strip_suffix('\r').unwrap_or(line);
    loop {
        let sp = rest.bytes().take_while(|&b| b == b' ').count();
        if sp <= 3 && rest.as_bytes().get(sp) == Some(&b'>') {
            rest = &rest[sp + 1..];
            if matches!(rest.as_bytes().first(), Some(b' ' | b'\t')) {
                rest = &rest[1..];
            }
            continue;
        }
        if sp > 3 || rest.as_bytes().get(sp) == Some(&b'\t') {
            return None;
        }
        return Some(&rest[sp..]);
    }
}

/// Stateful CommonMark / GFM fenced code block tracker supporting `0..=3` leading spaces,
/// `>` blockquote prefixes, `>= 3` `` ` `` or `~` delimiters, and matching closing fences
/// (`close_char == open_char && close_len >= open_len` with trailing whitespace only).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct MarkdownFenceState {
    open: Option<(u8, usize)>,
}

impl MarkdownFenceState {
    pub(crate) fn new() -> Self {
        Self { open: None }
    }

    /// Advance fence state on `line`; returns `true` when `line` is a fence delimiter or inside
    /// an active fenced code block (and therefore excluded from Markdown heading/tag scanning).
    pub(crate) fn step_line(&mut self, line: &str) -> bool {
        if let Some((open_ch, open_len)) = self.open {
            if let Some(after_indent) = strip_blockquote_and_fence_indent(line) {
                let close_len = after_indent.bytes().take_while(|&b| b == open_ch).count();
                if close_len >= open_len
                    && after_indent[close_len..]
                        .bytes()
                        .all(|b| b == b' ' || b == b'\t')
                {
                    self.open = None;
                }
            }
            return true;
        }

        if let Some(after_indent) = strip_blockquote_and_fence_indent(line)
            && let Some(&ch @ (b'`' | b'~')) = after_indent.as_bytes().first()
        {
            let open_len = after_indent.bytes().take_while(|&b| b == ch).count();
            if open_len >= 3 {
                let info = &after_indent[open_len..];
                if ch == b'~' || !info.as_bytes().contains(&b'`') {
                    self.open = Some((ch, open_len));
                    return true;
                }
            }
        }
        false
    }
}

/// Parse a CommonMark ATX heading (`0..=3` spaces, `1..=6` `#`, followed by `' '`, `'\t'`, or EOL),
/// stripping optional closing `#` runs preceded by whitespace.
/// Returns `(indent_cols, level, clean_title)`.
pub(crate) fn parse_markdown_atx_heading(line: &str) -> Option<(i64, i64, String)> {
    let clean = line.strip_suffix('\r').unwrap_or(line);
    let lead_spaces = clean.bytes().take_while(|&b| b == b' ').count();
    if lead_spaces > 3 {
        return None;
    }
    let after_indent = &clean[lead_spaces..];
    let hashes = after_indent.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let raw_rest = match after_indent.as_bytes().get(hashes) {
        None => "",
        Some(b' ' | b'\t') => &after_indent[hashes + 1..],
        Some(_) => return None,
    };
    let mut body = raw_rest.trim();
    if body.ends_with('#') {
        let tail_start = body.bytes().rposition(|b| b != b'#').map_or(0, |p| p + 1);
        if tail_start == 0 {
            body = "";
        } else if matches!(body.as_bytes()[tail_start - 1], b' ' | b'\t') {
            body = body[..tail_start - 1].trim_end();
        }
    }
    Some((lead_spaces as i64, hashes as i64, body.to_string()))
}

fn scan_symbol_in_line(line: &str, mode_lower: &str) -> Option<(String, &'static str, i64, i64)> {
    if is_markdown_mode_name(mode_lower) {
        let (indent_cols, level, title) = parse_markdown_atx_heading(line)?;
        return Some((title, "Heading", indent_cols, level));
    }

    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    let indent_bytes = line.len() - trimmed.len();
    let indent_cols = byte_to_char_idx(line, indent_bytes) as i64;

    if mode_lower == "org" {
        let lvl = trimmed.chars().take_while(|c| *c == '*').count();
        if lvl > 0 && matches!(trimmed.chars().nth(lvl), Some(' ' | '\t')) {
            let title = trimmed[lvl..].trim().to_string();
            if !title.is_empty() {
                return Some((title, "Heading", indent_cols, lvl as i64));
            }
        }
        return None;
    }

    let clean = trimmed
        .strip_prefix("pub(crate) ")
        .or_else(|| trimmed.strip_prefix("pub "))
        .or_else(|| trimmed.strip_prefix("private "))
        .or_else(|| trimmed.strip_prefix("async "))
        .unwrap_or(trimmed);

    const CHECKS: &[(&str, &str)] = &[
        ("fn ", "Function"),
        ("def ", "Function"),
        ("struct ", "Struct"),
        ("class ", "Class"),
        ("enum ", "Enum"),
        ("trait ", "Trait"),
        ("interface ", "Interface"),
        ("impl ", "Impl"),
        ("mod ", "Module"),
        ("const ", "Constant"),
        ("global ", "Variable"),
    ];
    for &(kw, kind) in CHECKS {
        if let Some(rest) = clean.strip_prefix(kw)
            && let Some(name) = extract_ident_after(rest)
        {
            let level = if indent_cols > 0 { 2 } else { 1 };
            return Some((name, kind, indent_cols, level));
        }
    }
    None
}

#[derive(Clone, Copy)]
struct LineBraceInfo {
    opens: i32,
    closes: i32,
    saw_semi_zero: bool,
}

fn scan_line_brace_delta(line: &str, line_cmt: &str, cur_depth: i32) -> LineBraceInfo {
    let bytes = line.as_bytes();
    let mut b_idx = 0usize;
    let mut opens = 0i32;
    let mut closes = 0i32;
    let mut saw_semi_zero = false;
    let mut running = cur_depth;

    while b_idx < bytes.len() {
        if !line_cmt.is_empty() && line[b_idx..].starts_with(line_cmt) {
            break;
        }
        let b = bytes[b_idx];
        if b == b'"' {
            b_idx += 1;
            while b_idx < bytes.len() {
                if bytes[b_idx] == b'\\' {
                    b_idx += 1;
                    if b_idx < bytes.len() {
                        let esc = line[b_idx..].chars().next().unwrap_or(' ');
                        b_idx += esc.len_utf8();
                    }
                } else if bytes[b_idx] == b'"' {
                    b_idx += 1;
                    break;
                } else {
                    let ch = line[b_idx..].chars().next().unwrap_or(' ');
                    b_idx += ch.len_utf8();
                }
            }
            continue;
        }
        if b == b'{' {
            opens += 1;
            running += 1;
            b_idx += 1;
        } else if b == b'}' {
            closes += 1;
            running -= 1;
            b_idx += 1;
        } else if b == b';' {
            if running <= 0 {
                saw_semi_zero = true;
            }
            b_idx += 1;
        } else if b.is_ascii() {
            b_idx += 1;
        } else {
            let ch = line[b_idx..].chars().next().unwrap_or(' ');
            b_idx += ch.len_utf8();
        }
    }

    LineBraceInfo {
        opens,
        closes,
        saw_semi_zero,
    }
}

/// 3. Single-pass symbol outline & span computation (`end_row`) matching `ext_aerial`.
pub fn doc_scan_symbol_spans(
    doc_id: i64,
    major_mode: &str,
    max_items: i64,
) -> Vec<SymbolOutlineSpan> {
    let Some(doc) = snapshot_doc_view(doc_id) else {
        return Vec::new();
    };
    let cap = resolve_cap(max_items, 2_000);
    let mode = major_mode.to_ascii_lowercase();
    let max_row = (doc.total_lines as i64 - 1).max(0);

    if is_markdown_mode_name(&mode) || mode == "org" {
        let is_md = is_markdown_mode_name(&mode);
        let fm_end = if is_md {
            markdown_frontmatter_end_row(doc.iter_lines())
        } else {
            None
        };
        let mut fence = MarkdownFenceState::new();
        let mut syms: Vec<SymbolOutlineSpan> = Vec::new();
        for (idx, line) in doc.iter_lines().enumerate() {
            if is_md {
                if fm_end.is_some_and(|end| idx <= end) {
                    continue;
                }
                if fence.step_line(line) {
                    continue;
                }
            }
            if let Some((name, kind, col, level)) = scan_symbol_in_line(line, &mode) {
                syms.push(SymbolOutlineSpan {
                    name,
                    kind: kind.to_string(),
                    row: idx as i64,
                    col,
                    level,
                    end_row: max_row,
                });
            }
        }
        // Monotonic stack for `next_same_or_outer` in O(S)
        let mut stack: Vec<usize> = Vec::new();
        for i in 0..syms.len() {
            while let Some(&top) = stack.last() {
                if syms[i].level <= syms[top].level {
                    stack.pop();
                    syms[top].end_row = if syms[i].row > syms[top].row {
                        syms[i].row - 1
                    } else {
                        syms[top].row
                    };
                } else {
                    break;
                }
            }
            stack.push(i);
        }
        if syms.len() > cap {
            syms.truncate(cap);
        }
        return syms;
    }

    if mode == "python" {
        let mut syms: Vec<SymbolOutlineSpan> = Vec::new();
        // Active open Python symbols sorted by strictly increasing `col`
        let mut active: Vec<usize> = Vec::new();
        for (idx, line) in doc.iter_lines().enumerate() {
            let r = idx as i64;
            let trimmed = line.trim_start();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                let ind = byte_to_char_idx(line, line.len() - trimmed.len()) as i64;
                while let Some(&top) = active.last() {
                    if ind <= syms[top].col {
                        active.pop();
                    } else {
                        break;
                    }
                }
                for &open_i in &active {
                    syms[open_i].end_row = r;
                }
            }
            if let Some((name, kind, col, level)) = scan_symbol_in_line(line, &mode) {
                let new_idx = syms.len();
                syms.push(SymbolOutlineSpan {
                    name,
                    kind: kind.to_string(),
                    row: r,
                    col,
                    level,
                    end_row: r,
                });
                active.push(new_idx);
            }
        }
        if syms.len() > cap {
            syms.truncate(cap);
        }
        return syms;
    }

    // Brace-based languages (`nessa`, `rust`, `c`, `cpp`, `go`, `js`, `ts`, `java`, etc.)
    let line_cmt = if mode == "nessa" || mode == "lua" {
        "--"
    } else if mode == "toml" || mode == "yaml" || mode == "sh" || mode == "shell" {
        "#"
    } else {
        "//"
    };

    // Single pass over the rope: extract symbols AND compact per-line brace deltas simultaneously!
    let mut syms: Vec<SymbolOutlineSpan> = Vec::new();
    let mut line_braces: Vec<LineBraceInfo> = Vec::with_capacity(doc.total_lines);
    for (idx, line) in doc.iter_lines().enumerate() {
        let r = idx as i64;
        if let Some((name, kind, col, level)) = scan_symbol_in_line(line, &mode) {
            syms.push(SymbolOutlineSpan {
                name,
                kind: kind.to_string(),
                row: r,
                col,
                level,
                end_row: r,
            });
        }
        // Compute independent per-line `(opens, closes)` and zero-depth semicolon check on `r`
        line_braces.push(scan_line_brace_delta(line, line_cmt, 0));
    }

    // Monotonic stack to find `next_same_or_outer` row for every symbol in O(S)
    let mut next_outer_row = vec![-1i64; syms.len()];
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..syms.len() {
        while let Some(&top) = stack.last() {
            if syms[i].level <= syms[top].level {
                stack.pop();
                next_outer_row[top] = syms[i].row;
            } else {
                break;
            }
        }
        stack.push(i);
    }

    let inspect_n = syms.len().min(cap);
    for i in 0..inspect_n {
        let sym_row = syms[i].row;
        let next_row = next_outer_row[i];
        let limit_row = if next_row >= 0 {
            (next_row - 1).max(sym_row)
        } else {
            max_row
        } as usize;

        let mut depth = 0i32;
        let mut saw_open = false;
        let mut end_r = sym_row;
        for (r_u, info) in line_braces
            .iter()
            .enumerate()
            .take(limit_row + 1)
            .skip(sym_row as usize)
        {
            let r = r_u as i64;
            if info.opens > 0 {
                saw_open = true;
            }
            let prev_depth = depth;
            depth += info.opens - info.closes;
            if saw_open && depth <= 0 {
                end_r = r;
                break;
            }
            if !saw_open {
                if r == sym_row && info.saw_semi_zero && prev_depth == 0 {
                    end_r = r;
                    break;
                }
                if r > sym_row {
                    if r > sym_row + 6 {
                        end_r = sym_row;
                        break;
                    }
                    let trimmed = doc.line_ref(r_u).trim();
                    if trimmed.is_empty() || (!line_cmt.is_empty() && trimmed.starts_with(line_cmt))
                    {
                        end_r = sym_row;
                        break;
                    }
                }
            }
            end_r = r;
        }
        syms[i].end_row = end_r;
    }

    if syms.len() > cap {
        syms.truncate(cap);
    }
    syms
}

// ---------------------------------------------------------------------------
// 4. Cached `HEAD` Blob + O(changed_window) Rope Diff (`doc_diff_head`)
// ---------------------------------------------------------------------------

type HeadBlobEntry = Option<Arc<Vec<String>>>;
type HeadBlobCacheMap = HashMap<PathBuf, HeadBlobEntry>;

static HEAD_BLOB_CACHE: OnceLock<Mutex<HeadBlobCacheMap>> = OnceLock::new();

fn normalize_cache_path(file_path: &str) -> PathBuf {
    let p = Path::new(file_path);
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn load_or_fetch_head_lines(file_path: &str, reload_head: bool) -> Option<Arc<Vec<String>>> {
    if file_path.is_empty() {
        return None;
    }
    let key = normalize_cache_path(file_path);
    let cache = HEAD_BLOB_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if !reload_head
        && let Some(cached) = cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned()
    {
        return cached;
    }

    let parent_dir = key
        .parent()
        .or_else(|| Path::new(file_path).parent())
        .and_then(Path::to_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(".");
    let fetched = git_head_file_lines(parent_dir, file_path).map(Arc::new);
    cache
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, fetched.clone());
    fetched
}

#[inline]
fn hash_line_fast(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in s.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Fast O(M + N) unique-line hash-anchor diff for large middle windows (> 6,000 lines or product > 400k),
/// refining small gaps with exact `diff_line_hunks`.
fn diff_window_anchored(
    a: &[String],
    b: &[String],
    base_old: usize,
    base_new: usize,
    out: &mut Vec<DiffHunk>,
) {
    if a.is_empty() && b.is_empty() {
        return;
    }
    if a.is_empty() {
        out.push(DiffHunk {
            old_start: base_old,
            old_count: 0,
            new_start: base_new,
            new_count: b.len(),
            kind: "add".to_string(),
        });
        return;
    }
    if b.is_empty() {
        out.push(DiffHunk {
            old_start: base_old,
            old_count: a.len(),
            new_start: base_new,
            new_count: 0,
            kind: "delete".to_string(),
        });
        return;
    }
    if a.len().saturating_mul(b.len()) <= 400_000 {
        for mut h in diff_line_hunks(a, b) {
            h.old_start += base_old;
            h.new_start += base_new;
            out.push(h);
        }
        return;
    }

    // Collect unique non-trivial lines in `a` and `b` (`usize::MAX` marks duplicate)
    let mut a_unique: HashMap<u64, usize> = HashMap::with_capacity(a.len());
    for (i, line) in a.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let h = hash_line_fast(line);
        a_unique
            .entry(h)
            .and_modify(|v| *v = usize::MAX)
            .or_insert(i);
    }
    let mut b_unique: HashMap<u64, usize> = HashMap::with_capacity(b.len());
    for (j, line) in b.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let h = hash_line_fast(line);
        if a_unique.get(&h).is_some_and(|&ai| ai != usize::MAX) {
            b_unique
                .entry(h)
                .and_modify(|v| *v = usize::MAX)
                .or_insert(j);
        }
    }

    // Greedy monotonic anchor chain in O(M)
    let mut anchors: Vec<(usize, usize)> = Vec::new();
    let mut min_bj = 0usize;
    for (i, line) in a.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let h = hash_line_fast(line);
        if a_unique.get(&h) == Some(&i)
            && let Some(&j) = b_unique.get(&h)
            && j != usize::MAX
            && j >= min_bj
            && a[i] == b[j]
        {
            anchors.push((i, j));
            min_bj = j + 1;
        }
    }

    if anchors.is_empty() {
        out.push(DiffHunk {
            old_start: base_old,
            old_count: a.len(),
            new_start: base_new,
            new_count: b.len(),
            kind: "change".to_string(),
        });
        return;
    }

    let mut cur_a = 0usize;
    let mut cur_b = 0usize;
    for (anc_a, anc_b) in anchors {
        if anc_a < cur_a || anc_b < cur_b {
            continue;
        }
        // Trim common suffix before anchor and common prefix after anchor
        let mut ga_end = anc_a;
        let mut gb_end = anc_b;
        while ga_end > cur_a && gb_end > cur_b && a[ga_end - 1] == b[gb_end - 1] {
            ga_end -= 1;
            gb_end -= 1;
        }
        let sub_a = &a[cur_a..ga_end];
        let sub_b = &b[cur_b..gb_end];
        if !sub_a.is_empty() || !sub_b.is_empty() {
            if sub_a.len().saturating_mul(sub_b.len()) <= 400_000 {
                for mut h in diff_line_hunks(sub_a, sub_b) {
                    h.old_start += base_old + cur_a;
                    h.new_start += base_new + cur_b;
                    out.push(h);
                }
            } else {
                let kind = if sub_a.is_empty() {
                    "add"
                } else if sub_b.is_empty() {
                    "delete"
                } else {
                    "change"
                };
                out.push(DiffHunk {
                    old_start: base_old + cur_a,
                    old_count: sub_a.len(),
                    new_start: base_new + cur_b,
                    new_count: sub_b.len(),
                    kind: kind.to_string(),
                });
            }
        }
        cur_a = anc_a + 1;
        cur_b = anc_b + 1;
        while cur_a < a.len() && cur_b < b.len() && a[cur_a] == b[cur_b] {
            cur_a += 1;
            cur_b += 1;
        }
    }

    if cur_a < a.len() || cur_b < b.len() {
        let sub_a = &a[cur_a..];
        let sub_b = &b[cur_b..];
        if sub_a.len().saturating_mul(sub_b.len()) <= 400_000 {
            for mut h in diff_line_hunks(sub_a, sub_b) {
                h.old_start += base_old + cur_a;
                h.new_start += base_new + cur_b;
                out.push(h);
            }
        } else {
            let kind = if sub_a.is_empty() {
                "add"
            } else if sub_b.is_empty() {
                "delete"
            } else {
                "change"
            };
            out.push(DiffHunk {
                old_start: base_old + cur_a,
                old_count: sub_a.len(),
                new_start: base_new + cur_b,
                new_count: sub_b.len(),
                kind: kind.to_string(),
            });
        }
    }
}

fn diff_head_slice_against_rope(
    head_lines: &[String],
    doc: &RopeDoc,
) -> (Vec<DiffHunk>, i64, i64, i64) {
    let old_len = head_lines.len();
    let new_len = doc.total_lines;

    // 1) Strip common prefix chunk-by-chunk / line-by-line without allocating!
    let mut prefix = 0usize;
    'prefix_scan: for chunk in &doc.chunks {
        let rem_old = old_len.saturating_sub(prefix);
        if rem_old == 0 {
            break;
        }
        let check_len = chunk.len().min(rem_old);
        if chunk[..check_len] == head_lines[prefix..prefix + check_len] {
            prefix += check_len;
            if check_len < chunk.len() {
                break;
            }
        } else {
            for line in chunk.iter().take(check_len) {
                if line == &head_lines[prefix] {
                    prefix += 1;
                } else {
                    break 'prefix_scan;
                }
            }
            break;
        }
    }

    if prefix == old_len && prefix == new_len {
        return (Vec::new(), 0, 0, 0);
    }

    // 2) Strip common suffix chunk-by-chunk / line-by-line from the end!
    let mut old_end = old_len;
    let mut new_end = new_len;
    'suffix_scan: for chunk in doc.chunks.iter().rev() {
        if old_end <= prefix || new_end <= prefix {
            break;
        }
        let max_step = chunk.len().min(old_end - prefix).min(new_end - prefix);
        let c_from = chunk.len() - max_step;
        if chunk[c_from..] == head_lines[old_end - max_step..old_end] {
            old_end -= max_step;
            new_end -= max_step;
            if max_step < chunk.len() {
                break;
            }
        } else {
            for idx in (c_from..chunk.len()).rev() {
                if old_end > prefix && new_end > prefix && chunk[idx] == head_lines[old_end - 1] {
                    old_end -= 1;
                    new_end -= 1;
                } else {
                    break 'suffix_scan;
                }
            }
            break;
        }
    }

    let a = &head_lines[prefix..old_end];
    let b: Vec<String> = doc
        .iter_range(prefix, new_end)
        .map(str::to_string)
        .collect();

    let mut hunks = Vec::new();
    if a.len() <= 6_000 && b.len() <= 6_000 && a.len().saturating_mul(b.len()) <= 400_000 {
        for mut h in diff_line_hunks(a, &b) {
            h.old_start += prefix;
            h.new_start += prefix;
            hunks.push(h);
        }
    } else {
        diff_window_anchored(a, &b, prefix, prefix, &mut hunks);
    }

    let mut added: i64 = 0;
    let mut changed: i64 = 0;
    let mut deleted: i64 = 0;
    for h in &hunks {
        match h.kind.as_str() {
            "add" => added += h.new_count as i64,
            "change" => changed += h.new_count.max(h.old_count) as i64,
            "delete" => deleted += h.old_count as i64,
            _ => {}
        }
    }

    if hunks.len() > 4_096 {
        hunks.truncate(4_096);
    }

    (hunks, added, changed, deleted)
}

/// 4. Diff `HEAD` blob against `doc_id` in `O(changed_window)` with cached `HEAD` lines.
pub fn doc_diff_head(
    doc_id: i64,
    file_path: &str,
    reload_head: bool,
) -> Option<(Vec<DiffHunk>, i64, i64, i64)> {
    let doc = snapshot_doc_view(doc_id)?;
    let head_lines = load_or_fetch_head_lines(file_path, reload_head)?;
    Some(diff_head_slice_against_rope(&head_lines, &doc))
}

fn is_ours_marker(line: &str) -> bool {
    line == "<<<<<<<" || line.starts_with("<<<<<<< ")
}

fn is_base_marker(line: &str) -> bool {
    line == "|||||||" || line.starts_with("||||||| ")
}

fn is_sep_marker(line: &str) -> bool {
    line == "=======" || line.starts_with("======= ")
}

fn is_theirs_marker(line: &str) -> bool {
    line == ">>>>>>>" || line.starts_with(">>>>>>> ")
}

fn marker_label(line: &str, fallback: &str) -> String {
    if char_len_fast(line) <= 7 {
        return fallback.to_string();
    }
    let byte_off = char_to_byte_idx(line, 7);
    let rest = line[byte_off..].trim();
    if rest.is_empty() {
        fallback.to_string()
    } else {
        rest.to_string()
    }
}

/// 5. Scan git merge conflict blocks (`<<<<<<<` / optional `|||||||` / `=======` / `>>>>>>>`).
pub fn doc_scan_conflicts(doc_id: i64, max_items: i64) -> Vec<NativeConflictHit> {
    let Some(doc) = snapshot_doc_view(doc_id) else {
        return Vec::new();
    };
    let cap = resolve_cap(max_items, 500);
    let mut blocks = Vec::new();

    let mut cur_start: i64 = -1;
    let mut cur_ours_label = String::new();
    let mut cur_base: i64 = -1;
    let mut cur_sep: i64 = -1;

    for (r_u, line) in doc.iter_lines().enumerate() {
        let r = r_u as i64;
        if is_ours_marker(line) {
            cur_start = r;
            cur_ours_label = marker_label(line, "ours");
            cur_base = -1;
            cur_sep = -1;
        } else if cur_start >= 0 && cur_sep < 0 && cur_base < 0 && is_base_marker(line) {
            cur_base = r;
        } else if cur_start >= 0 && cur_sep < 0 && is_sep_marker(line) {
            cur_sep = r;
        } else if cur_start >= 0 && cur_sep >= 0 && is_theirs_marker(line) {
            blocks.push(NativeConflictHit {
                start_row: cur_start,
                base_row: cur_base,
                sep_row: cur_sep,
                end_row: r,
                ours_label: std::mem::take(&mut cur_ours_label),
                theirs_label: marker_label(line, "theirs"),
            });
            cur_start = -1;
            cur_base = -1;
            cur_sep = -1;
            if blocks.len() >= cap {
                break;
            }
        }
    }

    blocks
}

#[inline]
fn is_ident_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

#[inline]
fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

/// 6. Whole-word identifier occurrences in `[start_row, end_row_inclusive]`.
pub fn doc_find_word_occurrences(
    doc_id: i64,
    word: &str,
    start_row: i64,
    end_row_inclusive: i64,
    max_items: i64,
) -> Vec<(i64, i64, i64)> {
    if word.is_empty() {
        return Vec::new();
    }
    let Some(doc) = snapshot_doc_view(doc_id) else {
        return Vec::new();
    };
    let cap = resolve_cap(max_items, 500);
    let max_valid = doc.total_lines as i64 - 1;
    let lo = start_row.max(0);
    let hi = end_row_inclusive.min(max_valid);
    if hi < lo {
        return Vec::new();
    }
    let wlen_chars = char_len_fast(word) as i64;
    let mut out = Vec::new();

    for (offset, line) in doc.iter_range(lo as usize, hi as usize + 1).enumerate() {
        let r = lo + offset as i64;
        let mut from_b = 0usize;
        while from_b + word.len() <= line.len() {
            let Some(rel) = line[from_b..].find(word) else {
                break;
            };
            let b_pos = from_b + rel;
            let left_ok =
                b_pos == 0 || !line[..b_pos].chars().next_back().is_some_and(is_ident_char);
            let after_b = b_pos + word.len();
            let right_ok =
                after_b >= line.len() || !line[after_b..].chars().next().is_some_and(is_ident_char);
            if left_ok && right_ok {
                let sc = byte_to_char_idx(line, b_pos) as i64;
                out.push((r, sc, sc + wlen_chars));
                if out.len() >= cap {
                    return out;
                }
                from_b = after_b;
            } else {
                let step = line[b_pos..].chars().next().map_or(1, char::len_utf8);
                from_b = b_pos + step;
            }
        }
    }

    out
}

fn collect_words_in_line(
    line: &str,
    row: i64,
    skip_row: i64,
    skip_col: i64,
    seen: &mut HashSet<String>,
    out: &mut Vec<String>,
    cap: usize,
) {
    if line.is_ascii() {
        let bytes = line.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() && out.len() < cap {
            let b = bytes[i];
            if b == b'_' || b.is_ascii_alphabetic() {
                let start = i;
                i += 1;
                while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                if i - start >= 2 && !(row == skip_row && start as i64 == skip_col) {
                    let tok = &line[start..i];
                    if !seen.contains(tok) {
                        let owned = tok.to_string();
                        seen.insert(owned.clone());
                        out.push(owned);
                    }
                }
            } else {
                i += 1;
            }
        }
        return;
    }

    let mut cur_start_b = 0usize;
    let mut cur_start_c = 0i64;
    let mut cur_len_c = 0usize;
    let mut in_word = false;

    for (c_idx, (b_idx, ch)) in line.char_indices().enumerate() {
        if !in_word {
            if is_ident_start(ch) {
                in_word = true;
                cur_start_b = b_idx;
                cur_start_c = c_idx as i64;
                cur_len_c = 1;
            }
        } else if is_ident_char(ch) {
            cur_len_c += 1;
        } else {
            if cur_len_c >= 2 && !(row == skip_row && cur_start_c == skip_col) {
                let tok = &line[cur_start_b..b_idx];
                if !seen.contains(tok) {
                    let owned = tok.to_string();
                    seen.insert(owned.clone());
                    out.push(owned);
                    if out.len() >= cap {
                        return;
                    }
                }
            }
            in_word = false;
        }
    }
    if in_word && cur_len_c >= 2 && out.len() < cap && !(row == skip_row && cur_start_c == skip_col)
    {
        let tok = &line[cur_start_b..];
        if !seen.contains(tok) {
            let owned = tok.to_string();
            seen.insert(owned.clone());
            out.push(owned);
        }
    }
}

/// Collect unique identifier words (`len >= 2`) from `[center_row - radius_rows, center_row + radius_rows]`
/// plus `0..120`, skipping the token starting at `(skip_row, skip_col)`, up to `max_words`.
pub fn doc_collect_words(
    doc_id: i64,
    center_row: i64,
    radius_rows: i64,
    skip_row: i64,
    skip_col: i64,
    max_words: i64,
) -> Vec<String> {
    let Some(doc) = snapshot_doc_view(doc_id) else {
        return Vec::new();
    };
    let cap = resolve_cap(max_words, 500);
    if doc.total_lines == 0 || cap == 0 {
        return Vec::new();
    }

    let rad = radius_rows.max(0);
    let win_lo = (center_row.saturating_sub(rad)).clamp(0, doc.total_lines as i64 - 1) as usize;
    let win_hi_excl = (center_row.saturating_add(rad).saturating_add(1))
        .clamp(0, doc.total_lines as i64) as usize;

    let mut seen: HashSet<String> = HashSet::with_capacity(cap.min(1024));
    let mut out: Vec<String> = Vec::with_capacity(cap.min(512));

    // Scan window around cursor first so local context gets priority
    for (offset, line) in doc.iter_range(win_lo, win_hi_excl).enumerate() {
        let r = (win_lo + offset) as i64;
        collect_words_in_line(line, r, skip_row, skip_col, &mut seen, &mut out, cap);
        if out.len() >= cap {
            return out;
        }
    }

    // Also include top-of-file imports / declarations (`0..120`) not covered by `[win_lo, win_hi_excl)`
    let top_excl = 120usize.min(doc.total_lines);
    let head_part1_end = top_excl.min(win_lo);
    if head_part1_end > 0 {
        for (r_u, line) in doc.iter_range(0, head_part1_end).enumerate() {
            collect_words_in_line(
                line, r_u as i64, skip_row, skip_col, &mut seen, &mut out, cap,
            );
            if out.len() >= cap {
                return out;
            }
        }
    }
    if win_hi_excl < top_excl {
        for (offset, line) in doc.iter_range(win_hi_excl, top_excl).enumerate() {
            let r = (win_hi_excl + offset) as i64;
            collect_words_in_line(line, r, skip_row, skip_col, &mut seen, &mut out, cap);
            if out.len() >= cap {
                return out;
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn temp_test_path(label: &str) -> PathBuf {
        let n = TEST_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "lava_doc_store_{label}_{}_{n}.tmp",
            std::process::id()
        ))
    }

    #[test]
    fn rope_250k_lines_open_save_roundtrip_and_cow_snapshots() {
        let path_lf = temp_test_path("250k_lf");
        let path_crlf = temp_test_path("250k_crlf");

        let mut raw_lf = String::with_capacity(250_000 * 32);
        for i in 0..250_000 {
            raw_lf.push_str(&format!(
                "static int sqlite3_row_{i}(int x) {{ return x + {i}; }}\n"
            ));
        }
        fs::write(&path_lf, raw_lf.as_bytes()).expect("write 250k LF file");

        let (doc_id, line_count, has_crlf) = doc_open_file(path_lf.to_str().unwrap()).unwrap();
        assert_eq!(line_count, 250_000);
        assert!(!has_crlf);
        assert_eq!(doc_version(doc_id), 1);
        assert_eq!(
            doc_line_at(doc_id, 0),
            "static int sqlite3_row_0(int x) { return x + 0; }"
        );
        assert_eq!(
            doc_line_at(doc_id, 249_999),
            "static int sqlite3_row_249999(int x) { return x + 249999; }"
        );

        // Take CoW snapshot before editing
        let snap_orig = doc_snapshot_take(doc_id);
        assert!(snap_orig > 0);
        assert!(doc_snapshot_matches(doc_id, snap_orig));

        // Mutate in the middle of 250,000 lines
        let end_pos = doc_insert_text(doc_id, 125_000, 7, "/*mid*/ ");
        assert_eq!(end_pos, (125_000, 15));
        assert_eq!(doc_version(doc_id), 2);
        assert!(!doc_snapshot_matches(doc_id, snap_orig));

        let snap_edited = doc_snapshot_take(doc_id);
        assert!(doc_snapshot_matches(doc_id, snap_edited));

        // Restore original CoW snapshot (undo)
        let v3 = doc_snapshot_restore(doc_id, snap_orig);
        assert_eq!(v3, 3);
        assert!(doc_snapshot_matches(doc_id, snap_orig));
        assert!(!doc_snapshot_matches(doc_id, snap_edited));

        // Byte-for-byte LF save roundtrip
        let written_lf = doc_save_file(doc_id, path_lf.to_str().unwrap(), false).unwrap();
        assert_eq!(written_lf as usize, raw_lf.len());
        let saved_lf = fs::read(&path_lf).unwrap();
        assert_eq!(saved_lf, raw_lf.as_bytes());

        // Redo snapshot restore
        let v4 = doc_snapshot_restore(doc_id, snap_edited);
        assert_eq!(v4, 4);
        assert!(doc_snapshot_matches(doc_id, snap_edited));

        // CRLF roundtrip
        let doc_crlf = doc_new(vec![
            "alpha".to_string(),
            "beta".to_string(),
            "gamma".to_string(),
        ]);
        let written_crlf = doc_save_file(doc_crlf, path_crlf.to_str().unwrap(), true).unwrap();
        assert_eq!(written_crlf, 20);
        let (reopened_crlf, lc_crlf, det_crlf) =
            doc_open_file(path_crlf.to_str().unwrap()).unwrap();
        assert_eq!(lc_crlf, 3);
        assert!(det_crlf);
        assert_eq!(
            doc_slice_lines(reopened_crlf, 0, 10),
            vec!["alpha", "beta", "gamma"]
        );

        // Free cleans up both doc and owned snapshots
        doc_free(doc_id);
        assert_eq!(doc_line_count(doc_id), 0);
        assert!(!doc_snapshot_matches(doc_id, snap_orig));
        doc_free(doc_crlf);
        doc_free(reopened_crlf);
        let _ = fs::remove_file(&path_lf);
        let _ = fs::remove_file(&path_crlf);
    }

    #[test]
    fn multiline_insert_delete_span_utf8_and_512kb_cap() {
        let doc = doc_new(vec![
            "héllo 世界!".to_string(),
            "second line".to_string(),
            "third line".to_string(),
        ]);
        assert_eq!(doc_line_char_len(doc, 0), 9);

        // Insert multiline at Unicode char column 6 (before "世界!")
        let pos = doc_insert_text(doc, 0, 6, "αβ\nγδ ");
        assert_eq!(pos, (1, 3));
        assert_eq!(doc_line_count(doc), 4);
        assert_eq!(doc_line_at(doc, 0), "héllo αβ");
        assert_eq!(doc_line_at(doc, 1), "γδ 世界!");

        // Extract and delete span across lines
        let extracted = doc_extract_span(doc, 0, 6, 1, 3);
        assert_eq!(extracted, "αβ\nγδ ");
        let deleted = doc_delete_span(doc, 0, 6, 1, 3);
        assert_eq!(deleted, "αβ\nγδ ");
        assert_eq!(doc_line_count(doc), 3);
        assert_eq!(doc_line_at(doc, 0), "héllo 世界!");

        // Pathologically large >600KB single line is truncated across VM boundary but preserved in Rust
        let huge = "🦀".repeat(150_000); // 600,000 bytes
        doc_set_line(doc, 1, huge.clone());
        let capped_line = doc_line_at(doc, 1);
        assert!(capped_line.len() <= MAX_NESSA_STR_BYTES);
        assert!(capped_line.len() > MAX_NESSA_STR_BYTES - 8);
        let full = doc_full_text(doc, false).unwrap();
        assert!(full.len() > 600_000);

        // Deleting all lines leaves 1 empty line invariant
        let del_all = doc_delete_lines(doc, 0, 100);
        assert!(del_all.len() <= MAX_NESSA_STR_BYTES);
        assert_eq!(doc_line_count(doc), 1);
        assert_eq!(doc_line_at(doc, 0), "");

        doc_free(doc);
    }

    #[test]
    fn search_forward_backward_and_bounded_bracket_match() {
        let doc = doc_new(vec![
            "fn outer(a: i64) {".to_string(),
            "    let FooBar = [1, (2 + 3)];".to_string(),
            "    let café = FooBar + 1;".to_string(),
            "}".to_string(),
        ]);

        // Case-insensitive ASCII fast path + wrap
        assert_eq!(
            doc_find_forward(doc, "foobar", 0, -1, 0, 3, true, false),
            Some((1, 8))
        );
        assert_eq!(
            doc_find_forward(doc, "foobar", 1, 8, 0, 3, true, false),
            Some((2, 15))
        );
        assert_eq!(
            doc_find_forward(doc, "foobar", 2, 15, 0, 3, true, false),
            None
        );
        assert_eq!(
            doc_find_forward(doc, "foobar", 2, 15, 0, 3, true, true),
            Some((1, 8))
        );

        // Case-sensitive vs Unicode search & backward search
        assert_eq!(
            doc_find_forward(doc, "CAFÉ", 0, -1, 0, 3, true, false),
            Some((2, 8))
        );
        assert_eq!(
            doc_find_backward(doc, "FooBar", 2, 15, 0, 3, false, false),
            Some((1, 8))
        );
        assert_eq!(
            doc_find_backward(doc, "FooBar", 1, 8, 0, 3, false, true),
            Some((2, 15))
        );

        // Bracket matching + max_scan_lines bound
        assert_eq!(
            doc_find_matching_bracket(doc, 0, 17, 0, 3, 100),
            Some((3, 0))
        );
        assert_eq!(
            doc_find_matching_bracket(doc, 3, 0, 0, 3, 100),
            Some((0, 17))
        );
        // Bounded scan of 1 line from row 0 cannot reach row 3
        assert_eq!(doc_find_matching_bracket(doc, 0, 17, 0, 3, 1), None);

        doc_free(doc);
    }

    #[test]
    fn ide_accelerators_diagnostics_todos_symbols_conflicts_words_and_diff() {
        let doc = doc_new(vec![
            "pub struct Widget {".to_string(),
            " \tlet mixed = 1;   ".to_string(),
            "    let TODOS = 0; // TODO: fix after TODOS identifier".to_string(),
            "}".to_string(),
            "<<<<<<< HEAD".to_string(),
            "fn ours() {".to_string(),
            "||||||| base".to_string(),
            "=======".to_string(),
            "fn theirs() {".to_string(),
            ">>>>>>> feature/branch".to_string(),
            "fn broken() { let s = \"unterminated".to_string(),
        ]);

        // 1) Diagnostics
        let (diags, errs, warns) = doc_analyze_diagnostics(doc, "rust", 100);
        assert!(errs >= 5); // 4 conflict markers + unclosed `{`s + unterminated string
        assert_eq!(warns, 2); // mixed indent + trailing whitespace on row 1
        assert!(diags.iter().any(|d| d.source == "git" && d.row == 4));
        assert!(
            diags
                .iter()
                .any(|d| d.message == "Mixed tab and space indentation" && d.row == 1)
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message == "Trailing whitespace" && d.row == 1)
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message == "Unterminated string literal" && d.row == 10)
        );

        // 2) Todos: finds `TODO` even after `TODOS` earlier on the same line!
        let todos = doc_scan_todos(doc, "src/lib.rs", 50);
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].row, 2);
        assert_eq!(todos[0].tag, "TODO");
        assert_eq!(todos[0].col, 22);
        assert_eq!(todos[0].text, "TODO: fix after TODOS identifier");

        // 3) Conflicts
        let conflicts = doc_scan_conflicts(doc, 50);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts[0],
            NativeConflictHit {
                start_row: 4,
                base_row: 6,
                sep_row: 7,
                end_row: 9,
                ours_label: "HEAD".to_string(),
                theirs_label: "feature/branch".to_string(),
            }
        );

        // 4) Symbol spans (brace, markdown, python)
        let sym_doc = doc_new(vec![
            "pub struct Pair {".to_string(),
            "    pub a: i64,".to_string(),
            "}".to_string(),
            "".to_string(),
            "pub fn compute(x: i64) -> i64 {".to_string(),
            "    x + 1".to_string(),
            "}".to_string(),
        ]);
        let spans = doc_scan_symbol_spans(sym_doc, "nessa", 50);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].name, "Pair");
        assert_eq!((spans[0].row, spans[0].end_row), (0, 2));
        assert_eq!(spans[1].name, "compute");
        assert_eq!((spans[1].row, spans[1].end_row), (4, 6));

        let py_doc = doc_new(vec![
            "class Worker:".to_string(),
            "    def run(self):".to_string(),
            "        return 42".to_string(),
            "".to_string(),
            "def helper():".to_string(),
            "    pass".to_string(),
        ]);
        let py_spans = doc_scan_symbol_spans(py_doc, "python", 50);
        assert_eq!(py_spans.len(), 3);
        assert_eq!((py_spans[0].row, py_spans[0].end_row), (0, 2));
        assert_eq!((py_spans[1].row, py_spans[1].end_row), (1, 2));
        assert_eq!((py_spans[2].row, py_spans[2].end_row), (4, 5));

        // 5) Whole-word occurrences & completion word collection
        let occ = doc_find_word_occurrences(doc, "TODOS", 0, 10, 50);
        assert_eq!(occ, vec![(2, 8, 13), (2, 38, 43)]);
        let words = doc_collect_words(doc, 2, 2, 2, 8, 50);
        assert!(words.contains(&"Widget".to_string()));
        assert!(words.contains(&"identifier".to_string()));

        // 6) O(changed_window) rope diff on 100k lines with 1 edited line
        let head: Vec<String> = (0..100_000).map(|i| format!("line_{i}")).collect();
        let big_doc = doc_new(head.clone());
        doc_set_line(big_doc, 50_000, "line_50000_modified".to_string());
        let big_view = snapshot_doc_view(big_doc).unwrap();
        let (hunks, added, changed, deleted) = diff_head_slice_against_rope(&head, &big_view);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 50_000);
        assert_eq!(hunks[0].new_start, 50_000);
        assert_eq!(hunks[0].kind, "change");
        assert_eq!((added, changed, deleted), (0, 1, 0));

        // 7) Native incremental search preview (`doc_search_preview`)
        let (hit_total, prevs) = doc_search_preview(big_doc, "line_5000", 0, 99_999, false, 12);
        assert_eq!(hit_total, 11); // `line_5000` + `line_50000_modified`..`line_50009`
        assert_eq!(prevs.len(), 11);
        assert_eq!(prevs[0], (5000, 0, "line_5000".to_string()));

        doc_free(doc);
        doc_free(sym_doc);
        doc_free(py_doc);
        doc_free(big_doc);
    }
}
