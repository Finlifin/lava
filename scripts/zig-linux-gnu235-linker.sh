#!/usr/bin/env bash
# Rust linker wrapper targeting Linux x86_64 with glibc 2.35 (Ubuntu 22.04 LTS baseline) via `zig cc`.
set -euo pipefail

cache_dir=${LAVA_ZIG_CACHE_DIR:-/tmp/lava-zig-cache}
mkdir -p -- "$cache_dir"
export ZIG_GLOBAL_CACHE_DIR="$cache_dir"
export ZIG_LOCAL_CACHE_DIR="$cache_dir"

filtered=()
for arg in "$@"; do
    case "$arg" in
        -nodefaultlibs|-Wl,-Bstatic|-Wl,-Bdynamic|-lgcc_s|-lutil|-fuse-ld=lld|-B*)
            ;;
        *Scrt1.o|*crti.o|*crtn.o|*crtbeginS.o|*crtendS.o)
            ;;
        *)
            filtered+=("$arg")
            ;;
    esac
done

exec zig cc -target x86_64-linux-gnu.2.35 -lunwind "${filtered[@]}"
