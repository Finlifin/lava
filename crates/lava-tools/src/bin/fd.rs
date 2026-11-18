//! Bundled `fd` (fd-find compatible CLI) for Lava Windows 10 1709+ distributions.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lava_tools::{WalkOptions, compile_glob_regex, format_display_path, walk_root};
use regex::{Regex, RegexBuilder};

#[derive(Default)]
struct FdTypeFilter {
    file: bool,
    dir: bool,
    symlink: bool,
    empty: bool,
    executable: bool,
}

impl FdTypeFilter {
    fn any_set(&self) -> bool {
        self.file || self.dir || self.symlink || self.empty || self.executable
    }

    fn matches(&self, path: &Path, is_dir: bool, is_file: bool, is_symlink: bool) -> bool {
        if !self.any_set() {
            return true;
        }
        if self.file && is_file {
            return true;
        }
        if self.dir && is_dir {
            return true;
        }
        if self.symlink && is_symlink {
            return true;
        }
        if self.empty {
            if is_file && fs::metadata(path).map(|m| m.len() == 0).unwrap_or(false) {
                return true;
            }
            if is_dir
                && fs::read_dir(path)
                    .map(|mut rd| rd.next().is_none())
                    .unwrap_or(false)
            {
                return true;
            }
        }
        if self.executable && is_file {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if fs::metadata(path)
                    .map(|m| (m.permissions().mode() & 0o111) != 0)
                    .unwrap_or(false)
                {
                    return true;
                }
            }
            #[cfg(windows)]
            {
                if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| {
                        matches!(
                            ext.to_ascii_lowercase().as_str(),
                            "exe" | "cmd" | "bat" | "com" | "ps1"
                        )
                    })
                {
                    return true;
                }
            }
        }
        false
    }
}

struct FdOptions {
    hidden: bool,
    respect_ignore: bool,
    ignore_case: bool,
    case_sensitive: bool,
    glob_mode: bool,
    fixed_strings: bool,
    full_path: bool,
    absolute_path: bool,
    null_sep: bool,
    max_depth: Option<usize>,
    min_depth: usize,
    max_results: Option<usize>,
    path_sep: Option<String>,
    types: FdTypeFilter,
    extensions: Vec<String>,
    excludes: Vec<Regex>,
    pattern: Option<String>,
    paths: Vec<PathBuf>,
    show_help: bool,
    show_version: bool,
}

impl Default for FdOptions {
    fn default() -> Self {
        Self {
            hidden: false,
            respect_ignore: true,
            ignore_case: false,
            case_sensitive: false,
            glob_mode: false,
            fixed_strings: false,
            full_path: false,
            absolute_path: false,
            null_sep: false,
            max_depth: None,
            min_depth: 1,
            max_results: None,
            path_sep: None,
            types: FdTypeFilter::default(),
            extensions: Vec::new(),
            excludes: Vec::new(),
            pattern: None,
            paths: Vec::new(),
            show_help: false,
            show_version: false,
        }
    }
}

fn add_type_spec(tf: &mut FdTypeFilter, raw: &str) {
    for part in raw.split(',') {
        match part.trim().to_ascii_lowercase().as_str() {
            "f" | "file" => tf.file = true,
            "d" | "dir" | "directory" => tf.dir = true,
            "l" | "symlink" => tf.symlink = true,
            "e" | "empty" => tf.empty = true,
            "x" | "executable" => tf.executable = true,
            _ => {}
        }
    }
}

