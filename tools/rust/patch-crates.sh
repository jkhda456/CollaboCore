#!/usr/bin/env bash
# The crates.io crates we patch, ready for cargo: for each patches/<crate>-<version>-<what>.patch,
# the crate's registry source is copied to OUT/<crate>-<version> and its patches applied (in name
# order; again only when they change). Prints the --config arguments that point cargo at the
# copies, one per line.   usage: patch-crates.sh MANIFEST_DIR OUT
# MANIFEST_DIR is the workspace (a copy of it: the patch rewrites its Cargo.lock entries).
set -euo pipefail
W="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
M="$1" OUT="$2"
mkdir -p "$OUT"
declare -A seen=()
for p in "$W"/patches/*.patch; do
  [[ -e "$p" ]] || continue
  base="$(basename "$p")"
  [[ "$base" =~ ^(.+)-([0-9]+\.[0-9]+\.[0-9]+)-[a-z0-9_]+\.patch$ ]] || { echo "patch-crates: bad patch name $base" >&2; exit 1; }
  seen["${BASH_REMATCH[1]}-${BASH_REMATCH[2]}"]=1
done
for cv in "${!seen[@]}"; do
  crate="${cv%-*}" version="${cv##*-}"
  src="$(ls -d "$CARGO_HOME"/registry/src/*/"$cv" 2>/dev/null | head -1 || true)"
  if [[ -z "$src" ]]; then
    cargo +"${NIGHTLY:-stable}" fetch --manifest-path "$M/Cargo.toml" >/dev/null 2>&1 || true
    src="$(ls -d "$CARGO_HOME"/registry/src/*/"$cv" 2>/dev/null | head -1 || true)"
  fi
  [[ -n "$src" ]] || { echo "patch-crates: $cv is not in the cargo registry (cargo fetch failed?)" >&2; exit 1; }
  stamp="$(cat "$W"/patches/"$cv"-*.patch | sha256sum | cut -d' ' -f1)"
  if [[ "$(cat "$OUT/$cv/.collabo-patched" 2>/dev/null)" != "$stamp" ]]; then
    rm -rf "$OUT/$cv"
    cp -r "$src" "$OUT/$cv"
    for p in "$W"/patches/"$cv"-*.patch; do (cd "$OUT/$cv" && patch -p1 --quiet < "$p"); done
    echo "$stamp" > "$OUT/$cv/.collabo-patched"
  fi
  echo "--config"
  echo "patch.crates-io.$crate.path=\"$OUT/$cv\""
done
