//! Windows 10 1709 (RS3, Build 16299) PE64 static import audit and end-to-end Wine runtime verification
//! for `lava.exe`, bundled `rg.exe` & `fd.exe`, bundled Fira Code + Nerd Font TTFs, and masked extensions.

#![cfg(windows)]

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

fn read_u16_le(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([bytes[off], bytes[off + 1]])
}

fn read_u32_le(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

fn read_u64_le(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        bytes[off],
        bytes[off + 1],
        bytes[off + 2],
        bytes[off + 3],
        bytes[off + 4],
        bytes[off + 5],
        bytes[off + 6],
        bytes[off + 7],
    ])
}

struct PeAudit {
    major_os: u16,
    minor_os: u16,
    major_subsys: u16,
    minor_subsys: u16,
    imports: BTreeMap<String, BTreeSet<String>>,
}

fn rva_to_offset(sections: &[(u32, u32, u32, u32)], rva: u32) -> Option<usize> {
    for &(virt_addr, virt_size, raw_ptr, raw_size) in sections {
        let span = virt_size.max(raw_size);
        if rva >= virt_addr && rva < virt_addr.saturating_add(span) {
            return Some((raw_ptr + (rva - virt_addr)) as usize);
        }
    }
    None
}

fn read_cstr(bytes: &[u8], mut off: usize) -> String {
    let start = off;
    while off < bytes.len() && bytes[off] != 0 {
        off += 1;
    }
    String::from_utf8_lossy(&bytes[start..off]).into_owned()
}

fn audit_pe64_binary(bytes: &[u8]) -> PeAudit {
    assert!(bytes.len() > 0x100, "PE binary too small");
    assert_eq!(&bytes[0..2], b"MZ", "missing DOS MZ magic");
    let pe_off = read_u32_le(bytes, 0x3C) as usize;
    assert_eq!(
        &bytes[pe_off..pe_off + 4],
        b"PE\0\0",
        "missing PE signature"
    );

    let coff_off = pe_off + 4;
    assert_eq!(read_u16_le(bytes, coff_off), 0x8664, "expected AMD64 PE");
    let num_sections = read_u16_le(bytes, coff_off + 2) as usize;
    let opt_hdr_size = read_u16_le(bytes, coff_off + 16) as usize;

    let opt_off = coff_off + 20;
    assert_eq!(read_u16_le(bytes, opt_off), 0x20B, "expected PE32+");

    let major_os = read_u16_le(bytes, opt_off + 40);
    let minor_os = read_u16_le(bytes, opt_off + 42);
    let major_subsys = read_u16_le(bytes, opt_off + 48);
    let minor_subsys = read_u16_le(bytes, opt_off + 50);
    let num_rva_and_sizes = read_u32_le(bytes, opt_off + 108) as usize;

    let sec_table_off = opt_off + opt_hdr_size;
    let mut sections = Vec::with_capacity(num_sections);
    for i in 0..num_sections {
        let s = sec_table_off + i * 40;
        sections.push((
            read_u32_le(bytes, s + 12),
            read_u32_le(bytes, s + 8),
            read_u32_le(bytes, s + 20),
            read_u32_le(bytes, s + 16),
        ));
    }

    let mut imports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if num_rva_and_sizes >= 2 {
        let import_dir_rva = read_u32_le(bytes, opt_off + 112 + 8);
        let import_dir_size = read_u32_le(bytes, opt_off + 112 + 12);
        if import_dir_rva != 0
            && import_dir_size >= 20
            && let Some(mut desc_off) = rva_to_offset(&sections, import_dir_rva)
        {
            while desc_off + 20 <= bytes.len() {
                let ilt_rva = read_u32_le(bytes, desc_off);
                let name_rva = read_u32_le(bytes, desc_off + 12);
                let iat_rva = read_u32_le(bytes, desc_off + 16);
                if ilt_rva == 0 && name_rva == 0 && iat_rva == 0 {
                    break;
                }
                let dll_name = rva_to_offset(&sections, name_rva)
                    .map(|o| read_cstr(bytes, o).to_ascii_lowercase())
                    .unwrap_or_default();
                let syms = imports.entry(dll_name).or_default();
                let table_rva = if ilt_rva != 0 { ilt_rva } else { iat_rva };
                if let Some(mut thunk_off) = rva_to_offset(&sections, table_rva) {
                    while thunk_off + 8 <= bytes.len() {
                        let entry = read_u64_le(bytes, thunk_off);
                        if entry == 0 {
                            break;
                        }
                        if (entry & (1u64 << 63)) == 0 {
                            let hint_name_rva = (entry & 0x7FFF_FFFF) as u32;
                            if let Some(hn_off) = rva_to_offset(&sections, hint_name_rva) {
                                syms.insert(read_cstr(bytes, hn_off + 2));
                            }
                        } else {
                            syms.insert(format!("ordinal#{}", entry & 0xFFFF));
                        }
                        thunk_off += 8;
                    }
                }
                desc_off += 20;
            }
        }
    }

    PeAudit {
        major_os,
        minor_os,
        major_subsys,
        minor_subsys,
        imports,
    }
}

