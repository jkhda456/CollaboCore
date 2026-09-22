#!/bin/sh
# Xcode build phase (Runner target, after "Bundle Framework"): copy the collaboCore runtime(s) into
# <App>.app/Contents/Resources/collabo_core_runtime/, where CollaboRuntime.locate() looks.
# Both architectures are copied when present, so a universal app finds the right one.
set -e
src="${COLLABO_CORE_RUNTIME_DIR:-$SRCROOT/../../../dist/runtime}"
dest="$BUILT_PRODUCTS_DIR/$CONTENTS_FOLDER_PATH/Resources/collabo_core_runtime"
mkdir -p "$dest"
found=0
for a in darwin-arm64 darwin-x64; do
  if [ -f "$src/collabo-core-$a/manifest.json" ]; then
    rsync -a --delete "$src/collabo-core-$a" "$dest/"
    # The engine compiles wasm at run time, so a signed app needs the JIT entitlements
    # (see readme.detail.md, "Flutter 통합"); the quarantine flag on a copied binary is cleared here.
    xattr -dr com.apple.quarantine "$dest/collabo-core-$a" 2>/dev/null || true
    found=1
  fi
done
if [ "$found" != 1 ]; then
  echo "error: collaboCore runtime not found in $src (run: python3 build.py runtime)"
  exit 1
fi
