#!/usr/bin/env bash
# Build the guest userspace (musl + busybox + /init) and pack it into out/initramfs.cpio.
#
# Everything is built without Nix and without root, with stock clang-19 standing in for
# tombl's LLVM 22 fork; see bin/wasm-cc and docs/note.md for why.
# Stages are skipped when their output already exists; FORCE=stage[,stage] redoes them.
#
#   bash build.sh                    # all stages
#   FORCE=musl,sysroot,rt bash build.sh
#   stages: src kheaders musl sysroot rt busybox init hfetch collabo-agentd hostcall collabo-sshagent cpio
set -euo pipefail

U="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$U")"
export PATH="$ROOT/.tools/cmake/bin:$U/tools:$PATH"
LINUX="$ROOT/kernel/linux"

# wasm-cc is make's CC and tools/bzip2 is found on PATH, so both are run by other programs and
# need their executable bit, which a checkout made on Windows or through a shared folder loses.
chmod +x "$U/bin/wasm-cc" "$U/tools/bzip2"

# musl's and busybox's build systems call the LLVM tools by their unversioned names, which
# Ubuntu does not install. Point them at the versioned ones here rather than checking in
# symlinks to this machine's paths.
for tool in ar nm objcopy objdump ranlib readelf strip; do
  [[ -e "$U/tools/llvm-$tool" ]] || ln -sf "$(command -v "llvm-$tool-19" || echo /usr/bin/llvm-$tool-19)" "$U/tools/llvm-$tool"
done
FORCE=",${FORCE:-},"

# Pinned to the commits distro/distro/{musl,busybox,llvm-runtimes}/package.nix build.
MUSL_REV=637b0d25dafa7e4740357f25fb0b5e3949f1ed1f
BUSYBOX_REV=4ad91c87e7efeff15628bea7240b6055b2737644
LLVM_REV=9aaceb42fef4f924a00126e0d66140d01482921c

LINK_FLAGS=(-Wl,--import-memory -Wl,--max-memory=4294967296 -Wl,--shared-memory -Wl,--export-table -Wl,-z,stack-size=8388608)

# stage NAME OUTPUT [SOURCE...]: true when the stage must run, i.e. FORCE names it, the
# output is missing, or a source file is newer than the output.
stage() {
  local name=$1 out=$2; shift 2
  local src
  if [[ "$FORCE" != *",$name,"* && -e "$out" ]]; then
    for src in "$@"; do
      if [[ "$src" -nt "$out" ]]; then echo "== $name: $(basename "$src") changed"; return 0; fi
    done
    echo "== $name: up to date"; return 1
  fi
  echo "== $name"; return 0
}

