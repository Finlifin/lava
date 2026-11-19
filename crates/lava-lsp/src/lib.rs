//! Non-blocking Language Server Protocol (LSP) JSON-RPC 2.0 `Content-Length` stdio client manager.
//!
//! Spawns real external language servers (`rust-analyzer`, `pyright-langserver`, `clangd`,
//! `typescript-language-server`, `gopls`, `lua-language-server`, `zls`, `marksman`, `taplo`, etc.),
//! frames/parses JSON-RPC 2.0 messages on dedicated background threads, automatically completes
//! the `initialize` -> `initialized` handshake, decodes LSP notifications and responses into
//! structured Rust/Nessa tuples, and wakes Lava's single-VM `EventQueue` via `LSP_WAKE_TIMER_ID`.

pub mod json;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

pub use json::{JsonValue, parse_json};

/// Dedicated timer ID pushed to `EventQueue` (`UiEvent::Tick`) whenever any LSP server produces output.
pub const LSP_WAKE_TIMER_ID: u64 = 0x4C53_5000; // 1280528384

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDiagItem {
    pub row: i64,
    pub col: i64,
    pub end_row: i64,
    pub end_col: i64,
    pub severity: String,
    pub source: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspLocationItem {
    pub file_path: String,
    pub row: i64,
    pub col: i64,
    pub end_row: i64,
    pub end_col: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSymbolItem {
    pub name: String,
    pub kind: String,
    pub detail: String,
    pub row: i64,
    pub col: i64,
    pub end_row: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspCompletionItem {
    pub label: String,
    pub insert_text: String,
    pub kind: String,
    pub detail: String,
    pub documentation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspTextEditItem {
    pub file_path: String,
    pub start_row: i64,
    pub start_col: i64,
    pub end_row: i64,
    pub end_col: i64,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspCodeActionItem {
    pub title: String,
    pub kind: String,
    pub edits: Vec<LspTextEditItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspEvent {
    Initialized {
        server_name: String,
        server_version: String,
        capabilities: String,
    },
    Diagnostics {
        file_path: String,
        items: Vec<LspDiagItem>,
    },
    Hover {
        req_id: i64,
        markdown: String,
    },
    Locations {
        req_id: i64,
        kind: String,
        items: Vec<LspLocationItem>,
    },
    Symbols {
        req_id: i64,
        items: Vec<LspSymbolItem>,
    },
    Completions {
        req_id: i64,
        items: Vec<LspCompletionItem>,
    },
    SignatureHelp {
        req_id: i64,
        label: String,
        documentation: String,
        parameters: Vec<String>,
        active_param: i64,
    },
    WorkspaceEdit {
        req_id: i64,
        label: String,
        edits: Vec<LspTextEditItem>,
    },
    CodeActions {
        req_id: i64,
        items: Vec<LspCodeActionItem>,
    },
    Log {
        level: String,
        message: String,
    },
    RequestError {
        req_id: i64,
        method: String,
        message: String,
    },
    Exited {
        exit_code: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSessionStatus {
    pub id: u64,
    pub command: String,
    pub root_dir: String,
    pub pid: u32,
    pub alive: bool,
    pub initialized: bool,
    pub server_name: String,
    pub server_version: String,
    pub capabilities: String,
    pub pending_requests: usize,
}

/// Encode an already-resolved POSIX path (starting with `/`) into a `file://` URI
/// without consulting local `env::current_dir()`.
pub fn posix_path_to_file_uri(posix_path: &str) -> String {
    if posix_path.starts_with("file://") {
        return posix_path.to_string();
    }
    let clean = if posix_path.starts_with('/') {
        posix_path.to_string()
    } else {
        format!("/{posix_path}")
    };
    let mut encoded = String::from("file://");
    for b in clean.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' | b':' => {
                encoded.push(b as char)
            }
            _ => encoded.push_str(&format!("%{b:02X}")),
        }
    }
    encoded
}

/// Strip a remote URI prefix (such as `"ssh://user@host:2222"` or Emacs TRAMP `"/ssh:host:"`)
/// down to the remote server's local POSIX path (e.g. `"/work/proj/src/main.cpp"`).
pub fn strip_remote_uri_to_posix_path(path_or_uri: &str, remote_prefix: &str) -> String {
    let trimmed_prefix = remote_prefix.trim_end_matches('/');
    if !trimmed_prefix.is_empty()
        && let Some(rest) = path_or_uri.strip_prefix(trimmed_prefix)
    {
        if rest.is_empty() {
            return "/".to_string();
        }
        return if rest.starts_with('/') {
            rest.to_string()
        } else {
            format!("/{rest}")
        };
    }
    if let Some(after_scheme) = path_or_uri.strip_prefix("ssh://") {
        if let Some((_authority, rem)) = after_scheme.split_once('/') {
            return format!("/{rem}");
        }
        return "/".to_string();
    }
    if let Some(after_tramp) = path_or_uri.strip_prefix("/ssh:")
        && let Some((_authority, rem)) = after_tramp.split_once(':')
    {
        return if rem.starts_with('/') {
            rem.to_string()
        } else {
            format!("/{rem}")
        };
    }
    path_or_uri.to_string()
}

fn session_path_to_uri(file_path: &str, remote_prefix: &str) -> String {
    if remote_prefix.is_empty() {
        path_to_uri(file_path)
    } else {
        let server_path = strip_remote_uri_to_posix_path(file_path, remote_prefix);
        posix_path_to_file_uri(&server_path)
    }
}

fn remap_server_path_to_client(server_path: String, remote_prefix: &str) -> String {
    let pfx = remote_prefix.trim_end_matches('/');
    if pfx.is_empty() || server_path.starts_with("ssh://") || server_path.starts_with("/ssh:") {
        return server_path;
    }
    if server_path.starts_with('/') {
        format!("{pfx}{server_path}")
    } else {
        format!("{pfx}/{server_path}")
    }
}

fn remap_event_for_remote(ev: LspEvent, remote_prefix: &str) -> LspEvent {
    if remote_prefix.is_empty() {
        return ev;
    }
    match ev {
        LspEvent::Diagnostics { file_path, items } => LspEvent::Diagnostics {
            file_path: remap_server_path_to_client(file_path, remote_prefix),
            items,
        },
        LspEvent::Locations {
            req_id,
            kind,
            items,
        } => LspEvent::Locations {
            req_id,
            kind,
            items: items
                .into_iter()
                .map(|mut loc| {
                    loc.file_path = remap_server_path_to_client(loc.file_path, remote_prefix);
                    loc
                })
                .collect(),
        },
        LspEvent::WorkspaceEdit {
            req_id,
            label,
            edits,
        } => LspEvent::WorkspaceEdit {
            req_id,
            label,
            edits: edits
                .into_iter()
                .map(|mut te| {
                    te.file_path = remap_server_path_to_client(te.file_path, remote_prefix);
                    te
                })
                .collect(),
        },
        LspEvent::CodeActions { req_id, items } => LspEvent::CodeActions {
            req_id,
            items: items
                .into_iter()
                .map(|mut act| {
                    act.edits = act
                        .edits
                        .into_iter()
                        .map(|mut te| {
                            te.file_path = remap_server_path_to_client(te.file_path, remote_prefix);
                            te
                        })
                        .collect();
                    act
                })
                .collect(),
        },
        other => other,
    }
}

fn strip_win_verbatim(raw: &str) -> &str {
    raw.strip_prefix(r"\\?\").unwrap_or(raw)
}

fn is_windows_drive_path(raw: &str) -> bool {
    let b = raw.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Convert a filesystem path into a standard `file://` URI.
pub fn path_to_uri(path: &str) -> String {
    if path.starts_with("file://") {
        return path.to_string();
    }
    let clean_input = strip_win_verbatim(path);
    let abs_str = if is_windows_drive_path(clean_input)
        || clean_input.starts_with('/')
        || Path::new(clean_input).is_absolute()
    {
        clean_input.to_string()
    } else {
        let joined = env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(clean_input);
        strip_win_verbatim(&joined.to_string_lossy()).to_string()
    };

    let slash_norm = abs_str.replace('\\', "/");
    let with_leading_slash = if slash_norm.starts_with('/') {
        slash_norm
    } else {
        format!("/{slash_norm}")
    };

    let mut encoded = String::from("file://");
    for b in with_leading_slash.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' | b':' => {
                encoded.push(b as char)
            }
            _ => encoded.push_str(&format!("%{b:02X}")),
        }
    }
    encoded
}

/// Convert a `file://` URI back into a local filesystem path.
pub fn uri_to_path(uri: &str) -> String {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex_str) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(decoded) = u8::from_str_radix(hex_str, 16)
        {
            out.push(decoded);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    let decoded = String::from_utf8_lossy(&out).into_owned();
    let b = decoded.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        #[cfg(windows)]
        {
            return decoded[1..].replace('/', "\\");
        }
        #[cfg(not(windows))]
        {
            return decoded[1..].to_string();
        }
    }
    decoded
}

fn check_executable_with_win_exts(base: &Path) -> Option<String> {
    if is_executable_file(base) {
        return Some(base.to_string_lossy().into_owned());
    }
    #[cfg(windows)]
    {
        if base.extension().is_none() {
            let pathext = env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string());
            for raw_ext in pathext.split(';') {
                let ext = raw_ext.trim().trim_start_matches('.');
                if ext.is_empty() {
                    continue;
                }
                let mut cand_os = base.as_os_str().to_os_string();
                cand_os.push(".");
                cand_os.push(ext.to_ascii_lowercase());
                let cand = PathBuf::from(cand_os);
                if is_executable_file(&cand) {
                    return Some(cand.to_string_lossy().into_owned());
                }
            }
        }
    }
    None
}

/// Resolve an executable binary name on `$PATH` (or verify an explicit path).
/// Returns the resolved full path, or `""` if not found.
pub fn which_binary(bin: &str) -> String {
    let trimmed = bin.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let candidate_path = Path::new(trimmed);
    if trimmed.contains('/') || trimmed.contains('\\') || candidate_path.is_absolute() {
        return check_executable_with_win_exts(candidate_path).unwrap_or_default();
    }
    if let Some(path_os) = env::var_os("PATH") {
        for dir in env::split_paths(&path_os) {
            let full = dir.join(trimmed);
            if let Some(found) = check_executable_with_win_exts(&full) {
                return found;
            }
        }
    }
    #[cfg(windows)]
    {
        let sys_root = env::var("SystemRoot")
            .or_else(|_| env::var("WINDIR"))
            .unwrap_or_else(|_| r"C:\Windows".to_string());
        let root_pb = PathBuf::from(sys_root);
        for sys_dir in [
            root_pb.join("System32"),
            root_pb.join("System32").join("OpenSSH"),
            root_pb,
        ] {
            let full = sys_dir.join(trimmed);
            if let Some(found) = check_executable_with_win_exts(&full) {
                return found;
            }
        }
    }
    if let Some(home) = env::var_os("HOME").filter(|h| !h.is_empty()) {
        let home_dir = PathBuf::from(home);
        for rel in [".local/bin", ".cargo/bin"] {
            let full = home_dir.join(rel).join(trimmed);
            if let Some(found) = check_executable_with_win_exts(&full) {
                return found;
            }
        }
    }
    if matches!(trimmed, "nessa-lsp" | "nessa-fmt" | "nessa")
        && let Ok(cwd) = env::current_dir()
    {
        let mut cur: Option<&Path> = Some(cwd.as_path());
        while let Some(dir) = cur {
            let full = dir.join("nessa2/target/release").join(trimmed);
            if let Some(found) = check_executable_with_win_exts(&full) {
                return found;
            }
            cur = dir.parent();
        }
    }
    String::new()
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// Walk upward from `start_path` looking for any marker file/directory in `markers`
/// (such as `Cargo.toml`, `package.toml`, `pyproject.toml`, `go.mod`, `.git`).
pub fn find_project_root(start_path: &str, markers: &[String]) -> String {
    if start_path.trim().is_empty() {
        return env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string());
    }
    let p = Path::new(start_path);
    let start_dir = if p.is_dir() {
        p.to_path_buf()
    } else {
        p.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let canon_start = fs::canonicalize(&start_dir).unwrap_or(start_dir);
    let clean_start = PathBuf::from(strip_win_verbatim(&canon_start.to_string_lossy()));

    for marker in markers {
        if marker.is_empty() {
            continue;
        }
        let mut cur: Option<&Path> = Some(clean_start.as_path());
        while let Some(dir) = cur {
            if dir.join(marker).exists() {
                return dir.to_string_lossy().into_owned();
            }
            cur = dir.parent();
        }
    }

    clean_start.to_string_lossy().into_owned()
}

fn frame_json_rpc(payload: &JsonValue) -> Vec<u8> {
    let body = payload.to_json_string();
    let mut framed = Vec::with_capacity(body.len() + 32);
    let _ = write!(framed, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    framed
}

fn read_json_rpc_message<R: BufRead>(reader: &mut R) -> Result<Option<JsonValue>, String> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("LSP stdout header read error: {e}"))?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if content_length.is_some() {
                break;
            }
            continue;
        }
        if let Some((name, val)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("Content-Length")
        {
            content_length = val.trim().parse::<usize>().ok();
        }
    }

    let len = content_length.ok_or_else(|| "missing Content-Length header".to_string())?;
    if len > 64 * 1024 * 1024 {
        return Err(format!("LSP message exceeds 64 MiB limit ({len} bytes)"));
    }
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .map_err(|e| format!("LSP stdout body read error: {e}"))?;
    let text = String::from_utf8_lossy(&body);
    parse_json(&text).map(Some)
}

// ---------------------------------------------------------------------------
// LSP JSON -> Structured Event Decoders
// ---------------------------------------------------------------------------

fn severity_name(code: i64) -> &'static str {
    match code {
        1 => "ERROR",
        2 => "WARN",
        3 => "INFO",
        4 => "HINT",
        _ => "WARN",
    }
}

