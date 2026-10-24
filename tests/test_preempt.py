#!/usr/bin/env python3
"""A program running its own code without syscalls is still interrupted (the engine's tick:
engine/src/user.rs on_epoch, the kernel's user_interrupt, kernel/patches/user-interrupt.patch):
SIGKILL and SIGTERM end it, its SIGALRM handler runs, a loop of calls without a loop is stopped
too, and more such loops than CPUs leave the rest of the guest running. tests/busy-loop.c.

  python3 tests/test_preempt.py   engine/target/release/collabo-core-engine and the build's images,
                                  or the runtime folder COLLABO_RUNTIME names (bin/, app/images/)
"""
import os
import re
import subprocess
import sys
import tempfile
import time
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
            print("     " + str(detail)[-2000:].replace("\n", "\n     "))


work = Path(tempfile.mkdtemp(prefix="collabo-preempt-"))
program, overlay = work / "busy-loop.wasm", work / "busy.cpio"
subprocess.run([str(ROOT / "userspace/bin/wasm-cc"), "-O2", "-Wl,--import-memory", "-Wl,--max-memory=4294967296",
                "-Wl,--shared-memory", "-Wl,--export-table", str(ROOT / "tests/busy-loop.c"), "-o", str(program)],
               check=True)
subprocess.run([sys.executable, str(ROOT / "userspace/mkinitramfs.py"), "--overlay", "-o", str(overlay),
                "--file", f"usr/bin/busy-loop={program}"], check=True, capture_output=True)

# Without the tick each of these never ends (not even the engine: the outer timeout fires).
SCRIPT = r'''
timeout -s KILL 2 busy-loop spin; echo "rc[kill]=$?"
timeout 2 busy-loop spin; echo "rc[term]=$?"
timeout -s KILL 2 busy-loop recurse; echo "rc[recurse]=$?"
busy-loop alarm; echo "rc[alarm]=$?"
busy-loop spin & a=$!; busy-loop spin & b=$!; busy-loop spin & c=$!
sleep 1
i=0; while [ $i -lt 2000 ]; do i=$((i+1)); done; echo "shell ran $i with 3 spinning on 2 cpus"
kill -9 $a $b $c; wait; echo "rc[spinners]=done"
'''
started = time.monotonic()
try:
    run = subprocess.run(ENGINE[:1] + ["exec"] + ENGINE[1:] + ["--initramfs", str(overlay), "--cpus", "2", "--no-network",
                         "--", "sh", "-c", SCRIPT], capture_output=True, timeout=180)
    out, err, code = run.stdout.decode(errors="replace").replace("\r", ""), run.stderr.decode(errors="replace"), run.returncode
except subprocess.TimeoutExpired as e:
    out, err, code = (e.stdout or b"").decode(errors="replace"), (e.stderr or b"").decode(errors="replace"), "timeout"
elapsed = time.monotonic() - started
check(code == 0, "the engine ends normally", f"exit {code}\n{err[-1500:]}")
rc = dict(re.findall(r"^rc\[([a-z]+)\]=(\S+)$", out, re.M))
check(rc.get("kill") == "137", "SIGKILL ends a loop without syscalls", out)
check(rc.get("term") == "143", "SIGTERM ends it (default action)", out)
check(rc.get("recurse") == "137", "a call tree without loops is interrupted too (checks at function entry)", out)
check(rc.get("alarm") == "4" and "alarm handled" in out, "its SIGALRM handler runs in the loop", out)
check("shell ran 2000 with 3 spinning on 2 cpus" in out and rc.get("spinners") == "done",
      "more spinning programs than CPUs: the shell still runs", out)
check(elapsed < 120, f"all within the time the timeouts allow ({elapsed:.0f} s)")

# --no-preempt: no tick, the machine otherwise the same
run = subprocess.run(ENGINE[:1] + ["exec"] + ENGINE[1:] + ["--no-preempt", "--no-network", "--", "sh", "-c", "echo plain"],
                     capture_output=True, timeout=180)
check(run.returncode == 0 and b"plain" in run.stdout, "--no-preempt boots and runs a command", run.stderr.decode(errors="replace")[-800:])

print(f"{'all preemption checks passed' if not failures else f'{failures} preemption checks FAILED'}")
sys.exit(1 if failures else 0)
