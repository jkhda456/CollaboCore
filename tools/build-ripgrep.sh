#!/usr/bin/env bash
# ripgrep (`rg`) for the guest (wasm32 Linux, musl), the `ripgrep` step of tools/build.sh, into
# tools/stage/usr/bin/rg, and its crates' licenses into build/ripgrep/licenses. Unlike the node
# add-on's, with PCRE2 (`rg -P`).
#
# As addons/node/tools/build-ripgrep.sh builds it (the node add-on carries its own copy for
# Claude Code's Grep and Glob): the release tag, patches/ripgrep-*.patch (no fork, no mmap), the
# guest's Rust toolchain (python/build-rust-toolchain.sh) and tombl's libc crate fork, linked
# through wasm-cc.
#
#   tools/build-ripgrep.sh        FORCE=1 refetches and rebuilds from scratch
set -euo pipefail
N="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$N")"
U="$ROOT/userspace"
R="$ROOT/.tools/rust-wasm"
TARGET=wasm32-unknown-linux-musl
NIGHTLY=nightly-2026-07-22   # as python/build-rust-toolchain.sh
SRC="$N/build/ripgrep/src"
B="$N/build/ripgrep"
STAGE="${STAGE:-$N/stage}"

RG_URL=https://github.com/BurntSushi/ripgrep
RG_COMMIT=e89fff89ac9af12e8d4ce9d5fd07beb408ca730f   # tag 15.2.0

[[ -f "$U/sysroot/lib/crt1.o" ]] || { echo "missing userspace/sysroot; run: ./build.sh userspace" >&2; exit 1; }
bash "$ROOT/python/build-rust-toolchain.sh" >/dev/null

export RUSTUP_HOME="$ROOT/.tools/rustup" CARGO_HOME="$ROOT/.tools/cargo"
export PATH="$CARGO_HOME/bin:$PATH"
HOST_TRIPLE="$(rustc +"$NIGHTLY" -vV | sed -n 's/^host: //p')"
HOST_ENV="$(echo "$HOST_TRIPLE" | tr a-z- A-Z_)"
export "CARGO_TARGET_${HOST_ENV}_LINKER=clang-19" "CC_${HOST_TRIPLE//-/_}=clang-19"

# The release tag, patched.
if [[ -n "${FORCE:-}" || "$(git -C "$SRC" rev-parse HEAD 2>/dev/null)" != "$RG_COMMIT" ]]; then
  rm -rf "$B"
  mkdir -p "$SRC"
  git -C "$SRC" init -q
  git -C "$SRC" remote add origin "$RG_URL"
  git -C "$SRC" fetch -q --depth 1 origin "$RG_COMMIT"
  git -C "$SRC" checkout -q FETCH_HEAD
fi
git -C "$SRC" checkout -q -- .     # back to the release before patching
for p in "$N"/patches/ripgrep-*.patch; do git -C "$SRC" apply "$p"; done

LOG="$B/build.log"
# rustc hands the linker --target=wasm32-unknown-linux-musl (the spec's name), which clang-19
# behind wasm-cc does not know; wasm-cc sets its own --target=wasm32.
cat > "$B/wasm-cc-rust" <<SH
#!/bin/sh
for a; do shift; case "\$a" in --target=*) ;; *) set -- "\$@" "\$a" ;; esac; done
exec "$U/bin/wasm-cc" "\$@"
SH
chmod +x "$B/wasm-cc-rust"
# The lock file may name a newer libc than the fork; resolve libc to the fork so the patch takes.
LIBC_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$R/libc/Cargo.toml" | head -1)"
cargo +"$NIGHTLY" update --manifest-path "$SRC/Cargo.toml" --config "patch.crates-io.libc.path=\"$R/libc\"" \
  -p libc --precise "$LIBC_VERSION" > "$LOG" 2>&1 || true
# With PCRE2 (-P), which pcre2-sys compiles from its bundled C sources by wasm-cc (the cc crate
# also passes --target). Its build script turns PCRE2's JIT on for every target but a few, wasm
# among them; the JIT (sljit) has no wasm backend, so this compiler leaves -DSUPPORT_JIT=1 out
# and -P runs PCRE2's interpreter.
cat > "$B/wasm-cc-pcre2" <<SH
#!/bin/sh
for a; do shift; case "\$a" in --target=*|-DSUPPORT_JIT=1) ;; *) set -- "\$@" "\$a" ;; esac; done
exec "$U/bin/wasm-cc" "\$@"
SH
chmod +x "$B/wasm-cc-pcre2"
export CC_wasm32_unknown_linux_musl="$B/wasm-cc-pcre2" AR_wasm32_unknown_linux_musl=llvm-ar-19
export WASM_MUSL_SYSROOT="$U/sysroot"   # cc wants it for wasm32 Linux targets (--sysroot)
# The stack: 8 MiB, as the guest's other programs (regex compilation recurses).
WASM_LD="$R/bin/wasm-ld" RUSTC="$R/bin/rustc" \
RUSTFLAGS="-Clinker=$B/wasm-cc-rust -Clink-arg=-Wl,-z,stack-size=8388608 --remap-path-prefix=$SRC=/usr/src/ripgrep --remap-path-prefix=$CARGO_HOME=/usr/src/cargo" \
  cargo +"$NIGHTLY" build --release --target "$TARGET" --features pcre2 \
    --manifest-path "$SRC/Cargo.toml" --target-dir "$B/target" \
    --config "patch.crates-io.libc.path=\"$R/libc\"" \
    --config profile.release.debug=0 --config profile.release.strip=true \
    >> "$LOG" 2>&1 || { tail -40 "$LOG"; echo "FAILED: rg (log: $LOG)" >&2; exit 1; }
OUT="$B/target/$TARGET/release/rg.wasm"
[[ -f "$OUT" ]] || OUT="$B/target/$TARGET/release/rg"
[[ -f "$OUT" ]] || { echo "cargo wrote no rg binary" >&2; exit 1; }
install -D -m755 "$OUT" "$STAGE/usr/bin/rg"

# ripgrep's own license (MIT or Unlicense) and the crates linked in (scripts/build-engine.sh
# copies both into the runtime's licenses).
mkdir -p "$B/licenses"
cp "$SRC/LICENSE-MIT" "$B/licenses/ripgrep.MIT.txt"
# PCRE2 10.46, which pcre2-sys 0.2.10 bundles without its licence (the release's LICENCE.md)
cp "$N/licenses/pcre2-10.46-LICENCE.md" "$B/licenses/pcre2.BSD-3-Clause.txt"
cargo +"$NIGHTLY" metadata --format-version 1 --manifest-path "$SRC/Cargo.toml" \
    --config "patch.crates-io.libc.path=\"$R/libc\"" --filter-platform "$HOST_TRIPLE" 2>/dev/null \
  | python3 -c '
import json, sys
meta = json.load(sys.stdin)
used = {n["id"] for n in meta["resolve"]["nodes"]}
for p in sorted(meta["packages"], key=lambda p: p["name"]):
    if p["id"] in used and p["name"] != "ripgrep":
        print(p["name"], p["version"] + ":", p.get("license") or "see crate", p.get("repository") or "")
' > "$B/licenses/ripgrep-crates.txt"
ls -l "$STAGE/usr/bin/rg" | awk '{print "  " $NF, $5}'
