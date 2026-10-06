#!/usr/bin/env bash
# The guest's display for GUI programs, part of tools.cpio (tools/build.sh runs this as its
# `gui` step; it also runs alone). Into tools/stage:
#
#   usr/bin/gui                     the display server and its command line (Rust, rust/)
#   usr/bin/gui-demo                a small GUI program (C, client/gui-demo.c)
#   usr/include/collabo_gui.h       the client library, for porting programs to the display
#   usr/lib/libcollabo-gui.a        (C, client/collabo_gui.c; no dependencies)
#
# and build/gui/licenses/ (the crates linked in). Cargo and the C compiles are incremental, so a
# run with nothing changed takes seconds.
#
# Needs the userspace step (musl, compiler-rt: userspace/sysroot) and the guest's Rust toolchain
# (python/build-rust-toolchain.sh, which this runs; it skips when it is up to date).
#   TEST=1 tools/gui/build.sh    also run the unit tests natively first
set -euo pipefail

G="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
N="$(dirname "$G")"
ROOT="$(dirname "$N")"
U="$ROOT/userspace"
R="$ROOT/.tools/rust-wasm"
TARGET=wasm32-unknown-linux-musl
NIGHTLY=nightly-2026-07-22   # as python/build-rust-toolchain.sh
B="$N/build/gui"
STAGE="${STAGE:-$N/stage}"
LINK_FLAGS=(-Wl,--import-memory -Wl,--max-memory=4294967296 -Wl,--shared-memory -Wl,--export-table -Wl,-z,stack-size=8388608)

[[ -f "$U/sysroot/lib/crt1.o" ]] || { echo "missing userspace/sysroot; run: ./build.sh userspace" >&2; exit 1; }
bash "$ROOT/python/build-rust-toolchain.sh" >/dev/null

export RUSTUP_HOME="$ROOT/.tools/rustup" CARGO_HOME="$ROOT/.tools/cargo"
export PATH="$CARGO_HOME/bin:$PATH"
# Build scripts are host programs, and this machine's C compiler is clang-19 (no `cc`).
HOST_TRIPLE="$(rustc +"$NIGHTLY" -vV | sed -n 's/^host: //p')"
HOST_ENV="$(echo "$HOST_TRIPLE" | tr a-z- A-Z_)"
export "CARGO_TARGET_${HOST_ENV}_LINKER=clang-19" "CC_${HOST_TRIPLE//-/_}=clang-19"
mkdir -p "$B" "$STAGE/usr/bin" "$STAGE/usr/include" "$STAGE/usr/lib"

if [[ -n "${TEST:-}" ]]; then
  echo "== gui: unit tests ($HOST_TRIPLE)"
  cargo +"$NIGHTLY" test --manifest-path "$G/rust/Cargo.toml" --target-dir "$B/host" --locked
fi

echo "== gui (Rust -> $TARGET)"
# Built from a copy: the libc patch below replaces a crates.io package in the lock file, which
# must not rewrite the checked-in one. rsync -c keeps unchanged files' times, so cargo rebuilds
# only after a change.
mkdir -p "$B/src"
rsync -a -c --delete "$G/rust/src/" "$B/src/src/"
rsync -a -c "$G/rust/Cargo.toml" "$G/rust/Cargo.lock" "$B/src/"
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
  -p libc --precise "$LIBC_VERSION" > "$B/gui.log" 2>&1 || true
# The link goes through wasm-cc with the toolchain's own wasm-ld; std is the one
# build-rust-toolchain.sh built; libc is tombl's fork (this platform's ILP32 musl ABI).
WASM_LD="$R/bin/wasm-ld" RUSTC="$R/bin/rustc" \
RUSTFLAGS="-Clinker=$B/wasm-cc-rust --remap-path-prefix=$B/src=/usr/src/gui --remap-path-prefix=$CARGO_HOME=/usr/src/cargo" \
  cargo +"$NIGHTLY" build --release --target "$TARGET" \
    --manifest-path "$B/src/Cargo.toml" --target-dir "$B/wasm" \
    --config "patch.crates-io.libc.path=\"$R/libc\"" \
    >> "$B/gui.log" 2>&1 || { tail -40 "$B/gui.log"; echo "FAILED: gui (log: $B/gui.log)" >&2; exit 1; }
GUI="$B/wasm/$TARGET/release/gui.wasm"
[[ -f "$GUI" ]] || GUI="$B/wasm/$TARGET/release/gui"
[[ -f "$GUI" ]] || { echo "cargo wrote no gui binary" >&2; exit 1; }
install -m755 "$GUI" "$STAGE/usr/bin/gui"

echo "== gui: client library and gui-demo (C -> wasm32)"
"$U/bin/wasm-cc" -std=c99 -Os -Wall -Wextra -Wno-unused-parameter -c "$G/client/collabo_gui.c" -o "$B/collabo_gui.o"
rm -f "$B/libcollabo-gui.a"
llvm-ar-19 rcs "$B/libcollabo-gui.a" "$B/collabo_gui.o"
"$U/bin/wasm-cc" -std=c99 -Os -Wall -Wextra -I"$G/client" -o "$B/gui-demo.wasm" "$G/client/gui-demo.c" \
  "$B/libcollabo-gui.a" "${LINK_FLAGS[@]}"
install -m755 "$B/gui-demo.wasm" "$STAGE/usr/bin/gui-demo"
install -m644 "$G/client/collabo_gui.h" "$STAGE/usr/include/collabo_gui.h"
install -m644 "$B/libcollabo-gui.a" "$STAGE/usr/lib/libcollabo-gui.a"

# The crates linked in, with their licenses, and the demo's font (scripts/build-engine.sh copies
# them into the runtime's licenses); Rust's std is listed there with python.cpio's.
mkdir -p "$B/licenses"
cargo +"$NIGHTLY" metadata --format-version 1 --offline --manifest-path "$B/src/Cargo.toml" \
    --config "patch.crates-io.libc.path=\"$R/libc\"" \
    --filter-platform "$HOST_TRIPLE" 2>/dev/null \
  | python3 -c '
import json, sys
meta = json.load(sys.stdin)
used = {n["id"] for n in meta["resolve"]["nodes"]}
for p in sorted(meta["packages"], key=lambda p: p["name"]):
    if p["id"] in used and p["name"] != "collabo-gui":
        print(p["name"], p["version"] + ":", p.get("license") or "see crate", p.get("repository") or "")
' > "$B/licenses/gui-crates.txt"
cp "$G/client/OFL.txt" "$B/licenses/gui-demo-font.OFL-1.1.txt"
echo "== gui: $(du -k "$STAGE/usr/bin/gui" | cut -f1) KiB gui, $(du -k "$STAGE/usr/bin/gui-demo" | cut -f1) KiB gui-demo"
