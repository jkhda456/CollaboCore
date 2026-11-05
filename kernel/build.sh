#!/usr/bin/env bash
# Build linux/vmlinux.wasm (tombl/linux WebAssembly port).
#
# Mirrors flake.nix: LLVM 19 toolchain, `make defconfig` then `make vmlinux.wasm`.
# The build is in-tree (arch/wasm/Makefile references arch/wasm/scripts/sections.pl
# relative to the source root, so O= is not usable).
#
# Our changes to the clone are kernel/patches/*.patch, applied before a build (idempotent: a
# patch that is already applied is skipped).
#
# Usage:
#   bash build.sh deps      # print missing tools + the apt command to fix it
#   bash build.sh           # defconfig (if no .config) + vmlinux.wasm
#   bash build.sh config    # force re-run of defconfig
#   bash build.sh clean     # make clean (keeps .config)
#   bash build.sh distclean # make distclean (removes .config too)
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
    # Which patches the tree already has: undo them from the last to the first on a copy of the
    # files they touch. Checked one by one, a patch whose lines a later one changed (ramfs-xattr,
    # then tmpfs-size-limit) would look missing and be applied again.
    patches=()
    for patch in "$ROOT"/patches/*.patch; do [[ -f "$patch" ]] && patches+=("$patch"); done
    probe="$(mktemp -d)"
    trap 'rm -rf "$probe"' EXIT
    for patch in "${patches[@]}"; do sed -n 's|^+++ b/||p' "$patch"; done | sort -u | while read -r f; do
      if [[ -f "$f" ]]; then mkdir -p "$probe/$(dirname "$f")"; cp "$f" "$probe/$f"; fi
    done
    applied=()
    for ((i = ${#patches[@]} - 1; i >= 0; i--)); do
      (cd "$probe" && git apply --reverse "${patches[i]}" 2>/dev/null) && applied[i]=1
    done
    for i in "${!patches[@]}"; do
      if [[ -n "${applied[i]:-}" ]]; then
        echo "== patch already applied: $(basename "${patches[i]}")"
      else
        echo "== apply patch: $(basename "${patches[i]}")"
        git apply "${patches[i]}"
      fi
    done
    [[ -f .config ]] || "${MAKE[@]}" defconfig
    "${MAKE[@]}" vmlinux.wasm
    ls -lh vmlinux.wasm
    ;;
  *) echo "unknown command: $cmd" >&2; exit 2 ;;
esac
