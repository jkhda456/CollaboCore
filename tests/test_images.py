#!/usr/bin/env python3
"""The boot images over the initramfs are served read-only by the engine and laid over it with
overlayfs (engine/src/image_fs.rs, userspace/rootfs/init.c, kernel/patches/ramfs-xattr.patch),
instead of being unpacked into the guest's memory:

  * the guest sees the same tree as when they are unpacked (COLLABO_IMAGE_SHARE=off): every path
    under /usr and /etc with its type, mode, owner, size, symlink target and contents,
  * they cost the guest's memory only what is read of them,
  * what changes them works: a package removed and made again (an opaque directory: pip
    reinstalling itself), a directory moved out, files renamed, appended to and deleted, extended
    attributes on tmpfs.

  python3 tests/test_images.py   engine/target/release/collabo-core-engine and the build's images,
                                 or the runtime folder COLLABO_RUNTIME names (bin/, app/images/)
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
    PYTHON, TOOLS = images / "python.cpio", images / "tools.cpio"
else:
    ENGINE = [str(ROOT / "engine/target/release/collabo-core-engine"), "--kernel", str(ROOT / "kernel/linux/vmlinux.wasm"),
              "--initramfs", str(ROOT / "userspace/out/initramfs.cpio")]
    PYTHON, TOOLS = ROOT / "python/out/python.cpio", ROOT / "tools/out/tools.cpio"
IMAGES = ["--python-image", str(PYTHON)] + (["--tools-image", str(TOOLS)] if TOOLS.is_file() else [])

failures = 0


def check(ok, what, detail=""):
    global failures
    print(("ok   " if ok else "FAIL ") + what)
    if not ok:
        failures += 1
        if detail:
            print("     " + str(detail)[-2000:].replace("\n", "\n     "))


def guest(script, share=True, timeout=600):
    env = dict(os.environ)
    if not share:
        env["COLLABO_IMAGE_SHARE"] = "off"
    run = subprocess.run(ENGINE[:1] + ["exec"] + ENGINE[1:] + IMAGES + ["--no-network", "--", "sh", "-c", script],
                         capture_output=True, timeout=timeout, env=env)
    return run.returncode, run.stdout.decode(errors="replace"), run.stderr.decode(errors="replace")


if not PYTHON.is_file():
    print(f"skipped: no {PYTHON}")
    sys.exit(0)

# --- The same tree either way -------------------------------------------------------------------
WALK = r'''
import os, stat, zlib
for top in ("/usr", "/etc"):
    for path, dirs, files in os.walk(top):
        dirs.sort()
        for name in sorted(dirs + files):
            full = os.path.join(path, name)
            st = os.lstat(full)
            what = "%s %o %d:%d" % (full, st.st_mode, st.st_uid, st.st_gid)
            if stat.S_ISLNK(st.st_mode):
                what += " -> " + os.readlink(full)
            elif stat.S_ISREG(st.st_mode):
                with open(full, "rb") as f:
                    what += " %d %08x" % (st.st_size, zlib.crc32(f.read()))
            print("W " + what)
'''
listings = {}
for share in (True, False):
    code, out, err = guest(f"python3 -c '{WALK}'; grep -E '^Cached:' /proc/meminfo; mount | grep -c ' type overlay '; true", share)
    lines = [line for line in out.splitlines() if line.startswith("W ")]
    listings[share] = lines
    cached = re.search(r"^Cached:\s+(\d+) kB", out, re.M)
    mode = "served" if share else "unpacked"
    check(code == 0 and len(lines) > 5000, f"{mode}: the guest walked /usr and /etc ({len(lines)} entries)", err[-1500:])
    overlays = re.search(r"^(\d+)$", out, re.M)
    check(overlays is not None and (int(overlays.group(1)) >= 2) == share,
          f"{mode}: /usr and /etc are {'overlays' if share else 'plain'}", out[-500:])
    if share and cached:
        # everything was read just now (the walk), so the cache holds about all of it here; the
        # boot itself is checked below
        check(True, f"served: guest page cache after reading everything: {int(cached.group(1)) // 1024} MB")
served, unpacked = set(listings[True]), set(listings[False])
check(served == unpacked, "the served tree is the unpacked one (type, mode, owner, size, target, contents)",
      "only served:\n" + "\n".join(sorted(served - unpacked)[:15]) + "\nonly unpacked:\n" + "\n".join(sorted(unpacked - served)[:15]))

# --- What it costs the guest --------------------------------------------------------------------
code, out, err = guest("grep -E '^Cached:' /proc/meminfo; python3 -c 'import json, ssl; print(\"python-ok\")'")
cached = re.search(r"^Cached:\s+(\d+) kB", out, re.M)
check(cached is not None and int(cached.group(1)) < 64 * 1024,
      f"served: guest page cache after boot {int(cached.group(1)) // 1024 if cached else '?'} MB (unpacked: ~580 MB)", out[-500:])
check("python-ok" in out, "served: python3 runs", err[-1500:])

# --- Changing what the images hold ---------------------------------------------------------------
CHANGES = r'''
SP=/usr/lib/python3.13/site-packages
pip install --no-index --force-reinstall --no-deps /usr/lib/python3.13/ensurepip/_bundled/pip-*.whl >/tmp/pip.log 2>&1 && pip --version >/dev/null && echo "C pip-reinstalled"
rm -rf $SP/idna && mkdir $SP/idna && echo "MARK = 7" > $SP/idna/__init__.py && python3 -c "import idna; print('C idna', idna.MARK)"
[ "$(ls $SP/idna)" = "__init__.py" ] && echo "C opaque"
mv $SP/h11 /tmp/h11 && [ -f /tmp/h11/__init__.py ] && [ ! -e $SP/h11 ] && echo "C moved-out"
mv /usr/bin/jq /usr/bin/jq2 && jq2 --version >/dev/null && [ ! -e /usr/bin/jq ] && echo "C renamed"
echo "# appended" >> /etc/profile && [ "$(tail -1 /etc/profile)" = "# appended" ] && echo "C appended"
rm /usr/bin/python3 && [ ! -e /usr/bin/python3 ] && python3.13 -c "pass" && echo "C deleted"
python3.13 -c "import os; open('/tmp/x', 'w').close(); os.setxattr('/tmp/x', 'user.k', b'v'); print('C xattr', os.getxattr('/tmp/x', 'user.k').decode())"
dmesg | grep -i overlayfs | sed 's/^/D /'
'''
code, out, err = guest(CHANGES)
said = {line[2:] for line in out.splitlines() if line.startswith("C ")}
for expected, what in [("pip-reinstalled", "pip reinstalls itself (its directories removed and made again)"),
                       ("idna 7", "a package removed and made again is the new one"),
                       ("opaque", "...with nothing of the old one in it (an opaque directory)"),
                       ("moved-out", "a directory of the images moves to /tmp"),
                       ("renamed", "a file of the images is renamed"),
                       ("appended", "a file of the images is appended to"),
                       ("deleted", "a file of the images is deleted"),
                       ("xattr v", "extended attributes on tmpfs")]:
    check(expected in said, what, out[-1500:] + err[-800:])
warnings = [line for line in out.splitlines() if line.startswith("D ")]
check(not warnings, "overlayfs has nothing to warn about (xattrs, RENAME_WHITEOUT on the upper layer)", "\n".join(warnings))

print("all image checks passed" if failures == 0 else f"{failures} image check(s) failed")
sys.exit(1 if failures else 0)
