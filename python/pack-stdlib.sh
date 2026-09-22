#!/usr/bin/env bash
# Turn the installed stdlib into /usr/lib/python313.zip: sourceless .pyc, stored (no compression).
# CPython imports straight from a zip on sys.path (zipimport), so the guest needs no unzip step
# and no inflate code, and the single file loads far faster than thousands of small ones.
set -euo pipefail
P="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STAGE="$P/stage"; HOSTPY="$P/host/bin/python3.13"
LIB="$STAGE/usr/lib/python3.13"
BUILD_ROOT="$(cd "$P/.." && pwd)"   # collaboCore: nothing under it may leak into the guest image

# Dropped: what needs a display (tkinter, idlelib, turtle), the test suite, and the static-library
# build kit (config-3.13-*: no compiler in the guest). Everything else of the stdlib stays,
# ensurepip included, so `python3 -m venv` gives a venv its own pip as on python.org.
( cd "$LIB" && rm -rf test idlelib tkinter turtledemo turtle.py config-3.13-* site-packages/* )
find "$LIB" -name __pycache__ -prune -exec rm -rf {} +
find "$LIB" -name '*.pyc' -delete

# Sandbox-specific modules (the client for the host request bridge) sit next to the stdlib.
cp "$P"/lib/*.py "$LIB/"

# The sysconfig snapshot records the build tree (Makefile variables, dependency lists). The guest
# has no compiler, so none of those paths mean anything there: map them to neutral ones, so the
# archive does not carry the layout of the machine it was built on.
for f in "$LIB"/_sysconfigdata_*.py; do
  sed -i -e "s#$HOSTPY#/usr/bin/python3#g" \
         -e "s#$BUILD_ROOT/userspace/bin/wasm-cc#cc#g" \
         -e "s#llvm-\(ar\|ranlib\|readelf\)-19#\1#g" \
         -e "s# *-[IL]$P/zlib[^ '\"]*##g" \
         -e "s#${PY_SRC:-$P/src/Python-3.13.14}#/usr/src/python#g" \
         -e "s#$P/build-wasm#/usr/src/python/build#g" \
         -e "s#$P/host/bin/python3.13#/usr/bin/python3#g" "$f"
  if grep -q "$BUILD_ROOT" "$f"; then echo "warning: build paths remain in $(basename "$f")" >&2; grep -o "[^ '\"]*$BUILD_ROOT[^ '\"]*" "$f" | sort -u | head -5 >&2; fi
done

"$HOSTPY" -m compileall -b -q -f -d /usr/lib/python3.13 --invalidation-mode unchecked-hash -o 0 "$LIB"

# Packages that read their data files through plain paths (venv's activate scripts) or need them
# at all (ensurepip's pip wheel) stay out of the zip, installed as ordinary files instead:
# stage/ondisk/<package>, which python/build.sh puts in /usr/lib/python3.13.
ON_DISK="venv ensurepip"
rm -rf "$STAGE/ondisk"; mkdir -p "$STAGE/ondisk"
for pkg in $ON_DISK; do
  cp -r "$LIB/$pkg" "$STAGE/ondisk/$pkg"
  find "$STAGE/ondisk/$pkg" -name '*.py' -delete
done

"$HOSTPY" - "$LIB" "$STAGE/usr/lib/python313.zip" $ON_DISK <<'PY'
import os, sys, zipfile
lib, out, *on_disk = sys.argv[1:]
names = sorted(os.path.relpath(os.path.join(d, f), lib) for d, _, fs in os.walk(lib) for f in fs if f.endswith(".pyc"))
names = [n for n in names if n.split(os.sep)[0] not in on_disk]
with zipfile.ZipFile(out, "w", zipfile.ZIP_STORED) as z:
    for n in names:
        info = zipfile.ZipInfo(n, (2026, 1, 1, 0, 0, 0))   # fixed time: the archive is reproducible
        info.external_attr = 0o644 << 16
        z.writestr(info, open(os.path.join(lib, n), "rb").read())
print(f"{out}: {len(names)} modules, {os.path.getsize(out) / 1048576:.1f} MiB")
PY
llvm-strip-19 --strip-debug "$STAGE/usr/bin/python3.13"
