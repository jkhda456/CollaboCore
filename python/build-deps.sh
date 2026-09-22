#!/usr/bin/env bash
# Fetch every source in sources.lock, then build the C libraries behind the stdlib's extension
# modules for wasm32-linux, static, into python/deps (include/, lib/, share/terminfo):
#
#   zlib      zlib, binascii.crc32          ncurses   _curses, _curses_panel (+ readline's termcap)
#   bzip2     _bz2                          libedit   readline (the REPL's line editing; BSD, as
#                                                     python.org's macOS builds, not GPL-3 readline)
#   xz        _lzma                         sqlite    _sqlite3
#   openssl   _ssl, _hashlib                cacert    /etc/ssl/cert.pem (the default trust store)
#
# Flags follow third_party/distro/distro/<name>/package.nix; the reasons are kept here in short.
# Stages skip when their library exists; FORCE=name[,name] redoes them (python/build.sh passes it on).
set -euo pipefail

P="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$P")"
U="$ROOT/userspace"
DISTRO="$ROOT/third_party/distro/distro"
D="$P/deps"
B="$P/build-deps"
FORCE=",${FORCE:-},"
CC="$U/bin/wasm-cc"
export PATH="$ROOT/.tools/cmake/bin:$U/tools:$PATH"
JOBS="$(nproc)"
HOST=(--host=wasm32-unknown-linux-musl --build=x86_64-pc-linux-gnu)
chmod +x "$CC"

stage() { # name output: true when the stage must run
  if [[ "$FORCE" != *",$1,"* && -e "$2" ]]; then echo "== $1: up to date"; return 1; fi
  echo "== $1"; return 0
}

# fetch NAME: download (once) and check the source that sources.lock names; prints its path.
fetch() {
  local url sum file
  read -r _ url sum < <(grep -E "^$1 " "$P/sources.lock")
  [[ -n "$url" ]] || { echo "$1 is not in sources.lock" >&2; exit 1; }
  file="$P/src/$(basename "$url")"
  mkdir -p "$P/src"
  if [[ ! -f "$file" ]]; then
    echo "== fetch $(basename "$url")" >&2
    curl -fsSL --retry 3 -o "$file.part" "$url" && mv "$file.part" "$file"
  fi
  echo "$sum  $file" | sha256sum -c --quiet - >&2 || { echo "checksum mismatch: $file (deleted)" >&2; rm -f "$file"; exit 1; }
  echo "$file"
}

# unpack NAME: a fresh copy of the source under build-deps/, so patches and in-tree builds never
# touch the downloaded tarball's tree; prints the directory.
unpack() {
  local file dir
  file="$(fetch "$1")"
  dir="$B/$(tar -tf "$file" | head -1 | cut -d/ -f1)"
  rm -rf "$dir"; mkdir -p "$B"
  tar -xf "$file" -C "$B"
  echo "$dir"
}

log() { "$@" > "$B/$STEP.log" 2>&1 || { tail -30 "$B/$STEP.log"; echo "FAILED: $STEP (log: $B/$STEP.log)" >&2; exit 1; }; }

mkdir -p "$D/lib" "$D/include" "$B"

# The interpreter's own source is unpacked in place (python/build.sh patches and builds it there).
if [[ ! -d "$P/src/Python-3.13.14" ]]; then
  tar -xf "$(fetch python)" -C "$P/src"
fi

