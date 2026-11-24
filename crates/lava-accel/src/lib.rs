//! High-performance native editor/IDE acceleration primitives for Lava (`lava-accel`).
//!
//! Covers domain-specific hot paths that belong in an editor runtime library rather than
//! the language's general standard library:
//! - Nucleo/Fzf-grade batch fuzzy matching & ranking
//! - `.gitignore`-aware workspace file indexing, live grep, and atomic multi-file replace
//! - Line-level LCS/Myers diff hunks for live gutter signs & interactive staging
//! - Git repository status, `HEAD` blob retrieval, and single-line blame porcelain parsing
//! - Multi-language symbol outline & `TODO`/`FIXME` comment extraction

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

pub mod doc_store;
pub mod treesitter;
pub use doc_store::{NativeConflictHit, NativeDiagHit, SymbolOutlineSpan};
pub use treesitter::{
    TsHighlightSpan, ts_highlight_doc_rows, ts_highlight_snippet, ts_inspect_doc_node,
    ts_status_info,
};

// ---------------------------------------------------------------------------
// 1. Batch Fuzzy Matcher & Ranker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub index: usize,
    pub score: i64,
}

fn is_word_boundary_byte(b: u8) -> bool {
    matches!(
        b,
        b'/' | b'\\' | b'_' | b'-' | b'.' | b' ' | b':' | b'*' | b'(' | b')'
    )
}

pub fn fuzzy_score_one(query_trimmed: &str, query_lower: &str, candidate: &str) -> Option<i64> {
    if query_trimmed.is_empty() {
        return Some(100);
    }
    let cand_lower = candidate.to_lowercase();
    if cand_lower == query_lower {
        return Some(20_000);
    }
    if cand_lower.starts_with(query_lower) {
        return Some(15_000 - (cand_lower.len() as i64 - query_lower.len() as i64));
    }

    // File basename exact/prefix bonus
    if let Some(slash_pos) = cand_lower.rfind('/') {
        let base = &cand_lower[slash_pos + 1..];
        if base == query_lower {
            return Some(18_000 - slash_pos as i64);
        }
        if base.starts_with(query_lower) {
            return Some(
                13_500 - (base.len() as i64 - query_lower.len() as i64) * 2 - slash_pos as i64,
            );
        }
    }

    // Contiguous substring match at a word boundary
    if let Some(pos) = cand_lower.find(query_lower)
        && (pos == 0 || is_word_boundary_byte(cand_lower.as_bytes()[pos - 1]))
    {
        return Some(11_500 - (pos as i64) * 4 - (cand_lower.len() as i64));
    }

    // Multi-token space-separated orderless match
    if query_lower.contains(' ') {
        let mut all_ok = true;
        let mut count = 0i64;
        for tok in query_lower.split_whitespace() {
            count += 1;
            if !cand_lower.contains(tok) {
                all_ok = false;
                break;
            }
        }
        if all_ok && count > 0 {
            return Some(7_500 - cand_lower.len() as i64);
        }
    }

    // Subsequence match with word-initial acronym, camelCase, word-boundary, and adjacency bonuses
    let q_chars: Vec<char> = query_lower.chars().collect();
    let c_orig: Vec<char> = candidate.chars().collect();
    let c_low: Vec<char> = cand_lower.chars().collect();
    if q_chars.len() > c_low.len() {
        return None;
    }

    let mut qi = 0usize;
    let mut prev_match: Option<usize> = None;
    let mut bonus = 0i64;
    let mut gaps = 0i64;
    let mut boundary_hits = 0usize;

    for ci in 0..c_low.len() {
        if qi < q_chars.len() && c_low[ci] == q_chars[qi] {
            let at_boundary = ci == 0
                || matches!(
                    c_orig[ci - 1],
                    '/' | '\\' | '_' | '-' | '.' | ' ' | ':' | '*'
                )
                || (c_orig[ci].is_uppercase() && c_orig[ci - 1].is_lowercase());
            if at_boundary {
                bonus += 55;
                boundary_hits += 1;
            }
            if let Some(pm) = prev_match {
                if pm + 1 == ci {
                    bonus += 30;
                } else {
                    gaps += (ci - pm - 1) as i64;
                }
            }
            prev_match = Some(ci);
            qi += 1;
        }
    }

    // Also try boundary-preferring acronym scan (`ff` -> `find-file`)
    let mut b_qi = 0usize;
    let mut b_gaps = 0i64;
    let mut b_prev: Option<usize> = None;
    for ci in 0..c_low.len() {
        let at_boundary = ci == 0
            || matches!(
                c_orig[ci - 1],
                '/' | '\\' | '_' | '-' | '.' | ' ' | ':' | '*'
            )
            || (c_orig[ci].is_uppercase() && c_orig[ci - 1].is_lowercase());
        if b_qi < q_chars.len() && at_boundary && c_low[ci] == q_chars[b_qi] {
            if let Some(pm) = b_prev {
                b_gaps += (ci - pm - 1) as i64;
            }
            b_prev = Some(ci);
            b_qi += 1;
        }
    }
    if b_qi == q_chars.len() {
        return Some(10_500 - b_gaps * 2 - c_low.len() as i64);
    }

    // Non-boundary contiguous substring match
    if let Some(pos) = cand_lower.find(query_lower) {
        return Some(8_500 - (pos as i64) * 4 - (cand_lower.len() as i64));
    }

    if qi == q_chars.len() {
        let acro_bonus = if boundary_hits == q_chars.len() {
            4_000
        } else {
            0
        };
        Some(3_000 + acro_bonus + bonus - gaps * 3 - c_low.len() as i64)
    } else {
        None
    }
}

pub fn fuzzy_rank_batch(query: &str, candidates: &[String], limit: usize) -> Vec<FuzzyMatch> {
    let trimmed = query.trim();
    let q_low = trimmed.to_lowercase();
    let mut hits: Vec<FuzzyMatch> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, cand)| {
            fuzzy_score_one(trimmed, &q_low, cand).map(|score| FuzzyMatch { index, score })
        })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.index.cmp(&b.index)));
    if limit > 0 && hits.len() > limit {
        hits.truncate(limit);
    }
    hits
}

/// Emacs Orderless + multi-tier match result with exact 0-based character highlight positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderlessMatch {
    pub index: usize,
    pub score: i64,
    pub positions: Vec<i64>,
}

#[derive(Debug, Clone)]
enum ParsedOrderlessToken {
    Negation(String),
    Prefix { raw: String, chars: Vec<char> },
    Suffix { chars: Vec<char> },
    Literal { raw: String, chars: Vec<char> },
    Fuzzy { raw: String, chars: Vec<char> },
}

fn parse_orderless_tokens(query: &str) -> Vec<ParsedOrderlessToken> {
    query
        .split_whitespace()
        .filter(|p| !p.is_empty())
        .map(|raw_tok| {
            let tlen = raw_tok.chars().count();
            if raw_tok.starts_with('!') && tlen > 1 {
                let neg: String = raw_tok.chars().skip(1).collect::<String>().to_lowercase();
                ParsedOrderlessToken::Negation(neg)
            } else if raw_tok.starts_with('^') && tlen > 1 {
                let anch: String = raw_tok.chars().skip(1).collect::<String>().to_lowercase();
                let chars = anch.chars().collect();
                ParsedOrderlessToken::Prefix { raw: anch, chars }
            } else if raw_tok.ends_with('$') && tlen > 1 {
                let anch: String = raw_tok
                    .chars()
                    .take(tlen - 1)
                    .collect::<String>()
                    .to_lowercase();
                ParsedOrderlessToken::Suffix {
                    chars: anch.chars().collect(),
                }
            } else if raw_tok.starts_with('=') && tlen > 1 {
                let lit: String = raw_tok.chars().skip(1).collect::<String>().to_lowercase();
                let chars = lit.chars().collect();
                ParsedOrderlessToken::Literal { raw: lit, chars }
            } else {
                let q = raw_tok.to_lowercase();
                let chars = q.chars().collect();
                ParsedOrderlessToken::Fuzzy { raw: q, chars }
            }
        })
        .collect()
}

fn is_orderless_boundary_sep(ch: char) -> bool {
    matches!(ch, '-' | '_' | '/' | '.' | ' ' | ':' | '*')
}

fn is_orderless_word_start(c_chars: &[char], idx: usize) -> bool {
    idx == 0 || is_orderless_boundary_sep(c_chars[idx - 1])
}

/// Dynamic-programming O(Q * C) word-boundary prefix alignment matching `score_word_boundary_fuzzy_pos`.
fn score_word_boundary_dp(q: &[char], c: &[char]) -> Option<(i64, Vec<i64>)> {
    let qlen = q.len();
    let clen = c.len();
    if qlen == 0 {
        return Some((0, Vec::new()));
    }
    if qlen > clen {
        return None;
    }

    let stride = (clen + 1) * 2;
    // `dp[(qi * stride) + ci * 2 + in_run] = best_bonus` (`-1` = unreachable)
    let mut dp = vec![-1i64; (qlen + 1) * stride];
    // `choice[(qi * stride) + ci * 2 + in_run] = matched_char_pos`
    let mut choice = vec![0usize; (qlen + 1) * stride];

    for ci in 0..=clen {
        dp[qlen * stride + ci * 2] = 0;
        dp[qlen * stride + ci * 2 + 1] = 0;
    }

    for qi in (0..qlen).rev() {
        let qch = q[qi];
        let next_row = (qi + 1) * stride;
        let cur_row = qi * stride;

        // Precompute suffix maximum of jumping to a word-start boundary `j >= ci`:
        // `jump_val[ci]` = max over `j in ci..clen` where `is_word_start(j) && c[j] == qch`
        let mut best_jump_sc = -1i64;
        let mut best_jump_pos = 0usize;
        for ci in (0..clen).rev() {
            if is_orderless_word_start(c, ci) && c[ci] == qch {
                let sub = dp[next_row + (ci + 1) * 2 + 1];
                if sub >= 0 {
                    let bonus = (if ci == 0 { 45 } else { 30 }) + sub;
                    // Strict `>` when scanning right-to-left prefers leftmost tie, matching forward loop `if bonus > best`!
                    if bonus >= best_jump_sc {
                        best_jump_sc = bonus;
                        best_jump_pos = ci;
                    }
                }
            }

            // State `in_run = 0`
            dp[cur_row + ci * 2] = best_jump_sc;
            choice[cur_row + ci * 2] = best_jump_pos;

            // State `in_run = 1`
            let mut best = best_jump_sc;
            let mut best_pos = best_jump_pos;
            if c[ci] == qch {
                let sub = dp[next_row + (ci + 1) * 2 + 1];
                if sub >= 0 {
                    let cont = 25 + sub;
                    if cont >= best {
                        best = cont;
                        best_pos = ci;
                    }
                }
            }
            dp[cur_row + ci * 2 + 1] = best;
            choice[cur_row + ci * 2 + 1] = best_pos;
        }
    }

    let root_sc = dp[0];
    if root_sc < 0 {
        return None;
    }

    let mut positions = Vec::with_capacity(qlen);
    let mut ci = 0usize;
    let mut in_run = 0usize;
    for qi in 0..qlen {
        let mpos = choice[qi * stride + ci * 2 + in_run];
        positions.push(mpos as i64);
        ci = mpos + 1;
        in_run = 1;
    }
    Some((root_sc, positions))
}

