#!/usr/bin/env bash
# The guest's tools written in Rust (this workspace, Cargo.toml), part of tools.cpio
# (tools/build.sh runs this as its `rust` step; it also runs alone). Into tools/stage:
#
#   usr/bin/collabo-archive   xz, zstd and 7z in one binary; the image links their names to it
#   usr/bin/jq                the JSON processor
#   usr/bin/git-lfs           Git Large File Storage
#
# and build/rust/licenses/ (the crates linked in). Cargo is incremental, so a run with nothing
# changed takes seconds.
#
# Needs the userspace step (musl, compiler-rt: userspace/sysroot) and the guest's Rust toolchain
# (python/build-rust-toolchain.sh, which this runs; it skips when it is up to date). Nothing here
# compiles C; git-lfs links libcurl from tools/build/curl-root (the curl step) with OpenSSL and
# zlib from python/deps (git-lfs/build.rs reads COLLABO_CURL_LIBDIRS).
#   TEST=1 tools/rust/build.sh    also run the unit tests natively first
#   HOST=1 tools/rust/build.sh    build for this machine instead, into build/rust/host (for the
#                                 differential tests in tests/, against the original tools)
set -euo pipefail

W="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
N="$(dirname "$W")"
ROOT="$(dirname "$N")"
U="$ROOT/userspace"
R="$ROOT/.tools/rust-wasm"
TARGET=wasm32-unknown-linux-musl
NIGHTLY=nightly-2026-07-22   # as python/build-rust-toolchain.sh
B="$N/build/rust"
STAGE="${STAGE:-$N/stage}"
BINS=(collabo-archive jq git-lfs)

[[ -f "$U/sysroot/lib/crt1.o" ]] || { echo "missing userspace/sysroot; run: ./build.sh userspace" >&2; exit 1; }
bash "$ROOT/python/build-rust-toolchain.sh" >/dev/null

export RUSTUP_HOME="$ROOT/.tools/rustup" CARGO_HOME="$ROOT/.tools/cargo"
export PATH="$CARGO_HOME/bin:$PATH"
# Build scripts are host programs, and this machine's C compiler is clang-19 (no `cc`).
HOST_TRIPLE="$(rustc +"$NIGHTLY" -vV | sed -n 's/^host: //p')"
HOST_ENV="$(echo "$HOST_TRIPLE" | tr a-z- A-Z_)"
export "CARGO_TARGET_${HOST_ENV}_LINKER=clang-19" "CC_${HOST_TRIPLE//-/_}=clang-19"
mkdir -p "$B" "$STAGE/usr/bin"

# Built from a copy: the libc patch below replaces a crates.io package in the lock file, which
# must not rewrite the checked-in one; so do our patches to crates (patches/, patch-crates.sh).
# rsync -c keeps unchanged files' times, so cargo rebuilds only after a change.
mkdir -p "$B/src"
rsync -a -c --delete --exclude target --exclude tests --exclude patches --exclude '*.sh' "$W/" "$B/src/"
mapfile -t PATCHED < <(NIGHTLY="$NIGHTLY" bash "$W/patch-crates.sh" "$B/src" "$B/patched")

if [[ -n "${TEST:-}" ]]; then
  echo "== rust tools: unit tests ($HOST_TRIPLE)"
  cargo +"$NIGHTLY" test --release --manifest-path "$B/src/Cargo.toml" --target-dir "$B/host" "${PATCHED[@]}"
fi

if [[ -n "${HOST:-}" ]]; then
  echo "== rust tools ($HOST_TRIPLE, into build/rust/host/release)"
  cargo +"$NIGHTLY" build --release --manifest-path "$B/src/Cargo.toml" --target-dir "$B/host" "${PATCHED[@]}"
  exit 0
fi

