#!/usr/bin/env bash
# The crates.io packages the engine builds with patches of its own: engine/crates/<name>-<version>,
# the published package with engine/patches/<name>-<version>-*.patch applied, where Cargo.toml's
# [patch.crates-io] finds it. engine/build.sh and scripts/package-runtime.sh run this before cargo
# (a plain `cargo build` in engine/ needs it to have run once). Redone when a patch changes.
#
#   wasmtime 37.0.3   a trap in a program with libraries loaded aborted the engine (upstream in 51)
set -euo pipefail
E="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# name, version and the published package's SHA-256 (its checksum in Cargo.lock before the patch)
CRATES=("wasmtime 37.0.3 efcab4481a639a8f3413aa011f733db105ecccc1326a51a6f5c7d09c99314f85")

sha256() { if command -v sha256sum >/dev/null; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi; }

mkdir -p "$E/crates"
for entry in "${CRATES[@]}"; do
  read -r name version sum <<<"$entry"
  dir="$E/crates/$name-$version"
  patches=("$E/patches/$name-$version"-*.patch)
  [[ -f "${patches[0]}" ]] || { echo "no engine/patches/$name-$version-*.patch" >&2; exit 1; }
  stamp="$(cat "${patches[@]}" | sha256)"
  if [[ -f "$dir/.patched" && "$(cat "$dir/.patched")" == "$stamp" ]]; then continue; fi
  # the package: cargo's download cache, else crates.io; either way the published one
  crate=""
  for cached in "${CARGO_HOME:-$HOME/.cargo}"/registry/cache/*/"$name-$version.crate"; do
    [[ -f "$cached" ]] && crate="$cached"
  done
  if [[ -z "$crate" ]]; then
    crate="$E/crates/$name-$version.crate"
    curl -sSfL --retry 3 -o "$crate" "https://static.crates.io/crates/$name/$name-$version.crate"
  fi
  [[ "$(sha256 < "$crate")" == "$sum" ]] || { echo "$crate: not the published $name $version (SHA-256)" >&2; exit 1; }
  rm -rf "$dir"
  tar -xzf "$crate" -C "$E/crates"
  for p in "${patches[@]}"; do patch -p1 --quiet --forward -d "$dir" < "$p"; done
  echo "$stamp" > "$dir/.patched"
  echo "== $name $version patched: $(cd "$E/patches" && ls "$name-$version"-*.patch | tr '\n' ' ')"
done