fn find_char_subslice(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn score_subsequence_chars(q: &[char], c: &[char]) -> Option<(i64, Vec<i64>)> {
    if q.len() > c.len() {
        return None;
    }
    let mut qi = 0usize;
    let mut prev_match: Option<usize> = None;
    let mut bonus = 0i64;
    let mut gaps = 0i64;
    let mut positions = Vec::with_capacity(q.len());

    for (ci, &ch) in c.iter().enumerate() {
        if qi < q.len() && ch == q[qi] {
            if is_orderless_word_start(c, ci) {
                bonus += 35;
            }
            if let Some(pm) = prev_match {
                if pm + 1 == ci {
                    bonus += 20;
                } else {
                    gaps += (ci - pm - 1) as i64;
                }
            }
            positions.push(ci as i64);
            prev_match = Some(ci);
            qi += 1;
        }
    }
    if qi < q.len() {
        return None;
    }
    Some((2_000 + bonus - gaps * 3 - c.len() as i64, positions))
}

fn score_parsed_orderless_token(
    tok: &ParsedOrderlessToken,
    c_lower: &str,
    c_chars: &[char],
) -> Option<(bool, i64, Vec<i64>)> {
    let clen = c_chars.len() as i64;
    match tok {
        ParsedOrderlessToken::Negation(neg) => {
            if c_lower.contains(neg.as_str()) {
                None
            } else {
                Some((true, 200, Vec::new()))
            }
        }
        ParsedOrderlessToken::Prefix { raw, chars } => {
            let alen = chars.len() as i64;
            if c_chars.starts_with(chars) {
                return Some((false, 8_500 - (clen - alen), (0..alen).collect()));
            }
            let mut slash_pat = Vec::with_capacity(chars.len() + 1);
            slash_pat.push('/');
            slash_pat.extend_from_slice(chars);
            if let Some(pos) = find_char_subslice(c_chars, &slash_pat) {
                let h0 = (pos + 1) as i64;
                let _ = raw;
                return Some((false, 7_600 - (clen - alen), (h0..h0 + alen).collect()));
            }
            None
        }
        ParsedOrderlessToken::Suffix { chars } => {
            let alen = chars.len() as i64;
            if c_chars.ends_with(chars) {
                let s0 = clen - alen;
                Some((false, 7_800 - (clen - alen), (s0..clen).collect()))
            } else {
                None
            }
        }
        ParsedOrderlessToken::Literal { raw, chars } => {
            let llen = chars.len() as i64;
            if c_lower == raw {
                return Some((false, 10_000, (0..clen).collect()));
            }
            if let Some(pos) = find_char_subslice(c_chars, chars) {
                let h0 = pos as i64;
                return Some((
                    false,
                    5_000 - h0 * 5 - (clen - llen),
                    (h0..h0 + llen).collect(),
                ));
            }
            None
        }
        ParsedOrderlessToken::Fuzzy { raw, chars } => {
            let qlen = chars.len() as i64;
            if c_lower == raw {
                return Some((false, 10_000, (0..clen).collect()));
            }
            if c_chars.starts_with(chars) {
                return Some((false, 8_000 - (clen - qlen), (0..qlen).collect()));
            }
            if let Some((wb_bonus, wb_pos)) = score_word_boundary_dp(chars, c_chars) {
                return Some((false, 6_000 + wb_bonus - clen, wb_pos));
            }
            if let Some(pos) = find_char_subslice(c_chars, chars) {
                let first_idx = pos as i64;
                return Some((
                    false,
                    4_000 - first_idx * 5 - (clen - qlen),
                    (first_idx..first_idx + qlen).collect(),
                ));
            }
            score_subsequence_chars(chars, c_chars).map(|(sc, pos)| (false, sc, pos))
        }
    }
}

pub fn orderless_score_one(query: &str, candidate: &str) -> Option<(i64, Vec<i64>)> {
    let tokens = parse_orderless_tokens(query);
    if tokens.is_empty() {
        return Some((100, Vec::new()));
    }
    orderless_score_with_tokens(&tokens, candidate)
}

fn orderless_score_with_tokens(
    tokens: &[ParsedOrderlessToken],
    candidate: &str,
) -> Option<(i64, Vec<i64>)> {
    let c_lower = candidate.to_lowercase();
    let c_chars: Vec<char> = c_lower.chars().collect();
    let clen = c_chars.len() as i64;

    if tokens.len() == 1 {
        let (_, sc0, mut pos0) = score_parsed_orderless_token(&tokens[0], &c_lower, &c_chars)?;
        pos0.retain(|&p| p >= 0 && p < clen);
        pos0.sort_unstable();
        pos0.dedup();
        return Some((sc0, pos0));
    }

    let mut pos_score_sum = 0i64;
    let mut pos_tok_count = 0i64;
    let mut combined = Vec::new();
    for tok in tokens {
        let (is_neg, sc, tpos) = score_parsed_orderless_token(tok, &c_lower, &c_chars)?;
        if !is_neg {
            pos_score_sum += sc;
            pos_tok_count += 1;
            combined.extend(tpos);
        }
    }

    combined.retain(|&p| p >= 0 && p < clen);
    combined.sort_unstable();
    combined.dedup();

    let final_score = if pos_tok_count > 0 {
        3_500 + (pos_score_sum / pos_tok_count) / 4 - clen
    } else {
        200 - clen
    };
    Some((final_score, combined))
}

/// Rank `candidates` using Emacs Orderless (`!neg`, `^prefix`, `suffix$`, `=literal`, acronym DP,
/// substring, subsequence), returning `(total_matches, top_matches_up_to_limit)`.
///
/// When `query.trim().is_empty()`, preserves caller input order with `score = 100 - index as i64`
/// so MRU buffers, sorted files, and sorted commands keep their natural default order.
pub fn orderless_rank_batch(
    query: &str,
    candidates: &[String],
    limit: usize,
) -> (usize, Vec<OrderlessMatch>) {
    let tokens = parse_orderless_tokens(query);
    if tokens.is_empty() {
        let total = candidates.len();
        let take_n = if limit > 0 { limit.min(total) } else { total };
        let items = (0..take_n)
            .map(|index| OrderlessMatch {
                index,
                score: 100 - index as i64,
                positions: Vec::new(),
            })
            .collect();
        return (total, items);
    }

    let mut hits: Vec<OrderlessMatch> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, cand)| {
            orderless_score_with_tokens(&tokens, cand).map(|(score, positions)| OrderlessMatch {
                index,
                score,
                positions,
            })
        })
        .collect();

    let total = hits.len();
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| candidates[a.index].cmp(&candidates[b.index]))
            .then_with(|| a.index.cmp(&b.index))
    });
    if limit > 0 && hits.len() > limit {
        hits.truncate(limit);
    }
    (total, hits)
}

// ---------------------------------------------------------------------------
// 2. Gitignore-Aware Workspace File Indexer, Live Grep & Multi-File Replace
// ---------------------------------------------------------------------------

const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".next",
    "dist",
    "build",
    ".cache",
    ".idea",
    ".vscode",
];

const BINARY_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "pdf", "zip", "tar", "gz", "xz", "zst", "7z", "so",
    "dylib", "dll", "exe", "o", "a", "rlib", "rmeta", "nsbc", "pyc", "wasm", "ttf", "otf", "woff",
    "woff2", "mp3", "mp4", "wav",
];

fn is_probably_binary_ext(name: &str) -> bool {
    if let Some(dot) = name.rfind('.') {
        let ext = name[dot + 1..].to_ascii_lowercase();
        BINARY_EXTS.contains(&ext.as_str())
    } else {
        false
    }
}

fn load_simple_gitignore_names(root: &Path) -> HashSet<String> {
    let mut ignored = HashSet::new();
    for d in IGNORED_DIRS {
        ignored.insert((*d).to_string());
    }
    let gi_path = root.join(".gitignore");
    if let Ok(content) = fs::read_to_string(gi_path) {
        for raw_line in content.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                continue;
            }
            let cleaned = line.trim_matches('/');
            if !cleaned.is_empty() && !cleaned.contains('*') && !cleaned.contains('/') {
                ignored.insert(cleaned.to_string());
            }
        }
    }
    ignored
}

pub const MAX_WORKSPACE_FILES: usize = 500_000;
const WORKSPACE_INDEX_TTL: Duration = Duration::from_millis(2_000);

#[derive(Clone)]
struct CachedWorkspaceIndex {
    built_at: Instant,
    stamp: u128,
    files: Arc<Vec<String>>,
    dirs: Arc<Vec<String>>,
}

type WorkspaceIndexMap = HashMap<(PathBuf, bool), CachedWorkspaceIndex>;

static WORKSPACE_INDEX_CACHE: OnceLock<Mutex<WorkspaceIndexMap>> = OnceLock::new();

pub fn invalidate_workspace_cache(_path_hint: &str) {
    if let Some(mtx) = WORKSPACE_INDEX_CACHE.get()
        && let Ok(mut guard) = mtx.lock()
    {
        guard.clear();
    }
    if let Some(gmtx) = SERVER_GIT_STATUS_CACHE.get()
        && let Ok(mut guard) = gmtx.lock()
    {
        guard.clear();
    }
}

fn compute_workspace_quick_stamp(root: &Path) -> u128 {
    let mut stamp: u128 = 0xcbf2_9ce4_8422_2325;
    if let Ok(meta) = fs::metadata(root) {
        stamp = stamp
            .wrapping_mul(1099511628211)
            .wrapping_add(meta.len() as u128);
        if let Ok(mt) = meta.modified()
            && let Ok(dur) = mt.duration_since(UNIX_EPOCH)
        {
            stamp = stamp
                .wrapping_mul(1099511628211)
                .wrapping_add(dur.as_nanos());
        }
    }
    if let Ok(rd) = fs::read_dir(root) {
        for entry in rd.flatten().take(96) {
            if let Ok(m) = entry.metadata() {
                stamp ^= (m.len() as u128).rotate_left(11);
                if let Ok(mt) = m.modified()
                    && let Ok(dur) = mt.duration_since(UNIX_EPOCH)
                {
                    stamp ^= dur.as_nanos().rotate_left(7);
                }
            }
        }
    }
    stamp
}

fn is_ignored_rel_component(rel: &str, include_hidden: bool) -> bool {
    for seg in rel.split('/') {
        if seg.is_empty() {
            continue;
        }
        if IGNORED_DIRS.contains(&seg) {
            return true;
        }
        if !include_hidden && seg.starts_with('.') {
            return true;
        }
    }
    false
}

fn try_rg_workspace_files(root: &Path, cap: usize, include_hidden: bool) -> Option<Vec<String>> {
    let mut cmd = Command::new("rg");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.arg("--files")
        .arg("--color=never")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if include_hidden {
        cmd.arg("--hidden")
            .arg("-g")
            .arg("!.git")
            .arg("-g")
            .arg("!target")
            .arg("-g")
            .arg("!node_modules");
    }
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let reader = BufReader::with_capacity(64 * 1024, stdout);
    let mut out = Vec::new();
    for line_res in reader.lines() {
        let Ok(raw) = line_res else {
            break;
        };
        let norm = raw.trim().trim_start_matches("./").replace('\\', "/");
        if norm.is_empty() {
            continue;
        }
        if is_ignored_rel_component(&norm, include_hidden) || is_probably_binary_ext(&norm) {
            continue;
        }
        out.push(norm);
        if out.len() >= cap {
            let _ = child.kill();
            let _ = child.wait();
            out.sort();
            return Some(out);
        }
    }
    let status = child.wait().ok()?;
    if !status.success() && out.is_empty() {
        return None;
    }
    out.sort();
    Some(out)
}

pub fn workspace_files(root: &str, max_files: usize, include_hidden: bool) -> Vec<String> {
    let root_path = Path::new(root);
    if !root_path.exists() {
        return Vec::new();
    }
    let cap = if max_files == 0 {
        MAX_WORKSPACE_FILES
    } else {
        max_files.min(MAX_WORKSPACE_FILES)
    };
    if cap >= 20_000
        && let Some(rg_files) = try_rg_workspace_files(root_path, cap, include_hidden)
    {
        return rg_files;
    }
    let ignored = load_simple_gitignore_names(root_path);
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, String, usize)> =
        vec![(root_path.to_path_buf(), String::new(), 0)];

    while let Some((dir, prefix, depth)) = stack.pop() {
        if depth > 64 || out.len() >= cap {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut subdirs = Vec::new();
        for entry_res in entries {
            let Ok(entry) = entry_res else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if ignored.contains(&name) {
                continue;
            }
            if !include_hidden && name.starts_with('.') {
                continue;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if ft.is_dir() {
                subdirs.push((entry.path(), rel, depth + 1));
            } else if ft.is_file() && !is_probably_binary_ext(&name) {
                out.push(rel);
                if out.len() >= cap {
                    break;
                }
            }
        }
        stack.extend(subdirs);
    }
    out.sort();
    out
}

fn synthesize_workspace_dirs(
    root_path: &Path,
    files: &[String],
    include_hidden: bool,
) -> Vec<String> {
    let mut seen = HashSet::new();
    if let Ok(rd) = fs::read_dir(root_path) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.is_empty() || IGNORED_DIRS.contains(&name.as_str()) {
                continue;
            }
            if !include_hidden && name.starts_with('.') {
                continue;
            }
            if ent.file_type().is_ok_and(|ft| ft.is_dir()) {
                seen.insert(name);
            }
        }
    }
    for rel in files {
        let mut search_from = 0usize;
        while let Some(slash_off) = rel[search_from..].find('/') {
            let end = search_from + slash_off;
            if end > 0 {
                let ancestor = &rel[..end];
                if !seen.contains(ancestor) {
                    seen.insert(ancestor.to_string());
                }
            }
            search_from = end + 1;
        }
    }
    let mut dirs: Vec<String> = seen.into_iter().collect();
    dirs.sort();
    dirs
}

