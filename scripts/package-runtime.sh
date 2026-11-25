#!/usr/bin/env bash
# Package the desktop runtime: one self-contained folder per platform, for an app to ship and spawn.
#
#   dist/runtime/collabo-core-<platform>/
#     bin/collabo-core-engine[.exe]   the native engine (engine/, Rust + wasmtime)
#     launcher[.exe], launcher.conf   starts the engine as launcher.conf says, with no arguments
#                                     (launcher --config FILE reads another; its bin/app keys
#                                     point at an engine and images kept elsewhere)
#     work/                           the folder launcher.conf shares with the guest as /work
#     app/images/vmlinux.wasm         the kernel
#     app/images/initramfs.cpio       guest root (busybox, /init, the agent)
#     app/images/python.cpio          CPython 3.13 overlay
#     app/images/tools.cpio           tools overlay (curl, ssh, git, screen, gui)
#     app/images/addons/*             add-ons, off unless the app asks (addons/README.md)
#     app/licenses/*
#     manifest.json                   { platform, engine, protocol, entry, addons, files }
#                                     (scripts/runtime-manifest.py; files: the sha256 of the
#                                     launcher, bin/ and app/, not launcher.conf, which is meant
#                                     to be edited; release.sh writes it again from what is there)
#
# Start it as:  <dir>/bin/collabo-core-engine <entry arguments…> --stdio
# (the Dart package reads `entry` from the manifest and adds --stdio; see dart/collabo_core).
#
#   scripts/package-runtime.sh                 this machine's platform
#   PLATFORMS="linux-x64 linux-arm64" scripts/package-runtime.sh
#   ARCHIVE=1 ...                              also write .tar.gz (.zip for Windows) next to each
#
# Cross-building needs the Rust target and a linker for it (rustup target add …), so a release
# for every platform is built on each platform, as .github/workflows does. On Windows (Git Bash,
# MSYS2) this hands over to scripts/package-runtime-windows.ps1, which picks the toolchain for
# each of win-x64 and win-arm64 on either machine (PLATFORMS, default: this machine's).
set -euo pipefail
PYTHON="${PYTHON:-python3}"   # build.py passes its own interpreter

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENGINE="$ROOT/dist/engine"
OUT="$ROOT/dist/runtime"
# The toolchain from .tools when this machine has one (scripts/bootstrap-tools.sh), else
# whatever cargo is already installed (CI runners bring their own).
if [[ -x "$ROOT/.tools/cargo/bin/cargo" ]]; then
  export RUSTUP_HOME="$ROOT/.tools/rustup" CARGO_HOME="$ROOT/.tools/cargo"
  export PATH="$ROOT/.tools/rustup/toolchains/stable-$(uname -m)-unknown-linux-gnu/bin:$PATH"
fi

# This machine, in the platform names the runtime folders use.
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) HERE=linux-x64 ;;
  Linux-aarch64) HERE=linux-arm64 ;;
  Darwin-x86_64) HERE=darwin-x64 ;;
  Darwin-arm64) HERE=darwin-arm64 ;;
  MINGW*|MSYS*|CYGWIN*)
    # The PowerShell script knows the machine (an x64 bash under emulation on ARM64 says x86_64
    # here) and the toolchains each platform needs.
    args=(-Platform "$(echo "${PLATFORMS:-host}" | tr ' ' ',')")
    [[ -n "${ARCHIVE:-}" ]] && args+=(-Archive)
    exec powershell -NoProfile -ExecutionPolicy Bypass -File "$(cygpath -w "$(dirname "${BASH_SOURCE[0]}")/package-runtime-windows.ps1")" "${args[@]}" ;;
  *) HERE="" ;;
esac
PLATFORMS="${PLATFORMS:-$HERE}"
[[ -n "$PLATFORMS" ]] || { echo "unknown platform $(uname -sm); set PLATFORMS" >&2; exit 2; }
[[ -f "$ENGINE/kernel/vmlinux.wasm" ]] || { echo "missing dist/engine; run: python3 build.py engine" >&2; exit 1; }
command -v cargo >/dev/null || { echo "no cargo; run: python3 build.py tools" >&2; exit 1; }
# the crates.io packages the engine builds with patches of ours (engine/Cargo.toml's [patch.crates-io])
bash "$ROOT/engine/patch-crates.sh"
mkdir -p "$OUT"

triple() {
  case "$1" in
    linux-x64) echo x86_64-unknown-linux-gnu ;;
    linux-arm64) echo aarch64-unknown-linux-gnu ;;
    darwin-x64) echo x86_64-apple-darwin ;;
    darwin-arm64) echo aarch64-apple-darwin ;;
    win-x64) echo x86_64-pc-windows-msvc ;;
    win-arm64) echo aarch64-pc-windows-msvc ;;
    *) echo "unknown platform $1" >&2; exit 2 ;;
  esac
}

for platform in $PLATFORMS; do
  target="$(triple "$platform")"
  echo "== build $platform ($target)"
  if [[ "$platform" == "$HERE" ]]; then
    (cd "$ROOT/engine" && cargo build --release --quiet)
    built="$ROOT/engine/target/release/collabo-core-engine"
    launcher="$ROOT/engine/target/release/launcher"
  else
    (cd "$ROOT/engine" && cargo build --release --quiet --target "$target")
    built="$ROOT/engine/target/$target/release/collabo-core-engine"
    launcher="$ROOT/engine/target/$target/release/launcher"
  fi
  case "$platform" in win-*) exe=collabo-core-engine.exe ;; *) exe=collabo-core-engine ;; esac
  [[ -f "$built" ]] || built="$built.exe"
  [[ -f "$launcher" ]] || launcher="$launcher.exe"
  [[ -f "$built" ]] || { echo "the engine was not built for $platform" >&2; exit 1; }

  dir="$OUT/collabo-core-$platform"
  rm -rf "$dir"; mkdir -p "$dir/bin" "$dir/app/images" "$dir/work"
  cp "$built" "$dir/bin/$exe"
  chmod +x "$dir/bin/$exe"
  cp "$launcher" "$dir/$(basename "$launcher")"
  chmod +x "$dir/$(basename "$launcher")"
  cp "$ROOT/engine/launcher.conf" "$dir/launcher.conf"
  cp "$ENGINE/kernel/vmlinux.wasm" "$dir/app/images/"
  cp "$ENGINE"/images/*.cpio "$dir/app/images/"
  [[ -d "$ENGINE/images/addons" ]] && cp -r "$ENGINE/images/addons" "$dir/app/images/addons"
  cp -r "$ENGINE/licenses" "$dir/app/licenses"

  # what release.sh writes again before it archives the folder (an image may change after this)
  "$PYTHON" "$ROOT/scripts/runtime-manifest.py" "$dir"

  if [[ -n "${ARCHIVE:-}" ]]; then
    case "$platform" in
      win-*) (cd "$OUT" && rm -f "collabo-core-$platform.zip" && "$PYTHON" -c "
import os, sys, zipfile
root = sys.argv[1]
with zipfile.ZipFile(root + '.zip', 'w', zipfile.ZIP_DEFLATED) as z:
    for d, _, fs in os.walk(root):
        for f in fs: z.write(os.path.join(d, f))" "collabo-core-$platform") ;;
      *) tar -czf "$OUT/collabo-core-$platform.tar.gz" -C "$OUT" "collabo-core-$platform" ;;
    esac
  fi
  echo "== $platform: $(du -sh "$dir" | cut -f1)  $dir"
done