fn symbol_kind_name(kind: i64) -> &'static str {
    match kind {
        1 => "File",
        2 => "Module",
        3 => "Namespace",
        4 => "Package",
        5 => "Class",
        6 => "Method",
        7 => "Property",
        8 => "Field",
        9 => "Constructor",
        10 => "Enum",
        11 => "Interface",
        12 => "Function",
        13 => "Variable",
        14 => "Constant",
        22 => "EnumMember",
        23 => "Struct",
        _ => "Symbol",
    }
}

fn completion_kind_name(kind: i64) -> &'static str {
    match kind {
        2 => "Method",
        3 => "Function",
        4 => "Constructor",
        5 => "Field",
        6 => "Variable",
        7 => "Class",
        8 => "Interface",
        9 => "Module",
        10 => "Property",
        13 => "Enum",
        14 => "Keyword",
        15 => "Snippet",
        21 => "Constant",
        22 => "Struct",
        _ => "Text",
    }
}

fn extract_range(range_val: &JsonValue) -> (i64, i64, i64, i64) {
    let start = range_val.get("start");
    let end = range_val.get("end");
    let sr = start.and_then(|s| s.get_i64("line")).unwrap_or(0).max(0);
    let sc = start
        .and_then(|s| s.get_i64("character"))
        .unwrap_or(0)
        .max(0);
    let er = end.and_then(|s| s.get_i64("line")).unwrap_or(sr).max(sr);
    let ec = end
        .and_then(|s| s.get_i64("character"))
        .unwrap_or(sc)
        .max(0);
    (sr, sc, er, ec)
}

fn decode_publish_diagnostics(params: &JsonValue) -> Option<LspEvent> {
    let uri = params.get_str("uri")?;
    let file_path = uri_to_path(uri);
    let mut items = Vec::new();
    if let Some(arr) = params.get("diagnostics").and_then(JsonValue::as_array) {
        for d in arr {
            let (row, col, end_row, end_col) =
                d.get("range").map(extract_range).unwrap_or((0, 0, 0, 0));
            let sev = severity_name(d.get_i64("severity").unwrap_or(1)).to_string();
            let source = d.get_str("source").unwrap_or("lsp").to_string();
            let code = match d.get("code") {
                Some(JsonValue::Str(s)) => s.clone(),
                Some(JsonValue::Int(n)) => n.to_string(),
                _ => String::new(),
            };
            let message = d.get_str("message").unwrap_or("").to_string();
            if !message.is_empty() {
                items.push(LspDiagItem {
                    row,
                    col,
                    end_row,
                    end_col,
                    severity: sev,
                    source,
                    code,
                    message,
                });
            }
        }
    }
    Some(LspEvent::Diagnostics { file_path, items })
}

fn extract_marked_string(val: &JsonValue, out: &mut Vec<String>) {
    match val {
        JsonValue::Str(s) => {
            if !s.trim().is_empty() {
                out.push(s.clone());
            }
        }
        JsonValue::Array(arr) => {
            for item in arr {
                extract_marked_string(item, out);
            }
        }
        JsonValue::Object(_) => {
            if let Some(value) = val.get_str("value") {
                if let Some(lang) = val.get_str("language")
                    && !lang.is_empty()
                {
                    out.push(format!("```{lang}\n{value}\n```"));
                } else if !value.trim().is_empty() {
                    out.push(value.to_string());
                }
            }
        }
        _ => {}
    }
}

fn decode_hover(req_id: i64, result: &JsonValue) -> LspEvent {
    let mut parts = Vec::new();
    if let Some(contents) = result.get("contents") {
        extract_marked_string(contents, &mut parts);
    }
    LspEvent::Hover {
        req_id,
        markdown: parts.join("\n\n"),
    }
}

fn push_location_item(val: &JsonValue, out: &mut Vec<LspLocationItem>) {
    if let Some(uri) = val.get_str("uri") {
        let (row, col, end_row, end_col) =
            val.get("range").map(extract_range).unwrap_or((0, 0, 0, 0));
        out.push(LspLocationItem {
            file_path: uri_to_path(uri),
            row,
            col,
            end_row,
            end_col,
        });
    } else if let Some(target_uri) = val.get_str("targetUri") {
        let range_node = val
            .get("targetSelectionRange")
            .or_else(|| val.get("targetRange"));
        let (row, col, end_row, end_col) = range_node.map(extract_range).unwrap_or((0, 0, 0, 0));
        out.push(LspLocationItem {
            file_path: uri_to_path(target_uri),
            row,
            col,
            end_row,
            end_col,
        });
    }
}