fn load_cached_workspace_index(
    root: &str,
    include_hidden: bool,
) -> (Arc<Vec<String>>, Arc<Vec<String>>) {
    let root_path = Path::new(root);
    if !root_path.exists() {
        return (Arc::new(Vec::new()), Arc::new(Vec::new()));
    }
    let canon = strip_win_verbatim_buf(
        fs::canonicalize(root_path).unwrap_or_else(|_| root_path.to_path_buf()),
    );
    let key = (canon.clone(), include_hidden);
    let stamp = compute_workspace_quick_stamp(&canon);
    let cache = WORKSPACE_INDEX_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    if let Ok(guard) = cache.lock()
        && let Some(entry) = guard.get(&key)
        && now.duration_since(entry.built_at) < WORKSPACE_INDEX_TTL
        && entry.stamp == stamp
    {
        return (Arc::clone(&entry.files), Arc::clone(&entry.dirs));
    }

    let files_vec = workspace_files(
        &canon.to_string_lossy(),
        MAX_WORKSPACE_FILES,
        include_hidden,
    );
    let dirs_vec = synthesize_workspace_dirs(&canon, &files_vec, include_hidden);
    let files_arc = Arc::new(files_vec);
    let dirs_arc = Arc::new(dirs_vec);

    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= 16 {
            guard.clear();
        }
        guard.insert(
            key,
            CachedWorkspaceIndex {
                built_at: Instant::now(),
                stamp,
                files: Arc::clone(&files_arc),
                dirs: Arc::clone(&dirs_arc),
            },
        );
    }
    (files_arc, dirs_arc)
}

#[inline]
fn ascii_subseq_possible(needle_lower_ascii: &[u8], haystack: &[u8]) -> bool {
    if needle_lower_ascii.len() > haystack.len() {
        return false;
    }
    let mut ni = 0usize;
    for &hb in haystack {
        if hb.to_ascii_lowercase() == needle_lower_ascii[ni] {
            ni += 1;
            if ni == needle_lower_ascii.len() {
                return true;
            }
        }
    }
    ni == needle_lower_ascii.len()
}

fn query_uses_orderless(q_trim: &str) -> bool {
    q_trim.contains(' ')
        || q_trim.starts_with('!')
        || q_trim.starts_with('^')
        || q_trim.starts_with('=')
        || q_trim.ends_with('$')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedFileEntry {
    pub rel_path: String,
    pub is_dir: bool,
    pub score: i64,
}

/// Server-side full-workspace file/directory search & ranking:
/// indexes up to `500,000` files in memory (`WORKSPACE_INDEX_CACHE`), scores all candidates
/// using Nucleo-tier fuzzy or Emacs Orderless matching, and returns ONLY
/// `(total_indexed, total_matched, top_entries[..limit])` so only renderable rows cross SSH!
pub fn search_workspace_files_ranked(
    root: &str,
    query: &str,
    include_hidden: bool,
    include_dirs: bool,
    limit: usize,
) -> (usize, usize, Vec<RankedFileEntry>) {
    let (files, dirs) = load_cached_workspace_index(root, include_hidden);
    let total_indexed = files.len() + if include_dirs { dirs.len() } else { 0 };
    let cap = if limit == 0 { 100 } else { limit.min(2_000) };
    let q_trim = query.trim();

    if q_trim.is_empty() {
        let mut out = Vec::with_capacity(cap.min(total_indexed));
        if include_dirs {
            for d in dirs.iter().take(cap) {
                out.push(RankedFileEntry {
                    rel_path: d.clone(),
                    is_dir: true,
                    score: 100,
                });
            }
        }
        if out.len() < cap {
            let rem = cap - out.len();
            for f in files.iter().take(rem) {
                out.push(RankedFileEntry {
                    rel_path: f.clone(),
                    is_dir: false,
                    score: 100,
                });
            }
        }
        return (total_indexed, total_indexed, out);
    }

    if query_uses_orderless(q_trim) {
        let tokens = parse_orderless_tokens(q_trim);
        let mut hits: Vec<RankedFileEntry> = Vec::new();
        if include_dirs {
            for d in dirs.iter() {
                let d_slash = format!("{d}/");
                if let Some((sc, _)) = orderless_score_with_tokens(&tokens, &d_slash) {
                    hits.push(RankedFileEntry {
                        rel_path: d.clone(),
                        is_dir: true,
                        score: sc,
                    });
                }
            }
        }
        for f in files.iter() {
            if let Some((sc, _)) = orderless_score_with_tokens(&tokens, f) {
                hits.push(RankedFileEntry {
                    rel_path: f.clone(),
                    is_dir: false,
                    score: sc,
                });
            }
        }
        let total_matched = hits.len();
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.rel_path.cmp(&b.rel_path))
        });
        if hits.len() > cap {
            hits.truncate(cap);
        }
        return (total_indexed, total_matched, hits);
    }

    let q_lower = q_trim.to_lowercase();
    let ascii_fast = q_lower.is_ascii();
    let q_ascii = q_lower.as_bytes();
    let mut hits: Vec<RankedFileEntry> = Vec::new();

    if include_dirs {
        for d in dirs.iter() {
            if ascii_fast && !ascii_subseq_possible(q_ascii, d.as_bytes()) {
                continue;
            }
            if let Some(sc) = fuzzy_score_one(q_trim, &q_lower, d) {
                hits.push(RankedFileEntry {
                    rel_path: d.clone(),
                    is_dir: true,
                    score: sc,
                });
            }
        }
    }
    for f in files.iter() {
        if ascii_fast && !ascii_subseq_possible(q_ascii, f.as_bytes()) {
            continue;
        }
        if let Some(sc) = fuzzy_score_one(q_trim, &q_lower, f) {
            hits.push(RankedFileEntry {
                rel_path: f.clone(),
                is_dir: false,
                score: sc,
            });
        }
    }
    let total_matched = hits.len();
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    if hits.len() > cap {
        hits.truncate(cap);
    }
    (total_indexed, total_matched, hits)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    pub file_path: String,
    pub row: i64,
    pub col: i64,
    pub line_text: String,
}

fn try_rg_workspace_grep(
    root: &Path,
    query: &str,
    case_sensitive: bool,
    cap: usize,
    file_filter: &str,
) -> Option<(usize, Vec<GrepMatch>)> {
    let mut cmd = Command::new("rg");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.arg("--fixed-strings")
        .arg("--line-number")
        .arg("--column")
        .arg("--no-heading")
        .arg("--color=never");
    if case_sensitive {
        cmd.arg("--case-sensitive");
    } else {
        cmd.arg("--ignore-case");
    }
    cmd.arg("--").arg(query).arg(".").current_dir(root);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let reader = BufReader::with_capacity(64 * 1024, stdout);
    let filter_trim = file_filter.trim().to_lowercase();
    let mut results = Vec::with_capacity(cap.min(256));
    let mut total_matched = 0usize;

    for line_res in reader.lines() {
        let Ok(raw) = line_res else {
            break;
        };
        let mut parts = raw.splitn(4, ':');
        let Some(rel_raw) = parts.next() else {
            continue;
        };
        let Some(row_s) = parts.next() else { continue };
        let Some(col_s) = parts.next() else { continue };
        let Some(txt) = parts.next() else { continue };
        let rel = rel_raw.trim_start_matches("./").replace('\\', "/");
        if !filter_trim.is_empty() && !rel.to_lowercase().contains(&filter_trim) {
            continue;
        }
        let Ok(row_1) = row_s.parse::<i64>() else {
            continue;
        };
        let byte_col = col_s.parse::<usize>().unwrap_or(1).saturating_sub(1);
        let char_col = txt
            .get(..byte_col.min(txt.len()))
            .map_or(0, |p| p.chars().count() as i64);
        total_matched += 1;
        if results.len() < cap {
            let preview = if txt.chars().count() > 240 {
                txt.chars().take(240).collect()
            } else {
                txt.to_string()
            };
            results.push(GrepMatch {
                file_path: rel,
                row: (row_1 - 1).max(0),
                col: char_col,
                line_text: preview,
            });
        } else if total_matched >= cap.saturating_mul(20).max(5_000) {
            let _ = child.kill();
            let _ = child.wait();
            return Some((total_matched, results));
        }
    }
    let status = child.wait().ok()?;
    // `rg` exits 0 on matches, 1 on zero matches, 2 on error.
    if matches!(status.code(), Some(0 | 1)) || !results.is_empty() {
        Some((total_matched, results))
    } else {
        None
    }
}

pub fn workspace_grep_counted(
    root: &str,
    query: &str,
    case_sensitive: bool,
    max_results: usize,
    file_filter: &str,
) -> (usize, Vec<GrepMatch>) {
    if query.is_empty() {
        return (0, Vec::new());
    }
    let cap = if max_results == 0 {
        500
    } else {
        max_results.min(5_000)
    };
    let root_path = Path::new(root);
    if !root_path.exists() {
        return (0, Vec::new());
    }
    if let Some(rg_res) = try_rg_workspace_grep(root_path, query, case_sensitive, cap, file_filter)
    {
        return rg_res;
    }

    let (files, _) = load_cached_workspace_index(root, false);
    let needle_low = if case_sensitive {
        String::new()
    } else {
        query.to_lowercase()
    };
    let filter_trim = file_filter.trim().to_lowercase();
    let mut results = Vec::new();
    let mut total_matched = 0usize;

    for rel in files.iter() {
        if !filter_trim.is_empty() && !rel.to_lowercase().contains(&filter_trim) {
            continue;
        }
        let full = root_path.join(rel);
        let Ok(bytes) = fs::read(&full) else {
            continue;
        };
        if bytes.len() > 2 * 1024 * 1024 || bytes.iter().take(512).any(|&b| b == 0) {
            continue;
        }
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        for (line_idx, line) in content.lines().enumerate() {
            let byte_hit = if case_sensitive {
                line.find(query)
            } else {
                line.to_lowercase().find(&needle_low)
            };
            if let Some(byte_pos) = byte_hit {
                total_matched += 1;
                if results.len() < cap {
                    let char_col = line
                        .get(..byte_pos)
                        .map_or(0, |prefix| prefix.chars().count() as i64);
                    let preview = if line.chars().count() > 240 {
                        line.chars().take(240).collect()
                    } else {
                        line.to_string()
                    };
                    results.push(GrepMatch {
                        file_path: rel.clone(),
                        row: line_idx as i64,
                        col: char_col,
                        line_text: preview,
                    });
                } else if total_matched >= cap.saturating_mul(10).max(2_000) {
                    return (total_matched, results);
                }
            }
        }
    }
    (total_matched, results)
}

pub fn workspace_grep(
    root: &str,
    query: &str,
    case_sensitive: bool,
    max_results: usize,
    file_filter: &str,
) -> Vec<GrepMatch> {
    workspace_grep_counted(root, query, case_sensitive, max_results, file_filter).1
}

pub fn workspace_replace(
    root: &str,
    query: &str,
    replacement: &str,
    case_sensitive: bool,
    file_filter: &str,
) -> Result<(usize, usize), String> {
    if query.is_empty() {
        return Err("search query cannot be empty".to_string());
    }
    let files = workspace_files(root, MAX_WORKSPACE_FILES, false);
    let root_path = Path::new(root);
    let filter_trim = file_filter.trim().to_lowercase();
    let mut files_changed = 0usize;
    let mut total_replacements = 0usize;

    for rel in files {
        if !filter_trim.is_empty() && !rel.to_lowercase().contains(&filter_trim) {
            continue;
        }
        let full = root_path.join(&rel);
        let Ok(bytes) = fs::read(&full) else {
            continue;
        };
        if bytes.len() > 2 * 1024 * 1024 || bytes.iter().take(512).any(|&b| b == 0) {
            continue;
        }
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        let (updated, count) = replace_str_counted(&content, query, replacement, case_sensitive);
        if count > 0 {
            fs::write(&full, updated)
                .map_err(|e| format!("failed to write {}: {e}", full.display()))?;
            files_changed += 1;
            total_replacements += count;
        }
    }
    if files_changed > 0 {
        invalidate_workspace_cache(root);
    }
    Ok((files_changed, total_replacements))
}

