#!/usr/bin/env python3
"""Writes a runtime folder's manifest.json from what the folder holds.

  scripts/runtime-manifest.py dist/runtime/collabo-core-<platform>/...

package-runtime.sh writes it when it packs a runtime, and release.sh again just before it
archives one, so that an image put into app/images/ after packaging (a rebuilt .cpio, another
add-on) is what the published manifest lists: `launcher --update --apply` checks every file it
installs against the manifest's sha256. (On Windows, package-runtime-windows.ps1 writes the same
manifest itself; release.sh rewrites it in this form when anything in it differs.)

  { name, platform, engine, protocol, entry, addons, files }
  platform  from the folder's name, collabo-core-<platform>
  entry     the engine and the images it starts with (--tools-image when there is a
            tools.cpio, --addon-dir when there are add-ons); the Dart package and the
            launcher add their own arguments after it
  addons    the add-ons shipped, app/images/addons/<name>.cpio, by name
  files     path -> sha256 of the runtime's own files: the launcher, bin/ and app/. Not
            launcher.conf (the user's to edit; an update never replaces it), not what a
            runtime gathers as it runs (work/, logs, .update/, downloads), and not macOS's
            .DS_Store and ._* files (release.sh leaves those out of the archive).

The file is rewritten only when what it says changes; each folder gets a line saying what did.
"""
import hashlib
import json
import os
import sys

NAME = "manifest.json"
CONF = "launcher.conf"
# The top-level names that make up a runtime: what package-runtime.sh puts there, but work/.
OWN = {"launcher", "launcher.exe", "bin", "app"}


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def walk(folder):
    """Every file under `folder` by its /-separated path, sorted; macOS's own left out."""
    found = []
    for base, _, names in os.walk(folder):
        for name in names:
            if name == ".DS_Store" or name.startswith("._"):
                continue
            found.append(os.path.relpath(os.path.join(base, name), folder).replace(os.sep, "/"))
    return sorted(found)


def build(folder):
    """The manifest for the runtime in `folder`, and the files there that are not the runtime's."""
    top = os.path.basename(os.path.normpath(folder))
    if not top.startswith("collabo-core-"):
        raise SystemExit(f"{folder}: not a collabo-core-<platform> folder")
    platform = top[len("collabo-core-"):]
    windows = platform.startswith("win-")
    exe = "collabo-core-engine.exe" if windows else "collabo-core-engine"
    files, other = {}, []
    for rel in walk(folder):
        if rel in (NAME, CONF):
            continue
        if rel.split("/", 1)[0] in OWN:
            files[rel] = sha256(os.path.join(folder, *rel.split("/")))
        else:
            other.append(rel)
    needed = ["bin/" + exe, "launcher.exe" if windows else "launcher", "app/images/vmlinux.wasm",
              "app/images/initramfs.cpio", "app/images/python.cpio"]
    missing = [rel for rel in needed if rel not in files]
    if missing:
        raise SystemExit(f"{top}: no {', '.join(missing)} (package it again: python3 build.py runtime)")
    entry = ["bin/" + exe,
             "--kernel", "app/images/vmlinux.wasm",
             "--initramfs", "app/images/initramfs.cpio",
             "--python-image", "app/images/python.cpio"]
    if "app/images/tools.cpio" in files:
        entry += ["--tools-image", "app/images/tools.cpio"]
    addons = sorted(rel[len("app/images/addons/"):-len(".cpio")] for rel in files
                    if rel.startswith("app/images/addons/") and rel.endswith(".cpio") and rel.count("/") == 3)
    if addons:
        entry += ["--addon-dir", "app/images/addons"]
    manifest = {"name": "collabo-core-runtime", "platform": platform, "engine": "wasmtime", "protocol": 1,
                "entry": entry, "addons": addons, "files": dict(sorted(files.items()))}
    return manifest, other


def listed(names, most=4):
    shown = ", ".join(names[:most])
    return shown + (f" and {len(names) - most} more" if len(names) > most else "")


def changes(old, new):
    """What `new` says that `old` did not, in words."""
    before = old.get("files") if isinstance(old.get("files"), dict) else {}
    after = new["files"]
    said = []
    for what, names in (("changed", [r for r in after if r in before and before[r] != after[r]]),
                        ("new", [r for r in after if r not in before]),
                        ("gone", [r for r in before if r not in after])):
        if names:
            said.append(f"{len(names)} {what} ({listed(names)})")
    if old.get("addons") != new["addons"]:
        said.append(f"addons {', '.join(new['addons']) or 'none'}")
    other = [key for key in new if key not in ("files", "addons") and old.get(key) != new[key]]
    other += [key for key in old if key not in new]
    if other:
        said.append(", ".join(other))
    return said


def write(folder):
    path = os.path.join(folder, NAME)
    manifest, other = build(folder)
    try:
        with open(path, encoding="utf-8-sig") as f:
            old = json.load(f)
    except FileNotFoundError:
        old = None
    except ValueError:
        old = {}
    count = f"{len(manifest['files'])} files"
    if old == manifest:
        print(f"  {NAME}: {count}, as it was")
    else:
        tmp = path + ".tmp"
        with open(tmp, "w", encoding="utf-8", newline="\n") as f:
            json.dump(manifest, f, indent=1)
        os.replace(tmp, path)
        said = "written" if old is None else "rewritten: " + ("; ".join(changes(old, manifest)) or "the same, in another form")
        print(f"  {NAME}: {count}, {said}")
    if other:
        print(f"  note: not the runtime's, so in no manifest (an update leaves them out): {listed(other)}")


def main(args):
    if not args or any(arg.startswith("-") for arg in args):
        sys.stdout.write(__doc__)
        return 0 if args and args[0] in ("-h", "--help") else 2
    for folder in args:
        if not os.path.isdir(folder):
            raise SystemExit(f"{folder}: no such folder")
        write(folder)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