fn decode_locations(req_id: i64, kind: &str, result: &JsonValue) -> LspEvent {
    let mut items = Vec::new();
    match result {
        JsonValue::Array(arr) => {
            for v in arr {
                push_location_item(v, &mut items);
            }
        }
        JsonValue::Object(_) => push_location_item(result, &mut items),
        _ => {}
    }
    LspEvent::Locations {
        req_id,
        kind: kind.to_string(),
        items,
    }
}

fn flatten_doc_symbols(arr: &[JsonValue], prefix: &str, out: &mut Vec<LspSymbolItem>) {
    for sym in arr {
        let Some(raw_name) = sym.get_str("name") else {
            continue;
        };
        let full_name = if prefix.is_empty() {
            raw_name.to_string()
        } else {
            format!("{prefix}::{raw_name}")
        };
        let kind = symbol_kind_name(sym.get_i64("kind").unwrap_or(12)).to_string();
        let detail = sym.get_str("detail").unwrap_or("").to_string();
        if let Some(loc) = sym.get("location") {
            let (row, col, end_row, _) =
                loc.get("range").map(extract_range).unwrap_or((0, 0, 0, 0));
            out.push(LspSymbolItem {
                name: full_name,
                kind,
                detail,
                row,
                col,
                end_row,
            });
        } else {
            let sel_range = sym.get("selectionRange").or_else(|| sym.get("range"));
            let full_range = sym.get("range").or(sel_range);
            let (row, col, _, _) = sel_range.map(extract_range).unwrap_or((0, 0, 0, 0));
            let (_, _, end_row, _) = full_range
                .map(extract_range)
                .unwrap_or((row, col, row, col));
            out.push(LspSymbolItem {
                name: full_name.clone(),
                kind,
                detail,
                row,
                col,
                end_row,
            });
            if let Some(children) = sym.get("children").and_then(JsonValue::as_array) {
                flatten_doc_symbols(children, &full_name, out);
            }
        }
    }
}

fn decode_symbols(req_id: i64, result: &JsonValue) -> LspEvent {
    let mut items = Vec::new();
    if let Some(arr) = result.as_array() {
        flatten_doc_symbols(arr, "", &mut items);
    }
    LspEvent::Symbols { req_id, items }
}

fn slice_utf16_range(text: &str, start_u16: usize, end_u16: usize) -> String {
    if end_u16 <= start_u16 {
        return String::new();
    }
    let mut u16_pos = 0usize;
    let mut byte_start = None;
    let mut byte_end = None;
    for (b_idx, ch) in text.char_indices() {
        if u16_pos >= start_u16 && byte_start.is_none() {
            byte_start = Some(b_idx);
        }
        if u16_pos >= end_u16 {
            byte_end = Some(b_idx);
            break;
        }
        u16_pos += ch.len_utf16();
    }
    let s = byte_start.unwrap_or(text.len());
    let e = byte_end.unwrap_or(text.len());
    if e >= s {
        text[s..e].to_string()
    } else {
        String::new()
    }
}

fn decode_completions(req_id: i64, result: &JsonValue) -> LspEvent {
    let raw_list = match result {
        JsonValue::Array(arr) => Some(arr.as_slice()),
        JsonValue::Object(_) => result.get("items").and_then(JsonValue::as_array),
        _ => None,
    };
    let mut items = Vec::new();
    if let Some(arr) = raw_list {
        for item in arr.iter().take(120) {
            let Some(label) = item.get_str("label") else {
                continue;
            };
            let insert_text = item
                .get("textEdit")
                .and_then(|te| te.get_str("newText"))
                .or_else(|| item.get_str("insertText"))
                .unwrap_or(label)
                .to_string();
            let kind = completion_kind_name(item.get_i64("kind").unwrap_or(1)).to_string();
            let raw_detail = item.get_str("detail").unwrap_or("").trim();
            let ld_detail = item
                .get("labelDetails")
                .and_then(|ld| ld.get_str("detail"))
                .unwrap_or("")
                .trim();
            let ld_desc = item
                .get("labelDetails")
                .and_then(|ld| ld.get_str("description"))
                .unwrap_or("")
                .trim();
            let detail = match (
                !ld_detail.is_empty(),
                !raw_detail.is_empty(),
                !ld_desc.is_empty(),
            ) {
                (true, true, _) if !raw_detail.contains(ld_detail) => {
                    format!("{ld_detail}  {raw_detail}")
                }
                (true, false, true) => format!("{ld_detail}  {ld_desc}"),
                (true, false, false) => ld_detail.to_string(),
                (_, true, true) if !raw_detail.contains(ld_desc) => {
                    format!("{raw_detail}  ({ld_desc})")
                }
                (_, true, _) => raw_detail.to_string(),
                (false, false, true) => ld_desc.to_string(),
                _ => String::new(),
            };
            let mut doc_parts = Vec::new();
            if let Some(doc_val) = item.get("documentation") {
                extract_marked_string(doc_val, &mut doc_parts);
            }
            items.push(LspCompletionItem {
                label: label.to_string(),
                insert_text,
                kind,
                detail,
                documentation: doc_parts.join("\n\n"),
            });
        }
    }
    LspEvent::Completions { req_id, items }
}

fn decode_signature_help(req_id: i64, result: &JsonValue) -> LspEvent {
    let Some(sigs) = result.get("signatures").and_then(JsonValue::as_array) else {
        return LspEvent::SignatureHelp {
            req_id,
            label: String::new(),
            documentation: String::new(),
            parameters: Vec::new(),
            active_param: 0,
        };
    };
    if sigs.is_empty() {
        return LspEvent::SignatureHelp {
            req_id,
            label: String::new(),
            documentation: String::new(),
            parameters: Vec::new(),
            active_param: 0,
        };
    }
    let active_sig_idx = result.get_i64("activeSignature").unwrap_or(0).max(0) as usize;
    let sig = sigs.get(active_sig_idx).unwrap_or_else(|| &sigs[0]);
    let label = sig.get_str("label").unwrap_or("").to_string();
    let mut doc_parts = Vec::new();
    if let Some(doc_val) = sig.get("documentation") {
        extract_marked_string(doc_val, &mut doc_parts);
    }
    let mut parameters = Vec::new();
    if let Some(parr) = sig.get("parameters").and_then(JsonValue::as_array) {
        for p in parr {
            match p.get("label") {
                Some(JsonValue::Str(ps)) => parameters.push(ps.clone()),
                Some(JsonValue::Array(pair)) if pair.len() >= 2 => {
                    let s = pair[0].as_i64().unwrap_or(0).max(0) as usize;
                    let e = pair[1].as_i64().unwrap_or(0).max(0) as usize;
                    parameters.push(slice_utf16_range(&label, s, e));
                }
                _ => parameters.push(String::new()),
            }
        }
    }
    let active_param = sig
        .get_i64("activeParameter")
        .or_else(|| result.get_i64("activeParameter"))
        .unwrap_or(0)
        .max(0);
    LspEvent::SignatureHelp {
        req_id,
        label,
        documentation: doc_parts.join("\n\n"),
        parameters,
        active_param,
    }
}

fn collect_workspace_edit_items(edit_obj: &JsonValue) -> Vec<LspTextEditItem> {
    let mut out = Vec::new();
    if let Some(changes) = edit_obj.get("changes").and_then(JsonValue::as_object) {
        for (uri, edits_val) in changes {
            let file_path = uri_to_path(uri);
            if let Some(arr) = edits_val.as_array() {
                for te in arr {
                    let (sr, sc, er, ec) =
                        te.get("range").map(extract_range).unwrap_or((0, 0, 0, 0));
                    let new_text = te.get_str("newText").unwrap_or("").to_string();
                    out.push(LspTextEditItem {
                        file_path: file_path.clone(),
                        start_row: sr,
                        start_col: sc,
                        end_row: er,
                        end_col: ec,
                        new_text,
                    });
                }
            }
        }
    }
    if let Some(doc_changes) = edit_obj
        .get("documentChanges")
        .and_then(JsonValue::as_array)
    {
        for dc in doc_changes {
            let Some(uri) = dc.get("textDocument").and_then(|td| td.get_str("uri")) else {
                continue;
            };
            let file_path = uri_to_path(uri);
            if let Some(arr) = dc.get("edits").and_then(JsonValue::as_array) {
                for te in arr {
                    let (sr, sc, er, ec) =
                        te.get("range").map(extract_range).unwrap_or((0, 0, 0, 0));
                    let new_text = te.get_str("newText").unwrap_or("").to_string();
                    out.push(LspTextEditItem {
                        file_path: file_path.clone(),
                        start_row: sr,
                        start_col: sc,
                        end_row: er,
                        end_col: ec,
                        new_text,
                    });
                }
            }
        }
    }
    out
}

