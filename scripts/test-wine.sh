#!/usr/bin/env bash
# Build Windows x86_64 binaries (`lava.exe`, `rg.exe`, `fd.exe`), stage bundled Fira Code &
# Nerd Font TTFs, and execute unit + end-to-end compatibility suites under Wine configured to
# Windows 10 version 1709 (Build 16299).
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$repo_root"

# Stage bundled Fira Code & Nerd Font TTFs so Win32 GDI tests verify real private font loading
mkdir -p -- "$repo_root/target/lava-bundled-fonts"
sys_font_dir=${LAVA_SYSTEM_FONTS_DIR:-/usr/share/fonts/TTF}
for ttf in \
    FiraCode-Regular.ttf \
    FiraCode-Bold.ttf \
    FiraCode-Medium.ttf \
    FiraCode-Light.ttf \
    FiraCode-Retina.ttf \
    FiraCode-SemiBold.ttf \
    FiraCodeNerdFontMono-Regular.ttf \
    FiraCodeNerdFontMono-Bold.ttf \
    SymbolsNerdFontMono-Regular.ttf \
    SymbolsNerdFont-Regular.ttf
do
    if [[ -f "$sys_font_dir/$ttf" ]]; then
        cp -f -- "$sys_font_dir/$ttf" "$repo_root/target/lava-bundled-fonts/$ttf"
    fi
done

bash scripts/cargo-win.sh build -p lava -p lava-tools

bash scripts/cargo-win.sh test \
    -p lava-proto \
    -p lava-accel \
    -p lava-lsp \
    -p lava-remote \
    -p lava-term \
    -p lava-tools \
    -p lava-vm \
    --lib \
    -- --test-threads=2

bash scripts/cargo-win.sh test -p lava-gtk --lib -- --test-threads=1

bash scripts/cargo-win.sh test -p lava \
    --bins \
    --test windows_compat \
    -- --test-threads=1
