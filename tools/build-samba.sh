#!/usr/bin/env bash
# smbclient for the guest (wasm32 Linux, musl), the `smbclient` step of tools/build.sh: Samba's own
# client, so SMB1 (NT1), SMB2 and SMB3 servers alike, into tools/stage/usr/bin/smbclient, and the
# license texts of what it carries into build/smbclient/licenses.
#
# Samba needs GnuTLS (its crypto), which needs nettle, GMP and libtasn1; smbclient's tar wants
# libarchive. They are built first, static, into build/samba-deps (zlib, libedit and ncurses are
# python/deps'). Samba itself is configured by waf for another machine (--cross-compile): the
# checks that run a program have their answers in patches/samba-cross-answers.txt, which came
# from running them in the guest (see samba-guest-run below), and only smbclient is built, as one
# program without Samba's shared libraries (--nonshared-binary). patches/samba-wasm.patch says
# what had to change: waf's --hostcc, no fork, no mmap, calls through pointers of another type.
#
#   tools/build-samba.sh          the steps below that are not done yet; waf's build always runs
#   FORCE=step[,step]             redoes those: gmp nettle tasn1 gnutls libarchive samba-src samba-configure
#   SAMBA_ANSWERS=update          configure asks the guest again (COLLABO_RUNTIME, default
#                                 dist/runtime/collabo-core-linux-x64) and rewrites the answers file
set -euo pipefail
N="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$N")"
U="$ROOT/userspace"
DEPS="$ROOT/python/deps"
B="$N/build"
SD="$B/samba-deps"          # the libraries only Samba uses
STAGE="${STAGE:-$N/stage}"
FORCE=",${FORCE:-},"
CC="$U/bin/wasm-cc"
export PATH="$U/tools:$PATH"
JOBS="$(nproc)"
HOST=(--host=wasm32-unknown-linux-musl --build=x86_64-pc-linux-gnu)
LINK_FLAGS="-Wl,--import-memory -Wl,--max-memory=4294967296 -Wl,--shared-memory -Wl,--export-table -Wl,-z,stack-size=8388608"
export CONFIG_SITE="$ROOT/python/config.site"

step() { # name output: true when the step must run
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
  dir="$B/$(tar -tf "$file" | awk -F/ 'NR == 1 { print $1 }')"
  rm -rf "$dir"; mkdir -p "$B"
  tar -xf "$file" -C "$B"
  echo "$dir"
}
log() { "$@" > "$B/$STEP.log" 2>&1 || { tail -40 "$B/$STEP.log"; echo "FAILED: $STEP (log: $B/$STEP.log)" >&2; exit 1; }; }

mkdir -p "$B" "$SD"
DEPCONF=("${HOST[@]}" CC="$CC" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 CFLAGS="-O2" --prefix="$SD"
         --disable-shared --enable-static)
export PKG_CONFIG_LIBDIR="$SD/lib/pkgconfig" PKG_CONFIG_PATH=

STEP=gmp  # for nettle's hogweed (GnuTLS insists on it); portable C, no assembly
if step gmp "$SD/lib/libgmp.a"; then
  S="$(unpack gmp)"
  (cd "$S" && log ./configure "${DEPCONF[@]}" --disable-assembly CC_FOR_BUILD=clang-19)
  log make -C "$S" -j"$JOBS"
  log make -C "$S" install
fi

STEP=nettle
if step nettle "$SD/lib/libnettle.a"; then
  S="$(unpack nettle)"
  (cd "$S" && log ./configure "${DEPCONF[@]}" --libdir="$SD/lib" CC_FOR_BUILD=clang-19 \
     CPPFLAGS="-I$SD/include" LDFLAGS="-L$SD/lib" --disable-assembler --disable-openssl \
     --disable-documentation --disable-fat --disable-x86-aesni --disable-arm-neon)
  log make -C "$S" -j"$JOBS"
  log make -C "$S" install
fi

STEP=tasn1
if step tasn1 "$SD/lib/libtasn1.a"; then
  S="$(unpack libtasn1)"
  (cd "$S" && log ./configure "${DEPCONF[@]}" --disable-doc --disable-valgrind-tests)
  log make -C "$S" -j"$JOBS" -C lib
  log make -C "$S" -C lib install
