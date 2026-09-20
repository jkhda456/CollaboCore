"""Guest-side smoke test for the wasm32-linux CPython. Every check runs even if others fail,
and the result of each is printed, so one run maps what this platform can and cannot do.

    python3 /usr/share/tests/smoke.py          # exit status 1 if anything unexpected failed
"""
import os, sys, time, traceback

results = []          # (name, ok, detail)
NOTES = []            # informational measurements, not pass/fail


def check(name):
    def deco(fn):
        try:
            detail = fn()
            results.append((name, True, detail or ""))
            print(f"ok    {name}" + (f"  [{detail}]" if detail else ""), flush=True)
        except BaseException as e:      # noqa: BLE001 - report, keep going
            results.append((name, False, f"{type(e).__name__}: {e}"))
            print(f"FAIL  {name}: {type(e).__name__}: {e}", flush=True)
            tb = traceback.format_exc().strip().splitlines()
            for line in tb[-4:-1]:
                print("        " + line.strip(), flush=True)
        return fn
    return deco


def must_not_import(module):
    try:
        __import__(module)
    except ImportError:
        return f"{module} absent, as configured"
    raise AssertionError(f"{module} unexpectedly importable")


# ---- the interpreter itself -------------------------------------------------------------------

@check("version and prefix")
def _():
    assert sys.version_info[:3] == (3, 13, 14), sys.version
    assert sys.prefix == "/usr", sys.prefix
    assert "PYTHONHOME" not in os.environ
    assert sys.platform == "linux"
    return f"{sys.version_info[:3]} {os.uname().machine}"

@check("stdlib is imported from the stored zip")
def _():
    import json
    assert json.__file__.startswith("/usr/lib/python313.zip/"), json.__file__
    return json.__file__

@check("arithmetic, big ints, floats, decimal, fractions")
def _():
    from decimal import Decimal, getcontext
    from fractions import Fraction
    assert eval("6 * 7") == 42
    assert len(str(2 ** 10000)) == 3011
    assert 0.1 + 0.2 != 0.3 and abs(0.1 + 0.2 - 0.3) < 1e-15
    getcontext().prec = 40
    assert str(Decimal(1) / Decimal(7)) == "0.1428571428571428571428571428571428571429"
    assert Fraction(1, 3) + Fraction(1, 6) == Fraction(1, 2)

@check("unicode: str, encodings, unicodedata, console encoding")
def _():
    import unicodedata
    s = "한글 é 😀"
    assert s.encode("utf-8").decode("utf-8") == s and len(s) == 6
    assert unicodedata.normalize("NFD", "é") == "é"
    assert unicodedata.name("한") == "HANGUL SYLLABLE HAN"
    print("      console: 한글 é 😀", flush=True)
    return f"stdout={sys.stdout.encoding} fs={sys.getfilesystemencoding()}"

@check("json round trip")
def _():
    import json
    obj = {"n": 42, "xs": [1, 2.5, None, True], "s": "héllo 한글", "nested": {"k": [{}]}}
    assert json.loads(json.dumps(obj, ensure_ascii=False)) == obj

@check("re, string formatting, textwrap, difflib")
def _():
    import re, textwrap, difflib
    assert re.sub(r"(\d+)", lambda m: str(int(m[1]) * 2), "a1 b22 c333") == "a2 b44 c666"
    assert re.findall(r"\b\w{5}\b", "hello world foo barbaz") == ["hello", "world"]
    assert f"{3.14159:8.3f}|{255:#x}|{'x':>4}" == "   3.142|0xff|   x"
    assert textwrap.fill("a " * 30, 20).count("\n") >= 2
    assert list(difflib.unified_diff(["a\n"], ["b\n"]))

