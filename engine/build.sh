#!/usr/bin/env bash
# Build the native engine (engine/target/release/collabo-core-engine).
#
# Uses the Rust toolchain from .tools (scripts/bootstrap-tools.sh) when this machine has one,
# else whatever cargo is installed. Arguments go to cargo; --tests runs the unit tests instead.
set -euo pipefail

E="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$E")"
if [[ -x "$ROOT/.tools/cargo/bin/cargo" ]]; then
  export RUSTUP_HOME="$ROOT/.tools/rustup" CARGO_HOME="$ROOT/.tools/cargo"
  export PATH="$ROOT/.tools/rustup/toolchains/stable-$(uname -m)-unknown-linux-gnu/bin:$PATH"
fi

command -v cargo >/dev/null || { echo "no cargo; run scripts/bootstrap-tools.sh" >&2; exit 1; }
cd "$E"
if [[ "${1:-}" == "--tests" ]]; then
  exec cargo test --release
fi
cargo build --release "$@"
ls -l "$E/target/release/collabo-core-engine"
