#!/usr/bin/env bash
# Checks for the guest's display (gui, libcollabo-gui, gui-demo):
#
#   tests/run.sh          unit tests; the scripted session (tests/test_gui.py) with a native
#                         build; then the same session in the guest (dist/runtime)
#   tests/run.sh native   the native half only (no runtime needed)
#
# The guest half boots the runtime's tools.cpio, or GUI_TOOLS_IMAGE (e.g. tools/out/tools.cpio,
# to try a build before it is packaged).
set -euo pipefail
T="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
G="$(dirname "$T")"
ROOT="$(cd "$G/../.." && pwd)"
export RUSTUP_HOME="$ROOT/.tools/rustup" CARGO_HOME="$ROOT/.tools/cargo"
export PATH="$CARGO_HOME/bin:$PATH"
HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
export "CARGO_TARGET_$(echo "$HOST_TRIPLE" | tr a-z- A-Z_)_LINKER=clang-19"
CC="${CC:-clang-19}"
H="$ROOT/tools/build/gui/host"

echo "== unit tests"
cargo test --quiet --manifest-path "$G/rust/Cargo.toml" --target-dir "$H" 2>&1 | grep -E "^test result|FAILED|panicked"
echo "== native build"
cargo build --quiet --manifest-path "$G/rust/Cargo.toml" --target-dir "$H"
mkdir -p "$H"
"$CC" -std=c99 -O2 -Wall -Wextra -Wno-unused-parameter -Werror -c "$G/client/collabo_gui.c" -o "$H/collabo_gui.o"
rm -f "$H/libcollabo-gui.a"
llvm-ar-19 rcs "$H/libcollabo-gui.a" "$H/collabo_gui.o"
"$CC" -std=c99 -O2 -Wall -Wextra -Werror -I"$G/client" "$G/client/gui-demo.c" "$H/libcollabo-gui.a" -o "$H/gui-demo"
# The header is C++ too (a port such as WPE is).
echo '#include "collabo_gui.h"
int main() { cg_event e = {}; return e.type; }' > "$H/cxx.cc"
"${CXX:-clang++-19}" -std=c++17 -Wall -Werror -I"$G/client" -c "$H/cxx.cc" -o "$H/cxx.o"
echo "== native session"
python3 "$T/test_gui.py" native

[[ "${1:-}" == native ]] && exit 0
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) platform=linux-x64 ;; Linux-aarch64) platform=linux-arm64 ;;
  Darwin-arm64) platform=darwin-arm64 ;; Darwin-x86_64) platform=darwin-x64 ;;
esac
RT="${COLLABO_RUNTIME:-$ROOT/dist/runtime/collabo-core-$platform}"
[[ -n "${GUI_TOOLS_IMAGE:-}" || -f "$RT/app/images/tools.cpio" ]] \
  || { echo "no runtime with the tools image; run: python3 build.py tools-image engine runtime" >&2; exit 1; }
echo "== the session in the guest"
python3 "$T/test_gui.py" guest "$RT"
