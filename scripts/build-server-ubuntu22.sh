#!/usr/bin/env bash
# Build and package `lava-server` (Linux x86_64) targeting Ubuntu 22.04 LTS (glibc 2.35 baseline)
# with statically linked `libuv` and zero post-2.35 GLIBC symbol dependencies.
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd -- "$repo_root"

chmod +x scripts/zig-linux-gnu235-cc.sh scripts/zig-linux-gnu235-linker.sh

profile_flag=${LAVA_SERVER_PROFILE:---release}
profile_dir="release"
if [[ "$profile_flag" == "--dev" || "$profile_flag" == "dev" ]]; then
    profile_flag=""
    profile_dir="debug"
fi

# Isolate target directory so the static-libuv + glibc-2.35 C/link flags never collide with
# host Arch Linux (glibc 2.44) GTK4 dev artifacts.
ubuntu22_target_dir=${LAVA_UBUNTU22_TARGET_DIR:-"$repo_root/target/ubuntu22-x86_64"}

echo "[lava-server/ubuntu22] Building lava-server ($profile_dir) targeting x86_64-linux-gnu.2.35..."
CARGO_TARGET_DIR="$ubuntu22_target_dir" \
CC_x86_64_unknown_linux_gnu="$repo_root/scripts/zig-linux-gnu235-cc.sh" \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$repo_root/scripts/zig-linux-gnu235-linker.sh" \
LIBUV_NO_PKG_CONFIG=1 \
PKG_CONFIG=/bin/false \
bash scripts/cargo.sh build -p lava-server ${profile_flag:+$profile_flag}

bin_path="$ubuntu22_target_dir/$profile_dir/lava-server"
if [[ ! -f "$bin_path" ]]; then
    echo "error: expected binary not found at $bin_path" >&2
    exit 1
fi

echo "[lava-server/ubuntu22] Auditing ELF dynamic libraries and GLIBC symbol versions..."
python3 - "$bin_path" <<'PY'
import re, subprocess, sys

bin_path = sys.argv[1]
dyn = subprocess.check_output(["readelf", "-d", bin_path], text=True)
needed = re.findall(r"\(NEEDED\)\s+Shared library:\s+\[([^\]]+)\]", dyn)
allowed_libs = {
    "libc.so.6",
    "libm.so.6",
    "libpthread.so.0",
    "libdl.so.2",
    "librt.so.1",
    "libgcc_s.so.1",
    "ld-linux-x86-64.so.2",
}
bad_libs = [lib for lib in needed if lib not in allowed_libs]
if bad_libs:
    print(f"FAIL: lava-server links non-Ubuntu-22 base libraries: {bad_libs} (all={needed})", file=sys.stderr)
    sys.exit(1)

ver_out = subprocess.check_output(["readelf", "-V", bin_path], text=True)
bad_glibc = []
for m in re.finditer(r"Name:\s+GLIBC_([0-9]+)\.([0-9]+)(?:\.([0-9]+))?\s+Flags:\s+(\S+)", ver_out):
    maj, minor, patch, flags = int(m.group(1)), int(m.group(2)), int(m.group(3) or 0), m.group(4)
    if flags != "WEAK" and (maj, minor) > (2, 35):
        bad_glibc.append(f"GLIBC_{maj}.{minor}" + (f".{patch}" if patch else ""))

if bad_glibc:
    print(f"FAIL: lava-server requires post-2.35 GLIBC versions: {sorted(set(bad_glibc))}", file=sys.stderr)
    sys.exit(1)

print(f"OK: ELF NEEDED={needed}, max required GLIBC <= 2.35")
PY

dist_dir="$repo_root/dist/lava-server-linux-x86_64-ubuntu22"
rm -rf -- "$dist_dir"
mkdir -p -- "$dist_dir/server"

cp -f -- "$bin_path" "$dist_dir/lava-server"
chmod 755 "$dist_dir/lava-server"
cp -rf -- "$repo_root/server/packages" "$dist_dir/server/packages"
cp -rf -- "$repo_root/server/extensions" "$dist_dir/server/extensions"

# Smoke-check the packaged Ubuntu 22.04 binary directly against its bundled `server/` collection.
LAVA_SERVER_CACHE_DIR="/tmp/lava-server-ubuntu22-check-$$" \
    "$dist_dir/lava-server" check --server-root "$dist_dir/server"
rm -rf -- "/tmp/lava-server-ubuntu22-check-$$"

tar -C "$repo_root/dist" -czf "$repo_root/dist/lava-server-linux-x86_64-ubuntu22.tar.gz" "lava-server-linux-x86_64-ubuntu22"
echo "[lava-server/ubuntu22] Packaged: $repo_root/dist/lava-server-linux-x86_64-ubuntu22.tar.gz"
