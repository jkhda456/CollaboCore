#!/usr/bin/env bash
# Rust for the guest (wasm32-unknown-linux-musl), for the native parts of pydantic-core and jiter.
# A port of third_party/distro/distro/rust-toolchain/package.nix without Nix:
#
#   .tools/rust-wasm/target/wasm32-unknown-linux-musl.json   the target spec (rustc finds it via
#                                                             RUST_TARGET_PATH, so cargo sees a name)
#   .tools/rust-wasm/sysroot/                                 host std from the nightly + our wasm std
#   .tools/rust-wasm/libc/                                    tombl's libc crate fork (ILP32 musl ABI)
#   .tools/rust-wasm/bin/rustc                                rustc with all of the above applied
#
# Nightly is required, as in distro: a custom target spec needs -Zunstable-options and std is built
# from source (-Zbuild-std) with three small patches. Everything is pinned: the nightly date is the
# one distro's fenix revision resolves to, the libc fork is at distro's commit, and std's registry
# dependencies are locked by distro's Cargo.lock.
set -euo pipefail

P="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$P")"
U="$ROOT/userspace"
R="$ROOT/.tools/rust-wasm"
DISTRO="$ROOT/third_party/distro/distro/rust-toolchain"
NIGHTLY=nightly-2026-07-22
LIBC_REV=fc8cc62b93f1c374e944d7880e71aa16434b7c6e
TARGET=wasm32-unknown-linux-musl
export RUSTUP_HOME="$ROOT/.tools/rustup" CARGO_HOME="$ROOT/.tools/cargo"
export PATH="$CARGO_HOME/bin:$PATH"

STD="$R/sysroot/lib/rustlib/$TARGET/lib"
if [[ -f "$STD/libstd.stamp" && -z "${FORCE_RUST:-}" ]]; then
  echo "== rust-wasm: up to date ($NIGHTLY)"; exit 0
fi

echo "== rust $NIGHTLY"
rustup toolchain install "$NIGHTLY" --profile minimal --component rust-src >/dev/null
TC="$(rustc +"$NIGHTLY" --print sysroot)"
HOST_TRIPLE="$(rustc +"$NIGHTLY" -vV | sed -n 's/^host: //p')"
# Build scripts and proc macros are host programs; this machine's C compiler is clang-19 (no `cc`).
HOST_ENV="$(echo "$HOST_TRIPLE" | tr a-z- A-Z_)"
export "CARGO_TARGET_${HOST_ENV}_LINKER=clang-19" "CC_${HOST_TRIPLE//-/_}=clang-19" "CXX_${HOST_TRIPLE//-/_}=clang++-19"

echo "== libc fork"
if [[ ! -d "$R/libc/.git" ]]; then
  git init -q "$R/libc"; git -C "$R/libc" remote add origin https://github.com/tombl/libc.git
fi
git -C "$R/libc" fetch -q --depth 1 origin "$LIBC_REV"
git -C "$R/libc" checkout -q FETCH_HEAD

# The target spec. It mirrors wasm32-wali-linux-musl where the platforms agree (musl on a wasm32
# Linux kernel) and differs where the ABIs do: ILP32, musl's crt1 entry, static linking through
# wasm-cc, the platform's link flags (userspace/build.sh LINK_FLAGS). panic=abort: wasm here has no
# unwinder. Target features are the ones every guest object is built with (wasm-cc's -m flags).
mkdir -p "$R/target" "$R/bin"
cat > "$R/target/$TARGET.json" <<EOF
{
  "arch": "wasm32", "os": "linux", "env": "musl", "vendor": "unknown",
  "llvm-target": "$TARGET",
  "data-layout": "e-m:e-p:32:32-p10:8:8-p20:8:8-i64:64-i128:128-n32:64-S128-ni:1:10:20",
  "target-pointer-width": 32,
  "max-atomic-width": 64,
  "target-family": ["wasm", "unix"],
  "features": "+atomics,+bulk-memory,+mutable-globals,+sign-ext",
  "entry-name": "__main_argc_argv",
  "main-needs-argc-argv": true,
  "linker": "$U/bin/wasm-cc",
  "linker-flavor": "wasm-lld-cc",
  "linker-is-gnu": false,
  "lld-flavor": "wasm",
  "pre-link-args": { "wasm-lld-cc": ["-Wl,--import-memory", "-Wl,--max-memory=4294967296", "-Wl,--shared-memory",
                                     "-Wl,--export-table", "-Wl,-z,stack-size=8388608", "-Wl,--no-demangle"] },
  "crt-static-default": true,
  "crt-static-respected": true,
  "crt-objects-fallback": "false",
  "dynamic-linking": false,
  "relocation-model": "static",
  "panic-strategy": "abort",
  "singlethread": false,
  "has-thread-local": true,
  "tls-model": "local-exec",
  "is-like-wasm": true,
  "eh-frame-header": false,
  "emit-debug-gdb-scripts": false,
  "generate-arange-section": false,
  "limit-rdylib-exports": false
}
EOF

