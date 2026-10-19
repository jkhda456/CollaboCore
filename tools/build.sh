#!/usr/bin/env bash
# Build the guest's tools for wasm32-linux and pack them as tools.cpio, an overlay like python.cpio
# (`python3 build.py tools-image`):
#
#   curl      8.21  HTTP(S)/FTP client and libcurl (OpenSSL, zlib); git's http(s) transport uses it
#   dropbear  2026  `ssh` (dbclient) and dropbearkey; keys can stay in the host's ssh-agent
#   git       2.55  with the http(s) and ssh transports
#   screen    5.0   GNU screen, the terminal multiplexer
#   openssl   3.5   the openssl command (keys, certificates, s_client, enc, dgst, ...)
#   sqlite    3.51  the sqlite3 shell, the same SQLite (and options) as Python's sqlite3 module
#   svn       1.15  Subversion: svn, svnadmin, svnlook, svnserve, svnsync, svnrdump, svnmucc,
#                   svndumpfilter, svnfsfs, svnversion (one program, the others links to it),
#                   over APR 1.7, APR-util 1.6, expat and serf 1.3 (http and https)
#   gui             the display for GUI programs: `gui` (server and command line, Rust),
#                   libcollabo-gui and gui-demo (C); our own, in gui/ (gui/README.md)
#   rust            our Rust reimplementations, in rust/ (rust/README.md): xz, zstd, 7z (one
#                   binary, collabo-archive, under each name), jq, git-lfs
#
# telnet, wget, nc, ftpget and ssl_client come with busybox in the base image; so do xz, unxz,
# xzcat, lzma, unlzma and lzcat (decompression only), which this image's links replace.
#
# Mirrors third_party/distro/distro/{curl,dropbear,git}/package.nix without Nix, with distro's
# patches; distro has no screen or Subversion, so their patches are ours (patches/screen-nofork.patch,
# patches/apr-wasm.patch). OpenSSL, zlib, ncurses and SQLite are the static libraries
# python/build-deps.sh builds (python/deps), so the python step comes first. Stages skip when their output exists; FORCE=stage[,stage] redoes
# them.   stages: curl dropbear git screen openssl sqlite svn gui rust cpio (gui and rust always run: cargo
# and the compiles are incremental)
set -euo pipefail

N="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$N")"
U="$ROOT/userspace"
DISTRO="$ROOT/third_party/distro/distro"
DEPS="$ROOT/python/deps"
B="$N/build"
STAGE="$N/stage"
FORCE=",${FORCE:-},"
CC="$U/bin/wasm-cc"
export PATH="$U/tools:$PATH"
JOBS="$(nproc)"
HOST=(--host=wasm32-unknown-linux-musl --build=x86_64-pc-linux-gnu)
LINK_FLAGS="-Wl,--import-memory -Wl,--max-memory=4294967296 -Wl,--shared-memory -Wl,--export-table -Wl,-z,stack-size=8388608"
export CONFIG_SITE="$ROOT/python/config.site"
chmod +x "$CC"

[[ -f "$DEPS/lib/libssl.a" && -f "$DEPS/lib/libz.a" && -f "$DEPS/lib/libncursesw.a" ]] \
  || { echo "missing OpenSSL/zlib/ncurses for the guest; build the python step first (python3 build.py python)" >&2; exit 1; }

stage() { # name output: true when the stage must run
  if [[ "$FORCE" != *",$1,"* && -e "$2" ]]; then echo "== $1: up to date"; return 1; fi
  echo "== $1"; return 0
}

fetch() { # NAME: download (once) and check the source sources.lock names; prints its path
  local url sum file
  read -r _ url sum < <(grep -E "^$1 " "$N/sources.lock")
  file="$N/src/$(basename "$url")"
  mkdir -p "$N/src"
  [[ -f "$file" ]] || { echo "== fetch $(basename "$url")" >&2; curl -fsSL --retry 3 -o "$file.part" "$url" && mv "$file.part" "$file"; }
  echo "$sum  $file" | sha256sum -c --quiet - >&2 || { echo "checksum mismatch: $file (deleted)" >&2; rm -f "$file"; exit 1; }
  echo "$file"
}

