#!/usr/bin/env python3
r"""What busybox gained for builds and test suites, in the guest:

  * diff -c / -C N (userspace/patches/busybox-diff-context.patch): the context format, byte for
    byte what GNU diff (the host's) prints for the same files and options;
  * a file without #! is run as a shell script (userspace/patches/busybox-exec-script.patch), as
    POSIX wants of execvp and glibc's does (musl's fails with ENOEXEC): by the shell, by path and
    through PATH, by env, xargs and find -exec, with its arguments; a program of another machine
    (an ELF file) still fails with "Exec format error";
  * /bin/sh (hush) runs what POSIX scripts — autoconf's configure, libtool — ask of a shell
    (userspace/patches/busybox-hush-posix-scripts.patch): [n]>&word and [n]<&word with the word
    expanded when run, fd 0 duplicated (exec 5<&0), an unquoted expansion in a case pattern as a
    pattern, ${var=word} assigning a quoted word as it reads, set -u (nounset), a function
    definition across backslash-newlines (automake's test harness). Each script prints what the
    host's bash prints for it, but for the messages a shell words its own way;
  * grep takes GNU grep's options (userspace/patches/busybox-grep-gnu-options.patch), which agents
    and scripts use: --include, --exclude (the last matching one decides), --exclude-dir, -I and
    --binary-files, -r with no FILE (the current directory, names without "./"), each run against
    the host's GNU grep (the lines, sorted, and the exit status); and GNU's long names of the
    options busybox had, each printing what the short option prints in the guest;
  * GNU behaviour that scripts and agents count on, each tool against the GNU one on the host (the
    output, the status, the files it writes): sed's \U \L \u \l \E, s///Ng, 0,/RE/, FIRST~STEP,
    ADDR,~N, l q Q R W F z v, the M and I flags, -s -z -u -l; awk's match(s, re, arr); diff's
    normal format (the default when no -u or -c) and -s; tar -t and -x with --exclude and -X, as GNU
    tar matches them; timeout's statuses, signals, -k -p -f; tree as tree 2.3.1 prints it (TREE_CASES
    hold its output: tree is seldom on a host). And, with nothing on the host to compare: date +%N,
    zcat/gunzip/bzcat/tar -z reading a pipe, lsof -i/-t/-s/-p in lsof 4.99's columns;
  * more of GNU's tools, each case in a fresh folder in the guest and with the host's GNU tool,
    the output, status and folder after it the same: find's -printf/-fprint/-newerXY/-regextype/
    -iregex/-lname/-xtype/-execdir/-readable/-true and GNU's messages; date -d and touch -d with
    GNU's date grammar ("2 days ago", "next monday", ISO 8601, zones); du's --max-depth, -b,
    --apparent-size, --si, -B, -t, -S, --inodes, --exclude, -0; xargs -d, -L, -l and the long
    names; realpath -e/-m/-s/-L/--relative-to/--relative-base and readlink -e/-m; ln -r/-t/-L/
    --backup; rm -d/-v/-I/--interactive and GNU's prompts; cp -b/--backup/--preserve/
    --no-preserve/-x/--attributes-only (and cp writing into a file it replaces); mktemp's
    templates and --suffix. And ps as procps-ng prints it: its headers, -o, -C, --sort, status 1.

  python3 tests/test_busybox_tools.py     engine/target/release/collabo-core-engine and the build's
                                          images, or the runtime folder COLLABO_RUNTIME names
"""
import os
import random
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

runtime = os.environ.get("COLLABO_RUNTIME")
if runtime:
    images = Path(runtime) / "app" / "images"
    ENGINE = [str(Path(runtime) / "bin" / "collabo-core-engine"), "exec", "--kernel", str(images / "vmlinux.wasm"),
              "--initramfs", str(images / "initramfs.cpio")]
else:
    ENGINE = [str(ROOT / "engine/target/release/collabo-core-engine"), "exec", "--kernel", str(ROOT / "kernel/linux/vmlinux.wasm"),
              "--initramfs", str(ROOT / "userspace/out/initramfs.cpio")]

fails = []


def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else "\n     " + str(detail)[-2000:]), flush=True)
    if not ok:
        fails.append(name)


def guest(work, script, timeout=300):
    """Runs `script` (sh) in a fresh guest with `work` at /work; returns (status, stdout)."""
    p = subprocess.run(ENGINE + ["--no-network", "--mount", f"{work}:/work", "--cwd", "/work", "--",
                                 "/bin/sh", "-c", script], capture_output=True, text=True, timeout=timeout)
    return p.returncode, p.stdout


def files(work):
    """Pairs of files for diff: changes, insertions and deletions in the middle and at both ends,
    hunks near enough to merge, an empty file, files without a newline at the end."""
    base = [f"line {i}" for i in range(80)]
    out = {"o": base}
    for k in range(10):
        r = random.Random(k)
        lines = list(base)
        for _ in range(r.randint(1, 10)):
            i = r.randrange(len(lines))
            op = r.choice("dac")
            if op == "d":
                del lines[i]
            elif op == "a":
                lines.insert(i, f"new {r.randint(0, 999)}")
            else:
                lines[i] += " changed"
        out[f"n{k}"] = lines
    out["head"] = ["first"] + base[2:]                 # a change at the first line
    out["tail"] = base[:-1] + ["last", "more"]         # and at the end
    for name, lines in out.items():
        (work / name).write_text("\n".join(lines) + "\n")
    (work / "empty").write_text("")
    (work / "nonl").write_text("\n".join(base[:6]))     # no newline at the end
    pairs = [("o", n) for n in out if n != "o"] + [("empty", "o"), ("o", "empty"), ("o", "nonl"), ("nonl", "o")]
    return pairs


def grep_checks(work):
    """grep's GNU options: the new ones against the host's GNU grep, the long names against the
    short options in the guest."""
    tree = {"t/a.py": "hello a\n", "t/b.js": "hello b\n", "t/c.min.js": "hello c\n", "t/.dot.py": "hello dot\n",
            "t/sub/s.py": "hello s\nbye s\n", "t/sub/t.txt": "hello t\n", "t/sub/.hid/h.py": "hello h\n",
            "t/node_modules/n.js": "hello n\n", "t/.git/g.py": "hello g\n", "t/opt.txt": "--include=x\n",
            "b/a.txt": "hello a\nbye a\n", "b/bin.dat": "hello\0bin\n",
            "b/late.dat": "hello\n" * 5000 + "x\0y\n"}
    for name, text in tree.items():
        (work / name).parent.mkdir(parents=True, exist_ok=True)
        (work / name).write_text(text)
    # cd DIR; grep ARGS (GNU's output sorted: -r walks a directory in no fixed order)
    against_gnu = [
        ("", "-r --include='*.py' hello t"),
        ("", "-r --include='*.py' --include='*.js' hello t"),
        ("", "-r --include '*.py' hello t"),
        ("", "-r --inc='*.py' hello t"),
        ("", "-r --exclude='*.py' hello t"),
        ("", "-r --exclude='*.min.js' --include='*.js' hello t"),
        ("", "-r --include='*.js' --exclude='*.min.js' hello t"),
        ("", "-r hello t --exclude='*.min.js' --include='*.js'"),
        ("", "-r --exclude-dir=sub --exclude-dir=.git --exclude-dir=node_modules hello t"),
        ("", "-r --exclude-dir='.*' hello t"),
        ("", "-r --exclude-dir=t hello t"),
        ("", "-r --exclude-dir=sub hello t/sub/"),
        ("", "-r --exclude-dir=sub hello t/sub t/a.py"),
        ("", "--include='*.py' hello t/a.py t/b.js t/sub/t.txt"),
        ("", "--exclude='sub/*' hello t/sub/t.txt t/a.py"),
        ("", "--exclude=t.txt hello t/sub/t.txt t/a.py"),
        ("", "-e --include=x t/opt.txt"),
        ("", "-- --include=x t/opt.txt"),
        ("t", "-r hello"),
        ("t", "-rn --include='*.py' hello"),
        ("t", "-rl --exclude-dir='.*' hello"),
        ("t/sub", "-rc hello"),
        ("", "-I hello b/bin.dat b/a.txt"),
        ("", "-c -I hello b/bin.dat b/late.dat b/a.txt"),
        ("", "-L -I hello b/bin.dat b/a.txt"),
        ("", "-l -I hello b/bin.dat b/a.txt"),
        ("", "-v -I hello b/bin.dat b/a.txt"),
        ("", "-rI hello b"),
        ("", "-c -I -a hello b/bin.dat"),
        ("", "-c -a -I hello b/bin.dat"),
        ("", "--binary-files=without-match hello b/bin.dat b/a.txt"),
        ("", "-c -I --binary-files=text hello b/bin.dat"),
        ("", "--binary-files=bogus hello b/a.txt"),
    ]
    # the guest: grep LONG and grep SHORT print the same, in b/ (no binary file there to differ)
    long_short = [
        ("--ignore-case HELLO a.txt", "-i HELLO a.txt"), ("--line-number bye a.txt", "-n bye a.txt"),
        ("--count hello a.txt bin.dat", "-c hello a.txt bin.dat"), ("--files-with-matches bye *", "-l bye *"),
        ("--files-without-match bye *", "-L bye *"), ("--invert-match hello a.txt", "-v hello a.txt"),
        ("--word-regexp a a.txt", "-w a a.txt"), ("--line-regexp 'bye a' a.txt", "-x 'bye a' a.txt"),
        ("--fixed-strings 'o a' a.txt", "-F 'o a' a.txt"), ("--extended-regexp 'h(e)+llo' a.txt", "-E 'h(e)+llo' a.txt"),
        ("--only-matching 'b.e' a.txt", "-o 'b.e' a.txt"), ("--max-count=1 . a.txt", "-m 1 . a.txt"),
        ("--context=1 bye a.txt", "-C 1 bye a.txt"), ("--after-context=1 hello a.txt", "-A 1 hello a.txt"),
        ("--before-context 1 bye a.txt", "-B 1 bye a.txt"), ("--no-filename hello a.txt a.txt", "-h hello a.txt a.txt"),
        ("--with-filename hello a.txt", "-H hello a.txt"), ("--quiet hello a.txt; echo $?", "-q hello a.txt; echo $?"),
        ("--silent nope a.txt; echo $?", "-q nope a.txt; echo $?"), ("--no-messages hello nofile; echo $?", "-s hello nofile; echo $?"),
        ("--regexp=bye --regexp=hello a.txt", "-e bye -e hello a.txt"), ("--file=pat a.txt", "-f pat a.txt"),
        ("--recursive bye .", "-r bye ."), ("--dereference-recursive bye .", "-R bye ."),
        ("--color=never hello a.txt", "hello a.txt"), ("--colour hello a.txt", "hello a.txt"),
        ("--line-buffered hello a.txt", "hello a.txt"),
    ]
    (work / "b" / "pat").write_text("bye\n")
    lines, expected = [], {}
    for cwd, args in against_gnu:
        out = f"grep.{len(expected)}"
        ref = subprocess.run(f"grep {args}", shell=True, cwd=work / cwd, capture_output=True)
        expected[out] = (f"(cd {cwd or '.'}) grep {args}", ref.stdout, ref.returncode)
        lines.append(f"(cd /work/{cwd} && grep {args}) > /work/{out} 2>/dev/null; echo $? > /work/{out}.rc")
    for i, (long, short) in enumerate(long_short):
        lines.append(f"(cd /work/b && grep {long}) > /work/long.{i} 2>&1; (cd /work/b && grep {short}) > /work/short.{i} 2>&1")
    (work / "greps.sh").write_text("\n".join(lines) + "\n")
    guest(work, "sh greps.sh")

    def sort(b):
        return sorted(b.split(b"\n"))
    wrong = []
    for out, (key, ref, ref_rc) in expected.items():
        got = (work / out).read_bytes() if (work / out).exists() else b"(none)"
        got_rc = (work / f"{out}.rc").read_text().strip() if (work / f"{out}.rc").exists() else "?"
        if sort(got) != sort(ref) or got_rc != str(ref_rc):
            wrong.append(f"{key}: status {got_rc} (GNU {ref_rc})\n--- guest\n{got.decode(errors='replace')[:600]}"
                         f"\n--- GNU grep\n{ref.decode(errors='replace')[:600]}")
    check(f"grep: {len(expected)} uses of GNU grep's options print what GNU grep prints", not wrong, "\n".join(wrong[:3]))
    wrong = []
    for i, (long, short) in enumerate(long_short):
        a, b = (work / f"long.{i}").read_bytes(), (work / f"short.{i}").read_bytes()
        if a != b or not b.strip():
            wrong.append(f"grep {long}:\n{a.decode(errors='replace')[:300]}\n--- grep {short}:\n{b.decode(errors='replace')[:300]}")
    check(f"grep: {len(long_short)} of GNU's long option names do what the short options do", not wrong, "\n".join(wrong[:3]))