fn replace_str_counted(
    haystack: &str,
    needle: &str,
    replacement: &str,
    case_sensitive: bool,
) -> (String, usize) {
    if case_sensitive {
        let count = haystack.matches(needle).count();
        if count == 0 {
            return (String::new(), 0);
        }
        return (haystack.replace(needle, replacement), count);
    }
    let h_chars: Vec<char> = haystack.chars().collect();
    let n_low: Vec<char> = needle.to_lowercase().chars().collect();
    if n_low.is_empty() || n_low.len() > h_chars.len() {
        return (String::new(), 0);
    }
    let h_low: Vec<char> = h_chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let mut out = String::with_capacity(haystack.len());
    let mut count = 0usize;
    let mut i = 0usize;
    while i < h_chars.len() {
        if i + n_low.len() <= h_low.len() && h_low[i..i + n_low.len()] == n_low[..] {
            out.push_str(replacement);
            count += 1;
            i += n_low.len();
        } else {
            out.push(h_chars[i]);
            i += 1;
        }
    }
    (out, count)
}

// ---------------------------------------------------------------------------
// 3. Fast Line Diff & Hunk Engine (Prefix/Suffix Trim + Windowed LCS)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    /// `"add"`, `"change"`, or `"delete"`
    pub kind: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffOp {
    Equal,
    Delete,
    Insert,
}

pub fn diff_line_hunks(old_lines: &[String], new_lines: &[String]) -> Vec<DiffHunk> {
    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    if prefix == old_lines.len() && prefix == new_lines.len() {
        return Vec::new();
    }

    let mut old_end = old_lines.len();
    let mut new_end = new_lines.len();
    while old_end > prefix && new_end > prefix && old_lines[old_end - 1] == new_lines[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }

    let a = &old_lines[prefix..old_end];
    let b = &new_lines[prefix..new_end];

    // If the middle changed window is very large, emit a single coalesced hunk cleanly in O(1).
    if a.len().saturating_mul(b.len()) > 400_000 {
        let kind = if a.is_empty() {
            "add"
        } else if b.is_empty() {
            "delete"
        } else {
            "change"
        };
        return vec![DiffHunk {
            old_start: prefix,
            old_count: a.len(),
            new_start: prefix,
            new_count: b.len(),
            kind: kind.to_string(),
        }];
    }

    let m = a.len();
    let n = b.len();
    let cols = n + 1;
    let mut dp = vec![0u32; (m + 1) * cols];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i * cols + j] = if a[i] == b[j] {
                dp[(i + 1) * cols + j + 1] + 1
            } else {
                dp[(i + 1) * cols + j].max(dp[i * cols + j + 1])
            };
        }
    }

    let mut ops = Vec::with_capacity(m + n);
    let (mut i, mut j) = (0usize, 0usize);
    while i < m && j < n {
        if a[i] == b[j] {
            ops.push(DiffOp::Equal);
            i += 1;
            j += 1;
        } else if dp[(i + 1) * cols + j] >= dp[i * cols + j + 1] {
            ops.push(DiffOp::Delete);
            i += 1;
        } else {
            ops.push(DiffOp::Insert);
            j += 1;
        }
    }
    while i < m {
        ops.push(DiffOp::Delete);
        i += 1;
    }
    while j < n {
        ops.push(DiffOp::Insert);
        j += 1;
    }

    let mut hunks = Vec::new();
    let mut oi = prefix;
    let mut ni = prefix;
    let mut idx = 0usize;
    while idx < ops.len() {
        if ops[idx] == DiffOp::Equal {
            oi += 1;
            ni += 1;
            idx += 1;
            continue;
        }
        let h_old_start = oi;
        let h_new_start = ni;
        let mut del_cnt = 0usize;
        let mut ins_cnt = 0usize;
        while idx < ops.len() && ops[idx] != DiffOp::Equal {
            match ops[idx] {
                DiffOp::Delete => {
                    del_cnt += 1;
                    oi += 1;
                }
                DiffOp::Insert => {
                    ins_cnt += 1;
                    ni += 1;
                }
                DiffOp::Equal => unreachable!(),
            }
            idx += 1;
        }
        let kind = if del_cnt == 0 {
            "add"
        } else if ins_cnt == 0 {
            "delete"
        } else {
            "change"
        };
        hunks.push(DiffHunk {
            old_start: h_old_start,
            old_count: del_cnt,
            new_start: h_new_start,
            new_count: ins_cnt,
            kind: kind.to_string(),
        });
    }
    hunks
}

// ---------------------------------------------------------------------------
// 4. Native Git Porcelain Bridge
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct GitRepoStatus {
    pub is_repo: bool,
    pub root: String,
    pub branch: String,
    pub staged_count: i64,
    pub modified_count: i64,
    pub untracked_count: i64,
    /// `(index_status, worktree_status, rel_path)`
    pub entries: Vec<(String, String, String)>,
}

fn strip_win_verbatim_buf(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p
    }
}

fn run_git(cwd: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

const NON_REPO_STATUS_TTL: Duration = Duration::from_millis(1_500);
const SERVER_GIT_STATUS_TTL: Duration = Duration::from_millis(2_000);

static NON_REPO_STATUS_CACHE: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
static SERVER_GIT_STATUS_CACHE: OnceLock<Mutex<HashMap<PathBuf, (Instant, GitRepoStatus)>>> =
    OnceLock::new();

fn resolve_git_cwd_dir(cwd_or_path: &str) -> (String, PathBuf) {
    if cwd_or_path.is_empty() {
        let cur = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let canon = strip_win_verbatim_buf(fs::canonicalize(&cur).unwrap_or(cur));
        return (String::new(), canon);
    }
    let p = Path::new(cwd_or_path);
    let dir = if p.is_file() {
        p.parent().unwrap_or_else(|| Path::new("."))
    } else {
        p
    };
    let canon = strip_win_verbatim_buf(fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()));
    let dir_str = dir.to_string_lossy().into_owned();
    (dir_str, canon)
}

pub fn git_status_summary(cwd_or_path: &str) -> GitRepoStatus {
    git_repo_status(cwd_or_path)
}

pub fn git_repo_status(cwd: &str) -> GitRepoStatus {
    let (effective_cwd, cache_key) = resolve_git_cwd_dir(cwd);
    let cache = NON_REPO_STATUS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    if let Some(ts) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        .copied()
        && now.duration_since(ts) < NON_REPO_STATUS_TTL
        && !cache_key.join(".git").exists()
    {
        return GitRepoStatus::default();
    }

    let Some(root_raw) = run_git(&effective_cwd, &["rev-parse", "--show-toplevel"]) else {
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(cache_key, Instant::now());
        return GitRepoStatus::default();
    };
    let root = root_raw.trim().to_string();
    let branch = run_git(&root, &["symbolic-ref", "--short", "HEAD"])
        .or_else(|| run_git(&root, &["rev-parse", "--short", "HEAD"]))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "HEAD".to_string());

    let mut status = GitRepoStatus {
        is_repo: true,
        root: root.clone(),
        branch,
        ..GitRepoStatus::default()
    };

    if let Some(porcelain) = run_git(&root, &["status", "--porcelain=v1"]) {
        for line in porcelain.lines() {
            if line.len() < 3 {
                continue;
            }
            let x = line[0..1].to_string();
            let y = line[1..2].to_string();
            let raw_rel = line[3..].trim();
            let rel = if (x == "R" || x == "C")
                && let Some((_, new_path)) = raw_rel.split_once(" -> ")
            {
                new_path.trim().to_string()
            } else {
                raw_rel.to_string()
            };
            if x == "?" && y == "?" {
                status.untracked_count += 1;
            } else {
                if x != " " && x != "?" {
                    status.staged_count += 1;
                }
                if y != " " && y != "?" {
                    status.modified_count += 1;
                }
            }
            status.entries.push((x, y, rel));
        }
    }
    status
}

pub fn git_repo_status_cached(cwd: &str) -> GitRepoStatus {
    let (_, cache_key) = resolve_git_cwd_dir(cwd);
    let cache = SERVER_GIT_STATUS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    if let Ok(guard) = cache.lock()
        && let Some((ts, st)) = guard.get(&cache_key)
        && now.duration_since(*ts) < SERVER_GIT_STATUS_TTL
    {
        return st.clone();
    }
    let fresh = git_repo_status(cwd);
    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= 32 {
            guard.clear();
        }
        guard.insert(cache_key, (Instant::now(), fresh.clone()));
    }
    fresh
}

pub fn git_file_diff_hunks(
    cwd: &str,
    file_path: &str,
    override_lines: Option<&[String]>,
) -> (bool, Vec<DiffHunk>) {
    let Some(head_lines) = git_head_file_lines(cwd, file_path) else {
        return (false, Vec::new());
    };
    if let Some(cur) = override_lines {
        return (true, diff_line_hunks(&head_lines, cur));
    }
    let full = if Path::new(file_path).is_absolute() {
        PathBuf::from(file_path)
    } else {
        Path::new(cwd).join(file_path)
    };
    let Ok(raw) = fs::read_to_string(&full) else {
        return (true, Vec::new());
    };
    let body = raw.strip_suffix('\n').unwrap_or(&raw);
    let disk_lines: Vec<String> = if body.is_empty() && raw.is_empty() {
        vec![String::new()]
    } else {
        body.split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
            .collect()
    };
    (true, diff_line_hunks(&head_lines, &disk_lines))
}

pub fn git_head_file_lines(cwd: &str, file_path: &str) -> Option<Vec<String>> {
    let root_raw = run_git(cwd, &["rev-parse", "--show-toplevel"])?;
    let root = Path::new(root_raw.trim());
    let full = if Path::new(file_path).is_absolute() {
        PathBuf::from(file_path)
    } else {
        Path::new(cwd).join(file_path)
    };
    let rel_str = match full.strip_prefix(root) {
        Ok(p) => p.to_string_lossy().replace('\\', "/"),
        Err(_) => {
            let cf = fs::canonicalize(&full).ok()?;
            let cr = fs::canonicalize(root).ok()?;
            cf.strip_prefix(cr)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/")
        }
    };
    let spec = format!("HEAD:{rel_str}");
    let blob = run_git(root_raw.trim(), &["show", &spec])?;
    let body = blob.strip_suffix('\n').unwrap_or(&blob);
    let lines: Vec<String> = body
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    Some(lines)
}

#[derive(Debug, Clone)]
pub struct GitBlameInfo {
    pub commit_short: String,
    pub author: String,
    pub date_ymd: String,
    pub summary: String,
}

pub fn git_blame_line(cwd: &str, file_path: &str, row_0based: usize) -> Option<GitBlameInfo> {
    let line_1 = row_0based + 1;
    let range_arg = format!("{line_1},{line_1}");
    let porcelain = run_git(
        cwd,
        &["blame", "-L", &range_arg, "--porcelain", "--", file_path],
    )?;
    let mut lines = porcelain.lines();
    let header = lines.next()?;
    let commit_full = header.split_whitespace().next()?;
    let commit_short: String = commit_full.chars().take(7).collect();
    let mut author = String::from("You");
    let mut author_time: i64 = 0;
    let mut summary = String::new();
    for l in lines {
        if let Some(rest) = l.strip_prefix("author ") {
            author = rest.trim().to_string();
        } else if let Some(rest) = l.strip_prefix("author-time ") {
            author_time = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = l.strip_prefix("summary ") {
            summary = rest.trim().to_string();
        }
    }
    if commit_full.chars().all(|c| c == '0') {
        author = "Uncommitted".to_string();
        summary = "Not committed yet".to_string();
    }
    let date_ymd = format_days_ymd(author_time);
    Some(GitBlameInfo {
        commit_short,
        author,
        date_ymd,
        summary,
    })
}

fn format_days_ymd(unix_secs: i64) -> String {
    if unix_secs <= 0 {
        return "working-tree".to_string();
    }
    // Howard Hinnant civil-from-days algorithm (UTC)
    let z = unix_secs.div_euclid(86_400) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}")
}

// ---------------------------------------------------------------------------
// 5. Multi-Language Symbol Outline & TODO Scanner
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolEntry {
    pub name: String,
    pub kind: String,
    pub row: i64,
    pub col: i64,
    pub level: i64,
}

fn extract_ident_after(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    let ident: String = trimmed
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if ident.is_empty() { None } else { Some(ident) }
}

