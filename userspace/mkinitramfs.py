#!/usr/bin/env python3
"""Pack the agent sandbox root into a newc cpio (uncompressed) for bootMachine({ initcpio }).

Needs no root: device nodes and ownership are just cpio header fields.

    mkinitramfs.py --init out/init.wasm --busybox out/busybox --etc rootfs/etc -o out/initramfs.cpio
    mkinitramfs.py --overlay --file usr/bin/x=x.wasm --link usr/bin/y=x -o extra.cpio

The kernel unpacks several cpio archives placed back to back, so an --overlay archive can be
concatenated after the base one (the page does this for optional pieces like Python).
"""
import argparse
import os
import stat
import sys

S_IFREG, S_IFDIR, S_IFLNK, S_IFCHR = 0o100000, 0o040000, 0o120000, 0o020000


class Cpio:
    def __init__(self):
        self.buf = bytearray()
        self.ino = 0
        self.seen = set()

    def add(self, path, mode, data=b"", rdev=(0, 0), nlink=1):
        path = path.lstrip("/")
        if path in self.seen:
            raise ValueError(f"duplicate entry {path}")
        self.seen.add(path)
        self.ino += 1
        name = path.encode() + b"\0"
        hdr = "070701" + "".join(
            f"{v:08x}"
            for v in (
                self.ino, mode, 0, 0,  # ino, mode, uid, gid (root)
                nlink, 0, len(data),  # nlink, mtime (reproducible), filesize
                0, 0, rdev[0], rdev[1],  # devmajor, devminor, rdevmajor, rdevminor
                len(name), 0,  # namesize, check
            )
        )
        self.buf += hdr.encode() + name
        self.buf += b"\0" * (-len(self.buf) % 4)
        self.buf += data
        self.buf += b"\0" * (-len(self.buf) % 4)

    def dir(self, path, perm=0o755):
        self.add(path, S_IFDIR | perm, nlink=2)

    def file(self, path, data, perm=0o644):
        self.add(path, S_IFREG | perm, data)

    def symlink(self, path, target):
        self.add(path, S_IFLNK | 0o777, target.encode())

    def chardev(self, path, major, minor, perm=0o600):
        self.add(path, S_IFCHR | perm, rdev=(major, minor))

    def finish(self):
        self.add("TRAILER!!!", 0)
        return bytes(self.buf)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--init", help="wasm binary installed as /init (required unless --overlay)")
    ap.add_argument("--busybox", help="busybox `make install` tree (bin/, sbin/, usr/ ...) (required unless --overlay)")
    ap.add_argument("--overlay", action="store_true",
                    help="an extra archive to concatenate after the base one: no /init, busybox or "
                         "base directories, only what --file/--link/--etc name")
    ap.add_argument("--etc", help="directory copied to /etc")
    ap.add_argument("--file", action="append", default=[], metavar="DEST=SRC",
                    help="add a file, e.g. usr/bin/hfetch=out/hfetch.wasm; 755 when it is a wasm program or "
                         "starts with #! (repeatable)")
    ap.add_argument("--dir", action="append", default=[], metavar="PATH",
                    help="add an (empty) directory, e.g. usr/lib/python3.13/lib-dynload (repeatable)")
    ap.add_argument("--link", action="append", default=[], metavar="DEST=TARGET",
                    help="add a symlink, e.g. usr/bin/python=python3.13 (repeatable)")
    ap.add_argument("--tree", action="append", default=[], metavar="DEST=SRC",
                    help="add a directory tree, e.g. usr/share/terminfo=deps/share/terminfo; a file "
                         "is 755 when it is a wasm program, starts with #! or is executable on the host (repeatable)")
    ap.add_argument("-o", "--output", required=True)
    args = ap.parse_args()
    if not args.overlay and not (args.init and args.busybox):
        ap.error("--init and --busybox are required (or use --overlay)")

    c = Cpio()

    def ensure_parents(path):
        """Directories on the way to `path`. Harmless if the base archive already made them."""
        parts = path.split("/")[:-1]
        for i in range(1, len(parts) + 1):
            d = "/".join(parts[:i])
            if d not in c.seen:
                c.dir(d)

    if not args.overlay:
        for d in ("bin", "sbin", "usr", "usr/bin", "usr/sbin", "etc", "dev", "proc", "sys", "tmp", "root", "mnt", "var", "var/tmp"):
            c.dir(d, 0o1777 if d in ("tmp", "var/tmp") else 0o755)

        # The kernel opens /dev/console for init before any userspace runs; devtmpfs is
        # only mounted later by /init, so the node must already exist in the initramfs.
        c.chardev("dev/console", 5, 1)
        c.chardev("dev/null", 1, 3, 0o666)

        c.file("init", open(args.init, "rb").read(), 0o755)

        # busybox tree: real binary plus applet symlinks (`make install`, CONFIG_PREFIX).
        for root, dirs, files in os.walk(args.busybox):
            dirs.sort()
            rel = os.path.relpath(root, args.busybox)
            if rel != "." and rel not in c.seen:
                c.dir(rel)
            for f in sorted(files):
                p = os.path.join(root, f)
                r = os.path.normpath(os.path.join(rel, f))
                if os.path.islink(p):
                    c.symlink(r, os.readlink(p))
                else:
                    c.file(r, open(p, "rb").read(), 0o755 if os.stat(p).st_mode & stat.S_IXUSR else 0o644)

    for spec in args.file:
        dest, _, src = spec.partition("=")
        if not dest or not src:
            ap.error(f"--file expects DEST=SRC, got {spec!r}")
        ensure_parents(dest)
        data = open(src, "rb").read()
        c.file(dest, data, 0o755 if data.startswith((b"\0asm", b"#!")) else 0o644)

    for d in args.dir:
        ensure_parents(d + "/x")
        if d not in c.seen:
            c.dir(d)

    for spec in args.link:
        dest, _, target = spec.partition("=")
        if not dest or not target:
            ap.error(f"--link expects DEST=TARGET, got {spec!r}")
        ensure_parents(dest)
        c.symlink(dest, target)

    for spec in args.tree:
        dest, _, src = spec.partition("=")
        if not dest or not src:
            ap.error(f"--tree expects DEST=SRC, got {spec!r}")
        ensure_parents(dest + "/x")
        if dest not in c.seen:
            c.dir(dest)
        for root, dirs, files in os.walk(src):
            dirs.sort()
            rel = os.path.relpath(root, src)
            base = dest if rel == "." else f"{dest}/{rel}"
            if base not in c.seen:
                c.dir(base)
            for f in sorted(files):
                p = os.path.join(root, f)
                if os.path.islink(p):
                    c.symlink(f"{base}/{f}", os.readlink(p))
                    continue
                data = open(p, "rb").read()
                executable = data.startswith((b"\0asm", b"#!")) or os.stat(p).st_mode & stat.S_IXUSR
                c.file(f"{base}/{f}", data, 0o755 if executable else 0o644)

    if args.etc:
        for root, dirs, files in os.walk(args.etc):
            dirs.sort()
            for f in sorted(files):
                p = os.path.join(root, f)
                r = os.path.join("etc", os.path.relpath(p, args.etc))
                ensure_parents(r)
                # These come from the source tree, where a checkout on Windows or through a shared
                # folder loses the executable bit and may turn LF into CRLF. A script is known by
                # its #! line, not by the host's mode, and the guest's shell wants LF.
                data = open(p, "rb").read().replace(b"\r\n", b"\n")
                executable = data.startswith(b"#!") or os.stat(p).st_mode & stat.S_IXUSR
                c.file(r, data, 0o755 if executable else 0o644)

    data = c.finish()
    with open(args.output, "wb") as fh:
        fh.write(data)
    print(f"{args.output}: {len(data)} bytes, {len(c.seen) - 1} entries", file=sys.stderr)


if __name__ == "__main__":
    main()