# GNU behaviour busybox's tools gained (userspace/patches/busybox-{tree-options,sed-gnu,awk-match,
# diff-normal,tar-exclude,timeout-gnu,zcat-pipe,lsof-options}.patch), each against the GNU tool on
# the host where there is one.

SED_FILES = {
    "a.txt": b"apple\nbanana\nApple pie\ncherry\nbanana\ndate 2024\ngrape  \n10 apples\n",
    "b.txt": b"one\ntwo\nthree\n",
    "r.txt": b"r1\nr2\n",
    "z.bin": b"x\0y\nz\0last",
    "w.txt": b"hello world foo bar\n",
    "noeol.txt": b"a\nlast",
    "n4.txt": b"1\n2\n3\n4\n",
    "n3.txt": b"1\n2\n3\n",
    "l.txt": "tab\there\\back\x01ctl é\n".encode() + b"0" * 80 + b"\n",
}
# sed ARGS in a folder of SED_FILES: what it prints, its status and the files it writes, as GNU sed
SED_CASES = [
    # \U \L \u \l \E in the replacement
    [r"s/\(.\)/\U\1/", "a.txt"], [r"s/.*/\L&/", "a.txt"], [r"s/\w\+/\u&/g", "w.txt"], [r"s/.*/\U&\E!/", "w.txt"],
    [r"s/\(\w*\) \(\w*\)/\U\1\E \u\2/", "w.txt"], [r"s/o/\U&/2g", "w.txt"], [r"s/.*/\L\u&/", "a.txt"],
    [r"s/\(a\)\(p*\)/\u\1\U\2x\Ey/", "a.txt"], [r"s/\(.*\)/\u\L\1/", "a.txt"], [r"s/p/\l&\uP/g", "a.txt"],
    # s///Ng: the Nth match and those after it
    ["s/a/A/2g", "a.txt"], ["s/a/A/2", "a.txt"], ["s/a/A/3g", "a.txt"], ["s/a/A/g2", "a.txt"],
    # 0,/RE/, FIRST~STEP, ADDR,~N
    ["0,/banana/s//BANANA/", "a.txt"], ["0,/apple/s//APPLE/", "a.txt"], ["0,/banana/d", "a.txt"],
    ["-n", "0,/cherry/p", "a.txt"], ["0,5d", "a.txt"], ["-n", "0p", "a.txt"],
    ["-n", "1~2p", "a.txt"], ["-n", "0~3p", "a.txt"], ["-n", "2~0p", "a.txt"], ["1~3d", "a.txt"],
    ["-n", "2,~4p", "a.txt"], ["-n", "4,~4p", "a.txt"], ["-n", "/cherry/,~2p", "a.txt"], ["-n", "2~3{p;p}", "a.txt"],
    ["-n", "2,~0p", "n4.txt"],
    # l [N], q/Q with a status, R, W, F, z, v
    ["-n", "l", "l.txt"], ["-n", "l 10", "l.txt"], ["-l", "20", "-n", "l", "l.txt"], ["-n", "l 1", "b.txt"],
    ["-n", "l 0", "l.txt"], ["-n", "N;l", "b.txt"], ["3q", "a.txt"], ["3q5", "a.txt"], ["3Q", "a.txt"],
    ["3Q7", "a.txt"], ["2{a\\\nadded\nQ}", "a.txt"], ["2{a\\\nadded\nq}", "a.txt"], ["-n", "3q 4", "a.txt"],
    ["R r.txt", "b.txt"], ["1R r.txt", "b.txt"], ["R nofile", "b.txt"], ["-n", "F;=", "b.txt"],
    ["F", "b.txt", "a.txt"], ["2z", "b.txt"], ["2W out.w", "b.txt"], ["N;W out.w", "b.txt"], ["v", "b.txt"],
    ["v 4.2", "b.txt"],
    # -s -z -u -l and their long names, --posix
    ["-s", "-n", "$p", "a.txt", "b.txt"], ["-s", "1d", "a.txt", "b.txt"], ["-s", "-n", "F", "a.txt", "b.txt"],
    ["-s", "-n", "/two/,/x/p", "b.txt", "a.txt"], ["-s", "-n", "/3/,/1/p", "n4.txt", "n3.txt"],
    ["-n", "/3/,/1/p", "n4.txt", "n3.txt"], ["-z", r"s/\n/,/g", "a.txt"], ["-z", "s/^/>/", "z.bin"],
    ["--null-data", "s/x/X/", "z.bin"], ["-z", "-n", "$p", "z.bin"], ["-u", "-n", "1p", "a.txt"],
    ["--unbuffered", "2q", "a.txt"], ["--separate", "-n", "1p", "a.txt", "b.txt"],
    ["--line-length=5", "-n", "l", "b.txt"], ["--posix", "-n", "1p", "a.txt"], ["-sn", "1p", "a.txt", "b.txt"],
    # the M flag, I and M on addresses
    ["s/^/x/M", "a.txt"], ["N;s/^/>/Mg", "b.txt"], ["N;N;s/o$/O/Mg", "b.txt"], ["-n", "/APPLE/Ip", "a.txt"],
    ["-n", "/a/I,/X/Ip", "a.txt"], ["-n", "N;/^two/Mp", "b.txt"], ["/APPLE/Id", "a.txt"],
    ["-n", "/^t/I{s//T/p}", "b.txt"],
    # a last line without a newline, -i
    ["-n", "p", "noeol.txt"], ["p", "noeol.txt"], ["-n", "p;p", "noeol.txt"], ["$!d", "noeol.txt", "b.txt"],
    ["-n", "p", "noeol.txt", "b.txt"], ["-i", "s/a/A/", "a.txt"], ["-i.bak", "s/a/A/2g", "a.txt"],
]

# awk PROGRAM with stdin: what gawk prints. match()'s third argument: the text of the match and of
# each group, and where each starts and how long it is
AWK_SHOW = ('function show(m,  k, n, i, o) { for (k in m) n++; o = n + 0; for (i = 0; i < 10; i++) {'
            ' if (i in m) o = o " " i "=[" m[i] "]"; if ((i, "start") in m) o = o "@" m[i, "start"] "+" m[i, "length"] };'
            ' return o }\n')
