#!/usr/bin/env bash
# Build and package Lava for Arch Linux (`pacman -U dist/lava-*.pkg.tar.zst`).
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd -- "$repo_root"

if ! command -v makepkg >/dev/null 2>&1; then
    echo "error: makepkg not found (Arch Linux base-devel required)" >&2
    exit 1
fi

profile_flag=${LAVA_PACMAN_PROFILE:---release}
profile_dir="release"
if [[ "$profile_flag" == "--dev" || "$profile_flag" == "dev" ]]; then
    profile_flag=""
    profile_dir="debug"
fi

echo "[lava/pacman] Building lava & lava-server ($profile_dir)..."
bash scripts/cargo.sh build -p lava -p lava-server ${profile_flag:+$profile_flag}

bin_dir="$repo_root/target/$profile_dir"
for bin in lava lava-server; do
    if [[ ! -x "$bin_dir/$bin" ]]; then
        echo "error: expected binary not found: $bin_dir/$bin" >&2
        exit 1
    fi
done

stage_dir="$repo_root/dist/arch-build"
rm -rf -- "$stage_dir"
mkdir -p -- "$stage_dir"
cp -f -- "$repo_root/packaging/arch/PKGBUILD" "$stage_dir/PKGBUILD"
cp -f -- "$repo_root/packaging/arch/lava.desktop" "$stage_dir/lava.desktop"

echo "[lava/pacman] Running makepkg..."
(
    cd -- "$stage_dir"
    LAVA_REPO_ROOT="$repo_root" \
    LAVA_PREBUILT_DIR="$bin_dir" \
    PKGDEST="$repo_root/dist" \
    BUILDDIR="$stage_dir/src-build" \
    makepkg -f --noconfirm --nodeps
)

pkg_file=$(ls -t "$repo_root"/dist/lava-*.pkg.tar.* 2>/dev/null | head -n 1 || true)
if [[ -z "$pkg_file" ]]; then
    echo "error: makepkg did not produce a package in $repo_root/dist" >&2
    exit 1
fi

echo "[lava/pacman] Packaged: $pkg_file"
echo "[lava/pacman] Install with: sudo pacman -U $pkg_file"
