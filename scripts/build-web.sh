#!/usr/bin/env bash
# The browser flavor: a static site in dist/web built from dist/engine (build-engine.sh), the
# portable host modules in host/ and the page in web/. Serve it with web/serve.py (it needs the
# COOP/COEP headers for SharedArrayBuffer) or any server that sends the headers in _headers.
#
#   dist/web/index.html, _headers, serve.py, serve.mjs     from web/
#   dist/web/app/*.js                                     host/*.js + the page's panels
#   dist/web/static/{kernel,guest}                        from dist/engine
#   dist/web/static/{initramfs,python,tools}.cpio         from dist/engine/images
#   dist/web/vendor/xterm                                 terminal widget (third_party/distro)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENGINE="$ROOT/dist/engine"
OUT="$ROOT/dist/web"
[[ -f "$ENGINE/kernel/vmlinux.wasm" ]] || { echo "missing dist/engine; run: python3 build.py engine" >&2; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT/app" "$OUT/static" "$OUT/vendor"
cp "$ROOT/web/index.html" "$ROOT/web/_headers" "$ROOT/web/serve.py" "$ROOT/web/serve.mjs" "$OUT/"
cp "$ROOT"/host/*.js "$ROOT"/web/*.js "$OUT/app/"
cp -r "$ENGINE/kernel" "$ENGINE/guest" "$ENGINE/licenses" "$OUT/static/"
cp "$ENGINE"/images/*.cpio "$OUT/static/"
cp -r "$ROOT/third_party/distro/apps/site/vendor/xterm" "$OUT/vendor/xterm"

echo "== web ready: $OUT"
(cd "$OUT" && find . -type f -not -path './vendor/*' -printf '%9s  %p\n' | sort -k2 | grep -v "static/guest/\|static/kernel/dist/")
du -sh "$OUT"