AWK_CASES = [
    ('BEGIN { r = match("foo=bar baz", /(o+)=(b(x)?a)/, m); print r, RSTART, RLENGTH, show(m) }', ""),
    ('BEGIN { m[7] = 1; r = match("zzz", /q/, m); print r, RSTART, RLENGTH, show(m) }', ""),
    ('BEGIN { r = match("abc", "(a)(b)(c)", m); print r, show(m) }', ""),
    ('BEGIN { re = "([0-9]+)-([0-9]+)"; r = match("x 12-345 y", re, m); print r, show(m) }', ""),
    ('{ if (match($0, /([a-z]+)@([a-z.]+)/, m)) print NR, m[1], m[2], m[0, "start"]; else print NR, "no" }',
     "mail bob@x.org now\nnone here\na@b\n"),
    ('BEGIN { m[1] = "aXbXc"; r = match(m[1], /(X)b/, m); print r, show(m) }', ""),
    ('BEGIN { r = match("aaa", /a*/, m); print r, RLENGTH, show(m) }', ""),
    ('BEGIN { r = match("", /x*/, m); print r, RSTART, RLENGTH, show(m) }', ""),
    ('BEGIN { SUBSEP = ":"; r = match("hello", /l+/, m); for (k in m) c++; print r, c, m["0:start"], m["0:length"] }', ""),
    ('BEGIN { r = match("xyz", /(y)|(q)/, m); print r, show(m), (2 in m) }', ""),
    ('BEGIN { print match("abc", /c/), RSTART, RLENGTH; print match("abc", /d/), RSTART, RLENGTH }', ""),
    ('function f(s, a) { return match(s, /(b+)/, a) } BEGIN { f("abbbc", arr); print arr[1], arr[1, "length"] }', ""),
    ('BEGIN { x = 0; r = match("a1b22", "[0-9]" (x++ ? "+" : ""), m); print r, x, m[0] }', ""),
]

TREE_FIXTURE = 'mkdir -p tr/src/lib tr/docs tr/build tr/.hidden tr/node_modules/pkg tr/.git tr/empty\nfor f in README.md Makefile \'a b.txt\' src/main.c src/lib/util.c src/lib/util.h src/lib/x10.c src/lib/x9.c   docs/guide.md build/out.o .hidden/h.txt node_modules/pkg/index.js; do printf \'x\\n\' > "tr/$f"; done\nprintf \'build/\\n*.o\\nnode_modules\\n\' > tr/.gitignore; printf \'*.h\\n\' > tr/src/.gitignore\nprintf \'#!/bin/sh\\n\' > tr/run.sh; chmod +x tr/run.sh; ln -s src tr/link'
# tree ARGS, run in the folder FIXTURE made: what tree 2.3.1 prints, and its exit status
TREE_CASES = [
    ('tr', 0, 'tr\n├── Makefile\n├── README.md\n├── a b.txt\n├── build\n│\xa0\xa0 └── out.o\n├── docs\n│\xa0\xa0 └── guide.md\n├── empty\n├── link -> src\n├── node_modules\n│\xa0\xa0 └── pkg\n│\xa0\xa0     └── index.js\n├── run.sh\n└── src\n    ├── lib\n    │\xa0\xa0 ├── util.c\n    │\xa0\xa0 ├── util.h\n    │\xa0\xa0 ├── x10.c\n    │\xa0\xa0 └── x9.c\n    └── main.c\n\n9 directories, 12 files\n'),
    ('-a tr', 0, 'tr\n├── .git\n├── .gitignore\n├── .hidden\n│\xa0\xa0 └── h.txt\n├── Makefile\n├── README.md\n├── a b.txt\n├── build\n│\xa0\xa0 └── out.o\n├── docs\n│\xa0\xa0 └── guide.md\n├── empty\n├── link -> src\n├── node_modules\n│\xa0\xa0 └── pkg\n│\xa0\xa0     └── index.js\n├── run.sh\n└── src\n    ├── .gitignore\n    ├── lib\n    │\xa0\xa0 ├── util.c\n    │\xa0\xa0 ├── util.h\n    │\xa0\xa0 ├── x10.c\n    │\xa0\xa0 └── x9.c\n    └── main.c\n\n11 directories, 15 files\n'),
    ('-d tr', 0, 'tr\n├── build\n├── docs\n├── empty\n├── link -> src\n├── node_modules\n│\xa0\xa0 └── pkg\n└── src\n    └── lib\n\n9 directories\n'),
    ('-L 1 tr', 0, 'tr\n├── Makefile\n├── README.md\n├── a b.txt\n├── build\n├── docs\n├── empty\n├── link -> src\n├── node_modules\n├── run.sh\n└── src\n\n7 directories, 4 files\n'),
    ("-I 'node_modules|build' tr", 0, 'tr\n├── Makefile\n├── README.md\n├── a b.txt\n├── docs\n│\xa0\xa0 └── guide.md\n├── empty\n├── link -> src\n├── run.sh\n└── src\n    ├── lib\n    │\xa0\xa0 ├── util.c\n    │\xa0\xa0 ├── util.h\n    │\xa0\xa0 ├── x10.c\n    │\xa0\xa0 └── x9.c\n    └── main.c\n\n6 directories, 10 files\n'),
    ("-P '*.c' tr", 0, 'tr\n├── build\n├── docs\n├── empty\n├── node_modules\n│\xa0\xa0 └── pkg\n└── src\n    ├── lib\n    │\xa0\xa0 ├── util.c\n    │\xa0\xa0 ├── x10.c\n    │\xa0\xa0 └── x9.c\n    └── main.c\n\n8 directories, 4 files\n'),
    ("-P '*.c' --prune tr", 0, 'tr\n└── src\n    ├── lib\n    │\xa0\xa0 ├── util.c\n    │\xa0\xa0 ├── x10.c\n    │\xa0\xa0 └── x9.c\n    └── main.c\n\n3 directories, 4 files\n'),
    ('--gitignore tr', 0, 'tr\n├── Makefile\n├── README.md\n├── a b.txt\n├── docs\n│\xa0\xa0 └── guide.md\n├── empty\n├── link -> src\n├── run.sh\n└── src\n    ├── lib\n    │\xa0\xa0 ├── util.c\n    │\xa0\xa0 ├── x10.c\n    │\xa0\xa0 └── x9.c\n    └── main.c\n\n6 directories, 9 files\n'),
    ('-J tr/src', 0, '[\n    {"type":"directory","name":"tr/src","contents":[\n        {"type":"directory","name":"lib","contents":[\n            {"type":"file","name":"util.c"},\n            {"type":"file","name":"util.h"},\n            {"type":"file","name":"x10.c"},\n            {"type":"file","name":"x9.c"}\n        ]},\n        {"type":"file","name":"main.c"}\n    ]}\n,    {"type":"report","directories":2,"files":5}\n]\n'),
    ('-f tr/src', 0, 'tr/src\n├── tr/src/lib\n│\xa0\xa0 ├── tr/src/lib/util.c\n│\xa0\xa0 ├── tr/src/lib/util.h\n│\xa0\xa0 ├── tr/src/lib/x10.c\n│\xa0\xa0 └── tr/src/lib/x9.c\n└── tr/src/main.c\n\n2 directories, 5 files\n'),
    ('-i --noreport tr/src', 0, 'tr/src\nlib\nutil.c\nutil.h\nx10.c\nx9.c\nmain.c\n'),
    ('--charset=ascii tr/src', 0, 'tr/src\n|-- lib\n|   |-- util.c\n|   |-- util.h\n|   |-- x10.c\n|   `-- x9.c\n`-- main.c\n\n2 directories, 5 files\n'),
    ('--dirsfirst tr', 0, 'tr\n├── build\n│\xa0\xa0 └── out.o\n├── docs\n│\xa0\xa0 └── guide.md\n├── empty\n├── link -> src\n├── node_modules\n│\xa0\xa0 └── pkg\n│\xa0\xa0     └── index.js\n├── src\n│\xa0\xa0 ├── lib\n│\xa0\xa0 │\xa0\xa0 ├── util.c\n│\xa0\xa0 │\xa0\xa0 ├── util.h\n│\xa0\xa0 │\xa0\xa0 ├── x10.c\n│\xa0\xa0 │\xa0\xa0 └── x9.c\n│\xa0\xa0 └── main.c\n├── Makefile\n├── README.md\n├── a b.txt\n└── run.sh\n\n9 directories, 12 files\n'),
    ('-F tr', 0, 'tr/\n├── Makefile\n├── README.md\n├── a b.txt\n├── build/\n│\xa0\xa0 └── out.o\n├── docs/\n│\xa0\xa0 └── guide.md\n├── empty/\n├── link -> src/\n├── node_modules/\n│\xa0\xa0 └── pkg/\n│\xa0\xa0     └── index.js\n├── run.sh*\n└── src/\n    ├── lib/\n    │\xa0\xa0 ├── util.c\n    │\xa0\xa0 ├── util.h\n    │\xa0\xa0 ├── x10.c\n    │\xa0\xa0 └── x9.c\n    └── main.c\n\n9 directories, 12 files\n'),
    ('-P lib --matchdirs tr/src', 0, 'tr/src\n└── lib\n    ├── util.c\n    ├── util.h\n    ├── x10.c\n    └── x9.c\n\n2 directories, 4 files\n'),
    ('-v tr/src/lib', 0, 'tr/src/lib\n├── util.c\n├── util.h\n├── x9.c\n└── x10.c\n\n1 directory, 4 files\n'),
    ('-r tr/src', 0, 'tr/src\n├── main.c\n└── lib\n    ├── x9.c\n    ├── x10.c\n    ├── util.h\n    └── util.c\n\n2 directories, 5 files\n'),
    ('--filelimit 3 tr/src', 2, 'tr/src\n├── lib  [4 entries exceeds filelimit, not opening dir]\n└── main.c\n\n2 directories, 1 file\n'),
    ('-l tr', 0, 'tr\n├── Makefile\n├── README.md\n├── a b.txt\n├── build\n│\xa0\xa0 └── out.o\n├── docs\n│\xa0\xa0 └── guide.md\n├── empty\n├── link -> src\n│\xa0\xa0 ├── lib\n│\xa0\xa0 │\xa0\xa0 ├── util.c\n│\xa0\xa0 │\xa0\xa0 ├── util.h\n│\xa0\xa0 │\xa0\xa0 ├── x10.c\n│\xa0\xa0 │\xa0\xa0 └── x9.c\n│\xa0\xa0 └── main.c\n├── node_modules\n│\xa0\xa0 └── pkg\n│\xa0\xa0     └── index.js\n├── run.sh\n└── src\n    ├── lib\n    │\xa0\xa0 ├── util.c\n    │\xa0\xa0 ├── util.h\n    │\xa0\xa0 ├── x10.c\n    │\xa0\xa0 └── x9.c\n    └── main.c\n\n10 directories, 17 files\n'),
    ("-a -I '.git' --gitignore tr", 0, 'tr\n├── .gitignore\n├── .hidden\n│\xa0\xa0 └── h.txt\n├── Makefile\n├── README.md\n├── a b.txt\n├── docs\n│\xa0\xa0 └── guide.md\n├── empty\n├── link -> src\n├── run.sh\n└── src\n    ├── .gitignore\n    ├── lib\n    │\xa0\xa0 ├── util.c\n    │\xa0\xa0 ├── x10.c\n    │\xa0\xa0 └── x9.c\n    └── main.c\n\n7 directories, 12 files\n'),
    ('-dL 1 --noreport tr', 0, 'tr\n├── build\n├── docs\n├── empty\n├── link -> src\n├── node_modules\n└── src\n'),
    ("-P '*.C' --ignore-case tr/src", 0, 'tr/src\n├── lib\n│\xa0\xa0 ├── util.c\n│\xa0\xa0 ├── x10.c\n│\xa0\xa0 └── x9.c\n└── main.c\n\n2 directories, 4 files\n'),
    ('--filesfirst tr/src', 0, 'tr/src\n├── main.c\n└── lib\n    ├── util.c\n    ├── util.h\n    ├── x10.c\n    └── x9.c\n\n2 directories, 5 files\n'),
    ('tr/src tr/docs', 0, 'tr/src\n├── lib\n│\xa0\xa0 ├── util.c\n│\xa0\xa0 ├── util.h\n│\xa0\xa0 ├── x10.c\n│\xa0\xa0 └── x9.c\n└── main.c\ntr/docs\n└── guide.md\n\n3 directories, 6 files\n'),
    ('nosuch', 2, 'nosuch  [error opening dir]\n\n0 directories, 0 files\n'),
]

