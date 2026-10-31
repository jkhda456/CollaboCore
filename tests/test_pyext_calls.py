#!/usr/bin/env python3
"""Python extension modules whose functions have other types than the pointers the interpreter
calls them through (python/patches/cpython-call-trampoline.patch, engine/src/dylink.rs `params`):
tests/pyext/fpcast.c, built here as a side module (userspace/bin/wasm-cc -shared) against the
interpreter's headers, imported in the guest. METH_NOARGS methods declared with only self (as
python-xxhash's are), or with no parameters, METH_VARARGS | METH_KEYWORDS without the keywords',
METH_VARARGS with one, getters and setters without the closure — each called with the parameters it
has, where WebAssembly would trap ("indirect call type mismatch"); also once the interpreter has
specialized the calls, in a thread older than the module, and a function with more parameters than
any call (SystemError).

  python3 tests/test_pyext_calls.py   engine/target/release/collabo-core-engine and the build's images
                                      (python/out/python.cpio), or the runtime folder COLLABO_RUNTIME names
"""
import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

runtime = os.environ.get("COLLABO_RUNTIME")
if runtime:
    images = Path(runtime) / "app" / "images"
    ENGINE = [str(Path(runtime) / "bin" / "collabo-core-engine"), "--kernel", str(images / "vmlinux.wasm"),
              "--initramfs", str(images / "initramfs.cpio"), "--python-image", str(images / "python.cpio")]
else:
    ENGINE = [str(ROOT / "engine/target/release/collabo-core-engine"), "--kernel", str(ROOT / "kernel/linux/vmlinux.wasm"),
              "--initramfs", str(ROOT / "userspace/out/initramfs.cpio"), "--python-image", str(ROOT / "python/out/python.cpio")]
INCLUDE = ROOT / "python/stage/usr/include/python3.13"

CHECK = r'''
import sys, threading
ready, go, seen = threading.Event(), threading.Event(), []
def early():
    ready.set()
    go.wait()
    import fpcast
    seen.append(fpcast.in_thread())
t = threading.Thread(target=early)
t.start()
ready.wait()
sys.path.insert(0, "/t")
import fpcast
print("noargs:", fpcast.one_param())
print("void:", fpcast.no_params())
print("o:", fpcast.exact(21))
print("no kwargs:", fpcast.no_kwargs(1, 2, x=3))
print("extra kwargs:", fpcast.extra_kwargs(1, 2))
try:
    fpcast.four_params()
except SystemError as e:
    print("too many:", e)
b = fpcast.Box()
b.value = 5
print("getset:", b.value, b.tripled)
try:
    del b.value
except TypeError as e:
    print("setter error:", e)
def loop():
    total = 0
    for i in range(500):
        total += b.bump() + b.add(1) + fpcast.exact(i)
    return total
print("loop:", loop(), b.value)
print("unbound:", fpcast.Box.bump(b), list(map(fpcast.Box.add, [b], [10])))
go.set()
t.join()
print("thread:", seen)
'''

EXPECTED = [
    "noargs: one param",
    "void: no params",
    "o: 42",
    "no kwargs: 2",                              # the keyword argument goes nowhere, as natively
    "extra kwargs: 2 args, kwargs NULL",
    "too many: Handler takes too many arguments",
    "getset: 5 15",
    "setter error: value cannot be deleted",
    "loop: 755000 1005",                         # sum of 13 + 6i for i < 500; 5 + 1000
    "unbound: 1006 [1016]",
    "thread: ['in a thread']",
]

for need in (Path(ENGINE[0]), INCLUDE, Path(ENGINE[ENGINE.index("--python-image") + 1])):
    if not need.exists():
        print(f"missing {need}", file=sys.stderr)
        sys.exit(2)

work = Path(tempfile.mkdtemp(prefix="collabo-pyext-"))
os.chmod(work, 0o755)
env = dict(os.environ, WASM_LD=os.environ.get("WASM_LD", str(ROOT / ".tools/rust-wasm/bin/wasm-ld")))
if not Path(env["WASM_LD"]).exists():
    env.pop("WASM_LD")
subprocess.run([str(ROOT / "userspace/bin/wasm-cc"), "-O2", "-fPIC", "-shared", f"-I{INCLUDE}", str(ROOT / "tests/pyext/fpcast.c"),
                "-o", str(work / "fpcast.cpython-313.so")], check=True, env=env)
(work / "check.py").write_text(CHECK)

p = subprocess.run([*ENGINE[:1], "exec", *ENGINE[1:], "--no-network", "--mount", f"{work}:/t", "--", "python3", "/t/check.py"],
                   capture_output=True, text=True, timeout=300)
lines = p.stdout.replace("\r", "").splitlines()
failures = 0
for want in EXPECTED:
    ok = want in lines
    print(("ok   " if ok else "FAIL ") + want)
    failures += not ok
if p.returncode != 0:
    print(f"FAIL exit status {p.returncode}")
    failures += 1
if failures:
    print("     output:\n     " + "\n     ".join(lines[-30:]) + "\n     " + p.stderr[-1500:].replace("\n", "\n     "))
print("all extension call checks passed" if not failures else f"{failures} extension call checks failed")
sys.exit(1 if failures else 0)
