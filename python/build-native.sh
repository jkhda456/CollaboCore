#!/usr/bin/env bash
# The Rust halves of pydantic-core and jiter (PyO3 extension modules), built from their sdists for
# wasm32-unknown-linux-musl as static libraries and linked into the interpreter as built-in modules:
#
#   deps/lib/libpydantic_core.a   PyInit__pydantic_core -> built-in "pydantic_core._pydantic_core"
#   deps/lib/libjiter.a           PyInit_jiter          -> built-in "jiter.jiter"
#
# The guest has no dlopen, so an extension module cannot be a .so there; a built-in module under
# the same dotted name is what `from ._pydantic_core import ...` finds first (BuiltinImporter
# precedes the path finders). The Python half of each package comes from its wheel
# (python/install-packages.py). Versions and checksums: python/packages.lock.
#
# Two libraries rather than one: pydantic-core uses PyO3 0.28 and jiter 0.29, and cargo allows
# only one crate that links "python" per build. Each archive carries its own copy of Rust's std
# (identical: same sysroot), which the interpreter's link tolerates (see python/build.sh).
set -euo pipefail

P="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$P")"
R="$ROOT/.tools/rust-wasm"
D="$P/deps"
B="$P/build-native"
TARGET=wasm32-unknown-linux-musl
NIGHTLY=nightly-2026-07-22
FORCE=",${FORCE:-},"
export RUSTUP_HOME="$ROOT/.tools/rustup" CARGO_HOME="$ROOT/.tools/cargo"
export PATH="$CARGO_HOME/bin:$PATH"
export RUSTC="$R/bin/rustc"
[[ -x "$RUSTC" ]] || { echo "no Rust for the guest; run python/build-rust-toolchain.sh" >&2; exit 1; }

HOST_TRIPLE="$(rustc +"$NIGHTLY" -vV | sed -n 's/^host: //p')"
HOST_ENV="$(echo "$HOST_TRIPLE" | tr a-z- A-Z_)"
export "CARGO_TARGET_${HOST_ENV}_LINKER=clang-19" "CC_${HOST_TRIPLE//-/_}=clang-19" "CXX_${HOST_TRIPLE//-/_}=clang++-19"

# PyO3 cannot ask the guest interpreter about itself, so it is told: CPython 3.13, static, 32-bit.
# No link lines: the interpreter's own link supplies every Py* symbol.
mkdir -p "$B" "$D/lib"
cat > "$B/pyo3-config.txt" <<'EOF'
implementation=CPython
version=3.13
shared=false
abi3=false
lib_name=python3.13
pointer_width=32
build_flags=
suppress_build_script_link_lines=true
EOF
export PYO3_CONFIG_FILE="$B/pyo3-config.txt"

# sdist NAME: the checked source tarball from packages.lock, unpacked fresh; prints the directory.
sdist() {
  local url sum file
  read -r _ _ _ url sum < <(grep -E "^sdist +$1 " "$P/packages.lock")
  file="$P/src/$(basename "$url")"
  [[ -f "$file" ]] || { echo "== fetch $(basename "$url")" >&2; curl -fsSL --retry 3 -o "$file.part" "$url" && mv "$file.part" "$file"; }
  echo "$sum  $file" | sha256sum -c --quiet - >&2 || { echo "checksum mismatch: $file (deleted)" >&2; rm -f "$file"; exit 1; }
  local dir="$B/$(tar -tzf "$file" | head -1 | cut -d/ -f1)"
  rm -rf "$dir"; tar -xzf "$file" -C "$B"
  echo "$dir"
}

# build NAME MANIFEST LIBNAME OUT: `cargo rustc` turns the crate's cdylib into a staticlib.
build() {
  local name=$1 manifest=$2 lib=$3 out=$4
  if [[ "$FORCE" != *",$name,"* && -f "$out" ]]; then echo "== $name: up to date"; return; fi
  echo "== $name (Rust -> $TARGET)"
  (
    cd "$(dirname "$manifest")"
    # The libc crate as this platform's ABI has it (ILP32 musl), not upstream's WALI one.
    # Source paths stay in panic messages; name them for the guest, not this machine.
    RUSTFLAGS="--remap-path-prefix=$B=/usr/src --remap-path-prefix=$CARGO_HOME=/usr/src/cargo" \
    cargo +"$NIGHTLY" rustc --release --lib --target "$TARGET" --manifest-path "$manifest" \
      --config "patch.crates-io.libc.path=\"$R/libc\"" --crate-type staticlib \
      > "$B/$name.log" 2>&1 || { tail -40 "$B/$name.log"; echo "FAILED: $name (log: $B/$name.log)" >&2; exit 1; }
  )
  local target_dir
  target_dir="$(cargo +"$NIGHTLY" metadata --format-version 1 --no-deps --manifest-path "$manifest" \
    | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
  cp "$target_dir/$TARGET/release/lib$lib.a" "$out"
  # (Not `nm | grep -q`: grep stopping early would fail the pipe under pipefail.)
  [[ "$(llvm-nm-19 "$out" 2>/dev/null)" == *" T PyInit_"* ]] || { echo "$out has no PyInit_ symbol" >&2; exit 1; }
}

S="$(sdist pydantic-core)"
build pydantic-core "$S/Cargo.toml" _pydantic_core "$D/lib/libpydantic_core.a"
S="$(sdist jiter)"
build jiter "$S/crates/jiter-python/Cargo.toml" jiter_python "$D/lib/libjiter.a"
ls -l "$D/lib/libpydantic_core.a" "$D/lib/libjiter.a" | awk '{printf "  %-24s %s\n", $NF, $5}' | sed "s#$D/lib/##"