fn assert_win10_1709_pe_compliance(label: &str, path: &Path) {
    let bytes =
        fs::read(path).unwrap_or_else(|e| panic!("read {label} at {}: {e}", path.display()));
    let audit = audit_pe64_binary(&bytes);

    assert!(
        (audit.major_os, audit.minor_os) <= (10, 0),
        "{label} PE OS version {}.{} exceeds Windows 10 1709 (10.0)",
        audit.major_os,
        audit.minor_os
    );
    assert!(
        (audit.major_subsys, audit.minor_subsys) <= (10, 0),
        "{label} PE Subsystem version {}.{} exceeds Windows 10 1709 (10.0)",
        audit.major_subsys,
        audit.minor_subsys
    );

    let forbidden_dlls = [
        "api-ms-win-core-memory-l1-1-6.dll", // Win10 1803+
        "api-ms-win-core-memory-l1-1-7.dll", // Win10 1903+
        "api-ms-win-core-file-l1-2-4.dll",
        "api-ms-win-core-console-l1-2-1.dll", // ConPTY 1809+
        "libgcc_s_seh-1.dll",
        "libwinpthread-1.dll",
        "libstdc++-6.dll",
        "vcruntime140.dll",
        "libgtk-4-1.dll",
        "libtree-sitter.dll",
    ];
    for dll in audit.imports.keys() {
        assert!(
            !forbidden_dlls.contains(&dll.as_str()),
            "{label} imports forbidden or non-self-contained DLL `{dll}`; all={:?}",
            audit.imports.keys().collect::<Vec<_>>()
        );
    }

    let forbidden_symbols = [
        "VirtualAlloc2",
        "VirtualAlloc2FromApp",
        "MapViewOfFile3",
        "MapViewOfFile3FromApp",
        "CreateFileMapping2",
        "GetTempPath2W",
        "GetTempPath2A",
        "CreateFile3",
        "CreatePseudoConsole",
        "ResizePseudoConsole",
        "ClosePseudoConsole",
    ];
    for (dll, syms) in &audit.imports {
        for bad in forbidden_symbols {
            assert!(
                !syms.contains(bad),
                "{label} imports post-1709 API `{bad}` from `{dll}`"
            );
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn lava_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lava"))
}

fn sibling_tool_exe(name: &str) -> PathBuf {
    let lava = lava_exe();
    if let Some(bin_dir) = lava.parent() {
        // Cargo puts workspace binaries in `target/x86_64-pc-windows-gnullvm/debug/`
        let direct = bin_dir.join(name);
        if direct.is_file() {
            return direct;
        }
        if let Some(parent) = bin_dir.parent().and_then(Path::parent) {
            let up = parent.join(name);
            if up.is_file() {
                return up;
            }
        }
    }
    workspace_root()
        .join("target")
        .join("x86_64-pc-windows-gnullvm")
        .join("debug")
        .join(name)
}

#[test]
fn pe64_headers_and_imports_strictly_comply_with_windows_10_1709() {
    let lava = lava_exe();
    assert_win10_1709_pe_compliance("lava.exe", &lava);

    let rg = sibling_tool_exe("rg.exe");
    if rg.is_file() {
        assert_win10_1709_pe_compliance("rg.exe", &rg);
    }
    let fd = sibling_tool_exe("fd.exe");
    if fd.is_file() {
        assert_win10_1709_pe_compliance("fd.exe", &fd);
    }
}

#[test]
fn windows_10_1709_wine_e2e_extensions_check_headless_edit_png_and_bundled_rg_fd() {
    #[repr(C)]
    struct OsVersionInfoW {
        os_version_info_size: u32,
        major_version: u32,
        minor_version: u32,
        build_number: u32,
        platform_id: u32,
        csd_version: [u16; 128],
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(lp_version_information: *mut OsVersionInfoW) -> i32;
    }

    let mut ver = OsVersionInfoW {
        os_version_info_size: std::mem::size_of::<OsVersionInfoW>() as u32,
        major_version: 0,
        minor_version: 0,
        build_number: 0,
        platform_id: 0,
        csd_version: [0u16; 128],
    };
    // SAFETY: `RtlGetVersion` (NT 5.0+) populates `OsVersionInfoW`.
    let status = unsafe { RtlGetVersion(&raw mut ver) };
    assert_eq!(status, 0);
    assert!(
        ver.major_version > 10 || (ver.major_version == 10 && ver.build_number >= 16299),
        "expected Windows 10 1709+ (10.0.16299+), got {}.{}.{}",
        ver.major_version,
        ver.minor_version,
        ver.build_number
    );

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let stamp = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temp_dir = env::temp_dir().join(format!(
        "lava win1709 e2e {} {}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed) ^ (stamp as u64)
    ));
    fs::create_dir_all(temp_dir.join("src")).expect("create temp project dir with spaces");
    fs::write(temp_dir.join(".gitignore"), "ignored.log\n").unwrap();
    fs::write(temp_dir.join("ignored.log"), "SHOULD_BE_IGNORED\n").unwrap();
    let edit_file = temp_dir.join("src").join("hello_win.ns");
    fs::write(&edit_file, "-- initial line\n").unwrap();

    // 1. Verify bundled `rg.exe` and `fd.exe` on Windows paths + `.gitignore` + stdin filter
    let rg_bin = sibling_tool_exe("rg.exe");
    let fd_bin = sibling_tool_exe("fd.exe");
    assert!(
        rg_bin.is_file(),
        "expected bundled rg.exe at {}",
        rg_bin.display()
    );
    assert!(
        fd_bin.is_file(),
        "expected bundled fd.exe at {}",
        fd_bin.display()
    );

    let rg_out = Command::new(&rg_bin)
        .args([
            "-n",
            "--column",
            "--no-heading",
            "--color=never",
            "initial",
            ".",
        ])
        .current_dir(&temp_dir)
        .output()
        .expect("run bundled rg.exe");
    assert!(rg_out.status.success());
    let rg_stdout = String::from_utf8_lossy(&rg_out.stdout);
    assert!(
        rg_stdout.contains("hello_win.ns:1:4:-- initial line"),
        "got: {rg_stdout}"
    );
    assert!(!rg_stdout.contains("ignored.log"));

    let mut rg_stdin_child = Command::new(&rg_bin)
        .args(["-v", r"^\s*#"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn rg.exe stdin filter");
    rg_stdin_child
        .stdin
        .take()
        .unwrap()
        .write_all(b"# comment\nkeep_line\n")
        .unwrap();
    let rg_stdin_out = rg_stdin_child.wait_with_output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&rg_stdin_out.stdout).trim(),
        "keep_line"
    );

    let fd_out = Command::new(&fd_bin)
        .args(["-e", "ns"])
        .current_dir(&temp_dir)
        .output()
        .expect("run bundled fd.exe");
    assert!(fd_out.status.success());
    let fd_stdout = String::from_utf8_lossy(&fd_out.stdout);
    assert!(fd_stdout.contains("hello_win.ns"), "got: {fd_stdout}");

    // 2. Verify `lava.exe list-extensions` masks `ext_toggleterm` and `ext_treesitter`
    let ws = workspace_root();
    let cache_dir = temp_dir.join("nsbc-cache");
    let list_out = Command::new(lava_exe())
        .arg("list-extensions")
        .arg("--workspace")
        .arg(&ws)
        .env("LAVA_NSBC_CACHE_DIR", &cache_dir)
        .output()
        .expect("run lava.exe list-extensions");
    assert!(
        list_out.status.success(),
        "list-extensions failed: {}",
        String::from_utf8_lossy(&list_out.stderr)
    );
    let list_txt = String::from_utf8_lossy(&list_out.stdout);
    assert!(list_txt.contains("lava.org/ext_dirvish"), "got: {list_txt}");
    assert!(list_txt.contains("lava.org/ext_remote"), "got: {list_txt}");
    assert!(
        !list_txt.contains("lava.org/ext_toggleterm"),
        "ext_toggleterm must be masked on Windows 10 1709"
    );
    assert!(
        !list_txt.contains("lava.org/ext_treesitter"),
        "ext_treesitter must be masked on Windows"
    );

    // 3. Verify `lava.exe check` compiles and caches all 45 Windows-supported extensions
    let check_out = Command::new(lava_exe())
        .arg("check")
        .arg("--no-init")
        .arg("--workspace")
        .arg(&ws)
        .env("LAVA_NSBC_CACHE_DIR", &cache_dir)
        .output()
        .expect("run lava.exe check");
    assert!(
        check_out.status.success(),
        "lava.exe check failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&check_out.stdout),
        String::from_utf8_lossy(&check_out.stderr)
    );
    assert!(String::from_utf8_lossy(&check_out.stdout).contains("verified 45 extension(s)"));

    // 4. Headless edit + save on Windows path with spaces + Win32 GDI + Nerd Font PNG export!
    let png_path = temp_dir.join("win1709_frame.png");
    let win_state = temp_dir.join("win_state.txt");
    let run_out = Command::new(lava_exe())
        .arg("--no-init")
        .arg("--workspace")
        .arg(&ws)
        .arg("--headless")
        .arg("--headless-keys")
        .arg("olet<Space>win_ok<Space>=<Space>1709<Escape>:wq<Return>")
        .arg("--dump-frame-png")
        .arg(&png_path)
        .arg(&edit_file)
        .env("LAVA_NSBC_CACHE_DIR", &cache_dir)
        .env("LAVA_WINDOW_STATE_FILE", &win_state)
        .output()
        .expect("run headless lava.exe edit + PNG");
    assert!(
        run_out.status.success(),
        "headless lava.exe failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&run_out.stdout),
        String::from_utf8_lossy(&run_out.stderr)
    );

    let saved = fs::read_to_string(&edit_file).expect("read saved hello_win.ns");
    assert!(saved.contains("let win_ok = 1709"), "saved={saved}");

    let png_bytes = fs::read(&png_path).expect("read exported PNG");
    assert_eq!(&png_bytes[0..8], b"\x89PNG\r\n\x1a\n");
    assert!(png_bytes.len() > 4096);

    let _ = fs::remove_dir_all(&temp_dir);
}
