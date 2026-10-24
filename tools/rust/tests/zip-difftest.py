"""Differential tests of our zip (collabo-archive) against Info-ZIP's zip 3.0: each case runs its
steps in a fresh copy of the same files with each, and compares exit statuses, messages and the
archives left behind, record by record (local headers, data descriptors, central headers, end
records, with their extra fields). The deflate data is zlib's rather than Info-ZIP's own, so what
depends on its bytes is masked: a deflated entry's compressed size, the offsets after it, the ratio
in "(deflated n%)".

    python3 zip-difftest.py OUR_DIR REF_DIR

Each directory holds a `zip` (OUR_DIR's a link to collabo-archive). The reference is Info-ZIP's
zip30.tar.gz built for this machine: `make -f unix/Makefile generic` (with a modern compiler,
CC="cc -std=gnu89" and the implicit-declaration errors turned back into warnings). It has no bzip2,
so -Z bzip2 is left out. Known, intended differences are listed in KNOWN."""
import os, re, shutil, struct, subprocess, sys, tempfile
OURS, REF = sys.argv[1], sys.argv[2]
T0 = 1_780_000_000  # the fixture's times (2026-05-28)

def setup(d):
    os.makedirs(f"{d}/d/sub")
    open(f"{d}/d/a.txt", "w").write("".join(f"{i}\n" for i in range(1, 2001)))
    open(f"{d}/d/sub/b.txt", "w").write("hi\n")
    open(f"{d}/d/empty", "w").close()
    open(f"{d}/d/rand.bin", "wb").write(bytes((i * 7919 + (i >> 3) * 104729) % 251 for i in range(3000)))
    open(f"{d}/big.bin", "wb").write(bytes((i * 2654435761 >> 13) & 0xff for i in range(200_000)))
    open(f"{d}/text.txt", "w").write("hello world\n" * 3000)
    open(f"{d}/crlf.txt", "wb").write(b"one\r\ntwo\r\n")
    open(f"{d}/in.zip", "wb").write(b"PK\x05\x06" + bytes(18))
    open(f"{d}/한글.txt", "w").write("한\n")
    os.symlink("a.txt", f"{d}/d/link")
    os.symlink("nowhere", f"{d}/dang")
    os.mkfifo(f"{d}/fifo")
    for root, dirs, files in os.walk(d, topdown=False):
        for n in files + dirs:
            p = os.path.join(root, n)
            os.utime(p, (T0 + 7, T0 + 3), follow_symlinks=False)
        os.utime(root, (T0 + 7, T0 + 3))

# (name, [(argv, stdin), ...])
def one(*a, stdin=b""):
    return [(list(a), stdin)]