fn decode_code_actions(req_id: i64, result: &JsonValue) -> LspEvent {
    let mut items = Vec::new();
    if let Some(arr) = result.as_array() {
        for ca in arr {
            let Some(title) = ca.get_str("title") else {
                continue;
            };
            let kind = ca.get_str("kind").unwrap_or("quickfix").to_string();
            let edits = ca
                .get("edit")
                .map(collect_workspace_edit_items)
                .unwrap_or_default();
            items.push(LspCodeActionItem {
                title: title.to_string(),
                kind,
                edits,
            });
        }
    }
    LspEvent::CodeActions { req_id, items }
}

fn summarize_capabilities(caps: &JsonValue) -> String {
    let checks = [
        ("hoverProvider", "hover"),
        ("definitionProvider", "def"),
        ("referencesProvider", "refs"),
        ("implementationProvider", "impl"),
        ("typeDefinitionProvider", "typedef"),
        ("documentSymbolProvider", "symbols"),
        ("completionProvider", "cmp"),
        ("signatureHelpProvider", "sig"),
        ("renameProvider", "rename"),
        ("codeActionProvider", "codeAction"),
        ("documentFormattingProvider", "format"),
    ];
    let mut active = Vec::new();
    for (key, short) in checks {
        if let Some(v) = caps.get(key)
            && !matches!(v, JsonValue::Null | JsonValue::Bool(false))
        {
            active.push(short);
        }
    }
    active.join(",")
}

// ---------------------------------------------------------------------------
// Session & Manager
// ---------------------------------------------------------------------------

struct SessionShared {
    command: String,
    root_dir: String,
    remote_prefix: String,
    pid: u32,
    alive: AtomicBool,
    initialized: AtomicBool,
    server_name: Mutex<String>,
    server_version: Mutex<String>,
    capabilities: Mutex<String>,
    pending_methods: Mutex<HashMap<i64, (String, String)>>,
    pre_init_queue: Mutex<Vec<Vec<u8>>>,
    events: Mutex<VecDeque<LspEvent>>,
}

struct LspSession {
    id: u64,
    tx: mpsc::Sender<Vec<u8>>,
    next_req_id: AtomicI64,
    child: Mutex<Child>,
    shared: Arc<SessionShared>,
}