@check("data structures: collections, itertools, functools, dataclasses, enum, struct, heapq, bisect")
def _():
    import collections, itertools, functools, dataclasses, enum, struct, heapq, bisect
    assert collections.Counter("abracadabra").most_common(1) == [("a", 5)]
    assert list(itertools.islice(itertools.count(), 3)) == [0, 1, 2]
    assert functools.reduce(lambda a, b: a * b, range(1, 11)) == 3628800
    @dataclasses.dataclass
    class P:
        x: int
        y: int = 2
    assert P(1) == P(1, 2)
    class C(enum.Enum):
        A = 1
    assert C(1) is C.A
    assert struct.unpack("<IH", struct.pack("<IH", 7, 9)) == (7, 9)
    h = [5, 1, 4]; heapq.heapify(h)
    assert heapq.heappop(h) == 1 and bisect.bisect([1, 3, 5], 4) == 2

@check("deep recursion (8 MiB stack)")
def _():
    sys.setrecursionlimit(20000)
    def f(n): return 0 if n == 0 else 1 + f(n - 1)
    assert f(9000) == 9000

@check("compile, exec, ast, importlib from a file on disk")
def _():
    import ast, importlib, tempfile
    assert ast.parse("x = 1 + 2").body
    ns = {}
    exec(compile("y = [i * i for i in range(5)]", "<t>", "exec"), ns)
    assert ns["y"] == [0, 1, 4, 9, 16]
    with tempfile.TemporaryDirectory() as d:
        with open(os.path.join(d, "guest_mod.py"), "w") as f:
            f.write("VALUE = 'imported from disk'\n")
        sys.path.insert(0, d)
        try:
            importlib.invalidate_caches()
            import guest_mod
            assert guest_mod.VALUE == "imported from disk"
            assert not os.path.exists(os.path.join(d, "__pycache__")), "bytecode should not be written"
        finally:
            sys.path.remove(d)

@check("no fork/mmap-based modules, as configured")
def _():
    assert not hasattr(os, "fork") and not hasattr(os, "vfork")
    notes = [must_not_import(m) for m in ("mmap", "_ctypes", "_posixsubprocess", "sqlite3", "ssl", "_bz2", "_lzma")]
    return f"{len(notes)} optional modules absent"

# ---- files ------------------------------------------------------------------------------------

@check("files: text/binary I/O, stat, rename, listdir, walk, unlink")
def _():
    import tempfile
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "a.txt")
        with open(p, "w", encoding="utf-8") as f:
            f.write("line1\n라인2\n")
        with open(p, "a") as f:
            f.write("line3\n")
        with open(p, encoding="utf-8") as f:
            assert f.read().splitlines() == ["line1", "라인2", "line3"]
        with open(os.path.join(d, "b.bin"), "wb") as f:
            f.write(bytes(range(256)) * 100)
        assert os.stat(os.path.join(d, "b.bin")).st_size == 25600
        os.rename(p, os.path.join(d, "c.txt"))
        os.makedirs(os.path.join(d, "x", "y"))
        os.symlink("c.txt", os.path.join(d, "link"))
        assert os.readlink(os.path.join(d, "link")) == "c.txt"
        assert sorted(os.listdir(d)) == ["b.bin", "c.txt", "link", "x"]
        assert sorted(r for r, _, _ in os.walk(d)) == [d, d + "/x", d + "/x/y"]
        os.chmod(os.path.join(d, "c.txt"), 0o600)
        assert os.stat(os.path.join(d, "c.txt")).st_mode & 0o777 == 0o600
        os.unlink(os.path.join(d, "b.bin"))
        assert not os.path.exists(os.path.join(d, "b.bin"))

@check("pathlib, shutil, tempfile, glob, fnmatch")
def _():
    import pathlib, shutil, tempfile, glob
    with tempfile.TemporaryDirectory() as d:
        root = pathlib.Path(d)
        (root / "src" / "pkg").mkdir(parents=True)
        (root / "src" / "pkg" / "m.py").write_text("x = 1\n")
        shutil.copytree(root / "src", root / "copy")
        assert (root / "copy" / "pkg" / "m.py").read_text() == "x = 1\n"
        assert sorted(p.name for p in root.rglob("*.py")) == ["m.py", "m.py"]
        assert len(glob.glob(str(root / "*"))) == 2
        shutil.rmtree(root / "copy")
        assert not (root / "copy").exists()