CASES = [
    ("add -r", one("-r", "t", "d")),
    ("add files", one("t.zip", "d/a.txt", "d/sub/b.txt", "text.txt", "big.bin")),
    ("add again (update all)", [(["-r", "t", "d"], b""), (["-r", "t", "d"], b"")]),
    ("-u nothing newer", [(["t", "d/a.txt"], b""), (["-u", "t", "d/a.txt"], b"")]),
    ("-u adds new", [(["t", "d/a.txt"], b""), (["-u", "t", "d/a.txt", "text.txt"], b"")]),
    ("-u newer", [(["t", "d/a.txt", "text.txt"], b""), (["touch", "-d", "2030-01-01", "text.txt"], None), (["-u", "t", "d/a.txt", "text.txt"], b"")]),
    ("-f newer", [(["t", "d/a.txt"], b""), (["touch", "-d", "2030-01-01", "d/a.txt"], None), (["-f", "t", "d/a.txt", "text.txt"], b"")]),
    ("-f freshen none", [(["t", "d/a.txt"], b""), (["-f", "t", "d/a.txt", "text.txt"], b"")]),
    ("-d one", [(["-r", "t", "d"], b""), (["-d", "t", "d/empty"], b"")]),
    ("-d pattern", [(["-r", "t", "d"], b""), (["-d", "t", "d/sub/*"], b"")]),
    ("-d nomatch", [(["-r", "t", "d"], b""), (["-d", "t", "nope"], b"")]),
    ("-d all", [(["t", "d/a.txt"], b""), (["-d", "t", "d/a.txt"], b"")]),
    ("missing", one("t", "nope")),
    ("missing + ok", one("t", "nope", "d/a.txt")),
    ("-q missing", one("-q", "t", "nope", "d/a.txt")),
    ("-j -q", one("-jq", "t", "d/sub/b.txt", "d/a.txt")),
    ("-y link", one("-y", "t", "d/link")),
    ("link followed", one("t", "d/link")),
    ("dangling", one("t", "dang")),
    ("dangling -y", one("-y", "t", "dang")),
    ("fifo skipped", one("t", "fifo", "d/a.txt")),
    ("only zip", one("t")),
    ("-0", one("-0", "t", "d/a.txt")),
    ("-1 flags", one("-1", "t", "d/a.txt")),
    ("-9 flags", one("-9", "t", "d/a.txt")),
    ("-x", one("-r", "t", "d", "-x", "*.bin", "d/sub/*")),
    ("-i", one("-r", "t", "d", "-i", "*.txt")),
    ("-x dir entry", one("t", "d", "-x", "d/")),
    ("-D -r", one("-D", "-r", "t", "d")),
    ("dir without -r", one("t", "d")),
    ("stdout", one("-", "d/a.txt")),
    ("stdout dir", one("-r", "-", "d")),
    ("stdin entry", one("t", "-", stdin=b"piped\n")),
    ("stdin to stdout", one("-", "-", stdin=b"piped data\n" * 10)),
    ("-@", one("t", "-@", stdin=b"d/a.txt\nd/sub/b.txt\n")),
    ("-sf alone", [(["-r", "t", "d"], b""), (["-sf", "t"], b"")]),
    ("-sf with files", one("-sf", "t", "d/a.txt", "d/sub")),
    ("-sf existing + add", [(["-q", "t", "d/a.txt"], b""), (["-sf", "t", "d/sub/b.txt"], b"")]),
    ("-m", one("-m", "t", "text.txt")),
    ("-m -r dir", one("-m", "-r", "t", "d/sub")),
    ("bad option", one("-Q", "t", "d")),
    ("bad long option", one("--nonsense", "t", "d")),
    ("-P encrypt", one("-P", "secret", "t", "d/a.txt", "d/sub/b.txt")),
    ("-P stdout", one("-P", "secret", "-", "d/a.txt")),
    ("-T", one("-T", "t", "d/a.txt")),
    ("-T existing only", [(["t", "d/a.txt"], b""), (["-T", "t"], b"")]),
    ("-v add", one("-v", "t", "d/a.txt", "text.txt")),
    ("dup -j", one("-j", "t", "d/a.txt", "d/sub/b.txt", "text.txt", "d/../d/a.txt")),
    ("same twice", one("t", "d/a.txt", "d/a.txt")),
    ("-z comment", one("-z", "t", "d/a.txt", stdin=b"my comment\nline2\n")),
    ("-c comment", one("-c", "t", "d/a.txt", stdin=b"entry comment\n")),
    ("name with dot", one("t.jar", "d/a.txt")),
    ("-FS", [(["-q", "-r", "t", "d"], b""), (["rm", "d/empty"], None), (["-FS", "-r", "t", "d"], b"")]),
    ("-FS current", [(["-q", "-r", "t", "d"], b""), (["-FS", "-r", "t", "d"], b"")]),
    ("-O", [(["-q", "t", "d/a.txt"], b""), (["t", "d/sub/b.txt", "-O", "t2.zip"], b"")]),
    ("-t after", one("t", "-t", "01012030", "d/a.txt")),
    ("-t before", one("t", "-t", "2020-01-01", "d/a.txt")),
    ("-tt", one("t", "-tt", "01012030", "d/a.txt")),
    ("bad date", one("t", "-t", "2020-13-01", "d/a.txt")),
    ("-n default .zip", one("t", "in.zip", "d/a.txt")),
    ("-n custom", one("-n", ".txt", "t", "d/a.txt", "text.txt", "big.bin")),
    ("-MM", one("-MM", "t", "d/a.txt", "nope")),
    ("abs path", None),  # filled in below (needs the directory)
    ("dotdot", one("t", "d/sub/../a.txt")),
    ("-r wildcard hint", one("-r", "t", "d/s*")),
    ("-R", one("-R", "t", "*.txt")),
    ("not a zip", [(["sh", "-c", "echo junk > bad.zip"], None), (["bad.zip", "d/a.txt"], b"")]),
    ("-l", one("-l", "t", "text.txt")),
    ("-ll", one("-ll", "t", "crlf.txt")),
    ("-l binary", one("-l", "t", "big.bin")),
    ("-X", one("-X", "t", "d/a.txt")),
    ("-fd", one("-fd", "t", "d/sub/b.txt", "d/a.txt")),
    ("-g grow", [(["-q", "t", "d/a.txt"], b""), (["-g", "t", "text.txt"], b"")]),
    ("-g grow replace", [(["-q", "t", "d/a.txt", "text.txt"], b""), (["-g", "t", "text.txt"], b"")]),
    ("-o latest", [(["-o", "t", "d/a.txt"], b""), (["sh", "-c", "stat -c %Y t.zip"], None)]),
    ("utf8 name", one("t", "한글.txt")),
    ("-ws", one("-r", "t", "d", "-i", "*.txt", "-ws")),
    ("-nw -x", one("-r", "t", "d", "-nw", "-x", "d/sub/*")),
    ("-ic delete", [(["-r", "t", "d"], b""), (["-ic", "-d", "t", "D/A.TXT"], b"")]),
    ("-ic include", one("-r", "t", "d", "-i", "D/*.TXT", "-ic")),
    ("-i nothing", one("-r", "t", "d", "-i", "nomatch")),
    ("-ws -sf", one("-sf", "-r", "t", "d", "-i", "*.txt", "-ws")),
    ("-b temp", [(["sh", "-c", "mkdir tmpd"], None), (["-b", "tmpd", "t", "d/a.txt"], b"")]),
    ("--long options", one("--recurse-paths", "--quiet", "t", "d", "--exclude", "*.bin")),
    ("-x@file", [(["sh", "-c", "printf '*.bin\\nd/sub/*\\n' > ex.lst"], None), (["-r", "t", "d", "-x@ex.lst"], b"")]),
    ("-0 stdin pipe", one("-0", "-", "-", stdin=b"x" * 100)),
    ("empty file only", one("t", "d/empty")),
    ("unreadable", [(["sh", "-c", "echo s > unr; chmod 000 unr"], None), (["t", "unr", "d/a.txt"], b"")]),
]