echo "== rust tools (-> $TARGET)"
[[ -f "$N/build/curl-root/usr/lib/libcurl.a" ]] || { echo "missing tools/build/curl-root (libcurl for git-lfs); run: tools/build.sh curl" >&2; exit 1; }
export COLLABO_CURL_LIBDIRS="$N/build/curl-root/usr/lib:$ROOT/python/deps/lib"
# rustc hands the linker --target=wasm32-unknown-linux-musl (the spec's name), which clang-19
# behind wasm-cc does not know; wasm-cc sets its own --target=wasm32.
cat > "$B/wasm-cc-rust" <<SH
#!/bin/sh
for a; do shift; case "\$a" in --target=*) ;; *) set -- "\$@" "\$a" ;; esac; done
exec "$U/bin/wasm-cc" "\$@"
SH
chmod +x "$B/wasm-cc-rust"
LIBC_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$R/libc/Cargo.toml" | head -1)"
cargo +"$NIGHTLY" update --manifest-path "$B/src/Cargo.toml" --config "patch.crates-io.libc.path=\"$R/libc\"" \
  -p libc --precise "$LIBC_VERSION" > "$B/rust.log" 2>&1 || true
# The link goes through wasm-cc with the toolchain's own wasm-ld; std is the one
# build-rust-toolchain.sh built; libc is tombl's fork (this platform's ILP32 musl ABI).
WASM_LD="$R/bin/wasm-ld" RUSTC="$R/bin/rustc" \
RUSTFLAGS="-Clinker=$B/wasm-cc-rust --remap-path-prefix=$B/src=/usr/src/collabo --remap-path-prefix=$CARGO_HOME=/usr/src/cargo" \
  cargo +"$NIGHTLY" build --release --target "$TARGET" \
    --manifest-path "$B/src/Cargo.toml" --target-dir "$B/wasm" \
    --config "patch.crates-io.libc.path=\"$R/libc\"" "${PATCHED[@]}" \
    >> "$B/rust.log" 2>&1 || { tail -40 "$B/rust.log"; echo "FAILED: rust tools (log: $B/rust.log)" >&2; exit 1; }
for bin in "${BINS[@]}"; do
  f="$B/wasm/$TARGET/release/$bin.wasm"
  [[ -f "$f" ]] || f="$B/wasm/$TARGET/release/$bin"
  [[ -f "$f" ]] || { echo "cargo wrote no $bin binary" >&2; exit 1; }
  install -m755 "$f" "$STAGE/usr/bin/$bin"
done

# The crates linked in, with their licenses (scripts/build-engine.sh copies the list into the
# runtime's licenses); Rust's std is listed there with python.cpio's.
mkdir -p "$B/licenses"
cargo +"$NIGHTLY" metadata --format-version 1 --offline --manifest-path "$B/src/Cargo.toml" \
    --config "patch.crates-io.libc.path=\"$R/libc\"" "${PATCHED[@]}" \
    --filter-platform "$HOST_TRIPLE" 2>/dev/null \
  | python3 -c '
import json, sys
meta = json.load(sys.stdin)
used = {n["id"] for n in meta["resolve"]["nodes"]}
ours = {p["id"] for p in meta["packages"] if p["source"] is None}
for p in sorted(meta["packages"], key=lambda p: p["name"]):
    if p["id"] in used and p["id"] not in ours:
        print(p["name"], p["version"] + ":", p.get("license") or "see crate", p.get("repository") or "")
' > "$B/licenses/rust-tools-crates.txt"
# jq's builtin definitions (jq/src/builtin.jq) and git-lfs's manual pages and completion scripts
# come from those projects, both MIT-licensed.
cp "$W/jq/COPYING-jq" "$B/licenses/jq.MIT.txt"
cp "$W/git-lfs/LICENSE-git-lfs.md" "$B/licenses/git-lfs.MIT.txt"
for bin in "${BINS[@]}"; do echo "== rust tools: $(du -k "$STAGE/usr/bin/$bin" | cut -f1) KiB $bin"; done
