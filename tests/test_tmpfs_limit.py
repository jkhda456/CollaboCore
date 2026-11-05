#!/usr/bin/env python3
"""tmpfs (ramfs here: the kernel has no shmem without an MMU) has limits
(kernel/patches/tmpfs-size-limit.patch): every ramfs and tmpfs mount, the root's too, shares
ramfs.limit, half of RAM by default, and a mount's size= is a limit of its own; past either a write
is ENOSPC instead of the machine running out of memory. du and df show what files use.

The guest's memory grows to 2 GiB here (host RSS too).

  python3 tests/test_tmpfs_limit.py   engine/target/release/collabo-core-engine and the build's
                                      images, or the runtime folder COLLABO_RUNTIME names
"""
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

runtime = os.environ.get("COLLABO_RUNTIME")
if runtime:
    images = Path(runtime) / "app" / "images"
    ENGINE = [str(Path(runtime) / "bin" / "collabo-core-engine"), "--kernel", str(images / "vmlinux.wasm"),
              "--initramfs", str(images / "initramfs.cpio")]
else:
    ENGINE = [str(ROOT / "engine/target/release/collabo-core-engine"), "--kernel", str(ROOT / "kernel/linux/vmlinux.wasm"),
              "--initramfs", str(ROOT / "userspace/out/initramfs.cpio")]

failures = 0


def check(ok, what, detail=""):
    global failures
    print(("ok   " if ok else "FAIL ") + what)
    if not ok:
        failures += 1
        if detail:
            print("     " + str(detail)[-3000:].replace("\n", "\n     "))


SCRIPT = r'''
used() { df -k "$1" | awk 'NR==2{print $3}'; }
avail() { df -k "$1" | awk 'NR==2{print $4}'; }
total_kb() { while read -r k v u; do [ "$k" = MemTotal: ] && echo $v; done < /proc/meminfo; }
echo "limit=$(cat /sys/module/ramfs/parameters/limit)"
echo "memtotal=$(total_kb)"
echo "tmp-used-before=$(used /tmp)"
# dd's complaint goes through a pipe: /tmp is full by then
dd if=/dev/zero of=/tmp/big bs=1M count=4096 2>&1 | grep -q 'No space left' && echo "fill-enospc=1"
echo "big-bytes=$(wc -c < /tmp/big)"
echo "big-du=$(du -k /tmp/big | cut -f1)"
echo "tmp-avail-full=$(avail /tmp)"
echo x > /root/shared 2>/dev/null; echo "root-rc=$?"
rm /tmp/big
echo "tmp-used-after=$(used /tmp)"
echo x > /root/shared; echo "root-after-rc=$?"
# a file written again from the start (O_TRUNC to 0, then pages again), as unpacking archives does
dd if=/dev/zero of=/tmp/o bs=1M count=10 2>/dev/null; dd if=/dev/zero of=/tmp/o bs=1M count=5 2>/dev/null
echo "rewritten-du=$(du -k /tmp/o | cut -f1) rewritten-used=$(used /tmp)"
rm /tmp/o; echo "rewritten-used-after=$(used /tmp)"
echo x > /tmp/after-rewrite; echo "after-rewrite-rc=$?"; rm -f /tmp/after-rewrite
# a sparse file read back: ramfs keeps a page for each hole it reads, within the limit too
# (not truncate from size 0: nommu ramfs then allocates the whole size at once, for shared mmap)
echo x > /tmp/sparse; truncate -s 3G /tmp/sparse; echo "sparse-du=$(du -k /tmp/sparse | cut -f1)"
cat /tmp/sparse 2>&1 >/dev/null | grep -q 'No space left' && echo "sparse-enospc=1"
rm /tmp/sparse; echo "tmp-used-sparse-after=$(used /tmp)"
mkdir -p /mnt/t; mount -t tmpfs -o size=8M tmpfs /mnt/t; echo "mount-rc=$?"
grep -q ' /mnt/t tmpfs .*size=8192k' /proc/mounts && echo "shown-size=1"
dd if=/dev/zero of=/mnt/t/a bs=1M count=16 2>/dev/null; echo "own-bytes=$(wc -c < /mnt/t/a)"
mount -o remount,size=1M /mnt/t 2>/dev/null; echo "shrink-rc=$?"
mount -o remount,size=16M /mnt/t; echo "grow-rc=$?"
dd if=/dev/zero of=/mnt/t/b bs=1M count=16 2>/dev/null; echo "grown-bytes=$(wc -c < /mnt/t/b)"
rm /mnt/t/a /mnt/t/b; echo "own-used-after=$(used /mnt/t)"; umount /mnt/t
echo 64M > /sys/module/ramfs/parameters/limit; echo "set-limit=$(cat /sys/module/ramfs/parameters/limit)"
dd if=/dev/zero of=/tmp/c bs=1M count=128 2>/dev/null; echo "small-limit-bytes=$(wc -c < /tmp/c)"; rm /tmp/c
echo 101% > /sys/module/ramfs/parameters/limit 2>/dev/null; echo "bad-limit-rc=$?"
echo "oom-kills=$(dmesg | grep -c 'invoked oom-killer')"
'''
try:
    run = subprocess.run(ENGINE[:1] + ["exec"] + ENGINE[1:] + ["--no-network", "--", "sh", "-c", SCRIPT],
                         capture_output=True, timeout=300)
    out, err, code = run.stdout.decode(errors="replace").replace("\r", ""), run.stderr.decode(errors="replace"), run.returncode