def dump(path):
    """The archive's records, with what depends on the deflate bytes masked."""
    try:
        d = open(path, "rb").read()
    except OSError:
        return ["(none)"]
    out = []
    def extras(b):
        r, i = [], 0
        while i + 4 <= len(b):
            t, n = struct.unpack_from("<HH", b, i)
            r.append(f"{t:04x}:{b[i + 4:i + 4 + n].hex()}")
            i += 4 + n
        return " ".join(r)
    i = 0
    deflated = set()
    while i <= len(d) - 4:
        sig = d[i:i + 4]
        if sig == b"PK\x03\x04":
            ver, flag, meth, tm, dt, crc, cs, us, nl, el = struct.unpack_from("<HHHHHIIIHH", d, i + 4)
            name, ex = d[i + 30:i + 30 + nl], d[i + 30 + nl:i + 30 + nl + el]
            size = cs
            if cs == 0xFFFFFFFF:
                k = 0
                while k + 4 <= len(ex):
                    t, n = struct.unpack_from("<HH", ex, k)
                    if t == 1:
                        size = struct.unpack_from("<Q", ex, k + 4 + (8 if us == 0xFFFFFFFF else 0))[0]
                    k += 4 + n
            masked = meth != 0 and cs not in (0, 0xFFFFFFFF)
            if meth != 0:
                deflated.add(name)
                ex_s = re.sub(r"0001:([0-9a-f]{16})[0-9a-f]{16}", r"0001:\1<cs>", extras(ex))
            else:
                ex_s = extras(ex)
            # An access time from the run itself (a directory read by the scan) is "now": seconds apart
            ex_s = re.sub(r"5455:03([0-9a-f]{8})([0-9a-f]{8})", lambda mm: "5455:03" + mm[1] + ("<now>" if struct.unpack("<I", bytes.fromhex(mm[2]))[0] > T0 + 1000 else mm[2]), ex_s)
            out.append(f"LOC ver={ver} flag={flag:#x} meth={meth} time={dt:04x}{tm:04x} crc={crc:08x} cs={'<cs>' if masked else cs} us={us} name={name!r} ex=[{ex_s}]")
            i += 30 + nl + el + (size if not flag & 8 else 0)
            if flag & 8:
                j = d.find(b"PK\x07\x08", i)
                if j < 0:
                    break
                dd = d[j + 4:j + 8].hex()
                out.append(f"  DD crc={dd}" + (" sizes=<masked>" if meth != 0 else f" rest={d[j + 8:j + 16].hex()}"))
                i = j + 4
            continue
        if sig == b"PK\x01\x02":
            vm, vn, flag, meth, tm, dt, crc, cs, us, nl, el, cl, dn, ia, ea, off = struct.unpack_from("<HHHHHHIIIHHHHHII", d, i + 4)
            name, ex, cm = d[i + 46:i + 46 + nl], d[i + 46 + nl:i + 46 + nl + el], d[i + 46 + nl + el:i + 46 + nl + el + cl]
            out.append(f"CEN made={vm:#06x} need={vn} flag={flag:#x} meth={meth} time={dt:04x}{tm:04x} crc={crc:08x} cs={'<cs>' if meth != 0 else cs} us={us} ia={ia} ea={ea:#010x} name={name!r} ex=[{extras(ex)}] comment={cm!r}")
            i += 46 + nl + el + cl
            continue
        if sig == b"PK\x05\x06":
            n1, n2, c1, c2, size, off, cl = struct.unpack_from("<HHHHIIH", d, i + 4)
            out.append(f"END entries={c1} cdsize={size} comment={d[i + 22:i + 22 + cl]!r}")
            i += 22
            continue
        i += 1
    return out

