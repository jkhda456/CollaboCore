#!/usr/bin/env python3
"""collaboCore build driver. Run it with python3; it never relies on a file's executable bit, so a
checkout that lost its permissions (a commit made on Windows, a copy through a shared folder)
builds the same as one that kept them.

  python3 build.py                   every step: tools kernel userspace python nettools addons engine web runtime
  python3 build.py engine web        only the named steps, in the order given
  python3 build.py runtime release   steps, then one command (a command never builds by itself)
  python3 build.py status            what each step has produced so far

Steps
  tools       fetch what the build needs into .tools (node, cmake/ninja, binaryen, rust)
              and clone kernel/linux and third_party/distro
  kernel      the WebAssembly Linux kernel                       -> kernel/linux/vmlinux.wasm
  userspace   musl, compiler-rt, busybox, /init and guest tools  -> userspace/out/initramfs.cpio
  python      CPython 3.13 for the guest: the stdlib's C libraries, Rust for the guest,
              pip and the openai SDK (all sources pinned)        -> python/out/python.cpio
  nettools    curl, ssh (dropbear), git for the guest; after python (its OpenSSL, zlib)
                                                                 -> nettools/out/tools.cpio
  addons      the optional overlays under addons/ (addons/README.md); after python
              (its Rust for the guest)                          -> addons/*/out/*.cpio
  engine      collect the kernel, images and host JS             -> dist/engine
  web         the browser build                                  -> dist/web
  runtime     build the native engine and package it             -> dist/runtime/collabo-core-*

Commands
  test [--boot|--all]        run the checks (tests/run.sh)
  clean [dist|build|all]     remove build output (DRY=1 to look first)
  release [--build]          collect archives and checksums      -> dist/release
  testkit [DIR]              runtime + end-to-end test for another machine (PLATFORMS=...)
  export DIR                 copy the source tree, ready to commit, to DIR
  deploy [DIR] [--release|--dist|--source|--check]
                             source + dist/ to DIR (default: $COLLABO_DEPLOY_DIR or .deploy-target),
                             then verify checksums; DRY=1 to look first
  sync-web [--dry-run|--check]   copy dist/web to the VirtualBox share
  deps                       list the system tools this machine is missing
  perms [--git]              mark every script executable (by its #! line); --git also records
                             the mode in the git index, so the next commit carries it

Environment: FORCE=stage[,stage] redoes stages inside userspace/python, JOBS=n for the kernel,
PLATFORMS="linux-x64 ..." for runtime/release/testkit. First time on a new machine: `deps`.
"""
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent

# name: (script, what it produces)
STEPS = {
    "tools":     ("scripts/bootstrap-tools.sh", ".tools/cargo/bin/cargo"),
    "kernel":    ("kernel/build.sh",            "kernel/linux/vmlinux.wasm"),
    "userspace": ("userspace/build.sh",         "userspace/out/initramfs.cpio"),
    "python":    ("python/build.sh",            "python/out/python.cpio"),
    "nettools":  ("nettools/build.sh",          "nettools/out/tools.cpio"),
    "addons":    ("addons/build.sh",            "addons/claude-code/out/claude-code.cpio"),
    "engine":    ("scripts/build-engine.sh",    "dist/engine/kernel/vmlinux.wasm"),
    "web":       ("scripts/build-web.sh",       "dist/web/index.html"),
    "runtime":   ("scripts/package-runtime.sh", "dist/runtime"),
}

COMMANDS = {
    "test":     "tests/run.sh",
    "clean":    "scripts/clean.sh",
    "release":  "scripts/release.sh",
    "testkit":  "scripts/make-testkit.sh",
    "export":   "scripts/export-source.sh",
    "deploy":   "scripts/deploy.sh",
    "sync-web": "scripts/sync-web.sh",
}