# tar ARGS on an archive GNU tar made of d/: what GNU tar lists (or, with -x, extracts)
TAR_EXCLUDES = ["*.log", "sub", "d/sub", "sub/*", "*/deep/*", "d*c.txt", "d/*", "deep/", "sub/", "*/", "su?/c.txt",
                "[ab].*", "/d", "d", "*"]
TAR_CASES = [f"-tf x.tar --exclude='{p}'" for p in TAR_EXCLUDES] + [
    "-tf x.tar -X ex.lst", "-tf x.tar --exclude=c.txt d/sub", "-tzf x.tgz --exclude=deep", "-tf x.tar --exclude '*.txt'"]
TAR_EXTRACTS = ["--exclude='*.txt'", "--exclude=deep", "-X ex.lst"]

# timeout ARGS: its status, what the command printed, as GNU timeout
TIMEOUT_CASES = [
    "1 sleep 5", "0.5 sleep 5", "5 true", "5 sh -c 'exit 3'", "0 true", "-s KILL 1 sleep 5", "-s INT 1 sleep 5",
    "--signal=HUP 1 sleep 5", "-k 1 1 sh -c 'trap \"\" TERM; sleep 5'", "-p 1 sleep 5", "--preserve-status 1 sleep 5",
    "-f 1 sleep 5", "-v 1 sleep 5", "1 sh -c 'trap \"exit 0\" TERM; sleep 5 & wait'",
    "-p 1 sh -c 'trap \"exit 7\" TERM; sleep 5 & wait'", "2 sh -c 'kill -TERM $$'", "1 nosuchcmd", "1 /etc/passwd",
    "x sleep 1", "1 sh -c 'echo out; sleep 5'", "3 sh -c 'echo fast'", "-s BOGUS 1 true", "1",
]


