#!/usr/bin/env bash
# Build the network tools for wasm32-linux and pack them as tools.cpio, an overlay like python.cpio:
#
#   curl      8.21  HTTP(S)/FTP client and libcurl (OpenSSL, zlib); git's http(s) transport uses it
#   dropbear  2026  `ssh` (dbclient) and dropbearkey; keys can stay in the host's ssh-agent
#   git       2.55  with the http(s) and ssh transports
#
# telnet, wget, nc, ftpget and ssl_client come with busybox in the base image.
#
# Mirrors third_party/distro/distro/{curl,dropbear,git}/package.nix without Nix, with distro's
# patches. OpenSSL and zlib are the static libraries python/build-deps.sh builds (python/deps),
# so the python step comes first. Stages skip when their output exists; FORCE=stage[,stage]
# redoes them.   stages: curl dropbear git cpio
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

[[ -f "$DEPS/lib/libssl.a" && -f "$DEPS/lib/libz.a" ]] \
  || { echo "missing OpenSSL/zlib for the guest; build the python step first (python3 build.py python)" >&2; exit 1; }

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

unpack() { # NAME: a fresh copy of the source under build/; prints the directory
  local file dir
  file="$(fetch "$1")"
  dir="$B/$(tar -tf "$file" 2>/dev/null | head -1 | cut -d/ -f1)"
  rm -rf "$dir"; mkdir -p "$B"
  tar -xf "$file" -C "$B"
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

# The image: an overlay after initramfs.cpio (and python.cpio, if present). etc/ssl/cert.pem is the
# same Mozilla bundle python.cpio carries, so either image alone has a trust store.
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
    --file etc/ssl/cert.pem="$CACERT" \
    --etc "$N/etc"
}
ls -l "$N/out/tools.cpio"