@check("the /work share: write, read back, mtime, big file")
def _():
    if not os.path.ismount("/work"):
        return "skipped: /work is not mounted in this run"
    p = "/work/.py-smoke"
    os.makedirs(p, exist_ok=True)
    f = p + "/data.bin"
    payload = os.urandom(3 * 1024 * 1024)
    with open(f, "wb") as fh:
        fh.write(payload)
    with open(f, "rb") as fh:
        assert fh.read() == payload
    assert abs(os.stat(f).st_mtime - time.time()) < 300
    os.utime(f, (1_700_000_000, 1_700_000_000))
    assert int(os.stat(f).st_mtime) == 1_700_000_000
    os.unlink(f); os.rmdir(p)
    return "3 MiB round trip"

# ---- compression and hashing ---------------------------------------------------------------

@check("zlib, gzip, zipfile (stored and deflated), tarfile")
def _():
    import zlib, gzip, zipfile, tarfile, io
    data = b"the quick brown fox " * 500
    assert zlib.decompress(zlib.compress(data, 9)) == data and len(zlib.compress(data)) < len(data) // 10
    assert gzip.decompress(gzip.compress(data)) == data
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("stored.txt", data, compress_type=zipfile.ZIP_STORED)
        z.writestr("deflated.txt", data, compress_type=zipfile.ZIP_DEFLATED)
    with zipfile.ZipFile(buf) as z:
        assert z.testzip() is None and z.read("stored.txt") == z.read("deflated.txt") == data
        assert z.getinfo("deflated.txt").compress_size < z.getinfo("stored.txt").compress_size
    tb = io.BytesIO()
    with tarfile.open(fileobj=tb, mode="w:gz") as t:
        ti = tarfile.TarInfo("d/f"); ti.size = len(data); t.addfile(ti, io.BytesIO(data))
    tb.seek(0)
    with tarfile.open(fileobj=tb, mode="r:gz") as t:
        assert t.extractfile("d/f").read() == data

@check("hashlib (md5, sha1, sha2, sha3, blake2), hmac, base64, binascii")
def _():
    import hashlib, hmac, base64, binascii
    assert hashlib.md5(b"abc").hexdigest() == "900150983cd24fb0d6963f7d28e17f72"
    assert hashlib.sha1(b"abc").hexdigest() == "a9993e364706816aba3e25717850c26c9cd0d89d"
    assert hashlib.sha256(b"abc").hexdigest() == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    assert hashlib.sha512(b"abc").hexdigest().startswith("ddaf35a193617aba")
    assert hashlib.sha3_256(b"abc").hexdigest() == "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
    assert len(hashlib.blake2b(b"abc").digest()) == 64
    assert hmac.new(b"key", b"The quick brown fox jumps over the lazy dog", "sha256").hexdigest() == "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
    assert base64.b64decode(base64.b64encode(b"\x00\xff" * 5)) == b"\x00\xff" * 5
    assert binascii.crc32(b"123456789") == 0xCBF43926

@check("randomness: os.urandom, secrets, random")
def _():
    import secrets, random
    assert len(os.urandom(32)) == 32 and os.urandom(16) != os.urandom(16)
    assert len(secrets.token_hex(16)) == 32
    assert random.Random(7).random() == random.Random(7).random()
    assert random.Random(7).random() != random.Random(8).random()

# ---- time -------------------------------------------------------------------------------------

@check("time, datetime, sleep, monotonic")
def _():
    import datetime
    now = datetime.datetime.now(datetime.timezone.utc)
    assert now.year >= 2026, now
    assert datetime.datetime(2026, 9, 19, 12, 0).strftime("%Y-%m-%d %H:%M") == "2026-09-19 12:00"
    t0 = time.monotonic(); time.sleep(0.2); dt = time.monotonic() - t0
    assert 0.15 < dt < 2.0, dt
    assert time.perf_counter() > 0
    return f"sleep(0.2) took {dt:.3f}s"

# ---- processes, threads ------------------------------------------------------------------------