def gnu_checks(work):
    import shlex
    g = work / "gnu"
    g.mkdir()
    (g / "sed").mkdir()
    for name, data in SED_FILES.items():
        (g / "sed" / name).write_bytes(data)
    d = g / "tar" / "d"
    (d / "sub" / "deep").mkdir(parents=True)
    for f in ("a.txt", "b.log", "sub/c.txt", "sub/d.log", "sub/deep/e.txt"):
        (d / f).write_text(f + "\n")
    (g / "tar" / "ex.lst").write_text("*.log\ndeep\n")
    subprocess.run("tar -cf x.tar d && tar -czf x.tgz d", shell=True, cwd=g / "tar", check=True)
    diff_pairs = files(g)
    (g / "same").write_bytes((g / "o").read_bytes())

    expected = {}  # output file -> (what was run, what the host's GNU tool gave)
    lines = []

    st = "printf '\\nstatus %s\\n' \"$?\""  # after each command: its status

    def run_host(cmd, cwd):
        return subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, env=dict(os.environ, LC_ALL="C.UTF-8")).stdout

    # sed: a fresh copy of the files for each case, then what it printed and the files it wrote
    for i, args in enumerate(SED_CASES):
        cmd = "sed " + " ".join(shlex.quote(a) for a in args)
        after = "; for f in a.txt out.w a.txt.bak; do [ -f $f ] && { echo \"== $f\"; cat $f; }; done"
        host = Path(tempfile.mkdtemp(dir=work))
        shutil.copytree(g / "sed", host, dirs_exist_ok=True)
        expected[f"sed.{i}"] = (cmd, run_host(f"{cmd} 2>/dev/null; {st}{after}", host))
        shutil.rmtree(host)
        lines.append(f"rm -rf /tmp/s; cp -r /work/gnu/sed /tmp/s; cd /tmp/s; ({cmd} 2>/dev/null; {st}{after})"
                     f" > /work/gnu/sed.{i}")
    for i, (prog, stdin) in enumerate(AWK_CASES):
        (g / f"awk.{i}.awk").write_text(AWK_SHOW + prog + "\n")
        (g / f"awk.{i}.in").write_text(stdin)
        p = subprocess.run(["gawk", "-f", g / f"awk.{i}.awk"], input=stdin.encode(), capture_output=True)
        expected[f"awk.{i}"] = (f"awk '{prog}'", p.stdout + f"\nstatus {p.returncode}\n".encode())
        lines.append(f"(awk -f /work/gnu/awk.{i}.awk < /work/gnu/awk.{i}.in 2>/dev/null; {st}) > /work/gnu/awk.{i}")
    # diff's normal format (no -u or -c), -s, --normal and --unified=N
    diff_cases = [f"{o} {a} {b}" for o in ("", "-b", "-i", "--normal") for a, b in diff_pairs]
    diff_cases += ["-s o same", "-s o o", "--report-identical-files o same", "-s o n1", "-q o same",
                   "--unified=2 -L a -L b o n2", "--unified -L a -L b o n3"]
    for i, c in enumerate(diff_cases):
        expected[f"diff.{i}"] = (f"diff {c}", run_host(f"diff {c}; {st}", g))
        lines.append(f"cd /work/gnu; (diff {c}; {st}) > /work/gnu/diff.{i} 2>/dev/null")
    for i, c in enumerate(TAR_CASES):
        expected[f"tar.{i}"] = (f"tar {c}", run_host(f"tar {c} 2>/dev/null; {st}", g / "tar"))
        lines.append(f"cd /work/gnu/tar; (tar {c} 2>/dev/null; {st}) > /work/gnu/tar.{i}")
    for i, c in enumerate(TAR_EXTRACTS):
        x = f"rm -rf {{d}} && mkdir {{d}} && tar -xf x.tar -C {{d}} {c} 2>/dev/null; {st}; (cd {{d}} && find . | sort)"
        host = Path(tempfile.mkdtemp(dir=work))
        expected[f"tarx.{i}"] = (f"tar -xf x.tar {c}", run_host(x.format(d=host), g / "tar"))
        shutil.rmtree(host)
        lines.append(f"cd /work/gnu/tar; ({x.format(d='/tmp/x')}) > /work/gnu/tarx.{i}")
    for i, c in enumerate(TIMEOUT_CASES):
        expected[f"timeout.{i}"] = (f"timeout {c}", run_host(f"gnutimeout {c} 2>/dev/null; {st}", "/tmp"))
        lines.append(f"cd /tmp; (timeout {c} 2>/dev/null; {st}) > /work/gnu/timeout.{i}")
    # tree: what tree 2.3.1 prints (TREE_CASES, from the real one: tree is seldom on a host)
    lines.append(f"rm -rf /tmp/t; mkdir /tmp/t; cd /tmp/t; {TREE_FIXTURE}")
    for i, (args, status, out) in enumerate(TREE_CASES):
        expected[f"tree.{i}"] = (f"tree {args}", out.encode() + f"\nstatus {status}\n".encode())
        lines.append(f"cd /tmp/t; (tree {args}; {st}) > /work/gnu/tree.{i} 2>/dev/null")
    (g / "gnu.sh").write_text("\n".join(lines) + "\n")
    t0 = __import__("time").time()
    guest(work, "sh /work/gnu/gnu.sh", timeout=600)
    took = __import__("time").time() - t0

    wrong = {}
    for out, (cmd, ref) in expected.items():
        got = (g / out).read_bytes() if (g / out).exists() else b"(none)"
        if got != ref:
            wrong.setdefault(out.split(".")[0], []).append(
                f"{cmd}\n--- guest\n{got.decode(errors='replace')[:500]}\n--- GNU\n{ref.decode(errors='replace')[:500]}")
    names = {"sed": "GNU sed", "awk": "gawk", "diff": "GNU diff", "tar": "GNU tar", "tarx": "GNU tar",
             "timeout": "GNU timeout", "tree": "tree 2.3.1"}
    for kind, what in [("sed", "GNU sed's addresses, commands, flags and options"),
                       ("awk", "match(s, re, arr)"), ("diff", "the normal format, -s, --normal, --unified=N"),
                       ("tar", "-t with --exclude and -X"), ("tarx", "-x with --exclude and -X"),
                       ("timeout", "statuses (124, 125, 126, 127, 128+N), -s, -k, -p, -f"),
                       ("tree", "tree 2.3.1's options and output")]:
        n = sum(1 for k in expected if k.split(".")[0] == kind)
        check(f"{kind}: {n} uses of {what} print what {names[kind]} prints", kind not in wrong,
              "\n".join(wrong.get(kind, [])[:3]))
    print(f"     ({took:.0f} s in the guest)")

    # no GNU tool to hold these against on the host: what the guest prints, checked
    rc, out = guest(work, r"""
        echo "ns=$(date +%N) ms=$(date +%3N) sns=$(date +%s%N)"
        a=$(date +%s%N); b=$(date +%s%N); [ "$b" -ge "$a" ] && echo "ns increase"
        cd /tmp; printf 'through a pipe\n' > z; gzip -c z > z.gz; bzip2 -c z > z.bz2; tar -czf z.tgz z
        echo "zcat: $(cat z.gz | zcat)"; echo "gunzip -c: $(cat z.gz | gunzip -c)"
        echo "gzip -dc -: $(cat z.gz | gzip -dc -)"; echo "bzcat: $(cat z.bz2 | bzcat)"
        echo "tar xzOf -: $(cat z.tgz | tar -xzOf -)"; echo "tar tzf -: $(cat z.tgz | tar -tzf -)"
        echo "zcat file: $(zcat z.gz)"
        httpd -f -p 4321 & pid=$!; sleep 1
        lsof -i :4321; echo "lsof -i rc=$?"
        echo "-t: [$(lsof -t -i TCP:4321 -s TCP:LISTEN)] pid=$pid"
        lsof -i :4322; echo "none rc=$?"
        lsof -p $$ | head -3
        kill $pid
    """)
    import re
    m = re.search(r"ns=(\d+) ms=(\d+) sns=(\d+)", out)
    check("date +%N, +%3N, +%s%N: nanoseconds", bool(m) and len(m[1]) == 9 and len(m[2]) == 3 and len(m[3]) == 19
          and "ns increase" in out, out)
    check("zcat, gunzip, bzcat and tar -z read a pipe", all(f"{w}: through a pipe" in out for w in (
        "zcat", "gunzip -c", "gzip -dc -", "bzcat", "tar xzOf -", "zcat file")) and "tar tzf -: z" in out, out)
    check("lsof -i: lsof 4.99's columns, a listening socket",
          re.search(r"^COMMAND +PID +USER +FD +TYPE +DEVICE +SIZE/OFF +NODE +NAME$", out, re.M) is not None
          and re.search(r"^httpd +\d+ .* TCP \*:4321 \(LISTEN\)$", out, re.M) is not None and "lsof -i rc=0" in out, out)
    pid = re.search(r"pid=(\d+)", out)
    check("lsof -t -s TCP:LISTEN: the pid; nothing found: status 1",
          bool(pid) and f"-t: [{pid[1]}]" in out and "none rc=1" in out, out)
    check("lsof -p: cwd and rtd first", re.search(r"^sh +\d+ .* cwd +DIR .* /tmp$", out, re.M) is not None
          and re.search(r" rtd +DIR .* /$", out, re.M) is not None, out)


# More of GNU's tools (userspace/patches/busybox-{find,date,du,xargs,realpath,cp-ln,rm,mktemp,ps}-gnu
# .patch): each case runs in a fresh folder FIXTURE makes, in the guest and with the host's GNU
# tools (shell functions stand in for the names), and prints its output, its status and the
# folder after it. Nothing the two machines differ in: users, inodes, blocks, filesystem types.
GNU_TOOLS = {"find": "/usr/bin/find", "xargs": "/usr/bin/xargs", "date": "gnudate", "touch": "gnutouch",
             "du": "gnudu", "realpath": "gnurealpath", "readlink": "gnureadlink", "ln": "gnuln",
             "rm": "gnurm", "cp": "gnucp", "mktemp": "gnumktemp", "mkdir": "gnumkdir", "chmod": "gnuchmod",
             "sort": "gnusort", "mkfifo": "gnumkfifo"}
FIXTURES = {
    "find": "mkdir -p ft/src/lib ft/docs ft/.hid ft/empty; printf 'hello\\n' > ft/README.md; "
            "printf 'int main;\\n' > ft/src/main.c; printf 'x\\n' > ft/src/lib/util.c; printf 'h\\n' > ft/src/lib/util.h; "
            "printf '%5000s' '' > ft/docs/big.bin; : > ft/docs/empty.txt; printf 'MiXeD\\n' > ft/docs/Guide.MD; "
            "printf 'h\\n' > ft/.hid/secret; printf '#!/bin/sh\\n' > ft/run.sh; chmod 755 ft/run.sh; "
            "chmod 600 ft/docs/Guide.MD; chmod 444 ft/src/lib/util.h; ln -s src/main.c ft/link.c; ln -s nowhere ft/dangling; "
            "ln -s loop1 ft/loop2; ln -s loop2 ft/loop1; ln -s src ft/srclink; ln ft/README.md ft/hardlink.md; "
            "for f in README.md hardlink.md src/main.c src/lib/util.c docs/big.bin; do touch -d '2025-03-04 05:06:07.5' ft/$f; done; "
            "touch -d '2025-10-30 12:00:00' ft/src/lib/util.h ft/docs/Guide.MD ft/run.sh ft/docs/empty.txt ft/.hid/secret; "
            "touch -h -d '2025-01-02 03:04:05' ft/link.c",
    "du": "mkdir -p d/a/b d/c d/node_modules/x; printf '%5000s' '' > d/f5k; printf '%150000s' '' > d/a/big; "
          "echo hi > d/a/b/s.txt; printf '%70000s' '' > d/c/m.log; printf '%300s' '' > d/node_modules/x/n.js; "
          "ln d/f5k d/hard; ln -s f5k d/sym; printf 'node_modules\\n*.log\\n' > ex.lst",
    "xargs": "printf 'a b\\nc d\\ne\\n\\nf  \\ng h\\n' > in.txt; printf 'x,y,,z\\n' > comma.txt; "
             "printf '\"q 1\" \\047q 2\\047 r\\\\ 3\\n4\\n' > quoted.txt; printf 'l1 \\nl2\\nl3 \\nl4\\nl5\\n' > cont.txt; "
             "printf 'a\\000b c\\000d' > nul.txt",
    "path": "mkdir -p t/a/b t/c; : > t/a/f; ln -s a/b t/lb; ln -s ../c t/a/lc; ln -s nowhere t/dang; "
            "ln -s loop t/loop; ln -s f t/a/lf; cd t/a",
    "files": "mkdir -p w/d/s w/e/d; echo x > w/a; echo yy > w/b; echo q > w/d/f; echo r > w/d/s/g; echo old > w/e/f; "
             "echo o2 > w/e/d/f; chmod 640 w/a; chmod 750 w/d/s; echo p > w/ro; chmod 444 w/ro; : > w/z; "
             "touch -d '2025-01-02 03:04:05.123456789' w/a w/d/f; ln -s a w/la; ln -s d w/ld; ln w/b w/hb; "
             "ln w/d/f w/d/hf; mkfifo w/fifo; cd w",
    "none": ":",
}
LIST_TREE = ("find . -mindepth 1 \\( -type l -printf '%p -> %l\\n' -o -type d -printf '%p d %m\\n' "
             "-o -printf '%p %y %m %s %n\\n' \\) | LC_ALL=C sort; "
             "find . -mindepth 1 ! -type l ! -newermt 2026-01-01 -printf 'old %p %T@\\n' | LC_ALL=C sort")