# System tools the steps call. .tools supplies node, pnpm, cmake, ninja, wasm-opt and cargo.
SYSTEM_TOOLS = ["bash", "git", "curl", "rsync", "tar", "xz", "patch", "make", "perl", "bc", "flex",
                "bison", "pkg-config", "dtc", "wasm2wat", "tic", "clang-19", "clang++-19", "ld.lld-19",
                "wasm-ld-19", "llvm-ar-19", "llvm-nm-19", "llvm-objcopy-19", "llvm-objdump-19",
                "llvm-ranlib-19", "llvm-readelf-19", "llvm-strip-19"]
APT = ("sudo apt-get install -y make flex bison bc pkg-config libncurses-dev device-tree-compiler "
       "wabt clang-19 lld-19 llvm-19 rsync python3 git curl xz-utils patch ncurses-bin")

# Directories that hold no scripts of ours: build output, downloads and third-party clones.
NOT_OURS = {".git", ".tools", "dist", "kernel/linux", "third_party", "userspace/src", "userspace/build",
            "userspace/out", "userspace/sysroot", "userspace/kheaders", "python/src", "python/host",
            "python/build-host", "python/build-wasm", "python/build-zlib", "python/zlib", "python/stage",
            "python/deps", "python/build-deps", "python/build-native", "python/build-etc",
            "python/out", "addons/claude-code/build", "addons/claude-code/out", "nettools/src", "nettools/build", "nettools/stage", "nettools/out", "engine/target", "tests/node_modules", "node_modules", ".dart_tool", "build",
            "ephemeral", "Pods"}


def usage(out=sys.stdout):
    out.write(__doc__)


def scripts():
    """Every file of ours that starts with #!: those are the ones meant to be run."""
    found = []
    for base, dirs, files in os.walk(ROOT):
        rel = Path(base).relative_to(ROOT)
        dirs[:] = [d for d in dirs if d not in NOT_OURS and (rel / d).as_posix() not in NOT_OURS]
        for name in files:
            path = Path(base, name)
            try:
                with open(path, "rb") as f:
                    if f.read(2) == b"#!":
                        found.append(path)
            except OSError:
                pass
    return sorted(found)


def fix_perms(git=False, quiet=False):
    changed = []
    for path in scripts():
        mode = path.stat().st_mode
        if mode & 0o111 != 0o111:
            try:
                path.chmod(mode | 0o755)
                changed.append(path)
            except OSError:
                pass  # a shared folder (vboxsf, SMB) has no modes to set
    if not quiet:
        for path in changed:
            print(f"  +x {path.relative_to(ROOT).as_posix()}")
        print(f"perms: {len(changed)} changed, {len(scripts())} scripts")
    if git:
        if not (ROOT / ".git").exists():
            sys.exit("perms --git: this tree is not a git work tree")
        rel = [p.relative_to(ROOT).as_posix() for p in scripts()]
        tracked = subprocess.run(["git", "ls-files", "-s", "--", *rel], cwd=ROOT, check=True,
                                 capture_output=True, text=True).stdout.splitlines()
        plain = [line.split("\t", 1)[1] for line in tracked if line.startswith("100644")]
        if plain:
            subprocess.run(["git", "update-index", "--chmod=+x", "--", *plain], cwd=ROOT, check=True)
        for p in plain:
            print(f"  git +x {p}")
        print(f"perms --git: {len(plain)} index entries now 100755 (commit to keep them)")


def bash():
    found = shutil.which("bash")
    if found:
        return found
    # Git for Windows puts bash here without adding it to PATH.
    for guess in (r"C:\Program Files\Git\bin\bash.exe", r"C:\Program Files\Git\usr\bin\bash.exe"):
        if Path(guess).exists():
            return guess
    sys.exit("bash is needed to run the build steps (on Windows: Git for Windows or MSYS2)")


