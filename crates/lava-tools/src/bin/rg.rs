//! Bundled `rg` (ripgrep-compatible CLI) for Lava Windows 10 1709+ distributions.

use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lava_tools::{
    WalkOptions, compile_glob_regex, extensions_for_type, format_display_path, walk_root,
};
use memchr::memchr;
use regex::{Regex, RegexBuilder};

struct RgOptions {
    files_mode: bool,
    line_number: Option<bool>,
    column: bool,
    with_filename: Option<bool>,
    ignore_case: bool,
    case_sensitive: bool,
    smart_case: bool,
    fixed_strings: bool,
    invert_match: bool,
    word_regexp: bool,
    files_with_matches: bool,
    count_matches: bool,
    only_matching: bool,
    quiet: bool,
    null_sep: bool,
    text_binary: bool,
    hidden: bool,
    respect_ignore: bool,
    max_count: Option<usize>,
    max_depth: Option<usize>,
    path_sep: Option<String>,
    patterns: Vec<String>,
    include_globs: Vec<Regex>,
    exclude_globs: Vec<Regex>,
    include_exts: Vec<String>,
    exclude_exts: Vec<String>,
    paths: Vec<String>,
    show_help: bool,
    show_version: bool,
}

impl Default for RgOptions {
    fn default() -> Self {
        Self {
            files_mode: false,
            line_number: None,
            column: false,
            with_filename: None,
            ignore_case: false,
            case_sensitive: false,
            smart_case: false,
            fixed_strings: false,
            invert_match: false,
            word_regexp: false,
            files_with_matches: false,
            count_matches: false,
            only_matching: false,
            quiet: false,
            null_sep: false,
            text_binary: false,
            hidden: false,
            respect_ignore: true,
            max_count: None,
            max_depth: None,
            path_sep: None,
            patterns: Vec::new(),
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            include_exts: Vec::new(),
            exclude_exts: Vec::new(),
            paths: Vec::new(),
            show_help: false,
            show_version: false,
        }
    }
}

fn push_glob_filter(opts: &mut RgOptions, raw: &str, ci: bool) {
    let trimmed = raw.trim();
    if let Some(excl) = trimmed.strip_prefix('!') {
        if let Some(rx) = compile_glob_regex(excl, ci, false) {
            opts.exclude_globs.push(rx);
        }
    } else if let Some(rx) = compile_glob_regex(trimmed, ci, false) {
        opts.include_globs.push(rx);
    }
}

