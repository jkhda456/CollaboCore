#!/usr/bin/env bash
# Build CPython 3.13 for wasm32-linux (static, no fork/mmap) and pack it as python.cpio.
#
# The goal is the standard library as python.org ships it, minus what this platform cannot have
# (fork, dlopen, memory mappings, a display), with pip preinstalled as python.org does:
#   * every extension module whose C library ports: zlib, bz2, lzma, ssl/hashlib (OpenSSL), sqlite3,
#     readline (over libedit) and curses (ncurses) — python/build-deps.sh; the CA bundle as
#     /etc/ssl/cert.pem
#   * mmap as a copy-based module with the same interface (python/lib/mmap.py)
#   * pip, and the openai SDK and its dependencies preinstalled (python/packages.lock), including the Rust
#     extensions pydantic-core and jiter, compiled for the guest and linked in as built-in modules
#     (python/build-rust-toolchain.sh, python/build-native.sh)
#
# Mirrors distro/distro/python/package.nix, without Nix and without root: stock clang-19 via
# userspace/bin/wasm-cc (see docs/note.md) and a host CPython of the same minor version for the
# cross-build steps. Sources and packages are pinned by sha256 (sources.lock, packages.lock).
# Stages skip when their output exists; FORCE=stage[,stage] redoes them.
#   stages: host configure build stdlib packages cpio tests
#   (and those of build-deps.sh and build-native.sh: zlib bzip2 xz ncurses libedit sqlite openssl
#   pydantic-core jiter; FORCE_RUST=1 rebuilds the Rust std)
set -euo pipefail

P="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$P")"
U="$ROOT/userspace"
export PATH="$ROOT/.tools/cmake/bin:$U/tools:$PATH"
export PY_SRC="$P/src/Python-3.13.14"
SRC="$PY_SRC"
D="$P/deps"
HOSTPY="$P/host/bin/python3.13"
STAGE="$P/stage"
SITE="$STAGE/usr/lib/python3.13/site-packages"
LINK_FLAGS="-Wl,--import-memory -Wl,--max-memory=4294967296 -Wl,--shared-memory -Wl,--export-table -Wl,-z,stack-size=8388608"

# stage NAME OUTPUT [SOURCE...]: true when the stage must run (forced, output missing, or a
# source newer than the output).
stage() {
  local name=$1 out=$2; shift 2
  local src
  if [[ ",${FORCE:-}," != *",$name,"* && -e "$out" ]]; then
    for src in "$@"; do
      if [[ "$src" -nt "$out" ]]; then echo "== $name: $(basename "$src") changed"; return 0; fi
    done
    echo "== $name: up to date"; return 1
  fi
  echo "== $name"; return 0
}

# wasm-cc is configure's CC: it must be executable (see userspace/build.sh), and it needs the
# sysroot and compiler-rt that the userspace step builds.
chmod +x "$U/bin/wasm-cc"
[[ -f "$U/sysroot/lib/libc.a" && -f "$U/out/rt/lib/wasm32-unknown-unknown/libclang_rt.builtins.a" ]] \
  || { echo "missing the guest sysroot; build the userspace step first (python3 build.py userspace)" >&2; exit 1; }

bash "$P/build-deps.sh"            # sources + C libraries -> deps/
bash "$P/build-rust-toolchain.sh"  # Rust std for the guest -> .tools/rust-wasm
bash "$P/build-native.sh"          # pydantic-core, jiter -> deps/lib/lib{pydantic_core,jiter}.a

# The interpreter links Rust objects made by the nightly's LLVM, so it links with that LLVM's
# wasm-ld (LLD 22) rather than the system's LLD 19: same object format generation, and it has
# --allow-multiple-definition (below).
export WASM_LD="$ROOT/.tools/rust-wasm/bin/wasm-ld"

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