untar() { # FILE TAR-ARGS...: tar of FILE; a .bz2 through Python's bz2 if the host has no bzip2
  local file="$1"; shift
  if [[ "$file" == *.bz2 ]] && ! command -v bzip2 > /dev/null; then
    python3 -I -c 'import bz2, shutil, sys; shutil.copyfileobj(bz2.open(sys.argv[1]), sys.stdout.buffer)' "$file" \
      | tar "$@" -f -
  else
    tar "$@" -f "$file"
  fi
}

unpack() { # NAME: a fresh copy of the source under build/; prints the directory
  local file dir
  file="$(fetch "$1")"
  dir="$B/$(untar "$file" -t | awk -F/ 'NR == 1 { print $1 }')"
  rm -rf "$dir"; mkdir -p "$B"
  untar "$file" -x -C "$B"
  echo "$dir"
}

log() { "$@" > "$B/$STEP.log" 2>&1 || { tail -30 "$B/$STEP.log"; echo "FAILED: $STEP (log: $B/$STEP.log)" >&2; exit 1; }; }

mkdir -p "$B" "$STAGE"

STEP=curl  # static, OpenSSL + zlib; no threaded resolver (lookups stay in-process). Unlike distro,
           # it trusts /etc/ssl/cert.pem: here TLS runs in the guest, over the managed network.
if stage curl "$STAGE/usr/bin/curl"; then
  S="$(unpack curl)"
  (cd "$S" && log ./configure "${HOST[@]}" CC="$CC" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 \
     --prefix=/usr CPPFLAGS="-I$DEPS/include" LDFLAGS="-L$DEPS/lib $LINK_FLAGS" LIBS="-lssl -lcrypto -lz" \
     --disable-shared --enable-static --with-openssl="$DEPS" --with-zlib="$DEPS" \
     --disable-threaded-resolver --disable-ares --disable-ldap --disable-ldaps --disable-manual --disable-docs \
     --disable-ntlm --without-brotli --without-zstd --without-libpsl --without-libidn2 --without-nghttp2 \
     --without-nghttp3 --without-ngtcp2 --without-libssh2 --without-libssh \
     --with-ca-bundle=/etc/ssl/cert.pem --without-ca-path)
  log make -C "$S" -j"$JOBS"
  log make -C "$S" install DESTDIR="$B/curl-root"
  install -D "$B/curl-root/usr/bin/curl" "$STAGE/usr/bin/curl"
fi

STEP=dropbear  # the client side only: dbclient (installed as ssh too), dropbearkey, dropbearconvert.
               # distro's patch replaces fork with posix_spawn; localoptions.h compiles out the rest.
if stage dropbear "$STAGE/usr/bin/dbclient"; then
  S="$(unpack dropbear)"
  (cd "$S" && patch -p1 --quiet < "$DISTRO/dropbear/wasm-posix-spawn.patch")
  cp "$N/patches/dropbear-localoptions.h" "$S/localoptions.h"
  # HAVE_DAEMON: keeps compat.c's fork()-based daemon() fallback out (nothing calls daemon()).
  (cd "$S" && log env ac_cv_func_daemon=yes ./configure "${HOST[@]}" CC="$CC" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 \
     --prefix=/usr LDFLAGS="$LINK_FLAGS" \
     --disable-zlib --disable-pam --disable-lastlog --disable-wtmp --disable-utmp --disable-loginfunc)
  log make -C "$S" -j"$JOBS" PROGRAMS="dbclient dropbearkey dropbearconvert"
  for p in dbclient dropbearkey dropbearconvert; do install -D "$S/$p" "$STAGE/usr/bin/$p"; done
fi

STEP=git  # distro's flags, each explained in its package.nix; in short: every child goes through
          # run-command's start_command, which the patch turns into posix_spawn; no mmap (compat
          # reads objects), git's own regex (REG_STARTEND), no Perl/Tcl/Python/gettext/expat/Rust.