fetch() { # dir url rev [sparse dirs...]
  local dir=$1 url=$2 rev=$3; shift 3
  [[ -d "$dir/.git" ]] || { git init -q "$dir"; git -C "$dir" remote add origin "$url"; }
  if (($#)); then git -C "$dir" sparse-checkout set --cone "$@" >/dev/null; fi
  git -C "$dir" fetch -q --depth 1 --filter=blob:none origin "$rev"
  git -C "$dir" checkout -q FETCH_HEAD
}

mkdir -p "$U/src" "$U/build" "$U/out"

if stage src "$U/src/llvm/compiler-rt/lib/builtins/CMakeLists.txt"; then
  fetch "$U/src/musl"    https://github.com/tombl/musl.git         "$MUSL_REV"
  fetch "$U/src/busybox" https://github.com/tombl/busybox.git      "$BUSYBOX_REV"
  fetch "$U/src/llvm"    https://github.com/tombl/llvm-project.git "$LLVM_REV" compiler-rt cmake llvm/cmake runtimes
fi

# Kernel uapi headers, from the kernel tree we built (same ARCH=wasm ABI the kernel exports).
if stage kheaders "$U/kheaders/include/linux"; then
  make -C "$LINUX" LLVM=-19 headers_install INSTALL_HDR_PATH="$U/kheaders" >/dev/null
fi

# musl: arch/wasm32/arch.mak already adds --target=wasm32 -matomics -mbulk-memory.
if stage musl "$U/out/musl/lib/libc.a"; then
  cd "$U/src/musl"
  printf 'ARCH=wasm32\nprefix=%s\nsyslibdir=%s\nCFLAGS=\n' "$U/out/musl" "$U/out/musl" > config.mak
  MK=(make CC=clang-19 AR=llvm-ar-19 RANLIB=llvm-ranlib-19 "-j$(nproc)")
  "${MK[@]}" clean >/dev/null
  "${MK[@]}" >/dev/null
  "${MK[@]}" install-libs install-headers >/dev/null
  cd "$U"
fi

# sysroot = kernel headers + musl headers/libs (musl's win on overlap, as in sysroot-base).
if stage sysroot "$U/sysroot/lib/libc.a"; then
  rm -rf "$U/sysroot"; mkdir -p "$U/sysroot/lib" "$U/sysroot/include"
  cp -r "$U/kheaders/include/." "$U/sysroot/include/"; chmod -R u+w "$U/sysroot/include"
  cp -r "$U/out/musl/include/." "$U/sysroot/include/"
  cp "$U/out/musl/lib/"*.a "$U/out/musl/lib/"*.o "$U/sysroot/lib/"
fi

# compiler-rt builtins, built with atomics+bulk-memory: wasm-ld refuses --shared-memory
# if any input object lacks those features, so distro packages cannot be reused.
# The triple must have dashes for compiler-rt's cmake, and stock clang accepts only
# wasm-format triples, hence wasm32-unknown-unknown (linux macros are added by -D).
RT_LIB="$U/out/rt/lib/wasm32-unknown-unknown/libclang_rt.builtins.a"
if stage rt "$RT_LIB"; then
  T=wasm32-unknown-unknown
  FL="--sysroot=$U/sysroot -matomics -mbulk-memory -mllvm -wasm-enable-sjlj -D__linux__=1 -D__linux=1 -D__unix__=1 -D__gnu_linux__=1"
  rm -rf "$U/build/rt"
  cmake -Wno-dev -S "$U/src/llvm/runtimes" -B "$U/build/rt" -G Ninja -DCMAKE_INSTALL_PREFIX="$U/out/rt" \
    -DCMAKE_BUILD_TYPE=Release -DCMAKE_SYSTEM_NAME=Linux -DCMAKE_SYSROOT="$U/sysroot" \
    -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
    -DCMAKE_C_COMPILER=/usr/bin/clang-19 -DCMAKE_CXX_COMPILER=/usr/bin/clang++-19 -DCMAKE_ASM_COMPILER=/usr/bin/clang-19 \
    -DCMAKE_AR=/usr/bin/llvm-ar-19 -DCMAKE_RANLIB=/usr/bin/llvm-ranlib-19 \
    -DCMAKE_C_COMPILER_TARGET=$T -DCMAKE_CXX_COMPILER_TARGET=$T -DCMAKE_ASM_COMPILER_TARGET=$T \
    "-DCMAKE_C_FLAGS=$FL" "-DCMAKE_CXX_FLAGS=$FL" -DCMAKE_ASM_FLAGS=-mexception-handling \
    -DLLVM_ENABLE_RUNTIMES=compiler-rt -DLLVM_DEFAULT_TARGET_TRIPLE=$T \
    -DLLVM_ENABLE_PER_TARGET_RUNTIME_DIR=ON -DLLVM_INCLUDE_TESTS=OFF -DLLVM_INCLUDE_DOCS=OFF \
    -DCOMPILER_RT_BUILD_CTX_PROFILE=OFF -DCOMPILER_RT_BUILD_CRT=OFF -DCOMPILER_RT_DEFAULT_TARGET_ONLY=ON \
    -DCOMPILER_RT_BUILD_LIBFUZZER=OFF -DCOMPILER_RT_BUILD_MEMPROF=OFF -DCOMPILER_RT_BUILD_ORC=OFF \
    -DCOMPILER_RT_BUILD_PROFILE=OFF -DCOMPILER_RT_BUILD_SANITIZERS=OFF -DCOMPILER_RT_BUILD_XRAY=OFF >/dev/null
  cmake --build "$U/build/rt" --target install >/dev/null
fi

if stage busybox "$U/out/busybox/bin/busybox"; then
  bash "$U/build-busybox.sh"
fi

# The kernel rejects modules that V8 refuses to compile, and the host swallows the reason
# (worker.ts compile_end -> ENOEXEC). Validate here so a bad link fails loudly.
validate() {
  "$ROOT/.tools/node/bin/node" -e '
    const fs=require("fs");
    for (const f of process.argv.slice(1)) {
      try { new WebAssembly.Module(fs.readFileSync(f)); console.log("valid wasm:", f); }
      catch (e) { console.error("INVALID wasm:", f, "-", e.message); process.exit(1); }
    }' "$@"
}

if stage init "$U/out/init.wasm" "$U/rootfs/init.c" "$U/bin/wasm-cc"; then
  "$U/bin/wasm-cc" -Os -Wall -o "$U/out/init.wasm" "$U/rootfs/init.c" "${LINK_FLAGS[@]}"
fi

# hfetch: the guest client of the host's HTTP request API (see release/app/http-protocol.js).
if stage hfetch "$U/out/hfetch.wasm" "$U/rootfs/hfetch.c" "$U/bin/wasm-cc"; then
  "$U/bin/wasm-cc" -Os -Wall -Wextra -o "$U/out/hfetch.wasm" "$U/rootfs/hfetch.c" "${LINK_FLAGS[@]}"
fi

# collabo-agentd runs commands for the host; hostcall reaches host functions; collabo-sshagent
# carries SSH_AUTH_SOCK to the host's ssh-agent (see those files).
for tool in collabo-agentd hostcall collabo-sshagent; do
  if stage "$tool" "$U/out/$tool.wasm" "$U/rootfs/$tool.c" "$U/bin/wasm-cc"; then
    "$U/bin/wasm-cc" -Os -Wall -Wextra -o "$U/out/$tool.wasm" "$U/rootfs/$tool.c" "${LINK_FLAGS[@]}"
  fi
done

validate "$U/out/init.wasm" "$U/out/hfetch.wasm" "$U/out/collabo-agentd.wasm" "$U/out/hostcall.wasm" \
  "$U/out/collabo-sshagent.wasm" "$U/out/busybox/bin/busybox"

# Always repacked: cheap, and picks up rootfs/etc edits.
echo "== cpio"
python3 "$U/mkinitramfs.py" --init "$U/out/init.wasm" --busybox "$U/out/busybox" \
  --file usr/bin/hfetch="$U/out/hfetch.wasm" \
  --file usr/bin/hostcall="$U/out/hostcall.wasm" \
  --file usr/sbin/collabo-agentd="$U/out/collabo-agentd.wasm" \
  --file usr/sbin/collabo-sshagent="$U/out/collabo-sshagent.wasm" \
  --etc "$U/rootfs/etc" -o "$U/out/initramfs.cpio"