pub struct LspManager {
    next_id: AtomicU64,
    sessions: Mutex<HashMap<u64, Arc<LspSession>>>,
    waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
            waker: Mutex::new(None),
        }
    }

    pub fn set_waker(&self, waker: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut guard) = self.waker.lock() {
            *guard = Some(waker);
        }
    }

    fn current_waker(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        self.waker.lock().ok().and_then(|g| g.clone())
    }

    pub fn spawn_server(
        &self,
        cmd_bin: &str,
        args: &[String],
        cwd: &str,
        root_dir: &str,
        init_options_json: &str,
    ) -> Result<u64, String> {
        let resolved = if cmd_bin.contains('/')
            || cmd_bin.contains('\\')
            || Path::new(cmd_bin).is_absolute()
        {
            cmd_bin.to_string()
        } else {
            let w = which_binary(cmd_bin);
            if w.is_empty() {
                return Err(format!("LSP server binary '{cmd_bin}' not found in PATH"));
            }
            w
        };

        let mut cmd = Command::new(&resolved);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let effective_cwd = if !cwd.is_empty() && Path::new(cwd).is_dir() {
            cwd
        } else if !root_dir.is_empty() && Path::new(root_dir).is_dir() {
            root_dir
        } else {
            "."
        };
        cmd.current_dir(effective_cwd);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn LSP server '{resolved}': {e}"))?;
        let pid = child.id();
        let child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open LSP stdin pipe".to_string())?;
        let child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to open LSP stdout pipe".to_string())?;
        let child_stderr = child.stderr.take();

        let (tx, rx) = mpsc::channel::<Vec<u8>>();

        // Writer thread: drains framed JSON-RPC messages into server stdin.
        thread::spawn(move || {
            let mut writer = child_stdin;
            while let Ok(bytes) = rx.recv() {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(SessionShared {
            command: resolved.clone(),
            root_dir: root_dir.to_string(),
            remote_prefix: String::new(),
            pid,
            alive: AtomicBool::new(true),
            initialized: AtomicBool::new(false),
            server_name: Mutex::new(
                Path::new(&resolved)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(cmd_bin)
                    .to_string(),
            ),
            server_version: Mutex::new(String::new()),
            capabilities: Mutex::new(String::new()),
            pending_methods: Mutex::new(HashMap::new()),
            pre_init_queue: Mutex::new(Vec::new()),
            events: Mutex::new(VecDeque::new()),
        });

        // Stderr log reader thread
        if let Some(stderr_pipe) = child_stderr {
            let shared_err = Arc::clone(&shared);
            thread::spawn(move || {
                let reader = BufReader::new(stderr_pipe);
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if !trimmed.is_empty()
                        && let Ok(mut q) = shared_err.events.lock()
                        && q.len() < 512
                    {
                        q.push_back(LspEvent::Log {
                            level: "stderr".to_string(),
                            message: trimmed.chars().take(240).collect(),
                        });
                    }
                }
            });
        }

        // Stdout JSON-RPC reader thread
        let shared_out = Arc::clone(&shared);
        let tx_for_reader = tx.clone();
        let waker = self.current_waker();
        thread::spawn(move || {
            let mut reader = BufReader::new(child_stdout);
            loop {
                match read_json_rpc_message(&mut reader) {
                    Ok(Some(msg)) => {
                        handle_incoming_rpc(&msg, &shared_out, &tx_for_reader);
                        if let Some(ref w) = waker {
                            w();
                        }
                    }
                    Ok(None) => break,
                    Err(err_msg) => {
                        if let Ok(mut q) = shared_out.events.lock() {
                            q.push_back(LspEvent::Log {
                                level: "error".to_string(),
                                message: err_msg,
                            });
                        }
                        break;
                    }
                }
            }
            shared_out.alive.store(false, Ordering::Release);
            if let Ok(mut q) = shared_out.events.lock() {
                q.push_back(LspEvent::Exited { exit_code: 0 });
            }
            if let Some(ref w) = waker {
                w();
            }
        });

        let session = Arc::new(LspSession {
            id,
            tx,
            next_req_id: AtomicI64::new(2),
            child: Mutex::new(child),
            shared,
        });

        // Immediately send request #1: `initialize`
        let init_opts = if init_options_json.trim().is_empty() {
            JsonValue::Null
        } else {
            parse_json(init_options_json).unwrap_or(JsonValue::Null)
        };
        let root_uri = path_to_uri(if root_dir.is_empty() {
            effective_cwd
        } else {
            root_dir
        });
        let root_name = Path::new(if root_dir.is_empty() {
            effective_cwd
        } else {
            root_dir
        })
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();

        let init_req = build_initialize_request(1, &root_uri, &root_name, init_opts);
        if let Ok(mut pending) = session.shared.pending_methods.lock() {
            pending.insert(1, ("initialize".to_string(), String::new()));
        }
        let _ = session.tx.send(frame_json_rpc(&init_req));

        if let Ok(mut map) = self.sessions.lock() {
            map.insert(id, session);
        }
        Ok(id)
    }

    /// Spawn a remote language server over an arbitrary stdio transport (such as
    /// `ssh <opts> user@host "cd '/remote/root' && exec clangd"` or
    /// `lava-server --lsp-proxy /remote/root clangd`) with automatic bidirectional
    /// `remote_prefix` (`"ssh://user@host"`) <-> `file://` URI translation.
    pub fn spawn_remote_server(
        &self,
        transport_bin: &str,
        transport_args: &[String],
        remote_prefix: &str,
        remote_root_dir: &str,
        display_server_name: &str,
        init_options_json: &str,
    ) -> Result<u64, String> {
        let resolved = if transport_bin.contains('/') || Path::new(transport_bin).is_absolute() {
            transport_bin.to_string()
        } else {
            let w = which_binary(transport_bin);
            if w.is_empty() {
                return Err(format!(
                    "remote LSP transport binary '{transport_bin}' not found in PATH"
                ));
            }
            w
        };

        let clean_prefix = remote_prefix.trim().trim_end_matches('/').to_string();
        let server_root = if remote_root_dir.trim().is_empty() {
            "/".to_string()
        } else {
            strip_remote_uri_to_posix_path(remote_root_dir.trim(), &clean_prefix)
        };
        let display_root = if clean_prefix.is_empty() {
            server_root.clone()
        } else {
            remap_server_path_to_client(server_root.clone(), &clean_prefix)
        };

        let mut cmd = Command::new(&resolved);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.args(transport_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "failed to spawn remote LSP transport '{resolved}' for '{display_server_name}': {e}"
            )
        })?;
        let pid = child.id();
        let child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open remote LSP stdin pipe".to_string())?;
        let child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to open remote LSP stdout pipe".to_string())?;
        let child_stderr = child.stderr.take();

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut writer = child_stdin;
            while let Ok(bytes) = rx.recv() {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let fallback_name = if display_server_name.trim().is_empty() {
            "remote-lsp"
        } else {
            display_server_name.trim()
        };
        let shared = Arc::new(SessionShared {
            command: format!("{resolved} {}", transport_args.join(" ")),
            root_dir: display_root,
            remote_prefix: clean_prefix,
            pid,
            alive: AtomicBool::new(true),
            initialized: AtomicBool::new(false),
            server_name: Mutex::new(fallback_name.to_string()),
            server_version: Mutex::new(String::new()),
            capabilities: Mutex::new(String::new()),
            pending_methods: Mutex::new(HashMap::new()),
            pre_init_queue: Mutex::new(Vec::new()),
            events: Mutex::new(VecDeque::new()),
        });

        if let Some(stderr_pipe) = child_stderr {
            let shared_err = Arc::clone(&shared);
            thread::spawn(move || {
                let reader = BufReader::new(stderr_pipe);
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if !trimmed.is_empty()
                        && let Ok(mut q) = shared_err.events.lock()
                        && q.len() < 512
                    {
                        q.push_back(LspEvent::Log {
                            level: "stderr".to_string(),
                            message: trimmed.chars().take(240).collect(),
                        });
                    }
                }
            });
        }

        let shared_out = Arc::clone(&shared);
        let tx_for_reader = tx.clone();
        let waker = self.current_waker();
        thread::spawn(move || {
            let mut reader = BufReader::new(child_stdout);
            loop {
                match read_json_rpc_message(&mut reader) {
                    Ok(Some(msg)) => {
                        handle_incoming_rpc(&msg, &shared_out, &tx_for_reader);
                        if let Some(ref w) = waker {
                            w();
                        }
                    }
                    Ok(None) => break,
                    Err(err_msg) => {
                        if let Ok(mut q) = shared_out.events.lock() {
                            q.push_back(LspEvent::Log {
                                level: "error".to_string(),
                                message: err_msg,
                            });
                        }
                        break;
                    }
                }
            }
            shared_out.alive.store(false, Ordering::Release);
            if let Ok(mut q) = shared_out.events.lock() {
                q.push_back(LspEvent::Exited { exit_code: 0 });
            }
            if let Some(ref w) = waker {
                w();
            }
        });

        let session = Arc::new(LspSession {
            id,
            tx,
            next_req_id: AtomicI64::new(2),
            child: Mutex::new(child),
            shared,
        });

        let init_opts = if init_options_json.trim().is_empty() {
            JsonValue::Null
        } else {
            parse_json(init_options_json).unwrap_or(JsonValue::Null)
        };
        let root_uri = posix_path_to_file_uri(&server_root);
        let root_name = Path::new(&server_root)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("remote-workspace")
            .to_string();

        let init_req = build_initialize_request(1, &root_uri, &root_name, init_opts);
        if let Ok(mut pending) = session.shared.pending_methods.lock() {
            pending.insert(1, ("initialize".to_string(), String::new()));
        }
        let _ = session.tx.send(frame_json_rpc(&init_req));

        if let Ok(mut map) = self.sessions.lock() {
            map.insert(id, session);
        }
        Ok(id)
    }

    fn get_session(&self, id: u64) -> Result<Arc<LspSession>, String> {
        let guard = self
            .sessions
            .lock()
            .map_err(|_| "LSP manager lock poisoned".to_string())?;
        guard
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("unknown LSP session id {id}"))
    }

    fn send_or_queue(sess: &LspSession, framed: Vec<u8>) -> Result<(), String> {
        if sess.shared.initialized.load(Ordering::Acquire) {
            return sess
                .tx
                .send(framed)
                .map_err(|_| "LSP stdin pipe closed".to_string());
        }
        let mut q = sess
            .shared
            .pre_init_queue
            .lock()
            .map_err(|_| "LSP pre-init queue lock poisoned".to_string())?;
        if sess.shared.initialized.load(Ordering::Acquire) {
            drop(q);
            return sess
                .tx
                .send(framed)
                .map_err(|_| "LSP stdin pipe closed".to_string());
        }
        q.push(framed);
        Ok(())
    }

    pub fn notify_did_open(
        &self,
        id: u64,
        file_path: &str,
        language_id: &str,
        version: i64,
        text: &str,
    ) -> Result<(), String> {
        let sess = self.get_session(id)?;
        let uri = session_path_to_uri(file_path, &sess.shared.remote_prefix);
        let msg = JsonValue::obj([
            ("jsonrpc", JsonValue::Str("2.0".to_string())),
            ("method", JsonValue::Str("textDocument/didOpen".to_string())),
            (
                "params",
                JsonValue::obj([(
                    "textDocument",
                    JsonValue::obj([
                        ("uri", JsonValue::Str(uri)),
                        ("languageId", JsonValue::Str(language_id.to_string())),
                        ("version", JsonValue::Int(version)),
                        ("text", JsonValue::Str(text.to_string())),
                    ]),
                )]),
            ),
        ]);
        Self::send_or_queue(&sess, frame_json_rpc(&msg))
    }

    pub fn notify_did_change(
        &self,
        id: u64,
        file_path: &str,
        version: i64,
        full_text: &str,
    ) -> Result<(), String> {
        let sess = self.get_session(id)?;
        let uri = session_path_to_uri(file_path, &sess.shared.remote_prefix);
        let msg = JsonValue::obj([
            ("jsonrpc", JsonValue::Str("2.0".to_string())),
            (
                "method",
                JsonValue::Str("textDocument/didChange".to_string()),
            ),
            (
                "params",
                JsonValue::obj([
                    (
                        "textDocument",
                        JsonValue::obj([
                            ("uri", JsonValue::Str(uri)),
                            ("version", JsonValue::Int(version)),
                        ]),
                    ),
                    (
                        "contentChanges",
                        JsonValue::Array(vec![JsonValue::obj([(
                            "text",
                            JsonValue::Str(full_text.to_string()),
                        )])]),
                    ),
                ]),
            ),
        ]);
        Self::send_or_queue(&sess, frame_json_rpc(&msg))
    }

    pub fn notify_did_save(&self, id: u64, file_path: &str, text: &str) -> Result<(), String> {
        let sess = self.get_session(id)?;
        let uri = session_path_to_uri(file_path, &sess.shared.remote_prefix);
        let msg = JsonValue::obj([
            ("jsonrpc", JsonValue::Str("2.0".to_string())),
            ("method", JsonValue::Str("textDocument/didSave".to_string())),
            (
                "params",
                JsonValue::obj([
                    (
                        "textDocument",
                        JsonValue::obj([("uri", JsonValue::Str(uri))]),
                    ),
                    ("text", JsonValue::Str(text.to_string())),
                ]),
            ),
        ]);
        Self::send_or_queue(&sess, frame_json_rpc(&msg))
    }

    pub fn notify_did_close(&self, id: u64, file_path: &str) -> Result<(), String> {
        let sess = self.get_session(id)?;
        let uri = session_path_to_uri(file_path, &sess.shared.remote_prefix);
        let msg = JsonValue::obj([
            ("jsonrpc", JsonValue::Str("2.0".to_string())),
            (
                "method",
                JsonValue::Str("textDocument/didClose".to_string()),
            ),
            (
                "params",
                JsonValue::obj([(
                    "textDocument",
                    JsonValue::obj([("uri", JsonValue::Str(uri))]),
                )]),
            ),
        ]);
        Self::send_or_queue(&sess, frame_json_rpc(&msg))
    }

    /// Send a standard LSP request (`hover`, `definition`, `implementation`, `typeDefinition`,
    /// `references`, `documentSymbol`, `completion`, `rename`, `codeAction`, `formatting`)
    /// and return its `req_id`.
    pub fn send_request(
        &self,
        id: u64,
        kind: &str,
        file_path: &str,
        row: i64,
        col: i64,
        extra: &str,
    ) -> Result<i64, String> {
        let sess = self.get_session(id)?;
        let req_id = sess.next_req_id.fetch_add(1, Ordering::Relaxed);
        let uri = session_path_to_uri(file_path, &sess.shared.remote_prefix);

        let td = JsonValue::obj([("uri", JsonValue::Str(uri.clone()))]);
        let pos = JsonValue::obj([
            ("line", JsonValue::Int(row.max(0))),
            ("character", JsonValue::Int(col.max(0))),
        ]);

        let (method, params) = match kind {
            "hover" => (
                "textDocument/hover",
                JsonValue::obj([("textDocument", td), ("position", pos)]),
            ),
            "definition" => (
                "textDocument/definition",
                JsonValue::obj([("textDocument", td), ("position", pos)]),
            ),
            "implementation" => (
                "textDocument/implementation",
                JsonValue::obj([("textDocument", td), ("position", pos)]),
            ),
            "typeDefinition" => (
                "textDocument/typeDefinition",
                JsonValue::obj([("textDocument", td), ("position", pos)]),
            ),
            "references" => (
                "textDocument/references",
                JsonValue::obj([
                    ("textDocument", td),
                    ("position", pos),
                    (
                        "context",
                        JsonValue::obj([("includeDeclaration", JsonValue::Bool(true))]),
                    ),
                ]),
            ),
            "documentSymbol" | "symbols" => (
                "textDocument/documentSymbol",
                JsonValue::obj([("textDocument", td)]),
            ),
            "completion" => (
                "textDocument/completion",
                JsonValue::obj([("textDocument", td), ("position", pos)]),
            ),
            "signatureHelp" => (
                "textDocument/signatureHelp",
                JsonValue::obj([("textDocument", td), ("position", pos)]),
            ),
            "rename" => (
                "textDocument/rename",
                JsonValue::obj([
                    ("textDocument", td),
                    ("position", pos),
                    ("newName", JsonValue::Str(extra.to_string())),
                ]),
            ),
            "codeAction" => (
                "textDocument/codeAction",
                JsonValue::obj([
                    ("textDocument", td),
                    (
                        "range",
                        JsonValue::obj([("start", pos.clone()), ("end", pos)]),
                    ),
                    (
                        "context",
                        JsonValue::obj([("diagnostics", JsonValue::Array(Vec::new()))]),
                    ),
                ]),
            ),
            "formatting" => (
                "textDocument/formatting",
                JsonValue::obj([
                    ("textDocument", td),
                    (
                        "options",
                        JsonValue::obj([
                            ("tabSize", JsonValue::Int(4)),
                            ("insertSpaces", JsonValue::Bool(true)),
                        ]),
                    ),
                ]),
            ),
            other => return Err(format!("unsupported LSP request kind '{other}'")),
        };

        if let Ok(mut pending) = sess.shared.pending_methods.lock() {
            pending.insert(req_id, (method.to_string(), uri));
        }

        let req_obj = JsonValue::obj([
            ("jsonrpc", JsonValue::Str("2.0".to_string())),
            ("id", JsonValue::Int(req_id)),
            ("method", JsonValue::Str(method.to_string())),
            ("params", params),
        ]);
        Self::send_or_queue(&sess, frame_json_rpc(&req_obj))?;
        Ok(req_id)
    }

    pub fn poll_events(&self, id: u64) -> Result<Vec<LspEvent>, String> {
        let sess = self.get_session(id)?;
        let mut q = sess
            .shared
            .events
            .lock()
            .map_err(|_| "LSP event queue lock poisoned".to_string())?;
        Ok(q.drain(..).collect())
    }

    pub fn status(&self, id: u64) -> Result<LspSessionStatus, String> {
        let sess = self.get_session(id)?;
        let server_name = sess
            .shared
            .server_name
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let server_version = sess
            .shared
            .server_version
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let capabilities = sess
            .shared
            .capabilities
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let pending_requests = sess
            .shared
            .pending_methods
            .lock()
            .map(|g| g.len())
            .unwrap_or(0);
        Ok(LspSessionStatus {
            id: sess.id,
            command: sess.shared.command.clone(),
            root_dir: sess.shared.root_dir.clone(),
            pid: sess.shared.pid,
            alive: sess.shared.alive.load(Ordering::Acquire),
            initialized: sess.shared.initialized.load(Ordering::Acquire),
            server_name,
            server_version,
            capabilities,
            pending_requests,
        })
    }

    pub fn stop(&self, id: u64) -> bool {
        let removed = self.sessions.lock().ok().and_then(|mut g| g.remove(&id));
        let Some(sess) = removed else {
            return false;
        };
        let shutdown_req = JsonValue::obj([
            ("jsonrpc", JsonValue::Str("2.0".to_string())),
            ("id", JsonValue::Int(999_999)),
            ("method", JsonValue::Str("shutdown".to_string())),
            ("params", JsonValue::Null),
        ]);
        let exit_notif = JsonValue::obj([
            ("jsonrpc", JsonValue::Str("2.0".to_string())),
            ("method", JsonValue::Str("exit".to_string())),
            ("params", JsonValue::Null),
        ]);
        let _ = sess.tx.send(frame_json_rpc(&shutdown_req));
        let _ = sess.tx.send(frame_json_rpc(&exit_notif));
        sess.shared.alive.store(false, Ordering::Release);
        if let Ok(mut child) = sess.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        true
    }

    pub fn stop_all(&self) {
        let ids: Vec<u64> = self
            .sessions
            .lock()
            .map(|g| g.keys().copied().collect())
            .unwrap_or_default();
        for id in ids {
            let _ = self.stop(id);
        }
    }
}

