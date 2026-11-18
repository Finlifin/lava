//! Shared `.gitignore` / `.ignore` / glob rule compiler and fast directory walker for Lava's
//! bundled Windows 10 1709+ `rg.exe` and `fd.exe` tools.

use std::fs;
use std::path::{Path, PathBuf};

use regex::{Regex, RegexBuilder};

/// Converts a gitignore / CLI glob pattern (`*`, `?`, `**`, `[abc]`, `{a,b}`) into a [`Regex`].
pub fn compile_glob_regex(
    glob: &str,
    case_insensitive: bool,
    anchored_slash: bool,
) -> Option<Regex> {
    let clean = glob.replace('\\', "/");
    let trimmed = clean.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut rx = String::with_capacity(trimmed.len() * 2 + 16);
    if anchored_slash || trimmed.starts_with('/') {
        rx.push('^');
    } else if !trimmed.contains('/') {
        rx.push_str("(?:^|/)");
    } else {
        rx.push('^');
    }

    let pat = trimmed.trim_start_matches('/');
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0usize;
    let mut brace_depth = 0usize;

    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    if i + 2 < chars.len() && chars[i + 2] == '/' {
                        rx.push_str("(?:.*/)?");
                        i += 3;
                    } else {
                        rx.push_str(".*");
                        i += 2;
                    }
                } else {
                    rx.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                rx.push_str("[^/]");
                i += 1;
            }
            '[' => {
                rx.push('[');
                i += 1;
                if i < chars.len() && (chars[i] == '!' || chars[i] == '^') {
                    rx.push('^');
                    i += 1;
                }
                while i < chars.len() && chars[i] != ']' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        rx.push('\\');
                        rx.push(chars[i + 1]);
                        i += 2;
                    } else {
                        rx.push(chars[i]);
                        i += 1;
                    }
                }
                rx.push(']');
                if i < chars.len() {
                    i += 1;
                }
            }
            '{' => {
                brace_depth += 1;
                rx.push_str("(?:");
                i += 1;
            }
            '}' if brace_depth > 0 => {
                brace_depth -= 1;
                rx.push(')');
                i += 1;
            }
            ',' if brace_depth > 0 => {
                rx.push('|');
                i += 1;
            }
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '\\' => {
                rx.push('\\');
                rx.push(ch);
                i += 1;
            }
            other => {
                rx.push(other);
                i += 1;
            }
        }
    }

    rx.push('$');
    RegexBuilder::new(&rx)
        .case_insensitive(case_insensitive)
        .build()
        .ok()
}

#[derive(Clone)]
struct IgnoreRule {
    negated: bool,
    dir_only: bool,
    regex: Regex,
}

#[derive(Clone, Default)]
pub struct IgnoreStack {
    rules: Vec<IgnoreRule>,
}

impl IgnoreStack {
    pub fn with_dir_ignore_files(&self, dir: &Path, extra_names: &[&str]) -> Self {
        let mut next = self.clone();
        for name in [".gitignore", ".ignore"]
            .into_iter()
            .chain(extra_names.iter().copied())
        {
            let p = dir.join(name);
            if let Ok(content) = fs::read_to_string(&p) {
                next.append_gitignore_text(&content);
            }
        }
        next
    }

    pub fn append_gitignore_text(&mut self, text: &str) {
        for raw_line in text.lines() {
            let line = raw_line.trim_end_matches('\r');
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (negated, rest) = if let Some(after) = trimmed.strip_prefix('!') {
                (true, after.trim())
            } else {
                (false, trimmed)
            };
            if rest.is_empty() {
                continue;
            }
            let (dir_only, pat) = if let Some(stripped) = rest.strip_suffix('/') {
                (true, stripped)
            } else {
                (false, rest)
            };
            let anchored = pat.starts_with('/') || pat[..pat.len()].contains('/');
            if let Some(regex) = compile_glob_regex(pat, false, anchored) {
                self.rules.push(IgnoreRule {
                    negated,
                    dir_only,
                    regex,
                });
            }
        }
    }