fi

STEP=gnutls  # TLS for nothing here (SMB does not use it): Samba wants GnuTLS for its ciphers, hashes
             # and key derivations. No p11-kit, TPM, IDN or compression; libunistring its own copy.
             # The crypto-auditing hooks are compiled out, but their context argument still is
             # __builtin_return_address(0), which WebAssembly has not: the "orphaned" context.
if step gnutls "$SD/lib/libgnutls.a"; then
  S="$(unpack gnutls)"
  (cd "$S" && log ./configure "${DEPCONF[@]}" CPPFLAGS="-I$SD/include -DCRAU_AUTO_CONTEXT=CRAU_ORPHANED_CONTEXT" \
     LDFLAGS="-L$SD/lib $LINK_FLAGS" GMP_LIBS="-L$SD/lib -lgmp" --with-included-unistring --without-p11-kit --without-tpm2 --without-tpm \
     --without-idn --without-zlib --without-brotli --without-zstd --without-leancrypto \
     --disable-tools --disable-doc --disable-cxx --disable-nls --disable-libdane --disable-tests \
     --disable-hardware-acceleration --disable-padlock --disable-guile --disable-valgrind-tests \
     --with-default-trust-store-file=/etc/ssl/cert.pem)
  log make -C "$S" -j"$JOBS"
  log make -C "$S" install
  # Samba reads `pkg-config --libs`, not --static: the libraries a static GnuTLS needs on Libs
  sed -i 's/^Libs: .*/Libs: -L${libdir} -lgnutls -lhogweed -lgmp -lnettle -ltasn1/' "$SD/lib/pkgconfig/gnutls.pc"
fi

STEP=libarchive  # smbclient's tar (-T, the tar command): the library only, with python/deps' zlib,
                 # bzip2 and xz; other filters run their programs through posix_spawnp
if step libarchive "$SD/lib/libarchive.a"; then
  S="$(unpack libarchive)"
  (cd "$S" && log ./configure "${DEPCONF[@]}" CPPFLAGS="-I$DEPS/include" LDFLAGS="-L$DEPS/lib $LINK_FLAGS" \
     --disable-bsdtar --disable-bsdcpio --disable-bsdcat --disable-bsdunzip --disable-acl --disable-xattr \
     --with-zlib --with-bz2lib --with-lzma --without-zstd --without-lz4 --without-lzo2 --without-libb2 \
     --without-cng --without-mbedtls --without-nettle --without-openssl --without-xml2 --without-expat)
  log make -C "$S" -j"$JOBS" libarchive.la
  log make -C "$S" install-libLTLIBRARIES install-includeHEADERS
fi

# What Samba's configure finds through pkg-config or by name: zlib and readline are python/deps'
# (readline is libedit's compatible API, under the names Samba looks for).
mkdir -p "$SD/lib/pkgconfig" "$SD/include/readline"
cat > "$SD/lib/pkgconfig/zlib.pc" <<PC
Name: zlib
Description: zlib compression library
Version: $(sed -n 's/^#define ZLIB_VERSION "\(.*\)"/\1/p' "$DEPS/include/zlib.h")
Libs: -L$DEPS/lib -lz
Cflags: -I$DEPS/include
PC
for h in readline history; do printf '#include <editline/readline.h>\n' > "$SD/include/readline/$h.h"; done
ln -sf "$DEPS/lib/libedit.a" "$SD/lib/libreadline.a"

# The configure checks that run a program run it in the guest: samba-guest-run boots the runtime's
# kernel and base image with the build tree shared at the same path (half a second a program).
# Their answers are kept in patches/samba-cross-answers.txt, so a build needs no guest: with
# SAMBA_ANSWERS=update configure asks the guest again and the file is written anew.
RT="${COLLABO_RUNTIME:-$ROOT/dist/runtime/collabo-core-linux-x64}"
cat > "$B/samba-guest-run" <<SH
#!/bin/sh
# samba-guest-run PROGRAM [ARGS...]: PROGRAM (built for the guest) run in the guest, in this folder
exec "$RT/bin/collabo-core-engine" exec --kernel "$RT/app/images/vmlinux.wasm" \\
  --initramfs "$RT/app/images/initramfs.cpio" --no-network --mount "$B:$B" --cwd "\$PWD" -- "\$@" 2>/dev/null
