#!/usr/bin/env python3
"""dlopen in the guest (addons/mod: the C library's dlopen, userspace/patches/musl-wasm-dlopen.patch,
over the engine's dylink imports, engine/src/dylink.rs): tests/dlopen/main.c, a program linked with
-rdynamic, loads tests/dlopen/lib.c, a library linked with -shared (userspace/bin/wasm-cc), and
checks what a library does with its program: its functions, data and constructor; functions and
data of the program, and pointers to them that compare equal; setjmp/longjmp through the program's
C library; thread-local storage, per thread; a thread that existed before the library and comes to
it through a mutex, or is handed a pointer from dlsym through one; a library that needs another (DT_NEEDED, found beside it); two threads that
load a library each at once, one waiting for the other's; the floating-point flags; weak definitions (its own — which wasm-ld imports
too —, and one the program has, which wins) and a weak symbol nothing defines; dlerror. Only the
native engine loads libraries: on the JavaScript host (userspace/test-boot.sh, under Node) the same
program runs, and its dlopen fails with an error.

  python3 tests/test_dlopen.py     engine/target/release/collabo-core-engine and the build's images,
                                   or the runtime folder COLLABO_RUNTIME names (bin/, app/images/)
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
              "--initramfs", str(images / "initramfs.cpio")]
else:
    ENGINE = [str(ROOT / "engine/target/release/collabo-core-engine"), "--kernel", str(ROOT / "kernel/linux/vmlinux.wasm"),
              "--initramfs", str(ROOT / "userspace/out/initramfs.cpio")]

EXPECTED = [
    "missing: error ok",
    "again: same handle",
    "entry: 1339",                      # 5 + the program's counter (100) + the library's data (1234)
    "data: 1234",
    "constructor: 42",
    "own pointer: 42",
    "program pointer equal: 1",
    "callback: 16",
    "setjmp: 7",
    "format: lib-3-4",
    "weak: 82 9 -1",                    # its own lib_weak (8, 9), the program's program_weak (2), no absent
    "missing symbol: error ok",
    "default handle: program_add",
    "tls: main 1 2, thread 12",
    "early thread: 1335",
    "handoff: 77",                      # a function pointer from dlsym, called in a thread that existed
    "fenv: divbyzero",
    "needed: 10",                       # top.so's dep_value() * 2, from libdep.so
    "concurrent loads: ok ok",          # each library's storage set up in both threads
    "done",
]

work = Path(tempfile.mkdtemp(prefix="collabo-dlopen-"))
os.chmod(work, 0o755)
cc = str(ROOT / "userspace/bin/wasm-cc")
env = dict(os.environ, WASM_LD=os.environ.get("WASM_LD", str(ROOT / ".tools/rust-wasm/bin/wasm-ld")))
if not Path(env["WASM_LD"]).exists():
    env.pop("WASM_LD")
link = ["-Wl,--import-memory", "-Wl,--max-memory=4294967296", "-Wl,--shared-memory", "-Wl,--export-table"]
subprocess.run([cc, "-O2", "-fPIC", "-shared", str(ROOT / "tests/dlopen/lib.c"), "-o", str(work / "lib.so")], check=True, env=env)
# for the concurrent loads: top.so, which needs libdep.so (linked against it), and a copy of lib.so
# (another file: another library)
subprocess.run([cc, "-O2", "-fPIC", "-shared", str(ROOT / "tests/dlopen/dep.c"), "-o", str(work / "libdep.so")], check=True, env=env)
subprocess.run([cc, "-O2", "-fPIC", "-shared", str(ROOT / "tests/dlopen/top.c"), str(work / "libdep.so"), "-o", str(work / "top.so")],
               check=True, env=env)
(work / "lib2.so").write_bytes((work / "lib.so").read_bytes())
subprocess.run([cc, "-O2", "-fvisibility=default", "-rdynamic", str(ROOT / "tests/dlopen/main.c"), "-o", str(work / "main"), *link],
               check=True, env=env)
os.chmod(work / "main", 0o755)

p = subprocess.run([*ENGINE[:1], "exec", *ENGINE[1:], "--no-network", "--mount", f"{work}:/t", "--", "/t/main", "/t/lib.so", "/t/top.so", "/t/lib2.so"],
                   capture_output=True, text=True, timeout=300)
lines = p.stdout.replace("\r", "").splitlines()
failures = 0
for want in EXPECTED:
    ok = want in lines
    print(("ok   " if ok else "FAIL ") + want)
    failures += not ok
if failures:
    print("     output:\n     " + "\n     ".join(lines[-30:]) + "\n     " + p.stderr[-1500:].replace("\n", "\n     "))

# The JavaScript host has no dynamic loading: the program (which has dlopen) runs all the same, its
# dlopen fails with an error (the dylink calls are syscalls its kernel answers with ENOSYS).
boot = ROOT / "userspace/test-boot.sh"
if boot.exists() and (ROOT / "dist/engine/kernel").is_dir():
    tree = work / "tree"
    tree.mkdir()
    for f in ("main", "lib.so"):
        (tree / f).write_bytes((work / f).read_bytes())
        os.chmod(tree / f, 0o755)
    subprocess.run([sys.executable, str(ROOT / "userspace/mkinitramfs.py"), "--overlay", "-o", str(work / "t.cpio"),
                    "--tree", f"opt/dlopen={tree}"], check=True, capture_output=True)
    js = subprocess.run(["bash", str(boot), str(ROOT / "userspace/out/initramfs.cpio"), "/opt/dlopen/main /opt/dlopen/lib.so"],
                        env=dict(os.environ, EXTRA_CPIO=str(work / "t.cpio"), TEST_TIMEOUT_MS="300000"),
                        capture_output=True, text=True, timeout=400)
    want = "dlopen failed: /opt/dlopen/lib.so: this host cannot load shared libraries"
    ok = want in js.stdout.replace("\r", "").splitlines()
    print(("ok   " if ok else "FAIL ") + "JavaScript host: " + want)
    if not ok:
        failures += 1
        print("     output:\n     " + "\n     ".join(js.stdout.replace("\r", "").splitlines()[-15:]))
print("all dlopen checks passed" if not failures else f"{failures} dlopen checks failed")
sys.exit(1 if failures else 0)