# The Rust extension modules join the interpreter's table of built-in modules, under the names
# their packages import them by. (makesetup's Setup files cannot name a dotted module.)
python3 - "$SRC/Modules/config.c.in" <<'PY'
import sys
path = sys.argv[1]
text = open(path).read()
if "PyInit__pydantic_core" not in text:
    text = text.replace("/* -- ADDMODULE MARKER 1 -- */",
        "/* -- ADDMODULE MARKER 1 -- */\nextern PyObject* PyInit__pydantic_core(void);\nextern PyObject* PyInit_jiter(void);", 1)
    text = text.replace("/* -- ADDMODULE MARKER 2 -- */",
        "/* -- ADDMODULE MARKER 2 -- */\n    {\"pydantic_core._pydantic_core\", PyInit__pydantic_core},\n    {\"jiter.jiter\", PyInit_jiter},", 1)
    open(path, "w").write(text)
PY
grep -q '"jiter.jiter"' "$SRC/Modules/config.c.in" || { echo "error: built-in module table not patched" >&2; exit 1; }

if stage host "$HOSTPY"; then bash "$P/build-host.sh"; fi

# Configure. Module switches and cache values follow distro's package.nix; each has a reason there.
# PKG_CONFIG=false: the host's pkg-config would report host (x86) libraries as usable for the wasm
# build, so each library is handed over by its *_CFLAGS/*_LIBS pair instead.
# Switched off, because the platform cannot have them: _posixsubprocess (fork), mmap (the C one;
# lib/mmap.py stands in), _posixshmem (shared memory), _ctypes (libffi has no port for this ABI),
# _tkinter (no display). Not built for want of their library: _dbm/_gdbm (dbm.sqlite3 and
# dbm.dumb work) and _uuid (the uuid module works without it).
# Each Rust archive carries its own copy of the Rust runtime from the same sysroot: identical
# compiler_builtins objects plus two symbols of std (rust_eh_personality, EMPTY_PANIC), so the
# link keeps the first definition of each (--allow-multiple-definition).
if stage configure "$P/build-wasm/Makefile" "$P/build.sh"; then
  rm -rf "$P/build-wasm"; mkdir -p "$P/build-wasm"; cd "$P/build-wasm"
  CONFIG_SITE="$P/config.site" \
  CC="$U/bin/wasm-cc" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 READELF=llvm-readelf-19 \
  OPT="-DNDEBUG -fwrapv -O2" \
  CFLAGS="-DHAVE_POSIX_SPAWN_FILE_ACTIONS_ADDCHDIR_NP=1" \
  LDFLAGS="$LINK_FLAGS -Wl,--allow-multiple-definition" \
  LIBS="-L$D/lib -lpydantic_core -ljiter" \
  ZLIB_CFLAGS="-I$D/include" ZLIB_LIBS="-L$D/lib -lz" \
  BZIP2_CFLAGS="-I$D/include" BZIP2_LIBS="-L$D/lib -lbz2" \
  LIBLZMA_CFLAGS="-I$D/include" LIBLZMA_LIBS="-L$D/lib -llzma" \
  LIBSQLITE3_CFLAGS="-I$D/include" LIBSQLITE3_LIBS="-L$D/lib -lsqlite3" \
  CURSES_CFLAGS="-I$D/include/ncursesw -I$D/include" CURSES_LIBS="-L$D/lib -lncursesw" \
  PANEL_CFLAGS="-I$D/include/ncursesw -I$D/include" PANEL_LIBS="-L$D/lib -lpanelw" \
  LIBEDIT_CFLAGS="-I$D/include" LIBEDIT_LIBS="-L$D/lib -ledit -lncursesw" \
  PKG_CONFIG=false \
  "$SRC/configure" \
    --host=wasm32-unknown-linux-musl --build=x86_64-pc-linux-gnu \
    --prefix=/usr --disable-shared \
    --with-build-python="$HOSTPY" \
    --with-ensurepip=no --disable-test-modules --without-mimalloc \
    --with-openssl="$D" --with-openssl-rpath=no --with-readline=editline \
    ac_cv_file__dev_ptmx=yes ac_cv_file__dev_ptc=no ac_cv_buggy_getaddrinfo=no \
    ac_cv_posix_semaphores_enabled=yes ac_cv_broken_sem_getvalue=no \
    py_cv_module__posixsubprocess=n/a py_cv_module_mmap=n/a py_cv_module__ctypes=n/a py_cv_module__posixshmem=n/a \
    py_cv_module__tkinter=n/a py_cv_module__dbm=n/a py_cv_module__gdbm=n/a py_cv_module__uuid=n/a \
    > "$P/configure.log" 2>&1 || { tail -30 "$P/configure.log"; exit 1; }
  cd "$P"
