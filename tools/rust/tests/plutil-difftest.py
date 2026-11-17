#!/usr/bin/env python3
"""plutil (collabo-data's applet) against two other property list implementations.

  plutil-difftest.py files OUR_PLUTIL PLISTUTIL FILE_OR_DIR...
      every file through our -convert xml1 and -convert binary1; libplist's plistutil (built from
      the libplist sources: ./configure --without-cython && make) reads the original and both of
      ours, and its key-sorted XML (plistutil -s -f xml) of the three must be the same. Where one
      side reads a file and the other does not, it is listed (CF and libplist do differ there).

  plutil-difftest.py random OUR_PLUTIL [COUNT [SEED]]
      random property lists written by Python's plistlib (binary and XML) go through our
      -convert xml1, binary1 and json, and plistlib reads ours back: the same values, and our
      binary output is CF's layout (plistlib checks the trailer and every reference).

Exit status 1 when something differs.
"""
import datetime
import json
import os
import plistlib
import random
import subprocess
import sys
import tempfile

failures = 0

# Files where CF/Foundation and libplist disagree, and plutil follows CF.
KNOWN = {
    "empty_keys.plist": "CF reads no comment inside <key> (its string parser takes '<!' for CDATA)",
    "int64_min_max.json": "Foundation counts '-' among the digits: -9223372036854775808 is no Int64 but a decimal, written as <real>",
}


def report(ok, what, detail=""):
    global failures
    if not ok:
        failures += 1
        print(f"FAIL {what}" + (f"\n     {detail}" if detail else ""))


def run(argv, stdin=None):
    p = subprocess.run(argv, input=stdin, capture_output=True, timeout=60)
    return p.returncode, p.stdout, p.stderr


def files_mode(ours, plistutil, paths):
    tmp = tempfile.mkdtemp(prefix="plutil-diff-")
    files = []
    for p in paths:
        if os.path.isdir(p):
            files += sorted(os.path.join(p, f) for f in os.listdir(p) if os.path.isfile(os.path.join(p, f)))
        else:
            files.append(p)
    agree = only_ours = only_ref = both_refuse = 0
    for f in files:
        name = os.path.basename(f)
        if name in KNOWN:
            print(f"known        {name}: {KNOWN[name]}")
            continue
        rc, ref, _ = run([plistutil, "-s", "-f", "xml", "-i", f])
        ox, ob = os.path.join(tmp, name + ".xml"), os.path.join(tmp, name + ".bin")
        rx, _, ex = run([ours, "-convert", "xml1", "-o", ox, f])
        rb, _, eb = run([ours, "-convert", "binary1", "-o", ob, f])
        if rc != 0 and rx != 0:
            both_refuse += 1
            print(f"both refuse  {name}: {ex.decode(errors='replace').strip()}")
            continue
        if rc != 0:
            only_ours += 1
            print(f"ours only    {name}")
            continue
        if rx != 0:
            only_ref += 1
            print(f"libplist only {name}: {ex.decode(errors='replace').strip()}")
            continue
        report(rb == 0, f"{name}: binary1 failed where xml1 did not", eb.decode(errors="replace"))
        _, back_xml, e1 = run([plistutil, "-s", "-f", "xml", "-i", ox])
        _, back_bin, e2 = run([plistutil, "-s", "-f", "xml", "-i", ob])
        same = back_xml == ref and back_bin == ref
        report(same, f"{name}: libplist reads ours differently", (e1 + e2).decode(errors="replace")[:300] or "(the dumps differ)")
        if same:
            agree += 1
    print(f"\nfiles: {agree} agree, {both_refuse} both refuse, {only_ref} libplist only, {only_ours} ours only, {failures} failed")