GNU2_CASES = [("find", c, "") for c in [
    "find ft -printf '%p\\n'", "find ft -printf '%P|%f|%h|%H|%d|%y|%Y\\n'", "find ft/ -maxdepth 1 -printf '[%f][%h][%P]\\n'",
    "find ft// -maxdepth 0 -printf '[%f][%h]\\n'", "find ft/src -printf '%10p|%-12f|%.3P|\\n'",
    "find ft -type f -printf '%s %m %#m %M %n %p\\n'",
    "find ft -name '*.c' -printf '%T@ %t %TY-%Tm-%Td %TH:%TM:%TS %T+ %TT\\n'",
    "find ft -name README.md -printf '%Tb %Te %Tk %TI %Tp %Tr %TD %TF %Ts %Tz %TZ %Tw %TU %Tj\\n'",
    "find ft -type l -printf '%p -> %l %Y\\n'", "find ft -name README.md -printf 'a\\tb\\\\c\\101\\0x\\n'",
    "find ft -name README.md -printf 'before\\cafter\\n'", "find ft -name README.md -printf '%z %Q\\q\\n'",
    "find ft -name README.md -printf '%%|%5%|\\n'", "find ft -name README.md -printf '%Aq|%Tk\\n'",
    "find ft -name README.md -printf '%'", "find ft -type f -fprint out.txt; cat out.txt",
    "find ft -name '*.c' -fprintf out.txt '%f\\n'; cat out.txt", "find ft -name '*.c' -fprint0 out.txt; cat out.txt",
    "find ft -iregex '.*\\.MD'", "find ft -regex '.*\\.\\(c\\|h\\)'", "find ft -regex '.*/[a-z]+\\.c'",
    "find ft -regextype posix-extended -regex '.*\\.(c|h)'", "find ft -regextype posix-basic -regex '.*\\.\\(c\\|h\\)'",
    "find ft -regextype egrep -iregex '.*GUIDE.*'", "find ft -regextype bogus -regex x",
    "find ft -newermt '2025-06-01'", "find ft -newermt '2025-10-30 12:00:00'", "find ft -not -newermt '2025-06-01' -type f",
    "find ft -newermm ft/src/lib/util.h", "find ft -newer ft/src/main.c -type f", "find ft -newerXY x",
    "find ft -newermt garbage", "find ft -readable -type f", "find ft -executable -type f", "find ft -true",
    "find ft -false", "find ft -name '*.c' -o -false", "find ft -lname 'src*'", "find ft -ilname 'NOW*'",
    "find ft -iwholename '*SRC/LIB*'", "find ft -xtype l", "find ft -xtype f", "find -L ft -xtype l 2>/dev/null", "find ft -type fd", "find ft -type f,f", "find ft -bogus",
    "find ft -type f,l", "find ft -type d,p", "find ft -type f,", "find ft -type x", "find ft -noleaf -name '*.c'",
    "find ft -mount -name '*.c'", "find ft -ignore_readdir_race -warn -nowarn -name '*.h'",
    "find ft -name '*.c' -execdir echo {} \\;", "find ft -name '*.c' -execdir pwd \\;",
    "find ft -type f -execdir echo {} +", "find ft/src -execdir echo '[{}]' \\;", "find ft -maxdepth 0 -execdir echo {} \\;",
    "find ft -empty -printf '%y %p\\n'", "find ft -path '*src*' -prune -o -type f -printf '%P\\n'",
    "find ft -size +1k -printf '%s %f\\n'", "find ft -daystart -mtime -400 -type f"]] + [
    ("none", f"date -d '{d}' '+%F %T.%N %Z %a'", "") for d in [
    "2026-06-01", "2026-01-31 + 1 month", "2026-03-31 -1 month", "2026-01-02T03:04:05", "2026-01-02T03:04:05Z",
    "2026-01-02T03:04:05+09:00", "2026-01-02 03:04:05 +0900", "2026-01-02T03:04:05.5Z", "@1700000000",
    "@1700000000.25", "Jan 2 2026", "2 Jan 2026", "January 2, 2026", "1/2/2026", "20260102", "2026-06-01 3pm",
    "2026-06-01 3:04 pm", "2026-06-01 12am", "2026-01-02 10:00 EST", "2026-01-02 10:00 KST", "2026-02-30",
    "2026-06-01 25:00", "garbage", "Mon, 02 Nov 2026 01:02:03 +0000", "Mon Nov  2 01:02:03 UTC 2026",
    "2026-11-02 01:02:03.123456789", "2026-06-01 + 3 days 2 hours", "2026-06-01 1 year 2 months ago",
    "2026-06-01 next monday", "2026-06-01 10:00 yesterday", "2024-02-29 + 1 year", "26-01-02", "2026/01/02",
    "2-Jan-2026"]] + [
    ("none", f"date -d '{d}' +%F", "") for d in ["yesterday", "tomorrow", "2 days ago", "next friday", "last month",
                                                  "thursday week", "1 week ago", "today"]] + [
    ("none", "touch -d '2026-01-02 03:04:05.5' f && find f -printf '%T@\\n'", ""),
    ("none", "touch -d '@1700000000.123' f && find f -printf '%T@\\n'", ""),
    ("none", "touch -d '2026-02-30' f; echo $?", ""), ("none", "touch -d 'yesterday 10:00' f && find f -printf '%TF %TT\\n'", "")] + [
    ("du", f"du {o} | LC_ALL=C sort -k2", "") for o in [
    "-b d", "--apparent-size d", "-h --apparent-size d", "--si -b d", "-B1000 --apparent-size d",
    "-BK --apparent-size d", "--inodes d", "-a -b --exclude=node_modules d", "--exclude='*.log' -a -b d",
    "--exclude=d/c -b d", "--exclude='*/x' -a -b d", "-s -b d", "-s -b d d/a", "-c -b d/a d/c", "-sc -b d/a d/c",
    "--max-depth=1 -b d", "--max-depth 0 -b d", "-d 1 -h -b d", "--all --max-depth=1 -b d", "--total --bytes d/a",
    "-S -b d", "-c -S -b d", "-b -t 10K d", "-b -t -10K d", "-l -a -b d", "-X ex.lst -a -b d", "-L -s -b d",
    "-H -s -b d/sym", "-s -b d/sym", "-b d/f5k d/a/big", "-0 -s -b d | tr '\\0' @", "-b --threshold=100K -a d"]] + [
    ("xargs", f"xargs {a} < {i}", "") for i, a in [
    ("in.txt", "echo"), ("in.txt", "-L 1 echo"), ("in.txt", "-L 2 echo"), ("in.txt", "-l echo"),
    ("in.txt", "--max-lines=2 echo"), ("cont.txt", "-L 1 echo"), ("cont.txt", "-L 2 echo"), ("quoted.txt", "-L 1 echo"),
    ("in.txt", "-d '\\n' echo"), ("in.txt", "-d '\\n' -n 2 echo"), ("comma.txt", "-d , echo"),
    ("comma.txt", "-d , -n 1 echo"), ("nul.txt", "-d '\\0' echo"), ("in.txt", "-d '\\x0a' echo"),
    ("in.txt", "-d '\\012' echo"), ("in.txt", "-d ab echo"), ("in.txt", "-d '\\n' -I{} echo [{}]"),
    ("nul.txt", "-0 -I{} echo [{}]"), ("comma.txt", "-d , -I{} echo [{}]"), ("in.txt", "--max-args=2 echo"),
    ("in.txt", "--verbose echo"), ("in.txt", "--eof=c echo"), ("in.txt", "--replace=% echo [%]"),
    ("in.txt", "--arg-file=comma.txt -d , echo"), ("in.txt", "--max-chars=12 echo"), ("in.txt", "-L 1 -n 2 echo"),
    ("in.txt", "-n 2 -L 1 echo"), ("in.txt", "-n 1 sh -c 'exit 255'"), ("in.txt", "-L 1 sh -c 'exit 3'")]] + [
    ("path", c, "") for c in [
    "realpath f", "realpath ../lb", "realpath ../lb/..", "realpath lc/../a", "realpath ../missing",
    "realpath ../missing/x", "realpath f/x", "realpath ../dang", "realpath ../loop", "realpath -e ../missing",
    "realpath -m ../missing/x/../y", "realpath -m f/x", "realpath -s ../lb/..", "realpath -s lf", "realpath -L ../lb/..",
    "realpath -L lc/../b", "realpath --relative-to=. ../c", "realpath --relative-to=b ../c/x",
    "realpath --relative-base=.. f /etc", "realpath --relative-base=. --relative-to=.. f ../c",
    "realpath -z . ../c | tr '\\0' @", "realpath -q ../missing/x", "realpath . ../missing/x ../c", "realpath /etc/../..",
    "readlink ../lb", "readlink f", "readlink -f ../lb/..", "readlink -e ../missing", "readlink -m ../missing/x",
    "readlink -n ../lb ../dang", "readlink ../lb ../dang .", "readlink -v .", "readlink -v ../missing",
    "readlink -e ../dang", "readlink -f ../dang", "readlink -f ../loop", "readlink --canonicalize-missing ../q/r"]] + [
    ("files", c, LIST_TREE) for c in [
    "ln -s a d/x", "ln -sr a d/x", "ln -sr a e/d/x", "ln -sr la e/x", "ln -sr ld/../a e/x",
    "ln -r a x", "ln -t d a b", "ln -st d a b", "ln -sv a d/y", "ln -s a b", "ln -sf a b", "ln -sbv a b",
    "ln -s --backup=numbered a b; ln -s --backup=numbered a b", "ln -s --suffix=.old -b a b", "ln -L la hl",
    "ln la hl", "ln -sT a d", "ln -n -s a ld", "ln -s a ld", "ln a b", "ln -v a c", "ln d z", "ln -s a b c",
    "ln -st nodir a", "ln -sr e e/d/x", "ln -s --no-dereference --force a ld",
    "rm a", "rm missing", "rm -f missing", "rm d", "rm -d e/d", "rm -d d", "rm -r d", "rm -rv d", "rm -v a z la",
    "rm .", "rm -r .", "rm -rf d/..", "rm", "rm -f", "rm -r ld", "rm -r ld/", "rm ro",
    "echo y | rm -i a z la fifo", "echo n | rm -i a", "printf 'y\\ny\\ny\\ny\\ny\\ny\\n' | rm -ri d",
    "echo n | rm -ri d", "echo y | rm -I a z la fifo", "echo n | rm -I a z la fifo", "echo y | rm -I -r d",
    "echo y | rm --interactive=once a", "rm --recursive --force --verbose d", "rm a a",
    "rm -rf d e a",
    "cp a c", "cp -bv a b", "cp -b a b; cp -b a b", "cp --backup=numbered -v a b; cp --backup=numbered -v a b",
    "cp -S .bak a b", "cp -rb d e", "cp -rv d x | LC_ALL=C sort", "cp -p a c", "cp --preserve=mode a c", "cp --preserve=timestamps a c",
    "cp --preserve a c", "cp -a --no-preserve=mode a c", "cp --no-preserve=mode a c", "cp -a --no-preserve=timestamps a c",
    "cp -rp d x", "cp -a d x", "cp -r --no-preserve=mode d x", "cp --attributes-only a b", "cp --attributes-only -p a c",
    "cp -r --strip-trailing-slashes d/ x", "cp -x -r d x", "cp --sparse=always a c", "cp -a a la x", "cp -d la y",
    "cp -r d e", "cp -ru d e", "cp -vt e a b", "cp a b", "cp a la", "echo n | cp -i a b", "echo y | cp -i a b",
    "cp -l a b", "cp -fl a b", "cp -s a lb", "cp -fs a b"]] + [
    ("none", c, "find . -mindepth 1 -printf '%y %m %d\\n' | LC_ALL=C sort") for c in [
    "mkdir dd; TMPDIR= mktemp fooXXX | sed 's/foo.../fooRRR/'", "TMPDIR= mktemp fooXX", "mktemp fooXXX.txt | sed 's/foo.../fooRRR/'",
    "mktemp fooXXXbarXXX.c | sed 's/bar.../barRRR/'", "mktemp --suffix=.c fooXXX | sed 's/foo.../fooRRR/'",
    "mktemp --suffix=.c fooXXX.h", "mktemp --suffix=a/b fooXXX", "mkdir dd; mktemp -p dd xXXX | sed 's/x.../xRRR/'",
    "mkdir dd; mktemp -p dd a/xXXX", "mktemp -t a/xXXX", "mktemp nodir/xXXX", "mktemp -d fooXXX | sed 's/foo.../fooRRR/'",
    "mktemp -d fooXXX.d | sed 's/foo.../fooRRR/'", "mktemp -u fooXXX | sed 's/foo.../fooRRR/'", "mktemp -q nodir/xXXX",
    "mkdir dd; mktemp -p dd | sed 's/tmp\\.........../tmp.RRRRRRRRRR/'", "mkdir dd; TMPDIR=$PWD/dd mktemp -t fXXX | sed 's/.*\\/f.../fRRR/'",
    "mkdir dd; mktemp -p dd /abs/xXXX"]]


