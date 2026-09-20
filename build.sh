#!/usr/bin/env bash
# Build everything, in order. Each step skips work whose output is up to date.
#
#   ./build.sh                  every step: tools kernel userspace python engine web runtime
#   ./build.sh engine web       only the named steps
#
# Steps
#   tools       fetch what the build needs into .tools (node, cmake/ninja, binaryen, rust)
#   kernel      the WebAssembly Linux kernel                       -> kernel/linux/vmlinux.wasm
#   userspace   musl, compiler-rt, busybox, /init and guest tools  -> userspace/out/initramfs.cpio
#   python      CPython 3.13 for the guest                         -> python/out/python.cpio
#   engine      collect the kernel, images and host JS             -> dist/engine
#   web         the browser build                                  -> dist/web
#   runtime     build the native engine and package it             -> dist/runtime/collabo-core-*
#
# Commands
#   ./build.sh test [--boot|--all]   run the checks (tests/run.sh)
#   ./build.sh clean [dist|build|all]  remove build output (scripts/clean.sh; DRY=1 to look first)
#   ./build.sh release              collect archives and checksums  -> dist/release
#   ./build.sh export DIR           copy the source tree, ready to commit, to DIR
#
# First time on a new machine: see scripts/bootstrap-tools.sh for the system packages.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# The comment block above is the help text.
usage() {
  awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$ROOT/build.sh"
}

case "${1:-}" in
  -h|--help|help) usage; exit 0 ;;
  test)    shift; exec "$ROOT/tests/run.sh" "$@" ;;
  clean)   shift; exec "$ROOT/scripts/clean.sh" "$@" ;;
  release) shift; exec "$ROOT/scripts/release.sh" "$@" ;;
  export)  shift; exec "$ROOT/scripts/export-source.sh" "$@" ;;
esac

steps=("$@")
[[ ${#steps[@]} -gt 0 ]] || steps=(tools kernel userspace python engine web runtime)

for step in "${steps[@]}"; do
  echo "######## $step"
  case "$step" in
    tools)     "$ROOT/scripts/bootstrap-tools.sh" ;;
    kernel)    "$ROOT/kernel/build.sh" ;;
    userspace) "$ROOT/userspace/build.sh" ;;
    python)    "$ROOT/python/build.sh" ;;
    engine)    "$ROOT/scripts/build-engine.sh" ;;
    web)       "$ROOT/scripts/build-web.sh" ;;
    runtime)   "$ROOT/scripts/package-runtime.sh" ;;
    *) echo "unknown step or command: $step" >&2; echo; usage >&2; exit 2 ;;
  esac
done
