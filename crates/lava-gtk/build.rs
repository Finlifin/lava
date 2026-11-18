use std::collections::HashSet;
use std::process::Command;

const PACKAGES: &[&str] = &[
    "gtk4",
    "pangocairo",
    "cairo",
    "pango",
    "glib-2.0",
    "gio-2.0",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_LIBDIR");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_SYSROOT_DIR");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        for lib in ["user32", "gdi32", "imm32", "dwmapi"] {
            println!("cargo:rustc-link-lib={lib}");
        }
        return;
    }

    let pkg_config = std::env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".to_string());
    let output = match Command::new(&pkg_config)
        .arg("--libs")
        .arg("--cflags")
        .args(PACKAGES)
        .output()
    {
        Ok(out) => out,
        Err(err) => {
            eprintln!(
                "error: failed to execute `{pkg_config}` for GTK4/PangoCairo ({}): {err}",
                PACKAGES.join(", ")
            );
            std::process::exit(1);
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "error: `{pkg_config} --libs --cflags {}` exited with {}:\n{}",
            PACKAGES.join(" "),
            output.status,
            stderr.trim()
        );
        std::process::exit(1);
    }

    let stdout = match String::from_utf8(output.stdout) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("error: `{pkg_config}` emitted non-UTF-8 flags: {err}");
            std::process::exit(1);
        }
    };

    let mut search_dirs = HashSet::new();
    let mut link_libs = HashSet::new();
    let mut tokens = stdout.split_whitespace();

    while let Some(token) = tokens.next() {
        if let Some(dir) = token.strip_prefix("-L") {
            let resolved = if dir.is_empty() {
                tokens.next().unwrap_or_default()
            } else {
                dir
            };
            if !resolved.is_empty() && search_dirs.insert(resolved.to_string()) {
                println!("cargo:rustc-link-search=native={resolved}");
            }
        } else if let Some(lib) = token.strip_prefix("-l") {
            let resolved = if lib.is_empty() {
                tokens.next().unwrap_or_default()
            } else {
                lib
            };
            if !resolved.is_empty() && link_libs.insert(resolved.to_string()) {
                println!("cargo:rustc-link-lib={resolved}");
            }
        }
    }

    if link_libs.is_empty() {
        eprintln!(
            "error: `{pkg_config}` returned no `-l` link libraries for {}",
            PACKAGES.join(", ")
        );
        std::process::exit(1);
    }
}