def random_value(r, depth=0):
    kinds = ["str", "int", "float", "bool", "date", "data"] + (["array", "dict"] * 2 if depth < 4 else [])
    k = r.choice(kinds)
    if k == "str":
        alphabet = "abcXYZ019 <>&\"'/\\\n\té€😀"
        return "".join(r.choice(alphabet) for _ in range(r.randrange(0, 20)))
    if k == "int":
        return r.choice([0, 1, -1, 255, 256, 65535, 65536, 2**31, -2**31, 2**32, 2**63 - 1, -2**63, 2**64 - 1, r.randrange(-2**63, 2**63)])
    if k == "float":
        return r.choice([0.0, 0.5, -2.25, 1e20, 1.5e-7, 3.141592653589793, r.uniform(-1e6, 1e6)])
    if k == "bool":
        return r.choice([True, False])
    if k == "date":
        return datetime.datetime(2001, 1, 1) + datetime.timedelta(seconds=r.randrange(-2**31, 2**31))
    if k == "data":
        return bytes(r.randrange(256) for _ in range(r.choice([0, 1, 2, 3, 14, 15, 16, 57, 300])))
    if k == "array":
        return [random_value(r, depth + 1) for _ in range(r.choice([0, 1, 3, 14, 15, 16]))]
    return {"".join(r.choice("kKé_09") for _ in range(r.randrange(1, 6))): random_value(r, depth + 1) for _ in range(r.choice([0, 1, 3, 15, 16]))}


def normal(v):
    """What a value is after a round trip: dates to the second (XML keeps no fraction)."""
    if isinstance(v, dict):
        return {k: normal(x) for k, x in v.items()}
    if isinstance(v, list):
        return [normal(x) for x in v]
    if isinstance(v, datetime.datetime):
        return v.replace(microsecond=0, tzinfo=None)
    return v


def random_mode(ours, count, seed):
    r = random.Random(seed)
    tmp = tempfile.mkdtemp(prefix="plutil-random-")
    src = os.path.join(tmp, "in.plist")
    for i in range(count):
        top = random_value(r)
        if not isinstance(top, (dict, list)) and r.random() < 0.5:
            top = {"v": top}
        fmt = r.choice([plistlib.FMT_BINARY, plistlib.FMT_XML])
        with open(src, "wb") as f:
            f.write(plistlib.dumps(top, fmt=fmt, sort_keys=False))
        want = normal(top)
        for target in ["xml1", "binary1"]:
            rc, out, err = run([ours, "-convert", target, "-o", "-", src])
            if rc != 0:
                report(False, f"case {i}: -convert {target} failed", err.decode(errors="replace"))
                continue
            try:
                got = normal(plistlib.loads(out))
            except Exception as e:  # noqa: BLE001
                report(False, f"case {i}: plistlib cannot read our {target}", repr(e))
                continue
            report(got == want, f"case {i}: {target} round trip differs", f"{json.dumps(want, default=str)[:300]}\n     {json.dumps(got, default=str)[:300]}")
        if not _has(top, (datetime.datetime, bytes)):
            rc, out, err = run([ours, "-convert", "json", "-o", "-", src])
            ok = rc == 0 and json.loads(out) == json.loads(json.dumps(top))
            report(ok, f"case {i}: json differs", (out + err).decode(errors="replace")[:300])
    print(f"random: {count} cases (seed {seed}), {failures} failed")


def _has(v, types):
    if isinstance(v, types):
        return True
    if isinstance(v, dict):
        return any(_has(x, types) for x in v.values())
    if isinstance(v, list):
        return any(_has(x, types) for x in v)
    return False


if __name__ == "__main__":
    if len(sys.argv) >= 4 and sys.argv[1] == "files":
        files_mode(sys.argv[2], sys.argv[3], sys.argv[4:])
    elif len(sys.argv) >= 3 and sys.argv[1] == "random":
        random_mode(sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 300, int(sys.argv[4]) if len(sys.argv) > 4 else 1)
    else:
        print(__doc__)
        sys.exit(2)
    sys.exit(1 if failures else 0)