# rustc that knows the target (and, once built, the sysroot holding its std).
cat > "$R/bin/rustc" <<EOF
#!/bin/sh
export RUST_TARGET_PATH="$R/target\${RUST_TARGET_PATH:+:\$RUST_TARGET_PATH}"
export RUSTUP_HOME="$RUSTUP_HOME"
# While std itself is being built there is no wasm sysroot yet (RUST_WASM_NO_SYSROOT=1).
[ -n "\${RUST_WASM_NO_SYSROOT:-}" ] || set -- --sysroot="$R/sysroot" "\$@"
exec "$TC/bin/rustc" -Zunstable-options "\$@"
EOF
chmod +x "$R/bin/rustc"
# The toolchain's wasm-ld (LLD from the same LLVM that compiles the Rust objects), for wasm-cc.
printf '#!/bin/sh\nexec "%s" -flavor wasm "$@"\n' "$TC/lib/rustlib/$HOST_TRIPLE/bin/rust-lld" > "$R/bin/wasm-ld"
chmod +x "$R/bin/wasm-ld"

echo "== std for $TARGET (build-std, patched)"
rm -rf "$R/src" "$R/std-build"
mkdir -p "$R/src"
cp -r "$TC/lib/rustlib/src/rust/." "$R/src/"
chmod -R u+w "$R/src"
for patch in "$DISTRO"/patches/*.patch; do patch -p1 --quiet -d "$R/src" < "$patch"; done
sed -i "s|^\[patch.crates-io\]$|[patch.crates-io]\nlibc = { path = \"$R/libc\" }|" "$R/src/library/Cargo.toml"
install -m644 "$DISTRO/Cargo.lock" "$R/src/library/Cargo.lock"

mkdir -p "$R/std-build/src" "$R/std-build/.cargo"
touch "$R/std-build/src/lib.rs"
printf '[package]\nname = "sysroot-build"\nversion = "0.0.0"\nedition = "2021"\n' > "$R/std-build/Cargo.toml"
# build-std-features = []: std's default backtrace support needs mmap and an unwinder.
printf '[unstable]\nbuild-std = ["std", "panic_abort"]\nbuild-std-features = []\n' > "$R/std-build/.cargo/config.toml"
(
  cd "$R/std-build"
  export RUST_WASM_NO_SYSROOT=1 RUSTC="$R/bin/rustc"
  export __CARGO_TESTS_ONLY_SRC_ROOT="$R/src/library"
  # Source paths survive in std's panic messages: name them for the guest, not this machine.
  # Bitcode in the rlibs, as in the official ones: the extensions are built with fat LTO.
  export RUSTFLAGS="-Cembed-bitcode=yes --remap-path-prefix=$R/src=/usr/src/rust --remap-path-prefix=$CARGO_HOME=/usr/src/cargo"
  cargo +"$NIGHTLY" build --release --target "$TARGET" > "$R/std-build.log" 2>&1 \
    || { tail -30 "$R/std-build.log"; echo "FAILED: std for $TARGET (log: $R/std-build.log)" >&2; exit 1; }
)

rm -rf "$R/sysroot"; mkdir -p "$STD" "$R/sysroot/lib/rustlib"
ln -s "$TC/lib/rustlib/$HOST_TRIPLE" "$R/sysroot/lib/rustlib/$HOST_TRIPLE"
cp "$R/std-build/target/$TARGET/release/deps/"*.rlib "$STD/"
# panic=abort and no unwinder, but std's backtrace scaffolding still names _Unwind_ symbols.
"$U/bin/wasm-cc" -c "$DISTRO/unwind-stubs.c" -o "$R/std-build/unwind-stubs.o"
llvm-ar-19 rcs "$STD/libunwind.a" "$R/std-build/unwind-stubs.o"
# The license terms of the standard library that ends up in the guest (copied into the runtime's
# licenses by scripts/build-engine.sh).
cp "$TC/share/doc/rust/COPYRIGHT-library.html" "$R/COPYRIGHT-library.html"
echo "$NIGHTLY $LIBC_REV" > "$STD/libstd.stamp"
echo "== rust-wasm ready: $(ls "$STD"/*.rlib | wc -l) std crates in $STD"