if stage git "$STAGE/usr/bin/git"; then
  S="$(unpack git)"
  (cd "$S" && patch -p1 --quiet < "$DISTRO/git/run-command-posix-spawn.patch")
  CURL_ROOT="$B/curl-root/usr"
  log make -C "$S" -j"$JOBS" CC="$CC" AR=llvm-ar-19 \
    CFLAGS="-O2 -I$DEPS/include" LDFLAGS="-L$DEPS/lib -L$CURL_ROOT/lib $LINK_FLAGS" \
    CURL_CONFIG=/bin/false CURL_CFLAGS="-I$CURL_ROOT/include" CURL_LDFLAGS="-lcurl -lssl -lcrypto -lz" \
    uname_S=Linux NO_RUST=YesPlease NO_MMAP=YesPlease NO_REGEX=YesPlease NO_POSIX_GOODIES=YesPlease \
    NO_OPENSSL=YesPlease NO_PERL=YesPlease NO_TCLTK=YesPlease NO_PYTHON=YesPlease NO_GETTEXT=YesPlease \
    NO_EXPAT=YesPlease PERL_PATH=/usr/bin/perl NO_INSTALL_HARDLINKS=YesPlease INSTALL_SYMLINKS=YesPlease \
    LINK_FUZZ_PROGRAMS= prefix=/usr \
    all install DESTDIR="$B/git-root"
  rm -rf "$STAGE/usr/libexec/git-core" "$STAGE/usr/share/git-core"
  mkdir -p "$STAGE/usr/libexec" "$STAGE/usr/share"
  cp -a "$B/git-root/usr/libexec/git-core" "$STAGE/usr/libexec/"
  cp -a "$B/git-root/usr/share/git-core" "$STAGE/usr/share/"
  install -D "$B/git-root/usr/bin/git" "$STAGE/usr/bin/git"
  # Servers and rarely used helpers stay out (4 MB of wasm each for the curl-linked ones):
  # git-http-fetch is the standalone dumb-http walker (git-remote-http has its own), imap-send,
  # daemon and http-backend serve or mail rather than fetch.
  rm -f "$STAGE"/usr/libexec/git-core/git-{http-fetch,imap-send,daemon,http-backend}
fi

STEP=screen  # the patch replaces screen's fork()s: the backend is screen started again, a window's
             # shell a clone(CLONE_VM | CLONE_VFORK) child that takes its pty before exec (see its
             # header). No PAM (the guest has only root, and screen asks for a password only with
             # `auth on`), utmp or telnet. Sockets in ~/.screen; terminfo is the cpio's own copy.
if stage screen "$STAGE/usr/bin/screen"; then
  S="$(unpack screen)"
  (cd "$S" && patch -p1 --quiet < "$N/patches/screen-nofork.patch")
  (cd "$S" && log ./configure "${HOST[@]}" CC="$CC" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 \
     --prefix=/usr CFLAGS="-O2" CPPFLAGS="-I$DEPS/include" LDFLAGS="-L$DEPS/lib $LINK_FLAGS" \
     --disable-pam --disable-utmp --disable-telnet)
  log make -C "$S" -j"$JOBS"
  install -D "$S/screen" "$STAGE/usr/bin/screen"
fi

STEP=openssl  # the openssl command, from the release python's libssl/libcrypto come from and with
              # the same Configure (python/build-deps.sh explains each flag); the libraries are
              # built again here, as the apps link the tree's own. -DHAVE_FORK=0 leaves out the two
              # parts that fork: `speed -multi` and the forking HTTP server of `ocsp -port`/`-multi`;
              # patches/openssl-wasm.patch the one that needs mmap's family (`speed -mlock`).
              # openssl.cnf is upstream's default config, read from /etc/ssl (the OPENSSLDIR).
if stage openssl "$STAGE/usr/bin/openssl"; then
  S="$(unpack openssl)"
  (cd "$S" && patch -p1 --quiet < "$N/patches/openssl-wasm.patch")
  (cd "$S" && log perl ./Configure linux-generic32 CC="$CC" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 \
     -DHAVE_FORK=0 $LINK_FLAGS --prefix=/usr --libdir=lib --openssldir=/etc/ssl \
     no-shared no-dso no-dynamic-engine no-async no-afalgeng no-secure-memory no-tests no-docs)
  log make -C "$S" -j"$JOBS" build_programs
  install -D "$S/apps/openssl" "$STAGE/usr/bin/openssl"
  install -D -m644 "$S/apps/openssl.cnf" "$STAGE/etc/ssl/openssl.cnf"
fi

