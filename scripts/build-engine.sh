#!/usr/bin/env bash
# Collect everything the sandbox needs to boot into dist/engine, the common input of the web
# page (build-web.sh) and the desktop runtime (package-runtime.sh):
#
#   dist/engine/kernel/dist/*.js     host side of the kernel (workers, virtio devices, boot API)
#   dist/engine/kernel/bytes/*.js    its binary-struct helper (bare import @lowland/bytes)
#   dist/engine/kernel/vmlinux.wasm  the kernel (dist/index.js loads ../vmlinux.wasm, so siblings)
#   dist/engine/guest/*.js           host side of guest networking and the NodeFS disk backend
#   dist/engine/images/initramfs.cpio  busybox + /init + tools        (userspace/build.sh)
#   dist/engine/images/python.cpio     CPython 3.13 overlay, optional (python/build.sh)
#
# The kernel and guest JS come from third_party/distro with our patches (patches/*.patch).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$ROOT/.tools/node/bin:$PATH"
OUT="$ROOT/dist/engine"
WASM="$ROOT/kernel/linux/vmlinux.wasm"
DISTRO="$ROOT/third_party/distro"

[[ -f "$WASM" ]] || { echo "missing $WASM; run kernel/build.sh first" >&2; exit 1; }
[[ -f "$ROOT/userspace/out/initramfs.cpio" ]] || { echo "missing userspace/out/initramfs.cpio; run userspace/build.sh" >&2; exit 1; }
command -v pnpm >/dev/null || { echo "pnpm not found (expected in $ROOT/.tools/node/bin; see scripts/bootstrap-tools.sh)" >&2; exit 1; }

# Local patches to the distro clone (kept in ./patches, never edited silently in place).
# Idempotent: a patch that is already applied is skipped.
for patch in "$ROOT"/patches/*.patch; do
  if git -C "$DISTRO" apply --reverse --check "$patch" 2>/dev/null; then
    echo "== patch already applied: $(basename "$patch")"
  else
    echo "== apply patch: $(basename "$patch")"
    git -C "$DISTRO" apply "$patch"
  fi
done

echo "== build host JS (distro packages)"
(
  cd "$DISTRO"
  pnpm install --filter "@lowland/kernel..." --filter "@lowland/guest..." --frozen-lockfile >/dev/null
  pnpm --filter @lowland/bytes build >/dev/null
  pnpm --filter @lowland/kernel build >/dev/null
  pnpm --filter @lowland/guest build >/dev/null
)

echo "== assemble $OUT"
rm -rf "$OUT"
mkdir -p "$OUT/kernel/bytes" "$OUT/guest" "$OUT/images"
# Only runtime JS: drop type declarations and sourcemaps.
rsync -a --exclude='*.d.ts' --exclude='*.map' "$DISTRO/packages/kernel/dist/" "$OUT/kernel/dist/"
rsync -a --exclude='*.d.ts' --exclude='*.map' "$DISTRO/packages/bytes/dist/" "$OUT/kernel/bytes/"
rsync -a --exclude='*.d.ts' --exclude='*.map' "$DISTRO/packages/linux-guest/dist/" "$OUT/guest/"
cp "$WASM" "$OUT/kernel/vmlinux.wasm"
cp "$ROOT/userspace/out/initramfs.cpio" "$OUT/images/initramfs.cpio"
if [[ -f "$ROOT/python/out/python.cpio" ]]; then
  cp "$ROOT/python/out/python.cpio" "$OUT/images/python.cpio"
else
  echo "note: python/out/python.cpio missing; the sandbox will have no python3 (run python/build.sh)" >&2
fi

# License texts travel with the redistributed binaries.
mkdir -p "$OUT/licenses"
cp "$DISTRO/packages/kernel/LICENSE" "$OUT/licenses/kernel-js.MIT.txt"
cp "$ROOT/kernel/linux/COPYING" "$OUT/licenses/linux.GPL-2.0.txt"
cp "$ROOT/python/src/Python-3.13.14/LICENSE" "$OUT/licenses/python.PSF.txt" 2>/dev/null || true
cp "$ROOT/userspace/src/busybox/LICENSE" "$OUT/licenses/busybox.GPL-2.0.txt" 2>/dev/null || true
cp "$ROOT/userspace/src/musl/COPYRIGHT" "$OUT/licenses/musl.MIT.txt" 2>/dev/null || true

echo "== engine ready"
du -sh "$OUT"
