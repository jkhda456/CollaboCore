#!/usr/bin/env bash
# Build linux/vmlinux.wasm (tombl/linux WebAssembly port).
#
# Mirrors flake.nix: LLVM 19 toolchain, `make defconfig` then `make vmlinux.wasm`.
# The build is in-tree (arch/wasm/Makefile references arch/wasm/scripts/sections.pl
# relative to the source root, so O= is not usable).
#
# Usage:
#   ./build.sh deps      # print missing tools + the apt command to fix it
#   ./build.sh           # defconfig (if no .config) + vmlinux.wasm
#   ./build.sh config    # force re-run of defconfig
#   ./build.sh clean     # make clean (keeps .config)
#   ./build.sh distclean # make distclean (removes .config too)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="$ROOT/linux"
LLVM_VER="${LLVM_VER:-19}"
JOBS="${JOBS:-$(nproc)}"

# Tools the build shells out to. wasm-ld/llvm-* come from lld-N / llvm-N.
required=(
  make perl bc flex bison pkg-config dtc wasm2wat
  "clang-$LLVM_VER" "ld.lld-$LLVM_VER" "wasm-ld-$LLVM_VER"
  "llvm-ar-$LLVM_VER" "llvm-nm-$LLVM_VER" "llvm-objcopy-$LLVM_VER"
  "llvm-objdump-$LLVM_VER" "llvm-readelf-$LLVM_VER" "llvm-strip-$LLVM_VER"
)
apt_pkgs="make flex bison bc pkg-config libncurses-dev device-tree-compiler wabt clang-$LLVM_VER lld-$LLVM_VER llvm-$LLVM_VER"

check_deps() {
  local missing=()
  for t in "${required[@]}"; do
    command -v "$t" >/dev/null 2>&1 || missing+=("$t")
  done
  if ((${#missing[@]})); then
    echo "missing tools: ${missing[*]}" >&2
    echo "install with:  sudo apt-get install -y $apt_pkgs" >&2
    return 1
  fi
  echo "toolchain ok (LLVM $LLVM_VER, $(clang-$LLVM_VER --version | head -1))"
}

cmd="${1:-build}"
case "$cmd" in
  deps) check_deps; exit ;;
esac

check_deps
cd "$SRC"
MAKE=(make "LLVM=-$LLVM_VER" "-j$JOBS")
export KBUILD_BUILD_TIMESTAMP="1970-01-01 00:00:00 UTC"

case "$cmd" in
  config)    "${MAKE[@]}" defconfig ;;
  clean)     "${MAKE[@]}" clean ;;
  distclean) "${MAKE[@]}" distclean ;;
  build)
    [[ -f .config ]] || "${MAKE[@]}" defconfig
    "${MAKE[@]}" vmlinux.wasm
    ls -lh vmlinux.wasm
    ;;
  *) echo "unknown command: $cmd" >&2; exit 2 ;;
esac
