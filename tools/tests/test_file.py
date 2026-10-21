"""file(1) in the guest (tools.cpio): text, scripts, images, archives, links and the guest's own
wasm programs; --mime; -z, whose gzip, bzip2, xz and lzma are decompressed in-process and zstd by
the `zstd` command (posix_spawnp); a pipe on standard input.

    COLLABO_RUNTIME=dist/runtime/collabo-core-linux-x64 python3 tools/tests/test_file.py
    TOOLS_IMAGE=tools/out/tools.cpio ...   another tools.cpio
"""
import json, os, re, subprocess, sys
RT = os.environ["COLLABO_RUNTIME"]
fails = []
def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f"\n     {str(detail)[-1500:]}"))
    if not ok: fails.append(name)

entry = json.load(open(os.path.join(RT, "manifest.json")))["entry"]
images = {"--tools-image": os.environ.get("TOOLS_IMAGE")}
argv, words = [os.path.join(RT, entry[0])], iter(entry[1:])
for word in words:
    argv.append(word)
    if word.startswith("--"):
        value = next(words)
        argv.append(images.get(word) or (os.path.join(RT, value) if value.startswith("app/") else value))

def guest(script, timeout=180):
    """Output of a shell script run in the guest (after the prompt)."""
    out = subprocess.run([argv[0], "exec"] + argv[1:] + ["--no-network", "--", "sh", "-c", script],
                         capture_output=True, text=True, timeout=timeout).stdout.replace("\r", "")
    return out.split("root@collabo:~# ", 1)[-1]

out = guest(r'''
cd /tmp
file --version
printf 'hello, this is plain ascii text\n' > a.txt
printf '한글 텍스트\n' > u.txt
printf '#!/bin/sh\necho hi\n' > s.sh
: > empty; mkdir d; ln -s a.txt l
printf '\211PNG\r\n\032\n\0\0\0\rIHDR\0\0\0\020\0\0\0\010\010\006\0\0\0' > p.png
printf '{"a": [1, 2]}\n' > j.json
tar cf t.tar a.txt u.txt
gzip -c t.tar > t.tar.gz; bzip2 -c t.tar > t.tar.bz2; xz -c t.tar > t.tar.xz
xz --format=lzma -c t.tar > t.tar.lzma; zstd -q -c t.tar > t.tar.zst
file a.txt u.txt s.sh empty d l p.png j.json t.tar t.tar.gz /usr/bin/file /usr/share/misc/magic.mgc
file -L l
file -b --mime-type p.png
file -i a.txt
file -z t.tar.gz t.tar.bz2 t.tar.xz t.tar.lzma t.tar.zst
cat t.tar.xz | file -z -
file /nonexistent; echo "rc=$?"
file -E /nonexistent; echo "rc=$?"
''')
version = re.search(r"^file-(\d+\.\d+)$", out, re.M)
check("file --version", version, out)
check("the magic database is /usr/share/misc/magic", "magic file from /usr/share/misc/magic" in out, out)
expected = {
    "a.txt": "ASCII text",
    "u.txt": "Unicode text, UTF-8 text",
    "s.sh": "POSIX shell script, ASCII text executable",
    "empty": "empty",
    "d": "directory",
    "l": "symbolic link to a.txt",
    "p.png": "PNG image data, 16 x 8, 8-bit/color RGBA, non-interlaced",
    "j.json": "JSON text data",
    "t.tar": "POSIX tar archive (GNU)",
    "t.tar.gz": "gzip compressed data, from Unix, original size modulo 2^32 3072",
    "/usr/bin/file": "WebAssembly (wasm) binary version 0x1 (MVP module)",
    "/usr/share/misc/magic.mgc": "magic binary file for file(1) cmd (version 21) (little endian)",
}
for name, what in expected.items():
    check(f"{name}: {what}", re.search(rf"^{re.escape(name)}:\s+{re.escape(what)}$", out, re.M), out)
check("-L follows the link", re.search(r"^l: ASCII text$", out, re.M), out)
check("--mime-type", re.search(r"^image/png$", out, re.M), out)
check("-i", "a.txt: text/plain; charset=us-ascii" in out, out)
for name, what in [("gz", "gzip compressed data"), ("bz2", "bzip2 compressed data"), ("xz", "XZ compressed data"),
                   ("lzma", "LZMA compressed data"), ("zst", "Zstandard compressed data")]:
    check(f"-z looks into .{name}", re.search(rf"^t\.tar\.{name}:\s+POSIX tar archive \(GNU\) \({what}", out, re.M), out)
check("-z on a pipe", re.search(r"^/dev/stdin: POSIX tar archive \(GNU\) \(XZ compressed data", out, re.M), out)
check("a missing file: a message, status 0", "/nonexistent: cannot open `/nonexistent' (No such file or directory)\nrc=0" in out, out)
check("-E: status 1", re.search(r"^rc=1$", out, re.M), out)

print(f"{'FAIL' if fails else 'PASS'}: file ({len(fails)} failed)")
sys.exit(1 if fails else 0)
