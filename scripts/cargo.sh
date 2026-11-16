#!/usr/bin/env bash
# Serialize Cargo builds and default to offline cached dependencies.
set -euo pipefail

workspace_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$workspace_dir"

target_dir=${CARGO_TARGET_DIR:-"$workspace_dir/target"}
if [[ "$target_dir" != /* ]]; then
    target_dir="$workspace_dir/$target_dir"
fi
export CARGO_TARGET_DIR="$target_dir"
export CARGO_NET_OFFLINE=${CARGO_NET_OFFLINE:-true}

cache_limit_kib=${LAVA_BUILD_CACHE_LIMIT_KIB:-6291456}
if [[ ! "$cache_limit_kib" =~ ^[0-9]{1,18}$ ]]; then
    echo 'LAVA_BUILD_CACHE_LIMIT_KIB must be a positive integer.' >&2
    exit 2
fi
cache_limit_kib=$((10#$cache_limit_kib))

mkdir -p -- "$(dirname -- "$target_dir")"
exec 9>"$target_dir.lava-cargo.lock"
flock -x 9

if [[ "${1:-}" != clean && -d "$target_dir" ]]; then
    cache_size_kib=$(du -sk -- "$target_dir")
    cache_size_kib=${cache_size_kib%%$'\t'*}
    if (( cache_size_kib >= cache_limit_kib )); then
        echo 'Build cache reached its limit; running cargo clean.' >&2
        cargo clean
    fi
fi

exec cargo "$@"