pub fn scan_symbols(lines: &[String], major_mode: &str) -> Vec<SymbolEntry> {
    let mode = major_mode.to_ascii_lowercase();
    let mut out = Vec::new();

    if doc_store::is_markdown_mode_name(&mode) {
        let fm_end = doc_store::markdown_frontmatter_end_row(lines.iter().map(String::as_str));
        let mut fence = doc_store::MarkdownFenceState::new();
        for (idx, line) in lines.iter().enumerate() {
            if fm_end.is_some_and(|end| idx <= end) {
                continue;
            }
            if fence.step_line(line) {
                continue;
            }
            if let Some((indent_cols, level, title)) = doc_store::parse_markdown_atx_heading(line) {
                out.push(SymbolEntry {
                    name: title,
                    kind: "Heading".to_string(),
                    row: idx as i64,
                    col: indent_cols,
                    level,
                });
            }
        }
        return out;
    }

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let indent_cols = (line.chars().count() - trimmed.chars().count()) as i64;
        if trimmed.is_empty() {
            continue;
        }
        if mode == "org" {
            let lvl = trimmed.chars().take_while(|c| *c == '*').count();
            if lvl > 0 && matches!(trimmed.chars().nth(lvl), Some(' ' | '\t')) {
                let title = trimmed[lvl..].trim().to_string();
                if !title.is_empty() {
                    out.push(SymbolEntry {
                        name: title,
                        kind: "Heading".to_string(),
                        row: idx as i64,
                        col: indent_cols,
                        level: lvl as i64,
                    });
                }
            }
            continue;
        }

        let clean = trimmed
            .strip_prefix("pub(crate) ")
            .or_else(|| trimmed.strip_prefix("pub "))
            .or_else(|| trimmed.strip_prefix("private "))
            .or_else(|| trimmed.strip_prefix("async "))
            .unwrap_or(trimmed);

        let checks: &[(&str, &str)] = &[
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
        for (kw, kind) in checks {
            if let Some(rest) = clean.strip_prefix(kw)
                && let Some(name) = extract_ident_after(rest)
            {
                out.push(SymbolEntry {
                    name,
                    kind: (*kind).to_string(),
                    row: idx as i64,
                    col: indent_cols,
                    level: if indent_cols > 0 { 2 } else { 1 },
                });
                break;
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoMatch {
    pub file_path: String,
    pub row: i64,
    pub col: i64,
    pub tag: String,
    pub text: String,
}

pub fn scan_buffer_todos(lines: &[String], file_path: &str) -> Vec<TodoMatch> {
    let mut out = Vec::new();
    for (row, line) in lines.iter().enumerate() {
        if let Some(hit) = doc_store::scan_todo_in_line(line, row as i64, file_path) {
            out.push(hit);
        }
    }
    out
}

pub fn scan_workspace_todos(root: &str, max_results: usize) -> Vec<TodoMatch> {
    let cap = if max_results == 0 {
        300
    } else {
        max_results.min(2_000)
    };
    let root_path = Path::new(root);
    if !root_path.exists() {
        return Vec::new();
    }
    // Fast path via `rg` across the entire workspace when ripgrep is available.
    if let Some(rg_hits) = try_rg_workspace_todos(root_path, cap) {
        return rg_hits;
    }
    let files = workspace_files(root, MAX_WORKSPACE_FILES, false);
    let mut out = Vec::new();
    for rel in files {
        let full = root_path.join(&rel);
        let Ok(bytes) = fs::read(&full) else {
            continue;
        };
        if bytes.len() > 1024 * 1024 || bytes.iter().take(512).any(|&b| b == 0) {
            continue;
        }
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        let lines: Vec<String> = content.lines().map(str::to_string).collect();
        for hit in scan_buffer_todos(&lines, &rel) {
            out.push(hit);
            if out.len() >= cap {
                return out;
            }
        }
    }
    out
}

fn try_rg_workspace_todos(root: &Path, cap: usize) -> Option<Vec<TodoMatch>> {
    let mut cmd = Command::new("rg");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never")
        .arg(r"\b(TODO|FIXME|HACK|WARN|WARNING|PERF|NOTE|BUG)\b")
        .arg(".")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let reader = BufReader::with_capacity(64 * 1024, stdout);
    let mut out = Vec::new();
    for line_res in reader.lines() {
        let Ok(raw) = line_res else { break };
        let mut parts = raw.splitn(3, ':');
        let Some(rel_raw) = parts.next() else {
            continue;
        };
        let Some(row_s) = parts.next() else { continue };
        let Some(txt) = parts.next() else { continue };
        let Ok(row_1) = row_s.parse::<i64>() else {
            continue;
        };
        let rel = rel_raw.trim_start_matches("./").replace('\\', "/");
        if let Some(hit) = doc_store::scan_todo_in_line(txt, (row_1 - 1).max(0), &rel) {
            out.push(hit);
            if out.len() >= cap {
                let _ = child.kill();
                let _ = child.wait();
                return Some(out);
            }
        }
    }
    let status = child.wait().ok()?;
    if matches!(status.code(), Some(0 | 1)) || !out.is_empty() {
        Some(out)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 5b. Windowed File Preview & Single-RPC Dirvish / Tree Directory Snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct PreviewWindow {
    pub lines: Vec<String>,
    pub start_row: i64,
    pub total_lines: i64,
    pub is_binary: bool,
    pub truncated: bool,
    pub total_size: i64,
}

/// Stream only a bounded `[start_row .. start_row + max_lines]` slice from `path` so remote
/// Telescope / Dirvish previews never read or transmit whole files over SSH!
pub fn read_file_preview_window(
    path: &str,
    start_row: usize,
    max_lines: usize,
    max_bytes: usize,
) -> Result<PreviewWindow, String> {
    let target = Path::new(path);
    let meta = fs::metadata(target).map_err(|e| format!("cannot stat '{path}': {e}"))?;
    if meta.is_dir() {
        return Err(format!("path is a directory, not a file: {path}"));
    }
    let total_size = meta.len() as i64;
    let cap_lines = max_lines.clamp(5, 2_000);
    let cap_bytes = max_bytes.clamp(512, 262_144);

    let mut sample_file = File::open(target).map_err(|e| format!("cannot open '{path}': {e}"))?;
    let mut sample = [0u8; 4096];
    let sample_n = sample_file
        .read(&mut sample)
        .map_err(|e| format!("cannot read '{path}': {e}"))?;
    if sample[..sample_n].contains(&0) {
        return Ok(PreviewWindow {
            lines: Vec::new(),
            start_row: 0,
            total_lines: 0,
            is_binary: true,
            truncated: true,
            total_size,
        });
    }
    drop(sample_file);

    let file = File::open(target).map_err(|e| format!("cannot open '{path}': {e}"))?;
    let mut reader = BufReader::with_capacity(32 * 1024, file);
    let mut raw_buf = Vec::with_capacity(256);
    let mut cur_row = 0usize;
    let mut window_bytes = 0usize;
    let mut scanned_bytes = 0usize;
    let max_scan_bytes = 32 * 1024 * 1024usize;
    let mut lines = Vec::with_capacity(cap_lines.min(128));
    let mut truncated = false;

    loop {
        raw_buf.clear();
        let n = reader
            .read_until(b'\n', &mut raw_buf)
            .map_err(|e| format!("preview read error on '{path}': {e}"))?;
        if n == 0 {
            break;
        }
        scanned_bytes += n;
        if cur_row < start_row {
            cur_row += 1;
            if scanned_bytes >= max_scan_bytes {
                truncated = true;
                break;
            }
            continue;
        }
        if lines.len() >= cap_lines || window_bytes >= cap_bytes {
            truncated = true;
            break;
        }
        if raw_buf.contains(&0) {
            return Ok(PreviewWindow {
                lines: Vec::new(),
                start_row: 0,
                total_lines: 0,
                is_binary: true,
                truncated: true,
                total_size,
            });
        }
        let Ok(s) = std::str::from_utf8(&raw_buf) else {
            return Ok(PreviewWindow {
                lines: Vec::new(),
                start_row: 0,
                total_lines: 0,
                is_binary: true,
                truncated: true,
                total_size,
            });
        };
        let clean = s.trim_end_matches(['\r', '\n']);
        let clipped: String = if clean.chars().count() > 400 {
            clean.chars().take(400).collect()
        } else {
            clean.to_string()
        };
        window_bytes += clipped.len() + 1;
        lines.push(clipped);
        cur_row += 1;
    }

    if lines.is_empty() && start_row == 0 {
        lines.push(String::new());
    }

    Ok(PreviewWindow {
        start_row: start_row as i64,
        total_lines: cur_row as i64,
        lines,
        is_binary: false,
        truncated,
        total_size,
    })
}

#[derive(Debug, Clone)]
pub struct DirSnapshotParentEntry {
    pub name: String,
    pub full_path: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct DirSnapshotEntry {
    pub name: String,
    pub full_path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub is_exec: bool,
    pub size: i64,
    pub modified_secs: i64,
    pub perms: String,
    pub symlink_target: String,
    pub git_badge: String,
}

#[derive(Debug, Clone, Default)]
pub struct DirSnapshotPreview {
    /// `"none"`, `"dir"`, `"text"`, `"binary"`, or `"unreadable"`
    pub kind: String,
    pub target_path: String,
    pub target_name: String,
    pub total_count: i64,
    pub total_size: i64,
    pub lines: Vec<String>,
    pub child_entries: Vec<(String, bool, bool, i64)>,
}

#[derive(Debug, Clone, Default)]
pub struct DirSnapshotResult {
    pub resolved_path: String,
    pub parent_path: String,
    pub parent_entries: Vec<DirSnapshotParentEntry>,
    pub entries: Vec<DirSnapshotEntry>,
    pub git_is_repo: bool,
    pub git_root: String,
    pub git_branch: String,
    pub git_staged: i64,
    pub git_modified: i64,
    pub git_untracked: i64,
    pub preview: DirSnapshotPreview,
}

fn format_entry_perms(meta: &fs::Metadata, is_dir: bool, is_symlink: bool) -> (bool, String) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        let is_exec = !is_dir && (mode & 0o111) != 0;
        let kind_ch = if is_symlink {
            'l'
        } else if is_dir {
            'd'
        } else {
            '-'
        };
        let bit = |mask: u32, ch: char| if (mode & mask) != 0 { ch } else { '-' };
        let s = format!(
            "{kind_ch}{}{}{}{}{}{}{}{}{}",
            bit(0o400, 'r'),
            bit(0o200, 'w'),
            bit(0o100, 'x'),
            bit(0o040, 'r'),
            bit(0o020, 'w'),
            bit(0o010, 'x'),
            bit(0o004, 'r'),
            bit(0o002, 'w'),
            bit(0o001, 'x'),
        );
        (is_exec, s)
    }
    #[cfg(not(unix))]
    {
        let ro = meta.permissions().readonly();
        let s = if is_symlink {
            "lrwxrwxrwx"
        } else if is_dir {
            if ro { "dr-xr-xr-x" } else { "drwxr-xr-x" }
        } else if ro {
            "-r--r--r--"
        } else {
            "-rw-r--r--"
        };
        (false, s.to_string())
    }
}

fn git_badge_char(x: &str, y: &str) -> &'static str {
    if x == "?" || y == "?" {
        "?"
    } else if x == "A" {
        "A"
    } else if x == "D" || y == "D" {
        "D"
    } else if x == "R" {
        "R"
    } else if x == "M" || y == "M" || x != " " || y != " " {
        "M"
    } else {
        " "
    }
}

fn entry_ext_lower(name: &str) -> String {
    match name.rfind('.') {
        Some(idx) if idx > 0 && idx + 1 < name.len() => name[idx + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Build a complete, render-ready Dirvish / Neotree directory snapshot on `lava-server` in a
/// single pass: parent listing + filtered/sorted directory entries (with server-attached Git
/// badges, permissions, symlink targets, and sizes) + Git repo summary + focused item preview!
#[allow(clippy::too_many_arguments)]
pub fn dir_snapshot(
    raw_dir: &str,
    show_hidden: bool,
    filter_query: &str,
    sort_by: &str,
    sort_reverse: bool,
    focus_name: &str,
    preferred_row: usize,
    include_parent: bool,
    include_preview: bool,
) -> Result<DirSnapshotResult, String> {
    let dir_p = Path::new(raw_dir);
    if !dir_p.exists() {
        return Err(format!("directory does not exist: {raw_dir}"));
    }
    if !dir_p.is_dir() {
        return Err(format!("path is not a directory: {raw_dir}"));
    }
    let resolved =
        strip_win_verbatim_buf(fs::canonicalize(dir_p).unwrap_or_else(|_| dir_p.to_path_buf()));
    let resolved_str = resolved.to_string_lossy().replace('\\', "/");

    // 1. Cached Git status + per-entry badge map restricted to `resolved` (zero full-repo wire dump!).
    let git_st = git_repo_status_cached(&resolved_str);
    let mut direct_badges: HashMap<String, String> = HashMap::new();
    if git_st.is_repo && !git_st.root.is_empty() {
        let git_root_norm = git_st.root.replace('\\', "/");
        let dir_rel_prefix = if resolved_str == git_root_norm {
            String::new()
        } else if let Some(rest) = resolved_str.strip_prefix(&format!("{git_root_norm}/")) {
            format!("{rest}/")
        } else {
            "\0__outside__".to_string()
        };
        if !dir_rel_prefix.starts_with('\0') {
            for (x, y, rel) in &git_st.entries {
                let b = git_badge_char(x, y);
                if b == " " {
                    continue;
                }
                let rel_norm = rel.replace('\\', "/");
                let Some(sub) = (if dir_rel_prefix.is_empty() {
                    Some(rel_norm.as_str())
                } else {
                    rel_norm.strip_prefix(&dir_rel_prefix)
                }) else {
                    continue;
                };
                let child_name = sub.split('/').next().unwrap_or("");
                if child_name.is_empty() {
                    continue;
                }
                match direct_badges.get(child_name).map(String::as_str) {
                    None => {
                        direct_badges.insert(child_name.to_string(), b.to_string());
                    }
                    Some(prev) if prev != "M" && b == "M" => {
                        direct_badges.insert(child_name.to_string(), "M".to_string());
                    }
                    _ => {}
                }
            }
        }
    }

    // 2. Scan current directory entries.
    let rd = fs::read_dir(&resolved)
        .map_err(|e| format!("cannot read directory '{resolved_str}': {e}"))?;
    let filt_low = filter_query.trim().to_lowercase();
    let mut entries = Vec::new();
    for item_res in rd {
        let Ok(de) = item_res else { continue };
        let name = de.file_name().to_string_lossy().into_owned();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        if !filt_low.is_empty() && !name.to_lowercase().contains(&filt_low) {
            continue;
        }
        let symlink_meta = fs::symlink_metadata(de.path());
        let is_symlink = symlink_meta
            .as_ref()
            .is_ok_and(|m| m.file_type().is_symlink());
        let symlink_target = if is_symlink {
            fs::read_link(de.path())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let target_meta = fs::metadata(de.path()).or(symlink_meta);
        let (is_dir, size, modified_secs, is_exec, perms) = match target_meta {
            Ok(ref m) => {
                let d = m.is_dir();
                let sz = if d { 0 } else { m.len() as i64 };
                let mt = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as i64);
                let (ex, pm) = format_entry_perms(m, d, is_symlink);
                (d, sz, mt, ex, pm)
            }
            Err(_) => (false, 0, 0, false, "-rw-r--r--".to_string()),
        };
        let full_child = if resolved_str == "/" {
            format!("/{name}")
        } else {
            format!("{resolved_str}/{name}")
        };
        let git_badge = direct_badges
            .get(&name)
            .cloned()
            .unwrap_or_else(|| " ".to_string());
        entries.push(DirSnapshotEntry {
            name,
            full_path: full_child,
            is_dir,
            is_symlink,
            is_exec,
            size,
            modified_secs,
            perms,
            symlink_target,
            git_badge,
        });
    }

    entries.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            return b.is_dir.cmp(&a.is_dir);
        }
        let ord = match sort_by {
            "size" => a
                .size
                .cmp(&b.size)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            "ext" => entry_ext_lower(&a.name)
                .cmp(&entry_ext_lower(&b.name))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            _ => a
                .name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name)),
        };
        if sort_reverse { ord.reverse() } else { ord }
    });

    // 3. Optional parent directory entries (compact for Dirvish left Miller column).
    let (parent_path, parent_entries) = if include_parent {
        if let Some(par) = resolved.parent()
            && par != resolved
        {
            let par_str = par.to_string_lossy().replace('\\', "/");
            let mut p_items = Vec::new();
            if let Ok(prd) = fs::read_dir(par) {
                for pe_res in prd.flatten() {
                    let pname = pe_res.file_name().to_string_lossy().into_owned();
                    if pname.is_empty() || pname == "." || pname == ".." {
                        continue;
                    }
                    if !show_hidden && pname.starts_with('.') {
                        continue;
                    }
                    let pis_dir = fs::metadata(pe_res.path()).is_ok_and(|m| m.is_dir());
                    let pfull = if par_str == "/" {
                        format!("/{pname}")
                    } else {
                        format!("{par_str}/{pname}")
                    };
                    p_items.push(DirSnapshotParentEntry {
                        name: pname,
                        full_path: pfull,
                        is_dir: pis_dir,
                    });
                }
            }
            p_items.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            (par_str, p_items)
        } else {
            (String::new(), Vec::new())
        }
    } else {
        (String::new(), Vec::new())
    };

    // 4. Optional initial preview for the focused row so opening Dirvish needs ONLY 1 RTT!
    let preview = if include_preview && !entries.is_empty() {
        let idx = if !focus_name.is_empty()
            && let Some(pos) = entries.iter().position(|e| e.name == focus_name)
        {
            pos
        } else {
            preferred_row.min(entries.len() - 1)
        };
        let focused = &entries[idx];
        build_snapshot_entry_preview(
            &focused.full_path,
            &focused.name,
            focused.is_dir,
            show_hidden,
        )
    } else {
        DirSnapshotPreview {
            kind: "none".to_string(),
            ..DirSnapshotPreview::default()
        }
    };

    Ok(DirSnapshotResult {
        resolved_path: resolved_str,
        parent_path,
        parent_entries,
        entries,
        git_is_repo: git_st.is_repo,
        git_root: git_st.root.replace('\\', "/"),
        git_branch: git_st.branch,
        git_staged: git_st.staged_count,
        git_modified: git_st.modified_count,
        git_untracked: git_st.untracked_count,
        preview,
    })
}

pub fn build_snapshot_entry_preview(
    target_path: &str,
    target_name: &str,
    is_dir: bool,
    show_hidden: bool,
) -> DirSnapshotPreview {
    if is_dir {
        let Ok(rd) = fs::read_dir(target_path) else {
            return DirSnapshotPreview {
                kind: "unreadable".to_string(),
                target_path: target_path.to_string(),
                target_name: target_name.to_string(),
                ..DirSnapshotPreview::default()
            };
        };
        let mut children: Vec<(String, bool, bool, i64)> = Vec::new();
        for ch_res in rd.flatten() {
            let cname = ch_res.file_name().to_string_lossy().into_owned();
            if cname.is_empty() || cname == "." || cname == ".." {
                continue;
            }
            if !show_hidden && cname.starts_with('.') {
                continue;
            }
            let csym =
                fs::symlink_metadata(ch_res.path()).is_ok_and(|m| m.file_type().is_symlink());
            let (cdir, csz) = match fs::metadata(ch_res.path()) {
                Ok(m) => (m.is_dir(), if m.is_dir() { 0 } else { m.len() as i64 }),
                Err(_) => (false, 0),
            };
            children.push((cname, cdir, csym, csz));
        }
        children.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
        });
        let total = children.len() as i64;
        if children.len() > 80 {
            children.truncate(80);
        }
        return DirSnapshotPreview {
            kind: "dir".to_string(),
            target_path: target_path.to_string(),
            target_name: target_name.to_string(),
            total_count: total,
            total_size: 0,
            lines: Vec::new(),
            child_entries: children,
        };
    }

    match read_file_preview_window(target_path, 0, 100, 8_192) {
        Ok(win) if win.is_binary => DirSnapshotPreview {
            kind: "binary".to_string(),
            target_path: target_path.to_string(),
            target_name: target_name.to_string(),
            total_count: 0,
            total_size: win.total_size,
            lines: Vec::new(),
            child_entries: Vec::new(),
        },
        Ok(win) => DirSnapshotPreview {
            kind: "text".to_string(),
            target_path: target_path.to_string(),
            target_name: target_name.to_string(),
            total_count: win.total_lines,
            total_size: win.total_size,
            lines: win.lines,
            child_entries: Vec::new(),
        },
        Err(_) => DirSnapshotPreview {
            kind: "unreadable".to_string(),
            target_path: target_path.to_string(),
            target_name: target_name.to_string(),
            ..DirSnapshotPreview::default()
        },
    }
}

