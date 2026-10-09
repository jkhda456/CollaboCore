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
#   dist/engine/images/tools.cpio      curl, ssh, git, screen, gui overlay, optional (tools/build.sh)
#   dist/engine/images/addons/<name>.cpio + <name>.json   the bundled add-ons (addons/build.sh)
#
#   ADDONS="claude-code codex" scripts/build-engine.sh   bundle these add-ons instead of the ones
#                                                       whose addon.json says "bundled": true
#
# The kernel and guest JS come from third_party/distro with our patches (patches/*.patch).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$ROOT/.tools/node/bin:$PATH"
OUT="$ROOT/dist/engine"
WASM="$ROOT/kernel/linux/vmlinux.wasm"
DISTRO="$ROOT/third_party/distro"

[[ -f "$WASM" ]] || { echo "missing $WASM; run: python3 build.py kernel" >&2; exit 1; }
[[ -f "$ROOT/userspace/out/initramfs.cpio" ]] || { echo "missing userspace/out/initramfs.cpio; run: python3 build.py userspace" >&2; exit 1; }
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
  echo "note: python/out/python.cpio missing; the sandbox will have no python3 (python3 build.py python)" >&2
fi
if [[ -f "$ROOT/tools/out/tools.cpio" ]]; then
  cp "$ROOT/tools/out/tools.cpio" "$OUT/images/tools.cpio"
else
  echo "note: tools/out/tools.cpio missing; the sandbox will have no curl/ssh/git (python3 build.py tools-image)" >&2
fi

# The add-ons that ship (addons/README.md): the ones ADDONS names, else those whose addon.json says
# "bundled": true. The rest stay in addons/<name>/out, for an app that adds them itself.
if [[ -n "${ADDONS+set}" ]]; then
  bundled="$ADDONS"
else
  bundled=""
  for json in "$ROOT"/addons/*/addon.json; do
    [[ -f "$json" ]] || continue
    python3 -c 'import json, sys; sys.exit(json.load(open(sys.argv[1])).get("bundled") is not True)' "$json" \
      && bundled+=" $(basename "$(dirname "$json")")"
  done
fi
shipped=" "
for name in $bundled; do
  cpio="$ROOT/addons/$name/out/$name.cpio"
  if [[ ! -f "$cpio" ]]; then
    [[ -z "${ADDONS+set}" ]] || { echo "missing addons/$name/out/$name.cpio; run: addons/build.sh $name" >&2; exit 1; }
    echo "note: addons/$name/out/$name.cpio missing; the runtime ships without it (addons/build.sh $name)" >&2
    continue
  fi
  mkdir -p "$OUT/images/addons"
  cp "$cpio" "$OUT/images/addons/"
  [[ -f "$ROOT/addons/$name/addon.json" ]] && cp "$ROOT/addons/$name/addon.json" "$OUT/images/addons/$name.json"
  shipped+="$name "
  echo "== addon $name"
done
for cpio in "$ROOT"/addons/*/out/*.cpio; do
  [[ -f "$cpio" ]] || continue
  name="$(basename "$cpio" .cpio)"
  [[ "$shipped" == *" $name "* ]] || echo "== addon $name: not bundled, stays in addons/$name/out"
done

# License texts travel with the redistributed binaries.
mkdir -p "$OUT/licenses"
cp "$DISTRO/packages/kernel/LICENSE" "$OUT/licenses/kernel-js.MIT.txt"
cp "$ROOT/kernel/linux/COPYING" "$OUT/licenses/linux.GPL-2.0.txt"
cp "$ROOT/python/src/Python-3.13.14/LICENSE" "$OUT/licenses/python.PSF.txt" 2>/dev/null || true
cp "$ROOT/userspace/src/busybox/LICENSE" "$OUT/licenses/busybox.GPL-2.0.txt" 2>/dev/null || true
cp "$ROOT/userspace/src/musl/COPYRIGHT" "$OUT/licenses/musl.MIT.txt" 2>/dev/null || true
# What python.cpio links in (python/build-deps.sh, build-native.sh). The Python packages in
# site-packages carry their own license files in their .dist-info directories.
if [[ -f "$ROOT/python/out/python.cpio" ]]; then
  B="$ROOT/python/build-deps" N="$ROOT/python/build-native"
  while read -r name file; do
    ext="${file##*.}"; [[ "$ext" == html ]] || ext=txt
    cp "$file" "$OUT/licenses/$name.$ext" 2>/dev/null || echo "note: no license text for $name ($file)" >&2
  done <<LICENSES
zlib.Zlib               $B/zlib-1.3.1/LICENSE
bzip2.bzip2-1.0.6       $B/bzip2-1.0.8/LICENSE
xz-liblzma.0BSD         $B/xz-5.6.4/COPYING.0BSD
ncurses.X11             $B/ncurses-6.6/COPYING
libedit.BSD-3-Clause    $B/libedit-20260512-3.1/COPYING
openssl.Apache-2.0      $B/openssl-3.5.7/LICENSE.txt
pydantic-core.MIT       $N/pydantic_core-2.46.5/LICENSE
jiter.MIT               $N/jiter-0.17.0/LICENSE
rust-std.MIT-Apache-2.0 $ROOT/.tools/rust-wasm/COPYRIGHT-library.html
LICENSES
  echo "SQLite is in the public domain: https://sqlite.org/copyright.html" > "$OUT/licenses/sqlite.public-domain.txt"
  printf '%s\n' "The CA bundle (/etc/ssl/cert.pem) is Mozilla's, under the Mozilla Public License 2.0:" \
    "https://www.mozilla.org/MPL/2.0/ (as extracted by curl.se: https://curl.se/docs/caextract.html)" \
    > "$OUT/licenses/ca-certificates.MPL-2.0.txt"
fi

# What tools.cpio carries (tools/build.sh); OpenSSL and zlib are the ones listed above.
if [[ -f "$ROOT/tools/out/tools.cpio" ]]; then
  B="$ROOT/tools/build"
  cp "$B/curl-8.21.0/COPYING" "$OUT/licenses/curl.curl.txt"
  cp "$B/dropbear-2026.92/LICENSE" "$OUT/licenses/dropbear.MIT.txt"
  cp "$B/git-2.55.0/COPYING" "$OUT/licenses/git.GPL-2.0.txt"
  cp "$B/screen-5.0.2/COPYING" "$OUT/licenses/screen.GPL-3.0.txt"
  for file in "$B"/gui/licenses/*; do [[ -f "$file" ]] && cp "$file" "$OUT/licenses/"; done
  # The openssl command is OpenSSL's (listed above); the Rust tools list their crates.
  for file in "$B"/rust/licenses/*; do [[ -f "$file" ]] && cp "$file" "$OUT/licenses/"; done
fi

# What each bundled add-on carries, as its build.sh left it in out/licenses.
for name in $shipped; do
  licenses="$ROOT/addons/$name/out/licenses"
  [[ -d "$licenses" ]] || continue
  for file in "$licenses"/*; do cp "$file" "$OUT/licenses/addon-$name-$(basename "$file")"; done
done

echo "== engine ready"
du -sh "$OUT"
