"""Differential tests of our xz and zstd against the originals (on a host that has both): each
case runs in a fresh directory with each, and compares exit status, messages (program path
normalized), stdout and the files left behind (new compressed files by what they decode to).

    python3 difftest.py TOOL OUR_DIR [REF_DIR]

TOOL is xz or zstd; the directories hold the commands under their names (xz, unxz, xzcat, lzma,
unlzma, lzcat / zstd, unzstd, zstdcat). Known, intended differences are listed in KNOWN."""
import hashlib, os, re, shutil, subprocess, sys, tempfile
TOOL, OURS, REF = sys.argv[1], sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else "/usr/bin"
DATA = bytes(range(256)) * 300 + b"hello world\n" * 1000
FIXTURE_XZ = subprocess.run(["xz", "-c"], input=DATA, capture_output=True, check=True).stdout
FIXTURE_ZST = subprocess.run(["zstd", "-c"], input=DATA, capture_output=True, check=True).stdout

def setup(d):
    {"xz": setup_xz, "zstd": setup_zstd, "7z": setup_7z}[TOOL](d)

SEVEN = os.environ.get("SEVENZ_REF", f"{REF}/7z")

def setup_7z(d):
    os.makedirs(f"{d}/d/sub")
    open(f"{d}/d/a.bin", "wb").write(DATA)
    open(f"{d}/d/h.txt", "w").write("hello\n")
    open(f"{d}/d/sub/s.txt", "w").write("sub\n")
    open(f"{d}/d/empty", "w").close()
    os.symlink("h.txt", f"{d}/d/link")
    os.chmod(f"{d}/d/h.txt", 0o640)
    for p, t in (("d/a.bin", 1_700_000_000), ("d/h.txt", 1_600_000_000), ("d/sub/s.txt", 1_650_000_000), ("d/empty", 1_650_000_001),
                 ("d/sub", 1_650_000_002), ("d", 1_650_000_003)):
        os.utime(f"{d}/{p}", (t, t))
    open(f"{d}/n.txt", "w").write("new\n")
    os.utime(f"{d}/n.txt", (1_700_000_100, 1_700_000_100))
    q = dict(stdout=subprocess.DEVNULL, cwd=d, check=True)
    subprocess.run([SEVEN, "a", "x.7z", "d"], **q)
    subprocess.run([SEVEN, "a", "-mx1", "-ms=off", "nx.7z", "d"], **q)
    subprocess.run([SEVEN, "a", "z.zip", "d"], **q)
    subprocess.run([SEVEN, "a", "t.tar", "d"], **q)
    subprocess.run([SEVEN, "a", "-tgzip", "n.gz", "n.txt"], **q)
    subprocess.run([SEVEN, "a", "-pSECRET", "p.7z", "n.txt"], **q)
    subprocess.run([SEVEN, "a", "-pSECRET", "-mhe=on", "he.7z", "n.txt"], **q)
    subprocess.run([SEVEN, "a", "-pSECRET", "pz.zip", "n.txt"], **q)
    subprocess.run([SEVEN, "a", "-pSECRET", "-mem=AES256", "pza.zip", "n.txt"], **q)
    subprocess.run([SEVEN, "a", "-txz", "n.xz", "n.txt"], **q)
    subprocess.run([SEVEN, "a", "-tbzip2", "n.bz2", "n.txt"], **q)
    b = bytearray(open(f"{d}/x.7z", "rb").read()); b[len(b) // 3] ^= 0xFF
    open(f"{d}/bad.7z", "wb").write(bytes(b))
    open(f"{d}/junk.7z", "w").write("junk\n")
    open(f"{d}/list.txt", "w").write("d/h.txt\nn.txt\n")

T0 = __import__("time").time()

def tree_of(arc, d):
    """What the original 7-Zip extracts from an archive: names, kinds, sizes, contents, modes, mtimes."""
    out = tempfile.mkdtemp()
    try:
        r = subprocess.run([SEVEN, "x", "-y", "-pSECRET", f"-o{out}", os.path.join(d, arc)], capture_output=True)
        if r.returncode != 0:
            return ("unextractable", r.returncode)
        res = []
        for root, dirs, files in sorted(os.walk(out)):
            for n in sorted(dirs + files):
                p = os.path.join(root, n); rel = os.path.relpath(p, out); st = os.lstat(p)
                if os.path.islink(p): res.append((rel, "link", os.readlink(p)))
                # A time from the run itself (nothing stored) is not compared.
                mt = int(st.st_mtime) if st.st_mtime < T0 - 60 else "now"
                if os.path.isdir(p): res.append((rel, "dir", oct(st.st_mode & 0o777), mt))
                else: res.append((rel, hashlib.sha1(open(p, "rb").read()).hexdigest()[:12], oct(st.st_mode & 0o777), mt))
        return tuple(res)
    finally:
        shutil.rmtree(out)

def setup_zstd(d):
    open(f"{d}/a.txt", "wb").write(DATA)
    open(f"{d}/b.txt", "wb").write(b"small\n")
    open(f"{d}/empty", "wb").close()
    os.makedirs(f"{d}/dir/sub")
    open(f"{d}/dir/x", "wb").write(b"x" * 1000)
    open(f"{d}/dir/sub/y", "wb").write(b"y" * 1000)
    os.symlink("a.txt", f"{d}/link")
    open(f"{d}/a.txt.zst", "wb").write(FIXTURE_ZST)
    open(f"{d}/bad.zst", "wb").write(b"not zstd at all")
    open(f"{d}/trunc.zst", "wb").write(FIXTURE_ZST[:len(FIXTURE_ZST) // 2])
    c = bytearray(FIXTURE_ZST); c[len(c) // 2] ^= 0xFF
    open(f"{d}/corrupt.zst", "wb").write(bytes(c))
    open(f"{d}/a.gz", "wb").write(subprocess.run(["gzip", "-c"], input=DATA, capture_output=True).stdout)
    open(f"{d}/a.xz", "wb").write(FIXTURE_XZ)
    open(f"{d}/skip.zst", "wb").write(b"\x50\x2a\x4d\x18\x04\x00\x00\x00abcd" + FIXTURE_ZST)
    open(f"{d}/nocheck.zst", "wb").write(subprocess.run([f"{REF}/zstd", "-c", "--no-check", "--no-content-size"], input=DATA, capture_output=True).stdout)
    open(f"{d}/list", "w").write("a.txt\nb.txt\n")
    os.chmod(f"{d}/a.txt", 0o640)
    os.utime(f"{d}/a.txt", (1_000_000_000, 1_000_000_000))

def setup_xz(d):
    open(f"{d}/a.txt", "wb").write(DATA)
    open(f"{d}/b.txt", "wb").write(b"small\n")
    open(f"{d}/empty", "wb").close()
    os.mkdir(f"{d}/dir")
    os.symlink("a.txt", f"{d}/link")
    open(f"{d}/a.txt.xz", "wb").write(FIXTURE_XZ)
    subprocess.run([f"{REF}/xz", "-kc", "--format=lzma", "a.txt"], cwd=d, check=True, stdout=open(f"{d}/a.lzma", "wb"))
    open(f"{d}/bad.xz", "wb").write(b"not xz at all")
    raw = open(f"{d}/a.txt.xz", "rb").read()
    open(f"{d}/trunc.xz", "wb").write(raw[:len(raw) // 2])
    c = bytearray(raw); c[len(c) // 2] ^= 0xFF
    open(f"{d}/corrupt.xz", "wb").write(bytes(c))
    os.chmod(f"{d}/a.txt", 0o640)
    os.utime(f"{d}/a.txt", (1_000_000_000, 1_000_000_000))

FIXTURES_7Z = {"x.7z", "nx.7z", "z.zip", "t.tar", "n.gz", "p.7z", "he.7z", "pz.zip", "pza.zip", "n.xz", "n.bz2", "bad.7z", "junk.7z"}

def snapshot(d):
    if TOOL == "7z":
        return snapshot_7z(d)
    out = {}
    for root, dirs, files in os.walk(d):
        for f in files + dirs:
            p = os.path.join(root, f); r = os.path.relpath(p, d)
            st = os.lstat(p)
            if os.path.islink(p): out[r] = ("link", os.readlink(p))
            elif os.path.isdir(p): out[r] = ("dir",)
            else:
                data = open(p, "rb").read()
                # Compressed outputs may differ byte-wise between encoders: what is new and
                # decodes is compared by its content.
                if r not in ("bad.xz", "trunc.xz", "corrupt.xz", "a.lzma", "bad.zst", "trunc.zst", "corrupt.zst", "a.gz", "skip.zst", "nocheck.zst"):
                    for dec in (["xz", "-dc", "-F", "auto"], ["zstd", "-dc"], ["gzip", "-dc"]):
                        res = subprocess.run(dec, input=data, capture_output=True)
                        if res.returncode == 0 and data[:4] in (b"\xfd7zX", b"\x28\xb5\x2f\xfd", b"\x1f\x8b\x08\x00", b"\x1f\x8b\x08\x08") or (res.returncode == 0 and r.endswith((".lzma", ".foo"))):
                            data = b"decoded:" + res.stdout
                            break
                out[r] = (hashlib.sha1(data).hexdigest()[:12], oct(st.st_mode & 0o7777), int(st.st_mtime) if r in ("a.txt", "a") else 0)
    return out

XZ_CASES = [
    "xz -k a.txt", "xz a.txt", "xz -c a.txt > o.xz", "xz a.txt.xz", "xz -f a.txt.xz", "xz -d a.txt.xz",
    "xz -dk a.txt.xz", "unxz a.txt.xz", "xzcat a.txt.xz > o", "xz -d a.txt", "xz -dc a.txt", "xz -dcf a.txt > o",
    "xz -dcf bad.xz > o", "xz -d bad.xz", "xz -t a.txt.xz", "xz -t bad.xz trunc.xz corrupt.xz a.txt.xz",
    "xz -d trunc.xz", "xz -d corrupt.xz", "xz -k a.txt; xz -k a.txt", "xz -kf a.txt; xz -kf a.txt",
    "xz dir", "xz link", "xz -f link", "xz -c link > o", "xz nonexist", "xz -k empty", "xz -dc empty.xz",
    "xz -S .foo -k a.txt", "xz -d -S .foo a.txt.xz", "lzma -k a.txt", "unlzma a.lzma", "lzcat a.lzma > o",
    "xz -dc a.lzma > o", "xz -F xz -d a.lzma", "xz -F lzma -dc a.txt.xz > o", "xz -l a.txt", "xz -l bad.xz",
    "xz -l a.lzma", "xz --list --format=lzma a.lzma", "xz -lq bad.xz", "xz -q a.txt.xz", "xz -Q a.txt.xz",
    "xz --bogus", "xz -Z", "xz --threads", "xz -T x a.txt", "xz -C sha512 a.txt", "xz -F foo a.txt",
    "xz -k -0 a.txt", "xz -k -9e a.txt", "xz -k --check=none a.txt", "xz -k --lzma2=preset=1,dict=64KiB a.txt",
    "xz -k --x86 --lzma2 a.txt", "xz -k --delta=dist=4 --lzma2=lc=1,lp=2 a.txt", "xz -k --lzma1 a.txt",
    "xz -k --filters='x86 lzma2:preset=2' a.txt", "xz -k --format=raw a.txt", "xz -kc --format=raw a.txt > o.foo",
    "xz -k --lzma2=lc=4,lp=1 a.txt", "xz -dc a.txt.xz a.txt.xz > o", "cat a.txt | xz > o.xz", "cat a.txt.xz | xz -d > o",
    "cat a.txt.xz | xz -d - b.txt > o", "printf 'a.txt\\nb.txt\\n' | xz -k --files", "printf 'a.txt\\0' | xz -k --files0",
    "XZ_OPT=-k xz a.txt", "XZ_OPT=file xz a.txt", "xz -k -S '' a.txt", "xz -d -S .txt a.txt", "xz -lv a.txt.xz bad.xz",
    "xz -dk --single-stream a.txt.xz", "cat a.txt.xz a.txt.xz | xz -dc --single-stream | wc -c",
    "cat a.txt.xz a.txt.xz | xz -dc | wc -c", "(cat a.txt.xz; printf '\\0\\0\\0\\0') | xz -dc | wc -c",
    "(cat a.txt.xz; printf '\\0\\0\\0') | xz -dc | wc -c", "(cat a.txt.xz; printf 'junk') | xz -dc | wc -c",
    "(cat a.txt.xz; printf 'junk') | xz -dc --single-stream | wc -c",
]

ZSTD_CASES = [
    "zstd a.txt", "zstd -q a.txt", "zstd a.txt --rm", "zstd -c a.txt > o.zst", "zstd a.txt -o o.zst", "zstd a.txt; zstd a.txt",
    "zstd -f a.txt; zstd -f a.txt", "zstd -q a.txt; zstd -q a.txt", "zstd a.txt < /dev/null", "zstd -d a.txt.zst", "zstd -df a.txt.zst",
    "zstd -d a.txt.zst -o out", "unzstd a.txt.zst --rm", "zstdcat a.txt.zst > o", "zstd -dc a.txt.zst > o", "zstd -t a.txt.zst",
    "zstd -t bad.zst trunc.zst corrupt.zst a.txt.zst", "zstd -d bad.zst", "zstd -d trunc.zst", "zstd -d corrupt.zst", "zstd -d a.txt",
    "zstd -d b.txt.zst", "zstdcat a.txt > o", "zstd -dcf a.txt > o", "zstd -dc a.txt", "zstd -d empty", "zstd -dc empty",
    "zstd dir", "zstd -r dir", "zstd -r dir --rm", "zstd -r dir -c > o", "zstd link", "zstd -f link", "zstd nonexist", "zstd -k empty",
    "zstd -d a.gz", "zstd -d a.xz", "zstd -dc a.gz > o", "zstd -l a.txt.zst", "zstd -lv a.txt.zst", "zstd -l a.txt.zst nocheck.zst skip.zst",
    "zstd -l bad.zst", "zstd -l trunc.zst", "zstd -l a.txt", "zstd -lv a.txt.zst skip.zst nocheck.zst", "zstd -l", "zstd -l -",
    "zstd -dc skip.zst > o", "zstd -dc nocheck.zst > o", "zstd -d --no-check corrupt.zst", "zstd --bogus", "zstd -Z", "zstd -o",
    "zstd -19 a.txt", "zstd -1 a.txt", "zstd --fast=3 a.txt", "zstd --fast a.txt", "zstd -25 a.txt", "zstd --ultra -22 a.txt",
    "zstd --long a.txt", "zstd --no-check a.txt", "zstd --no-content-size a.txt", "zstd -C a.txt", "zstd --format=gzip a.txt",
    "zstd --format=xz a.txt", "zstd --format=lzma a.txt", "zstd a.txt b.txt", "zstd a.txt b.txt -o o", "zstd -f a.txt b.txt -o o",
    "zstd a.txt b.txt -c > o", "zstd -d a.txt.zst nocheck.zst -c > o", "zstd -dq a.txt.zst nocheck.zst -o o", "zstd --filelist list",
    "zstd --output-dir-flat dir a.txt b.txt", "zstd --exclude-compressed a.txt.zst b.txt", "cat a.txt | zstd > o.zst",
    "cat a.txt | zstd -o o.zst", "cat a.txt.zst | zstd -d > o", "cat a.txt.zst a.txt.zst | zstd -d | wc -c",
    "(cat a.txt.zst; printf junk) | zstd -d | wc -c", "(cat a.txt.zst; printf ju) | zstd -d | wc -c", "zstd -V", "zstd -qV",
    "zstd --zstd=wlog=18,strat=3 a.txt", "zstd --zstd=bogus=1 a.txt", "ZSTD_CLEVEL=9 zstd a.txt", "ZSTD_CLEVEL=x zstd a.txt",
    "zstd -T2 a.txt", "zstd -d -T2 a.txt.zst -f", "zstd -- -x", "zstd a.txt -o a.txt", "zstd -v a.txt", "zstd -vd a.txt.zst -f",
    "zstd --train a.txt", "zstd -b1 a.txt",
]
SEVENZ_CASES = [
    "7z l x.7z", "7z l nx.7z", "7z l -slt x.7z", "7z l -ba x.7z", "7z l z.zip", "7z l -ba z.zip", "7z l t.tar", "7z l -ba t.tar",
    "7z l n.gz", "7z l n.xz", "7z l n.bz2", "7z l -pSECRET p.7z", "7z l -pSECRET he.7z", "7z l -pBAD he.7z", "7z l junk.7z", "7z l nope.7z",
    "7z l x.7z d/h.txt", "7z l x.7z '*.txt' -r", "7z l x.7z -x'!*.txt' -r", "7z l -slt -ba x.7z d/sub",
    "7z t x.7z", "7z t nx.7z", "7z t z.zip", "7z t t.tar", "7z t n.gz", "7z t bad.7z", "7z t -pSECRET p.7z", "7z t -pBAD p.7z",
    "7z t -pSECRET pz.zip", "7z t -pSECRET pza.zip", "7z t -pBAD pz.zip",
    "7z x x.7z -oout", "7z x nx.7z -oout", "7z x z.zip -oout", "7z x t.tar -oout", "7z x n.gz -oout", "7z x n.xz -oout", "7z x n.bz2 -oout",
    "7z e x.7z -oout", "7z x x.7z -oout d/sub", "7z x x.7z -oout -x'!d/a.bin'", "7z x -pSECRET p.7z -oout", "7z x -pSECRET pza.zip -oout",
    "7z x x.7z -oout; 7z x x.7z -oout -aos", "7z x x.7z -oout; 7z x x.7z -oout -aoa", "7z x x.7z -oout; 7z x x.7z -oout -y",
    "7z x x.7z -oout; 7z x x.7z -oout -aot", "7z x -so n.gz > o", "7z x -so x.7z d/h.txt > o", "7z x x.7z -oout -spe d",
    "7z a new.7z d", "7z a new.7z d/h.txt n.txt", "7z a new.7z ./d/h.txt", "7z a -mx0 new.7z d", "7z a -mx9 new.7z d",
    "7z a -ms=off new.7z d", "7z a -m0=LZMA new.7z d", "7z a -m0=PPMd new.7z d", "7z a -m0=BZip2 new.7z d", "7z a -m0=Deflate new.7z d",
    "7z a -mf=BCJ new.7z d", "7z a -pPW new.7z d", "7z a -pPW -mhe=on new.7z d", "7z a new.zip d", "7z a -mx0 new.zip d",
    "7z a -pPW new.zip d", "7z a -pPW -mem=AES256 new.zip d", "7z a -m0=BZip2 new.zip d", "7z a new.tar d", "7z a -tgzip new.gz n.txt",
    "7z a -txz new.xz n.txt", "7z a -tbzip2 new.bz2 n.txt", "7z a -tgzip new.gz d", "7z a new d", "7z a -tzip new d",
    "cd d && 7z a ../new.7z '*.txt' -r", "7z a new.7z d -x'!*.bin' -r", "7z a new.7z d -xr'!*.txt'", "7z a new.7z @list.txt",
    "7z a new.7z nope", "7z a new.7z d nope", "7z a -sse new.7z d nope", "cp x.7z y.7z; 7z a y.7z n.txt", "cp x.7z y.7z; 7z u y.7z d",
    "cp x.7z y.7z; 7z d y.7z d/h.txt", "cp x.7z y.7z; 7z d y.7z d/sub", "cp z.zip y.zip; 7z a y.zip n.txt", "cp z.zip y.zip; 7z d y.zip d/h.txt",
    "cp t.tar y.tar; 7z a y.tar n.txt", "cp x.7z y.7z; 7z rn y.7z d/h.txt d/renamed.txt", "7z d nope.7z x", "7z a -sdel new.7z n.txt",
    "7z a -si new.gz < n.txt", "7z a -sinamed.txt new.7z < n.txt", "7z a -so -tgzip n.txt > o.gz", "7z a -snl new.7z d", "7z a -stl new.7z d",
    "7z h d/h.txt", "7z h -scrcSHA256 d/h.txt", "7z h d", "7z h -scrc* n.txt", "7z i", "7z", "7z z", "7z a", "7z x", "7z l -zz x.7z",
    "7z x x.7z -oout -bb1", "7z a new.7z d -bb1",
]
CASES = {"xz": XZ_CASES, "zstd": ZSTD_CASES, "7z": SEVENZ_CASES}[TOOL]
# Intended differences: our tools say who they are, and leave out what they do not do.
# The usage text and the too-high-level warning follow zstd's development branch (the reference
# clone), which the host's 1.5.7 predates; lz4 is not among our formats.
KNOWN = {"zstd -V", "zstd -qV", "zstd --train a.txt", "zstd -b1 a.txt", "xz --list --format=lzma a.lzma",
         # 7z: the usage names the program as run (the original always says 7zz); i lists our
         # formats; b is not ours.
         "7z", "7z i",
         "zstd --bogus", "zstd -Z", "zstd --zstd=bogus=1 a.txt", "zstd -25 a.txt", "zstd -d a.txt", "zstd -d empty"}

def snapshot_7z(d):
    out = {}
    for root, dirs, files in os.walk(d):
        for f in files + dirs:
            p = os.path.join(root, f); r = os.path.relpath(p, d)
            if r.startswith("d/") or r == "d" or r in FIXTURES_7Z or r in ("n.txt", "list.txt"):
                if os.path.exists(p) and not os.path.isdir(p) and r in FIXTURES_7Z:
                    continue  # unchanged fixtures; changed ones show in their own name below
                if os.path.lexists(p): out[r] = ("exists", os.path.isdir(p))
                continue
            st = os.lstat(p)
            if os.path.islink(p): out[r] = ("link", os.readlink(p))
            elif os.path.isdir(p): out[r] = ("dir", oct(st.st_mode & 0o777))
            elif r.endswith((".7z", ".zip", ".tar", ".gz", ".xz", ".bz2")) or r == "new":
                out[r] = ("archive", tree_of(r, d))
            else:
                out[r] = (hashlib.sha1(open(p, "rb").read()).hexdigest()[:12], oct(st.st_mode & 0o777), int(st.st_mtime))
    for f in ("y.7z", "y.zip", "y.tar"):
        if os.path.exists(os.path.join(d, f)): out[f] = ("archive", tree_of(f, d))
    return out

def run(d, binpath, cmd):
    env = dict(os.environ, PATH=f"{binpath}:/usr/bin:/bin", LC_ALL="C.UTF-8" if TOOL == "7z" else "C", TZ="UTC")
    r = subprocess.run(["sh", "-c", cmd], cwd=d, env=env, capture_output=True, timeout=120, stdin=subprocess.DEVNULL)
    err = r.stderr.decode(errors="replace").replace(f"{binpath}/", "")
    out = r.stdout.decode(errors="replace")
    # What depends on the encoder (compressed sizes in summaries) or on who we are (the banner).
    err = re.sub(r"=> +[0-9.]+ *[KMG]?i?B", "=> SIZE", err)
    err = re.sub(r"(compressed : |:) ?[0-9]+\.[0-9]+%", r"\1 PCT", err)
    err = re.sub(r"(?m)^\*\*\* Zstandard CLI.*\n", "", err)
    if TOOL == "7z":
        # The banner says who we are; sizes of archives made in the case depend on the encoder.
        out = re.sub(r"\n7-Zip [^\n]*\n [^\n]*\n", "\n7-Zip BANNER\n", out)
        err = re.sub(r"\n7-Zip [^\n]*\n [^\n]*\n", "\n7-Zip BANNER\n", err)
        if "7z a" in cmd or "7z u" in cmd or "7z d" in cmd or "7z rn" in cmd:
            out = re.sub(r"Archive size: \d+ bytes \(\d+ [KMG]iB\)", "Archive size: SIZE", out)
            out = re.sub(r"(Physical Size|Headers Size) = \d+", r"\1 = N", out)
    return r.returncode, out, err, snapshot(d)

fails = 0
for cmd in CASES:
    res = []
    for b in (REF, OURS):
        d = tempfile.mkdtemp()
        try:
            setup(d); res.append(run(d, b, cmd))
        finally:
            shutil.rmtree(d)
    if res[0] != res[1] and cmd in KNOWN:
        print(f"known {cmd}")
    elif res[0] != res[1]:
        fails += 1
        print(f"DIFF {cmd}")
        for name, i in (("exit", 0), ("stdout", 1), ("stderr", 2)):
            if res[0][i] != res[1][i]: print(f"  {name}: ref={res[0][i]!r}\n  {' ' * len(name)}  our={res[1][i]!r}")
        if res[0][3] != res[1][3]:
            keys = sorted(set(res[0][3]) | set(res[1][3]))
            for k in keys:
                if res[0][3].get(k) != res[1][3].get(k): print(f"  file {k}: ref={res[0][3].get(k)} our={res[1][3].get(k)}")
    else:
        print(f"same {cmd}")
print(f"{len(CASES) - fails}/{len(CASES)} the same")
sys.exit(1 if fails else 0)
