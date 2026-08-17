#!/usr/bin/env bash
# vendor-restore.sh — reconstruct the gitignored src-tauri/vendor/ crates from
# the pristine cargo registry copies + the tracked patches in
# src-tauri/vendor-patches/. The vendor/ trees are GB-scale (bundled C++
# sources), so .gitignore keeps them out of git; these patches ARE the
# source of truth for every WUPI modification. Run from the repo root (or
# anywhere — paths resolve from the script location).
#
# Usage (Git Bash):
#   bash scripts/vendor-restore.sh                # restore both crates
#   bash scripts/vendor-restore.sh llama-cpp-sys-2
#   bash scripts/vendor-restore.sh diffusion-rs-sys --force   # overwrite existing
#
# After a crate version BUMP in Cargo.toml: regenerate by copying the new
# registry crate into vendor/, re-applying each change by hand (the patches
# will likely need refresh — diff again and update the .patch files, and
# bump the version pins below).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR="$ROOT/src-tauri/vendor"
PATCHES="$ROOT/src-tauri/vendor-patches"
REGISTRY="${CARGO_HOME:-$HOME/.cargo}/registry/src"
REGHASH=$(ls "$REGISTRY" | grep -m1 '^index\.crates\.io-' || true)
if [ -z "$REGHASH" ]; then
  echo "vendor-restore: no crates.io registry src dir under $REGISTRY — run a cargo build once so the cache exists." >&2
  exit 1
fi
REGISTRY="$REGISTRY/$REGHASH"

restore_crate () {
  local name="$1" version="$2"; shift 2
  local src="$REGISTRY/$name-$version"
  local dst="$VENDOR/$name"
  local force=0
  for arg in "$@"; do [ "$arg" = "--force" ] && force=1; done

  if [ ! -d "$src" ]; then
    echo "vendor-restore: pristine $name-$version not found at $src" >&2
    echo "  (fetch it once: cargo fetch --manifest-path src-tauri/Cargo.toml)" >&2
    exit 1
  fi
  if [ -d "$dst" ] && [ "$force" -ne 1 ]; then
    echo "vendor-restore: $dst already exists (use --force to overwrite) — skipping $name" >&2
    return 0
  fi

  echo "vendor-restore: copying $name-$version -> $dst"
  rm -rf "$dst"
  cp -r "$src" "$dst"

  local pdir="$PATCHES/$name"
  for patch in "$pdir"/*.patch; do
    [ -e "$patch" ] || { echo "vendor-restore: no patches in $pdir?!" >&2; exit 1; }
    echo "vendor-restore: applying $(basename "$patch")"
    (cd "$dst" && git apply "$patch")
  done
  echo "vendor-restore: $name restored (pristine $version + $(ls "$pdir"/*.patch | wc -l) patch[es])"
}

WHAT="${1:-all}"
case "$WHAT" in
  diffusion-rs-sys) restore_crate diffusion-rs-sys 0.1.20 "${@:2}" ;;
  llama-cpp-sys-2)  restore_crate llama-cpp-sys-2  0.1.151 "${@:2}" ;;
  all)
    restore_crate diffusion-rs-sys 0.1.20 "${@:2}"
    restore_crate llama-cpp-sys-2  0.1.151 "${@:2}"
    ;;
  *) echo "usage: $0 [diffusion-rs-sys|llama-cpp-sys-2|all] [--force]" >&2; exit 2 ;;
esac