SH
chmod +x "$B/samba-guest-run"

# pidl, Samba's IDL compiler, wants Parse::Yapp (and finds yapp) on this machine
YAPP="$B/Parse-Yapp-1.21"
[[ -f "$YAPP/lib/Parse/Yapp/Driver.pm" ]] || unpack parse-yapp > /dev/null
export PERL5LIB="$YAPP/lib${PERL5LIB:+:$PERL5LIB}" PATH="$YAPP:$PATH"
chmod +x "$YAPP/yapp"

S="$B/samba"
STEP=samba-src   # the release with patches/samba-*.patch
if step samba-src "$S/.collabo-patched"; then
  rm -rf "$S"
  T="$(unpack samba)"; mv "$T" "$S"
  for p in "$N"/patches/samba-*.patch; do (cd "$S" && patch -p1 --quiet < "$p"); done
  touch "$S/.collabo-patched"
fi

# Heimdal's asn1_compile and compile_et run during the build: they are compiled for this machine
# (--hostcc; patches/samba-wasm.patch makes waf use it), against the guest's config.h. Two headers
# that config.h has seen in the guest's musl and this machine's glibc has not:
mkdir -p "$B/samba-hostcc"
printf '/* musl has crypt.h; the build tools only include it */\nchar *crypt(const char *, const char *);\n' \
  > "$B/samba-hostcc/crypt.h"
printf '/* musl has stropts.h; the build tools use nothing in it */\n' > "$B/samba-hostcc/stropts.h"

# The compiler Samba's build gets: wasm-cc, with the program link flags on program links only.
# waf builds libreplace as a shared library whatever the targets (its group comes first); nothing
# uses it, but it must link: without the program's flags and the -lc a shared library has not.
# waf writes -oFILE, and wasm-cc finds the program it rewrites after linking (setjmp's exception
# instructions to the standard encoding) by a separate -o FILE. Samba links its programs with
# --export-dynamic, for the modules it would load: here that only exports every function (and
# keeps them all), as nothing is loaded. A link naming the static libarchive gets what it needs.
cat > "$B/samba-cc" <<SH
#!/bin/sh
link=1 shared=0
for a; do
  shift
  case "\$a" in
    -o?*) set -- "\$@" -o "\${a#-o}" ;;
    -Wl,--export-dynamic) ;;
    -larchive) set -- "\$@" -larchive -lz -lbz2 -llzma ;;
    *) set -- "\$@" "\$a" ;;
  esac
  case "\$a" in -c|-S|-E|-M|-MM) link=0 ;; -shared) shared=1 ;; esac
done
if [ \$shared = 1 ]; then
  for a; do shift; [ "\$a" = -lc ] || set -- "\$@" "\$a"; done
  exec "$CC" "\$@"
fi
[ \$link = 1 ] && exec "$CC" "\$@" $LINK_FLAGS
exec "$CC" "\$@"
SH
chmod +x "$B/samba-cc"

