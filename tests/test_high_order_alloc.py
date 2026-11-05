#!/usr/bin/env python3
"""A kernel allocation of more than one page does not go to the OOM killer while clean page cache
is left (kernel/patches/nommu-high-order-alloc.patch): the guest has no compaction, so once the
page cache has filled memory the free memory is single 64 KiB pages, and a 256 KiB kmalloc used to
call the OOM killer after one round of reclaim, with gigabytes of cache left. exec, which reads a
program through such a buffer, then killed something on every call. tests/high-order-alloc.c sets
that state up: memory filled by tmpfs files and page cache side by side, the files deleted, every
free 256 KiB block taken.

The guest's memory grows to its full 4 GiB here (host RSS too), for about ten seconds.

  python3 tests/test_high_order_alloc.py   engine/target/release/collabo-core-engine and the build's
                                           images, or the runtime folder COLLABO_RUNTIME names
"""
import os
import re
import subprocess
import sys
import tempfile
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


work = Path(tempfile.mkdtemp(prefix="collabo-high-order-"))
program, overlay, share = work / "high-order-alloc.wasm", work / "high-order-alloc.cpio", work / "share"
subprocess.run([str(ROOT / "userspace/bin/wasm-cc"), "-O2", "-Wl,--import-memory", "-Wl,--max-memory=4294967296",
                "-Wl,--shared-memory", "-Wl,--export-table", str(ROOT / "tests/high-order-alloc.c"), "-o", str(program)],
               check=True)
subprocess.run([sys.executable, str(ROOT / "userspace/mkinitramfs.py"), "--overlay", "-o", str(overlay),
                "--file", f"usr/bin/high-order-alloc={program}"], check=True, capture_output=True)
share.mkdir()
with open(share / "cache", "wb") as f:  # sparse: the guest caches it, the host stores nothing
    f.truncate(4 << 30)

# $7.. of a /proc/buddyinfo line: free blocks of order 2 (256 KiB) and up, counted in 256 KiB
SCRIPT = r'''
ulimit -n 4096
# its files are half of memory, past the shared tmpfs limit (tests/test_tmpfs_limit.py): lifted here
echo 0 > /sys/module/ramfs/parameters/limit 2>/dev/null
blocks() { set -- $(while read -r l; do echo "$l"; done < /proc/buddyinfo)
           echo $(( $7 + $8*2 + $9*4 + ${10}*8 + ${11}*16 + ${12}*32 + ${13}*64 + ${14}*128 + ${15}*256 )); }
free_kb() { while read -r k v u; do [ "$k" = MemFree: ] && echo $v; done < /proc/meminfo; }
mkdir /tmp/f
high-order-alloc fragment /tmp/f /share/cache $(( $(free_kb) / 64 / 2 + 500 ))
echo "free-kb=$(free_kb)"
free=$(blocks); echo "free-blocks=$free"
high-order-alloc hold $((free + 64)) > /tmp/held &
i=0; while [ ! -s /tmp/held ] && [ $i -lt 150 ]; do sleep 0.2; i=$((i+1)); done
read -r held < /tmp/held; echo "$held asked=$((free + 64))"
echo "left-blocks=$(blocks)"
i=0; ok=0; while [ $i -lt 20 ]; do i=$((i+1)); sh -c 'exit 7'; [ $? = 7 ] && ok=$((ok+1)); done
echo "exec-ok=$ok"
echo "oom-kills=$(dmesg | grep -c 'invoked oom-killer')"
'''
try:
    run = subprocess.run(ENGINE[:1] + ["exec"] + ENGINE[1:] + ["--initramfs", str(overlay), "--no-network",
                         "--mount", f"{share}:/share", "--", "sh", "-c", SCRIPT], capture_output=True, timeout=300)
    out, err, code = run.stdout.decode(errors="replace").replace("\r", ""), run.stderr.decode(errors="replace"), run.returncode
except subprocess.TimeoutExpired as e:
    out, err, code = (e.stdout or b"").decode(errors="replace"), (e.stderr or b"").decode(errors="replace"), "timeout"
finally:
    (share / "cache").unlink()

values = {k: int(v) for k, v in re.findall(r"^([a-z-]+)=(\d+)", out, re.M)}
values.update({k: int(v) for k, v in re.findall(r"\b(held|asked)=(\d+)", out)})
print(f"     free {values.get('free-kb', 0) // 1024} MB in single pages, {values.get('free-blocks')} free 256 KiB blocks; "
      f"held {values.get('held')} of {values.get('asked')}; {values.get('left-blocks')} left")
check(code == 0, "the engine ends normally", f"exit {code}\n{err[-1500:]}")
check(values.get("free-kb", 0) > 1 << 20 and values.get("free-blocks", 1 << 30) < 1024,
      "set up: over 1 GiB free, all of it in pieces smaller than 256 KiB but a few", out)
check(values.get("held") is not None and values.get("held") == values.get("asked"),
      "256 KiB allocations beyond the free blocks are made by reclaiming page cache", out)
check(values.get("exec-ok") == 20, "programs start with no free 256 KiB block", out)
check(values.get("oom-kills") == 0, "the OOM killer is never called", out)

print(f"{'all high-order allocation checks passed' if not failures else f'{failures} high-order allocation checks FAILED'}")
sys.exit(1 if failures else 0)