fi

if stage build "$P/build-wasm/python" "$D/lib/libpydantic_core.a" "$D/lib/libjiter.a" "$D/lib/libssl.a" "$D/lib/libedit.a"; then
  make -C "$P/build-wasm" -j"$(nproc)" > "$P/build.log" 2>&1 || { grep -E "error|Error" "$P/build.log" | head -20; tail -15 "$P/build.log"; exit 1; }
  # A module whose library did not link is only a warning in CPython's build; here it is an error.
  missing="$(grep -A3 "The necessary bits to build these optional modules were not found" "$P/build.log" | tail -n +2 || true)"
  failed="$(grep -E "^(Failed to build|Following modules built successfully but were removed)" -A3 "$P/build.log" || true)"
  [[ -z "$missing$failed" ]] || { echo "$missing$failed"; echo "error: extension modules missing" >&2; exit 1; }
fi

# Install into a staging root, then shrink it: the stdlib becomes one stored zip of
# sourceless .pyc (zipimport reads it directly, no zlib and no inflate needed).
if stage stdlib "$STAGE/usr/lib/python313.zip" "$P/build-wasm/python" "$P"/lib/*.py "$P/pack-stdlib.sh"; then
  rm -rf "$STAGE"
  make -C "$P/build-wasm" install DESTDIR="$STAGE" > "$P/install.log" 2>&1 || { tail -20 "$P/install.log"; exit 1; }
  bash "$P/pack-stdlib.sh"
fi

# site-packages: the openai SDK and its dependencies, installed as pip would (RECORD and all).
if stage packages "$SITE/.installed" "$STAGE/usr/lib/python313.zip" "$P/packages.lock" "$P/install-packages.py"; then
  rm -rf "$SITE" "$STAGE/scripts"; mkdir -p "$SITE"
  "$HOSTPY" "$P/install-packages.py" "$SITE" "$STAGE/scripts" "$P/src/wheels"
  touch "$SITE/.installed"
fi

if stage cpio "$P/out/python.cpio" "$SITE/.installed" "$STAGE/usr/lib/python313.zip" "$STAGE/usr/bin/python3.13" "$U/mkinitramfs.py"; then
  mkdir -p "$P/out" "$P/build-etc/ssl"
  cp "$P/src/$(awk '$1 == "cacert" {n = split($2, a, "/"); print a[n]}' "$P/sources.lock")" "$P/build-etc/ssl/cert.pem"
  python3 "$U/mkinitramfs.py" --overlay -o "$P/out/python.cpio" \
    --file usr/bin/python3.13="$STAGE/usr/bin/python3.13" \
    --link usr/bin/python3="python3.13" --link usr/bin/python="python3.13" \
    --file usr/bin/pydoc3.13="$STAGE/usr/bin/pydoc3.13" --link usr/bin/pydoc3="pydoc3.13" \
    --tree usr/bin="$STAGE/scripts" \
    --file usr/lib/python313.zip="$STAGE/usr/lib/python313.zip" \
    --dir usr/lib/python3.13/lib-dynload \
    --tree usr/lib/python3.13/venv="$STAGE/ondisk/venv" \
    --tree usr/lib/python3.13/ensurepip="$STAGE/ondisk/ensurepip" \
    --tree usr/lib/python3.13/site-packages="$SITE" \
    --tree usr/share/terminfo="$D/share/terminfo" \
    --tree etc/ssl="$P/build-etc/ssl" --link etc/ssl/certs/ca-certificates.crt=../cert.pem
fi
# Guest-side tests, as a small overlay archive (see tests/run.sh --boot).
if stage tests "$P/out/test.cpio" "$P"/tests/*.py; then
  python3 "$U/mkinitramfs.py" --overlay -o "$P/out/test.cpio" \
    $(for t in "$P"/tests/*.py; do echo "--file usr/share/tests/$(basename "$t")=$t"; done)
fi
ls -l "$P/out/python.cpio" "$STAGE/usr/bin/python3.13" "$STAGE/usr/lib/python313.zip"