def run(script, args=()):
    cmd = [bash(), str(ROOT / script), *args]
    # The scripts run Python as $PYTHON: this interpreter. On Windows `python3` is usually the
    # Microsoft Store placeholder, which prints "Python" and fails.
    env = {**os.environ, "PYTHON": sys.executable.replace("\\", "/")}
    code = subprocess.run(cmd, cwd=ROOT, env=env).returncode
    if code:
        sys.exit(f"FAILED ({code}): {script} {' '.join(args)}".rstrip())


def deps():
    missing = [t for t in SYSTEM_TOOLS if not shutil.which(t)]
    if sys.version_info < (3, 8):
        missing.append("python3 >= 3.8")
    for tool, name in ((".tools/node/bin/node", "node"), (".tools/cmake/bin/cmake", "cmake"),
                       (".tools/binaryen/bin/wasm-opt", "wasm-opt"), (".tools/cargo/bin/cargo", "cargo")):
        print(f"  {name:10} {'ok' if (ROOT / tool).exists() else 'missing: python3 build.py tools'}")
    for clone in ("kernel/linux", "third_party/distro"):
        print(f"  {clone:18} {'ok' if (ROOT / clone).is_dir() else 'missing: python3 build.py tools'}")
    if missing:
        print(f"missing system tools: {' '.join(missing)}\ninstall with:  {APT}")
        sys.exit(1)
    print("system tools ok")


def status():
    for name, (_, out) in STEPS.items():
        path = ROOT / out
        if name == "runtime":
            built = sorted(p.name for p in path.glob("collabo-core-*") if (p / "manifest.json").exists())
            print(f"  {name:10} {', '.join(built) if built else '-'}")
        elif path.exists():
            size = path.stat().st_size if path.is_file() else 0
            print(f"  {name:10} {out}" + (f"  ({size / 1048576:.1f} MB)" if size > 1048576 else ""))
        else:
            print(f"  {name:10} - ({out})")


def main(argv):
    if argv and argv[0] in ("-h", "--help", "help"):
        return usage()
    # Before anything else: a checkout without permissions gets them back, because some of these
    # files are run by other tools rather than by us (wasm-cc is make's CC, bzip2 is on PATH).
    fix_perms(quiet=True)

    # Steps first, then at most one command with its own arguments: `runtime release` builds the
    # runtime and then collects the release.
    at = next((i for i, arg in enumerate(argv) if arg in COMMANDS), len(argv))
    steps, command = argv[:at], argv[at:]
    if command:
        misplaced = [arg for arg in command[1:] if arg in STEPS]
        if misplaced:
            sys.exit(f"`{command[0]}` does not build anything; name the steps before it: "
                     f"python build.py {' '.join(misplaced)} {command[0]}")
    if steps and steps[0] == "perms":
        return fix_perms(git="--git" in steps[1:])
    if steps and steps[0] == "deps":
        return deps()
    if steps and steps[0] == "status":
        return status()

    if not steps and not command:
        steps = list(STEPS)
    unknown = [s for s in steps if s not in STEPS]
    if unknown:
        print(f"unknown step or command: {' '.join(unknown)}\n", file=sys.stderr)
        usage(sys.stderr)
        sys.exit(2)
    if platform.system() != "Linux" and set(steps) - {"runtime"}:
        print("note: only `runtime` is meant to be built off Linux (the images come from a Linux build)",
              file=sys.stderr)
    for step in steps:
        print(f"######## {step}", flush=True)
        if step == "runtime" and platform.system() == "Windows":
            # No bash needed: the PowerShell twin of package-runtime.sh.
            ps1 = ROOT / "scripts/package-runtime-windows.ps1"
            code = subprocess.run(["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(ps1)],
                                  cwd=ROOT).returncode
            if code:
                sys.exit(f"FAILED ({code}): {ps1.relative_to(ROOT)}")
            continue
        run(STEPS[step][0])
    if command:
        print(f"######## {command[0]}", flush=True)
        run(COMMANDS[command[0]], command[1:])


if __name__ == "__main__":
    main(sys.argv[1:])
