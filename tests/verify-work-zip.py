#!/usr/bin/env python3
"""Checks the archive exported after the guest workspace scenario in tests/run.sh, using
Python's zipfile (an independent reader) - not our own code."""
import hashlib, sys, zipfile

path, busybox = sys.argv[1], sys.argv[2]
z = zipfile.ZipFile(path)
assert z.testzip() is None, "CRC error in the archive"
info = {i.filename: i for i in z.infolist()}
mode = lambda n: (info[n].external_attr >> 16) & 0o177777

assert all(i.compress_type == zipfile.ZIP_STORED for i in info.values()), "something is compressed"
assert sorted(info) == ["b.txt", "bb", "link", "src/", "src/deep/", "src/deep/main.c", "zeros"], sorted(info)
assert z.read("b.txt") == b"hello\nagain\n", "create + append + rename"
assert z.read("src/deep/main.c") == b"int main(){}\n"
assert mode("src/deep/main.c") == 0o100755, oct(mode("src/deep/main.c"))
assert mode("b.txt") == 0o100644
assert mode("link") & 0o170000 == 0o120000 and z.read("link") == b"src/deep/main.c", "symlink"
assert mode("src/") & 0o170000 == 0o040000
assert set(z.read("zeros")) == {0} and info["zeros"].file_size == 2 * 1024 * 1024, "2 MiB written by dd"
want = hashlib.sha256(open(busybox, "rb").read()).hexdigest()
assert hashlib.sha256(z.read("bb")).hexdigest() == want, "a 1.2 MB binary copied in the guest is bit-identical"
print("workspace archive ok:", len(info), "entries")
