#!/usr/bin/env bash
# The crates.io packages the engine builds with patches of its own (engine/patches/crates.txt):
# engine/crates/<name>-<version>, the published package with engine/patches/<name>-<version>-*.patch
# applied, where Cargo.toml's [patch.crates-io] finds it. engine/build.sh and
# scripts/package-runtime.sh run this before cargo (a plain `cargo build` in engine/ needs it to
# have run once); scripts/package-runtime-windows.ps1 does the same in PowerShell. Redone when a
# patch changes; quiet otherwise.
#
#   wasmtime 37.0.3   a fault in a program with libraries loaded aborted the engine (upstream in
#                     51); the warnings it gave with our features and toolchain, fixed (a path
#                     package's warnings show: kept on, they show for what we patch too)
set -euo pipefail
E="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

sha256() { if command -v sha256sum >/dev/null; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi; }

mkdir -p "$E/crates"
while read -r name version sum; do
  [[ -z "$name" || "$name" == \#* ]] && continue
  dir="$E/crates/$name-$version"
  patches=()
  while IFS= read -r p; do patches+=("$p"); done < <(cd "$E/patches" && ls "$name-$version"-*.patch 2>/dev/null | LC_ALL=C sort)
  [[ ${#patches[@]} -gt 0 ]] || { echo "no engine/patches/$name-$version-*.patch" >&2; exit 1; }
  # each patch's SHA-256, in name order (the PowerShell twin writes the same)
  stamp="$(for p in "${patches[@]}"; do sha256 < "$E/patches/$p"; done | paste -sd' ' -)"
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
  # git apply, which Windows has too; the ceiling keeps it from taking a checkout around engine/
  # for the patch's repository
  for p in "${patches[@]}"; do
    GIT_CEILING_DIRECTORIES="$E/crates" git -C "$dir" apply --whitespace=nowarn "$E/patches/$p"
  done
  echo "$stamp" > "$dir/.patched"
  echo "== $name $version patched: ${patches[*]}"
done < "$E/patches/crates.txt"