def norm(text, d):
    text = text.replace(d, "<DIR>")
    text = re.sub(r"\((deflated|stored|bzipped) -?\d+%\)", r"(\1 N%)", text)
    text = re.sub(r"\(out=\d+\)", "(out=N)", text)
    text = re.sub(r"compressed=\d+ -> -?\d+% savings", "compressed=N -> N% savings", text)
    return text

def run_case(tool_dir, steps, d, name):
    env = dict(os.environ, PATH=f"{tool_dir}:/usr/bin:/bin", TZ="UTC", LC_ALL="C.UTF-8")
    log = []
    for argv, stdin in steps:
        cmd = argv if stdin is None else ["zip"] + argv
        p = subprocess.run(cmd, cwd=d, input=stdin or b"", capture_output=True, env=env, timeout=60)
        out = p.stdout
        is_stdout_zip = stdin is not None and ("stdout" in name or name == "-0 stdin pipe")
        if is_stdout_zip:
            open(f"{d}/__stdout.zip", "wb").write(out)
            out = b"<zip on stdout>"
        log.append(f"$ {' '.join(cmd)}\n[{p.returncode}]\nstdout: {norm(out.decode(errors='replace'), d)}\nstderr: {norm(p.stderr.decode(errors='replace'), d)}")
    archives = sorted(f for f in os.listdir(d) if f.endswith((".zip", ".jar")) and f not in ("in.zip",))
    for a in archives:
        log.append(f"== {a}\n" + "\n".join(dump(f"{d}/{a}")))
        # Another implementation reads it back (Info-ZIP's unzip, CRCs checked)
        pw = ["-P", "secret"] if any("-P" in argv for argv, _ in steps) else []
        t = subprocess.run(["unzip", "-tqq", *pw, a], cwd=d, capture_output=True, stdin=subprocess.DEVNULL)
        log.append(f"unzip -t: {'ok' if t.returncode in (0, 1) else 'FAILED ' + t.stdout.decode(errors='replace')[-200:]}")
    left = sorted(os.listdir(d))
    log.append("files: " + " ".join(left))
    return "\n".join(log)

KNOWN = {
    # zlib's deflate is not Info-ZIP's: a 200 kB incompressible file ends up a little bigger with
    # Info-ZIP's (stored blocks in deflate, "-0%"), the same "0%" here; the masked records agree.
}

def main():
    tmp = tempfile.mkdtemp(prefix="zipdiff-")
    fails = 0
    for name, steps in CASES:
        da, db = f"{tmp}/a", f"{tmp}/b"
        for x in (da, db):
            shutil.rmtree(x, ignore_errors=True)
            os.makedirs(x)
            setup(x)
        if name == "abs path":
            steps_a = one("t", f"{da}/d/sub/b.txt")
            steps_b = one("t", f"{db}/d/sub/b.txt")
            ra, rb = run_case(OURS, steps_a, da, name), run_case(REF, steps_b, db, name)
            ra, rb = ra.replace(da[1:], "<DIR>"), rb.replace(db[1:], "<DIR>")
        else:
            ra, rb = run_case(OURS, steps, da, name), run_case(REF, steps, db, name)
        if ra == rb:
            print(f"PASS {name}")
        else:
            tag = "KNOWN" if name in KNOWN else "FAIL"
            fails += tag == "FAIL"
            print(f"{tag} {name}")
            if tag == "FAIL":
                import difflib
                for line in difflib.unified_diff(rb.splitlines(), ra.splitlines(), "reference", "ours", lineterm="", n=1):
                    print("    " + line)
    shutil.rmtree(tmp, ignore_errors=True)
    print(f"{len(CASES) - fails}/{len(CASES)} as the reference" + ("" if not fails else f", {fails} differ"))
    sys.exit(1 if fails else 0)

main()