@check("subprocess: run, capture, PATH lookup, shell, stdin, exit codes")
def _():
    import subprocess
    r = subprocess.run(["echo", "hi"], capture_output=True)
    assert r.returncode == 0 and r.stdout == b"hi\n", r
    assert subprocess.check_output("echo abc | tr a-z A-Z", shell=True) == b"ABC\n"
    assert subprocess.run(["cat"], input=b"piped in", capture_output=True).stdout == b"piped in"
    assert subprocess.run(["sh", "-c", "exit 7"]).returncode == 7
    assert subprocess.run(["sh", "-c", "echo err >&2"], capture_output=True).stderr == b"err\n"
    try:
        subprocess.run(["definitely-not-a-command"])
    except FileNotFoundError:
        pass
    else:
        raise AssertionError("missing command should raise FileNotFoundError")
    out = subprocess.run(["sh", "-c", "echo $FOO; pwd"], env={"FOO": "bar", "PATH": os.environ["PATH"]}, cwd="/tmp", capture_output=True).stdout
    assert out == b"bar\n/tmp\n", out

@check("subprocess: many sequential children, and a large output")
def _():
    import subprocess
    for i in range(20):
        assert subprocess.run(["true"]).returncode == 0
    out = subprocess.run(["sh", "-c", "dd if=/dev/zero bs=1024 count=512 2>/dev/null | tr '\\0' 'a'"], capture_output=True).stdout
    assert len(out) == 512 * 1024 and out[:3] == b"aaa"

@check("subprocess: timeout kills the child")
def _():
    import subprocess
    t0 = time.monotonic()
    try:
        subprocess.run(["sleep", "30"], timeout=0.5)
    except subprocess.TimeoutExpired:
        pass
    else:
        raise AssertionError("no timeout")
    assert time.monotonic() - t0 < 10

@check("subprocess: talking to hfetch (the host request bridge is not attached in this run)")
def _():
    import subprocess
    r = subprocess.run(["hfetch", "-h"], capture_output=True)
    assert r.returncode == 0 and b"usage: hfetch" in r.stdout

@check("threading: threads, locks, events, queue, ThreadPoolExecutor")
def _():
    import threading, queue, concurrent.futures
    lock = threading.Lock(); total = [0]
    def work():
        for _ in range(1000):
            with lock: total[0] += 1
    ts = [threading.Thread(target=work) for _ in range(4)]
    [t.start() for t in ts]; [t.join() for t in ts]
    assert total[0] == 4000, total
    q = queue.Queue(); ev = threading.Event()
    def producer():
        for i in range(5): q.put(i)
        ev.set()
    threading.Thread(target=producer).start()
    assert ev.wait(5) and [q.get(timeout=2) for _ in range(5)] == [0, 1, 2, 3, 4]
    with concurrent.futures.ThreadPoolExecutor(3) as ex:
        assert list(ex.map(lambda x: x * x, range(6))) == [0, 1, 4, 9, 16, 25]

# ---- sockets, asyncio ----------------------------------------------------------------------------

@check("sockets: socketpair, AF_UNIX, select/selectors")
def _():
    import socket, select, selectors, tempfile
    a, b = socket.socketpair()
    a.sendall(b"ping"); assert b.recv(10) == b"ping"
    r, _, _ = select.select([a], [], [], 0.05); assert r == []
    b.sendall(b"x"); r, _, _ = select.select([a], [], [], 1); assert r == [a]
    sel = selectors.DefaultSelector(); sel.register(a, selectors.EVENT_READ)
    assert sel.select(1); sel.close(); a.close(); b.close()
    with tempfile.TemporaryDirectory() as d:
        path = d + "/s"
        srv = socket.socket(socket.AF_UNIX); srv.bind(path); srv.listen(1)
        cli = socket.socket(socket.AF_UNIX); cli.connect(path)
        conn, _ = srv.accept(); cli.sendall(b"unix"); assert conn.recv(4) == b"unix"
        for s in (cli, conn, srv): s.close()
    return f"selector={type(selectors.DefaultSelector()).__name__}"

