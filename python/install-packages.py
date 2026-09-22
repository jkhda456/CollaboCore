"""Install the wheels named in packages.lock into the guest's site-packages, as pip would.

    install-packages.py SITE_PACKAGES BIN_DIR DOWNLOAD_DIR

Run with the host CPython 3.13 (python/host), so the .pyc files it writes are the guest's version.
Per wheel: check its sha256, unpack it, write INSTALLER, add launchers for its console scripts to
BIN_DIR, byte-compile it, and list everything in RECORD, so pip — when a user brings one — sees
ordinary installed distributions it can upgrade or uninstall.

"native" wheels (pydantic-core, jiter) are manylinux builds: their Python files and metadata are
platform-independent and used as they are, their .so is not. The same code, compiled from the
matching sdist by build-native.sh, is linked into the interpreter as a built-in module under the
.so's module name, and their WHEEL tag says so.
"""
import base64
import configparser
import csv
import hashlib
import io
import os
import py_compile
import subprocess
import sys
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
PYTHON = "/usr/bin/python3.13"
# The guest's own platform tag (sysconfig.get_platform() is "linux-wasm", from `uname -m`), so
# pip check / pip install treat the built-in extensions as matching this machine.
TAG = "cp313-cp313-linux_wasm"


def lock():
    with open(os.path.join(HERE, "packages.lock")) as f:
        for line in f:
            if line.strip() and not line.startswith("#"):
                kind, name, version, url, sha256 = line.split()
                yield kind, name, version, url, sha256


def fetch(url, sha256, downloads):
    path = os.path.join(downloads, url.rsplit("/", 1)[1])
    if not os.path.exists(path):
        print(f"== fetch {os.path.basename(path)}")
        # curl rather than urllib: the host interpreter this runs on is built without ssl.
        subprocess.run(["curl", "-fsSL", "--retry", "3", "-o", path + ".part", url], check=True)
        os.replace(path + ".part", path)
    digest = hashlib.sha256(open(path, "rb").read()).hexdigest()
    if digest != sha256:
        os.remove(path)
        sys.exit(f"checksum mismatch: {path} (deleted)")
    return path


def record_hash(data):
    return "sha256=" + base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()


def install(wheel, native, site, bindir):
    written = {}  # path relative to site-packages -> its bytes, for RECORD
    with zipfile.ZipFile(wheel) as z:
        names = z.namelist()
        info = next(n.split("/")[0] for n in names if n.split("/")[0].endswith(".dist-info"))
        for name in names:
            if name.endswith("/") or name == f"{info}/RECORD":
                continue
            if native and name.endswith(".so"):
                continue
            data = z.read(name)
            if name == f"{info}/WHEEL" and native:
                lines = [l for l in data.decode().splitlines() if not l.startswith("Tag:")]
                data = ("\n".join(lines) + f"\nTag: {TAG}\n").encode()
            dest = os.path.join(site, name)
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            with open(dest, "wb") as f:
                f.write(data)
            written[name] = data
        entry_points = z.read(f"{info}/entry_points.txt").decode() if f"{info}/entry_points.txt" in names else ""
    written[f"{info}/INSTALLER"] = b"collabo-core\n"
    with open(os.path.join(site, info, "INSTALLER"), "wb") as f:
        f.write(written[f"{info}/INSTALLER"])

    # Console scripts, as pip writes them (the kernel runs #! scripts).
    if entry_points:
        parser = configparser.ConfigParser(delimiters=("=",))
        parser.optionxform = str
        parser.read_string(entry_points)
        for script, target in (parser["console_scripts"].items() if "console_scripts" in parser else []):
            module, _, func = target.strip().partition(":")
            body = (f"#!{PYTHON}\n# -*- coding: utf-8 -*-\nimport re\nimport sys\n"
                    f"from {module} import {func.split('.')[0]}\n"
                    f"if __name__ == \"__main__\":\n"
                    f"    sys.argv[0] = re.sub(r\"(-script\\.pyw|\\.exe)?$\", \"\", sys.argv[0])\n"
                    f"    sys.exit({func}())\n").encode()
            path = os.path.join(bindir, script)
            with open(path, "wb") as f:
                f.write(body)
            os.chmod(path, 0o755)
            written[os.path.relpath(path, site)] = body

    # Byte-compile like pip; unchecked-hash because the image's timestamps are not the sources'.
    for name in list(written):
        if name.endswith(".py") and not name.startswith(".."):
            src = os.path.join(site, name)
            pyc = py_compile.compile(src, dfile=os.path.join("/usr/lib/python3.13/site-packages", name),
                                     doraise=True, invalidation_mode=py_compile.PycInvalidationMode.UNCHECKED_HASH)
            written[os.path.relpath(pyc, site)] = open(pyc, "rb").read()

    rows = [(name, record_hash(data), len(data)) for name, data in sorted(written.items())]
    rows.append((f"{info}/RECORD", "", ""))
    out = io.StringIO()
    csv.writer(out, lineterminator="\n").writerows(rows)
    with open(os.path.join(site, info, "RECORD"), "w") as f:
        f.write(out.getvalue())
    return info, len(written)


def main():
    if sys.version_info[:2] != (3, 13):
        sys.exit("run this with the host CPython 3.13 (its .pyc files must match the guest's)")
    site, bindir, downloads = sys.argv[1:4]
    os.makedirs(site, exist_ok=True)
    os.makedirs(bindir, exist_ok=True)
    os.makedirs(downloads, exist_ok=True)
    for kind, name, version, url, sha256 in lock():
        if kind == "sdist":
            continue  # the Rust source, built by build-native.sh
        info, count = install(fetch(url, sha256, downloads), kind == "native", site, bindir)
        print(f"  {info:40} {count} files{'  (native part built in)' if kind == 'native' else ''}")


if __name__ == "__main__":
    main()
