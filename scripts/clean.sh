#!/usr/bin/env bash
# Remove what the build produced. What is kept depends on how far you want to go:
#
#   scripts/clean.sh            dist/ only — the packaged output (a rebuild is quick)
#   scripts/clean.sh build      + every intermediate build directory (guest, Python, engine,
#                               Dart and Flutter caches). Downloaded sources stay, so a full
#                               rebuild needs no network.
#   scripts/clean.sh all        + downloaded sources and .tools (kernel/linux and
#                               third_party/* clones are left alone: they have their own git)
#   DRY=1 scripts/clean.sh all  print what would go, delete nothing
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LEVEL="${1:-dist}"

remove() {
  for path in "$@"; do
    [[ -e "$path" ]] || continue
    printf '  %-56s %s\n' "${path#"$ROOT"/}" "$(du -sh "$path" 2>/dev/null | cut -f1)"
    [[ -n "${DRY:-}" ]] || rm -rf "$path"
  done
}

echo "== dist"
remove "$ROOT/dist"

if [[ "$LEVEL" == build || "$LEVEL" == all ]]; then
  echo "== intermediate build output"
  remove "$ROOT/userspace/build" "$ROOT/userspace/out" "$ROOT/userspace/sysroot" \
         "$ROOT/python/build-host" "$ROOT/python/build-wasm" "$ROOT/python/build-zlib" \
         "$ROOT/python/host" "$ROOT/python/stage" "$ROOT/python/out" "$ROOT/python/zlib" \
         "$ROOT/python/deps" "$ROOT/python/build-deps" "$ROOT/python/build-native" "$ROOT/python/build-etc" \
         "$ROOT/engine/target" "$ROOT/tests/node_modules" \
         "$ROOT/dart/collabo_core/.dart_tool" "$ROOT/flutter/collabo_core_demo/build" \
         "$ROOT/flutter/collabo_core_demo/.dart_tool"
  remove "$ROOT"/python/*.log "$ROOT"/engine/build.log
  # The kernel keeps its own object tree; `make clean` there is the kernel's business.
  [[ -d "$ROOT/kernel/linux" ]] && echo "  (kernel/linux: run 'make ARCH=wasm clean' in it yourself)"
fi

if [[ "$LEVEL" == all ]]; then
  echo "== downloaded sources and tools"
  remove "$ROOT/userspace/src" "$ROOT/userspace/kheaders" "$ROOT/python/src" "$ROOT/.tools"
  echo "  (kernel/linux and third_party/* are clones; delete them by hand if you mean it)"
fi

case "$LEVEL" in
  dist|build|all) ;;
  *) echo "unknown level: $LEVEL (dist | build | all)" >&2; exit 2 ;;
esac
[[ -n "${DRY:-}" ]] && echo "(dry run: nothing was deleted)"
echo "clean: $LEVEL"
