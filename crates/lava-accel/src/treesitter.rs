//! Native Tree-sitter dynamic runtime (`dlopen`) and stateful Nessa highlighter (`lava-accel`).
//!
//! Dynamically loads system `libtree-sitter.so` (`0.26` / `0` / unversioned) and language `.so`
//! grammars without build-time C toolchain link dependencies, compiles `highlights.scm` queries
//! (with recursive `; inherits:` resolution, resilient per-pattern fallback on schema mismatch,
//! full predicate evaluation, and AST node-type fallback), caches `TSTree` instances per
//! `(doc_id, version, lang)` (LRU 32), and converts UTF-8 byte offsets to exact 0-based Unicode
//! `char` columns on every row.

#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CStr, c_char, c_void};
#[cfg(unix)]
use std::ffi::{CString, c_int};
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Mutex, OnceLock};

use crate::doc_store::{
    MAX_NESSA_LIST_ITEMS, byte_to_char_idx, char_len_fast, char_to_byte_idx,
    doc_snapshot_source_and_offsets, doc_version_and_line_count,
};

/// Single highlighted character range `[start_col, end_col)` on a buffer row, using 0-based
/// Unicode scalar (`char`) columns and standardized `1..=18` highlight group IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsHighlightSpan {
    pub start_col: i64,
    pub end_col: i64,
    pub hl_group: i64,
}

// Highlight group constants (`1..=18`; `0` = unstyled / generic `@variable`).
pub const HL_NONE: u8 = 0;
pub const HL_KEYWORD: u8 = 1;
pub const HL_KEYWORD_CONTROL: u8 = 2;
pub const HL_FUNCTION: u8 = 3;
pub const HL_MACRO_ATTR: u8 = 4;
pub const HL_TYPE: u8 = 5;
pub const HL_TYPE_BUILTIN: u8 = 6;
pub const HL_STRING: u8 = 7;
pub const HL_ESCAPE_REGEX: u8 = 8;
pub const HL_NUMBER_BOOL: u8 = 9;
pub const HL_CONSTANT: u8 = 10;
pub const HL_PROPERTY_FIELD: u8 = 11;
pub const HL_PARAMETER: u8 = 12;
pub const HL_NAMESPACE_LABEL: u8 = 13;
pub const HL_OPERATOR: u8 = 14;
pub const HL_COMMENT: u8 = 15;
pub const HL_DOC_COMMENT: u8 = 16;
pub const HL_TAG_HEADING: u8 = 17;
pub const HL_MARKUP_LINK_RAW: u8 = 18;
/// Internal sentinel for `@embedded` interpolation regions that clears outer `@string` cells
/// while allowing inner smaller-span tokens (`{`, `}`, operators, numbers, calls) to win.
const HL_EMBEDDED_CLEAR: u8 = 255;

const DOC_TREE_LRU_CAP: usize = 32;
const SNIPPET_LRU_CAP: usize = 64;
const NESSA_DOC_LRU_CAP: usize = 32;
const MAX_HIGHLIGHT_ROW_CHARS: usize = 8_192;
const MAX_PARSE_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const TS_ABI_FALLBACK_VERSION: i64 = 15;

pub fn hl_group_name(group: i64) -> &'static str {
    match group {
        1 => "Keyword",
        2 => "Control/Import Keyword",
        3 => "Function/Method/Constructor",
        4 => "Macro/BuiltinFn/Attribute/Preproc",
        5 => "Type",
        6 => "BuiltinType",
        7 => "String/Char",
        8 => "Escape/Regexp",
        9 => "Number/Float/Boolean/BuiltinConst",
        10 => "Constant",
        11 => "Property/Field/Member",
        12 => "Parameter",
        13 => "Namespace/Module/Label",
        14 => "Operator",
        15 => "Comment",
        16 => "DocComment",
        17 => "Tag/MarkupHeading",
        18 => "MarkupRaw/Link/URL",
        _ => "Variable/Plain",
    }
}

// ---------------------------------------------------------------------------
// 1. Exact C ABI Definitions (verified against `/usr/include/tree_sitter/api.h`)
// ---------------------------------------------------------------------------

#[repr(C)]
struct TSLanguage {
    _private: [u8; 0],
}

#[repr(C)]
struct TSParser {
    _private: [u8; 0],
}

#[repr(C)]
struct TSTree {
    _private: [u8; 0],
}

#[repr(C)]
struct TSQuery {
    _private: [u8; 0],
}

#[repr(C)]
struct TSQueryCursor {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TSPoint {
    row: u32,
    column: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TSInputEdit {
    start_byte: u32,
    old_end_byte: u32,
    new_end_byte: u32,
    start_point: TSPoint,
    old_end_point: TSPoint,
    new_end_point: TSPoint,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TSNode {
    context: [u32; 4],
    id: *const c_void,
    tree: *const TSTree,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TSQueryCapture {
    node: TSNode,
    index: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TSQueryMatch {
    id: u32,
    pattern_index: u16,
    capture_count: u16,
    captures: *const TSQueryCapture,
}

const TS_PREDICATE_STEP_DONE: u32 = 0;
const TS_PREDICATE_STEP_CAPTURE: u32 = 1;
const TS_PREDICATE_STEP_STRING: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct TSQueryPredicateStep {
    step_type: u32,
    value_id: u32,
}

#[cfg(unix)]
const RTLD_NOW: c_int = 0x00002;
#[cfg(unix)]
const RTLD_LOCAL: c_int = 0;

#[cfg(unix)]
#[cfg_attr(target_os = "linux", link(name = "dl"))]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

unsafe extern "C" {
    fn free(ptr: *mut c_void);
}

#[allow(dead_code)]
struct TsApi {
    _lib_handle: *mut c_void,
    ts_parser_new: unsafe extern "C" fn() -> *mut TSParser,
    ts_parser_delete: unsafe extern "C" fn(*mut TSParser),
    ts_parser_set_language: unsafe extern "C" fn(*mut TSParser, *const TSLanguage) -> bool,
    ts_parser_parse_string:
        unsafe extern "C" fn(*mut TSParser, *const TSTree, *const c_char, u32) -> *mut TSTree,
    ts_parser_reset: unsafe extern "C" fn(*mut TSParser),
    ts_tree_copy: Option<unsafe extern "C" fn(*const TSTree) -> *mut TSTree>,
    ts_tree_edit: Option<unsafe extern "C" fn(*mut TSTree, *const TSInputEdit)>,
    ts_tree_delete: unsafe extern "C" fn(*mut TSTree),
    ts_tree_root_node: unsafe extern "C" fn(*const TSTree) -> TSNode,
    ts_node_type: unsafe extern "C" fn(TSNode) -> *const c_char,
    ts_node_start_byte: unsafe extern "C" fn(TSNode) -> u32,
    ts_node_end_byte: unsafe extern "C" fn(TSNode) -> u32,
    ts_node_start_point: unsafe extern "C" fn(TSNode) -> TSPoint,
    ts_node_end_point: unsafe extern "C" fn(TSNode) -> TSPoint,
    ts_node_string: unsafe extern "C" fn(TSNode) -> *mut c_char,
    ts_node_is_null: unsafe extern "C" fn(TSNode) -> bool,
    ts_node_is_named: unsafe extern "C" fn(TSNode) -> bool,
    ts_node_is_missing: unsafe extern "C" fn(TSNode) -> bool,
    ts_node_is_error: unsafe extern "C" fn(TSNode) -> bool,
    ts_node_has_error: unsafe extern "C" fn(TSNode) -> bool,
    ts_node_parent: unsafe extern "C" fn(TSNode) -> TSNode,
    ts_node_child: unsafe extern "C" fn(TSNode, u32) -> TSNode,
    ts_node_child_count: unsafe extern "C" fn(TSNode) -> u32,
    ts_node_field_name_for_child: unsafe extern "C" fn(TSNode, u32) -> *const c_char,
    ts_node_descendant_for_point_range: unsafe extern "C" fn(TSNode, TSPoint, TSPoint) -> TSNode,
    ts_node_named_descendant_for_point_range:
        unsafe extern "C" fn(TSNode, TSPoint, TSPoint) -> TSNode,
    ts_query_new: unsafe extern "C" fn(
        *const TSLanguage,
        *const c_char,
        u32,
        *mut u32,
        *mut u32,
    ) -> *mut TSQuery,
    ts_query_delete: unsafe extern "C" fn(*mut TSQuery),
    ts_query_pattern_count: unsafe extern "C" fn(*const TSQuery) -> u32,
    ts_query_capture_count: unsafe extern "C" fn(*const TSQuery) -> u32,
    ts_query_predicates_for_pattern:
        unsafe extern "C" fn(*const TSQuery, u32, *mut u32) -> *const TSQueryPredicateStep,
    ts_query_capture_name_for_id:
        unsafe extern "C" fn(*const TSQuery, u32, *mut u32) -> *const c_char,
    ts_query_string_value_for_id:
        unsafe extern "C" fn(*const TSQuery, u32, *mut u32) -> *const c_char,
    ts_query_cursor_new: unsafe extern "C" fn() -> *mut TSQueryCursor,
    ts_query_cursor_delete: unsafe extern "C" fn(*mut TSQueryCursor),
    ts_query_cursor_exec: unsafe extern "C" fn(*mut TSQueryCursor, *const TSQuery, TSNode),
    ts_query_cursor_set_point_range:
        unsafe extern "C" fn(*mut TSQueryCursor, TSPoint, TSPoint) -> bool,
    ts_query_cursor_next_match: unsafe extern "C" fn(*mut TSQueryCursor, *mut TSQueryMatch) -> bool,
    ts_language_abi_version: Option<unsafe extern "C" fn(*const TSLanguage) -> u32>,
}

// SAFETY: `TsApi` holds an immutable `dlopen` shared-object handle (never closed during process
// lifetime) and resolved `extern "C"` function pointers into `libtree-sitter`, which are stateless
// code pointers safe to invoke from synchronized contexts across threads.
unsafe impl Send for TsApi {}
// SAFETY: All fields in `TsApi` are immutable after initialization in `OnceLock` and point to
// reentrant C library functions operating on caller-supplied parser/tree/cursor instances.
unsafe impl Sync for TsApi {}

#[cfg(unix)]
unsafe fn load_sym<T: Copy>(handle: *mut c_void, name: &[u8]) -> Option<T> {
    debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
    debug_assert_eq!(name.last(), Some(&0));
    // SAFETY: `handle` is a valid non-null `dlopen` handle and `name` is NUL-terminated.
    let raw = unsafe { dlsym(handle, name.as_ptr().cast::<c_char>()) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: POSIX/System V ABI guarantees `void*` returned by `dlsym` has identical size and
    // representation to an `extern "C"` function pointer `T`.
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&raw) })
}

#[cfg(unix)]
fn try_load_ts_api() -> Option<TsApi> {
    const CANDIDATES: &[&str] = &[
        "/usr/lib/libtree-sitter.so.0.26",
        "/usr/lib/libtree-sitter.so.0",
        "/usr/lib/libtree-sitter.so",
        "/usr/lib/x86_64-linux-gnu/libtree-sitter.so.0.26",
        "/usr/lib/x86_64-linux-gnu/libtree-sitter.so.0",
        "/usr/lib/x86_64-linux-gnu/libtree-sitter.so",
        "/usr/lib64/libtree-sitter.so.0.26",
        "/usr/lib64/libtree-sitter.so.0",
        "/usr/lib64/libtree-sitter.so",
        "libtree-sitter.so.0.26",
        "libtree-sitter.so.0",
        "libtree-sitter.so",
    ];

    let mut handle: *mut c_void = ptr::null_mut();
    for cand in CANDIDATES {
        let Ok(c_path) = CString::new(*cand) else {
            continue;
        };
        // SAFETY: `c_path` is a valid NUL-terminated string; `RTLD_NOW | RTLD_LOCAL` resolves all
        // symbols eagerly without polluting global symbol namespace.
        let h = unsafe { dlopen(c_path.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
        if !h.is_null() {
            handle = h;
            break;
        }
    }
    if handle.is_null() {
        return None;
    }

    // SAFETY: `handle` was just verified non-null from `dlopen`; every symbol literal below is
    // NUL-terminated and matched against `/usr/include/tree_sitter/api.h` signatures.
    unsafe {
        let abi_fn = load_sym(handle, b"ts_language_abi_version\0")
            .or_else(|| load_sym(handle, b"ts_language_version\0"));

        Some(TsApi {
            _lib_handle: handle,
            ts_parser_new: load_sym(handle, b"ts_parser_new\0")?,
            ts_parser_delete: load_sym(handle, b"ts_parser_delete\0")?,
            ts_parser_set_language: load_sym(handle, b"ts_parser_set_language\0")?,
            ts_parser_parse_string: load_sym(handle, b"ts_parser_parse_string\0")?,
            ts_parser_reset: load_sym(handle, b"ts_parser_reset\0")?,
            ts_tree_copy: load_sym(handle, b"ts_tree_copy\0"),
            ts_tree_edit: load_sym(handle, b"ts_tree_edit\0"),
            ts_tree_delete: load_sym(handle, b"ts_tree_delete\0")?,
            ts_tree_root_node: load_sym(handle, b"ts_tree_root_node\0")?,
            ts_node_type: load_sym(handle, b"ts_node_type\0")?,
            ts_node_start_byte: load_sym(handle, b"ts_node_start_byte\0")?,
            ts_node_end_byte: load_sym(handle, b"ts_node_end_byte\0")?,
            ts_node_start_point: load_sym(handle, b"ts_node_start_point\0")?,
            ts_node_end_point: load_sym(handle, b"ts_node_end_point\0")?,
            ts_node_string: load_sym(handle, b"ts_node_string\0")?,
            ts_node_is_null: load_sym(handle, b"ts_node_is_null\0")?,
            ts_node_is_named: load_sym(handle, b"ts_node_is_named\0")?,
            ts_node_is_missing: load_sym(handle, b"ts_node_is_missing\0")?,
            ts_node_is_error: load_sym(handle, b"ts_node_is_error\0")?,
            ts_node_has_error: load_sym(handle, b"ts_node_has_error\0")?,
            ts_node_parent: load_sym(handle, b"ts_node_parent\0")?,
            ts_node_child: load_sym(handle, b"ts_node_child\0")?,
            ts_node_child_count: load_sym(handle, b"ts_node_child_count\0")?,
            ts_node_field_name_for_child: load_sym(handle, b"ts_node_field_name_for_child\0")?,
            ts_node_descendant_for_point_range: load_sym(
                handle,
                b"ts_node_descendant_for_point_range\0",
            )?,
            ts_node_named_descendant_for_point_range: load_sym(
                handle,
                b"ts_node_named_descendant_for_point_range\0",
            )?,
            ts_query_new: load_sym(handle, b"ts_query_new\0")?,
            ts_query_delete: load_sym(handle, b"ts_query_delete\0")?,
            ts_query_pattern_count: load_sym(handle, b"ts_query_pattern_count\0")?,
            ts_query_capture_count: load_sym(handle, b"ts_query_capture_count\0")?,
            ts_query_predicates_for_pattern: load_sym(
                handle,
                b"ts_query_predicates_for_pattern\0",
            )?,
            ts_query_capture_name_for_id: load_sym(handle, b"ts_query_capture_name_for_id\0")?,
            ts_query_string_value_for_id: load_sym(handle, b"ts_query_string_value_for_id\0")?,
            ts_query_cursor_new: load_sym(handle, b"ts_query_cursor_new\0")?,
            ts_query_cursor_delete: load_sym(handle, b"ts_query_cursor_delete\0")?,
            ts_query_cursor_exec: load_sym(handle, b"ts_query_cursor_exec\0")?,
            ts_query_cursor_set_point_range: load_sym(
                handle,
                b"ts_query_cursor_set_point_range\0",
            )?,
            ts_query_cursor_next_match: load_sym(handle, b"ts_query_cursor_next_match\0")?,
            ts_language_abi_version: abi_fn,
        })
    }
}

#[cfg(not(unix))]
fn try_load_ts_api() -> Option<TsApi> {
    None
}

static GLOBAL_TS_API: OnceLock<Option<TsApi>> = OnceLock::new();

fn ts_api() -> Option<&'static TsApi> {
    GLOBAL_TS_API.get_or_init(try_load_ts_api).as_ref()
}

// ---------------------------------------------------------------------------
// 2. Language Normalization, Grammar `.so` Discovery & Query `.scm` Loading
// ---------------------------------------------------------------------------

pub fn normalize_ts_lang(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return String::new();
    }
    match trimmed.as_str() {
        "ns" | "nessa" => "nessa".to_string(),
        "rs" | "rust" => "rust".to_string(),
        "py" | "python" | "python3" | "pyi" => "python".to_string(),
        "js" | "jsx" | "mjs" | "cjs" | "javascript" | "ecmascript" => "javascript".to_string(),
        "ts" | "mts" | "cts" | "typescript" => "typescript".to_string(),
        "tsx" => "tsx".to_string(),
        "c++" | "cc" | "cxx" | "hpp" | "hxx" | "hh" | "cpp" => "cpp".to_string(),
        "h" | "c" => "c".to_string(),
        "sh" | "zsh" | "shell" | "ksh" | "bash" | "bashrc" => "bash".to_string(),
        "yml" | "yaml" => "yaml".to_string(),
        "md" | "gfm" | "markdown" | "markdown-inline" | "markdown_inline" => "markdown".to_string(),
        "htm" | "xhtml" | "html" => "html".to_string(),
        "cs" | "csharp" | "c#" | "c_sharp" | "c-sharp" => "c-sharp".to_string(),
        "golang" | "go" => "go".to_string(),
        "rb" | "ruby" => "ruby".to_string(),
        "kt" | "kts" | "kotlin" => "kotlin".to_string(),
        "hs" | "haskell" => "haskell".to_string(),
        "ex" | "exs" | "elixir" => "elixir".to_string(),
        "ml" | "mli" | "ocaml" => "ocaml".to_string(),
        "jsonc" | "jsonl" | "json" => "json".to_string(),
        "makefile" | "gnumake" | "mk" | "make" => "make".to_string(),
        "el" | "elisp" | "emacs-lisp" => "elisp".to_string(),
        "scm" | "tsq" | "query" => "query".to_string(),
        "viml" | "vim" => "vim".to_string(),
        "tex" | "plaintex" | "latex" => "latex".to_string(),
        "proto" | "protobuf" => "protobuf".to_string(),
        "docker" | "dockerfile" => "dockerfile".to_string(),
        _ => trimmed,
    }
}

fn home_dir_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|s| !s.is_empty()))
        .map(PathBuf::from)
}

fn sibling_nessa_repo_dir() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let mut cur: Option<&Path> = Some(cwd.as_path());
    while let Some(dir) = cur {
        let candidate = dir.join("nessa2");
        if candidate.join("tools/tree-sitter-nessa").is_dir() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}

fn parser_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(7);
    if let Some(env_dir) = std::env::var_os("LAVA_TS_PARSER_DIR")
        && !env_dir.is_empty()
    {
        dirs.push(PathBuf::from(env_dir));
    }
    if let Some(home) = home_dir_path() {
        dirs.push(home.join(".local/share/lava/parsers"));
    }
    dirs.push(PathBuf::from("/usr/lib/helix/runtime/grammars"));
    dirs.push(PathBuf::from("/usr/lib/tree_sitter"));
    dirs.push(PathBuf::from("/usr/lib"));
    if let Some(home) = home_dir_path() {
        dirs.push(home.join(".local/share/nvim/lazy/nvim-treesitter/parser"));
    }
    if let Some(nessa_repo) = sibling_nessa_repo_dir() {
        dirs.push(nessa_repo.join("target/release"));
    }
    dirs
}

fn candidate_grammar_paths(lang: &str) -> Vec<PathBuf> {
    let lang_under = lang.replace('-', "_");
    let mut out = Vec::new();

    if let Some(env_dir) = std::env::var_os("LAVA_TS_PARSER_DIR")
        && !env_dir.is_empty()
    {
        let p = PathBuf::from(env_dir);
        out.push(p.join(format!("{lang}.so")));
        if lang_under != lang {
            out.push(p.join(format!("{lang_under}.so")));
        }
        out.push(p.join(format!("libtree-sitter-{lang}.so")));
    }
    if let Some(home) = home_dir_path() {
        let p = home.join(".local/share/lava/parsers");
        out.push(p.join(format!("{lang}.so")));
        if lang_under != lang {
            out.push(p.join(format!("{lang_under}.so")));
        }
        out.push(p.join(format!("libtree-sitter-{lang}.so")));
    }
    out.push(PathBuf::from(format!(
        "/usr/lib/helix/runtime/grammars/{lang}.so"
    )));
    if lang_under != lang {
        out.push(PathBuf::from(format!(
            "/usr/lib/helix/runtime/grammars/{lang_under}.so"
        )));
    }
    out.push(PathBuf::from(format!("/usr/lib/tree_sitter/{lang}.so")));
    if lang_under != lang {
        out.push(PathBuf::from(format!(
            "/usr/lib/tree_sitter/{lang_under}.so"
        )));
    }
    out.push(PathBuf::from(format!("/usr/lib/libtree-sitter-{lang}.so")));
    if lang_under != lang {
        out.push(PathBuf::from(format!(
            "/usr/lib/libtree-sitter-{lang_under}.so"
        )));
    }
    if let Some(home) = home_dir_path() {
        let p = home.join(".local/share/nvim/lazy/nvim-treesitter/parser");
        out.push(p.join(format!("{lang}.so")));
        if lang_under != lang {
            out.push(p.join(format!("{lang_under}.so")));
        }
    }
    if lang == "nessa"
        && let Some(nessa_repo) = sibling_nessa_repo_dir()
    {
        let rel = nessa_repo.join("target/release");
        out.push(rel.join("nessa.so"));
        out.push(rel.join("libtree-sitter-nessa.so"));
    }
    out
}