// ---------------------------------------------------------------------------
// 6. Global Documentation Section/Tag Indexer, Full-Text Search & Source Defs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocSectionEntry {
    pub node_id: String,
    pub manual: String,
    pub title: String,
    pub level: i64,
    pub file_path: String,
    pub start_row: i64,
    pub end_row: i64,
    pub tags: Vec<String>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocSearchEntry {
    pub node_id: String,
    pub manual: String,
    pub title: String,
    pub file_path: String,
    pub row: i64,
    pub score: i64,
    pub matched_tag: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDefEntry {
    pub symbol: String,
    pub kind: String,
    pub file_path: String,
    pub row: i64,
    pub signature: String,
    pub doc_comment: String,
}

fn slugify_heading(title: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                slug.push(lc);
            }
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "section".to_string()
    } else {
        slug
    }
}

/// Extract `*help-tag*` tokens (Vim/Info style `*tag-name*`, excluding bold `**bold**` and spaces).
fn extract_star_tags_in_line(line: &str, out: &mut Vec<String>) {
    let bytes = line.as_bytes();
    let n = bytes.len();
    let mut i = 0usize;
    while i < n {
        if bytes[i] == b'*' {
            if i + 1 < n && bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'*') {
                    i += 1;
                }
                i = (i + 2).min(n);
                continue;
            }
            let prev_ok = i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'(' | b'[' | b'`');
            if prev_ok {
                let mut j = i + 1;
                while j < n
                    && (bytes[j].is_ascii_alphanumeric()
                        || matches!(bytes[j], b'-' | b'_' | b':' | b'.' | b'/'))
                {
                    j += 1;
                }
                if j > i + 1 && j < n && bytes[j] == b'*' && (j + 1 == n || bytes[j + 1] != b'*') {
                    let tag = &line[i + 1..j];
                    if tag.len() >= 2 && !out.iter().any(|t| t == tag) {
                        out.push(tag.to_string());
                    }
                    i = j + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
}

fn strip_trailing_star_tags(heading_text: &str) -> String {
    let mut cur = heading_text.trim();
    loop {
        if cur.ends_with('*')
            && !cur.ends_with("**")
            && let Some(open_idx) = cur[..cur.len() - 1].rfind('*')
        {
            let candidate = &cur[open_idx + 1..cur.len() - 1];
            let valid = candidate.len() >= 2
                && candidate.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.' | b'/')
                });
            if valid && (open_idx == 0 || cur.as_bytes()[open_idx - 1].is_ascii_whitespace()) {
                cur = cur[..open_idx].trim_end();
                continue;
            }
        }
        break;
    }
    cur.trim().to_string()
}

fn collect_files_with_ext(cur: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    if out.len() >= 4_000 {
        return;
    }
    let Ok(rd) = fs::read_dir(cur) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|d| d.path())).collect();
    entries.sort();
    for path in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.')
            || matches!(name, "target" | "node_modules" | "deprecated" | "TODO-jit")
        {
            continue;
        }
        if path.is_dir() {
            collect_files_with_ext(&path, ext, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
        if out.len() >= 4_000 {
            return;
        }
    }
}

fn manual_name_for_path(root: &Path, file_path: &Path) -> String {
    let rel = file_path.strip_prefix(root).unwrap_or(file_path);
    let mut s = rel.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = s.strip_suffix(".md") {
        s = stripped.to_string();
    }
    let root_name = root.file_name().and_then(|n| n.to_str()).unwrap_or("docs");
    if root_name != "docs"
        && let Some(parent_name) = root
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
    {
        return format!("{parent_name}:{s}");
    }
    if let Some(parent_name) = root
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        && parent_name == "nessa2"
    {
        return format!("nessa:{s}");
    }
    s
}

#[derive(Clone)]
struct ParsedDocSection {
    entry: DocSectionEntry,
    lines: Vec<String>,
}

