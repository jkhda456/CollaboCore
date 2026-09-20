#!/usr/bin/env bash
# Build CPython 3.13 for wasm32-linux (static, no fork/mmap) and pack it as python.cpio.
#
# Mirrors distro/distro/python/package.nix, without Nix and without root: stock clang-19 via
# userspace/bin/wasm-cc (see docs/note.md), a host CPython of the same minor version
# for the cross-build steps, and only zlib as a third-party library.
# Stages skip when their output exists; FORCE=stage[,stage] redoes them.
#   stages: host zlib configure build stdlib cpio tests
set -euo pipefail

P="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$P")"
U="$ROOT/userspace"
export PATH="$ROOT/.tools/cmake/bin:$U/tools:$PATH"
FORCE=",${FORCE:-},"
SRC="$P/src/Python-3.13.14"
HOSTPY="$P/host/bin/python3.13"
STAGE="$P/stage"
LINK_FLAGS="-Wl,--import-memory -Wl,--max-memory=4294967296 -Wl,--shared-memory -Wl,--export-table -Wl,-z,stack-size=8388608"

# stage NAME OUTPUT [SOURCE...]: true when the stage must run (forced, output missing, or a
# source newer than the output).
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

# distro's patch: run subprocess/multiprocessing through os.posix_spawn (there is no fork), plus
# musl's addchdir. GNU patch, not `git apply`: two hunks need fuzz 1, as with Nix's patchPhase.
# The check that matters is the marker afterwards: an unpatched build looks fine until the guest
# imports subprocess.
PATCH="$ROOT/third_party/distro/distro/python/process-posix-spawn.patch"
if ! grep -q _HAVE_FORK_EXEC "$SRC/Lib/subprocess.py"; then
  echo "== patch: process-posix-spawn"
  (cd "$SRC" && patch -p1 --forward --no-backup-if-mismatch < "$PATCH" > "$P/patch.log" 2>&1) || { cat "$P/patch.log"; exit 1; }
fi
grep -q _HAVE_FORK_EXEC "$SRC/Lib/subprocess.py" && grep -q POSIX_SPAWN_CHDIR "$SRC/Modules/posixmodule.c" \
  || { echo "error: the posix_spawn patch is not in the source tree" >&2; exit 1; }

if stage host "$HOSTPY"; then "$P/build-host.sh"; fi

# zlib: a handful of C files; its own configure is not worth fighting when cross-compiling.
if stage zlib "$P/zlib/lib/libz.a"; then
  Z="$P/src/zlib-1.3.1"; rm -rf "$P/build-zlib" "$P/zlib"; mkdir -p "$P/build-zlib" "$P/zlib/lib" "$P/zlib/include"
  (cd "$P/build-zlib" && for f in adler32 crc32 deflate infback inffast inflate inftrees trees zutil compress uncompr gzclose gzlib gzread gzwrite; do
     "$U/bin/wasm-cc" -Os -DZ_HAVE_UNISTD_H -I"$Z" -c "$Z/$f.c" -o "$f.o"
   done
   llvm-ar-19 rcs "$P/zlib/lib/libz.a" ./*.o)
  cp "$Z/zlib.h" "$Z/zconf.h" "$P/zlib/include/"
fi

# Configure. Module switches and cache values follow distro's package.nix; each has a reason there.
# PKG_CONFIG=false: the host's pkg-config would report host (x86) libraries such as xz as usable
# for the wasm build. The modules that need a library we do not ship are switched off by name.
if stage configure "$P/build-wasm/Makefile"; then
  mkdir -p "$P/build-wasm"; cd "$P/build-wasm"
  CONFIG_SITE="$P/config.site" \
  CC="$U/bin/wasm-cc" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 READELF=llvm-readelf-19 \
  OPT="-DNDEBUG -fwrapv -O2" \
  CFLAGS="-DHAVE_POSIX_SPAWN_FILE_ACTIONS_ADDCHDIR_NP=1" \
  LDFLAGS="$LINK_FLAGS" \
  ZLIB_CFLAGS="-I$P/zlib/include" ZLIB_LIBS="-L$P/zlib/lib -lz" \
  PKG_CONFIG=false \
  "$SRC/configure" \
    --host=wasm32-unknown-linux-musl --build=x86_64-pc-linux-gnu \
    --prefix=/usr --disable-shared \
    --with-build-python="$HOSTPY" \
    --with-ensurepip=no --disable-test-modules --without-mimalloc \
    ac_cv_file__dev_ptmx=yes ac_cv_file__dev_ptc=no ac_cv_buggy_getaddrinfo=no \
    ac_cv_posix_semaphores_enabled=yes ac_cv_broken_sem_getvalue=no \
    py_cv_module__posixsubprocess=n/a py_cv_module_mmap=n/a py_cv_module__ctypes=n/a py_cv_module__posixshmem=n/a \
    py_cv_module__lzma=n/a py_cv_module__bz2=n/a py_cv_module__sqlite3=n/a py_cv_module__ssl=n/a py_cv_module__hashlib=n/a \
    py_cv_module_readline=n/a py_cv_module__curses=n/a py_cv_module__curses_panel=n/a py_cv_module__dbm=n/a py_cv_module__gdbm=n/a \
    py_cv_module__tkinter=n/a py_cv_module__uuid=n/a \
    > "$P/configure.log" 2>&1 || { tail -30 "$P/configure.log"; exit 1; }
  cd "$P"
fi

if stage build "$P/build-wasm/python"; then
  make -C "$P/build-wasm" -j"$(nproc)" > "$P/build.log" 2>&1 || { grep -E "error|Error" "$P/build.log" | head -20; tail -15 "$P/build.log"; exit 1; }
fi

# Install into a staging root, then shrink it: the stdlib becomes one stored zip of
# sourceless .pyc (zipimport reads it directly, no zlib and no inflate needed).
if stage stdlib "$STAGE/usr/lib/python313.zip" "$P"/lib/*.py "$P/pack-stdlib.sh"; then
  rm -rf "$STAGE"
  make -C "$P/build-wasm" install DESTDIR="$STAGE" > "$P/install.log" 2>&1 || { tail -20 "$P/install.log"; exit 1; }
  "$P/pack-stdlib.sh"
fi

if stage cpio "$P/out/python.cpio" "$STAGE/usr/lib/python313.zip" "$STAGE/usr/bin/python3.13" "$U/mkinitramfs.py"; then
  mkdir -p "$P/out"
  python3 "$U/mkinitramfs.py" --overlay -o "$P/out/python.cpio" \
    --file usr/bin/python3.13="$STAGE/usr/bin/python3.13" \
    --link usr/bin/python3="python3.13" --link usr/bin/python="python3.13" \
    --file usr/lib/python313.zip="$STAGE/usr/lib/python313.zip" \
    --dir usr/lib/python3.13/lib-dynload --dir usr/lib/python3.13/site-packages
fi
# Guest-side tests, as a small overlay archive (see tests/run.sh --boot).
if stage tests "$P/out/test.cpio" "$P"/tests/*.py; then
  python3 "$U/mkinitramfs.py" --overlay -o "$P/out/test.cpio" \
    --file usr/share/tests/smoke.py="$P/tests/smoke.py" --file usr/share/tests/hostapi.py="$P/tests/hostapi.py"
fi
ls -l "$P/out/python.cpio" "$STAGE/usr/bin/python3.13" "$STAGE/usr/lib/python313.zip"