STEP=zlib   # a handful of C files; its own configure is not worth fighting when cross-compiling
if stage zlib "$D/lib/libz.a"; then
  S="$(unpack zlib)"
  (cd "$S" && for f in adler32 crc32 deflate infback inffast inflate inftrees trees zutil compress uncompr gzclose gzlib gzread gzwrite; do
     "$CC" -O2 -DZ_HAVE_UNISTD_H -c "$f.c" -o "$f.o"
   done && llvm-ar-19 rcs "$D/lib/libz.a" ./*.o)
  cp "$S/zlib.h" "$S/zconf.h" "$D/include/"
fi

STEP=bzip2  # the stock Makefile would run the wasm bzip2 over its samples: build the library only
if stage bzip2 "$D/lib/libbz2.a"; then
  S="$(unpack bzip2)"
  log make -C "$S" -j"$JOBS" CC="$CC" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 CFLAGS="-O2 -D_FILE_OFFSET_BITS=64" libbz2.a
  cp "$S/libbz2.a" "$D/lib/"; cp "$S/bzlib.h" "$D/include/"
fi

STEP=xz     # liblzma only; distro's patch keeps the signal-mask path musl really has
if stage xz "$D/lib/liblzma.a"; then
  S="$(unpack xz)"
  (cd "$S" && patch -p1 --quiet < "$DISTRO/xz/wasm-sigmask.patch")
  (cd "$S" && log ./configure "${HOST[@]}" CC="$CC" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 --prefix="$D" \
     --disable-shared --disable-nls --disable-threads --disable-sandbox --disable-doc --disable-scripts \
     --disable-xz --disable-xzdec --disable-lzmadec --disable-lzmainfo --disable-lzma-links)
  log make -C "$S/src/liblzma" -j"$JOBS" install
fi

STEP=ncurses  # wide-character libraries; the terminal database is compiled below with the host tic
if stage ncurses "$D/lib/libncursesw.a"; then
  S="$(unpack ncurses)"
  (cd "$S" && log env BUILD_CC=clang-19 CPP="$CC -E" ./configure "${HOST[@]}" CC="$CC" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 \
     --prefix="$D" --without-shared --without-debug --without-ada --without-manpages --without-cxx-binding \
     --without-progs --without-tests --disable-stripping --enable-widec --enable-pc-files=no \
     --with-default-terminfo-dir=/usr/share/terminfo --with-terminfo-dirs=/usr/share/terminfo:/etc/terminfo)
  log make -C "$S" -j"$JOBS" libs
  log make -C "$S" install.libs install.includes
  # Unsuffixed names for consumers that look for -lncurses or -ltinfo (libedit's configure does).
  for name in ncurses tinfo; do ln -sf libncursesw.a "$D/lib/lib$name.a"; done
  # The entries a terminal in front of this sandbox plausibly reports as TERM (compiled data is
  # architecture-independent). The full database is ~9 MB; these are what xterm.js, the Windows
  # console, macOS Terminal and tmux/screen present.
  rm -rf "$D/share/terminfo"; mkdir -p "$D/share/terminfo"
  tic -x -o "$D/share/terminfo" \
    -e ansi,dumb,linux,vt100,vt102,vt220,xterm,xterm-color,xterm-256color,xterm-direct,screen,screen-256color,tmux,tmux-256color,rxvt,rxvt-unicode,rxvt-unicode-256color,alacritty,xterm-kitty,st-256color \
    "$S/misc/terminfo.src"
fi

STEP=libedit  # the readline module over libedit: BSD-licensed, unlike GNU readline (GPL-3), which
              # would make the interpreter binary GPL-3. termcap comes from ncurses.
if stage libedit "$D/lib/libedit.a"; then
  S="$(unpack libedit)"
  rm -rf "$D/lib/libreadline.a" "$D/lib/libhistory.a" "$D/include/readline"  # an earlier GNU readline
  (cd "$S" && patch -p1 --quiet < "$P/patches/libedit-spawn-editor.patch")
  # -include stdc-predef.h: musl states there that wchar_t is ISO 10646 (__STDC_ISO_10646__), which
  # libedit insists on; clang includes it by itself only for targets it knows to be Linux.
  (cd "$S" && log ./configure "${HOST[@]}" CC="$CC" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 --prefix="$D" \
     CPPFLAGS="-include stdc-predef.h -I$D/include -I$D/include/ncursesw" LDFLAGS="-L$D/lib" \
     --disable-shared --enable-static --disable-examples)
  log make -C "$S" -j"$JOBS"
  log make -C "$S" install
fi

STEP=sqlite  # the amalgamation, compiled directly. No WAL and no mmap I/O: both need shared memory
             # mappings this platform does not have (distro builds it the same way).
if stage sqlite "$D/lib/libsqlite3.a"; then
  S="$(unpack sqlite)"
  (cd "$S" && log "$CC" -O2 -c sqlite3.c -o sqlite3.o \
     -DSQLITE_OMIT_WAL=1 -DSQLITE_MAX_MMAP_SIZE=0 -DSQLITE_THREADSAFE=1 -DSQLITE_OMIT_LOAD_EXTENSION=1 \
     -DSQLITE_ENABLE_FTS4=1 -DSQLITE_ENABLE_FTS5=1 -DSQLITE_ENABLE_RTREE=1 -DSQLITE_ENABLE_MATH_FUNCTIONS=1 \
     -DSQLITE_ENABLE_DBSTAT_VTAB=1 -DSQLITE_ENABLE_COLUMN_METADATA=1 -DSQLITE_ENABLE_UPDATE_DELETE_LIMIT=1)
  llvm-ar-19 rcs "$D/lib/libsqlite3.a" "$S/sqlite3.o"
  cp "$S/sqlite3.h" "$S/sqlite3ext.h" "$D/include/"
fi

STEP=openssl
# linux-generic32: 32-bit, no assembly (pure-C bignum and hashes), pthreads from musl.
# no-async: its job scheduler needs makecontext/swapcontext, which musl lacks on wasm.
# no-secure-memory: the mlocked arena needs mmap/mlock. no-afalgeng: no AF_ALG sockets here.
# no-shared/no-dso/no-dynamic-engine: static only, nothing to dlopen. -DHAVE_FORK=0: no fork().
# The trust store is /etc/ssl (cert.pem), where python.cpio puts the CA bundle.
if stage openssl "$D/lib/libssl.a"; then
  S="$(unpack openssl)"
  (cd "$S" && log perl ./Configure linux-generic32 CC="$CC" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 \
     -DHAVE_FORK=0 --prefix="$D" --libdir=lib --openssldir=/etc/ssl \
     no-shared no-dso no-dynamic-engine no-async no-afalgeng no-secure-memory no-tests no-apps no-docs)
  log make -C "$S" -j"$JOBS" build_libs
  log make -C "$S" install_dev
fi

fetch cacert > /dev/null
ls -l "$D"/lib/*.a | awk '{printf "  %-16s %s\n", $NF, $5}' | sed "s#$D/lib/##"
