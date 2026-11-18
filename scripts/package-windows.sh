#!/usr/bin/env bash
# Package Lava for Windows 10 1709+ (`x86_64-pc-windows-gnullvm`), bundling:
#   - `lava.exe` (Native Win32 + GDI DIB + smooth scroll & cursor physics)
#   - `bin/rg.exe` & `bin/fd.exe` (Win10 1709 self-contained search & file finder tools)
#   - `fonts/*.ttf` (Fira Code + FiraCode Nerd Font Mono + Symbols Nerd Font Mono)
#   - `packages/*` & `extensions/*` (excluding `ext_toggleterm` and `ext_treesitter`)
#   - `server/*` & optional Ubuntu 22.04 `share/lava-server/lava-server` for `:RemoteInstallServer`
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd -- "$repo_root"

profile_flag=${LAVA_WIN_PROFILE:---release}
profile_dir="release"
if [[ "$profile_flag" == "--dev" || "$profile_flag" == "dev" ]]; then
    profile_flag=""
    profile_dir="debug"
fi

echo "[lava/windows] Building lava.exe, rg.exe, and fd.exe ($profile_dir) for x86_64-pc-windows-gnullvm..."
bash scripts/cargo-win.sh build -p lava -p lava-tools ${profile_flag:+$profile_flag}

win_bin_dir="$repo_root/target/x86_64-pc-windows-gnullvm/$profile_dir"
for exe in lava.exe rg.exe fd.exe; do
    if [[ ! -f "$win_bin_dir/$exe" ]]; then
        echo "error: expected Windows binary not found: $win_bin_dir/$exe" >&2
        exit 1
    fi
done

dist_dir="$repo_root/dist/lava-windows-x86_64"
rm -rf -- "$dist_dir"
mkdir -p -- "$dist_dir/bin" "$dist_dir/fonts" "$dist_dir/packages" "$dist_dir/extensions" "$dist_dir/examples"

cp -f -- "$win_bin_dir/lava.exe" "$dist_dir/lava.exe"
cp -f -- "$win_bin_dir/rg.exe" "$dist_dir/bin/rg.exe"
cp -f -- "$win_bin_dir/fd.exe" "$dist_dir/bin/fd.exe"

# Bundle Fira Code & Nerd Font TTFs
sys_font_dir=${LAVA_SYSTEM_FONTS_DIR:-/usr/share/fonts/TTF}
font_files=(
    "FiraCode-Regular.ttf"
    "FiraCode-Bold.ttf"
    "FiraCode-Medium.ttf"
    "FiraCode-Light.ttf"
    "FiraCode-Retina.ttf"
    "FiraCode-SemiBold.ttf"
    "FiraCodeNerdFontMono-Regular.ttf"
    "FiraCodeNerdFontMono-Bold.ttf"
    "SymbolsNerdFontMono-Regular.ttf"
    "SymbolsNerdFont-Regular.ttf"
)
copied_fonts=0
for ttf in "${font_files[@]}"; do
    if [[ -f "$sys_font_dir/$ttf" ]]; then
        cp -f -- "$sys_font_dir/$ttf" "$dist_dir/fonts/$ttf"
        copied_fonts=$((copied_fonts + 1))
    elif [[ -f "$repo_root/target/lava-bundled-fonts/$ttf" ]]; then
        cp -f -- "$repo_root/target/lava-bundled-fonts/$ttf" "$dist_dir/fonts/$ttf"
        copied_fonts=$((copied_fonts + 1))
    fi
done
if (( copied_fonts < 4 )); then
    echo "error: could not locate Fira Code / Nerd Font TTFs in $sys_font_dir" >&2
    exit 1
fi

# Copy core Nessa packages & Windows-supported extensions (excluding terminal & treesitter)
cp -rf -- "$repo_root/packages/." "$dist_dir/packages/"
for ext_path in "$repo_root"/extensions/*; do
    [[ -d "$ext_path" ]] || continue
    ext_name=$(basename -- "$ext_path")
    case "$ext_name" in
        ext_toggleterm|ext_treesitter)
            continue
            ;;
    esac
    cp -rf -- "$ext_path" "$dist_dir/extensions/$ext_name"
done

if [[ -f "$repo_root/examples/init.ns" ]]; then
    cp -f -- "$repo_root/examples/init.ns" "$dist_dir/examples/init.ns"
fi

# Include server Nessa sources + Ubuntu 22.04 Linux `lava-server` ELF if built so `:RemoteInstallServer`
# works directly from a Windows client against Ubuntu 22+ Linux servers!
if [[ -d "$repo_root/server" ]]; then
    mkdir -p -- "$dist_dir/server"
    cp -rf -- "$repo_root/server/packages" "$dist_dir/server/packages"
    cp -rf -- "$repo_root/server/extensions" "$dist_dir/server/extensions"
fi
if [[ -f "$repo_root/dist/lava-server-linux-x86_64-ubuntu22/lava-server" ]]; then
    mkdir -p -- "$dist_dir/share/lava-server"
    cp -f -- "$repo_root/dist/lava-server-linux-x86_64-ubuntu22/lava-server" "$dist_dir/share/lava-server/lava-server"
fi

# Create portable `.zip` archive via Python `zipfile` (no external `zip` binary needed)
python3 - "$repo_root/dist" "lava-windows-x86_64" "$repo_root/dist/lava-windows-x86_64.zip" <<'PY'
import os, sys, zipfile

base_dir, folder_name, out_zip = sys.argv[1], sys.argv[2], sys.argv[3]
root_path = os.path.join(base_dir, folder_name)
if os.path.exists(out_zip):
    os.remove(out_zip)
with zipfile.ZipFile(out_zip, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=6) as zf:
    for dirpath, dirnames, filenames in os.walk(root_path):
        dirnames.sort()
        filenames.sort()
        for fname in filenames:
            full = os.path.join(dirpath, fname)
            rel = os.path.relpath(full, base_dir).replace(os.sep, "/")
            zf.write(full, rel)
print(f"[lava/windows] Packaged: {out_zip} ({os.path.getsize(out_zip) // 1024} KiB)")
PY