fn parse_markdown_sections(root: &Path, file_path: &Path) -> Vec<ParsedDocSection> {
    let Ok(content) = fs::read_to_string(file_path) else {
        return Vec::new();
    };
    let all_lines: Vec<String> = content.lines().map(str::to_string).collect();
    if all_lines.is_empty() {
        return Vec::new();
    }
    let manual = manual_name_for_path(root, file_path);
    let full_path_str = file_path.to_string_lossy().to_string();

    let mut headings: Vec<(usize, i64, String, Vec<String>)> = Vec::new();
    let fm_end = doc_store::markdown_frontmatter_end_row(all_lines.iter().map(String::as_str));
    let mut fence = doc_store::MarkdownFenceState::new();
    for (idx, line) in all_lines.iter().enumerate() {
        if fm_end.is_some_and(|end| idx <= end) {
            continue;
        }
        if fence.step_line(line) {
            continue;
        }
        if let Some((_, level, raw_title)) = doc_store::parse_markdown_atx_heading(line) {
            let mut tags = Vec::new();
            extract_star_tags_in_line(&raw_title, &mut tags);
            let clean_title = strip_trailing_star_tags(&raw_title);
            headings.push((idx, level, clean_title, tags));
        }
    }

    if headings.is_empty() {
        let mut tags = Vec::new();
        let mut summary = String::new();
        let mut fallback_fence = doc_store::MarkdownFenceState::new();
        for (idx, ln) in all_lines.iter().enumerate() {
            if fm_end.is_some_and(|end| idx <= end) || fallback_fence.step_line(ln) {
                continue;
            }
            extract_star_tags_in_line(ln, &mut tags);
            let t = ln.trim();
            if summary.is_empty() && !t.is_empty() {
                summary = t.chars().take(120).collect();
            }
        }
        return vec![ParsedDocSection {
            entry: DocSectionEntry {
                node_id: format!("{manual}#top"),
                title: manual.clone(),
                manual,
                level: 1,
                file_path: full_path_str,
                start_row: fm_end.map_or(0, |end| ((end + 1).min(all_lines.len() - 1)) as i64),
                end_row: (all_lines.len() - 1) as i64,
                tags,
                summary,
            },
            lines: all_lines,
        }];
    }

    let mut out = Vec::with_capacity(headings.len());
    let mut used_ids = HashSet::new();
    for h_idx in 0..headings.len() {
        let (start_row, level, ref title, ref init_tags) = headings[h_idx];
        let end_row = if h_idx + 1 < headings.len() {
            headings[h_idx + 1].0.saturating_sub(1)
        } else {
            all_lines.len() - 1
        };
        let slug = if h_idx == 0 && level == 1 {
            "top".to_string()
        } else {
            slugify_heading(title)
        };
        let mut node_id = format!("{manual}#{slug}");
        if used_ids.contains(&node_id) {
            node_id = format!("{manual}#{slug}-{}", start_row + 1);
        }
        used_ids.insert(node_id.clone());

        let mut tags = init_tags.clone();
        let mut summary = String::new();
        let mut sec_fence = doc_store::MarkdownFenceState::new();
        let slice = &all_lines[start_row..=end_row];
        for (rel_i, ln) in slice.iter().enumerate() {
            if sec_fence.step_line(ln) {
                continue;
            }
            let t = ln.trim();
            extract_star_tags_in_line(ln, &mut tags);
            if rel_i > 0
                && summary.is_empty()
                && !t.is_empty()
                && !t.starts_with('#')
                && !t.starts_with("---")
                && !t.starts_with('|')
            {
                let cleaned = strip_trailing_star_tags(t);
                if !cleaned.is_empty() {
                    summary = cleaned.chars().take(120).collect();
                }
            }
        }
        out.push(ParsedDocSection {
            entry: DocSectionEntry {
                node_id,
                manual: manual.clone(),
                title: title.clone(),
                level,
                file_path: full_path_str.clone(),
                start_row: start_row as i64,
                end_row: end_row as i64,
                tags,
                summary,
            },
            lines: slice.to_vec(),
        });
    }
    out
}

type DocCacheMap = HashMap<Vec<String>, (u128, Arc<Vec<ParsedDocSection>>)>;

static DOC_SECTIONS_CACHE: OnceLock<Mutex<DocCacheMap>> = OnceLock::new();

fn compute_doc_roots_stamp(doc_roots: &[String]) -> u128 {
    let mut stamp: u128 = 0xcbf2_9ce4_8422_2325;
    for root_str in doc_roots {
        let root = Path::new(root_str);
        if let Ok(meta) = fs::metadata(root) {
            stamp = stamp
                .wrapping_mul(1099511628211)
                .wrapping_add(meta.len() as u128);
            if let Ok(mtime) = meta.modified()
                && let Ok(dur) = mtime.duration_since(UNIX_EPOCH)
            {
                stamp = stamp
                    .wrapping_mul(1099511628211)
                    .wrapping_add(dur.as_nanos());
            }
        }
        if root.is_dir()
            && let Ok(rd) = fs::read_dir(root)
        {
            for entry in rd.flatten().take(128) {
                if let Ok(m) = entry.metadata() {
                    stamp ^= (m.len() as u128).rotate_left(13);
                    if let Ok(mt) = m.modified()
                        && let Ok(dur) = mt.duration_since(UNIX_EPOCH)
                    {
                        stamp ^= dur.as_nanos().rotate_left(7);
                    }
                }
            }
        }
    }
    stamp
}

fn load_cached_doc_sections(doc_roots: &[String]) -> Arc<Vec<ParsedDocSection>> {
    let key: Vec<String> = doc_roots.to_vec();
    let stamp = compute_doc_roots_stamp(doc_roots);
    let cache = DOC_SECTIONS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some((prev_stamp, arc_secs)) = guard.get(&key)
        && *prev_stamp == stamp
    {
        return Arc::clone(arc_secs);
    }

    let mut sections = Vec::new();
    let mut seen_files = HashSet::new();
    for root_str in doc_roots {
        let root = Path::new(root_str);
        if !root.exists() {
            continue;
        }
        if root.is_file() {
            let canon = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            if seen_files.insert(canon) {
                let parent = root.parent().unwrap_or(Path::new("."));
                sections.extend(parse_markdown_sections(parent, root));
            }
            continue;
        }
        let mut md_files = Vec::new();
        collect_files_with_ext(root, "md", &mut md_files);
        for md in md_files {
            let canon = fs::canonicalize(&md).unwrap_or_else(|_| md.clone());
            if seen_files.insert(canon) {
                sections.extend(parse_markdown_sections(root, &md));
            }
        }
    }

    let arc_secs = Arc::new(sections);
    if let Ok(mut guard) = cache.lock() {
        if guard.len() > 16 {
            guard.clear();
        }
        guard.insert(key, (stamp, Arc::clone(&arc_secs)));
    }
    arc_secs
}

pub fn index_docs(doc_roots: &[String]) -> Vec<DocSectionEntry> {
    let sections = load_cached_doc_sections(doc_roots);
    sections.iter().map(|sec| sec.entry.clone()).collect()
}

pub fn search_docs(doc_roots: &[String], query: &str, limit: usize) -> Vec<DocSearchEntry> {
    let cap = if limit == 0 { 100 } else { limit.min(500) };
    let q_trim = query.trim();
    let sections = load_cached_doc_sections(doc_roots);

    if q_trim.is_empty() {
        return sections
            .iter()
            .take(cap)
            .map(|sec| DocSearchEntry {
                node_id: sec.entry.node_id.clone(),
                manual: sec.entry.manual.clone(),
                title: sec.entry.title.clone(),
                file_path: sec.entry.file_path.clone(),
                row: sec.entry.start_row,
                score: 100,
                matched_tag: sec.entry.tags.first().cloned().unwrap_or_default(),
                snippet: sec.entry.summary.clone(),
            })
            .collect();
    }

    let q_lower = q_trim.to_lowercase();
    let mut pos_tokens = Vec::new();
    let mut neg_tokens = Vec::new();
    for raw_tok in q_lower.split_whitespace() {
        if let Some(neg) = raw_tok.strip_prefix('!') {
            if !neg.is_empty() {
                neg_tokens.push(neg.to_string());
            }
        } else {
            pos_tokens.push(raw_tok.to_string());
        }
    }

    let mut hits = Vec::new();
    for sec in sections.iter() {
        let title_low = sec.entry.title.to_lowercase();
        let manual_low = sec.entry.manual.to_lowercase();
        let body_joined = sec.lines.join("\n");
        let body_low = body_joined.to_lowercase();

        if neg_tokens
            .iter()
            .any(|nt| title_low.contains(nt) || manual_low.contains(nt) || body_low.contains(nt))
        {
            continue;
        }

        let mut matched_tag = String::new();
        let mut score = 0i64;

        // 1. Exact or prefix `*tag*` match
        for tag in &sec.entry.tags {
            let tl = tag.to_lowercase();
            if tl == q_lower {
                score += 25_000;
                matched_tag = tag.clone();
                break;
            } else if tl.starts_with(&q_lower) {
                if score < 18_000 {
                    score = 18_000;
                    matched_tag = tag.clone();
                }
            } else if tl.contains(&q_lower) && score < 12_000 {
                score = 12_000;
                matched_tag = tag.clone();
            }
        }

        // 2. Fuzzy title / manual match
        let header_hay = format!("{} {}", sec.entry.manual, sec.entry.title);
        if let Some(fsc) = fuzzy_score_one(q_trim, &q_lower, &header_hay) {
            score = score.max(fsc + 2_000);
        }

        // 3. Orderless full-text token match across section header + tags + body
        let mut all_pos_present = !pos_tokens.is_empty();
        let mut tf_bonus = 0i64;
        for tok in &pos_tokens {
            let in_title = title_low.contains(tok.as_str());
            let in_manual = manual_low.contains(tok.as_str());
            let in_tag = sec
                .entry
                .tags
                .iter()
                .any(|t| t.to_lowercase().contains(tok.as_str()));
            let body_count = body_low.matches(tok.as_str()).count() as i64;
            if !in_title && !in_manual && !in_tag && body_count == 0 {
                all_pos_present = false;
                break;
            }
            if in_tag {
                tf_bonus += 1_400;
                if matched_tag.is_empty()
                    && let Some(found) = sec
                        .entry
                        .tags
                        .iter()
                        .find(|t| t.to_lowercase().contains(tok.as_str()))
                {
                    matched_tag = found.clone();
                }
            }
            if in_title {
                tf_bonus += 900;
            }
            if in_manual {
                tf_bonus += 400;
            }
            tf_bonus += (body_count.min(12)) * 75;
        }
        if pos_tokens.len() > 1 && !all_pos_present {
            continue;
        }
        if all_pos_present {
            score = score.max(4_000 + tf_bonus);
        }

        if score <= 0 {
            continue;
        }

        // Find best matching line inside section for snippet + row jump
        let mut best_row = sec.entry.start_row;
        let mut best_snippet = sec.entry.summary.clone();
        for (rel_r, ln) in sec.lines.iter().enumerate() {
            let ln_trim = ln.trim();
            if ln_trim.is_empty() {
                continue;
            }
            let ln_low = ln_trim.to_lowercase();
            if ln_low.contains(&q_lower)
                || (!pos_tokens.is_empty()
                    && pos_tokens.iter().any(|t| ln_low.contains(t.as_str())))
            {
                best_row = sec.entry.start_row + rel_r as i64;
                best_snippet = ln_trim.chars().take(120).collect();
                if ln_low.contains(&q_lower) {
                    break;
                }
            }
        }

        hits.push(DocSearchEntry {
            node_id: sec.entry.node_id.clone(),
            manual: sec.entry.manual.clone(),
            title: sec.entry.title.clone(),
            file_path: sec.entry.file_path.clone(),
            row: best_row,
            score,
            matched_tag,
            snippet: best_snippet,
        });
    }

    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.manual.cmp(&b.manual))
            .then_with(|| a.row.cmp(&b.row))
    });
    hits.truncate(cap);
    hits
}

fn collect_preceding_doc_comment(lines: &[String], row: usize) -> String {
    let mut doc_lines = Vec::new();
    let mut cur = row;
    while cur > 0 {
        cur -= 1;
        let t = lines[cur].trim();
        if let Some(rest) = t.strip_prefix("--") {
            let cleaned = rest.trim();
            if !cleaned.starts_with("====") && !cleaned.starts_with("----") {
                doc_lines.push(cleaned.to_string());
            }
        } else if let Some(rest) = t.strip_prefix("///") {
            doc_lines.push(rest.trim().to_string());
        } else {
            break;
        }
    }
    doc_lines.reverse();
    doc_lines.join("\n")
}

