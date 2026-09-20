#!/usr/bin/env bash
# Build-machine tools that the distro packages do not provide, installed into .tools/ without
# root: Node.js LTS (+ pnpm), CMake and Ninja, Binaryen (wasm-opt) and the Rust toolchain that
# builds the native engine. Downloads are checked against the publishers'
# checksums. System packages still needed (install once, needs sudo):
#
#   sudo apt-get install -y make flex bison bc pkg-config libncurses-dev device-tree-compiler \
#        wabt clang-19 lld-19 llvm-19 rsync python3
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
T="$ROOT/.tools"
NODE_VERSION="${NODE_VERSION:-v24.21.0}"
CMAKE_VERSION="${CMAKE_VERSION:-4.4.3}"
BINARYEN_VERSION="${BINARYEN_VERSION:-124}"
mkdir -p "$T"

# The two upstream trees this builds on. They keep their own git history, so they are cloned
# rather than committed here; the CI workflow does the same.
clone() { # directory url [branch]
  [[ -d "$ROOT/$1" ]] && return 0
  echo "== clone $1"
  git clone --depth 1 ${3:+-b "$3"} "$2" "$ROOT/$1"
}
clone kernel/linux https://github.com/tombl/linux.git wasm
clone third_party/distro https://github.com/tombl/distro.git

cd "$T"

if [[ ! -x node/bin/node ]]; then
  echo "== node $NODE_VERSION"
  f="node-$NODE_VERSION-linux-x64.tar.xz"
  curl -fsSL -O "https://nodejs.org/dist/$NODE_VERSION/$f"
  curl -fsSL "https://nodejs.org/dist/$NODE_VERSION/SHASUMS256.txt" | grep " $f\$" | sha256sum -c -
  tar -xJf "$f" && rm "$f" && ln -sfn "node-$NODE_VERSION-linux-x64" node
fi
export PATH="$T/node/bin:$PATH"
if ! command -v pnpm >/dev/null; then
  pm="$(node -p "require('$ROOT/third_party/distro/package.json').packageManager.split('@')[1]")"
  echo "== pnpm $pm"; npm install -g "pnpm@$pm" >/dev/null
fi

if [[ ! -x cmake/bin/cmake ]]; then
  echo "== cmake $CMAKE_VERSION"
  f="cmake-$CMAKE_VERSION-linux-x86_64.tar.gz"
  curl -fsSL -O "https://github.com/Kitware/CMake/releases/download/v$CMAKE_VERSION/$f"
  curl -fsSL "https://github.com/Kitware/CMake/releases/download/v$CMAKE_VERSION/cmake-$CMAKE_VERSION-SHA-256.txt" | grep " $f\$" | sha256sum -c -
  tar -xzf "$f" && rm "$f" && ln -sfn "cmake-$CMAKE_VERSION-linux-x86_64" cmake
fi
if [[ ! -x cmake/bin/ninja ]]; then
  echo "== ninja"
  curl -fsSL -o ninja.zip https://github.com/ninja-build/ninja/releases/latest/download/ninja-linux.zip
  python3 -c "import zipfile; zipfile.ZipFile('ninja.zip').extractall('cmake/bin')" && rm ninja.zip && chmod +x cmake/bin/ninja
fi
# wasm-opt rewrites clang-19's legacy exception instructions for the native engine; see
# userspace/bin/wasm-cc. Binaryen publishes no checksum file, so the tarball speaks for itself.
if [[ ! -x binaryen/bin/wasm-opt ]]; then
  echo "== binaryen $BINARYEN_VERSION"
  f="binaryen-version_$BINARYEN_VERSION-x86_64-linux.tar.gz"
  curl -fsSL -O "https://github.com/WebAssembly/binaryen/releases/download/version_$BINARYEN_VERSION/$f"
  tar -xzf "$f" && rm "$f" && ln -sfn "binaryen-version_$BINARYEN_VERSION" binaryen
fi

# Rust builds engine/ (the native runtime). rustup's installer verifies its own downloads.
if [[ ! -x cargo/bin/cargo ]]; then
  echo "== rust"
  curl -fsSL -o rustup-install.sh https://sh.rustup.rs
  RUSTUP_HOME="$T/rustup" CARGO_HOME="$T/cargo" sh rustup-install.sh -y --no-modify-path \
    --profile minimal --default-toolchain stable > rustup-install.log 2>&1
  rm rustup-install.sh
fi

# The Dart SDK, only for testing dart/collabo_core here (apps use Flutter's): DART=1.
if [[ -n "${DART:-}" && ! -x dart-sdk/bin/dart ]]; then
  v="$(curl -fsSL https://storage.googleapis.com/dart-archive/channels/stable/release/latest/VERSION | python3 -c "import json,sys; print(json.load(sys.stdin)['version'])")"
  echo "== dart $v"
  f=dartsdk-linux-x64-release.zip
  base="https://storage.googleapis.com/dart-archive/channels/stable/release/$v/sdk/$f"
  curl -fsSL -o "$f" "$base"
  echo "$(curl -fsSL "$base.sha256sum" | awk '{print $1}')  $f" | sha256sum -c -
  python3 -c "import zipfile; zipfile.ZipFile('$f').extractall('.')" && rm "$f"
  find dart-sdk/bin -type f -exec chmod +x {} +
fi
echo "tools: node $(node --version), pnpm $(pnpm --version), $(cmake/bin/cmake --version | head -1), ninja $(cmake/bin/ninja --version), $(binaryen/bin/wasm-opt --version), $(RUSTUP_HOME=$T/rustup cargo/bin/cargo --version)"