STEP=sqlite  # the shell (shell.c) over the amalgamation compiled as python/build-deps.sh does for
             # _sqlite3 (no WAL and no mmap I/O, which need shared mappings; FTS4/5, R*Tree, math,
             # dbstat), so both see the same SQLite. Line editing from libedit, as for Python's
             # readline. `.system`, `.shell` and `.once |cmd` go through system()/popen(), which
             # musl does with posix_spawn.
if stage sqlite "$STAGE/usr/bin/sqlite3"; then
  S="$(unpack sqlite)"
  (cd "$S" && log "$CC" -O2 -o sqlite3 shell.c sqlite3.c -I"$DEPS/include" \
     -DSQLITE_OMIT_WAL=1 -DSQLITE_MAX_MMAP_SIZE=0 -DSQLITE_THREADSAFE=1 -DSQLITE_OMIT_LOAD_EXTENSION=1 \
     -DSQLITE_ENABLE_FTS4=1 -DSQLITE_ENABLE_FTS5=1 -DSQLITE_ENABLE_RTREE=1 -DSQLITE_ENABLE_MATH_FUNCTIONS=1 \
     -DSQLITE_ENABLE_DBSTAT_VTAB=1 -DSQLITE_ENABLE_COLUMN_METADATA=1 -DSQLITE_ENABLE_UPDATE_DELETE_LIMIT=1 \
     -DHAVE_EDITLINE=1 -L"$DEPS/lib" -ledit -lncursesw $LINK_FLAGS)
  install -D "$S/sqlite3" "$STAGE/usr/bin/sqlite3"
  install -D -m644 "$S/sqlite3.1" "$STAGE/usr/share/man/man1/sqlite3.1"
fi

STEP=svn  # Subversion and the libraries it needs, which only it uses (so they stay in build/svn-deps):
          # APR (patches/apr-wasm.patch: children start with clone(CLONE_VM | CLONE_VFORK), as
          # posix_spawn's do, and getopt's errfn has its type; no SysV IPC in the guest's kernel,
          # so its locks are fcntl's), APR-util
          # with expat for XML, and serf (http and https, OpenSSL and zlib), compiled here without
          # SCons as its SConstruct does. SQLite is python/deps' (the shell's, without WAL and mmap);
          # utf8proc and lz4 are Subversion's own copies. svnserve has no fork either: it serves
          # each connection in a thread, and -d stays in the foreground (APR_HAS_FORK is 0).
