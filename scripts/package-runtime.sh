#!/usr/bin/env bash
# Package the desktop runtime: one self-contained folder per platform, for an app to ship and spawn.
#
#   dist/runtime/collabo-core-<platform>/
#     bin/collabo-core-engine[.exe]   the native engine (engine/, Rust + wasmtime)
#     app/images/vmlinux.wasm         the kernel
#     app/images/initramfs.cpio       guest root (busybox, /init, the agent)
#     app/images/python.cpio          CPython 3.13 overlay
#     app/images/tools.cpio           network tools overlay (curl, ssh, git)
#     app/licenses/*
#     manifest.json                   { platform, engine, protocol, entry, files }
#
# Start it as:  <dir>/bin/collabo-core-engine <entry arguments…> --stdio
# (the Dart package reads `entry` from the manifest and adds --stdio; see dart/collabo_core).
#
#   scripts/package-runtime.sh                 this machine's platform
#   PLATFORMS="linux-x64 linux-arm64" scripts/package-runtime.sh
#   ARCHIVE=1 ...                              also write .tar.gz (.zip for Windows) next to each
#
# Cross-building needs the Rust target and a linker for it (rustup target add …), so a release
# for every platform is built on each platform, as .github/workflows does.
set -euo pipefail

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
  MINGW*|MSYS*|CYGWIN*) HERE=win-x64 ;;
  *) HERE="" ;;
esac
PLATFORMS="${PLATFORMS:-$HERE}"
[[ -n "$PLATFORMS" ]] || { echo "unknown platform $(uname -sm); set PLATFORMS" >&2; exit 2; }
[[ -f "$ENGINE/kernel/vmlinux.wasm" ]] || { echo "missing dist/engine; run: python3 build.py engine" >&2; exit 1; }
command -v cargo >/dev/null || { echo "no cargo; run: python3 build.py tools" >&2; exit 1; }
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
    (cd "$ROOT/engine" && cargo build --release >/dev/null)
    built="$ROOT/engine/target/release/collabo-core-engine"
  else
    (cd "$ROOT/engine" && cargo build --release --target "$target" >/dev/null)
    built="$ROOT/engine/target/$target/release/collabo-core-engine"
  fi
  case "$platform" in win-*) exe=collabo-core-engine.exe ;; *) exe=collabo-core-engine ;; esac
  [[ -f "$built" ]] || built="$built.exe"
  [[ -f "$built" ]] || { echo "the engine was not built for $platform" >&2; exit 1; }

  dir="$OUT/collabo-core-$platform"
  rm -rf "$dir"; mkdir -p "$dir/bin" "$dir/app/images"
  cp "$built" "$dir/bin/$exe"
  chmod +x "$dir/bin/$exe"
  cp "$ENGINE/kernel/vmlinux.wasm" "$dir/app/images/"
  cp "$ENGINE"/images/*.cpio "$dir/app/images/"
  cp -r "$ENGINE/licenses" "$dir/app/licenses"

  python3 - "$dir" "$platform" "$exe" <<'PY'
import json, os, sys, hashlib
d, platform, exe = sys.argv[1:]
files = {}
for root, _, names in os.walk(d):
    for n in names:
        p = os.path.join(root, n)
        rel = os.path.relpath(p, d).replace(os.sep, "/")
        if rel != "manifest.json":
            files[rel] = hashlib.sha256(open(p, "rb").read()).hexdigest()
entry = ["bin/" + exe,
         "--kernel", "app/images/vmlinux.wasm",
         "--initramfs", "app/images/initramfs.cpio",
         "--python-image", "app/images/python.cpio"]
if os.path.exists(os.path.join(d, "app/images/tools.cpio")):
    entry += ["--tools-image", "app/images/tools.cpio"]
json.dump({"name": "collabo-core-runtime", "platform": platform, "engine": "wasmtime", "protocol": 1,
           "entry": entry, "files": dict(sorted(files.items()))},
          open(os.path.join(d, "manifest.json"), "w"), indent=1)
PY

  if [[ -n "${ARCHIVE:-}" ]]; then
    case "$platform" in
      win-*) (cd "$OUT" && rm -f "collabo-core-$platform.zip" && python3 -c "
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
