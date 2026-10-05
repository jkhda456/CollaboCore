#!/usr/bin/env python3
"""The engine's logs on a real guest: the network log (--log-network), the exec log (--log-exec,
--log-exec-kinds), both in one file (--log-file), their line limit, escaping, size limit and rotation.

  python3 tests/test_logs.py      engine/target/release/collabo-core-engine and the build's images,
                                  or the runtime folder COLLABO_RUNTIME names (bin/, app/images/)
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LINE_LIMIT = 4096
STAMP = 21  # "2026-10-05T11:20:13Z "

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
            print("     " + str(detail)[:2000].replace("\n", "\n     "))


def guest(script, *options):
    """Runs `sh -c script` in the guest (no network) and returns its stdout and stderr."""
    run = subprocess.run(ENGINE[:1] + ["exec"] + ENGINE[1:] + ["--no-network", *options, "--cwd", "/tmp", "--", "sh", "-c", script],
                         capture_output=True, timeout=300)
    return run.stdout.decode(errors="replace"), run.stderr.decode(errors="replace")


def lines(path):
    return path.read_text(encoding="utf-8").splitlines() if path.exists() else []


work = Path(tempfile.mkdtemp(prefix="collabo-logs-"))

# --- Nothing by default -----------------------------------------------------------------------
out, err = guest("cat /proc/cmdline; ls /bin/busybox >/dev/null")
check("collabo.execlog" not in out, "without log options the kernel reports no execs", out)
check("[exec]" not in err and "[run]" not in err, "and nothing is logged", err[-500:])

# --- Both logs to files -----------------------------------------------------------------------
network_log, command_log = work / "logs/network.log", work / "logs/commands.log"
script = r"""
cd /etc && ls / >/dev/null
/bin/busybox echo "$(head -c 10000 /dev/zero | tr '\0' a)" | wc -c
/bin/busybox true "$(head -c 300000 /dev/zero | tr '\0' b)"; echo e2big=$?
/bin/busybox echo "$(printf 'one\ntwo\033[2J')" >/dev/null
hfetch "http://blocked.example/$(head -c 8000 /dev/zero | tr '\0' u)" >/dev/null 2>&1
hfetch "http://blocked.example/short" >/dev/null 2>&1
exit 3
"""
out, err = guest(script, "--log-network", str(network_log), "--log-exec", str(command_log))
commands, requests = lines(command_log), lines(network_log)
check("e2big=" in out and "e2big=0" not in out, "a 300 KB argv fails in the guest", out)
check(all(re.match(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ \[", line) for line in commands + requests),
      "every line starts with its UTC time and a tag", (commands + requests)[:3])
check(all(len(line.encode()) <= STAMP + LINE_LIMIT for line in commands + requests),
      f"no line is longer than {LINE_LIMIT} bytes after its time", max(map(len, commands + requests), default=0))
check(not any("[network]" in line for line in commands), "the command log has no network lines")
check(not any("[exec]" in line or "[run]" in line for line in requests), "the network log has no command lines")

run = [line[STAMP:] for line in commands if "[run]" in line]
check(len(run) == 2 and run[0].startswith("[run] cwd=/tmp: sh -c ") and run[1].startswith("[run] exit=3 after "),
      "[run]: the command exec runs, then its exit status", run)
check(any("cwd=/etc file=/bin/ls: ls /" in line for line in commands), "[exec]: a program with its cwd and argv", commands[-12:])
check(any("file=/sbin/init" in line or "file=/init" in line for line in commands), "[exec]: boot's programs too")
long = [line for line in commands if "file=/bin/busybox: /bin/busybox echo aaaa" in line]
check(len(long) == 1 and re.search(r" …\[\+(\d+) bytes\]$", long[0]) is not None,
      "a 10 KB argument is cut, the line saying how much is missing", long[0][-80:] if long else commands)
if long:
    shown = len(long[0].encode()) - STAMP - len(re.search(r" …\[\+\d+ bytes\]$", long[0]).group(0).encode())
    missing = int(re.search(r"\+(\d+) bytes\]$", long[0]).group(1))
    # The guest's record: "/etc\0/bin/busybox\0/bin/busybox\0echo\0" and 10000 a's and a NUL.
    check(missing > 10000 - LINE_LIMIT and missing < 10000, "and the count is about the part left out", (shown, missing))
check(any("file=/bin/busybox failed: E2BIG (argument list too long)" in line for line in commands),
      "[exec]: a failed exec, with the Linux errno", [l for l in commands if "failed" in l])
check(any(r"'one\ntwo\x1b[2J'" in line for line in commands), "control characters are escaped, not written",
      [l for l in commands if "one" in l])
blocked = [line[STAMP:] for line in requests if "blocked" in line]
check(any(line.startswith("[network] blocked GET http://blocked.example/short") for line in blocked),
      "[network]: a request the policy refused", blocked)
check(any(re.search(r"http://blocked.example/u+ …\[\+\d+ bytes\]: blocked by the network policy", line) for line in blocked),
      "[network]: an 8 KB URL is cut, and the reason after it stays", [l[-120:] for l in blocked])

# --- Both in one file -------------------------------------------------------------------------
both = work / "logs/all.log"
guest("ls / >/dev/null; hfetch http://blocked.example/both >/dev/null 2>&1", "--log-file", str(both))
tags = {line[STAMP:].split(" ")[0] for line in lines(both)}
check(tags == {"[run]", "[exec]", "[network]"}, "--log-file: the network and exec logs in one file", tags)

# --- Size limit and rotation ------------------------------------------------------------------
many = "for n in $(seq 1 40); do /bin/busybox true $n; done"
kept = work / "kept/commands.log"
guest(many, "--log-exec", str(kept), "--log-max-size", "2K")
size = kept.stat().st_size if kept.exists() else -1
check(0 < size <= 2048, "--log-max-size 2K: the file stays within 2 KiB", size)
check(lines(kept) and "[log] reached its size limit (2048 bytes): later lines are dropped" in lines(kept)[-1],
      "and ends saying later lines were dropped", lines(kept)[-1:] if kept.exists() else None)
check(not Path(str(kept) + ".1").exists(), "without --log-rotate nothing is rotated")

rotated = work / "rotated/commands.log"
guest(many, "--log-exec", str(rotated), "--log-max-size", "2K", "--log-rotate", "2")
files = sorted(p.name for p in rotated.parent.iterdir())
check(files == ["commands.log", "commands.log.1", "commands.log.2"], "--log-rotate 2: two old files are kept", files)
check(all(p.stat().st_size <= 2048 for p in rotated.parent.iterdir()), "each within the limit",
      [(p.name, p.stat().st_size) for p in rotated.parent.iterdir()])
check(any("[run] exit=0" in line for line in lines(rotated)), "the newest lines are in the file itself", lines(rotated)[-2:])

# --- One kind, to stderr ----------------------------------------------------------------------
out, err = guest("ls / >/dev/null; hfetch http://blocked.example/x >/dev/null 2>&1",
                 "--log-exec", "-", "--log-exec-kinds", "exec", "--log-requests")
check("[exec] " in err and "file=/bin/ls: ls /" in err, "--log-exec - --log-exec-kinds exec: the programs on stderr", err[-400:])
check("[run]" not in err, "and not the command exec runs")
check("[network] blocked GET http://blocked.example/x" in err, "--log-requests: the network log on stderr", err[-400:])

if failures:
    print(f"FAIL: {failures} log checks (files in {work})")
    sys.exit(1)
shutil.rmtree(work, ignore_errors=True)
print("all log checks passed")
