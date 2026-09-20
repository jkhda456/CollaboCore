#!/usr/bin/env bash
# Build busybox (tombl fork, NOMMU) for wasm32-linux with the wasm-cc wrapper.
# Config mirrors distro/distro/busybox/package.nix.
set -euo pipefail
U="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="$U/src/busybox"; OUT="$U/out/busybox"
export PATH="$U/tools:$PATH"
LF="-Wl,--import-memory -Wl,--max-memory=4294967296 -Wl,--shared-memory -Wl,--export-table -Wl,-z,stack-size=8388608"
cd "$SRC"

config() { # name value
  sed -i "/CONFIG_$1=/d;/CONFIG_$1 is not set/d" .config
  case $2 in y|n) echo "CONFIG_$1=$2" >> .config ;; *) echo "CONFIG_$1=\"$2\"" >> .config ;; esac
}
if [[ "${1:-}" != "--no-config" ]]; then
  make CC="$U/bin/wasm-cc" HOSTCC=clang-19 defconfig >/dev/null
  config STATIC y; config NOMMU y; config STATIC_LIBGCC n
  config CROSS_COMPILER_PREFIX llvm-
  config EXTRA_CFLAGS "-I$U/kheaders/include"
  config EXTRA_LDLIBS c
  for k in MOUNT SWITCH_ROOT CHROOT HUSH HUSH_TICK CROND CRONTAB FTPD HTTPD IFDOWN IFUP NC SCRIPT START_STOP_DAEMON \
           TCPSVD TIME TS UDPSVD WGET; do config $k y; done
  config SH_IS_ASH n; config SH_IS_HUSH y; config SH_IS_NONE n
  config BASH_IS_ASH n; config BASH_IS_HUSH n; config BASH_IS_NONE y
  config FEATURE_FTPD_AUTHENTICATION n
  # Embedding applet scripts (mim, nologin) needs host bzip2, which this machine lacks.
  config FEATURE_SH_EMBEDDED_SCRIPTS n
  # The kernel has no module support. Also, LLD 19 miscompiles the call to filename2modname()
  # (defined with different signatures in modutils.c and modprobe-small.c): the stub for the
  # mismatch is wired to function 0, which V8 rejects. Drop the applets that need it.
  for k in INSMOD RMMOD LSMOD MODINFO MODPROBE DEPMOD MODPROBE_SMALL; do config $k n; done
  for k in BOOTCHARTD CONSPY DEVMEM FBSPLASH HDPARM HEXEDIT INETD NSENTER SWAPOFF SWAPON TC TELNETD \
           SENDMAIL REFORMIME MAKEMIME POPMAILDIR INIT LINUXRC RUNSV RUNSVDIR SVLOGD HWCLOCK RTCWAKE; do config $k n; done
  # `yes` dies of SIGPIPE when oldconfig stops reading; that must not trip pipefail.
  (set +o pipefail; yes "" | make CC="$U/bin/wasm-cc" HOSTCC=clang-19 oldconfig >/dev/null)
fi

make -j"$(nproc)" CC="$U/bin/wasm-cc" HOSTCC=clang-19 CFLAGS_busybox="$LF"
rm -rf "$OUT"; mkdir -p "$OUT"
make CC="$U/bin/wasm-cc" HOSTCC=clang-19 CFLAGS_busybox="$LF" CONFIG_PREFIX="$OUT" install >/dev/null
ls -l "$OUT/bin/busybox"
