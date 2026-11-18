#!/usr/bin/env bash
# C compiler wrapper targeting Linux x86_64 with glibc 2.35 (Ubuntu 22.04 LTS baseline) via `zig cc`.
set -euo pipefail

cache_dir=${LAVA_ZIG_CACHE_DIR:-/tmp/lava-zig-cache}
mkdir -p -- "$cache_dir"
export ZIG_GLOBAL_CACHE_DIR="$cache_dir"
export ZIG_LOCAL_CACHE_DIR="$cache_dir"

filtered=()
skip_next=0
for arg in "$@"; do
    if (( skip_next )); then
        skip_next=0
        continue
    fi
    case "$arg" in
        --target=*)
            ;;
        -target|--target)
            skip_next=1
            ;;
        *)
            filtered+=("$arg")
            ;;
    esac
done

exec zig cc -target x86_64-linux-gnu.2.35 "${filtered[@]}"
