#!/usr/bin/env python3
"""A trap in a guest program (tests/guest-trap.c) ends that program with SIGSEGV and the machine
goes on (addons/mod/0009: the native engine); a thread's trap ends its whole process, so the
threads waiting on it do not wait forever (addons/mod/0010: the kernel). On both hosts: the native
engine and the browser's (Node, userspace/test-boot.sh).

  python3 tests/test_traps.py     engine/target/release/collabo-core-engine and the build's images,
                                  or the runtime folder COLLABO_RUNTIME names (bin/, app/images/)
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
    INITRAMFS = images / "initramfs.cpio"
else:
    ENGINE = [str(ROOT / "engine/target/release/collabo-core-engine"), "--kernel", str(ROOT / "kernel/linux/vmlinux.wasm"),
              "--initramfs", str(ROOT / "userspace/out/initramfs.cpio")]
    INITRAMFS = ROOT / "userspace/out/initramfs.cpio"

# mode -> what the program says before it traps (None: nothing)
TRAPS = {
    "unreachable": None,
    "indirect": None,
    "oob": None,
    "stack": None,
    "thread": "thread trapping",
    "main-with-thread": "main trapping",
    "signal": "handler trapping",
    "siginfo": "siginfo handler trapping",
}
# what the engine reports for each kind (the native engine's stderr)
REPORTS = ["`unreachable` instruction executed", "indirect call type mismatch", "out of bounds memory access",
           "call stack exhausted", "the guest program trapped in a signal handler"]

failures = 0


def check(ok, what, detail=""):
    global failures
    print(("ok   " if ok else "FAIL ") + what)
    if not ok:
        failures += 1
        if detail:
            print("     " + str(detail)[-2000:].replace("\n", "\n     "))


def script(seconds):
    # A program that should have ended but waits is killed by `timeout` (rc 143): the hang 0010 fixes.
    modes = " ".join(["ok", *TRAPS])
    return f'for m in {modes}; do timeout {seconds} guest-trap $m; echo "rc[$m]=$?"; done; echo machine-alive'


def check_run(host, out):
    out = out.replace("\r", "")
    rc = dict(re.findall(r"^rc\[([a-z-]+)\]=(\d+)$", out, re.M))
    check(rc.get("ok") == "0" and "joined 42" in out, f"{host}: a thread that returns is joined, the program exits 0", out)
    for mode, says in TRAPS.items():
        detail = f"rc {rc.get(mode)} (139: SIGSEGV; 143: killed by timeout, it hung)\n{out}"
        check(rc.get(mode) == "139", f"{host}: {mode}: ends with SIGSEGV", detail)
        if says:
            check(says in out, f"{host}: {mode}: ran up to its trap", out)
    check("woke up" not in out and "handler returned" not in out, f"{host}: nothing ran past a trap", out)
    # a line of its own: the Node host's console also echoes the command typed
    check(re.search(r"^machine-alive$", out, re.M) is not None, f"{host}: the machine went on", out)


work = Path(tempfile.mkdtemp(prefix="collabo-traps-"))
program, overlay = work / "guest-trap.wasm", work / "traps.cpio"
subprocess.run([str(ROOT / "userspace/bin/wasm-cc"), "-O2", "-Wl,--import-memory", "-Wl,--max-memory=4294967296",
                "-Wl,--shared-memory", "-Wl,--export-table", str(ROOT / "tests/guest-trap.c"), "-o", str(program)],
               check=True)
subprocess.run([sys.executable, str(ROOT / "userspace/mkinitramfs.py"), "--overlay", "-o", str(overlay),
                "--file", f"usr/bin/guest-trap={program}"], check=True, capture_output=True)

# --- The native engine ---------------------------------------------------------------------
run = subprocess.run(ENGINE[:1] + ["exec"] + ENGINE[1:] + ["--initramfs", str(overlay), "--no-network", "--cwd", "/tmp",
                     "--", "sh", "-c", script(30)], capture_output=True, timeout=600)
out, err = run.stdout.decode(errors="replace"), run.stderr.decode(errors="replace")
check(run.returncode == 0, "native: the engine ends normally (no guest panic)", f"exit {run.returncode}\n{err[-1500:]}")
check_run("native", out)
reports = err.count("a guest program trapped and was ended (SIGSEGV)")
check(reports == len(TRAPS), f"native: each trap is reported on the host's stderr ({len(TRAPS)})", f"{reports}\n{err[-1500:]}")
for kind in REPORTS:
    check(kind in err, f"native: reported: {kind}", err[-1500:])

# --- The browser's host (Node) ---------------------------------------------------------------
if runtime:
    print("(COLLABO_RUNTIME: the Node host is not part of a runtime folder; skipped)")
else:
    env = dict(os.environ, EXTRA_CPIO=str(overlay), TEST_TIMEOUT_MS="600000")
    boot = subprocess.run(["bash", str(ROOT / "userspace/test-boot.sh"), str(INITRAMFS), script(60)],
                          capture_output=True, env=env, timeout=900)
    out = boot.stdout.decode(errors="replace") + boot.stderr.decode(errors="replace")
    check(boot.returncode == 0, "node: the boot script ends normally (no guest panic)", f"exit {boot.returncode}\n{out[-1500:]}")
    check_run("node", out)

print(f"{'all trap checks passed' if not failures else f'{failures} trap checks FAILED'}")
sys.exit(1 if failures else 0)