except subprocess.TimeoutExpired as e:
    out, err, code = (e.stdout or b"").decode(errors="replace"), (e.stderr or b"").decode(errors="replace"), "timeout"

v = dict(re.findall(r"\b([a-z-]+)=(\S+)", out))
num = lambda k: int(v[k]) if v.get(k, "").isdigit() else -1
half_kb = num("memtotal") // 2
check(code == 0, "the engine ends normally", f"exit {code}\n{err[-1500:]}")
check(v.get("limit") == "50%", "the shared limit is half of RAM by default", out)
check(v.get("fill-enospc") == "1" and abs(num("big-bytes") // 1024 + num("tmp-used-before") - half_kb) < half_kb // 50,
      f"a file in /tmp stops with ENOSPC at about half of RAM ({num('big-bytes') >> 20} MiB)", out)
check(abs(num("big-du") * 1024 - num("big-bytes")) < 65536, "du shows the file's pages", out)
check(num("tmp-avail-full") == 0 and v.get("root-rc") == "1", "then nothing is left, for the root's files either", out)
check(num("tmp-used-after") == num("tmp-used-before") and v.get("root-after-rc") == "0",
      "deleting the file gives all of it back", out)
check(num("rewritten-du") == 5 << 10 and num("rewritten-used") == num("tmp-used-before") + (5 << 10)
      and num("rewritten-used-after") == num("tmp-used-before") and v.get("after-rewrite-rc") == "0",
      "a file cut to nothing and written again counts what it holds now", out)
check(0 < num("sparse-du") <= 128 and v.get("sparse-enospc") == "1" and num("tmp-used-sparse-after") == num("tmp-used-before"),
      "reading a sparse file's holes stops with ENOSPC at the limit too, and gives it all back", out)
check(v.get("mount-rc") == "0" and v.get("shown-size") == "1" and num("own-bytes") == 8 << 20,
      "a mount's size= is its own limit, shown in /proc/mounts", out)
check(v.get("shrink-rc") not in (None, "0") and v.get("grow-rc") == "0" and num("grown-bytes") == 8 << 20,
      "a remount changes it, but not below what the mount holds", out)
check(num("own-used-after") == 0, "the mount is empty again after rm", out)
check(v.get("set-limit") == "67108864" and 0 < num("small-limit-bytes") < 64 << 20,
      "ramfs.limit changes at run time", out)
check(v.get("bad-limit-rc") not in (None, "0"), "a limit above 100% is refused", out)
check(v.get("oom-kills") == "0", "the OOM killer is never called", out)

print(f"{'all tmpfs limit checks passed' if not failures else f'{failures} tmpfs limit checks FAILED'}")
sys.exit(1 if failures else 0)
