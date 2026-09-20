#!/usr/bin/env python3
"""Nix NAR sha256 (SRI) of a directory tree, to check a source tree against the hash that
distro's package.nix pins (fetchzip/fetchFromGitHub hash = "sha256-...").
    nar-hash.py DIR [EXPECTED_SRI]      exit 1 on mismatch"""
import base64, hashlib, os, struct, sys

def nar(path, h):
    def s(b):
        if isinstance(b, str): b = b.encode()
        h.update(struct.pack("<Q", len(b))); h.update(b); h.update(b"\0" * (-len(b) % 8))
    def node(p):
        s("("); s("type")
        if os.path.islink(p):
            s("symlink"); s("target"); s(os.readlink(p))
        elif os.path.isdir(p):
            s("directory")
            for name in sorted(os.listdir(p), key=lambda n: n.encode()):
                s("entry"); s("("); s("name"); s(name); s("node"); node(os.path.join(p, name)); s(")")
        else:
            s("regular")
            if os.stat(p).st_mode & 0o100: s("executable"); s("")
            s("contents"); s(open(p, "rb").read())
        s(")")
    s("nix-archive-1"); node(path)

h = hashlib.sha256(); nar(sys.argv[1], h)
got = "sha256-" + base64.b64encode(h.digest()).decode()
print(got)
if len(sys.argv) > 2 and got != sys.argv[2]:
    print(f"MISMATCH: expected {sys.argv[2]}", file=sys.stderr); sys.exit(1)