fn parse_args() -> Result<RgOptions, String> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut opts = RgOptions::default();
    let mut positional_only = false;
    let mut positionals: Vec<String> = Vec::new();
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
        if arg == "-" || !arg.starts_with('-') {
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
            "--files" => opts.files_mode = true,
            "-n" | "--line-number" => opts.line_number = Some(true),
            "-N" | "--no-line-number" => opts.line_number = Some(false),
            "--column" => opts.column = true,
            "--no-column" => opts.column = false,
            "--vimgrep" => {
                opts.line_number = Some(true);
                opts.column = true;
                opts.with_filename = Some(true);
            }
            "-H" | "--with-filename" => opts.with_filename = Some(true),
            "-I" | "--no-filename" => opts.with_filename = Some(false),
            "--no-heading" | "--heading" | "--line-buffered" | "--block-buffered" | "--mmap"
            | "--no-mmap" | "--no-config" | "--pcre2" => {}
            "-i" | "--ignore-case" => {
                opts.ignore_case = true;
                opts.case_sensitive = false;
            }
            "-s" | "--case-sensitive" => {
                opts.case_sensitive = true;
                opts.ignore_case = false;
            }
            "-S" | "--smart-case" => opts.smart_case = true,
            "-F" | "--fixed-strings" => opts.fixed_strings = true,
            "-v" | "--invert-match" => opts.invert_match = true,
            "-w" | "--word-regexp" => opts.word_regexp = true,
            "-l" | "--files-with-matches" => opts.files_with_matches = true,
            "-c" | "--count" => opts.count_matches = true,
            "-o" | "--only-matching" => opts.only_matching = true,
            "-q" | "--quiet" => opts.quiet = true,
            "-0" | "--null" => opts.null_sep = true,
            "-a" | "--text" => opts.text_binary = true,
            "-." | "--hidden" => opts.hidden = true,
            "--no-ignore" | "--no-ignore-vcs" => opts.respect_ignore = false,
            "-u" | "--unrestricted" => {
                opts.respect_ignore = false;
                opts.hidden = true;
            }
            "-uu" | "-uuu" => {
                opts.respect_ignore = false;
                opts.hidden = true;
                opts.text_binary = true;
            }
            "--color" | "--colors" => {
                i += 1;
            }
            s if s.starts_with("--color=") || s.starts_with("--colors=") => {}
            "-e" | "--regexp" => {
                let pat = args
                    .get(i + 1)
                    .ok_or_else(|| "missing pattern after -e/--regexp".to_string())?;
                opts.patterns.push(pat.clone());
                i += 1;
            }
            s if s.starts_with("--regexp=") => {
                opts.patterns.push(s["--regexp=".len()..].to_string());
            }
            "-g" | "--glob" => {
                let g = args
                    .get(i + 1)
                    .ok_or_else(|| "missing glob after -g/--glob".to_string())?;
                push_glob_filter(&mut opts, g, false);
                i += 1;
            }
            s if s.starts_with("--glob=") => {
                push_glob_filter(&mut opts, &s["--glob=".len()..], false);
            }
            "--iglob" => {
                let g = args
                    .get(i + 1)
                    .ok_or_else(|| "missing glob after --iglob".to_string())?;
                push_glob_filter(&mut opts, g, true);
                i += 1;
            }
            "-t" | "--type" => {
                let t = args
                    .get(i + 1)
                    .ok_or_else(|| "missing type after -t/--type".to_string())?;
                for ext in extensions_for_type(t) {
                    opts.include_exts.push((*ext).to_string());
                }
                i += 1;
            }
            "-T" | "--type-not" => {
                let t = args
                    .get(i + 1)
                    .ok_or_else(|| "missing type after -T/--type-not".to_string())?;
                for ext in extensions_for_type(t) {
                    opts.exclude_exts.push((*ext).to_string());
                }
                i += 1;
            }
            "-m" | "--max-count" => {
                let n = args
                    .get(i + 1)
                    .and_then(|v| v.parse::<usize>().ok())
                    .ok_or_else(|| "invalid --max-count".to_string())?;
                opts.max_count = Some(n);
                i += 1;
            }
            "--max-depth" | "-d" => {
                let n = args
                    .get(i + 1)
                    .and_then(|v| v.parse::<usize>().ok())
                    .ok_or_else(|| "invalid --max-depth".to_string())?;
                opts.max_depth = Some(n);
                i += 1;
            }
            "--path-separator" => {
                let sep = args
                    .get(i + 1)
                    .ok_or_else(|| "missing --path-separator value".to_string())?;
                opts.path_sep = Some(sep.clone());
                i += 1;
            }
            short if short.starts_with('-') && !short.starts_with("--") && short.len() > 2 => {
                // Combined short flags like `-ni`, `-rn`, `-iv`
                for ch in short[1..].chars() {
                    match ch {
                        'n' => opts.line_number = Some(true),
                        'N' => opts.line_number = Some(false),
                        'i' => opts.ignore_case = true,
                        's' => opts.case_sensitive = true,
                        'S' => opts.smart_case = true,
                        'F' => opts.fixed_strings = true,
                        'v' => opts.invert_match = true,
                        'w' => opts.word_regexp = true,
                        'l' => opts.files_with_matches = true,
                        'c' => opts.count_matches = true,
                        'o' => opts.only_matching = true,
                        'q' => opts.quiet = true,
                        'a' => opts.text_binary = true,
                        '.' => opts.hidden = true,
                        'u' => {
                            opts.respect_ignore = false;
                            opts.hidden = true;
                        }
                        'H' => opts.with_filename = Some(true),
                        'I' => opts.with_filename = Some(false),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }

    if opts.files_mode {
        opts.paths = positionals;
    } else if opts.patterns.is_empty() {
        if let Some(first) = positionals.first() {
            opts.patterns.push(first.clone());
            opts.paths = positionals.into_iter().skip(1).collect();
        }
    } else {
        opts.paths = positionals;
    }

    Ok(opts)
}

fn build_matcher(opts: &RgOptions) -> Result<Regex, String> {
    if opts.patterns.is_empty() {
        return Err("rg: missing search pattern".to_string());
    }
    let parts: Vec<String> = opts
        .patterns
        .iter()
        .map(|p| {
            let body = if opts.fixed_strings {
                regex::escape(p)
            } else {
                p.clone()
            };
            if opts.word_regexp {
                format!(r"\b(?:{body})\b")
            } else {
                format!("(?:{body})")
            }
        })
        .collect();
    let combined = parts.join("|");
    let effective_ci = if opts.case_sensitive {
        false
    } else if opts.ignore_case {
        true
    } else if opts.smart_case {
        !opts
            .patterns
            .iter()
            .any(|p| p.chars().any(char::is_uppercase))
    } else {
        false
    };

    RegexBuilder::new(&combined)
        .case_insensitive(effective_ci)
        .build()
        .map_err(|e| format!("rg: regex parse error: {e}"))
}

fn file_passes_filters(rel_slash: &str, path: &Path, opts: &RgOptions) -> bool {
    if !opts.exclude_globs.is_empty() && opts.exclude_globs.iter().any(|rx| rx.is_match(rel_slash))
    {
        return false;
    }
    if !opts.include_globs.is_empty() && !opts.include_globs.iter().any(|rx| rx.is_match(rel_slash))
    {
        return false;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !opts.exclude_exts.is_empty() && opts.exclude_exts.iter().any(|x| x == &ext) {
        return false;
    }
    if !opts.include_exts.is_empty() && !opts.include_exts.iter().any(|x| x == &ext) {
        return false;
    }
    true
}

fn search_reader<R: BufRead, W: Write>(
    mut reader: R,
    display_path: Option<&str>,
    rx: &Regex,
    opts: &RgOptions,
    show_line_num: bool,
    out: &mut W,
) -> io::Result<usize> {
    let mut line_buf = String::new();
    let mut row_1 = 0usize;
    let mut matches_found = 0usize;
    let term = if opts.null_sep { '\0' } else { '\n' };

    loop {
        line_buf.clear();
        let n = reader.read_line(&mut line_buf)?;
        if n == 0 {
            break;
        }
        row_1 += 1;
        let line = line_buf.trim_end_matches(['\r', '\n']);

        let first_match = rx.find(line);
        let matched = if opts.invert_match {
            first_match.is_none()
        } else {
            first_match.is_some()
        };
        if !matched {
            continue;
        }
        matches_found += 1;

        if opts.quiet {
            return Ok(matches_found);
        }
        if opts.files_with_matches {
            if let Some(p) = display_path {
                write!(out, "{p}{term}")?;
            }
            return Ok(matches_found);
        }
        if opts.count_matches {
            if let Some(max_c) = opts.max_count
                && matches_found >= max_c
            {
                break;
            }
            continue;
        }

        if opts.only_matching && !opts.invert_match {
            for m in rx.find_iter(line) {
                if let Some(p) = display_path {
                    write!(out, "{p}:")?;
                }
                if show_line_num {
                    write!(out, "{row_1}:")?;
                }
                if opts.column {
                    let col_1 = line[..m.start()].chars().count() + 1;
                    write!(out, "{col_1}:")?;
                }
                writeln!(out, "{}", m.as_str())?;
            }
        } else {
            if let Some(p) = display_path {
                write!(out, "{p}:")?;
            }
            if show_line_num {
                write!(out, "{row_1}:")?;
            }
            if opts.column {
                let col_1 = first_match
                    .map(|m| line[..m.start()].chars().count() + 1)
                    .unwrap_or(1);
                write!(out, "{col_1}:")?;
            }
            writeln!(out, "{line}")?;
        }

        if let Some(max_c) = opts.max_count
            && matches_found >= max_c
        {
            break;
        }
    }

    if opts.count_matches && matches_found > 0 {
        if let Some(p) = display_path {
            writeln!(out, "{p}:{matches_found}")?;
        } else {
            writeln!(out, "{matches_found}")?;
        }
    }

    Ok(matches_found)
}

fn search_file<W: Write>(
    path: &Path,
    display_path: Option<&str>,
    rx: &Regex,
    opts: &RgOptions,
    show_line_num: bool,
    out: &mut W,
) -> usize {
    let Ok(mut file) = File::open(path) else {
        return 0;
    };
    if !opts.text_binary {
        let mut probe = [0u8; 4096];
        let Ok(n) = file.read(&mut probe) else {
            return 0;
        };
        if memchr(0, &probe[..n]).is_some() {
            return 0;
        }
        // Re-open or seek to start
        let Ok(fresh) = File::open(path) else {
            return 0;
        };
        file = fresh;
    }
    let reader = BufReader::new(file);
    search_reader(reader, display_path, rx, opts, show_line_num, out).unwrap_or(0)
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
        println!("ripgrep 14.1.0 (lava-tools bundled win10-1709)");
        return ExitCode::SUCCESS;
    }
    if opts.show_help {
        println!(
            "rg (lava-tools) — Fast .gitignore-aware recursive search\n\
             Usage: rg [OPTIONS] PATTERN [PATH...]\n\
                    rg --files [OPTIONS] [PATH...]"
        );
        return ExitCode::SUCCESS;
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    let walk_opts = WalkOptions {
        include_hidden: opts.hidden,
        respect_ignore: opts.respect_ignore,
        max_depth: opts.max_depth,
        min_depth: 1,
        extra_ignore_files: vec![".rgignore"],
    };

    if opts.files_mode {
        let roots = if opts.paths.is_empty() {
            vec![PathBuf::from(".")]
        } else {
            opts.paths.iter().map(PathBuf::from).collect()
        };
        let term = if opts.null_sep { '\0' } else { '\n' };
        for root in &roots {
            walk_root(root, &walk_opts, &mut |entry| {
                if entry.is_file && file_passes_filters(&entry.rel_slash, &entry.path, &opts) {
                    let disp = if root == Path::new(".") {
                        format_display_path(Path::new(&entry.rel_slash), opts.path_sep.as_deref())
                    } else {
                        format_display_path(&entry.path, opts.path_sep.as_deref())
                    };
                    if write!(out, "{disp}{term}").is_err() {
                        return false;
                    }
                }
                true
            });
        }
        let _ = out.flush();
        return ExitCode::SUCCESS;
    }

    let rx = match build_matcher(&opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    // Stdin mode when no paths provided and stdin is piped, or explicit `-` path.
    let read_stdin = (opts.paths.is_empty() && !io::stdin().is_terminal())
        || opts.paths.iter().any(|p| p == "-");

    if read_stdin && opts.paths.len() <= 1 {
        let show_ln = opts.line_number.unwrap_or(false);
        let stdin = io::stdin();
        let hits = search_reader(stdin.lock(), None, &rx, &opts, show_ln, &mut out).unwrap_or(0);
        let _ = out.flush();
        return if hits > 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };
    }

    let roots: Vec<PathBuf> = if opts.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        opts.paths.iter().map(PathBuf::from).collect()
    };

    let single_file = roots.len() == 1 && roots[0].is_file();
    let show_filename = opts.with_filename.unwrap_or(!single_file);
    let show_line_num = opts.line_number.unwrap_or(true);

    let mut total_matches = 0usize;
    for root in &roots {
        walk_root(root, &walk_opts, &mut |entry| {
            if !entry.is_file || !file_passes_filters(&entry.rel_slash, &entry.path, &opts) {
                return true;
            }
            let disp = if root == Path::new(".") {
                format_display_path(Path::new(&entry.rel_slash), opts.path_sep.as_deref())
            } else {
                format_display_path(&entry.path, opts.path_sep.as_deref())
            };
            let disp_opt = if show_filename {
                Some(disp.as_str())
            } else {
                None
            };
            let hits = search_file(&entry.path, disp_opt, &rx, &opts, show_line_num, &mut out);
            total_matches += hits;
            !(opts.quiet && total_matches > 0)
        });
        if opts.quiet && total_matches > 0 {
            break;
        }
    }

    let _ = out.flush();
    if total_matches > 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