@check("sockets: TCP over loopback (needs lo up)")
def _():
    import socket, threading
    srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0)); srv.listen(1); port = srv.getsockname()[1]
    def serve():
        c, _ = srv.accept(); c.sendall(c.recv(100).upper()); c.close()
    threading.Thread(target=serve, daemon=True).start()
    cli = socket.create_connection(("127.0.0.1", port), timeout=5)
    cli.sendall(b"loopback"); assert cli.recv(100) == b"LOOPBACK"
    cli.close(); srv.close()

@check("sockets: AF_VSOCK exists (the host bridge transport)")
def _():
    import socket
    assert hasattr(socket, "AF_VSOCK")
    s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM); s.close()

@check("asyncio: run, gather, sleep, to_thread, streams over a socketpair")
def _():
    import asyncio, socket
    async def main():
        async def job(n):
            await asyncio.sleep(0.05); return n * 2
        assert await asyncio.gather(*(job(i) for i in range(5))) == [0, 2, 4, 6, 8]
        assert await asyncio.to_thread(lambda: 21 * 2) == 42
        a, b = socket.socketpair()
        ra, wa = await asyncio.open_connection(sock=a)
        rb, wb = await asyncio.open_connection(sock=b)
        wa.write(b"hello async\n"); await wa.drain()
        assert await asyncio.wait_for(rb.readline(), 2) == b"hello async\n"
        wa.close(); wb.close()
    asyncio.run(main())

@check("asyncio: subprocess")
def _():
    import asyncio
    async def main():
        p = await asyncio.create_subprocess_exec("echo", "from child", stdout=asyncio.subprocess.PIPE)
        out, _ = await p.communicate()
        assert out == b"from child\n" and p.returncode == 0
    asyncio.run(main())

# ---- misc stdlib ----------------------------------------------------------------------------------

@check("unittest, logging, argparse, csv, io, contextlib")
def _():
    import unittest, logging, argparse, csv, io, contextlib
    class T(unittest.TestCase):
        def test_a(self): self.assertEqual(1 + 1, 2)
        def test_b(self): self.assertRaises(ZeroDivisionError, lambda: 1 / 0)
    r = unittest.TextTestRunner(stream=io.StringIO()).run(unittest.defaultTestLoader.loadTestsFromTestCase(T))
    assert r.wasSuccessful() and r.testsRun == 2
    buf = io.StringIO(); h = logging.StreamHandler(buf); lg = logging.getLogger("t"); lg.addHandler(h); lg.warning("careful")
    assert "careful" in buf.getvalue()
    assert argparse.ArgumentParser().parse_args([]) is not None
    out = io.StringIO(); csv.writer(out).writerows([["a", "b,c"], [1, 2]])
    assert list(csv.reader(io.StringIO(out.getvalue()))) == [["a", "b,c"], ["1", "2"]]
    with contextlib.redirect_stdout(io.StringIO()) as o: print("x")
    assert o.getvalue() == "x\n"

@check("platform, os.uname, getpid, environ, signals (registration only)")
def _():
    import platform, signal
    assert os.getpid() >= 1 and os.uname().sysname == "Linux"
    got = []
    signal.signal(signal.SIGUSR1, lambda *a: got.append(1))
    return f"machine={platform.machine()} nodename={os.uname().nodename}"

@check("memory: a 150 MiB allocation is usable")
def _():
    b = bytearray(150 * 1024 * 1024)
    for i in range(0, len(b), 4096 * 64):
        b[i] = 1
    assert b[0] == 1 and len(b) == 150 * 1024 * 1024

@check("speed (informational)")
def _():
    t0 = time.perf_counter(); s = 0
    for i in range(1_000_000):
        s += i * i
    dt = time.perf_counter() - t0
    NOTES.append(f"1M-iteration loop: {dt:.2f}s")
    return f"1M-iteration loop {dt:.2f}s"

failed = [(n, d) for n, ok, d in results if not ok]
print(f"\nSMOKE: {len(results) - len(failed)} passed, {len(failed)} failed", flush=True)
for n, d in failed:
    print(f"  FAILED {n}: {d}", flush=True)
sys.exit(1 if failed else 0)
