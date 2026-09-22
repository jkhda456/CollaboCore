"""The standard library's extension modules and the preinstalled packages, inside the guest.

Each check does real work with its module rather than only importing it. Prints one line per
check and `STDLIB: n passed, m failed`; exits 1 on any failure.
"""
import os
import subprocess
import sys
import tempfile
import traceback

results = []


def check(name):
    def run(fn):
        try:
            detail = fn()
            results.append((name, True))
            print(f"ok    {name}{': ' + str(detail) if detail else ''}", flush=True)
        except Exception:
            results.append((name, False))
            print(f"FAIL  {name}\n" + "".join("      " + l for l in traceback.format_exc().splitlines(True)), flush=True)
        return fn
    return run


@check("ssl: OpenSSL, the default trust store")
def _():
    import ssl
    context = ssl.create_default_context()
    roots = context.cert_store_stats()["x509_ca"]
    assert roots > 100, f"only {roots} CA certificates loaded from {ssl.get_default_verify_paths()}"
    assert ssl.HAS_TLSv1_3
    return f"{ssl.OPENSSL_VERSION}, {roots} roots"


@check("hashlib: OpenSSL algorithms")
def _():
    import hashlib
    assert "sha3_256" in hashlib.algorithms_available and "blake2b" in hashlib.algorithms_available
    assert hashlib.sha256(b"abc").hexdigest().startswith("ba7816bf")
    assert hashlib.new("sha512_256", b"").hexdigest().startswith("c672b8d1")
    assert hashlib.pbkdf2_hmac("sha256", b"pw", b"salt", 1000).hex().startswith("0a382535")
    import _hashlib  # the OpenSSL-backed module, not only the builtin fallbacks
    return f"{len(hashlib.algorithms_available)} algorithms"


@check("bz2, lzma, zlib, gzip round trips")
def _():
    import bz2, gzip, lzma, zlib
    data = os.urandom(1000) * 50
    for module in (bz2, lzma, zlib, gzip):
        assert module.decompress(module.compress(data)) == data, module.__name__
    assert lzma.decompress(lzma.compress(data, format=lzma.FORMAT_XZ, preset=6)) == data


@check("sqlite3: in memory and on disk, FTS5, JSON, math")
def _():
    import sqlite3
    with tempfile.TemporaryDirectory() as d:
        db = sqlite3.connect(os.path.join(d, "t.db"))
        db.execute("create table t (k integer primary key, v text)")
        db.executemany("insert into t (v) values (?)", [(f"row {i}",) for i in range(1000)])
        db.commit()
        assert db.execute("select count(*) from t").fetchone()[0] == 1000
        db.execute("create virtual table f using fts5(body)")
        db.execute("insert into f values ('the quick brown fox'), ('lazy dog')")
        assert db.execute("select body from f where f match 'fox'").fetchall() == [("the quick brown fox",)]
        assert db.execute("select json_extract('{\"a\": [1, 2]}', '$.a[1]'), sqrt(16)").fetchone() == (2, 4.0)
        db.close()
    return sqlite3.sqlite_version


@check("readline and curses (terminfo)")
def _():
    import curses, readline
    assert readline.backend == "editline", readline.backend  # libedit, not GPL-3 GNU readline
    readline.add_history("print(1)")
    assert readline.get_current_history_length() == 1
    curses.setupterm("xterm-256color", sys.stdout.fileno() if sys.stdout.isatty() else os.open(os.devnull, os.O_WRONLY))
    assert curses.tigetnum("colors") == 256
    import curses.panel
    return f"{readline.backend} {readline._READLINE_LIBRARY_VERSION}"


@check("mmap (copy-based)")
def _():
    import mmap
    with tempfile.NamedTemporaryFile(delete=False) as f:
        f.write(b"hello world\n")
    with open(f.name, "r+b") as f2, mmap.mmap(f2.fileno(), 0) as m:
        assert m.find(b"world") == 6 and m.readline() == b"hello world\n" and m.find(b"world") == -1
        m[0:5] = b"HELLO"
    assert open(f.name, "rb").read() == b"HELLO world\n"
    os.unlink(f.name)