# A client only: no AD DC, Python bindings, LDAP/ADS, winbind, PAM, ACLs, quotas, printing, Avahi,
# keyring, systemd and the like (the guest has no use for them, or they need what it has not);
# every library bundled; tdb without robust mutexes, which need the file mapped (no mmap).
STEP=samba-configure
if step samba-configure "$S/bin/c4che/default_cache.py"; then
  ANSWERS="$B/samba-cross-answers.txt"
  CROSS=(--cross-compile "--cross-answers=$ANSWERS")
  if [[ "${SAMBA_ANSWERS:-}" == update ]]; then
    : > "$ANSWERS"; CROSS+=("--cross-execute=$B/samba-guest-run")
  else
    cp "$N/patches/samba-cross-answers.txt" "$ANSWERS"
  fi
  (cd "$S" && log env CC="$B/samba-cc" AR=llvm-ar-19 RANLIB=llvm-ranlib-19 CFLAGS="-O2" \
     CPPFLAGS="-I$SD/include -I$DEPS/include" LDFLAGS="-L$SD/lib -L$DEPS/lib" \
     PKG_CONFIG_LIBDIR="$SD/lib/pkgconfig" PYTHON=python3 \
     ./configure "${CROSS[@]}" "--hostcc=clang-19 -I$B/samba-hostcc" \
     --prefix=/usr --sysconfdir=/etc --localstatedir=/var --enable-fhs \
     --without-ad-dc --disable-python --without-ads --without-ldap --without-pam --without-winbind \
     --without-acl-support --without-quotas --without-systemd --without-lttng \
     --without-json --without-gpgme --without-ldb-lmdb \
     --disable-cups --disable-iprint --disable-avahi --without-kernel-keyring --without-regedit \
     --without-winexe --disable-spotlight --disable-wsp --without-libunwind --without-utmp \
     --without-automount --without-dmapi --without-fam --without-cluster-support \
     --disable-glusterfs --disable-cephfs --disable-cephrgw --without-syslog --without-gettext \
     --bundled-libraries=ALL --nonshared-binary=client/smbclient --with-shared-modules='!vfs_snapper' --disable-tdb-mutex-locking \
     --disable-rpath --disable-rpath-install --disable-symbol-versions --without-pie --without-relro) \
    || { rm -f "$S/bin/c4che/default_cache.py"; exit 1; }
  [[ "${SAMBA_ANSWERS:-}" == update ]] && cp "$ANSWERS" "$N/patches/samba-cross-answers.txt"
fi

STEP=samba-build   # waf's own dependency tracking: always run
echo "== samba-build"
(cd "$S" && log env PYTHONHASHSEED=1 python3 ./buildtools/bin/waf build -j"$JOBS" --targets=client/smbclient)

# Into the stage, without the DWARF sections (as the other tools have none), and the license
# texts of what the program carries, for scripts/build-engine.sh: Samba (GPL-3.0; its libraries
# are LGPL-3.0, linked in), the Heimdal, popt, ngtcp2 and kernel-QUIC copies it builds, GnuTLS and
# the libraries under it, libarchive. zlib, bzip2, xz, libedit and ncurses are python/deps' (their
# texts are there already).
install -D "$S/bin/default/source3/client/smbclient" "$STAGE/usr/bin/smbclient"
llvm-strip-19 --strip-debug "$STAGE/usr/bin/smbclient"
L="$B/smbclient/licenses"
rm -rf "$L"; mkdir -p "$L"
cp "$S/COPYING" "$L/samba.GPL-3.0.txt"
cat "$S/third_party/heimdal/LICENSE" - "$S/third_party/heimdal/lib/hcrypto/libtommath/LICENSE" \
  <<< $'\n---- lib/hcrypto/libtommath ----\n' > "$L/samba-heimdal.BSD-3-Clause.txt"
cp "$S/third_party/popt/COPYING" "$L/samba-popt.MIT.txt"
sed -n '2,/\*\//p' "$S/third_party/ngtcp2/lib/ngtcp2_conn.c" | sed 's/^ \*\/\?//; s/^ //' > "$L/samba-ngtcp2.MIT.txt"
cp "$S/third_party/quic/COPYING" "$L/samba-quic.GPL-2.0-LGPL-2.1.txt"
cp "$B/gnutls-3.8.14/COPYING.LESSERv2" "$L/gnutls.LGPL-2.1.txt"
for x in nettle:nettle-3.10.2 gmp:gmp-6.3.0; do
  cat "$B/${x#*:}/COPYING.LESSERv3" - "$B/${x#*:}/COPYINGv2" <<< $'\n---- or ----\n' > "$L/${x%%:*}.LGPL-3.0-or-GPL-2.0.txt"
done
cp "$B/libtasn1-4.21.0/COPYING.LESSERv2" "$L/libtasn1.LGPL-2.1.txt"
cp "$B/libarchive-3.8.10/COPYING" "$L/libarchive.BSD-2-Clause.txt"
ls -l "$STAGE/usr/bin/smbclient"