    pub fn is_ignored(&self, rel_slash_path: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            if rule.regex.is_match(rel_slash_path) {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

#[derive(Clone)]
pub struct WalkOptions {
    pub include_hidden: bool,
    pub respect_ignore: bool,
    pub max_depth: Option<usize>,
    pub min_depth: usize,
    pub extra_ignore_files: Vec<&'static str>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            include_hidden: false,
            respect_ignore: true,
            max_depth: None,
            min_depth: 1,
            extra_ignore_files: Vec::new(),
        }
    }
}

pub struct WalkEntry {
    pub path: PathBuf,
    pub rel_slash: String,
    pub file_name: String,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub depth: usize,
}

pub fn walk_root<F>(root: &Path, opts: &WalkOptions, on_entry: &mut F) -> bool
where
    F: FnMut(WalkEntry) -> bool,
{
    if root.is_file() {
        let fname = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned());
        let is_symlink = fs::symlink_metadata(root)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        return on_entry(WalkEntry {
            path: root.to_path_buf(),
            rel_slash: fname.clone(),
            file_name: fname,
            is_dir: false,
            is_file: true,
            is_symlink,
            depth: 0,
        });
    }

    let mut init_stack = IgnoreStack::default();
    if opts.respect_ignore {
        init_stack = init_stack.with_dir_ignore_files(root, &opts.extra_ignore_files);
    }
    walk_dir_rec(root, root, 1, &init_stack, opts, on_entry)
}

fn walk_dir_rec<F>(
    root: &Path,
    dir: &Path,
    depth: usize,
    ignores: &IgnoreStack,
    opts: &WalkOptions,
    on_entry: &mut F,
) -> bool
where
    F: FnMut(WalkEntry) -> bool,
{
    if let Some(max_d) = opts.max_depth
        && depth > max_d
    {
        return true;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return true;
    };

    let mut entries: Vec<fs::DirEntry> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let fname_os = entry.file_name();
        let fname = fname_os.to_string_lossy().into_owned();
        if fname == ".git" {
            continue;
        }
        if !opts.include_hidden && fname.starts_with('.') {
            continue;
        }

        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let is_symlink = ft.is_symlink();
        let is_dir = ft.is_dir();
        let is_file = ft.is_file();

        let rel_slash = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| fname.clone());

        if opts.respect_ignore && ignores.is_ignored(&rel_slash, is_dir) {
            continue;
        }

        if depth >= opts.min_depth
            && !on_entry(WalkEntry {
                path: path.clone(),
                rel_slash: rel_slash.clone(),
                file_name: fname,
                is_dir,
                is_file,
                is_symlink,
                depth,
            })
        {
            return false;
        }

        if is_dir {
            let child_ignores = if opts.respect_ignore {
                ignores.with_dir_ignore_files(&path, &opts.extra_ignore_files)
            } else {
                ignores.clone()
            };
            if !walk_dir_rec(root, &path, depth + 1, &child_ignores, opts, on_entry) {
                return false;
            }
        }
    }
    true
}

/// Maps common `rg -t <type>` short names to file extensions.
pub fn extensions_for_type(type_name: &str) -> &'static [&'static str] {
    match type_name.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => &["rs"],
        "nessa" | "ns" => &["ns"],
        "c" => &["c", "h"],
        "cpp" | "c++" | "cxx" => &["cpp", "cc", "cxx", "hpp", "hh", "hxx", "h"],
        "py" | "python" => &["py", "pyi"],
        "js" | "javascript" => &["js", "jsx", "mjs", "cjs"],
        "ts" | "typescript" => &["ts", "tsx", "mts", "cts"],
        "go" | "golang" => &["go"],
        "json" => &["json", "jsonc", "jsonl"],
        "toml" => &["toml"],
        "yaml" | "yml" => &["yaml", "yml"],
        "md" | "markdown" => &["md", "markdown"],
        "sh" | "shell" => &["sh", "bash", "zsh"],
        "html" => &["html", "htm"],
        "css" => &["css", "scss"],
        "lua" => &["lua"],
        _ => &[],
    }
}

pub fn format_display_path(path: &Path, path_sep: Option<&str>) -> String {
    let raw = path.to_string_lossy();
    let trimmed = raw.strip_prefix("./").unwrap_or(&raw);
    if let Some(sep) = path_sep {
        trimmed.replace(['/', '\\'], sep)
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_and_gitignore_negation_rules_match_accurately() {
        let mut ig = IgnoreStack::default();
        ig.append_gitignore_text("*.log\n!important.log\nbuild/\n");
        assert!(ig.is_ignored("debug.log", false));
        assert!(ig.is_ignored("sub/debug.log", false));
        assert!(!ig.is_ignored("important.log", false));
        assert!(ig.is_ignored("build", true));
        assert!(!ig.is_ignored("build", false));
    }
}
