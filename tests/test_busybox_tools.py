#!/usr/bin/env python3
"""What busybox gained for builds and test suites, in the guest:

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
    host's bash prints for it, but for the messages a shell words its own way.

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


def main():
    if not shutil.which("diff") or "GNU" not in subprocess.run(["diff", "--version"], capture_output=True, text=True).stdout:
        print("needs GNU diff on the host (the reference)", file=sys.stderr)
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

    shutil.rmtree(work, ignore_errors=True)
    print(f"{len(fails)} failed" if fails else "all passed")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