def gnu2_tool(cmd):
    """the tool a case is about: the first of them named in it"""
    import re
    m = re.search(r"\b(find|date|touch|du|xargs|realpath|readlink|ln|rm|cp|mktemp)\b", cmd)
    return m[1] if m else "?"


def gnu2_checks(work):
    import re, shlex
    g = work / "gnu2"
    g.mkdir()
    funcs = "".join(f"{name}() {{ {path} \"$@\"; }}\n" for name, path in GNU_TOOLS.items())
    host_lines, guest_lines = ["export TZ=UTC LC_ALL=C", funcs], ["export TZ=UTC"]
    for i, (fx, cmd, listing) in enumerate(GNU2_CASES):
        for lines, root, out in ((host_lines, str(g / "h"), str(g / f"host.{i}")),
                                 (guest_lines, "/tmp/g", f"/work/gnu2/guest.{i}")):
            lines.append(f"rm -rf {root}; mkdir -p {root}; cd {root}; umask 022; {FIXTURES[fx]}\n"
                         f"({cmd}) > {out} 2>&1; echo \"rc=$?\" >> {out}; ({listing or ':'}) >> {out} 2>&1; cd /")
    (g / "host.sh").write_text("\n".join(host_lines) + "\n")
    (g / "guest.sh").write_text("\n".join(guest_lines) + "\n")
    subprocess.run(["sh", str(g / "host.sh")], capture_output=True, timeout=600)
    guest(work, "sh /work/gnu2/guest.sh", timeout=1200)

    def norm(text, root):
        text = text.replace(root, "/tmp/g")
        text = re.sub(r"(?:/usr/bin/)?gnu([a-z]+): ", r"\1: ", text)
        text = re.sub(r"/usr/bin/([a-z]+): ", r"\1: ", text)
        text = text.replace("\u2018", "'").replace("\u2019", "'")
        text = text.replace("Symbolic link loop", "Too many levels of symbolic links")  # musl's, glibc's
        keep = [l for l in text.split("\n") if not l.startswith(("Try '", "Valid arguments are:", "  - "))
                and "behavior of -n is non-portable" not in l]
        return "\n".join(keep)
    wrong, counts = {}, {}
    for i, (fx, cmd, listing) in enumerate(GNU2_CASES):
        tool = "mktemp" if "mktemp" in cmd else "touch" if cmd.startswith("touch") else gnu2_tool(cmd)
        counts[tool] = counts.get(tool, 0) + 1
        h = (g / f"host.{i}").read_text(errors="replace") if (g / f"host.{i}").exists() else "(none)"
        b = (g / f"guest.{i}").read_text(errors="replace") if (g / f"guest.{i}").exists() else "(none)"
        h, b = norm(h, str(g / "h")), norm(b, "/tmp/g")
        if h != b:
            wrong.setdefault(tool, []).append(f"{cmd}\n--- guest\n{b[:700]}\n--- GNU\n{h[:700]}")
    for tool in ["find", "date", "touch", "du", "xargs", "realpath", "readlink", "ln", "rm", "cp", "mktemp"]:
        check(f"{tool}: {counts.get(tool, 0)} uses of GNU {tool}'s options print and do what GNU {tool} does",
              tool not in wrong, "\n".join(wrong.get(tool, [])[:3]))

    # ps: the guest's own processes, no procps to hold them against
    rc, out = guest(work, r"""
        sleep 30 & S=$!
        echo "[$(ps -o pid= -p $S)] [$S]"
        ps -p 999999; echo "none rc=$?"
        ps -ef | head -1; ps aux | head -1; ps -l -p $S | head -1; ps -j -p $S | head -1
        ps -o pid,ppid,stat,etime,time,args -p $S
        ps -o pid=,comm= -p $S; ps -C sleep -o comm=; ps -p 1,$S -o pid= --sort=-pid
        ps --ppid $$ -o comm= | sort | head -3
        ps -o user= -p $S; ps -o pid:3,comm:4,user -p $S | tail -1
        ps -e -o pid,comm --forest | grep -c .
        kill $S
    """)
    pid = re.search(r"\[ *(\d+)\] \[(\d+)\]", out)
    check("ps -o pid= -p PID: that pid, no header; none found: status 1",
          bool(pid) and pid[1] == pid[2] and "none rc=1" in out, out)
    check("ps -ef, aux, -l, -j: procps-ng's headers",
          "UID          PID    PPID  C STIME TTY          TIME CMD" in out
          and "USER         PID %CPU %MEM    VSZ   RSS TTY      STAT START   TIME COMMAND" in out
          and "F S   UID     PID    PPID  C PRI  NI ADDR SZ WCHAN  TTY          TIME CMD" in out
          and "    PID    PGID     SID TTY          TIME CMD" in out, out)
    check("ps -o, -C, --sort, --ppid: the columns and selections asked for",
          re.search(r"^\s+\d+\s+\d+ S\s+\d\d:\d\d 00:00:00 sleep 30$", out, re.M) is not None
          and re.search(r"^\s+\d+ sleep$", out, re.M) is not None and "\nsleep\n" in out
          and re.search(r"^\s+%s\n\s+1$" % (pid[2] if pid else "x"), out, re.M) is not None
          and re.search(r"^root$", out, re.M) is not None and re.search(r"^ *\d+ slee root$", out, re.M) is not None, out)