fn query_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(6);
    if let Some(home) = home_dir_path() {
        roots.push(home.join(".config/lava/queries"));
    }
    roots.push(PathBuf::from("/usr/lib/helix/runtime/queries"));
    roots.push(PathBuf::from("/usr/share/helix/runtime/queries"));
    roots.push(PathBuf::from("/usr/share/tree-sitter/queries"));
    roots.push(PathBuf::from("/usr/share/nvim/runtime/queries"));
    if let Some(nessa_repo) = sibling_nessa_repo_dir() {
        roots.push(nessa_repo.join("tools/tree-sitter-nessa/queries"));
    }
    roots
}

fn find_highlights_scm_file(qlang: &str) -> Option<PathBuf> {
    let qlang_under = qlang.replace('-', "_");
    let qlang_dash = qlang.replace('_', "-");
    for root in query_search_roots() {
        let p1 = root.join(qlang).join("highlights.scm");
        if p1.is_file() {
            return Some(p1);
        }
        if qlang_under != qlang {
            let p2 = root.join(&qlang_under).join("highlights.scm");
            if p2.is_file() {
                return Some(p2);
            }
        }
        if qlang_dash != qlang {
            let p3 = root.join(&qlang_dash).join("highlights.scm");
            if p3.is_file() {
                return Some(p3);
            }
        }
    }
    None
}

fn parse_inherits_line(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    let after_semi = trimmed.strip_prefix(';')?.trim_start();
    let rest = after_semi.strip_prefix("inherits:")?.trim();
    let mut parents = Vec::new();
    for raw_part in rest.split(',') {
        let clean = raw_part
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .trim();
        if !clean.is_empty() {
            parents.push(clean.to_string());
        }
    }
    Some(parents)
}

fn load_highlights_scm_recursive(qlang: &str, visited: &mut HashSet<String>) -> Option<String> {
    if !visited.insert(qlang.to_string()) {
        return None;
    }
    let scm_path = find_highlights_scm_file(qlang)?;
    let content = fs::read_to_string(scm_path).ok()?;

    let mut inherited_parents = Vec::new();
    for line in content.lines().take(32) {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with(';') {
            if let Some(parents) = parse_inherits_line(t) {
                inherited_parents.extend(parents);
            }
            continue;
        }
        break;
    }

    let mut combined = String::new();
    for parent in inherited_parents {
        if let Some(parent_src) = load_highlights_scm_recursive(&parent, visited) {
            combined.push_str(&parent_src);
            if !combined.ends_with('\n') {
                combined.push('\n');
            }
        }
    }
    combined.push_str(&content);
    Some(combined)
}