fn handle_incoming_rpc(msg: &JsonValue, shared: &Arc<SessionShared>, tx: &mpsc::Sender<Vec<u8>>) {
    // 1. Server-initiated request (has both `id` and `method`) -> reply `{ "result": null }`
    if let Some(req_id_val) = msg.get("id")
        && let Some(method) = msg.get_str("method")
    {
        let resp = JsonValue::obj([
            ("jsonrpc", JsonValue::Str("2.0".to_string())),
            ("id", req_id_val.clone()),
            ("result", JsonValue::Null),
        ]);
        let _ = tx.send(frame_json_rpc(&resp));
        if (method == "window/logMessage" || method == "window/showMessage")
            && let Some(params) = msg.get("params")
            && let Some(text) = params.get_str("message")
            && let Ok(mut q) = shared.events.lock()
        {
            q.push_back(LspEvent::Log {
                level: "server".to_string(),
                message: text.to_string(),
            });
        }
        return;
    }

    // 2. Notification (`method` without `id`)
    if let Some(method) = msg.get_str("method") {
        if method == "textDocument/publishDiagnostics" {
            if let Some(params) = msg.get("params")
                && let Some(ev) = decode_publish_diagnostics(params)
                && let Ok(mut q) = shared.events.lock()
            {
                q.push_back(remap_event_for_remote(ev, &shared.remote_prefix));
            }
        } else if (method == "window/logMessage" || method == "window/showMessage")
            && let Some(params) = msg.get("params")
            && let Some(text) = params.get_str("message")
            && let Ok(mut q) = shared.events.lock()
        {
            q.push_back(LspEvent::Log {
                level: "info".to_string(),
                message: text.to_string(),
            });
        }
        return;
    }

    // 3. Response to one of our client requests (`id` without `method`)
    let Some(req_id) = msg.get_i64("id") else {
        return;
    };
    let (method, req_uri) = shared
        .pending_methods
        .lock()
        .ok()
        .and_then(|mut p| p.remove(&req_id))
        .unwrap_or_default();

    if let Some(err_obj) = msg.get("error") {
        let emsg = err_obj
            .get_str("message")
            .unwrap_or("LSP error")
            .to_string();
        if let Ok(mut q) = shared.events.lock() {
            q.push_back(LspEvent::RequestError {
                req_id,
                method,
                message: emsg,
            });
        }
        return;
    }

    let result = msg.get("result").unwrap_or(&JsonValue::Null);
    if method == "initialize" || req_id == 1 {
        let mut sname = shared
            .server_name
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let mut sver = String::new();
        if let Some(info) = result.get("serverInfo") {
            if let Some(n) = info.get_str("name") {
                sname = n.to_string();
            }
            if let Some(v) = info.get_str("version") {
                sver = v.to_string();
            }
        }
        let caps_str = result
            .get("capabilities")
            .map(summarize_capabilities)
            .unwrap_or_default();
        if let Ok(mut g) = shared.server_name.lock() {
            *g = sname.clone();
        }
        if let Ok(mut g) = shared.server_version.lock() {
            *g = sver.clone();
        }
        if let Ok(mut g) = shared.capabilities.lock() {
            *g = caps_str.clone();
        }
        // Immediately send `initialized` notification, then flush any pre-init messages in FIFO order!
        let initialized_notif = JsonValue::obj([
            ("jsonrpc", JsonValue::Str("2.0".to_string())),
            ("method", JsonValue::Str("initialized".to_string())),
            ("params", JsonValue::Object(BTreeMap::new())),
        ]);
        if let Ok(mut pre_q) = shared.pre_init_queue.lock() {
            let _ = tx.send(frame_json_rpc(&initialized_notif));
            shared.initialized.store(true, Ordering::Release);
            for framed in pre_q.drain(..) {
                let _ = tx.send(framed);
            }
        } else {
            let _ = tx.send(frame_json_rpc(&initialized_notif));
            shared.initialized.store(true, Ordering::Release);
        }

        if let Ok(mut q) = shared.events.lock() {
            q.push_back(LspEvent::Initialized {
                server_name: sname,
                server_version: sver,
                capabilities: caps_str,
            });
        }
        return;
    }

    let decoded = match method.as_str() {
        "textDocument/hover" => Some(decode_hover(req_id, result)),
        "textDocument/definition" => Some(decode_locations(req_id, "definition", result)),
        "textDocument/implementation" => Some(decode_locations(req_id, "implementation", result)),
        "textDocument/typeDefinition" => Some(decode_locations(req_id, "typeDefinition", result)),
        "textDocument/references" => Some(decode_locations(req_id, "references", result)),
        "textDocument/documentSymbol" => Some(decode_symbols(req_id, result)),
        "textDocument/completion" => Some(decode_completions(req_id, result)),
        "textDocument/signatureHelp" => Some(decode_signature_help(req_id, result)),
        "textDocument/rename" => {
            let edits = collect_workspace_edit_items(result);
            Some(LspEvent::WorkspaceEdit {
                req_id,
                label: "rename".to_string(),
                edits,
            })
        }
        "textDocument/formatting" => {
            let file_path = uri_to_path(&req_uri);
            let mut edits = Vec::new();
            if let Some(arr) = result.as_array() {
                for te in arr {
                    let (sr, sc, er, ec) =
                        te.get("range").map(extract_range).unwrap_or((0, 0, 0, 0));
                    let new_text = te.get_str("newText").unwrap_or("").to_string();
                    edits.push(LspTextEditItem {
                        file_path: file_path.clone(),
                        start_row: sr,
                        start_col: sc,
                        end_row: er,
                        end_col: ec,
                        new_text,
                    });
                }
            }
            Some(LspEvent::WorkspaceEdit {
                req_id,
                label: "formatting".to_string(),
                edits,
            })
        }
        "textDocument/codeAction" => Some(decode_code_actions(req_id, result)),
        _ => None,
    };

    if let Some(ev) = decoded
        && let Ok(mut q) = shared.events.lock()
    {
        q.push_back(remap_event_for_remote(ev, &shared.remote_prefix));
    }
}

