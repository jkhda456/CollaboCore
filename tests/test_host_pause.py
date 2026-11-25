#!/usr/bin/env python3
"""The host stopping the machine is not a stall (kernel/patches/wasm-watchdog-host-pause.patch):
the engine is stopped (SIGSTOP, as a laptop's sleep does to it) for longer than RCU's stall timeout
while the guest keeps RCU busy; on resume no stall is reported, and the report that would have been
is dropped with a line in dmesg. A real stall is still reported: without preemption a loop pinned
to cpu 1 never gives it back, while cpu 0 goes on ticking. tests/busy-loop.c.

  python3 tests/test_host_pause.py   engine/target/release/collabo-core-engine and the build's images,
                                     or the runtime folder COLLABO_RUNTIME names (bin/, app/images/)
"""
import os
import signal
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

PAUSES, PAUSE_S = 2, 25   # RCU_CPU_STALL_TIMEOUT is 21 s

failures = 0


def check(ok, what, detail=""):
    global failures
    print(("ok   " if ok else "FAIL ") + what)
    if not ok:
        failures += 1
        if detail:
            print("     " + str(detail)[-2000:].replace("\n", "\n     "))


work = Path(tempfile.mkdtemp(prefix="collabo-pause-"))
program, overlay = work / "busy-loop.wasm", work / "busy.cpio"
subprocess.run([str(ROOT / "userspace/bin/wasm-cc"), "-O2", "-Wl,--import-memory", "-Wl,--max-memory=4294967296",
                "-Wl,--shared-memory", "-Wl,--export-table", str(ROOT / "tests/busy-loop.c"), "-o", str(program)],
               check=True)
subprocess.run([sys.executable, str(ROOT / "userspace/mkinitramfs.py"), "--overlay", "-o", str(overlay),
                "--file", f"usr/bin/busy-loop={program}"], check=True, capture_output=True)

# Processes coming and going and files made and removed: grace periods one after another, so the
# stops find one in progress.
seconds = PAUSES * (PAUSE_S + 3) + 10
SCRIPT = r'''
echo READY; end=$(( $(date +%s) + ''' + str(seconds) + r''' ))
while [ $(date +%s) -lt $end ]; do touch /tmp/a; rm /tmp/a; sleep 0.05; done
echo "stalls=$(dmesg | grep -c 'INFO: rcu')"; echo "dropped=$(dmesg | grep -c 'stall report dropped')"
dmesg | grep 'stall report dropped'; true
'''
out_path = work / "pause.out"
with open(out_path, "wb") as out:
    engine = subprocess.Popen(ENGINE[:1] + ["exec"] + ENGINE[1:] + ["--cpus", "2", "--no-network", "--", "sh", "-c", SCRIPT],
                              stdout=out, stderr=subprocess.STDOUT)
    started = time.monotonic()
    while b"READY" not in out_path.read_bytes() and engine.poll() is None and time.monotonic() - started < 120:
        time.sleep(0.2)
    for _ in range(PAUSES):
        time.sleep(3)
        engine.send_signal(signal.SIGSTOP)
        time.sleep(PAUSE_S)
        engine.send_signal(signal.SIGCONT)
    try:
        code = engine.wait(timeout=seconds + 120)
    except subprocess.TimeoutExpired:
        engine.kill()
        code = "timeout"
text = out_path.read_text(errors="replace").replace("\r", "")
values = dict(line.split("=", 1) for line in text.splitlines() if line.startswith(("stalls=", "dropped=")))
check(code == 0, "the engine ends normally after the stops", f"exit {code}\n{text[-1500:]}")
check(values.get("stalls") == "0" and "INFO: rcu" not in text, f"{PAUSES} stops of {PAUSE_S} s: no RCU stall reported",
      text[text.find("INFO: rcu") - 200:][:2500] if "INFO: rcu" in text else text[-1500:])
# A stop that finds no grace period in progress has nothing to drop: one of them is enough.
check(int(values.get("dropped", "0")) >= 1, "the report a stop would have caused is dropped, with a line in dmesg", text[-1500:])

# A real stall: cpu 1 held for good by a loop without syscalls (no preemption), cpu 0 ticking. The
# shell's own children soon wait for the grace period that never ends, so the machine is killed.
GENUINE = r'''
taskset -c 1 busy-loop spin &
while :; do touch /tmp/a; rm /tmp/a; sleep 0.1; done
'''
try:
    run = subprocess.run(ENGINE[:1] + ["exec"] + ENGINE[1:] + ["--initramfs", str(overlay), "--cpus", "2", "--no-network",
                         "--no-preempt", "--", "taskset", "-c", "0", "sh", "-c", GENUINE], capture_output=True, timeout=45)
    text = (run.stdout + run.stderr).decode(errors="replace")
except subprocess.TimeoutExpired as e:
    text = ((e.stdout or b"") + (e.stderr or b"")).decode(errors="replace")
text = text.replace("\r", "")
# (A dropped report prints nothing: the console's stall lines are the report itself.)
check("rcu_sched detected stalls" in text and "busy-loop" in text,
      "a cpu held by a program for good is still reported (by the cpu that ticks)", text[-1500:])

print("all host-pause checks passed" if failures == 0 else f"{failures} host-pause check(s) failed")
sys.exit(1 if failures else 0)