def main():
    if not shutil.which("diff") or "GNU" not in subprocess.run(["diff", "--version"], capture_output=True, text=True).stdout:
        print("needs GNU diff on the host (the reference)", file=sys.stderr)
        return 2
    if "GNU grep" not in subprocess.run("grep --version", shell=True, capture_output=True, text=True).stdout:
        print("needs GNU grep on the host (the reference)", file=sys.stderr)
        return 2
    for tool, version in (("sed", "GNU sed"), ("tar", "GNU tar"), ("gawk", "GNU Awk"), ("gnutimeout", "GNU coreutils"),
                          ("/usr/bin/find", "GNU findutils"), ("/usr/bin/xargs", "GNU findutils"),
                          ("gnudate", "GNU coreutils"), ("gnucp", "GNU coreutils")):
        if version not in subprocess.run(f"{tool} --version", shell=True, capture_output=True, text=True).stdout:
            print(f"needs {version} on the host as {tool} (the reference)", file=sys.stderr)
            return 2
    work = Path(tempfile.mkdtemp(prefix="busybox-tools-"))
    os.chmod(work, 0o777)
    pairs = files(work)
    options = ["-c", "-C0", "-C1", "-C6", "-c -b", "-c -i"]
    lines, expected = [], {}
    for o in options:
        for a, b in pairs:
            key = f"{o} {a} {b}"
            out = f"out.{len(expected)}"
            ref = subprocess.run(f"diff {o} -L {a} -L {b} {a} {b}", shell=True, cwd=work, capture_output=True)
            expected[out] = (key, ref.stdout, ref.returncode)
            lines.append(f"diff {o} -L {a} -L {b} {a} {b} > {out}; echo \"$?\" > {out}.rc")
    (work / "diffs.sh").write_text("\n".join(lines) + "\n")
    rc, out = guest(work, "sh diffs.sh")
    wrong = []
    for out_name, (key, ref, ref_rc) in expected.items():
        got = (work / out_name).read_bytes() if (work / out_name).exists() else b"(none)"
        got_rc = (work / f"{out_name}.rc").read_text().strip() if (work / f"{out_name}.rc").exists() else "?"
        if got != ref or got_rc != str(ref_rc):
            wrong.append(f"diff {key}: status {got_rc} (GNU {ref_rc})\n--- guest\n{got.decode(errors='replace')[:600]}"
                         f"\n--- GNU diff\n{ref.decode(errors='replace')[:600]}")
    check(f"diff -c/-C: {len(expected)} comparisons print what GNU diff prints", not wrong, "\n".join(wrong[:3]))

    rc, out = guest(work, """
        cd /tmp
        printf 'echo "ran $0 [$*]"\\n' > s.sh; chmod +x s.sh
        printf '\\177ELF\\002\\001\\001\\000\\000\\000\\000\\000' > elf; chmod +x elf
        ./s.sh a "b c"; echo "rc $?"
        PATH=/tmp:$PATH s.sh path
        env ./s.sh env
        echo xargs | xargs ./s.sh
        find /tmp -name s.sh -exec {} find \\;
        sh -c './s.sh nested'
        ./elf 2>&1; echo "elf rc $?"
        env ./elf 2>&1; echo "env elf rc $?"
    """)
    check("a script without #!: run by the shell, with its arguments", "ran ./s.sh [a b c]" in out and "rc 0" in out, out)
    check("... found through PATH", "ran /tmp/s.sh [path]" in out, out)
    check("... by env, xargs and find -exec", "ran ./s.sh [env]" in out and "ran ./s.sh [xargs]" in out
          and "ran /tmp/s.sh [find]" in out, out)
    check("... from sh -c", "ran ./s.sh [nested]" in out, out)
    check("an ELF file is not taken for a script: Exec format error", "can't execute './elf': Exec format error" in out
          and "elf rc 2" in out and "env elf rc 126" in out and "Exec format error" in out.split("elf rc 2")[1], out)

    # hush against the host's bash, script by script
    scripts = {
        "redirect a descriptor named by a word": r"""
fd=2; echo "to fd 2" >&$fd 2>&1
exec 5>/tmp/five; n=5; echo "to fd 5" >&$n; cat /tmp/five
f() { printf '%s\n' "to the fd the caller names" >&$1; }; f 1
echo "braces" >&${fd} 2>&1; echo "command substitution" >&$(echo 1)
c=-; ( echo "closed" >&$c ) 2>/dev/null || echo "closed stdout: write failed"
exec 6</tmp/five; i=6; read -r line <&$i; echo "read <&\$i: $line"
empty=; { echo nope >&$empty; } 2>/dev/null; echo "empty word: rc $?"
echo "a literal one" >&1 2>&1
printf 'from a file\n' > /tmp/rd.in
exec 5<&0 </tmp/rd.in && { read -r l; echo "stdin moved aside: $l"; exec 0<&5 5<&-; echo "stdin back"; }
exec 7<&0 </dev/null; echo "configure's exec 7<&0 </dev/null: rc $?"
""",
        # ncurses' configure, CF_PATH_SYNTAX, as it writes it
        "an unquoted expansion in a case pattern is a pattern": r"""
GLOB_FULLPATH_POSIX='/*'; GLOB_FULLPATH_OTHER='[a-zA-Z]:[\\/]*'
for v in /usr/bin/pkg-config 'C:\x' . '${prefix}/x' no 'a b'; do
  case "x$v" in
  (x\$\(*\)*|x\'*\'*) r=dollar-paren ;;
  (x.|x$GLOB_FULLPATH_POSIX|x$GLOB_FULLPATH_OTHER) r=path ;;
  (x\$\{*prefix\}*|x\$\{*dir\}*) r=prefix ;;
  (xno|xNONE/*) r=no ;;
  (*) r=other ;;
  esac
  echo "$v -> $r"
done
p='/*'; case /a/b in $p) echo "unquoted: a pattern" ;; *) echo "unquoted: literal" ;; esac
case /a/b in "$p") echo "quoted: a pattern" ;; *) echo "quoted: literal" ;; esac
q='a?c'; case abc in $q) echo "? matches" ;; esac; case 'a?c' in "$q") echo "quoted ? literal" ;; esac
s='x y'; case 'x y' in $s) echo "not split" ;; esac
case 'a*b' in a\*b) echo "escaped star literal" ;; esac
""",
        # libtool's : ${MV="mv -f"} and the like
        "${var=word} assigns a quoted word as it reads": r"""
: ${A="mv -f"}; printf '[%s]\n' "$A"
: ${B:="x*y[1]"}; printf '[%s]\n' "$B"
: ${C=a-b}; printf '[%s]\n' "$C"
printf '[%s]\n' "${D-"p -q"}" ${E-"r -s"}
F="${G="t -u"}"; printf '[%s] [%s]\n' "$F" "$G"
: ${H='v -w'}; printf '[%s]\n' "$H"
: ${I="back\\slash"}; printf '[%s]\n' "$I"
""",
        # automake's test-suite.log recipe and test-driver: set -u, and xz's own scripts
        "set -u: an unset variable is an error where its value is used": r"""
set -u
case $- in *u*) echo "\$- has u" ;; *) echo "\$- lacks u" ;; esac
unset x; y=
echo "default: [${x-d}] [${x:-d}] [${x+alt}] assign: [${x=a}]"; unset x
echo "set but empty: [$y] [${#y}] [${y:-e}]"
echo "no positional parameters: $# [$*] [$@]"; for a in "$@"; do echo never; done
( echo "unset: $x"; echo "not reached" ); [ $? -ne 0 ] && echo "\$x: failed"
( echo "${x}"; echo "not reached" ); [ $? -ne 0 ] && echo "\${x}: failed"
( echo "${#x}"; echo "not reached" ); [ $? -ne 0 ] && echo "\${#x}: failed"
( echo "${x%a}"; echo "not reached" ); [ $? -ne 0 ] && echo "\${x%a}: failed"
( echo "$1"; echo "not reached" ); [ $? -ne 0 ] && echo "\$1: failed"
( set +u; echo "set +u: [$x]" )
( set +o nounset; set -o nounset; echo "$x" ); [ $? -ne 0 ] && echo "set -o nounset: failed"
sh -u -c 'echo "$zz"; echo "not reached"'; [ $? -ne 0 ] && echo "sh -u: failed"
sh -c 'echo "without -u: [$zz]"'
""",
        # automake's result_count () \<newline>{ \ in its test-suite.log recipe
        "a function definition across backslash-newlines": r"""
result_count () \
{ \
    echo "result_count $1 $2"; \
}; result_count PASS 19
g \
() { echo "g"; }; g
h ( \
) { echo "h"; }; h
k () {\
 echo "k"; }; k
( \
echo "subshell" )
{ \
echo "group"; }
""",
    }
    bash = shutil.which("bash")
    for name, script in scripts.items():
        (work / "case.sh").write_text(script)
        ref = subprocess.run([bash, "-c", f"bash {work}/case.sh 2>&1"],
                             capture_output=True, text=True, cwd="/tmp")
        rc, out = guest(work, "sh /work/case.sh 2>&1")
        # the messages a shell words its own way: as many of them, the rest the same
        # (dash, the host's sh, says "parameter not set" where bash and hush say "unbound variable")
        own = ("ambiguous redirect", "unbound variable|parameter not set")
        def count(text, m):
            return sum(text.count(w) for w in m.split("|"))
        mine = [l for l in out.splitlines() if not any(count(l, m) for m in own)]
        want = [l for l in ref.stdout.splitlines() if not any(count(l, m) for m in own)]
        check(f"hush: {name}, as bash", mine == want and all(count(out, m) == count(ref.stdout, m) for m in own),
              f"--- hush\n{out}\n--- bash\n{ref.stdout}")

    grep_checks(work)
    gnu_checks(work)
    gnu2_checks(work)

    shutil.rmtree(work, ignore_errors=True)
    print(f"{len(fails)} failed" if fails else "all passed")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