fn build_initialize_request(
    req_id: i64,
    root_uri: &str,
    root_name: &str,
    init_options: JsonValue,
) -> JsonValue {
    let text_doc_caps = JsonValue::obj([
        (
            "synchronization",
            JsonValue::obj([
                ("didSave", JsonValue::Bool(true)),
                ("willSave", JsonValue::Bool(false)),
            ]),
        ),
        (
            "hover",
            JsonValue::obj([(
                "contentFormat",
                JsonValue::Array(vec![
                    JsonValue::Str("markdown".to_string()),
                    JsonValue::Str("plaintext".to_string()),
                ]),
            )]),
        ),
        (
            "completion",
            JsonValue::obj([(
                "completionItem",
                JsonValue::obj([
                    ("snippetSupport", JsonValue::Bool(false)),
                    ("labelDetailsSupport", JsonValue::Bool(true)),
                    (
                        "documentationFormat",
                        JsonValue::Array(vec![
                            JsonValue::Str("markdown".to_string()),
                            JsonValue::Str("plaintext".to_string()),
                        ]),
                    ),
                ]),
            )]),
        ),
        (
            "signatureHelp",
            JsonValue::obj([(
                "signatureInformation",
                JsonValue::obj([
                    (
                        "documentationFormat",
                        JsonValue::Array(vec![
                            JsonValue::Str("markdown".to_string()),
                            JsonValue::Str("plaintext".to_string()),
                        ]),
                    ),
                    (
                        "parameterInformation",
                        JsonValue::obj([("labelOffsetSupport", JsonValue::Bool(true))]),
                    ),
                ]),
            )]),
        ),
        (
            "documentSymbol",
            JsonValue::obj([("hierarchicalDocumentSymbolSupport", JsonValue::Bool(true))]),
        ),
        (
            "publishDiagnostics",
            JsonValue::obj([("relatedInformation", JsonValue::Bool(true))]),
        ),
    ]);

    let workspace_caps = JsonValue::obj([
        ("workspaceFolders", JsonValue::Bool(true)),
        (
            "workspaceEdit",
            JsonValue::obj([("documentChanges", JsonValue::Bool(true))]),
        ),
    ]);

    JsonValue::obj([
        ("jsonrpc", JsonValue::Str("2.0".to_string())),
        ("id", JsonValue::Int(req_id)),
        ("method", JsonValue::Str("initialize".to_string())),
        (
            "params",
            JsonValue::obj([
                ("processId", JsonValue::Int(i64::from(std::process::id()))),
                (
                    "clientInfo",
                    JsonValue::obj([
                        ("name", JsonValue::Str("Lava".to_string())),
                        ("version", JsonValue::Str("0.1.0".to_string())),
                    ]),
                ),
                ("rootUri", JsonValue::Str(root_uri.to_string())),
                (
                    "workspaceFolders",
                    JsonValue::Array(vec![JsonValue::obj([
                        ("uri", JsonValue::Str(root_uri.to_string())),
                        ("name", JsonValue::Str(root_name.to_string())),
                    ])]),
                ),
                ("initializationOptions", init_options),
                (
                    "capabilities",
                    JsonValue::obj([
                        ("textDocument", text_doc_caps),
                        ("workspace", workspace_caps),
                    ]),
                ),
            ]),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn json_roundtrip_handles_unicode_escapes_and_nested_lsp_objects() {
        let obj = JsonValue::obj([
            ("jsonrpc", JsonValue::Str("2.0".to_string())),
            ("id", JsonValue::Int(42)),
            (
                "message",
                JsonValue::Str("中文诊断: line 1\n\"quoted\"".to_string()),
            ),
            ("ok", JsonValue::Bool(true)),
        ]);
        let encoded = obj.to_json_string();
        let parsed = parse_json(&encoded).expect("parse encoded json");
        assert_eq!(parsed, obj);

        let surrogate = parse_json("{\"emoji\":\"\\uD83D\\uDE80\"}").expect("surrogate pair");
        assert_eq!(surrogate.get_str("emoji"), Some("🚀"));
    }

    #[test]
    fn completion_docs_label_details_and_signature_help_decode_cleanly() {
        let cmp_json = parse_json(
            r#"{
                "isIncomplete": false,
                "items": [{
                    "label": "clamp_pos",
                    "kind": 2,
                    "detail": "fn(self, pos: Pos, insert_mode: bool) -> Pos",
                    "labelDetails": { "detail": "(pos, insert_mode)", "description": "ext_document" },
                    "documentation": { "kind": "markdown", "value": "Clamps `pos` into valid buffer bounds." }
                }]
            }"#,
        )
        .expect("parse completion json");
        let LspEvent::Completions { req_id, items } = decode_completions(7, &cmp_json) else {
            panic!("expected Completions event");
        };
        assert_eq!(req_id, 7);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "clamp_pos");
        assert_eq!(items[0].kind, "Method");
        assert!(
            items[0]
                .detail
                .contains("fn(self, pos: Pos, insert_mode: bool) -> Pos")
        );
        assert!(items[0].documentation.contains("Clamps `pos`"));

        let sig_json = parse_json(
            r#"{
                "signatures": [{
                    "label": "fn calc(alpha: i64, beta: String) -> bool",
                    "documentation": "Computes result.",
                    "parameters": [
                        { "label": [8, 18] },
                        { "label": "beta: String" }
                    ],
                    "activeParameter": 1
                }],
                "activeSignature": 0
            }"#,
        )
        .expect("parse signatureHelp json");
        let LspEvent::SignatureHelp {
            req_id: sreq,
            label,
            documentation,
            parameters,
            active_param,
        } = decode_signature_help(9, &sig_json)
        else {
            panic!("expected SignatureHelp event");
        };
        assert_eq!(sreq, 9);
        assert_eq!(label, "fn calc(alpha: i64, beta: String) -> bool");
        assert_eq!(documentation, "Computes result.");
        assert_eq!(
            parameters,
            vec!["alpha: i64".to_string(), "beta: String".to_string()]
        );
        assert_eq!(active_param, 1);
    }

    #[test]
    fn uri_and_root_detection_roundtrip_cleanly() {
        let uri = path_to_uri("/tmp/my project/main.rs");
        assert_eq!(uri, "file:///tmp/my%20project/main.rs");
        assert_eq!(uri_to_path(&uri), "/tmp/my project/main.rs");

        let win_uri = path_to_uri(r"C:\Users\alice\my project\main.rs");
        assert_eq!(win_uri, "file:///C:/Users/alice/my%20project/main.rs");
        #[cfg(windows)]
        {
            assert_eq!(uri_to_path(&win_uri), r"C:\Users\alice\my project\main.rs");
            assert!(!which_binary("cmd").is_empty());
        }
        #[cfg(not(windows))]
        {
            assert_eq!(uri_to_path(&win_uri), "C:/Users/alice/my project/main.rs");
            assert!(!which_binary("sh").is_empty());
        }
    }

    #[test]
    fn real_stdio_lsp_handshake_diagnostics_hover_and_definition_over_pipes() {
        let py = which_binary("python3");
        if py.is_empty() {
            return;
        }
        let server_py = r#"
import sys, json

def read_msg():
    cl = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        s = line.decode('utf-8').strip()
        if not s:
            if cl is not None:
                break
            continue
        if s.lower().startswith('content-length:'):
            cl = int(s.split(':', 1)[1].strip())
    body = sys.stdin.buffer.read(cl)
    return json.loads(body.decode('utf-8'))

def send_msg(obj):
    raw = json.dumps(obj).encode('utf-8')
    hdr = f'Content-Length: {len(raw)}\r\n\r\n'.encode('utf-8')
    sys.stdout.buffer.write(hdr + raw)
    sys.stdout.buffer.flush()

while True:
    m = read_msg()
    if m is None:
        break
    method = m.get('method', '')
    mid = m.get('id')
    if method == 'initialize':
        send_msg({
            'jsonrpc': '2.0',
            'id': mid,
            'result': {
                'serverInfo': {'name': 'mock-py-lsp', 'version': '1.2.0'},
                'capabilities': {'hoverProvider': True, 'definitionProvider': True, 'completionProvider': {}}
            }
        })
    elif method == 'textDocument/didOpen':
        uri = m['params']['textDocument']['uri']
        send_msg({
            'jsonrpc': '2.0',
            'method': 'textDocument/publishDiagnostics',
            'params': {
                'uri': uri,
                'diagnostics': [{
                    'range': {'start': {'line': 2, 'character': 4}, 'end': {'line': 2, 'character': 9}},
                    'severity': 1,
                    'source': 'mock-py-lsp',
                    'message': 'undefined symbol foo'
                }]
            }
        })
    elif method == 'textDocument/hover':
        send_msg({
            'jsonrpc': '2.0',
            'id': mid,
            'result': {'contents': {'kind': 'markdown', 'value': '```rust\nfn demo() -> i64\n```'}}
        })
    elif method == 'textDocument/definition':
        send_msg({
            'jsonrpc': '2.0',
            'id': mid,
            'result': [{'uri': 'file:///tmp/demo.rs', 'range': {'start': {'line': 10, 'character': 3}, 'end': {'line': 10, 'character': 7}}}]
        })
    elif method == 'exit':
        break
"#;

        let mgr = LspManager::new();
        let woke = Arc::new(AtomicU64::new(0));
        let woke_clone = Arc::clone(&woke);
        mgr.set_waker(Arc::new(move || {
            woke_clone.fetch_add(1, Ordering::Relaxed);
        }));

        let sid = mgr
            .spawn_server(
                "python3",
                &["-u".to_string(), "-c".to_string(), server_py.to_string()],
                "/tmp",
                "/tmp",
                "",
            )
            .expect("spawn python3 mock LSP");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut got_init = false;
        while Instant::now() < deadline && !got_init {
            for ev in mgr.poll_events(sid).expect("poll") {
                if let LspEvent::Initialized {
                    server_name,
                    capabilities,
                    ..
                } = ev
                {
                    assert_eq!(server_name, "mock-py-lsp");
                    assert!(capabilities.contains("hover"));
                    assert!(capabilities.contains("def"));
                    got_init = true;
                }
            }
            if !got_init {
                thread::sleep(Duration::from_millis(15));
            }
        }
        assert!(got_init, "expected Initialized event from stdio LSP");

        mgr.notify_did_open(sid, "/tmp/demo.rs", "rust", 1, "fn main() {}\n")
            .expect("didOpen");
        let hover_req = mgr
            .send_request(sid, "hover", "/tmp/demo.rs", 0, 3, "")
            .expect("hover");
        let def_req = mgr
            .send_request(sid, "definition", "/tmp/demo.rs", 0, 3, "")
            .expect("definition");

        let mut got_diag = false;
        let mut got_hover = false;
        let mut got_def = false;
        let deadline2 = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline2 && !(got_diag && got_hover && got_def) {
            for ev in mgr.poll_events(sid).expect("poll2") {
                match ev {
                    LspEvent::Diagnostics { file_path, items } => {
                        assert_eq!(file_path, "/tmp/demo.rs");
                        assert_eq!(items.len(), 1);
                        assert_eq!(items[0].message, "undefined symbol foo");
                        got_diag = true;
                    }
                    LspEvent::Hover { req_id, markdown } => {
                        assert_eq!(req_id, hover_req);
                        assert!(markdown.contains("fn demo() -> i64"));
                        got_hover = true;
                    }
                    LspEvent::Locations {
                        req_id,
                        kind,
                        items,
                    } => {
                        assert_eq!(req_id, def_req);
                        assert_eq!(kind, "definition");
                        assert_eq!(items[0].row, 10);
                        got_def = true;
                    }
                    _ => {}
                }
            }
            if !(got_diag && got_hover && got_def) {
                thread::sleep(Duration::from_millis(15));
            }
        }
        assert!(got_diag && got_hover && got_def);
        assert!(woke.load(Ordering::Relaxed) >= 2);
        assert!(mgr.stop(sid));
    }

    #[test]
    fn remote_lsp_translates_ssh_uris_to_server_file_uris_and_back() {
        let py = which_binary("python3");
        if py.is_empty() {
            return;
        }
        let server_py = r#"
import sys, json

def read_msg():
    cl = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        s = line.decode('utf-8').strip()
        if not s:
            if cl is not None:
                break
            continue
        if s.lower().startswith('content-length:'):
            cl = int(s.split(':', 1)[1].strip())
    body = sys.stdin.buffer.read(cl)
    return json.loads(body.decode('utf-8'))

def send_msg(obj):
    raw = json.dumps(obj).encode('utf-8')
    hdr = f'Content-Length: {len(raw)}\r\n\r\n'.encode('utf-8')
    sys.stdout.buffer.write(hdr + raw)
    sys.stdout.buffer.flush()

while True:
    m = read_msg()
    if m is None:
        break
    method = m.get('method', '')
    mid = m.get('id')
    if method == 'initialize':
        root_uri = m['params']['rootUri']
        assert root_uri == 'file:///srv/cpp_proj', f'unexpected rootUri: {root_uri}'
        send_msg({
            'jsonrpc': '2.0',
            'id': mid,
            'result': {
                'serverInfo': {'name': 'clangd', 'version': '18.1.0'},
                'capabilities': {'hoverProvider': True, 'definitionProvider': True, 'renameProvider': True}
            }
        })
    elif method == 'textDocument/didOpen':
        uri = m['params']['textDocument']['uri']
        assert uri == 'file:///srv/cpp_proj/src/main.cpp', f'unexpected didOpen uri: {uri}'
        send_msg({
            'jsonrpc': '2.0',
            'method': 'textDocument/publishDiagnostics',
            'params': {
                'uri': 'file:///srv/cpp_proj/src/main.cpp',
                'diagnostics': [{
                    'range': {'start': {'line': 4, 'character': 2}, 'end': {'line': 4, 'character': 8}},
                    'severity': 1,
                    'source': 'clangd',
                    'message': 'undeclared identifier'
                }]
            }
        })
    elif method == 'textDocument/definition':
        uri = m['params']['textDocument']['uri']
        assert uri == 'file:///srv/cpp_proj/src/main.cpp', f'unexpected definition uri: {uri}'
        send_msg({
            'jsonrpc': '2.0',
            'id': mid,
            'result': [{'uri': 'file:///srv/cpp_proj/include/engine.hpp', 'range': {'start': {'line': 12, 'character': 6}, 'end': {'line': 12, 'character': 12}}}]
        })
    elif method == 'exit':
        break
"#;

        let mgr = LspManager::new();
        let sid = mgr
            .spawn_remote_server(
                "python3",
                &["-u".to_string(), "-c".to_string(), server_py.to_string()],
                "ssh://alice@devbox:2222",
                "/srv/cpp_proj",
                "clangd",
                "",
            )
            .expect("spawn remote mock clangd");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut got_init = false;
        while Instant::now() < deadline && !got_init {
            for ev in mgr.poll_events(sid).expect("poll init") {
                if let LspEvent::Initialized { server_name, .. } = ev {
                    assert_eq!(server_name, "clangd");
                    got_init = true;
                }
            }
            if !got_init {
                thread::sleep(Duration::from_millis(15));
            }
        }
        assert!(got_init);

        mgr.notify_did_open(
            sid,
            "ssh://alice@devbox:2222/srv/cpp_proj/src/main.cpp",
            "cpp",
            1,
            "#include \"engine.hpp\"\nint main() { return 0; }\n",
        )
        .expect("remote didOpen");
        let def_req = mgr
            .send_request(
                sid,
                "definition",
                "ssh://alice@devbox:2222/srv/cpp_proj/src/main.cpp",
                0,
                10,
                "",
            )
            .expect("remote definition");

        let mut got_diag = false;
        let mut got_def = false;
        let deadline2 = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline2 && !(got_diag && got_def) {
            for ev in mgr.poll_events(sid).expect("poll remote events") {
                match ev {
                    LspEvent::Diagnostics { file_path, items } => {
                        assert_eq!(
                            file_path,
                            "ssh://alice@devbox:2222/srv/cpp_proj/src/main.cpp"
                        );
                        assert_eq!(items.len(), 1);
                        assert_eq!(items[0].message, "undeclared identifier");
                        got_diag = true;
                    }
                    LspEvent::Locations { req_id, items, .. } => {
                        assert_eq!(req_id, def_req);
                        assert_eq!(
                            items[0].file_path,
                            "ssh://alice@devbox:2222/srv/cpp_proj/include/engine.hpp"
                        );
                        assert_eq!(items[0].row, 12);
                        got_def = true;
                    }
                    _ => {}
                }
            }
            if !(got_diag && got_def) {
                thread::sleep(Duration::from_millis(15));
            }
        }
        assert!(got_diag && got_def);
        let st = mgr.status(sid).expect("status");
        assert_eq!(st.root_dir, "ssh://alice@devbox:2222/srv/cpp_proj");
        assert!(mgr.stop(sid));
    }
}