@check("more of the stdlib: decimal, uuid, zipfile, dbm.sqlite3, asyncio, multiprocessing, pydoc")
def _():
    import asyncio, dbm.sqlite3, decimal, multiprocessing, pydoc, uuid, zipfile, io
    assert str(decimal.Decimal(1) / decimal.Decimal(7))[:8] == "0.142857"
    assert uuid.uuid4().version == 4
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_LZMA) as z:
        z.writestr("a.txt", "x" * 1000)
    assert zipfile.ZipFile(buffer).read("a.txt") == b"x" * 1000
    with tempfile.TemporaryDirectory() as d, dbm.sqlite3.open(os.path.join(d, "db"), "c") as db:
        db[b"k"] = b"v"
        assert db[b"k"] == b"v"
    assert asyncio.run(asyncio.sleep(0, "done")) == "done"
    assert pydoc.render_doc(len, renderer=pydoc.plaintext).strip()


@check("pip is installed and consistent (pip list, pip check)")
def _():
    run = lambda *a: subprocess.run([sys.executable, "-m", "pip", "--disable-pip-version-check", *a],
                                    check=True, capture_output=True, text=True).stdout
    assert run("--version").startswith("pip "), run("--version")
    names = {line.split()[0].lower().replace("_", "-") for line in run("list").splitlines()[2:]}
    assert {"pip", "openai", "pydantic", "pydantic-core", "jiter"} <= names, names
    run("check")
    # The console scripts are there too.
    assert os.access("/usr/bin/pip3", os.X_OK) and os.access("/usr/bin/pip", os.X_OK)


@check("venv with its own pip (ensurepip), and a program run in it")
def _():
    with tempfile.TemporaryDirectory() as d:
        env = os.path.join(d, "env")
        subprocess.run([sys.executable, "-m", "venv", env], check=True, capture_output=True)
        out = subprocess.run([os.path.join(env, "bin", "python"), "-c", "import sys; print(sys.prefix != sys.base_prefix)"],
                             check=True, capture_output=True, text=True).stdout
        assert out.strip() == "True", out
        pip = subprocess.run([os.path.join(env, "bin", "pip"), "--version"], check=True, capture_output=True, text=True).stdout
        assert env in pip, pip


@check("built-in Rust modules")
def _():
    names = [n for n in ("pydantic_core._pydantic_core", "jiter.jiter") if n in sys.builtin_module_names]
    assert len(names) == 2, sys.builtin_module_names


@check("jiter: parsing, partial JSON")
def _():
    import jiter
    assert jiter.from_json(b'{"a": [1, 2.5, "x"], "b": null}') == {"a": [1, 2.5, "x"], "b": None}
    assert jiter.from_json(b'{"a": [1, 2', partial_mode=True) == {"a": [1, 2]}
    return jiter.__version__


@check("pydantic v2 on pydantic-core")
def _():
    import pydantic
    from pydantic import BaseModel, Field, ValidationError

    class Item(BaseModel):
        name: str
        price: float = Field(gt=0)
        tags: list[str] = []

    item = Item.model_validate_json('{"name": "pen", "price": "1.5", "tags": ["a"]}')
    assert item.price == 1.5 and item.model_dump_json() == '{"name":"pen","price":1.5,"tags":["a"]}'
    try:
        Item(name="x", price=-1)
    except ValidationError as error:
        assert error.errors()[0]["type"] == "greater_than"
    else:
        raise AssertionError("no validation error")
    import pydantic_core
    return f"pydantic {pydantic.VERSION}, pydantic-core {pydantic_core.__version__}"


@check("openai SDK: a client, requests built, a response parsed")
def _():
    import httpx2
    import openai

    seen = []

    def answer(request):
        seen.append(request)
        return httpx2.Response(200, json={
            "id": "c1", "object": "chat.completion", "created": 0, "model": "m",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "hello from the mock"}}]})

    client = openai.OpenAI(api_key="k", base_url="http://mock.invalid/v1",
                           http_client=httpx2.Client(transport=httpx2.MockTransport(answer)))
    completion = client.chat.completions.create(model="m", messages=[{"role": "user", "content": "hi"}])
    assert completion.choices[0].message.content == "hello from the mock"
    assert seen[0].url.path == "/v1/chat/completions" and seen[0].headers["authorization"] == "Bearer k"
    return f"openai {openai.__version__}"


passed = sum(ok for _, ok in results)
print(f"STDLIB: {passed} passed, {len(results) - passed} failed")
sys.exit(0 if passed == len(results) else 1)
