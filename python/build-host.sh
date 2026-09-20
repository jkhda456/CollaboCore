#!/usr/bin/env bash
# Native CPython 3.13 with clang-19: the interpreter that cross-compiling the wasm one needs
# (--with-build-python must be the same minor version), and that byte-compiles the stdlib.
set -euo pipefail
P="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$P/build-host"
CC=clang-19 "$P/src/Python-3.13.14/configure" --prefix="$P/host" --with-ensurepip=no --disable-test-modules >/dev/null
make -j"$(nproc)" >/dev/null
make install >/dev/null
"$P/host/bin/python3.13" -c "import sys; print('host python', sys.version)"