# Upstream's `make install-bin` set, but svnbench (a benchmark)
SVN_PROGRAMS="svn svnadmin svndumpfilter svnfsfs svnlook svnmucc svnrdump svnserve svnsync svnversion"
if stage svn "$STAGE/usr/bin/svn"; then
  SD="$B/svn-deps"
  rm -rf "$SD"
  DEPCONF=("${HOST[@]}" CC="$CC" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 CFLAGS="-O2" --prefix="$SD"
           --disable-shared --enable-static)
  STEP=svn-apr
  S="$(unpack apr)"
  (cd "$S" && patch -p1 --quiet < "$N/patches/apr-wasm.patch")
  # What configure cannot run to find out: musl's answers (XSI strerror_r, a 32-bit size_t and
  # 64-bit off_t, epoll, accept4/dup3/SOCK_CLOEXEC); semget and sem_open exist in musl but not in
  # the kernel (no CONFIG_SYSVIPC) or need mmap, which leaves fcntl for process locks. No DSO:
  # there is no dlopen in a static program.
  (cd "$S" && log ./configure "${DEPCONF[@]}" CC_FOR_BUILD=clang-19 --disable-dso \
     ac_cv_file__dev_zero=yes ac_cv_strerror_r_rc_int=yes ac_cv_sizeof_struct_iovec=8 \
     ac_cv_sizeof_off_t=8 ac_cv_sizeof_pid_t=4 ac_cv_sizeof_size_t=4 ac_cv_sizeof_ssize_t=4 \
     apr_cv_process_shared_works=no apr_cv_mutex_robust_shared=no apr_cv_mutex_recursive=yes \
     apr_cv_tcp_nodelay_with_cork=yes ac_cv_func_setpgrp_void=yes apr_cv_epoll=yes \
     apr_cv_epoll_create1=yes apr_cv_dup3=yes apr_cv_accept4=yes apr_cv_sock_cloexec=yes \
     apr_cv_gai_addrconfig=yes ac_cv_func_semget=no ac_cv_func_semctl=no ac_cv_func_sem_open=no)
  log make -C "$S" -j"$JOBS"
  log make -C "$S" install
  STEP=svn-expat
  S="$(unpack expat)"
  (cd "$S" && log ./configure "${DEPCONF[@]}" --without-docbook --without-examples --without-tests --without-xmlwf)
  log make -C "$S" -j"$JOBS" install
  STEP=svn-apr-util  # only what Subversion uses of it: no crypto, DBM or SQL drivers, no LDAP
  S="$(unpack apr-util)"
  # wasm-ld 19 takes the static functions of an archive's members for lazy definitions too: this
  # static BF_set_key (bcrypt's) would stand for OpenSSL's, and e_bf.o's call to it go nowhere
  sed -i 's/\<BF_set_key\>/apu_bcrypt_set_key/g' "$S/crypto/crypt_blowfish.c"
  (cd "$S" && log ./configure "${DEPCONF[@]}" --with-apr="$SD" --with-expat="$SD" \
     --without-crypto --without-openssl --without-nss --without-commoncrypto --without-berkeley-db \
     --without-gdbm --without-ndbm --without-pgsql --without-mysql --without-sqlite2 --without-sqlite3 \
     --without-oracle --without-odbc --without-ldap)
  log make -C "$S" -j"$JOBS"
  log make -C "$S" install
  STEP=svn-serf  # SConstruct's sources and flags; no GSSAPI (Negotiate) or SSPI
  S="$(unpack serf)"
  (cd "$S" && for f in *.c buckets/*.c auth/auth*.c; do
     [[ "$f" == auth/auth_spnego_sspi.c ]] && continue
     "$CC" -O2 -DNDEBUG -DOPENSSL_NO_STDIO -D_GNU_SOURCE -I. -I"$SD/include/apr-1" -I"$DEPS/include" \
       -c "$f" -o "$(echo "${f%.c}" | tr / _).o" || exit 1
   done > "$B/$STEP.log" 2>&1) || { tail -30 "$B/$STEP.log"; echo "FAILED: $STEP" >&2; exit 1; }
  rm -f "$SD/lib/libserf-1.a"
  llvm-ar-19 rcs "$SD/lib/libserf-1.a" "$S"/*.o
  install -D -m644 -t "$SD/include/serf-1" "$S"/serf.h "$S"/serf_bucket_types.h "$S"/serf_bucket_util.h
  STEP=svn
  S="$(unpack subversion)"
  # LIBS: what the static serf needs, for configure's link tests and the programs. The editor
  # for log messages when neither SVN_EDITOR, VISUAL nor EDITOR is set: vi, as for git.
  (cd "$S" && log ./configure "${HOST[@]}" CC="$CC" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 \
     --prefix=/usr CFLAGS="-O2" CPPFLAGS="-I$DEPS/include" LDFLAGS="-L$DEPS/lib $LINK_FLAGS" \
     LIBS="-lssl -lcrypto -lz" --disable-shared --enable-static \
     --with-apr="$SD" --with-apr-util="$SD" --with-serf="$SD" --with-expat="$SD/include:$SD/lib:expat" \
     --with-sqlite="$DEPS" --with-zlib="$DEPS" --with-utf8proc=internal --with-lz4=internal \
     --without-berkeley-db --without-apxs --without-jdk --disable-javahl --without-gnome-keyring \
     --without-kwallet --without-sasl --without-doxygen --without-boost --disable-nls --with-editor=vi)
  # libtool would hand wasm-ld the -rpath every link gets (wasm-ld has none)
  sed -i 's/^hardcode_libdir_flag_spec=.*/hardcode_libdir_flag_spec=""/' "$S/libtool"
  log make -C "$S" -j"$JOBS" $SVN_PROGRAMS
  # The image carries them as one program (patches/svn-multicall.c), as their code is nearly all
  # the same libraries: each command's main() compiled again as svn_main_<command>, the rest of
  # its objects as make built them, and the libraries once.
  STEP=svn-multicall
  MC="$B/svn-multicall"
  rm -rf "$MC"; mkdir -p "$MC"
  mkvar() { printf 'show:\n\t@echo %s\n' "$1" | make -s -C "$S" -f Makefile -f - show; }
  COMPILE="$(mkvar '$(COMPILE)')"
  objs=()
  for p in $SVN_PROGRAMS; do
    (cd "$S" && log $COMPILE -Dmain="svn_main_$p" -c "subversion/$p/$p.c" -o "$MC/$p.o")
    for o in $(mkvar "\$(${p}_OBJECTS)"); do
      [[ "$o" == "$p.lo" ]] || objs+=("$S/subversion/$p/${o%.lo}.o")
    done
  done
  log "$CC" -O2 -c "$N/patches/svn-multicall.c" -o "$MC/multicall.o"
  log "$CC" -o "$MC/svn" "$MC"/*.o "${objs[@]}" \
    $(for l in client wc ra ra_local ra_svn ra_serf repos fs fs_fs fs_x fs_util delta diff subr; do
        echo "$S/subversion/libsvn_$l/.libs/libsvn_$l-1.a"; done) \
    -L"$SD/lib" -laprutil-1 -lexpat -lapr-1 -lserf-1 -L"$DEPS/lib" -lsqlite3 -lssl -lcrypto -lz $LINK_FLAGS
  rm -f "$STAGE"/usr/bin/svn*
  install -D "$MC/svn" "$STAGE/usr/bin/svn"
fi

STEP=gui
echo "== gui"
STAGE="$STAGE" bash "$N/gui/build.sh"

STEP=rust
echo "== rust"
STAGE="$STAGE" bash "$N/rust/build.sh"

# The image: an overlay after initramfs.cpio (and python.cpio, if present). etc/ssl/cert.pem is the
# same Mozilla bundle python.cpio carries, so either image alone has a trust store; likewise
# usr/share/terminfo, which screen needs for the terminal in front of it.
echo "== cpio"   # always: packing takes a second
{
  CACERT="$ROOT/python/src/$(awk '$1 == "cacert" {n = split($2, a, "/"); print a[n]}' "$ROOT/python/sources.lock")"
  mkdir -p "$N/out"
  python3 "$U/mkinitramfs.py" --overlay -o "$N/out/tools.cpio" \
    --file usr/bin/curl="$STAGE/usr/bin/curl" \
    --file usr/bin/git="$STAGE/usr/bin/git" \
    --file usr/bin/dbclient="$STAGE/usr/bin/dbclient" --link usr/bin/ssh=dbclient \
    --file usr/bin/dropbearkey="$STAGE/usr/bin/dropbearkey" \
    --file usr/bin/dropbearconvert="$STAGE/usr/bin/dropbearconvert" \
    --tree usr/libexec/git-core="$STAGE/usr/libexec/git-core" \
    --tree usr/share/git-core="$STAGE/usr/share/git-core" \
    --file usr/bin/screen="$STAGE/usr/bin/screen" \
    --file usr/bin/openssl="$STAGE/usr/bin/openssl" \
    --file etc/ssl/openssl.cnf="$STAGE/etc/ssl/openssl.cnf" \
    --file usr/bin/sqlite3="$STAGE/usr/bin/sqlite3" \
    --file usr/bin/gui="$STAGE/usr/bin/gui" \
    --file usr/bin/gui-demo="$STAGE/usr/bin/gui-demo" \
    --file usr/include/collabo_gui.h="$STAGE/usr/include/collabo_gui.h" \
    --file usr/lib/libcollabo-gui.a="$STAGE/usr/lib/libcollabo-gui.a" \
    --file usr/bin/collabo-archive="$STAGE/usr/bin/collabo-archive" \
    $(for n in xz unxz xzcat lzma unlzma lzcat zstd unzstd zstdcat zstdmt 7z 7za 7zr; do echo "--link usr/bin/$n=collabo-archive"; done) \
    --file usr/bin/jq="$STAGE/usr/bin/jq" \
    --file usr/bin/git-lfs="$STAGE/usr/bin/git-lfs" \
    --file usr/bin/svn="$STAGE/usr/bin/svn" \
    $(for p in $SVN_PROGRAMS; do [[ $p == svn ]] || echo "--link usr/bin/$p=svn"; done) \
    --tree usr/share/terminfo="$DEPS/share/terminfo" \
    --file etc/ssl/cert.pem="$CACERT" \
    --etc "$N/etc"
}
ls -l "$N/out/tools.cpio"