const NESSA_QUERY_AUGMENT_SCM: &str = r##"
(package_use_path "@" @module)
(package_use_path (identifier) @module)
(use_list_path path: (identifier) @module)
(scoped_use_path path: (identifier) @module)
(qualified_type "#" @attribute qualifier: (_) @attribute)
(postfix_expression "#" @attribute)
((identifier) @type.builtin
  (#match? @type.builtin "^(bool|char|i8|i16|i32|i64|i128|isize|u8|u16|u32|u64|u128|usize|f32|f64|Any|Unit|NoReturn|Type|String|Bytes|List|Map|Set|Continuation)$"))
(const_declaration pattern: [ (identifier) (arbitrary_identifier) ] @constant)
(enum_variant name: (identifier) @constant (#match? @constant "^[A-Z]"))
(member_expression object: (identifier) @_error_type member: (identifier) @constant (#match? @_error_type "^[A-Z]") (#match? @constant "^[A-Z]"))
(member_pattern object: (identifier) @_error_type member: (identifier) @constant (#match? @_error_type "^[A-Z]") (#match? @constant "^[A-Z]"))
(call_expression function: (member_expression object: (identifier) @_error_type member: (identifier) @constant) (#match? @_error_type "^[A-Z]") (#match? @constant "^[A-Z]"))
"##;

const BUILTIN_NESSA_HIGHLIGHTS_SCM: &str = r##"
(format_directive) @keyword.directive
(doc_comment) @comment.documentation
(line_comment) @comment
(block_comment) @comment
(integer_literal) @number
(float_literal) @number.float
(boolean_literal) @boolean
(char_literal) @character
(string_literal) @string
(escape_sequence) @string.escape
(null_literal) @constant.builtin
(unit_literal) @constant.builtin
(self_literal) @variable.builtin
(self_type) @type.builtin
(wildcard) @variable.builtin
(macro_content) @embedded
(string_interpolation "{" @punctuation.special "}" @punctuation.special) @embedded
(symbol_literal "." @punctuation.special name: [ (identifier) (arbitrary_identifier) ] @string.special.symbol)
(visibility) @keyword.modifier
[ "fn" "def" "struct" "enum" "trait" "assoc" "impl" "extend" "derive" "typealias" "newtype" "mod" "global" "const" "effect" "handles" "catch" "error" "reset" "shift" "case" "lambda" "let" "var" "async" "await" "and" "or" "not" "matches" "is" "as" ] @keyword
[ "use" ] @keyword.import
[ "if" "else" "when" "match" "do" "for" "while" "in" "break" "continue" "return" "resume" "defer" ] @keyword.control
[ "=" "+=" "-=" "*=" "/=" "%=" "==" "!=" "<" "<=" ">" ">=" "+" "-" "*" "/" "%" "++" "->" "=>" "|>" "|" ".." "..=" "..." "?" "!" "#" "@" "'" ] @operator
(mod_definition name: [ (identifier) (arbitrary_identifier) ] @module)
(use_as_clause alias: [ (identifier) (arbitrary_identifier) ] @module)
(struct_definition name: [ (identifier) (arbitrary_identifier) ] @type.definition)
(enum_definition name: [ (identifier) (arbitrary_identifier) ] @type.definition)
(trait_definition name: [ (identifier) (arbitrary_identifier) ] @type.definition)
(newtype_definition name: [ (identifier) (arbitrary_identifier) ] @type.definition)
(typealias_definition name: [ (identifier) (arbitrary_identifier) ] @type.definition)
(assoc_declaration name: [ (identifier) (arbitrary_identifier) ] @type.definition)
(function_definition name: [ (identifier) (arbitrary_identifier) ] @function)
(trait_method_signature name: [ (identifier) (arbitrary_identifier) ] @function.method)
(trait_default_method name: [ (identifier) (arbitrary_identifier) ] @function.method)
(effect_definition name: [ (identifier) (arbitrary_identifier) ] @function)
(handles_statement effect: (handles_target [ (identifier) (arbitrary_identifier) ] @function))
(enum_variant name: (identifier) @constructor (#match? @constructor "^[a-z_]"))
(enum_variant name: (arbitrary_identifier) @constructor)
(struct_field name: [ (identifier) (arbitrary_identifier) ] @variable.member)
(property name: [ (identifier) (arbitrary_identifier) ] @variable.member)
(property_pattern name: [ (identifier) (arbitrary_identifier) ] @variable.member)
(named_argument name: [ (identifier) (arbitrary_identifier) ] @variable.parameter)
(parameter pattern: [ (identifier) (arbitrary_identifier) ] @variable.parameter)
(optional_parameter name: [ (identifier) (arbitrary_identifier) ] @variable.parameter)
(varargs_parameter name: [ (identifier) (arbitrary_identifier) ] @variable.parameter)
(catch_parameter name: [ (identifier) (arbitrary_identifier) ] @variable.parameter)
(catch_arm name: [ (identifier) (arbitrary_identifier) ] @variable.parameter)
(shift_expression continuation: [ (identifier) (arbitrary_identifier) ] @variable.parameter)
(loop_label name: [ (identifier) (arbitrary_identifier) ] @label)
(break_statement label: [ (identifier) (arbitrary_identifier) ] @label)
(continue_statement label: [ (identifier) (arbitrary_identifier) ] @label)
(member_expression member: (tuple_index) @number)
(member_expression member: [ (identifier) (arbitrary_identifier) ] @variable.member)
(member_expression object: (identifier) @_enum_type member: (identifier) @constructor (#match? @_enum_type "^[A-Z]") (#match? @constructor "^[a-z_]"))
(member_pattern object: (identifier) @_enum_type member: (identifier) @constructor (#match? @_enum_type "^[A-Z]") (#match? @constructor "^[a-z_]"))
(call_expression function: [ (identifier) (arbitrary_identifier) ] @function.call)
(call_expression function: (member_expression member: [ (identifier) (arbitrary_identifier) ] @function.method.call))
(call_expression function: (identifier) @type (#match? @type "^[A-Z]"))
(call_expression function: (member_expression object: (identifier) @_enum_type member: (identifier) @constructor) (#match? @_enum_type "^[A-Z]") (#match? @constructor "^[a-z_]"))
(infix_call_expression function: [ (identifier) (arbitrary_identifier) ] @function.call)
(view_expression view: [ (identifier) (arbitrary_identifier) ] @attribute)
((identifier) @type (#match? @type "^[A-Z][a-zA-Z0-9_]*$"))
"##;

pub(crate) fn load_highlights_query_source(lang: &str) -> Option<String> {
    let mut visited = HashSet::new();
    let loaded = load_highlights_scm_recursive(lang, &mut visited);
    if normalize_ts_lang(lang) == "nessa" {
        let mut src = loaded.unwrap_or_else(|| BUILTIN_NESSA_HIGHLIGHTS_SCM.to_string());
        if !src.ends_with('\n') {
            src.push('\n');
        }
        src.push_str(NESSA_QUERY_AUGMENT_SCM);
        return Some(src);
    }
    loaded
}

// ---------------------------------------------------------------------------
// 3. Resilient `.scm` Top-Level Pattern Splitter & Error Recovery
// ---------------------------------------------------------------------------

/// Return byte ranges `[start, end)` of every top-level pattern in `scm`.
/// Each top-level `(...)`, `[...]`, or `"..."` atom keeps its trailing `@capture` / quantifier
/// suffixes attached so stripping a failing pattern leaves valid `.scm` syntax.
fn scan_top_level_scm_patterns(scm: &str) -> Vec<(usize, usize)> {
    let bytes = scm.as_bytes();
    let n = bytes.len();
    let mut i = 0usize;
    let mut patterns = Vec::new();

    let skip_ws_and_comments = |mut pos: usize| -> usize {
        while pos < n {
            match bytes[pos] {
                b' ' | b'\t' | b'\r' | b'\n' => pos += 1,
                b';' => {
                    while pos < n && bytes[pos] != b'\n' {
                        pos += 1;
                    }
                }
                _ => break,
            }
        }
        pos
    };

    let consume_string = |mut pos: usize| -> usize {
        pos += 1; // skip opening '"'
        while pos < n {
            if bytes[pos] == b'\\' {
                pos = (pos + 2).min(n);
            } else if bytes[pos] == b'"' {
                pos += 1;
                break;
            } else {
                pos += 1;
            }
        }
        pos
    };

    while i < n {
        i = skip_ws_and_comments(i);
        if i >= n {
            break;
        }
        let pat_start = i;
        match bytes[i] {
            b'(' | b'[' => {
                let mut depth = 0usize;
                while i < n {
                    match bytes[i] {
                        b'"' => i = consume_string(i),
                        b';' => {
                            while i < n && bytes[i] != b'\n' {
                                i += 1;
                            }
                        }
                        b'(' | b'[' => {
                            depth += 1;
                            i += 1;
                        }
                        b')' | b']' => {
                            depth = depth.saturating_sub(1);
                            i += 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => i += 1,
                    }
                }
            }
            b'"' => {
                i = consume_string(i);
            }
            _ => {
                while i < n
                    && !matches!(
                        bytes[i],
                        b' ' | b'\t' | b'\r' | b'\n' | b';' | b'(' | b')' | b'[' | b']' | b'"'
                    )
                {
                    i += 1;
                }
            }
        }

        // Attach optional quantifier (`?`, `*`, `+`) and `@capture` names following the form.
        loop {
            let after_ws = skip_ws_and_comments(i);
            if after_ws < n && matches!(bytes[after_ws], b'?' | b'*' | b'+') {
                // Only treat `?`/`*`/`+` as a quantifier if not followed immediately by `"` or `@`
                // as a standalone string literal `"*" @operator` on the next line!
                let next_non_ws = skip_ws_and_comments(after_ws + 1);
                let is_standalone_op = after_ws > i
                    && bytes[i..after_ws].contains(&b'\n')
                    && next_non_ws < n
                    && bytes[next_non_ws] == b'@';
                if !is_standalone_op {
                    i = after_ws + 1;
                    continue;
                }
            }
            if after_ws < n && bytes[after_ws] == b'@' {
                let mut end_cap = after_ws + 1;
                while end_cap < n
                    && (bytes[end_cap].is_ascii_alphanumeric()
                        || matches!(bytes[end_cap], b'_' | b'.' | b'-'))
                {
                    end_cap += 1;
                }
                i = end_cap;
                continue;
            }
            break;
        }

        if i > pat_start {
            patterns.push((pat_start, i));
        } else {
            i += 1;
        }
    }

    patterns
}

/// Remove the single top-level `.scm` pattern covering (or immediately adjacent to) `error_offset`.
pub(crate) fn strip_failing_top_level_pattern(scm: &str, error_offset: usize) -> Option<String> {
    let patterns = scan_top_level_scm_patterns(scm);
    if patterns.is_empty() {
        return None;
    }
    let clamped = error_offset.min(scm.len());
    let target_idx = patterns
        .iter()
        .position(|&(s, e)| clamped >= s && clamped <= e)
        .or_else(|| patterns.iter().position(|&(s, _)| s >= clamped))
        .unwrap_or(patterns.len() - 1);

    let (rm_start, rm_end) = patterns[target_idx];
    if rm_end <= rm_start {
        return None;
    }
    let mut out = String::with_capacity(scm.len() - (rm_end - rm_start));
    out.push_str(&scm[..rm_start]);
    out.push('\n');
    out.push_str(&scm[rm_end..]);
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// 4. Dependency-Free Regex & Lua-Pattern Engine + Predicate Evaluator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum CharClassItem {
    Single(char),
    Range(char, char),
    Digit,
    Word,
    Space,
    NotDigit,
    NotWord,
    NotSpace,
}

impl CharClassItem {
    fn matches(&self, ch: char) -> bool {
        match *self {
            Self::Single(c) => ch == c,
            Self::Range(lo, hi) => ch >= lo && ch <= hi,
            Self::Digit => ch.is_ascii_digit(),
            Self::Word => ch == '_' || ch.is_ascii_alphanumeric(),
            Self::Space => ch.is_ascii_whitespace(),
            Self::NotDigit => !ch.is_ascii_digit(),
            Self::NotWord => !(ch == '_' || ch.is_ascii_alphanumeric()),
            Self::NotSpace => !ch.is_ascii_whitespace(),
        }
    }
}

#[derive(Debug, Clone)]
enum RegexAtom {
    Any,
    Literal(char),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
    Class {
        negated: bool,
        items: Vec<CharClassItem>,
    },
    Group(RegexAlt),
    AnchorStart,
    AnchorEnd,
    WordBoundary,
    NotWordBoundary,
}

#[derive(Debug, Clone)]
struct RegexPiece {
    atom: RegexAtom,
    min_rep: usize,
    max_rep: Option<usize>,
}

#[derive(Debug, Clone)]
struct RegexSeq {
    pieces: Vec<RegexPiece>,
}

#[derive(Debug, Clone)]
struct RegexAlt {
    branches: Vec<RegexSeq>,
}

#[derive(Debug, Clone)]
pub(crate) struct CompiledRegex {
    root: RegexAlt,
    anchored_start: bool,
}

impl CompiledRegex {
    pub(crate) fn compile(pattern: &str) -> Option<Self> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut pos = 0usize;
        let root = parse_regex_alt(&chars, &mut pos, false)?;
        let anchored_start = !root.branches.is_empty()
            && root.branches.iter().all(|seq| {
                seq.pieces
                    .first()
                    .is_some_and(|p| matches!(p.atom, RegexAtom::AnchorStart) && p.min_rep >= 1)
            });
        Some(Self {
            root,
            anchored_start,
        })
    }

    pub(crate) fn compile_lua(lua_pat: &str) -> Option<Self> {
        let translated = translate_lua_pattern_to_regex(lua_pat);
        Self::compile(&translated)
    }

    pub(crate) fn is_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        if self.anchored_start {
            return match_alt(&self.root, &chars, 0).is_some();
        }
        for start in 0..=chars.len() {
            if match_alt(&self.root, &chars, start).is_some() {
                return true;
            }
        }
        false
    }
}

fn translate_lua_pattern_to_regex(lua: &str) -> String {
    let cs: Vec<char> = lua.chars().collect();
    let mut out = String::with_capacity(lua.len() + 8);
    let mut i = 0usize;
    let mut in_class = false;

    while i < cs.len() {
        let ch = cs[i];
        if ch == '%' && i + 1 < cs.len() {
            let esc = cs[i + 1];
            match esc {
                'd' => out.push_str("\\d"),
                'D' => out.push_str("\\D"),
                's' => out.push_str("\\s"),
                'S' => out.push_str("\\S"),
                'w' => {
                    if in_class {
                        out.push_str("A-Za-z0-9");
                    } else {
                        out.push_str("[A-Za-z0-9]");
                    }
                }
                'W' => {
                    if !in_class {
                        out.push_str("[^A-Za-z0-9]");
                    }
                }
                'a' => {
                    if in_class {
                        out.push_str("A-Za-z");
                    } else {
                        out.push_str("[A-Za-z]");
                    }
                }
                'A' => {
                    if !in_class {
                        out.push_str("[^A-Za-z]");
                    }
                }
                'u' => {
                    if in_class {
                        out.push_str("A-Z");
                    } else {
                        out.push_str("[A-Z]");
                    }
                }
                'U' => {
                    if !in_class {
                        out.push_str("[^A-Z]");
                    }
                }
                'l' => {
                    if in_class {
                        out.push_str("a-z");
                    } else {
                        out.push_str("[a-z]");
                    }
                }
                'L' => {
                    if !in_class {
                        out.push_str("[^a-z]");
                    }
                }
                'x' => {
                    if in_class {
                        out.push_str("0-9A-Fa-f");
                    } else {
                        out.push_str("[0-9A-Fa-f]");
                    }
                }
                other => {
                    if matches!(
                        other,
                        '.' | '^'
                            | '$'
                            | '('
                            | ')'
                            | '['
                            | ']'
                            | '*'
                            | '+'
                            | '?'
                            | '{'
                            | '}'
                            | '|'
                            | '\\'
                    ) {
                        out.push('\\');
                    }
                    out.push(other);
                }
            }
            i += 2;
            continue;
        }
        if ch == '[' && !in_class {
            in_class = true;
            out.push('[');
            i += 1;
            if i < cs.len() && cs[i] == '^' {
                out.push('^');
                i += 1;
            }
            if i < cs.len() && cs[i] == ']' {
                out.push_str("\\]");
                i += 1;
            }
            continue;
        }
        if ch == ']' && in_class {
            in_class = false;
            out.push(']');
            i += 1;
            continue;
        }
        if !in_class && ch == '-' && i > 0 && !matches!(cs[i - 1], '^' | '(' | '|') {
            // Lua non-greedy `*` quantifier (`-`)
            out.push('*');
            i += 1;
            continue;
        }
        if !in_class && matches!(ch, '{' | '}' | '|') {
            out.push('\\');
            out.push(ch);
            i += 1;
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
}

fn parse_regex_alt(chars: &[char], pos: &mut usize, in_group: bool) -> Option<RegexAlt> {
    let mut branches = Vec::new();
    loop {
        branches.push(parse_regex_seq(chars, pos, in_group)?);
        if *pos < chars.len() && chars[*pos] == '|' {
            *pos += 1;
        } else {
            break;
        }
    }
    Some(RegexAlt { branches })
}

fn parse_regex_seq(chars: &[char], pos: &mut usize, in_group: bool) -> Option<RegexSeq> {
    let mut pieces = Vec::new();
    while *pos < chars.len() {
        let ch = chars[*pos];
        if ch == '|' || (in_group && ch == ')') {
            break;
        }
        let atom = match ch {
            '^' => {
                *pos += 1;
                RegexAtom::AnchorStart
            }
            '$' => {
                *pos += 1;
                RegexAtom::AnchorEnd
            }
            '.' => {
                *pos += 1;
                RegexAtom::Any
            }
            '(' => {
                *pos += 1;
                if *pos + 1 < chars.len() && chars[*pos] == '?' && chars[*pos + 1] == ':' {
                    *pos += 2;
                }
                let inner = parse_regex_alt(chars, pos, true)?;
                if *pos < chars.len() && chars[*pos] == ')' {
                    *pos += 1;
                }
                RegexAtom::Group(inner)
            }
            '[' => parse_char_class(chars, pos)?,
            '\\' => {
                *pos += 1;
                if *pos >= chars.len() {
                    RegexAtom::Literal('\\')
                } else {
                    let esc = chars[*pos];
                    *pos += 1;
                    match esc {
                        'd' => RegexAtom::Digit,
                        'D' => RegexAtom::NotDigit,
                        'w' => RegexAtom::Word,
                        'W' => RegexAtom::NotWord,
                        's' => RegexAtom::Space,
                        'S' => RegexAtom::NotSpace,
                        'b' => RegexAtom::WordBoundary,
                        'B' => RegexAtom::NotWordBoundary,
                        'n' => RegexAtom::Literal('\n'),
                        'r' => RegexAtom::Literal('\r'),
                        't' => RegexAtom::Literal('\t'),
                        other => RegexAtom::Literal(other),
                    }
                }
            }
            ')' => {
                *pos += 1;
                RegexAtom::Literal(')')
            }
            lit => {
                *pos += 1;
                RegexAtom::Literal(lit)
            }
        };

        let is_anchor = matches!(
            atom,
            RegexAtom::AnchorStart
                | RegexAtom::AnchorEnd
                | RegexAtom::WordBoundary
                | RegexAtom::NotWordBoundary
        );
        let (min_rep, max_rep) = if !is_anchor && *pos < chars.len() {
            parse_quantifier(chars, pos)
        } else {
            (1, Some(1))
        };
        pieces.push(RegexPiece {
            atom,
            min_rep,
            max_rep,
        });
    }
    Some(RegexSeq { pieces })
}

fn parse_quantifier(chars: &[char], pos: &mut usize) -> (usize, Option<usize>) {
    match chars[*pos] {
        '*' => {
            *pos += 1;
            if *pos < chars.len() && chars[*pos] == '?' {
                *pos += 1;
            }
            (0, None)
        }
        '+' => {
            *pos += 1;
            if *pos < chars.len() && chars[*pos] == '?' {
                *pos += 1;
            }
            (1, None)
        }
        '?' => {
            *pos += 1;
            if *pos < chars.len() && chars[*pos] == '?' {
                *pos += 1;
            }
            (0, Some(1))
        }
        '{' => {
            let saved = *pos;
            *pos += 1;
            let mut min_v = 0usize;
            let mut has_min = false;
            while *pos < chars.len() && chars[*pos].is_ascii_digit() {
                has_min = true;
                min_v = min_v
                    .saturating_mul(10)
                    .saturating_add((chars[*pos] as u8 - b'0') as usize);
                *pos += 1;
            }
            if !has_min || *pos >= chars.len() {
                *pos = saved;
                return (1, Some(1));
            }
            if chars[*pos] == '}' {
                *pos += 1;
                return (min_v, Some(min_v));
            }
            if chars[*pos] == ',' {
                *pos += 1;
                if *pos < chars.len() && chars[*pos] == '}' {
                    *pos += 1;
                    return (min_v, None);
                }
                let mut max_v = 0usize;
                let mut has_max = false;
                while *pos < chars.len() && chars[*pos].is_ascii_digit() {
                    has_max = true;
                    max_v = max_v
                        .saturating_mul(10)
                        .saturating_add((chars[*pos] as u8 - b'0') as usize);
                    *pos += 1;
                }
                if has_max && *pos < chars.len() && chars[*pos] == '}' {
                    *pos += 1;
                    return (min_v, Some(max_v));
                }
            }
            *pos = saved;
            (1, Some(1))
        }
        _ => (1, Some(1)),
    }
}

fn parse_class_escape(chars: &[char], pos: &mut usize) -> EitherClassEscape {
    if *pos >= chars.len() {
        return EitherClassEscape::Char('\\');
    }
    let esc = chars[*pos];
    *pos += 1;
    match esc {
        'd' => EitherClassEscape::Special(CharClassItem::Digit),
        'D' => EitherClassEscape::Special(CharClassItem::NotDigit),
        'w' => EitherClassEscape::Special(CharClassItem::Word),
        'W' => EitherClassEscape::Special(CharClassItem::NotWord),
        's' => EitherClassEscape::Special(CharClassItem::Space),
        'S' => EitherClassEscape::Special(CharClassItem::NotSpace),
        'n' => EitherClassEscape::Char('\n'),
        'r' => EitherClassEscape::Char('\r'),
        't' => EitherClassEscape::Char('\t'),
        other => EitherClassEscape::Char(other),
    }
}

enum EitherClassEscape {
    Char(char),
    Special(CharClassItem),
}

fn parse_char_class(chars: &[char], pos: &mut usize) -> Option<RegexAtom> {
    *pos += 1; // skip '['
    let negated = if *pos < chars.len() && chars[*pos] == '^' {
        *pos += 1;
        true
    } else {
        false
    };
    let mut items = Vec::new();
    let mut first = true;
    while *pos < chars.len() {
        let ch = chars[*pos];
        if ch == ']' && !first {
            *pos += 1;
            return Some(RegexAtom::Class { negated, items });
        }
        first = false;
        let start_ch_opt = if ch == '\\' {
            *pos += 1;
            match parse_class_escape(chars, pos) {
                EitherClassEscape::Char(c) => Some(c),
                EitherClassEscape::Special(sp) => {
                    items.push(sp);
                    None
                }
            }
        } else {
            *pos += 1;
            Some(ch)
        };

        if let Some(start_c) = start_ch_opt {
            if *pos + 1 < chars.len() && chars[*pos] == '-' && chars[*pos + 1] != ']' {
                *pos += 1; // skip '-'
                let end_c = if chars[*pos] == '\\' {
                    *pos += 1;
                    match parse_class_escape(chars, pos) {
                        EitherClassEscape::Char(c) => c,
                        EitherClassEscape::Special(sp) => {
                            items.push(CharClassItem::Single(start_c));
                            items.push(CharClassItem::Single('-'));
                            items.push(sp);
                            continue;
                        }
                    }
                } else {
                    let c = chars[*pos];
                    *pos += 1;
                    c
                };
                items.push(CharClassItem::Range(start_c, end_c));
            } else {
                items.push(CharClassItem::Single(start_c));
            }
        }
    }
    None
}

fn is_regex_word_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

fn match_alt(alt: &RegexAlt, chars: &[char], idx: usize) -> Option<usize> {
    for branch in &alt.branches {
        if let Some(end) = match_pieces(&branch.pieces, chars, idx) {
            return Some(end);
        }
    }
    None
}

fn match_atom(atom: &RegexAtom, chars: &[char], idx: usize) -> Option<usize> {
    match atom {
        RegexAtom::AnchorStart => (idx == 0).then_some(idx),
        RegexAtom::AnchorEnd => (idx == chars.len()).then_some(idx),
        RegexAtom::WordBoundary => {
            let left = idx > 0 && is_regex_word_char(chars[idx - 1]);
            let right = idx < chars.len() && is_regex_word_char(chars[idx]);
            (left != right).then_some(idx)
        }
        RegexAtom::NotWordBoundary => {
            let left = idx > 0 && is_regex_word_char(chars[idx - 1]);
            let right = idx < chars.len() && is_regex_word_char(chars[idx]);
            (left == right).then_some(idx)
        }
        RegexAtom::Any => {
            let ch = *chars.get(idx)?;
            (ch != '\n').then_some(idx + 1)
        }
        RegexAtom::Literal(expected) => {
            let ch = *chars.get(idx)?;
            (ch == *expected).then_some(idx + 1)
        }
        RegexAtom::Digit => {
            let ch = *chars.get(idx)?;
            ch.is_ascii_digit().then_some(idx + 1)
        }
        RegexAtom::NotDigit => {
            let ch = *chars.get(idx)?;
            (!ch.is_ascii_digit()).then_some(idx + 1)
        }
        RegexAtom::Word => {
            let ch = *chars.get(idx)?;
            is_regex_word_char(ch).then_some(idx + 1)
        }
        RegexAtom::NotWord => {
            let ch = *chars.get(idx)?;
            (!is_regex_word_char(ch)).then_some(idx + 1)
        }
        RegexAtom::Space => {
            let ch = *chars.get(idx)?;
            ch.is_ascii_whitespace().then_some(idx + 1)
        }
        RegexAtom::NotSpace => {
            let ch = *chars.get(idx)?;
            (!ch.is_ascii_whitespace()).then_some(idx + 1)
        }
        RegexAtom::Class { negated, items } => {
            let ch = *chars.get(idx)?;
            let hit = items.iter().any(|it| it.matches(ch));
            (hit != *negated).then_some(idx + 1)
        }
        RegexAtom::Group(inner) => match_alt(inner, chars, idx),
    }
}

fn match_pieces(pieces: &[RegexPiece], chars: &[char], idx: usize) -> Option<usize> {
    let Some((first, rest)) = pieces.split_first() else {
        return Some(idx);
    };
    let max_r = first.max_rep.unwrap_or(chars.len().saturating_sub(idx) + 1);
    let mut positions = Vec::new();
    let mut cur = idx;
    if first.min_rep == 0 {
        positions.push(cur);
    }
    for count in 1..=max_r {
        let Some(next) = match_atom(&first.atom, chars, cur) else {
            break;
        };
        if next == cur && count > first.min_rep {
            break;
        }
        cur = next;
        if count >= first.min_rep {
            positions.push(cur);
        }
    }
    for &candidate_idx in positions.iter().rev() {
        if let Some(final_idx) = match_pieces(rest, chars, candidate_idx) {
            return Some(final_idx);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 5. Capture Mapping & Precompiled Query Predicates
// ---------------------------------------------------------------------------

pub(crate) fn map_capture_name_to_hl_group(raw_name: &str) -> u8 {
    let name = raw_name.strip_prefix('@').unwrap_or(raw_name);
    if name == "embedded" {
        return HL_EMBEDDED_CLEAR;
    }
    if name == "punctuation.special" {
        return HL_MACRO_ATTR;
    }
    if name.is_empty()
        || name.starts_with('_')
        || name == "variable"
        || name == "none"
        || name == "spell"
        || name == "nospell"
        || name.starts_with("punctuation")
        || name.starts_with("local")
    {
        return HL_NONE;
    }

    // 16 = DocComment
    if name == "string.documentation"
        || (name.starts_with("comment") && name.contains("documentation"))
    {
        return HL_DOC_COMMENT;
    }
    // 15 = Comment
    if name.starts_with("comment") {
        return HL_COMMENT;
    }

    // 2 = Control / Import Keyword
    if name.starts_with("keyword.control")
        || matches!(
            name,
            "keyword.conditional"
                | "keyword.repeat"
                | "keyword.return"
                | "keyword.exception"
                | "keyword.import"
                | "conditional"
                | "repeat"
                | "exception"
                | "include"
        )
    {
        return HL_KEYWORD_CONTROL;
    }

    // 4 = Macro / BuiltinFn / Attribute / Preproc
    if matches!(
        name,
        "function.macro"
            | "function.builtin"
            | "function.special"
            | "macro"
            | "preproc"
            | "define"
            | "special"
            | "keyword.directive"
            | "keyword.directive.define"
    ) || name.starts_with("attribute")
    {
        return HL_MACRO_ATTR;
    }

    // 1 = Keyword
    if name.starts_with("keyword") || name == "storageclass" {
        return HL_KEYWORD;
    }

    // 3 = Function / Method / Constructor
    if name.starts_with("function") || name.starts_with("method") || name.starts_with("constructor")
    {
        return HL_FUNCTION;
    }

    // 6 = BuiltinType, 10 = Enum Variant, 5 = Type
    if name == "type.builtin" {
        return HL_TYPE_BUILTIN;
    }
    if name == "type.enum.variant.builtin" {
        return HL_NUMBER_BOOL;
    }
    if name.starts_with("type.enum.variant") {
        return HL_CONSTANT;
    }
    if name.starts_with("type") {
        return HL_TYPE;
    }

    // 8 = Escape / Regexp
    if name == "constant.character.escape"
        || name == "string.escape"
        || name == "character.special"
        || name == "string.regexp"
        || name == "string.regex"
    {
        return HL_ESCAPE_REGEX;
    }

    // 18 = MarkupRaw / Link / URL
    if name == "string.special.url"
        || name == "text.uri"
        || name == "text.literal"
        || name.starts_with("markup.raw")
        || name.starts_with("markup.link")
    {
        return HL_MARKUP_LINK_RAW;
    }

    // 17 = Tag / MarkupHeading
    if name.starts_with("tag") || name.starts_with("markup.heading") || name == "text.title" {
        return HL_TAG_HEADING;
    }
    if name.starts_with("markup") {
        return HL_MARKUP_LINK_RAW;
    }

    // 7 = String / Char
    if name.starts_with("string") || name == "character" || name == "constant.character" {
        return HL_STRING;
    }

    // 9 = Number / Float / Boolean / BuiltinConst
    if name.starts_with("number")
        || name == "float"
        || name == "boolean"
        || name.starts_with("constant.numeric")
        || name.starts_with("constant.builtin")
        || name == "variable.builtin"
    {
        return HL_NUMBER_BOOL;
    }

    // 10 = Constant
    if name.starts_with("constant") {
        return HL_CONSTANT;
    }

    // 12 = Parameter
    if name == "parameter" || name.starts_with("variable.parameter") {
        return HL_PARAMETER;
    }

    // 11 = Property / Field / Member
    if name == "property"
        || name == "field"
        || name.starts_with("variable.member")
        || name.starts_with("variable.other.member")
    {
        return HL_PROPERTY_FIELD;
    }

    // 13 = Namespace / Module / Label
    if name == "namespace" || name.starts_with("module") || name == "label" {
        return HL_NAMESPACE_LABEL;
    }

    // 14 = Operator
    if name.starts_with("operator") {
        return HL_OPERATOR;
    }

    HL_NONE
}

#[derive(Debug, Clone)]
enum CompiledPredicate {
    EqStr {
        capture_id: u32,
        value: String,
        invert: bool,
        all_quantified: bool,
    },
    EqCapture {
        cap_a: u32,
        cap_b: u32,
        invert: bool,
    },
    AnyOf {
        capture_id: u32,
        values: HashSet<String>,
        invert: bool,
    },
    Regex {
        capture_id: u32,
        regex: CompiledRegex,
        invert: bool,
        all_quantified: bool,
    },
    AlwaysFalse,
}

#[derive(Clone, Copy)]
enum PredArg<'a> {
    Capture(u32),
    Str(&'a str),
}

fn query_string_for_id<'a>(api: &TsApi, query: *const TSQuery, value_id: u32) -> &'a str {
    let mut len: u32 = 0;
    // SAFETY: `query` is a valid `TSQuery` and `value_id` originates from `TSQueryPredicateStep`
    // produced by `ts_query_predicates_for_pattern` on the same `query`.
    let ptr = unsafe { (api.ts_query_string_value_for_id)(query, value_id, &mut len) };
    if ptr.is_null() {
        return "";
    }
    // SAFETY: `ptr` points to `len` valid UTF-8 bytes owned by `query`.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len as usize) };
    std::str::from_utf8(bytes).unwrap_or("")
}

fn compile_pattern_predicates(
    api: &TsApi,
    query: *const TSQuery,
    pattern_index: u32,
) -> Vec<CompiledPredicate> {
    let mut step_count: u32 = 0;
    // SAFETY: `query` is non-null and `pattern_index < ts_query_pattern_count(query)`.
    let steps_ptr =
        unsafe { (api.ts_query_predicates_for_pattern)(query, pattern_index, &mut step_count) };
    if steps_ptr.is_null() || step_count == 0 {
        return Vec::new();
    }
    // SAFETY: `steps_ptr` points to `step_count` valid `TSQueryPredicateStep` elements owned by `query`.
    let steps = unsafe { std::slice::from_raw_parts(steps_ptr, step_count as usize) };

    let mut out = Vec::new();
    let mut cur = Vec::new();
    for step in steps {
        match step.step_type {
            TS_PREDICATE_STEP_CAPTURE => cur.push(PredArg::Capture(step.value_id)),
            TS_PREDICATE_STEP_STRING => {
                cur.push(PredArg::Str(query_string_for_id(api, query, step.value_id)));
            }
            TS_PREDICATE_STEP_DONE => {
                if let Some(pred) = build_single_predicate(&cur) {
                    out.push(pred);
                }
                cur.clear();
            }
            _ => {}
        }
    }
    out
}

fn build_single_predicate(args: &[PredArg<'_>]) -> Option<CompiledPredicate> {
    let PredArg::Str(raw_op) = *args.first()? else {
        return None;
    };
    let op = raw_op.strip_prefix('#').unwrap_or(raw_op);
    match op {
        "eq?" | "not-eq?" | "any-eq?" | "any-not-eq?" => {
            let invert = op.contains("not-");
            let all_quantified = !op.starts_with("any-");
            let PredArg::Capture(cap_a) = *args.get(1)? else {
                return None;
            };
            match *args.get(2)? {
                PredArg::Str(val) => Some(CompiledPredicate::EqStr {
                    capture_id: cap_a,
                    value: val.to_string(),
                    invert,
                    all_quantified,
                }),
                PredArg::Capture(cap_b) => Some(CompiledPredicate::EqCapture {
                    cap_a,
                    cap_b,
                    invert,
                }),
            }
        }
        "any-of?" | "not-any-of?" => {
            let invert = op.starts_with("not-");
            let PredArg::Capture(capture_id) = *args.get(1)? else {
                return None;
            };
            let mut values = HashSet::with_capacity(args.len().saturating_sub(2));
            for arg in &args[2..] {
                if let PredArg::Str(s) = *arg {
                    values.insert(s.to_string());
                }
            }
            Some(CompiledPredicate::AnyOf {
                capture_id,
                values,
                invert,
            })
        }
        "match?" | "not-match?" | "any-match?" | "any-not-match?" | "vim-match?"
        | "not-vim-match?" => {
            let invert = op.contains("not-");
            let all_quantified = !op.starts_with("any-");
            let PredArg::Capture(capture_id) = *args.get(1)? else {
                return None;
            };
            let PredArg::Str(pat) = *args.get(2)? else {
                return None;
            };
            let regex = CompiledRegex::compile(pat)?;
            Some(CompiledPredicate::Regex {
                capture_id,
                regex,
                invert,
                all_quantified,
            })
        }
        "lua-match?" | "not-lua-match?" | "any-lua-match?" | "any-not-lua-match?" => {
            let invert = op.contains("not-");
            let all_quantified = !op.starts_with("any-");
            let PredArg::Capture(capture_id) = *args.get(1)? else {
                return None;
            };
            let PredArg::Str(pat) = *args.get(2)? else {
                return None;
            };
            let regex = CompiledRegex::compile_lua(pat)?;
            Some(CompiledPredicate::Regex {
                capture_id,
                regex,
                invert,
                all_quantified,
            })
        }
        "is?" => {
            if let Some(PredArg::Str("local")) = args.get(1) {
                Some(CompiledPredicate::AlwaysFalse)
            } else {
                None
            }
        }
        // `#is-not? local`, `#set!`, `#offset!`, etc. do not reject matches
        _ => None,
    }
}

fn node_text_slice<'a>(api: &TsApi, node: TSNode, source: &'a str) -> &'a str {
    // SAFETY: `node` comes from an active `TSTree` parsed from `source`.
    let sb = unsafe { (api.ts_node_start_byte)(node) } as usize;
    // SAFETY: `node` comes from an active `TSTree` parsed from `source`.
    let eb = unsafe { (api.ts_node_end_byte)(node) } as usize;
    if sb <= eb && eb <= source.len() && source.is_char_boundary(sb) && source.is_char_boundary(eb)
    {
        &source[sb..eb]
    } else {
        ""
    }
}

fn evaluate_match_predicates(
    api: &TsApi,
    preds: &[CompiledPredicate],
    captures: &[TSQueryCapture],
    source: &str,
) -> bool {
    if preds.is_empty() {
        return true;
    }
    for pred in preds {
        match pred {
            CompiledPredicate::AlwaysFalse => return false,
            CompiledPredicate::EqStr {
                capture_id,
                value,
                invert,
                all_quantified,
            } => {
                let mut matched_nodes = captures.iter().filter(|c| c.index == *capture_id);
                let Some(first) = matched_nodes.next() else {
                    continue;
                };
                let check = |cap: &TSQueryCapture| {
                    let eq = node_text_slice(api, cap.node, source) == value;
                    if *invert { !eq } else { eq }
                };
                let ok = if *all_quantified {
                    check(first) && matched_nodes.all(check)
                } else {
                    check(first) || matched_nodes.any(check)
                };
                if !ok {
                    return false;
                }
            }
            CompiledPredicate::EqCapture {
                cap_a,
                cap_b,
                invert,
            } => {
                let Some(ca) = captures.iter().find(|c| c.index == *cap_a) else {
                    continue;
                };
                let Some(cb) = captures.iter().find(|c| c.index == *cap_b) else {
                    continue;
                };
                let eq =
                    node_text_slice(api, ca.node, source) == node_text_slice(api, cb.node, source);
                let ok = if *invert { !eq } else { eq };
                if !ok {
                    return false;
                }
            }
            CompiledPredicate::AnyOf {
                capture_id,
                values,
                invert,
            } => {
                for cap in captures.iter().filter(|c| c.index == *capture_id) {
                    let contained = values.contains(node_text_slice(api, cap.node, source));
                    let ok = if *invert { !contained } else { contained };
                    if !ok {
                        return false;
                    }
                }
            }
            CompiledPredicate::Regex {
                capture_id,
                regex,
                invert,
                all_quantified,
            } => {
                let mut matched_nodes = captures.iter().filter(|c| c.index == *capture_id);
                let Some(first) = matched_nodes.next() else {
                    continue;
                };
                let check = |cap: &TSQueryCapture| {
                    let hit = regex.is_match(node_text_slice(api, cap.node, source));
                    if *invert { !hit } else { hit }
                };
                let ok = if *all_quantified {
                    check(first) && matched_nodes.all(check)
                } else {
                    check(first) || matched_nodes.any(check)
                };
                if !ok {
                    return false;
                }
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// 6. Loaded Grammar & Query Bundle + Resilient Compilation
// ---------------------------------------------------------------------------

struct LoadedLanguage {
    _grammar_handle: *mut c_void,
    language_ptr: *const TSLanguage,
    abi_version: i64,
    query_ptr: Option<*mut TSQuery>,
    capture_groups: Vec<u8>,
    capture_names: Vec<String>,
    pattern_predicates: Vec<Vec<CompiledPredicate>>,
}

// SAFETY: `language_ptr` is a static `TSLanguage` descriptor in a `dlopen`ed grammar `.so`
// (never unloaded), and `query_ptr` (`TSQuery`) is immutable after construction and only read
// via thread-local / fresh `TSQueryCursor`s.
unsafe impl Send for LoadedLanguage {}
// SAFETY: `LoadedLanguage` is never mutated after construction; `TSLanguage` and `TSQuery` are
// read-only tables safe to share across threads when cursors/parsers are not shared concurrently.
unsafe impl Sync for LoadedLanguage {}

impl Drop for LoadedLanguage {
    fn drop(&mut self) {
        if let (Some(api), Some(q)) = (ts_api(), self.query_ptr)
            && !q.is_null()
        {
            // SAFETY: `q` was allocated by `ts_query_new` from `api` and is uniquely owned here.
            unsafe { (api.ts_query_delete)(q) };
        }
    }
}

fn compile_ts_query_resilient(
    api: &TsApi,
    language_ptr: *const TSLanguage,
    initial_scm: &str,
) -> Option<*mut TSQuery> {
    let mut cur_scm = initial_scm.to_string();
    for _ in 0..256 {
        let Ok(src_len) = u32::try_from(cur_scm.len()) else {
            return None;
        };
        if cur_scm.trim().is_empty() {
            return None;
        }
        let mut err_offset: u32 = 0;
        let mut err_type: u32 = 0;
        // SAFETY: `language_ptr` is a verified `TSLanguage*`, `cur_scm` points to `src_len` valid
        // UTF-8 bytes, and `err_offset` / `err_type` are valid writable out-pointers.
        let q = unsafe {
            (api.ts_query_new)(
                language_ptr,
                cur_scm.as_ptr().cast::<c_char>(),
                src_len,
                &mut err_offset,
                &mut err_type,
            )
        };
        if !q.is_null() {
            return Some(q);
        }
        cur_scm = strip_failing_top_level_pattern(&cur_scm, err_offset as usize)?;
    }
    None
}

#[cfg(unix)]
fn load_ts_language(api: &TsApi, norm_lang: &str) -> Option<Arc<LoadedLanguage>> {
    let sym_name = format!("tree_sitter_{}", norm_lang.replace('-', "_"));
    let c_sym = CString::new(sym_name).ok()?;

    let mut chosen_handle: *mut c_void = ptr::null_mut();
    let mut chosen_lang: *const TSLanguage = ptr::null();

    for so_path in candidate_grammar_paths(norm_lang) {
        if !so_path.is_file() {
            continue;
        }
        let Some(path_str) = so_path.to_str() else {
            continue;
        };
        let Ok(c_path) = CString::new(path_str) else {
            continue;
        };
        // SAFETY: `c_path` is a NUL-terminated filesystem path to an existing `.so` file.
        let h = unsafe { dlopen(c_path.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
        if h.is_null() {
            continue;
        }
        // SAFETY: `h` is a valid `dlopen` handle and `c_sym` is NUL-terminated.
        let raw_fn = unsafe { dlsym(h, c_sym.as_ptr()) };
        if raw_fn.is_null() {
            continue;
        }
        // SAFETY: Tree-sitter grammar entrypoints have signature `extern "C" fn() -> *const TSLanguage`.
        let lang_fn: unsafe extern "C" fn() -> *const TSLanguage =
            unsafe { std::mem::transmute(raw_fn) };
        // SAFETY: Invoking the grammar's zero-argument descriptor accessor.
        let lang_ptr = unsafe { lang_fn() };
        if lang_ptr.is_null() {
            continue;
        }

        // Verify ABI compatibility via `ts_parser_set_language`.
        // SAFETY: Creating a temporary `TSParser`, testing `ts_parser_set_language`, and deleting it.
        let compat = unsafe {
            let parser = (api.ts_parser_new)();
            if parser.is_null() {
                false
            } else {
                let ok = (api.ts_parser_set_language)(parser, lang_ptr);
                (api.ts_parser_delete)(parser);
                ok
            }
        };
        if compat {
            chosen_handle = h;
            chosen_lang = lang_ptr;
            break;
        }
    }

    if chosen_lang.is_null() {
        return None;
    }

    let abi_version = match api.ts_language_abi_version {
        // SAFETY: `chosen_lang` was accepted by `ts_parser_set_language`.
        Some(f) => unsafe { i64::from(f(chosen_lang)) },
        None => TS_ABI_FALLBACK_VERSION,
    };

    let query_ptr = load_highlights_query_source(norm_lang)
        .and_then(|scm| compile_ts_query_resilient(api, chosen_lang, &scm));

    let mut capture_groups = Vec::new();
    let mut capture_names = Vec::new();
    let mut pattern_predicates = Vec::new();

    if let Some(q) = query_ptr {
        // SAFETY: `q` is a valid `TSQuery*` returned by `ts_query_new`.
        let cap_cnt = unsafe { (api.ts_query_capture_count)(q) };
        capture_groups.reserve(cap_cnt as usize);
        capture_names.reserve(cap_cnt as usize);
        for cid in 0..cap_cnt {
            let mut len: u32 = 0;
            // SAFETY: `cid < cap_cnt` on valid `q`.
            let name_ptr = unsafe { (api.ts_query_capture_name_for_id)(q, cid, &mut len) };
            let cap_name = if name_ptr.is_null() {
                ""
            } else {
                // SAFETY: `name_ptr` points to `len` UTF-8 bytes owned by `q`.
                let slice =
                    unsafe { std::slice::from_raw_parts(name_ptr.cast::<u8>(), len as usize) };
                std::str::from_utf8(slice).unwrap_or("")
            };
            capture_groups.push(map_capture_name_to_hl_group(cap_name));
            capture_names.push(cap_name.to_string());
        }

        // SAFETY: `q` is a valid `TSQuery*`.
        let pat_cnt = unsafe { (api.ts_query_pattern_count)(q) };
        pattern_predicates.reserve(pat_cnt as usize);
        for pid in 0..pat_cnt {
            pattern_predicates.push(compile_pattern_predicates(api, q, pid));
        }
    }

    Some(Arc::new(LoadedLanguage {
        _grammar_handle: chosen_handle,
        language_ptr: chosen_lang,
        abi_version,
        query_ptr,
        capture_groups,
        capture_names,
        pattern_predicates,
    }))
}

#[cfg(not(unix))]
fn load_ts_language(_api: &TsApi, _norm_lang: &str) -> Option<Arc<LoadedLanguage>> {
    None
}

static LANGUAGE_CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<LoadedLanguage>>>>> =
    OnceLock::new();

fn get_loaded_language(norm_lang: &str) -> Option<Arc<LoadedLanguage>> {
    let api = ts_api()?;
    let cache = LANGUAGE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(existing) = cache
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(norm_lang)
        .cloned()
    {
        return existing;
    }

    let loaded = load_ts_language(api, norm_lang).or_else(|| {
        // Fallback `tsx` -> `typescript` if `tsx.so` is absent
        if norm_lang == "tsx" {
            load_ts_language(api, "typescript")
        } else {
            None
        }
    });
    cache
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(norm_lang.to_string(), loaded.clone());
    loaded
}

// ---------------------------------------------------------------------------
// 7. LRU-32 `TSTree` Cache & Highlight Execution (with Fallback Node Walker)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Hash)]
struct DocTreeKey {
    doc_id: i64,
    version: i64,
    lang: String,
}

struct CachedDocTree {
    key: DocTreeKey,
    tree_ptr: *mut TSTree,
    source: Arc<String>,
    line_starts: Arc<Vec<usize>>,
}

// SAFETY: `CachedDocTree` is only accessed while holding `DOC_TREE_LRU` mutex or after cloning
// `Arc<String>` / `Arc<Vec<usize>>`; `TSTree` queries run under the cache lock or on a copied tree.
unsafe impl Send for CachedDocTree {}

impl Drop for CachedDocTree {
    fn drop(&mut self) {
        if let Some(api) = ts_api()
            && !self.tree_ptr.is_null()
        {
            // SAFETY: `self.tree_ptr` was allocated by `ts_parser_parse_string` and is uniquely
            // owned by this cache entry; dropping calls `ts_tree_delete` exactly once.
            unsafe { (api.ts_tree_delete)(self.tree_ptr) };
            self.tree_ptr = ptr::null_mut();
        }
    }
}

struct DocTreeLru {
    entries: VecDeque<CachedDocTree>,
}

impl DocTreeLru {
    fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(DOC_TREE_LRU_CAP),
        }
    }

    fn get_idx(&mut self, key: &DocTreeKey) -> Option<usize> {
        let pos = self.entries.iter().position(|e| &e.key == key)?;
        if pos + 1 < self.entries.len()
            && let Some(entry) = self.entries.remove(pos)
        {
            self.entries.push_back(entry);
            return Some(self.entries.len() - 1);
        }
        Some(pos)
    }

    fn insert(&mut self, entry: CachedDocTree) -> usize {
        // Evict stale versions of the same `(doc_id, lang)` first so edits don't churn unrelated docs
        self.entries
            .retain(|e| !(e.key.doc_id == entry.key.doc_id && e.key.lang == entry.key.lang));
        while self.entries.len() >= DOC_TREE_LRU_CAP {
            // `pop_front` drops the oldest `CachedDocTree`, invoking `ts_tree_delete` in `Drop`.
            let _ = self.entries.pop_front();
        }
        self.entries.push_back(entry);
        self.entries.len() - 1
    }
}

static DOC_TREE_LRU: OnceLock<Mutex<DocTreeLru>> = OnceLock::new();

fn build_source_and_line_starts(
    chunks: &[Arc<Vec<String>>],
    total_lines: usize,
) -> (String, Vec<usize>) {
    let total_bytes: usize = chunks
        .iter()
        .map(|c| c.iter().map(String::len).sum::<usize>())
        .sum::<usize>()
        + total_lines.saturating_sub(1);
    let mut source = String::with_capacity(total_bytes);
    let mut starts = Vec::with_capacity(total_lines + 1);
    let mut first = true;
    for chunk in chunks {
        for line in chunk.iter() {
            if !first {
                source.push('\n');
            }
            first = false;
            starts.push(source.len());
            source.push_str(line);
        }
    }
    if starts.is_empty() {
        starts.push(0);
    }
    starts.push(source.len() + 1); // sentinel for uniform `line_slice` calculation
    (source, starts)
}

#[inline]
fn line_slice_at<'a>(source: &'a str, line_starts: &[usize], row: usize) -> &'a str {
    let start = line_starts[row];
    let next_start = line_starts[row + 1];
    let end = next_start.saturating_sub(1).min(source.len());
    if start <= end {
        &source[start..end]
    } else {
        ""
    }
}

#[inline]
fn byte_offset_to_ts_point(line_starts: &[usize], byte_offset: usize) -> TSPoint {
    let actual_lines = line_starts.len().saturating_sub(1);
    if actual_lines == 0 {
        return TSPoint {
            row: 0,
            column: byte_offset as u32,
        };
    }
    let row = line_starts[..actual_lines]
        .partition_point(|&start| start <= byte_offset)
        .saturating_sub(1);
    let col_bytes = byte_offset.saturating_sub(line_starts[row]);
    TSPoint {
        row: row as u32,
        column: col_bytes as u32,
    }
}

fn compute_ts_input_edit(
    old_src: &str,
    old_starts: &[usize],
    new_src: &str,
    new_starts: &[usize],
) -> Option<TSInputEdit> {
    if old_src.len() > MAX_PARSE_SOURCE_BYTES || new_src.len() > MAX_PARSE_SOURCE_BYTES {
        return None;
    }
    let old_b = old_src.as_bytes();
    let new_b = new_src.as_bytes();
    let min_len = old_b.len().min(new_b.len());
    let mut prefix = 0usize;
    while prefix < min_len && old_b[prefix] == new_b[prefix] {
        prefix += 1;
    }
    while prefix > 0 && !old_src.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let old_rem = old_b.len() - prefix;
    let new_rem = new_b.len() - prefix;
    let min_rem = old_rem.min(new_rem);
    let mut suffix = 0usize;
    while suffix < min_rem && old_b[old_b.len() - 1 - suffix] == new_b[new_b.len() - 1 - suffix] {
        suffix += 1;
    }
    while suffix > 0 && !old_src.is_char_boundary(old_b.len() - suffix) {
        suffix -= 1;
    }
    let old_end = old_b.len() - suffix;
    let new_end = new_b.len() - suffix;
    Some(TSInputEdit {
        start_byte: u32::try_from(prefix).ok()?,
        old_end_byte: u32::try_from(old_end).ok()?,
        new_end_byte: u32::try_from(new_end).ok()?,
        start_point: byte_offset_to_ts_point(old_starts, prefix),
        old_end_point: byte_offset_to_ts_point(old_starts, old_end),
        new_end_point: byte_offset_to_ts_point(new_starts, new_end),
    })
}

fn parse_source_to_tree(api: &TsApi, lang: &LoadedLanguage, source: &str) -> Option<*mut TSTree> {
    parse_source_to_tree_with_old(api, lang, source, ptr::null())
}

fn parse_source_to_tree_with_old(
    api: &TsApi,
    lang: &LoadedLanguage,
    source: &str,
    old_tree: *const TSTree,
) -> Option<*mut TSTree> {
    if source.len() > MAX_PARSE_SOURCE_BYTES {
        return None;
    }
    let src_len = u32::try_from(source.len()).ok()?;
    // SAFETY: Creates an owned `TSParser`, assigns verified `lang.language_ptr`, parses `source`
    // (`src_len` valid UTF-8 bytes) optionally reusing `old_tree`, and deletes `TSParser`.
    unsafe {
        let parser = (api.ts_parser_new)();
        if parser.is_null() {
            return None;
        }
        if !(api.ts_parser_set_language)(parser, lang.language_ptr) {
            (api.ts_parser_delete)(parser);
            return None;
        }
        let tree = (api.ts_parser_parse_string)(
            parser,
            old_tree,
            source.as_ptr().cast::<c_char>(),
            src_len,
        );
        (api.ts_parser_delete)(parser);
        if tree.is_null() { None } else { Some(tree) }
    }
}

#[derive(Clone, Copy)]
struct CellWinner {
    hl_group: u8,
    span_len: u32,
    pattern_idx: u32,
    seq: u32,
}

impl CellWinner {
    const EMPTY: Self = Self {
        hl_group: HL_NONE,
        span_len: u32::MAX,
        pattern_idx: 0,
        seq: 0,
    };

    #[inline]
    fn beats(self, existing: Self) -> bool {
        if existing.hl_group == HL_NONE {
            return true;
        }
        if self.span_len != existing.span_len {
            // More specific inner AST node (strictly smaller byte span) layers over outer parent node
            self.span_len < existing.span_len
        } else if self.pattern_idx != existing.pattern_idx {
            // On the same AST node span, later pattern in `highlights.scm` overrides earlier pattern
            self.pattern_idx > existing.pattern_idx
        } else {
            self.seq >= existing.seq
        }
    }
}

struct RowHighlightBuffer {
    line_char_len: usize,
    byte_to_char: Option<Vec<u32>>,
    cells: Vec<CellWinner>,
}

impl RowHighlightBuffer {
    fn from_line(line: &str) -> Self {
        if line.is_ascii() {
            let clen = line.len().min(MAX_HIGHLIGHT_ROW_CHARS);
            Self {
                line_char_len: clen,
                byte_to_char: None,
                cells: vec![CellWinner::EMPTY; clen],
            }
        } else {
            let mut b2c = vec![0u32; line.len() + 1];
            let mut char_idx = 0u32;
            for (b_idx, ch) in line.char_indices() {
                let end_b = b_idx + ch.len_utf8();
                for slot in &mut b2c[b_idx..end_b] {
                    *slot = char_idx;
                }
                char_idx += 1;
            }
            b2c[line.len()] = char_idx;
            let clen = (char_idx as usize).min(MAX_HIGHLIGHT_ROW_CHARS);
            Self {
                line_char_len: clen,
                byte_to_char: Some(b2c),
                cells: vec![CellWinner::EMPTY; clen],
            }
        }
    }

    #[inline]
    fn byte_to_char_col(&self, line_byte_len: usize, byte_col: usize) -> usize {
        let clamped = byte_col.min(line_byte_len);
        let c = match &self.byte_to_char {
            Some(map) => map[clamped] as usize,
            None => clamped,
        };
        c.min(self.line_char_len)
    }

    fn apply_byte_span(
        &mut self,
        line_byte_len: usize,
        b_start: usize,
        b_end: usize,
        cand: CellWinner,
    ) {
        if b_end <= b_start || self.line_char_len == 0 {
            return;
        }
        let c_start = self.byte_to_char_col(line_byte_len, b_start);
        let c_end = self.byte_to_char_col(line_byte_len, b_end);
        if c_end <= c_start {
            return;
        }
        for cell in &mut self.cells[c_start..c_end] {
            if cand.beats(*cell) {
                *cell = cand;
            }
        }
    }

    fn into_spans(self) -> Vec<TsHighlightSpan> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < self.cells.len() {
            let grp = self.cells[i].hl_group;
            if grp == HL_NONE || grp == HL_EMBEDDED_CLEAR {
                i += 1;
                continue;
            }
            let start = i;
            i += 1;
            while i < self.cells.len() && self.cells[i].hl_group == grp {
                i += 1;
            }
            out.push(TsHighlightSpan {
                start_col: start as i64,
                end_col: i as i64,
                hl_group: i64::from(grp),
            });
        }
        out
    }
}

fn classify_fallback_node(kind: &str, is_named: bool) -> u8 {
    if is_named {
        if kind.contains("doc_comment") {
            return HL_DOC_COMMENT;
        }
        if kind.contains("comment") {
            return HL_COMMENT;
        }
        if kind.contains("escape") || kind.contains("regex") {
            return HL_ESCAPE_REGEX;
        }
        if kind.contains("string") || kind.contains("char") || kind.contains("heredoc") {
            return HL_STRING;
        }
        if kind.contains("int")
            || kind.contains("float")
            || kind.contains("number")
            || kind.contains("bool")
            || matches!(kind, "true" | "false" | "null" | "nil" | "none")
        {
            return HL_NUMBER_BOOL;
        }
        if kind == "primitive_type" || kind == "builtin_type" {
            return HL_TYPE_BUILTIN;
        }
        if kind.contains("type") {
            return HL_TYPE;
        }
        if kind.contains("field") || kind.contains("property") {
            return HL_PROPERTY_FIELD;
        }
        if kind.contains("parameter") {
            return HL_PARAMETER;
        }
        if kind.contains("namespace") || kind.contains("module") || kind.contains("label") {
            return HL_NAMESPACE_LABEL;
        }
        if kind.contains("attribute") || kind.contains("preproc") || kind.contains("macro") {
            return HL_MACRO_ATTR;
        }
        if kind.contains("tag") || kind.contains("heading") {
            return HL_TAG_HEADING;
        }
        HL_NONE
    } else if matches!(
        kind,
        "if" | "else"
            | "elif"
            | "for"
            | "while"
            | "loop"
            | "do"
            | "match"
            | "switch"
            | "case"
            | "return"
            | "break"
            | "continue"
            | "try"
            | "catch"
            | "finally"
            | "throw"
            | "raise"
            | "import"
            | "export"
            | "from"
            | "use"
            | "yield"
            | "await"
    ) {
        HL_KEYWORD_CONTROL
    } else if matches!(
        kind,
        "true" | "false" | "null" | "nil" | "None" | "True" | "False"
    ) {
        HL_NUMBER_BOOL
    } else if kind.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_') && !kind.is_empty() {
        HL_KEYWORD
    } else if !kind.is_empty()
        && kind.bytes().all(|b| {
            matches!(
                b,
                b'+' | b'-'
                    | b'*'
                    | b'/'
                    | b'%'
                    | b'='
                    | b'!'
                    | b'<'
                    | b'>'
                    | b'&'
                    | b'|'
                    | b'^'
                    | b'~'
                    | b'?'
                    | b':'
            )
        })
    {
        HL_OPERATOR
    } else {
        HL_NONE
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_node_to_row_buffers(
    api: &TsApi,
    node: TSNode,
    hl_group: u8,
    pattern_idx: u32,
    seq: u32,
    source: &str,
    line_starts: &[usize],
    total_lines: usize,
    start_row: usize,
    end_row: usize,
    row_bufs: &mut [RowHighlightBuffer],
) {
    if hl_group == HL_NONE {
        return;
    }
    // SAFETY: `node` belongs to the active `TSTree`.
    let sb = unsafe { (api.ts_node_start_byte)(node) };
    // SAFETY: `node` belongs to the active `TSTree`.
    let eb = unsafe { (api.ts_node_end_byte)(node) };
    if eb <= sb {
        return;
    }
    let span_len = eb - sb;
    // SAFETY: `node` belongs to the active `TSTree`.
    let sp = unsafe { (api.ts_node_start_point)(node) };
    // SAFETY: `node` belongs to the active `TSTree`.
    let ep = unsafe { (api.ts_node_end_point)(node) };

    let r_lo = (sp.row as usize).max(start_row);
    let r_hi = (ep.row as usize)
        .min(end_row)
        .min(total_lines.saturating_sub(1));
    if r_lo > r_hi {
        return;
    }

    let cand = CellWinner {
        hl_group,
        span_len,
        pattern_idx,
        seq,
    };

    for r in r_lo..=r_hi {
        let line = line_slice_at(source, line_starts, r);
        let b_start = if r == sp.row as usize {
            (sp.column as usize).min(line.len())
        } else {
            0
        };
        let b_end = if r == ep.row as usize {
            (ep.column as usize).min(line.len())
        } else {
            line.len()
        };
        row_bufs[r - start_row].apply_byte_span(line.len(), b_start, b_end, cand);
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_fallback_nodes(
    api: &TsApi,
    node: TSNode,
    seq: &mut u32,
    source: &str,
    line_starts: &[usize],
    total_lines: usize,
    start_row: usize,
    end_row: usize,
    row_bufs: &mut [RowHighlightBuffer],
) {
    // SAFETY: `node` belongs to the active `TSTree`.
    let sp = unsafe { (api.ts_node_start_point)(node) };
    // SAFETY: `node` belongs to the active `TSTree`.
    let ep = unsafe { (api.ts_node_end_point)(node) };
    if (ep.row as usize) < start_row || (sp.row as usize) > end_row {
        return;
    }

    // SAFETY: `ts_node_type` returns a static NUL-terminated string owned by `TSLanguage`.
    let kind_str = unsafe {
        let p = (api.ts_node_type)(node);
        if p.is_null() {
            ""
        } else {
            CStr::from_ptr(p).to_str().unwrap_or("")
        }
    };
    // SAFETY: `node` is non-null.
    let is_named = unsafe { (api.ts_node_is_named)(node) };
    // SAFETY: `node` is non-null.
    let child_cnt = unsafe { (api.ts_node_child_count)(node) };

    let grp = classify_fallback_node(kind_str, is_named);
    if grp != HL_NONE && (child_cnt == 0 || matches!(grp, HL_STRING | HL_COMMENT | HL_DOC_COMMENT))
    {
        *seq = seq.wrapping_add(1);
        apply_node_to_row_buffers(
            api,
            node,
            grp,
            0,
            *seq,
            source,
            line_starts,
            total_lines,
            start_row,
            end_row,
            row_bufs,
        );
        if matches!(grp, HL_COMMENT | HL_DOC_COMMENT) {
            return;
        }
    }

    for ci in 0..child_cnt {
        // SAFETY: `ci < child_cnt` on valid `node`.
        let child = unsafe { (api.ts_node_child)(node, ci) };
        if grp == HL_STRING {
            // SAFETY: `child` is a valid `TSNode`.
            let child_named = unsafe { (api.ts_node_is_named)(child) };
            if !child_named {
                continue;
            }
        }
        walk_fallback_nodes(
            api,
            child,
            seq,
            source,
            line_starts,
            total_lines,
            start_row,
            end_row,
            row_bufs,
        );
    }
}

fn highlight_tree_rows(
    api: &TsApi,
    lang: &LoadedLanguage,
    tree_ptr: *const TSTree,
    source: &str,
    line_starts: &[usize],
    start_row: usize,
    end_row: usize,
) -> Vec<Vec<TsHighlightSpan>> {
    let total_lines = line_starts.len().saturating_sub(1);
    let requested_len = (end_row - start_row + 1).min(MAX_NESSA_LIST_ITEMS);
    let clamped_end = start_row + requested_len - 1;
    let active_end = clamped_end.min(total_lines.saturating_sub(1));

    let mut row_bufs = Vec::with_capacity(requested_len);
    for r in start_row..=clamped_end {
        if r < total_lines {
            row_bufs.push(RowHighlightBuffer::from_line(line_slice_at(
                source,
                line_starts,
                r,
            )));
        } else {
            row_bufs.push(RowHighlightBuffer::from_line(""));
        }
    }

    if total_lines == 0 || start_row >= total_lines {
        return row_bufs
            .into_iter()
            .map(RowHighlightBuffer::into_spans)
            .collect();
    }

    // SAFETY: `tree_ptr` is a valid non-null `TSTree*`.
    let root = unsafe { (api.ts_tree_root_node)(tree_ptr) };

    if let Some(q) = lang.query_ptr {
        // SAFETY: Allocates a fresh `TSQueryCursor`, bounds point range to `[start_row, active_end + 1]`,
        // iterates all `TSQueryMatch`es, and frees the cursor before returning.
        unsafe {
            let cursor = (api.ts_query_cursor_new)();
            if !cursor.is_null() {
                let p_start = TSPoint {
                    row: start_row as u32,
                    column: 0,
                };
                let p_end = TSPoint {
                    row: (active_end as u32).saturating_add(1),
                    column: 0,
                };
                let _ = (api.ts_query_cursor_set_point_range)(cursor, p_start, p_end);
                (api.ts_query_cursor_exec)(cursor, q, root);

                let mut m = TSQueryMatch {
                    id: 0,
                    pattern_index: 0,
                    capture_count: 0,
                    captures: ptr::null(),
                };
                let mut seq: u32 = 0;

                while (api.ts_query_cursor_next_match)(cursor, &mut m) {
                    if m.capture_count == 0 || m.captures.is_null() {
                        continue;
                    }
                    let caps = std::slice::from_raw_parts(m.captures, usize::from(m.capture_count));
                    let pat_idx = usize::from(m.pattern_index);
                    if let Some(preds) = lang.pattern_predicates.get(pat_idx)
                        && !evaluate_match_predicates(api, preds, caps, source)
                    {
                        continue;
                    }
                    for cap in caps {
                        let grp = lang
                            .capture_groups
                            .get(cap.index as usize)
                            .copied()
                            .unwrap_or(HL_NONE);
                        if grp != HL_NONE {
                            seq = seq.wrapping_add(1);
                            apply_node_to_row_buffers(
                                api,
                                cap.node,
                                grp,
                                u32::from(m.pattern_index),
                                seq,
                                source,
                                line_starts,
                                total_lines,
                                start_row,
                                active_end,
                                &mut row_bufs,
                            );
                        }
                    }
                }

                (api.ts_query_cursor_delete)(cursor);
            }
        }
    } else {
        let mut seq: u32 = 0;
        walk_fallback_nodes(
            api,
            root,
            &mut seq,
            source,
            line_starts,
            total_lines,
            start_row,
            active_end,
            &mut row_bufs,
        );
    }

    row_bufs
        .into_iter()
        .map(RowHighlightBuffer::into_spans)
        .collect()
}

// ---------------------------------------------------------------------------
// 8. Rich Multi-Line Stateful Highlighter for Nessa (`"nessa"` / `"ns"`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum NessaFrame {
    /// Inside `"..."` string literal; `interp_brace_depth == 0` means string body,
    /// `> 0` means inside `{ ... }` interpolated Nessa expression.
    String { interp_brace_depth: u32 },
    /// Inside `'{ ... }'` macro content block with nested brace depth.
    MacroBlock { brace_depth: u32 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NessaLexState {
    /// Nested `{- ... -}` block comment depth (`0` = outside block comment).
    block_comment_depth: u32,
    /// Stack of active multi-line string / interpolation / macro frames.
    frames: Vec<NessaFrame>,
    /// True right after `fn`/`def`/`effect` keyword before parameter `(` opens.
    expect_fn_name: bool,
    /// True right after `fn name` before `(` opens.
    expect_fn_params_open: bool,
    /// Paren nesting depth of an active `fn(...)` signature (`0` = not in fn params).
    fn_param_paren_depth: u32,
    /// True at start of a parameter slot (after `(` or `,` at depth 1 of fn params).
    at_param_slot_start: bool,
    /// True inside `|p1: T1, p2: T2|` closure parameter list.
    in_closure_params: bool,
    at_closure_param_start: bool,
    /// True right after `use` or `@` module prefix.
    after_at_prefix: bool,
    /// True right after `#` effect prefix (`#EffectName` or `#[e1, e2]`).
    after_hash_prefix: bool,
    in_hash_effect_brackets: bool,
    /// True right after `.` member access.
    after_dot: bool,
}

fn is_nessa_control_kw(w: &str) -> bool {
    matches!(
        w,
        "if" | "else"
            | "when"
            | "match"
            | "matches"
            | "while"
            | "for"
            | "in"
            | "do"
            | "case"
            | "return"
            | "break"
            | "continue"
            | "defer"
            | "error"
            | "catch"
            | "handles"
            | "resume"
            | "reset"
            | "shift"
            | "await"
            | "use"
            | "mod"
    )
}

fn is_nessa_decl_or_other_kw(w: &str) -> bool {
    matches!(
        w,
        "fn" | "def"
            | "pub"
            | "private"
            | "let"
            | "var"
            | "const"
            | "global"
            | "static"
            | "struct"
            | "enum"
            | "trait"
            | "impl"
            | "extend"
            | "extern"
            | "effect"
            | "async"
            | "atomic"
            | "newtype"
            | "derive"
            | "typealias"
            | "assoc"
            | "where"
            | "as"
            | "is"
            | "and"
            | "or"
            | "not"
            | "lambda"
            | "test"
    )
}

fn is_nessa_builtin_const(w: &str) -> bool {
    matches!(w, "true" | "false" | "null" | "self" | "itself")
}

fn is_nessa_builtin_type(w: &str) -> bool {
    matches!(
        w,
        "i128"
            | "i64"
            | "i32"
            | "i16"
            | "i8"
            | "isize"
            | "u128"
            | "u64"
            | "u32"
            | "u16"
            | "u8"
            | "usize"
            | "f64"
            | "f32"
            | "bool"
            | "char"
            | "str"
            | "String"
            | "Bytes"
            | "List"
            | "Map"
            | "Set"
            | "Unit"
            | "NoReturn"
            | "Type"
            | "Any"
            | "Continuation"
            | "Future"
            | "Self"
    )
}

fn is_screaming_const(w: &str) -> bool {
    if w.len() < 2 {
        return false;
    }
    let bytes = w.as_bytes();
    if !bytes[0].is_ascii_uppercase() {
        return false;
    }
    let mut has_upper = false;
    for &b in bytes {
        if b.is_ascii_uppercase() {
            has_upper = true;
        } else if !b.is_ascii_digit() && b != b'_' {
            return false;
        }
    }
    has_upper
}

fn is_pascal_type(w: &str) -> bool {
    let mut chars = w.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_uppercase() && w.bytes().any(|b| b.is_ascii_lowercase())
}

fn string_has_unescaped_interp_ahead(cs: &[char], from: usize) -> bool {
    let mut i = from;
    while i < cs.len() {
        match cs[i] {
            '"' => return false,
            '\\' => i += 2,
            '{' if cs.get(i + 1) == Some(&'{') => i += 2,
            '{' => return true,
            _ => i += 1,
        }
    }
    false
}

fn closure_pipe_looks_valid(cs: &[char], open_pipe: usize) -> bool {
    let mut j = open_pipe + 1;
    let mut depth = 0i32;
    while j < cs.len() {
        match cs[j] {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '|' if depth == 0 => return true,
            '"' | ';' => return false,
            '-' if cs.get(j + 1) == Some(&'-') => return false,
            _ => {}
        }
        j += 1;
    }
    false
}

fn push_coalesced_span(spans: &mut Vec<TsHighlightSpan>, start: usize, end: usize, grp: u8) {
    if grp == HL_NONE || end <= start {
        return;
    }
    let s = start as i64;
    let e = end as i64;
    let g = i64::from(grp);
    if let Some(last) = spans.last_mut()
        && last.hl_group == g
        && last.end_col == s
    {
        last.end_col = e;
        return;
    }
    spans.push(TsHighlightSpan {
        start_col: s,
        end_col: e,
        hl_group: g,
    });
}

fn highlight_nessa_line(line: &str, state: &mut NessaLexState) -> Vec<TsHighlightSpan> {
    if line.is_empty() {
        return Vec::new();
    }
    let cs: Vec<char> = line.chars().take(MAX_HIGHLIGHT_ROW_CHARS).collect();
    let n = cs.len();
    let mut spans = Vec::new();
    let mut i = 0usize;

    while i < n {
        // 1. Nested multi-line block comment `{- ... -}`
        if state.block_comment_depth > 0 {
            let start = i;
            while i < n {
                if cs[i] == '{' && cs.get(i + 1) == Some(&'-') {
                    state.block_comment_depth += 1;
                    i += 2;
                } else if cs[i] == '-' && cs.get(i + 1) == Some(&'}') {
                    state.block_comment_depth -= 1;
                    i += 2;
                    if state.block_comment_depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            push_coalesced_span(&mut spans, start, i, HL_COMMENT);
            continue;
        }

        // 2. Active macro block `'{ ... }'`
        if let Some(NessaFrame::MacroBlock { brace_depth }) = state.frames.last_mut() {
            let start = i;
            while i < n {
                if cs[i] == '{' {
                    *brace_depth += 1;
                    i += 1;
                } else if cs[i] == '}' {
                    *brace_depth = brace_depth.saturating_sub(1);
                    i += 1;
                    if *brace_depth == 0 {
                        if cs.get(i) == Some(&'\'') {
                            i += 1;
                        }
                        state.frames.pop();
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            push_coalesced_span(&mut spans, start, i, HL_MACRO_ATTR);
            continue;
        }

        // 3. Active string body (`interp_brace_depth == 0`)
        if matches!(
            state.frames.last(),
            Some(NessaFrame::String {
                interp_brace_depth: 0
            })
        ) {
            if cs[i] == '"' {
                push_coalesced_span(&mut spans, i, i + 1, HL_STRING);
                i += 1;
                state.frames.pop();
                continue;
            }
            if cs[i] == '\\' {
                let esc_start = i;
                i += 1;
                if i < n {
                    if cs[i] == 'u' && cs.get(i + 1) == Some(&'{') {
                        i += 2;
                        while i < n && cs[i] != '}' && cs[i] != '"' {
                            i += 1;
                        }
                        if cs.get(i) == Some(&'}') {
                            i += 1;
                        }
                    } else if cs[i] == 'x' {
                        i = (i + 3).min(n);
                    } else {
                        i += 1;
                    }
                }
                push_coalesced_span(&mut spans, esc_start, i, HL_ESCAPE_REGEX);
                continue;
            }
            if cs[i] == '{' && cs.get(i + 1) == Some(&'{') {
                push_coalesced_span(&mut spans, i, i + 2, HL_ESCAPE_REGEX);
                i += 2;
                continue;
            }
            if cs[i] == '}' && cs.get(i + 1) == Some(&'}') {
                push_coalesced_span(&mut spans, i, i + 2, HL_ESCAPE_REGEX);
                i += 2;
                continue;
            }
            if cs[i] == '{' {
                push_coalesced_span(&mut spans, i, i + 1, HL_ESCAPE_REGEX);
                if let Some(NessaFrame::String { interp_brace_depth }) = state.frames.last_mut() {
                    *interp_brace_depth = 1;
                }
                i += 1;
                continue;
            }
            let lit_start = i;
            i += 1;
            while i < n && !matches!(cs[i], '"' | '\\' | '{' | '}') {
                i += 1;
            }
            push_coalesced_span(&mut spans, lit_start, i, HL_STRING);
            if i < n && cs[i] == '}' && cs.get(i + 1) != Some(&'}') {
                push_coalesced_span(&mut spans, i, i + 1, HL_STRING);
                i += 1;
            }
            continue;
        }

        let ch = cs[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }

        // Line comments & format directives: `--%` (directive = 4), `---` (doc = 16), `--` (comment = 15)
        if ch == '-' && cs.get(i + 1) == Some(&'-') {
            let grp = if cs.get(i + 2) == Some(&'%') {
                HL_MACRO_ATTR
            } else if cs.get(i + 2) == Some(&'-') && cs.get(i + 3) != Some(&'-') {
                HL_DOC_COMMENT
            } else {
                HL_COMMENT
            };
            push_coalesced_span(&mut spans, i, n, grp);
            break;
        }

        // Opening nested block comment `{-`
        if ch == '{' && cs.get(i + 1) == Some(&'-') {
            state.block_comment_depth = 1;
            push_coalesced_span(&mut spans, i, i + 2, HL_COMMENT);
            i += 2;
            continue;
        }

        // Interpolation `{` / `}` tracking when inside `{ ... }` of an f-string
        if ch == '{' {
            if let Some(NessaFrame::String { interp_brace_depth }) = state.frames.last_mut()
                && *interp_brace_depth > 0
            {
                *interp_brace_depth += 1;
            }
            state.expect_fn_name = false;
            state.expect_fn_params_open = false;
            state.after_dot = false;
            state.after_at_prefix = false;
            state.after_hash_prefix = false;
            i += 1;
            continue;
        }
        if ch == '}' {
            if let Some(NessaFrame::String { interp_brace_depth }) = state.frames.last_mut()
                && *interp_brace_depth > 0
            {
                *interp_brace_depth -= 1;
                if *interp_brace_depth == 0 {
                    push_coalesced_span(&mut spans, i, i + 1, HL_ESCAPE_REGEX);
                    i += 1;
                    continue;
                }
            }
            state.after_dot = false;
            state.after_at_prefix = false;
            state.after_hash_prefix = false;
            i += 1;
            continue;
        }

        // Opening string literal `"..."`
        if ch == '"' {
            push_coalesced_span(&mut spans, i, i + 1, HL_STRING);
            state.frames.push(NessaFrame::String {
                interp_brace_depth: 0,
            });
            state.after_dot = false;
            state.after_at_prefix = false;
            state.after_hash_prefix = false;
            i += 1;
            // Single-line unterminated strings in Nessa do not bleed into next line unless interpolated
            let _ = string_has_unescaped_interp_ahead(&cs, i);
            continue;
        }

        // Macro `'{ ... }'` or char literal `'x'` / `'\n'`
        if ch == '\'' {
            if cs.get(i + 1) == Some(&'{') {
                state.frames.push(NessaFrame::MacroBlock { brace_depth: 1 });
                push_coalesced_span(&mut spans, i, i + 2, HL_MACRO_ATTR);
                i += 2;
                continue;
            }
            let c_start = i;
            i += 1;
            if i < n && cs[i] == '\\' {
                push_coalesced_span(&mut spans, c_start, i, HL_STRING);
                let esc_s = i;
                i = (i + 2).min(n);
                push_coalesced_span(&mut spans, esc_s, i, HL_ESCAPE_REGEX);
                if cs.get(i) == Some(&'\'') {
                    push_coalesced_span(&mut spans, i, i + 1, HL_STRING);
                    i += 1;
                }
            } else if i + 1 < n && cs[i + 1] == '\'' {
                push_coalesced_span(&mut spans, c_start, i + 2, HL_STRING);
                i += 2;
            } else {
                push_coalesced_span(&mut spans, c_start, i, HL_OPERATOR);
            }
            state.after_dot = false;
            continue;
        }

        // Backtick arbitrary identifier `` `...` ``
        if ch == '`' {
            let b_start = i;
            i += 1;
            while i < n && cs[i] != '`' {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            push_coalesced_span(&mut spans, b_start, i, HL_FUNCTION);
            state.after_dot = false;
            continue;
        }

        // `@module` prefix -> 13 (Namespace/Module)
        if ch == '@' {
            let m_start = i;
            i += 1;
            while i < n && (cs[i].is_alphanumeric() || cs[i] == '_') {
                i += 1;
            }
            push_coalesced_span(&mut spans, m_start, i, HL_NAMESPACE_LABEL);
            state.after_at_prefix = i == m_start + 1;
            state.after_dot = false;
            continue;
        }

        // `#Effects` / `#[e1, e2]` / trailing `#` effect perform operator -> 4
        if ch == '#' {
            let h_start = i;
            i += 1;
            if i < n && (cs[i].is_alphabetic() || cs[i] == '_') {
                while i < n && (cs[i].is_alphanumeric() || cs[i] == '_') {
                    i += 1;
                }
                push_coalesced_span(&mut spans, h_start, i, HL_MACRO_ATTR);
            } else if cs.get(i) == Some(&'[') {
                state.in_hash_effect_brackets = true;
                push_coalesced_span(&mut spans, h_start, i + 1, HL_MACRO_ATTR);
                i += 1;
            } else {
                push_coalesced_span(&mut spans, h_start, i, HL_MACRO_ATTR);
            }
            state.after_dot = false;
            continue;
        }

        // Numeric literals: hex `0x`, bin `0b`, oct `0o`, decimal/float/scientific
        if ch.is_ascii_digit() {
            let num_start = i;
            if ch == '0' && matches!(cs.get(i + 1), Some('x' | 'X' | 'b' | 'B' | 'o' | 'O')) {
                i += 2;
                while i < n && (cs[i].is_ascii_hexdigit() || cs[i] == '_') {
                    i += 1;
                }
            } else {
                while i < n && (cs[i].is_ascii_digit() || cs[i] == '_') {
                    i += 1;
                }
                if cs.get(i) == Some(&'.') && cs.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                    while i < n && (cs[i].is_ascii_digit() || cs[i] == '_') {
                        i += 1;
                    }
                }
                if matches!(cs.get(i), Some('e' | 'E')) {
                    i += 1;
                    if matches!(cs.get(i), Some('+' | '-')) {
                        i += 1;
                    }
                    while i < n && (cs[i].is_ascii_digit() || cs[i] == '_') {
                        i += 1;
                    }
                }
            }
            push_coalesced_span(&mut spans, num_start, i, HL_NUMBER_BOOL);
            state.after_dot = false;
            state.at_param_slot_start = false;
            continue;
        }

        // Identifiers & keywords
        if ch.is_alphabetic() || ch == '_' {
            let id_start = i;
            i += 1;
            while i < n && (cs[i].is_alphanumeric() || cs[i] == '_') {
                i += 1;
            }
            let word: String = cs[id_start..i].iter().collect();

            // Peek next non-whitespace char on this line
            let mut k = i;
            while k < n && (cs[k] == ' ' || cs[k] == '\t') {
                k += 1;
            }
            let next_ch = cs.get(k).copied();

            let grp = if state.after_at_prefix {
                HL_NAMESPACE_LABEL
            } else if state.in_hash_effect_brackets || state.after_hash_prefix {
                HL_MACRO_ATTR
            } else if is_nessa_control_kw(&word) {
                HL_KEYWORD_CONTROL
            } else if is_nessa_decl_or_other_kw(&word) {
                if matches!(word.as_str(), "fn" | "def" | "effect") {
                    state.expect_fn_name = true;
                }
                HL_KEYWORD
            } else if is_nessa_builtin_const(&word) {
                HL_NUMBER_BOOL
            } else if is_nessa_builtin_type(&word) {
                HL_TYPE_BUILTIN
            } else if state.expect_fn_name {
                state.expect_fn_name = false;
                state.expect_fn_params_open = true;
                HL_FUNCTION
            } else if (state.fn_param_paren_depth == 1 && state.at_param_slot_start)
                || (state.in_closure_params && state.at_closure_param_start)
            {
                state.at_param_slot_start = false;
                state.at_closure_param_start = false;
                HL_PARAMETER
            } else if is_screaming_const(&word) {
                HL_CONSTANT
            } else if is_pascal_type(&word) {
                HL_TYPE
            } else if next_ch == Some('(') {
                HL_FUNCTION
            } else if state.after_dot {
                HL_PROPERTY_FIELD
            } else if next_ch == Some(':')
                && cs.get(k + 1) != Some(&':')
                && state.fn_param_paren_depth == 0
                && !state.in_closure_params
            {
                // Struct literal field `field: val`
                HL_PROPERTY_FIELD
            } else {
                HL_NONE
            };

            state.after_at_prefix = false;
            state.after_hash_prefix = false;
            state.after_dot = false;
            push_coalesced_span(&mut spans, id_start, i, grp);
            continue;
        }

        // Punctuation & parameter / bracket state transitions
        match ch {
            '(' => {
                if state.expect_fn_params_open {
                    state.expect_fn_params_open = false;
                    state.fn_param_paren_depth = 1;
                    state.at_param_slot_start = true;
                } else if state.fn_param_paren_depth > 0 {
                    state.fn_param_paren_depth += 1;
                    state.at_param_slot_start = false;
                }
                state.expect_fn_name = false;
                state.after_dot = false;
                i += 1;
            }
            ')' => {
                if state.fn_param_paren_depth > 0 {
                    state.fn_param_paren_depth -= 1;
                    state.at_param_slot_start = false;
                }
                state.after_dot = false;
                i += 1;
            }
            ',' => {
                if state.fn_param_paren_depth == 1 {
                    state.at_param_slot_start = true;
                }
                if state.in_closure_params {
                    state.at_closure_param_start = true;
                }
                state.after_dot = false;
                i += 1;
            }
            ']' => {
                if state.in_hash_effect_brackets {
                    state.in_hash_effect_brackets = false;
                    push_coalesced_span(&mut spans, i, i + 1, HL_MACRO_ATTR);
                }
                state.after_dot = false;
                i += 1;
            }
            '.' => {
                if cs.get(i + 1) == Some(&'.') {
                    push_coalesced_span(&mut spans, i, i + 2, HL_OPERATOR);
                    state.after_dot = false;
                    i += 2;
                } else {
                    // Named optional parameter `.limit: i64 = 100` keeps `at_param_slot_start = true`!
                    if !(state.fn_param_paren_depth == 1 && state.at_param_slot_start) {
                        state.after_dot = true;
                    }
                    i += 1;
                }
            }
            '|' => {
                if cs.get(i + 1) == Some(&'>') {
                    push_coalesced_span(&mut spans, i, i + 2, HL_OPERATOR);
                    state.after_dot = false;
                    i += 2;
                } else if state.in_closure_params {
                    state.in_closure_params = false;
                    state.at_closure_param_start = false;
                    push_coalesced_span(&mut spans, i, i + 1, HL_OPERATOR);
                    i += 1;
                } else if closure_pipe_looks_valid(&cs, i) {
                    state.in_closure_params = true;
                    state.at_closure_param_start = true;
                    push_coalesced_span(&mut spans, i, i + 1, HL_OPERATOR);
                    i += 1;
                } else {
                    push_coalesced_span(&mut spans, i, i + 1, HL_OPERATOR);
                    i += 1;
                }
            }
            '-' | '=' | '+' | '*' | '/' | '%' | '<' | '>' | '!' | '?' | '&' | '^' | '~' | ':' => {
                let op_start = i;
                i += 1;
                if i < n
                    && matches!(
                        (ch, cs[i]),
                        ('-', '>' | '=')
                            | ('=', '=' | '>')
                            | ('+', '+' | '=')
                            | ('*', '=')
                            | ('/', '=')
                            | ('%', '=')
                            | ('<', '=')
                            | ('>', '=')
                            | ('!', '=')
                    )
                {
                    i += 1;
                }
                if ch == ':' {
                    state.at_param_slot_start = false;
                    state.at_closure_param_start = false;
                }
                state.after_dot = false;
                push_coalesced_span(&mut spans, op_start, i, HL_OPERATOR);
            }
            _ => {
                state.after_dot = false;
                i += 1;
            }
        }
    }

    // Unterminated plain single-line string without `{` resets at EOL so typos don't tint whole file
    if matches!(
        state.frames.last(),
        Some(NessaFrame::String {
            interp_brace_depth: 0
        })
    ) {
        state.frames.pop();
    }

    spans
}

const NESSA_CHECKPOINT_STRIDE: usize = 32;

struct CachedNessaDoc {
    doc_id: i64,
    version: i64,
    chunks: Vec<Arc<Vec<String>>>,
    total_lines: usize,
    checkpoints: Vec<NessaLexState>,
}

static NESSA_DOC_CACHE: OnceLock<Mutex<VecDeque<CachedNessaDoc>>> = OnceLock::new();

type NessaDocSnapshot = (Vec<Arc<Vec<String>>>, usize, Vec<NessaLexState>);

fn find_dirty_row_window(
    old_chunks: &[Arc<Vec<String>>],
    old_total: usize,
    new_chunks: &[Arc<Vec<String>>],
    new_total: usize,
) -> (usize, usize) {
    let mut row = 0usize;
    let mut oi = 0usize;
    let mut o_off = 0usize;
    let mut ni = 0usize;
    let mut n_off = 0usize;
    let min_total = old_total.min(new_total);

    while row < min_total && oi < old_chunks.len() && ni < new_chunks.len() {
        if o_off == 0 && n_off == 0 && Arc::ptr_eq(&old_chunks[oi], &new_chunks[ni]) {
            row += old_chunks[oi].len();
            oi += 1;
            ni += 1;
            continue;
        }
        if old_chunks[oi][o_off] != new_chunks[ni][n_off] {
            break;
        }
        row += 1;
        o_off += 1;
        if o_off >= old_chunks[oi].len() {
            oi += 1;
            o_off = 0;
        }
        n_off += 1;
        if n_off >= new_chunks[ni].len() {
            ni += 1;
            n_off = 0;
        }
    }
    let first_diff = row;

    if old_total != new_total {
        return (first_diff, new_total);
    }

    // Same line count: scan backward from the end (skipping `Arc::ptr_eq` tail chunks) to bound `last_diff`.
    let mut rem_rows = new_total;
    let mut b_oi = old_chunks.len();
    let mut b_ni = new_chunks.len();
    while rem_rows > first_diff && b_oi > 0 && b_ni > 0 {
        if Arc::ptr_eq(&old_chunks[b_oi - 1], &new_chunks[b_ni - 1]) {
            let clen = new_chunks[b_ni - 1].len();
            if rem_rows.saturating_sub(clen) >= first_diff {
                rem_rows -= clen;
                b_oi -= 1;
                b_ni -= 1;
                continue;
            }
        }
        break;
    }
    while rem_rows > first_diff {
        let r = rem_rows - 1;
        let eq = match (chunks_line_at(old_chunks, r), chunks_line_at(new_chunks, r)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        if !eq {
            break;
        }
        rem_rows -= 1;
    }
    (first_diff, rem_rows)
}

fn get_or_build_nessa_doc(doc_id: i64) -> Option<NessaDocSnapshot> {
    let (ver, total_lines) = doc_version_and_line_count(doc_id)?;
    let cache = NESSA_DOC_CACHE.get_or_init(|| Mutex::new(VecDeque::new()));
    let prev_cached = {
        let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(pos) = guard
            .iter()
            .position(|e| e.doc_id == doc_id && e.version == ver)
        {
            let entry = guard.remove(pos).unwrap();
            let res = (
                entry.chunks.clone(),
                entry.total_lines,
                entry.checkpoints.clone(),
            );
            guard.push_back(entry);
            return Some(res);
        }
        guard
            .iter()
            .find(|e| e.doc_id == doc_id)
            .map(|e| (e.chunks.clone(), e.total_lines, e.checkpoints.clone()))
    };

    let (snap_ver, chunks, snap_total) = doc_snapshot_source_and_offsets(doc_id)?;
    let _ = total_lines;
    let total_cps = if snap_total == 0 {
        0
    } else {
        (snap_total - 1) / NESSA_CHECKPOINT_STRIDE + 1
    };
    let mut checkpoints = Vec::with_capacity(total_cps);

    if let Some((prev_chunks, prev_total, prev_cps)) = prev_cached
        && !prev_cps.is_empty()
        && snap_total > 0
    {
        let (first_diff, dirty_end_excl) =
            find_dirty_row_window(&prev_chunks, prev_total, &chunks, snap_total);
        let reuse_cp_idx = (first_diff / NESSA_CHECKPOINT_STRIDE).min(prev_cps.len() - 1);
        checkpoints.extend_from_slice(&prev_cps[..=reuse_cp_idx]);
        let mut state = checkpoints[reuse_cp_idx].clone();
        let start_lex_row = reuse_cp_idx * NESSA_CHECKPOINT_STRIDE;
        let same_line_count = prev_total == snap_total && prev_cps.len() == total_cps;

        let mut converged_at_cp: Option<usize> = None;
        for row in start_lex_row..snap_total {
            if row > start_lex_row && row.is_multiple_of(NESSA_CHECKPOINT_STRIDE) {
                let cp_i = row / NESSA_CHECKPOINT_STRIDE;
                if same_line_count && row >= dirty_end_excl && prev_cps[cp_i] == state {
                    converged_at_cp = Some(cp_i);
                    break;
                }
                checkpoints.push(state.clone());
            }
            if let Some(line) = chunks_line_at(&chunks, row) {
                let _ = highlight_nessa_line(line, &mut state);
            }
        }
        if let Some(cp_i) = converged_at_cp {
            checkpoints.extend_from_slice(&prev_cps[cp_i..]);
        }
    } else {
        let mut state = NessaLexState::default();
        let mut row = 0usize;
        for chunk in &chunks {
            for line in chunk.iter() {
                if row.is_multiple_of(NESSA_CHECKPOINT_STRIDE) {
                    checkpoints.push(state.clone());
                }
                let _ = highlight_nessa_line(line, &mut state);
                row += 1;
            }
        }
    }

    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    guard.retain(|e| e.doc_id != doc_id);
    while guard.len() >= NESSA_DOC_LRU_CAP {
        let _ = guard.pop_front();
    }
    guard.push_back(CachedNessaDoc {
        doc_id,
        version: snap_ver,
        chunks: chunks.clone(),
        total_lines: snap_total,
        checkpoints: checkpoints.clone(),
    });
    Some((chunks, snap_total, checkpoints))
}

fn chunks_line_at(chunks: &[Arc<Vec<String>>], target_row: usize) -> Option<&str> {
    let mut base = 0usize;
    for chunk in chunks {
        if target_row < base + chunk.len() {
            return Some(chunk[target_row - base].as_str());
        }
        base += chunk.len();
    }
    None
}

fn highlight_nessa_doc_rows(
    doc_id: i64,
    start_row: usize,
    end_row: usize,
) -> Vec<Vec<TsHighlightSpan>> {
    let Some((chunks, total_lines, checkpoints)) = get_or_build_nessa_doc(doc_id) else {
        return Vec::new();
    };
    let requested_len = (end_row - start_row + 1).min(MAX_NESSA_LIST_ITEMS);
    let clamped_end = start_row + requested_len - 1;
    let mut out = Vec::with_capacity(requested_len);

    if start_row >= total_lines {
        out.resize(requested_len, Vec::new());
        return out;
    }

    let cp_idx = start_row / NESSA_CHECKPOINT_STRIDE;
    let cp_row = cp_idx * NESSA_CHECKPOINT_STRIDE;
    let mut state = checkpoints.get(cp_idx).cloned().unwrap_or_default();

    for r in cp_row..start_row.min(total_lines) {
        if let Some(line) = chunks_line_at(&chunks, r) {
            let _ = highlight_nessa_line(line, &mut state);
        }
    }

    for r in start_row..=clamped_end {
        if let Some(line) = chunks_line_at(&chunks, r) {
            out.push(highlight_nessa_line(line, &mut state));
        } else {
            out.push(Vec::new());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 9. Public Accelerator Entrypoints (`lava_accel`)
// ---------------------------------------------------------------------------

fn ensure_cached_doc_tree(
    api: &'static TsApi,
    norm_lang: &str,
    loaded_lang: &Arc<LoadedLanguage>,
    doc_id: i64,
) -> Option<(Arc<String>, Arc<Vec<usize>>)> {
    let (ver, _) = doc_version_and_line_count(doc_id)?;
    let key = DocTreeKey {
        doc_id,
        version: ver,
        lang: norm_lang.to_string(),
    };
    let lru = DOC_TREE_LRU.get_or_init(|| Mutex::new(DocTreeLru::new()));
    let prev_tree_bundle = {
        let mut guard = lru.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(idx) = guard.get_idx(&key) {
            let entry = &guard.entries[idx];
            return Some((Arc::clone(&entry.source), Arc::clone(&entry.line_starts)));
        }
        if let (Some(copy_fn), Some(_edit_fn)) = (api.ts_tree_copy, api.ts_tree_edit) {
            guard
                .entries
                .iter()
                .find(|e| {
                    e.key.doc_id == doc_id && e.key.lang == norm_lang && !e.tree_ptr.is_null()
                })
                .and_then(|e| {
                    // SAFETY: `e.tree_ptr` is a valid `TSTree` protected by `DOC_TREE_LRU` mutex;
                    // `ts_tree_copy` returns an independent refcounted copy safe to edit outside the lock.
                    let copied = unsafe { copy_fn(e.tree_ptr) };
                    if copied.is_null() {
                        None
                    } else {
                        Some((copied, Arc::clone(&e.source), Arc::clone(&e.line_starts)))
                    }
                })
        } else {
            None
        }
    };

    let (snap_ver, chunks, total_lines) = match doc_snapshot_source_and_offsets(doc_id) {
        Some(s) => s,
        None => {
            if let Some((copied, _, _)) = prev_tree_bundle {
                // SAFETY: `copied` is uniquely owned here and must be freed on early return.
                unsafe { (api.ts_tree_delete)(copied) };
            }
            return None;
        }
    };
    let (source_raw, line_starts_raw) = build_source_and_line_starts(&chunks, total_lines);

    let tree_ptr = if let Some((copied_tree, prev_src, prev_starts)) = prev_tree_bundle {
        let parsed = if let (Some(edit_fn), Some(ts_edit)) = (
            api.ts_tree_edit,
            compute_ts_input_edit(&prev_src, &prev_starts, &source_raw, &line_starts_raw),
        ) {
            // SAFETY: `copied_tree` is a uniquely owned `TSTree` copy and `ts_edit` has verified
            // byte/point bounds derived directly from `prev_src` -> `source_raw`.
            unsafe { edit_fn(copied_tree, &ts_edit) };
            parse_source_to_tree_with_old(api, loaded_lang, &source_raw, copied_tree)
        } else {
            parse_source_to_tree(api, loaded_lang, &source_raw)
        };
        // SAFETY: `copied_tree` was created by `ts_tree_copy` for this incremental parse and is no longer needed.
        unsafe { (api.ts_tree_delete)(copied_tree) };
        parsed?
    } else {
        parse_source_to_tree(api, loaded_lang, &source_raw)?
    };

    let source_arc = Arc::new(source_raw);
    let starts_arc = Arc::new(line_starts_raw);
    let final_key = DocTreeKey {
        doc_id,
        version: snap_ver,
        lang: norm_lang.to_string(),
    };

    let mut guard = lru.lock().unwrap_or_else(|p| p.into_inner());
    guard.insert(CachedDocTree {
        key: final_key,
        tree_ptr,
        source: Arc::clone(&source_arc),
        line_starts: Arc::clone(&starts_arc),
    });
    Some((source_arc, starts_arc))
}

/// Highlight inclusive `[start_row ..= end_row]` rows of `doc_id` using cached `TSTree` or
/// stateful Nessa analysis, emitting 0-based Unicode `char` column spans with `1..=18` `hl_group`s.
pub fn ts_highlight_doc_rows(
    doc_id: i64,
    lang: &str,
    start_row: usize,
    end_row: usize,
) -> Vec<Vec<TsHighlightSpan>> {
    if end_row < start_row {
        return Vec::new();
    }
    let norm = normalize_ts_lang(lang);
    if norm.is_empty() {
        return Vec::new();
    }
    let Some(api) = ts_api() else {
        if norm == "nessa" {
            return highlight_nessa_doc_rows(doc_id, start_row, end_row);
        }
        return Vec::new();
    };
    let Some(loaded_lang) = get_loaded_language(&norm) else {
        if norm == "nessa" {
            return highlight_nessa_doc_rows(doc_id, start_row, end_row);
        }
        return Vec::new();
    };
    let Some(_) = ensure_cached_doc_tree(api, &norm, &loaded_lang, doc_id) else {
        return Vec::new();
    };
    let (ver, _) = match doc_version_and_line_count(doc_id) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let key = DocTreeKey {
        doc_id,
        version: ver,
        lang: norm,
    };

    let lru = DOC_TREE_LRU.get_or_init(|| Mutex::new(DocTreeLru::new()));
    let mut guard = lru.lock().unwrap_or_else(|p| p.into_inner());
    let Some(idx) = guard.get_idx(&key) else {
        return Vec::new();
    };
    let entry = &guard.entries[idx];
    highlight_tree_rows(
        api,
        &loaded_lang,
        entry.tree_ptr,
        &entry.source,
        &entry.line_starts,
        start_row,
        end_row,
    )
}

type SnippetCacheEntry = ((String, String), Vec<Vec<TsHighlightSpan>>);
static SNIPPET_LRU: OnceLock<Mutex<VecDeque<SnippetCacheEntry>>> = OnceLock::new();

/// Highlight an in-memory multi-line `text` snippet (e.g. Markdown code fences, hover previews),
/// cached in a 64-entry LRU.
pub fn ts_highlight_snippet(text: &str, lang: &str) -> Vec<Vec<TsHighlightSpan>> {
    if text.is_empty() {
        return Vec::new();
    }
    let norm = normalize_ts_lang(lang);
    if norm.is_empty() {
        return Vec::new();
    }

    let cache_key = (norm.clone(), text.to_string());
    let lru = SNIPPET_LRU.get_or_init(|| Mutex::new(VecDeque::new()));
    {
        let mut guard = lru.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(pos) = guard.iter().position(|(k, _)| k == &cache_key) {
            let entry = guard.remove(pos).unwrap();
            let cloned = entry.1.clone();
            guard.push_back(entry);
            return cloned;
        }
    }

    let computed = if let Some(api) = ts_api()
        && let Some(loaded_lang) = get_loaded_language(&norm)
    {
        let mut clean_source = String::with_capacity(text.len());
        let mut starts = Vec::new();
        for (idx, raw_l) in text.split('\n').take(MAX_NESSA_LIST_ITEMS).enumerate() {
            if idx > 0 {
                clean_source.push('\n');
            }
            starts.push(clean_source.len());
            clean_source.push_str(raw_l.strip_suffix('\r').unwrap_or(raw_l));
        }
        starts.push(clean_source.len() + 1);
        let line_cnt = starts.len().saturating_sub(1);
        if line_cnt == 0 {
            return Vec::new();
        }
        let Some(tree_ptr) = parse_source_to_tree(api, &loaded_lang, &clean_source) else {
            return Vec::new();
        };
        let rows = highlight_tree_rows(
            api,
            &loaded_lang,
            tree_ptr,
            &clean_source,
            &starts,
            0,
            line_cnt - 1,
        );
        // SAFETY: `tree_ptr` is a temporary snippet `TSTree*` allocated above; delete immediately.
        unsafe { (api.ts_tree_delete)(tree_ptr) };
        rows
    } else if norm == "nessa" {
        let mut state = NessaLexState::default();
        text.split('\n')
            .take(MAX_NESSA_LIST_ITEMS)
            .map(|raw_l| {
                let clean = raw_l.strip_suffix('\r').unwrap_or(raw_l);
                highlight_nessa_line(clean, &mut state)
            })
            .collect::<Vec<_>>()
    } else {
        return Vec::new();
    };

    if !computed.is_empty() {
        let mut guard = lru.lock().unwrap_or_else(|p| p.into_inner());
        while guard.len() >= SNIPPET_LRU_CAP {
            let _ = guard.pop_front();
        }
        guard.push_back((cache_key, computed.clone()));
    }
    computed
}

fn find_field_name_in_parent(api: &TsApi, parent: TSNode, target: TSNode) -> Option<String> {
    // SAFETY: `parent` is checked non-null by caller.
    let is_null = unsafe { (api.ts_node_is_null)(parent) };
    if is_null {
        return None;
    }
    // SAFETY: `parent` is a valid `TSNode`.
    let cnt = unsafe { (api.ts_node_child_count)(parent) };
    for idx in 0..cnt {
        // SAFETY: `idx < cnt` on valid `parent`.
        let ch = unsafe { (api.ts_node_child)(parent, idx) };
        if ch.id == target.id && ch.context == target.context {
            // SAFETY: `idx < cnt` on valid `parent`.
            let fp = unsafe { (api.ts_node_field_name_for_child)(parent, idx) };
            if !fp.is_null() {
                // SAFETY: `fp` is a static NUL-terminated string owned by `TSLanguage`.
                if let Ok(s) = unsafe { CStr::from_ptr(fp) }.to_str() {
                    return Some(s.to_string());
                }
            }
            return None;
        }
    }
    None
}

fn inspect_nessa_doc_node(doc_id: i64, row: usize, col_char: usize) -> String {
    let Some((chunks, total_lines, _)) = get_or_build_nessa_doc(doc_id) else {
        return String::new();
    };
    if row >= total_lines {
        return format!("Language: nessa (native)\nCursor ({row}:{col_char}) out of bounds");
    }
    let line = chunks_line_at(&chunks, row).unwrap_or("");
    let llen = char_len_fast(line);
    let clamped_col = col_char.min(llen.saturating_sub(1));
    let row_spans = highlight_nessa_doc_rows(doc_id, row, row);
    let spans = row_spans.first().map(Vec::as_slice).unwrap_or(&[]);

    let hit_span = spans
        .iter()
        .find(|sp| (clamped_col as i64) >= sp.start_col && (clamped_col as i64) < sp.end_col);

    if let Some(sp) = hit_span {
        let sb = char_to_byte_idx(line, sp.start_col as usize);
        let eb = char_to_byte_idx(line, sp.end_col as usize);
        let tok_text = &line[sb..eb];
        format!(
            "Language : nessa (native stateful analyzer)\n\
             Node     : {} ({:?})\n\
             Range    : [{}:{} .. {}:{}]\n\
             Highlight: {} (hl_group={})",
            hl_group_name(sp.hl_group),
            tok_text,
            row + 1,
            sp.start_col,
            row + 1,
            sp.end_col,
            hl_group_name(sp.hl_group),
            sp.hl_group,
        )
    } else {
        // Extract identifier or punctuation under cursor
        let b_pos = char_to_byte_idx(line, clamped_col);
        let ch_preview = line[b_pos..]
            .chars()
            .next()
            .map_or(String::from("<eol>"), |c| c.to_string());
        format!(
            "Language : nessa (native stateful analyzer)\n\
             Node     : identifier / punctuation ({ch_preview:?})\n\
             Range    : [{}:{clamped_col} .. {}:{}]\n\
             Highlight: Variable/Plain (hl_group=0)",
            row + 1,
            row + 1,
            (clamped_col + 1).min(llen),
        )
    }
}

/// Inspect the syntax tree node, ancestor chain, S-expression, and highlight capture at
/// 0-based `(row, col_char)` in `doc_id`.
pub fn ts_inspect_doc_node(doc_id: i64, lang: &str, row: usize, col_char: usize) -> String {
    let norm = normalize_ts_lang(lang);
    if norm.is_empty() {
        return String::new();
    }
    let Some(api) = ts_api() else {
        if norm == "nessa" {
            return inspect_nessa_doc_node(doc_id, row, col_char);
        }
        return format!("Language: {norm} (libtree-sitter runtime not loaded)");
    };
    let Some(loaded_lang) = get_loaded_language(&norm) else {
        if norm == "nessa" {
            return inspect_nessa_doc_node(doc_id, row, col_char);
        }
        return format!("Language: {norm} (no Tree-sitter grammar .so found)");
    };
    let Some(_) = ensure_cached_doc_tree(api, &norm, &loaded_lang, doc_id) else {
        return String::new();
    };
    let Some((ver, _)) = doc_version_and_line_count(doc_id) else {
        return String::new();
    };
    let key = DocTreeKey {
        doc_id,
        version: ver,
        lang: norm.clone(),
    };

    let lru = DOC_TREE_LRU.get_or_init(|| Mutex::new(DocTreeLru::new()));
    let mut guard = lru.lock().unwrap_or_else(|p| p.into_inner());
    let Some(idx) = guard.get_idx(&key) else {
        return String::new();
    };
    let entry = &guard.entries[idx];
    let total_lines = entry.line_starts.len().saturating_sub(1);
    if row >= total_lines {
        return format!(
            "Language: {norm} (ABI {})\nCursor out of bounds",
            loaded_lang.abi_version
        );
    }

    let line = line_slice_at(&entry.source, &entry.line_starts, row);
    let line_chars = char_len_fast(line);
    let safe_char_col = if line_chars > 0 {
        col_char.min(line_chars - 1)
    } else {
        0
    };
    let byte_col = char_to_byte_idx(line, safe_char_col) as u32;
    let pt = TSPoint {
        row: row as u32,
        column: byte_col,
    };

    // SAFETY: `entry.tree_ptr` is valid while `guard` is held; all `TSNode` queries and `free` of
    // `ts_node_string` complete before `guard` drops.
    unsafe {
        let root = (api.ts_tree_root_node)(entry.tree_ptr);
        let leaf = (api.ts_node_descendant_for_point_range)(root, pt, pt);
        let named = (api.ts_node_named_descendant_for_point_range)(root, pt, pt);
        let target = if !(api.ts_node_is_null)(leaf) {
            leaf
        } else {
            named
        };
        if (api.ts_node_is_null)(target) {
            return format!(
                "Language: {norm} (ABI {})\nNo syntax node at cursor",
                loaded_lang.abi_version
            );
        }

        let node_kind = {
            let p = (api.ts_node_type)(target);
            if p.is_null() {
                "unknown"
            } else {
                CStr::from_ptr(p).to_str().unwrap_or("unknown")
            }
        };
        let is_named = (api.ts_node_is_named)(target);
        let is_err = (api.ts_node_is_error)(target);
        let is_missing = (api.ts_node_is_missing)(target);
        let has_err = (api.ts_node_has_error)(target);

        let sp = (api.ts_node_start_point)(target);
        let ep = (api.ts_node_end_point)(target);
        let s_line = if (sp.row as usize) < total_lines {
            line_slice_at(&entry.source, &entry.line_starts, sp.row as usize)
        } else {
            ""
        };
        let e_line = if (ep.row as usize) < total_lines {
            line_slice_at(&entry.source, &entry.line_starts, ep.row as usize)
        } else {
            ""
        };
        let s_char = byte_to_char_idx(s_line, sp.column as usize);
        let e_char = byte_to_char_idx(e_line, ep.column as usize);

        let parent = (api.ts_node_parent)(target);
        let field_opt = find_field_name_in_parent(api, parent, target);

        // Build ancestor chain (up to 8 levels)
        let mut ancestors = Vec::new();
        let mut cur = target;
        for _ in 0..8 {
            if (api.ts_node_is_null)(cur) {
                break;
            }
            let kp = (api.ts_node_type)(cur);
            let ks = if kp.is_null() {
                "?"
            } else {
                CStr::from_ptr(kp).to_str().unwrap_or("?")
            };
            let cp = (api.ts_node_parent)(cur);
            if let Some(fname) = find_field_name_in_parent(api, cp, cur) {
                ancestors.push(format!("{fname}: {ks}"));
            } else {
                ancestors.push(ks.to_string());
            }
            cur = cp;
        }

        // S-expression of nearest named node
        let sexp_target = if !(api.ts_node_is_null)(named) {
            named
        } else {
            target
        };
        let sexp_raw_ptr = (api.ts_node_string)(sexp_target);
        let sexp_str = if sexp_raw_ptr.is_null() {
            String::new()
        } else {
            let s = CStr::from_ptr(sexp_raw_ptr).to_string_lossy().into_owned();
            free(sexp_raw_ptr.cast::<c_void>());
            if s.chars().count() > 160 {
                let head: String = s.chars().take(160).collect();
                format!("{head}...")
            } else {
                s
            }
        };

        // Determine active highlight capture(s) at `(row, safe_char_col)`
        let mut active_captures = Vec::new();
        let mut winning_winner = CellWinner::EMPTY;
        if let Some(q) = loaded_lang.query_ptr {
            let cursor = (api.ts_query_cursor_new)();
            if !cursor.is_null() {
                let _ = (api.ts_query_cursor_set_point_range)(
                    cursor,
                    TSPoint {
                        row: row as u32,
                        column: 0,
                    },
                    TSPoint {
                        row: (row as u32).saturating_add(1),
                        column: 0,
                    },
                );
                (api.ts_query_cursor_exec)(cursor, q, root);
                let mut m = TSQueryMatch {
                    id: 0,
                    pattern_index: 0,
                    capture_count: 0,
                    captures: ptr::null(),
                };
                let mut seq = 0u32;
                while (api.ts_query_cursor_next_match)(cursor, &mut m) {
                    if m.capture_count == 0 || m.captures.is_null() {
                        continue;
                    }
                    let caps = std::slice::from_raw_parts(m.captures, usize::from(m.capture_count));
                    if let Some(preds) = loaded_lang
                        .pattern_predicates
                        .get(usize::from(m.pattern_index))
                        && !evaluate_match_predicates(api, preds, caps, &entry.source)
                    {
                        continue;
                    }
                    for cap in caps {
                        let csp = (api.ts_node_start_point)(cap.node);
                        let cep = (api.ts_node_end_point)(cap.node);
                        let contains_pt = (csp.row < pt.row
                            || (csp.row == pt.row && csp.column <= pt.column))
                            && (cep.row > pt.row || (cep.row == pt.row && cep.column > pt.column));
                        if contains_pt {
                            let cname = loaded_lang
                                .capture_names
                                .get(cap.index as usize)
                                .map_or("", String::as_str);
                            let grp = loaded_lang
                                .capture_groups
                                .get(cap.index as usize)
                                .copied()
                                .unwrap_or(HL_NONE);
                            if !cname.is_empty() {
                                let entry_tag = format!("@{cname}");
                                if !active_captures.contains(&entry_tag) {
                                    active_captures.push(entry_tag);
                                }
                            }
                            if grp != HL_NONE {
                                seq = seq.wrapping_add(1);
                                let sb = (api.ts_node_start_byte)(cap.node);
                                let eb = (api.ts_node_end_byte)(cap.node);
                                let cand = CellWinner {
                                    hl_group: grp,
                                    span_len: eb.saturating_sub(sb),
                                    pattern_idx: u32::from(m.pattern_index),
                                    seq,
                                };
                                if cand.beats(winning_winner) {
                                    winning_winner = cand;
                                }
                            }
                        }
                    }
                }
                (api.ts_query_cursor_delete)(cursor);
            }
        }

        if winning_winner.hl_group == HL_EMBEDDED_CLEAR {
            winning_winner.hl_group = HL_NONE;
        }

        let mut flags = Vec::new();
        flags.push(if is_named { "named" } else { "anonymous" });
        if is_err {
            flags.push("ERROR");
        } else if has_err {
            flags.push("has_error");
        }
        if is_missing {
            flags.push("MISSING");
        }

        let field_str = field_opt
            .map(|f| format!(" (field: {f})"))
            .unwrap_or_default();
        let cap_summary = if active_captures.is_empty() {
            format!(
                "{} (hl_group={})",
                hl_group_name(i64::from(winning_winner.hl_group)),
                winning_winner.hl_group
            )
        } else {
            format!(
                "{} -> {} (hl_group={})",
                active_captures.join(", "),
                hl_group_name(i64::from(winning_winner.hl_group)),
                winning_winner.hl_group
            )
        };

        format!(
            "Language : {norm} (ABI {})\n\
             Node     : {node_kind}{field_str} [{}]\n\
             Range    : [{}:{} .. {}:{}]\n\
             Captures : {cap_summary}\n\
             Ancestors: {}\n\
             S-Expr   : {sexp_str}",
            loaded_lang.abi_version,
            flags.join(", "),
            sp.row + 1,
            s_char,
            ep.row + 1,
            e_char,
            ancestors.join(" <- "),
        )
    }
}

fn discover_available_grammars() -> Vec<String> {
    let mut langs = HashSet::new();
    langs.insert("nessa".to_string());

    for dir in parser_search_dirs() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        let is_usr_lib = dir == Path::new("/usr/lib");
        for entry in rd.flatten() {
            let file_name = entry.file_name();
            let Some(name_str) = file_name.to_str() else {
                continue;
            };
            if is_usr_lib {
                // Only inspect `libtree-sitter-<lang>.so*` inside `/usr/lib`
                let Some(rest) = name_str.strip_prefix("libtree-sitter-") else {
                    continue;
                };
                if let Some((lang_part, _)) = rest.split_once(".so")
                    && !lang_part.is_empty()
                {
                    langs.insert(lang_part.to_string());
                }
            } else if let Some(stem) = name_str.strip_suffix(".so") {
                let clean = stem.strip_prefix("libtree-sitter-").unwrap_or(stem);
                if !clean.is_empty() {
                    langs.insert(clean.to_string());
                }
            }
        }
    }

    let mut sorted: Vec<String> = langs.into_iter().collect();
    sorted.sort();
    sorted
}

static DISCOVERED_GRAMMARS_CACHE: OnceLock<Vec<String>> = OnceLock::new();

/// Return `(runtime_loaded, abi_version, grammar_count, available_languages)`.
pub fn ts_status_info() -> (bool, i64, i64, Vec<String>) {
    let loaded = ts_api().is_some();
    if !loaded {
        return (false, 0, 1, vec!["nessa".to_string()]);
    }
    let langs = DISCOVERED_GRAMMARS_CACHE
        .get_or_init(discover_available_grammars)
        .clone();
    let abi_ver = get_loaded_language("rust")
        .or_else(|| get_loaded_language("c"))
        .map_or(TS_ABI_FALLBACK_VERSION, |l| l.abi_version);
    let count = langs.len() as i64;
    (true, abi_ver, count, langs)
}

// ---------------------------------------------------------------------------
// 10. Comprehensive Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc_store::{doc_free, doc_new, doc_set_line};

    #[test]
    fn alias_normalization_covers_all_common_extensions() {
        assert_eq!(normalize_ts_lang("rs"), "rust");
        assert_eq!(normalize_ts_lang("py"), "python");
        assert_eq!(normalize_ts_lang("js"), "javascript");
        assert_eq!(normalize_ts_lang("jsx"), "javascript");
        assert_eq!(normalize_ts_lang("ts"), "typescript");
        assert_eq!(normalize_ts_lang("c++"), "cpp");
        assert_eq!(normalize_ts_lang("cc"), "cpp");
        assert_eq!(normalize_ts_lang("hpp"), "cpp");
        assert_eq!(normalize_ts_lang("h"), "c");
        assert_eq!(normalize_ts_lang("sh"), "bash");
        assert_eq!(normalize_ts_lang("zsh"), "bash");
        assert_eq!(normalize_ts_lang("shell"), "bash");
        assert_eq!(normalize_ts_lang("yml"), "yaml");
        assert_eq!(normalize_ts_lang("md"), "markdown");
        assert_eq!(normalize_ts_lang("gfm"), "markdown");
        assert_eq!(normalize_ts_lang("htm"), "html");
        assert_eq!(normalize_ts_lang("cs"), "c-sharp");
        assert_eq!(normalize_ts_lang("csharp"), "c-sharp");
        assert_eq!(normalize_ts_lang("golang"), "go");
        assert_eq!(normalize_ts_lang("ns"), "nessa");
    }

    #[test]
    fn regex_and_lua_match_engine_handles_classes_anchors_and_alternations() {
        let scream = CompiledRegex::compile(r"^[A-Z][A-Z0-9_]*$").unwrap();
        assert!(scream.is_match("MAX_SIZE_1"));
        assert!(scream.is_match("A"));
        assert!(!scream.is_match("PascalCase"));
        assert!(!scream.is_match("_LEADING"));

        let scream_d = CompiledRegex::compile(r"^[A-Z][A-Z\d_]*$").unwrap();
        assert!(scream_d.is_match("HTTP_200_OK"));
        assert!(!scream_d.is_match("Http200"));

        let pascal = CompiledRegex::compile(r"^[A-Z]").unwrap();
        assert!(pascal.is_match("Widget"));
        assert!(!pascal.is_match("widget"));

        let alt = CompiledRegex::compile(r"^(append|len|make|panic)$").unwrap();
        assert!(alt.is_match("len"));
        assert!(!alt.is_match("length"));

        let lua_const = CompiledRegex::compile_lua("^[A-Z_][A-Z%d_]*$").unwrap();
        assert!(lua_const.is_match("_CONST_42"));
        assert!(!lua_const.is_match("camelCase"));
    }

    #[test]
    fn resilient_scm_stripper_isolates_broken_pattern_and_compiles_rest() {
        let broken_scm = "\
            (line_comment) @comment\n\
            (non_existent_xyz_node_type field_zzz: (identifier) @function)\n\
            (string_literal) @string\n";

        if let (Some(api), Some(rust_lang)) = (ts_api(), get_loaded_language("rust")) {
            let q = compile_ts_query_resilient(api, rust_lang.language_ptr, broken_scm)
                .expect("resilient query compiler should strip invalid middle pattern");
            // SAFETY: `q` was just compiled by `compile_ts_query_resilient`.
            let pat_cnt = unsafe { (api.ts_query_pattern_count)(q) };
            assert_eq!(pat_cnt, 2);
            // SAFETY: `q` is uniquely owned in this test.
            unsafe { (api.ts_query_delete)(q) };
        }
    }

    #[test]
    fn rust_treesitter_highlights_predicates_utf8_char_cols_and_inspect() {
        let (loaded, abi_ver, grammar_cnt, langs) = ts_status_info();
        if !loaded || !langs.iter().any(|l| l == "rust") {
            return;
        }
        assert!(abi_ver >= 13);
        assert!(grammar_cnt >= 2);

        let lines = vec![
            "/// Outer doc comment".to_string(),
            "// Regular comment".to_string(),
            "use std::collections::HashMap;".to_string(),
            "pub const MAX_VAL: usize = 42;".to_string(),
            "pub fn greet(name: &str) -> Option<String> {".to_string(),
            "    let 变量 = \"你好🦀\\n\"; // 尾部注释".to_string(),
            "    Some(name.trim().to_string())".to_string(),
            "}".to_string(),
        ];
        let doc = doc_new(lines.clone());
        let rows = ts_highlight_doc_rows(doc, "rs", 0, lines.len() - 1);
        assert_eq!(rows.len(), lines.len());

        // Row 0: DocComment (16)
        assert_eq!(rows[0][0].hl_group, i64::from(HL_DOC_COMMENT));
        // Row 1: Comment (15)
        assert_eq!(rows[1][0].hl_group, i64::from(HL_COMMENT));
        // Row 2: `use` = 2, `std` = 13, `HashMap` = 5
        assert!(
            rows[2]
                .iter()
                .any(|s| s.start_col == 0 && s.end_col == 3 && s.hl_group == 2)
        );
        assert!(
            rows[2]
                .iter()
                .any(|s| s.start_col == 4 && s.end_col == 7 && s.hl_group == 13)
        );
        assert!(
            rows[2]
                .iter()
                .any(|s| s.start_col == 22 && s.end_col == 29 && s.hl_group == 5)
        );
        // Row 3: `MAX_VAL` = 10 (Constant), `usize` = 6 (BuiltinType), `42` = 9 (Number)
        assert!(
            rows[3]
                .iter()
                .any(|s| s.start_col == 10 && s.end_col == 17 && s.hl_group == 10)
        );
        assert!(
            rows[3]
                .iter()
                .any(|s| s.start_col == 19 && s.end_col == 24 && s.hl_group == 6)
        );
        assert!(
            rows[3]
                .iter()
                .any(|s| s.start_col == 27 && s.end_col == 29 && s.hl_group == 9)
        );
        // Row 4: `greet` = 3, `name` = 12, `str` = 6, `Option` = 6, `String` = 6
        assert!(
            rows[4]
                .iter()
                .any(|s| s.start_col == 7 && s.end_col == 12 && s.hl_group == 3)
        );
        assert!(
            rows[4]
                .iter()
                .any(|s| s.start_col == 13 && s.end_col == 17 && s.hl_group == 12)
        );
        assert!(
            rows[4]
                .iter()
                .any(|s| s.start_col == 20 && s.end_col == 23 && s.hl_group == 6)
        );
        assert!(
            rows[4]
                .iter()
                .any(|s| s.start_col == 28 && s.end_col == 34 && s.hl_group == 6)
        );

        // Row 5: `"    let 变量 = \"你好🦀\\n\"; // 尾部注释"`
        // Verify exact 0-based Unicode `char` columns across CJK + 4-byte emoji!
        // chars: 0..4="    ", 4..7="let", 8..10="变量", 11="=", 13..20="\"你好🦀\\n\"",
        // where `\n` escape is at char cols 17..19, and `// 尾部注释` is at char cols 22..29!
        assert!(
            rows[5]
                .iter()
                .any(|s| s.start_col == 13 && s.end_col == 17 && s.hl_group == 7)
        );
        assert!(
            rows[5]
                .iter()
                .any(|s| s.start_col == 17 && s.end_col == 19 && s.hl_group == 8)
        );
        assert!(
            rows[5]
                .iter()
                .any(|s| s.start_col == 19 && s.end_col == 20 && s.hl_group == 7)
        );
        assert!(
            rows[5]
                .iter()
                .any(|s| s.start_col == 22 && s.end_col == 29 && s.hl_group == 15)
        );

        // Inspect `greet` identifier at (4, 8)
        let info = ts_inspect_doc_node(doc, "rust", 4, 8);
        assert!(info.contains("Language : rust"));
        assert!(info.contains("identifier"));
        assert!(info.contains("function_item"));

        // Mutating doc invalidates old version and re-parses cleanly
        doc_set_line(doc, 3, "pub const MAX_VAL: usize = 999;".to_string());
        let updated = ts_highlight_doc_rows(doc, "rust", 3, 3);
        assert!(
            updated[0]
                .iter()
                .any(|s| s.start_col == 27 && s.end_col == 30 && s.hl_group == 9)
        );

        doc_free(doc);
    }

    #[test]
    fn inherits_queries_and_lru_eviction_work_cleanly() {
        let (loaded, _, _, langs) = ts_status_info();
        if loaded && langs.iter().any(|l| l == "typescript") {
            let ts_snip = ts_highlight_snippet(
                "import { readFile } from \"fs\";\nfunction run(x: number): boolean { return true; }",
                "ts",
            );
            assert_eq!(ts_snip.len(), 2);
            // `import` -> 2 (Control/Import)
            assert!(
                ts_snip[0]
                    .iter()
                    .any(|s| s.start_col == 0 && s.end_col == 6 && s.hl_group == 2)
            );
            // `"fs"` -> 7 (String)
            assert!(ts_snip[0].iter().any(|s| s.hl_group == 7));
            // `run` -> 3 (Function), `x` -> 12 (Parameter, from `_typescript`), `true` -> 9
            assert!(ts_snip[1].iter().any(|s| s.hl_group == 3));
            assert!(ts_snip[1].iter().any(|s| s.hl_group == 12));
            assert!(ts_snip[1].iter().any(|s| s.hl_group == 9));
        }

        // Exercise 40 distinct docs (> LRU 32) to verify eviction & `ts_tree_delete` safety
        for i in 0..40 {
            let d = doc_new(vec![format!("fn f_{i}(x: i32) -> i32 {{ x + {i} }}")]);
            let _ = ts_highlight_doc_rows(d, "rust", 0, 0);
            doc_free(d);
        }
    }

    #[test]
    fn nessa_multiline_stateful_highlighter_covers_all_constructs() {
        let ns_lines = vec![
            "--- Doc comment header".to_string(),
            "-- Normal comment".to_string(),
            "{- Multi-line".to_string(),
            "   nested {- inner -} block comment -}".to_string(),
            "use @lava_core.{SystemKernel, InteractiveEffects}".to_string(),
            "pub const MAX_LIMIT: i64 = 100".to_string(),
            "pub fn compute(".to_string(),
            "    kernel: SystemKernel,".to_string(),
            "    .limit: i64 = 42,".to_string(),
            ") -> #InteractiveEffects !String bool {".to_string(),
            "    let msg = \"val={limit + 1}\\n{{ok}}\"".to_string(),
            "    if kernel.is_ready and msg != null {".to_string(),
            "        status_emit(msg)#".to_string(),
            "        return true".to_string(),
            "    }".to_string(),
            "    false".to_string(),
            "}".to_string(),
        ];
        let doc = doc_new(ns_lines.clone());
        let rows = ts_highlight_doc_rows(doc, "ns", 0, ns_lines.len() - 1);
        assert_eq!(rows.len(), ns_lines.len());

        assert_eq!(rows[0][0].hl_group, 16); // `---` DocComment
        assert_eq!(rows[1][0].hl_group, 15); // `--` Comment
        assert_eq!(rows[2][0].hl_group, 15); // `{-` multi-line block comment row 1
        assert_eq!(rows[3][0].hl_group, 15); // `-}` multi-line block comment row 2
        // `use` = 2, `@lava_core` = 13, `SystemKernel` = 5
        assert!(
            rows[4]
                .iter()
                .any(|s| s.start_col == 0 && s.end_col == 3 && s.hl_group == 2)
        );
        assert!(
            rows[4]
                .iter()
                .any(|s| s.start_col == 4 && s.end_col == 14 && s.hl_group == 13)
        );
        assert!(rows[4].iter().any(|s| s.hl_group == 5));
        // `MAX_LIMIT` = 10, `i64` = 6, `100` = 9
        assert!(rows[5].iter().any(|s| s.hl_group == 10));
        assert!(rows[5].iter().any(|s| s.hl_group == 6));
        assert!(rows[5].iter().any(|s| s.hl_group == 9));
        // Multi-line `fn compute(` -> `compute` = 3, `kernel` (row 7) = 12, `limit` (row 8) = 12!
        assert!(
            rows[6]
                .iter()
                .any(|s| s.start_col == 7 && s.end_col == 14 && s.hl_group == 3)
        );
        assert!(
            rows[7]
                .iter()
                .any(|s| s.start_col == 4 && s.end_col == 10 && s.hl_group == 12)
        );
        assert!(
            rows[8]
                .iter()
                .any(|s| s.start_col == 5 && s.end_col == 10 && s.hl_group == 12)
        );
        // Row 9: `#InteractiveEffects` = 4, `String` = 6, `bool` = 6
        assert!(rows[9].iter().any(|s| s.hl_group == 4));
        assert!(rows[9].iter().any(|s| s.hl_group == 6));
        // Row 10: string `7`, `{` / `\n` / `{{` / `}}` escapes `8`, `1` inside `{limit + 1}` = `9`!
        assert!(rows[10].iter().any(|s| s.hl_group == 7));
        assert!(rows[10].iter().any(|s| s.hl_group == 8));
        assert!(rows[10].iter().any(|s| s.hl_group == 9));
        // Row 11: `if` = 2, `.is_ready` = 11, `and` = 1, `null` = 9
        assert!(rows[11].iter().any(|s| s.hl_group == 2));
        assert!(rows[11].iter().any(|s| s.hl_group == 11));
        assert!(rows[11].iter().any(|s| s.hl_group == 9));
        // Row 12: `status_emit` = 3, `#` = 4
        assert!(rows[12].iter().any(|s| s.hl_group == 3));
        assert!(rows[12].iter().any(|s| s.hl_group == 4));

        let inspect = ts_inspect_doc_node(doc, "nessa", 6, 8);
        assert!(inspect.contains("Language : nessa"));
        assert!(inspect.contains("hl_group=3"));

        doc_free(doc);
    }

    #[test]
    fn fallback_node_walker_and_multi_grammar_queries_highlight_cleanly() {
        let (loaded, _, _, langs) = ts_status_info();
        if !loaded {
            return;
        }
        // 1) Directly verify fallback AST node-type walker when `.scm` query is absent (`query_ptr: None`)
        if let (Some(api), Some(rust_lang)) = (ts_api(), get_loaded_language("rust")) {
            let no_query_lang = LoadedLanguage {
                _grammar_handle: ptr::null_mut(),
                language_ptr: rust_lang.language_ptr,
                abi_version: rust_lang.abi_version,
                query_ptr: None,
                capture_groups: Vec::new(),
                capture_names: Vec::new(),
                pattern_predicates: Vec::new(),
            };
            let src =
                "// cmt\npub fn add(a: i32) -> i32 { if true { \"ok\" } else { \"no\" }; 1 + 2 }";
            let starts = vec![0, 7, src.len() + 1];
            let tree = parse_source_to_tree(api, &no_query_lang, src).unwrap();
            let fb_rows = highlight_tree_rows(api, &no_query_lang, tree, src, &starts, 0, 1);
            // SAFETY: `tree` was allocated above for this test.
            unsafe { (api.ts_tree_delete)(tree) };
            assert_eq!(fb_rows[0][0].hl_group, 15); // comment
            assert!(fb_rows[1].iter().any(|s| s.hl_group == 1)); // keyword (`pub` / `fn`)
            assert!(fb_rows[1].iter().any(|s| s.hl_group == 2)); // control keyword (`if` / `else`)
            assert!(fb_rows[1].iter().any(|s| s.hl_group == 6)); // primitive_type (`i32`)
            assert!(fb_rows[1].iter().any(|s| s.hl_group == 7)); // string_literal (`"ok"`)
            assert!(fb_rows[1].iter().any(|s| s.hl_group == 9)); // boolean / integer (`true`, `1`, `2`)
            assert!(fb_rows[1].iter().any(|s| s.hl_group == 14)); // operator (`+`)
        }

        // 2) Verify multi-language `.so` grammars + Helix `.scm` queries compile & highlight cleanly
        let samples: &[(&str, &str)] = &[
            (
                "py",
                "# comment\ndef calc(x: int) -> bool:\n    return True\n",
            ),
            ("c", "#include <stdio.h>\nint main(void) { return 0; }\n"),
            (
                "cpp",
                "namespace app { class Widget { public: int count() { return 1; } }; }\n",
            ),
            ("golang", "package main\nfunc main() { println(\"hi\") }\n"),
            (
                "sh",
                "#!/usr/bin/env bash\nif [ -n \"$HOME\" ]; then echo 1; fi\n",
            ),
            ("json", "{\"enabled\": true, \"count\": 42}\n"),
            ("toml", "[package]\nname = \"lava\"\nversion = \"0.1.0\"\n"),
            ("yml", "enabled: true\ncount: 10\n"),
            (
                "md",
                "# Heading 1\n\n`inline_code` and [link](https://example.com)\n",
            ),
            (
                "lua",
                "-- comment\nlocal function f(x) if x then return 1 end end\n",
            ),
            (
                "cs",
                "public class Demo { public static void Main() { int x = 1; } }\n",
            ),
        ];
        for &(alias, code) in samples {
            let norm = normalize_ts_lang(alias);
            if langs.iter().any(|l| l == &norm) {
                let rows = ts_highlight_snippet(code, alias);
                assert!(!rows.is_empty(), "expected rows for {alias} ({norm})");
                assert!(
                    rows.iter().any(|r| !r.is_empty()),
                    "expected non-empty spans for {alias} ({norm})"
                );
            }
        }
    }

    #[test]
    fn incremental_ts_tree_edit_and_nessa_checkpoint_reuse_match_fresh_highlights() {
        use crate::doc_store::{
            doc_delete_span, doc_free, doc_insert_text, doc_new, doc_slice_lines,
        };

        // 1. Nessa incremental checkpoint convergence across a 300-line document
        let mut ns_lines: Vec<String> = (0..300)
            .map(|i| format!("pub fn item_{i}(x: i64) -> i64 = x + {i}"))
            .collect();
        let ns_doc = doc_new(ns_lines.clone());
        let _initial = ts_highlight_doc_rows(ns_doc, "nessa", 0, 299);

        // Single-line edit at row 150 (converges at next checkpoint without lexing tail)
        let _ = doc_insert_text(ns_doc, 150, 41, " + 99");
        ns_lines[150].push_str(" + 99");
        let inc_ns_1 = ts_highlight_doc_rows(ns_doc, "nessa", 140, 299);
        let fresh_ns_doc_1 = doc_new(ns_lines.clone());
        let exp_ns_1 = ts_highlight_doc_rows(fresh_ns_doc_1, "nessa", 140, 299);
        assert_eq!(inc_ns_1, exp_ns_1);
        doc_free(fresh_ns_doc_1);

        // Multi-line block comment insertion + removal
        let _ = doc_insert_text(ns_doc, 60, 0, "{-\nmultiline doc\n-}\n");
        let cur_ns_lines = doc_slice_lines(ns_doc, 0, 310);
        let inc_ns_2 = ts_highlight_doc_rows(ns_doc, "nessa", 55, 220);
        let fresh_ns_doc_2 = doc_new(cur_ns_lines);
        let exp_ns_2 = ts_highlight_doc_rows(fresh_ns_doc_2, "nessa", 55, 220);
        assert_eq!(inc_ns_2, exp_ns_2);
        doc_free(fresh_ns_doc_2);
        doc_free(ns_doc);

        // 2. Incremental `ts_tree_edit` on Rust grammar (if system `rust.so` available)
        if get_loaded_language("rust").is_some() {
            let rs_lines: Vec<String> = (0..120)
                .map(|i| format!("pub fn compute_{i}(val: i32) -> i32 {{ val + {i} }}"))
                .collect();
            let rs_doc = doc_new(rs_lines);
            let _ = ts_highlight_doc_rows(rs_doc, "rust", 0, 119);

            // Burst of edits: insert UTF-8 string + newline + delete span before next render
            let _ = doc_insert_text(rs_doc, 40, 36, " let label = \"café\";\n    let extra = 7;");
            let _ = doc_delete_span(rs_doc, 10, 0, 11, 0);

            let inc_rs = ts_highlight_doc_rows(rs_doc, "rust", 0, 120);
            let snap_lines = doc_slice_lines(rs_doc, 0, 130);
            let fresh_rs_doc = doc_new(snap_lines);
            let exp_rs = ts_highlight_doc_rows(fresh_rs_doc, "rust", 0, 120);
            assert_eq!(inc_rs, exp_rs);

            doc_free(fresh_rs_doc);
            doc_free(rs_doc);
        }
    }

    #[test]
    fn nessa_treesitter_grammar_highlights_directives_enums_interpolation_and_inspect() {
        let lines = vec![
            "--% .max_width = 100".to_string(),
            "pub enum IoError { pub NotFound }".to_string(),
            "pub enum Color { pub red, pub green }".to_string(),
            "pub fn greet(plain_var: String) -> !IoError String {".to_string(),
            "    let c = Color.red".to_string(),
            "    \"前缀:{plain_var + 1}!\"".to_string(),
            "}".to_string(),
        ];
        let doc = doc_new(lines.clone());
        let rows = ts_highlight_doc_rows(doc, "nessa", 0, lines.len() - 1);
        assert_eq!(rows.len(), lines.len());

        // Row 0: `--%` format directive -> HL_MACRO_ATTR (4)
        assert_eq!(rows[0][0].hl_group, i64::from(HL_MACRO_ATTR));

        if get_loaded_language("nessa").is_some() {
            // Row 1: `NotFound` PascalCase Error enum variant -> HL_CONSTANT (10)
            assert!(
                rows[1]
                    .iter()
                    .any(|s| s.start_col == 23 && s.end_col == 31 && s.hl_group == 10)
            );
            // Row 2: `red` snake_case ordinary enum variant -> HL_FUNCTION / constructor (3)
            assert!(
                rows[2]
                    .iter()
                    .any(|s| s.start_col == 21 && s.end_col == 24 && s.hl_group == 3)
            );
            // Row 5: `"    \"前缀:{plain_var + 1}!\""`
            // `plain_var` at char cols 9..18 must be cleared of outer string (HL_NONE, not 7)!
            assert!(
                !rows[5]
                    .iter()
                    .any(|s| s.start_col <= 10 && s.end_col > 10 && s.hl_group == 7),
                "interpolated plain_var inside string must not inherit outer HL_STRING (7): {:?}",
                rows[5]
            );
            // `1` at char col 21..22 -> HL_NUMBER_BOOL (9)
            assert!(
                rows[5]
                    .iter()
                    .any(|s| s.start_col == 21 && s.end_col == 22 && s.hl_group == 9)
            );

            let dir_inspect = ts_inspect_doc_node(doc, "nessa", 0, 2);
            assert!(dir_inspect.contains("format_directive"), "{dir_inspect}");
            let fn_inspect = ts_inspect_doc_node(doc, "nessa", 3, 8);
            assert!(fn_inspect.contains("function_definition"), "{fn_inspect}");
        }

        doc_free(doc);
    }
}