fn parse_args() -> Result<FdOptions, String> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut opts = FdOptions::default();
    let mut positional_only = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut explicit_search_paths: Vec<PathBuf> = Vec::new();
    let mut i = 0usize;

    while i < args.len() {
        let arg = &args[i];
        if positional_only {
            positionals.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "--" {
            positional_only = true;
            i += 1;
            continue;
        }
        if !arg.starts_with('-') {
            positionals.push(arg.clone());
            i += 1;
            continue;
        }

        match arg.as_str() {
            "-h" | "--help" => {
                opts.show_help = true;
                return Ok(opts);
            }
            "-V" | "--version" => {
                opts.show_version = true;
                return Ok(opts);
            }
            "-H" | "--hidden" => opts.hidden = true,
            "-I" | "--no-ignore" | "--no-ignore-vcs" => opts.respect_ignore = false,
            "-u" | "--unrestricted" => {
                opts.hidden = true;
                opts.respect_ignore = false;
            }
            "-i" | "--ignore-case" => {
                opts.ignore_case = true;
                opts.case_sensitive = false;
            }
            "-s" | "--case-sensitive" => {
                opts.case_sensitive = true;
                opts.ignore_case = false;
            }
            "-g" | "--glob" => opts.glob_mode = true,
            "-F" | "--fixed-strings" => opts.fixed_strings = true,
            "-p" | "--full-path" => opts.full_path = true,
            "-a" | "--absolute-path" => opts.absolute_path = true,
            "-0" | "--print0" => opts.null_sep = true,
            "-1" => opts.max_results = Some(1),
            "--color" => {
                i += 1;
            }
            s if s.starts_with("--color=") => {}
            "-t" | "--type" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| "missing value after -t/--type".to_string())?;
                add_type_spec(&mut opts.types, val);
                i += 1;
            }
            s if s.starts_with("--type=") => {
                add_type_spec(&mut opts.types, &s["--type=".len()..]);
            }
            "-e" | "--extension" => {
                let ext = args
                    .get(i + 1)
                    .ok_or_else(|| "missing value after -e/--extension".to_string())?;
                opts.extensions
                    .push(ext.trim().trim_start_matches('.').to_ascii_lowercase());
                i += 1;
            }
            "-E" | "--exclude" => {
                let pat = args
                    .get(i + 1)
                    .ok_or_else(|| "missing glob after -E/--exclude".to_string())?;
                if let Some(rx) = compile_glob_regex(pat, false, false) {
                    opts.excludes.push(rx);
                }
                i += 1;
            }
            "-d" | "--max-depth" => {
                let d = args
                    .get(i + 1)
                    .and_then(|v| v.parse::<usize>().ok())
                    .ok_or_else(|| "invalid --max-depth".to_string())?;
                opts.max_depth = Some(d);
                i += 1;
            }
            "--min-depth" => {
                let d = args
                    .get(i + 1)
                    .and_then(|v| v.parse::<usize>().ok())
                    .ok_or_else(|| "invalid --min-depth".to_string())?;
                opts.min_depth = d;
                i += 1;
            }
            "--max-results" => {
                let n = args
                    .get(i + 1)
                    .and_then(|v| v.parse::<usize>().ok())
                    .ok_or_else(|| "invalid --max-results".to_string())?;
                opts.max_results = Some(n);
                i += 1;
            }
            "--search-path" => {
                let p = args
                    .get(i + 1)
                    .ok_or_else(|| "missing --search-path".to_string())?;
                explicit_search_paths.push(PathBuf::from(p));
                i += 1;
            }
            "--path-separator" => {
                let sep = args
                    .get(i + 1)
                    .ok_or_else(|| "missing --path-separator".to_string())?;
                opts.path_sep = Some(sep.clone());
                i += 1;
            }
            short if short.starts_with('-') && !short.starts_with("--") && short.len() > 2 => {
                for ch in short[1..].chars() {
                    match ch {
                        'H' => opts.hidden = true,
                        'I' => opts.respect_ignore = false,
                        'u' => {
                            opts.hidden = true;
                            opts.respect_ignore = false;
                        }
                        'i' => opts.ignore_case = true,
                        's' => opts.case_sensitive = true,
                        'g' => opts.glob_mode = true,
                        'F' => opts.fixed_strings = true,
                        'p' => opts.full_path = true,
                        'a' => opts.absolute_path = true,
                        '0' => opts.null_sep = true,
                        '1' => opts.max_results = Some(1),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }

    if !explicit_search_paths.is_empty() {
        opts.pattern = positionals.into_iter().next();
        opts.paths = explicit_search_paths;
    } else if !positionals.is_empty() {
        opts.pattern = Some(positionals[0].clone());
        opts.paths = positionals[1..].iter().map(PathBuf::from).collect();
    }
    if opts.paths.is_empty() {
        opts.paths.push(PathBuf::from("."));
    }

    Ok(opts)
}

fn compile_fd_pattern(opts: &FdOptions) -> Result<Option<Regex>, String> {
    let Some(raw_pat) = &opts.pattern else {
        return Ok(None);
    };
    if raw_pat.is_empty() || raw_pat == "." {
        return Ok(None);
    }
    let ci = if opts.case_sensitive {
        false
    } else if opts.ignore_case {
        true
    } else {
        !raw_pat.chars().any(char::is_uppercase)
    };

    if opts.glob_mode {
        return compile_glob_regex(raw_pat, ci, true)
            .map(Some)
            .ok_or_else(|| format!("fd: invalid glob pattern `{raw_pat}`"));
    }
    let expr = if opts.fixed_strings {
        regex::escape(raw_pat)
    } else {
        raw_pat.clone()
    };
    RegexBuilder::new(&expr)
        .case_insensitive(ci)
        .build()
        .map(Some)
        .map_err(|e| format!("fd: invalid regex `{raw_pat}`: {e}"))
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    if opts.show_version {
        println!("fd 10.2.0 (lava-tools bundled win10-1709)");
        return ExitCode::SUCCESS;
    }
    if opts.show_help {
        println!(
            "fd (lava-tools) — Fast .gitignore-aware file & directory finder\n\
             Usage: fd [OPTIONS] [PATTERN] [PATH...]"
        );
        return ExitCode::SUCCESS;
    }

    let pat_rx = match compile_fd_pattern(&opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let walk_opts = WalkOptions {
        include_hidden: opts.hidden,
        respect_ignore: opts.respect_ignore,
        max_depth: opts.max_depth,
        min_depth: opts.min_depth,
        extra_ignore_files: vec![".fdignore"],
    };

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let term = if opts.null_sep { '\0' } else { '\n' };
    let mut emitted = 0usize;

    for root in &opts.paths {
        walk_root(root, &walk_opts, &mut |entry| {
            if !opts.excludes.is_empty()
                && opts.excludes.iter().any(|rx| rx.is_match(&entry.rel_slash))
            {
                return true;
            }
            if !opts
                .types
                .matches(&entry.path, entry.is_dir, entry.is_file, entry.is_symlink)
            {
                return true;
            }
            if !opts.extensions.is_empty() {
                let ext = entry
                    .path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_default();
                if !opts.extensions.iter().any(|want| want == &ext) {
                    return true;
                }
            }
            if let Some(rx) = &pat_rx {
                let hay = if opts.full_path {
                    entry.rel_slash.as_str()
                } else {
                    entry.file_name.as_str()
                };
                if !rx.is_match(hay) {
                    return true;
                }
            }

            let out_path = if opts.absolute_path {
                fs::canonicalize(&entry.path).unwrap_or_else(|_| entry.path.clone())
            } else if root == Path::new(".") {
                PathBuf::from(&entry.rel_slash)
            } else {
                entry.path.clone()
            };
            #[cfg_attr(not(windows), allow(unused_mut))]
            let mut disp = format_display_path(&out_path, opts.path_sep.as_deref());
            #[cfg(windows)]
            if let Some(stripped) = disp.strip_prefix(r"\\?\") {
                disp = stripped.to_string();
            }
            if write!(out, "{disp}{term}").is_err() {
                return false;
            }
            emitted += 1;
            opts.max_results.is_none_or(|cap| emitted < cap)
        });
        if opts.max_results.is_some_and(|cap| emitted >= cap) {
            break;
        }
    }

    let _ = out.flush();
    ExitCode::SUCCESS
}