fn extract_quoted_first_arg(s: &str) -> Option<String> {
    let q1 = s.find('"')?;
    let rest = &s[q1 + 1..];
    let q2 = rest.find('"')?;
    let inner = &rest[..q2];
    if inner.is_empty() {
        None
    } else {
        Some(inner.to_string())
    }
}

type CachedFileSymbols = Arc<Vec<(i64, SymbolDefEntry)>>;
type FileSymbolCacheMap = HashMap<PathBuf, (u64, u128, CachedFileSymbols)>;

fn file_symbol_cache() -> &'static Mutex<FileSymbolCacheMap> {
    static CACHE: OnceLock<Mutex<FileSymbolCacheMap>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn parse_ns_file_symbols_cached(file_path: &Path) -> CachedFileSymbols {
    let meta_opt = fs::metadata(file_path).ok();
    let (flen, mtime_ns) = meta_opt
        .as_ref()
        .map(|m| {
            let ns = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            (m.len(), ns)
        })
        .unwrap_or((0, 0));

    if meta_opt.is_some()
        && let Ok(guard) = file_symbol_cache().lock()
        && let Some((c_len, c_ns, entries)) = guard.get(file_path)
        && *c_len == flen
        && *c_ns == mtime_ns
    {
        return Arc::clone(entries);
    }

    let Ok(content) = fs::read_to_string(file_path) else {
        return Arc::new(Vec::new());
    };
    let lines: Vec<String> = content.lines().map(str::to_string).collect();
    let fp_str = file_path.to_string_lossy().to_string();
    let kw_table: &[(&str, &str)] = &[
        ("fn ", "fn"),
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("trait ", "trait"),
        ("effect ", "effect"),
        ("const ", "const"),
        ("type ", "type"),
    ];
    let mut parsed: Vec<(i64, SymbolDefEntry)> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("--") {
            continue;
        }

        if let Some(rc_pos) = trimmed.find("register_command(") {
            let after_rc = &trimmed[rc_pos + "register_command(".len()..];
            let cmd_opt = extract_quoted_first_arg(after_rc).or_else(|| {
                lines
                    .get(idx + 1)
                    .and_then(|next_ln| extract_quoted_first_arg(next_ln.trim()))
            });
            if let Some(cmd_name) = cmd_opt {
                let doc = collect_preceding_doc_comment(&lines, idx);
                parsed.push((
                    500,
                    SymbolDefEntry {
                        symbol: cmd_name,
                        kind: "command".to_string(),
                        file_path: fp_str.clone(),
                        row: idx as i64,
                        signature: trimmed.chars().take(120).collect(),
                        doc_comment: doc,
                    },
                ));
            }
        }

        let mut rest = trimmed;
        for vis in ["pub(crate) ", "pub ", "private ", "async "] {
            if let Some(s) = rest.strip_prefix(vis) {
                rest = s.trim_start();
            }
        }
        for (kw, kind) in kw_table {
            if let Some(after_kw) = rest.strip_prefix(kw) {
                let sym: String = after_kw
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !sym.is_empty() {
                    let pub_bonus = if trimmed.starts_with("pub ") { 250 } else { 0 };
                    let doc = collect_preceding_doc_comment(&lines, idx);
                    parsed.push((
                        pub_bonus,
                        SymbolDefEntry {
                            symbol: sym,
                            kind: (*kind).to_string(),
                            file_path: fp_str.clone(),
                            row: idx as i64,
                            signature: trimmed.chars().take(120).collect(),
                            doc_comment: doc,
                        },
                    ));
                }
                break;
            }
        }
    }

    let arc_entries = Arc::new(parsed);
    if meta_opt.is_some()
        && let Ok(mut guard) = file_symbol_cache().lock()
    {
        if guard.len() > 1024 {
            guard.clear();
        }
        guard.insert(
            file_path.to_path_buf(),
            (flen, mtime_ns, Arc::clone(&arc_entries)),
        );
    }
    arc_entries
}

pub fn find_symbol_defs(code_roots: &[String], query: &str, limit: usize) -> Vec<SymbolDefEntry> {
    let cap = if limit == 0 { 50 } else { limit.min(300) };
    let q_trim = query.trim();
    if q_trim.is_empty() {
        return Vec::new();
    }
    let q_lower = q_trim.to_lowercase();

    let mut ns_files = Vec::new();
    let mut seen = HashSet::new();
    for root_str in code_roots {
        let root = Path::new(root_str);
        if !root.exists() {
            continue;
        }
        if root.is_file() {
            let canon = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            if seen.insert(canon) {
                ns_files.push(root.to_path_buf());
            }
            continue;
        }
        let mut found = Vec::new();
        collect_files_with_ext(root, "ns", &mut found);
        for f in found {
            if seen.insert(f.clone()) {
                ns_files.push(f);
            }
        }
    }

    let mut scored: Vec<(i64, SymbolDefEntry)> = Vec::new();
    for file_path in ns_files {
        let entries = parse_ns_file_symbols_cached(&file_path);
        for (bonus, entry) in entries.iter() {
            if let Some(sc) = fuzzy_score_one(q_trim, &q_lower, &entry.symbol) {
                scored.push((sc + *bonus, entry.clone()));
            }
        }
    }

    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.symbol.cmp(&b.1.symbol))
            .then_with(|| a.1.file_path.cmp(&b.1.file_path))
    });
    scored.into_iter().take(cap).map(|(_, e)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_rank_prioritizes_exact_prefix_and_word_boundaries() {
        let cands = vec![
            "extensions/ext_minibuffer/src/lib.ns".to_string(),
            "find-file".to_string(),
            "format-file".to_string(),
            "unrelated".to_string(),
        ];
        let ranked = fuzzy_rank_batch("ff", &cands, 10);
        assert_eq!(ranked.len(), 3);
        assert!(ranked[0].index == 1 || ranked[0].index == 2);
    }

    #[test]
    fn orderless_rank_supports_acronym_positions_negation_and_anchors() {
        let cands = vec![
            "find-file".to_string(),
            "find-file-other-window".to_string(),
            "dirvish-find-file".to_string(),
            "window-split".to_string(),
        ];
        let (total, hits) = orderless_rank_batch("ff", &cands, 10);
        assert_eq!(total, 3);
        assert_eq!(hits[0].index, 0);
        assert_eq!(hits[0].positions, vec![0, 5]);

        let (total2, hits2) = orderless_rank_batch("file ^find !other", &cands, 10);
        assert_eq!(total2, 1);
        assert_eq!(hits2[0].index, 0);
        assert_eq!(hits2[0].positions, vec![0, 1, 2, 3, 5, 6, 7, 8]);
    }

    #[test]
    fn diff_hunks_identify_add_change_and_delete() {
        let old = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        let new = vec![
            "a".to_string(),
            "b_mod".to_string(),
            "d".to_string(),
            "e_added".to_string(),
        ];
        let hunks = diff_line_hunks(&old, &new);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].kind, "change");
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(hunks[1].kind, "add");
        assert_eq!(hunks[1].new_start, 3);
    }

    #[test]
    fn symbol_and_todo_scanners_extract_structured_entries() {
        let lines = vec![
            "-- TODO: optimize hot loop".to_string(),
            "pub struct Widget {".to_string(),
            "    pub fn render(self) {".to_string(),
        ];
        let syms = scan_symbols(&lines, "nessa");
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "Widget");
        assert_eq!(syms[1].name, "render");

        let todos = scan_buffer_todos(&lines, "demo.ns");
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].tag, "TODO");
    }

    #[test]
    fn doc_indexer_and_symbol_finder_extract_tags_orderless_and_defs() {
        let tmp = std::env::temp_dir().join(format!("lava_accel_doc_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let docs_dir = tmp.join("docs");
        let src_dir = tmp.join("src");
        fs::create_dir_all(&docs_dir).expect("create docs dir");
        fs::create_dir_all(&src_dir).expect("create src dir");

        fs::write(
            docs_dir.join("dirvish.md"),
            "# Dirvish File Manager *dirvish*\n\
             Three-column Miller preview browser.\n\n\
             ## Ranger Keybindings *ranger-keys*\n\
             Press `yy` to copy and `pp` to paste files.\n",
        )
        .expect("write md");

        fs::write(
            src_dir.join("sample.ns"),
            "-- Opens the interactive Info manual viewer.\n\
             pub fn open_info_manual(node: String) -> Unit {\n\
                 kernel.register_command(\"info\", \"ext_help\", \"Open Emacs Info manual\", handler)\n\
             }\n",
        )
        .expect("write ns");

        let roots = vec![docs_dir.to_string_lossy().to_string()];
        let indexed = index_docs(&roots);
        assert_eq!(indexed.len(), 2);
        assert_eq!(indexed[0].node_id, "dirvish#top");
        assert!(indexed[0].tags.contains(&"dirvish".to_string()));
        assert_eq!(indexed[1].title, "Ranger Keybindings");
        assert!(indexed[1].tags.contains(&"ranger-keys".to_string()));

        let tag_hits = search_docs(&roots, "ranger-keys", 10);
        assert!(!tag_hits.is_empty());
        assert_eq!(tag_hits[0].matched_tag, "ranger-keys");

        let orderless_hits = search_docs(&roots, "paste yy !miller", 10);
        assert_eq!(orderless_hits.len(), 1);
        assert_eq!(orderless_hits[0].title, "Ranger Keybindings");

        let code_roots = vec![src_dir.to_string_lossy().to_string()];
        let fn_defs = find_symbol_defs(&code_roots, "open_info", 10);
        assert!(!fn_defs.is_empty());
        assert_eq!(fn_defs[0].symbol, "open_info_manual");
        assert_eq!(fn_defs[0].kind, "fn");
        assert!(
            fn_defs[0]
                .doc_comment
                .contains("Opens the interactive Info")
        );

        let cmd_defs = find_symbol_defs(&code_roots, "info", 10);
        assert!(
            cmd_defs
                .iter()
                .any(|d| d.kind == "command" && d.symbol == "info")
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn markdown_frontmatter_fences_and_atx_headings_work_across_all_scanners() {
        let md_lines: Vec<String> = vec![
            "---",
            "title: Frontmatter Doc",
            "# Not a heading inside YAML frontmatter",
            "...",
            "# Real H1",
            "   ````markdown",
            "   ```sh",
            "   # Not a heading inside 4-backtick fence",
            "   ```",
            "   ~~~",
            "   # Still inside 4-backtick fence",
            "   ````",
            "> ~~~python",
            "> # Not a heading inside blockquote tilde fence",
            "> ~~~",
            "##\tTabbed H2",
            "###",
            "###### Max H6",
            "####### Seven hashes ignored",
            "#hashtag_ignored",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        for mode in ["markdown", "md", "gfm"] {
            let syms = scan_symbols(&md_lines, mode);
            assert_eq!(syms.len(), 4, "mode={mode}: {syms:?}");
            assert_eq!(
                (syms[0].name.as_str(), syms[0].level, syms[0].row),
                ("Real H1", 1, 4)
            );
            assert_eq!(
                (syms[1].name.as_str(), syms[1].level, syms[1].row),
                ("Tabbed H2", 2, 15)
            );
            assert_eq!(
                (syms[2].name.as_str(), syms[2].level, syms[2].row),
                ("", 3, 16)
            );
            assert_eq!(
                (syms[3].name.as_str(), syms[3].level, syms[3].row),
                ("Max H6", 6, 17)
            );

            let doc = doc_store::doc_new(md_lines.clone());
            let spans = doc_store::doc_scan_symbol_spans(doc, mode, 50);
            assert_eq!(spans.len(), 4, "doc spans mode={mode}: {spans:?}");
            assert_eq!((spans[0].row, spans[0].end_row), (4, 19));
            assert_eq!((spans[1].row, spans[1].end_row), (15, 19));
            assert_eq!((spans[2].row, spans[2].end_row), (16, 19));
            assert_eq!((spans[3].row, spans[3].end_row), (17, 19));
            doc_store::doc_free(doc);
        }

        let tmp = std::env::temp_dir().join(format!("lava_md_fence_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("create tmp md dir");
        let md_path = tmp.join("guide.md");
        fs::write(&md_path, md_lines.join("\n")).expect("write guide.md");
        let sections = index_docs(&[tmp.to_string_lossy().to_string()]);
        assert_eq!(sections.len(), 4);
        assert_eq!(sections[0].title, "Real H1");
        assert_eq!(sections[1].title, "Tabbed H2");
        assert_eq!(sections[3].title, "Max H6");
        assert_eq!(sections[3].level, 6);
        let _ = fs::remove_dir_all(&tmp);
    }
}
