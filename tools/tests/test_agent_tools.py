"""rg, column, scp, zip and perl in the guest (tools.cpio): the command-line tools agents reach for.
rg with PCRE2 and .gitignore; column's tables and JSON; scp through a stand-in ssh that runs the
remote side here (`-S`), so the transfer protocol runs both ways in the guest; zip's modes, read back
by busybox unzip and Python's zipfile; perl's modules, Encode's CJK tables, and its children
without fork (system, qx//, piped opens, IPC::Open3, prove).

    COLLABO_RUNTIME=dist/runtime/collabo-core-linux-x64 python3 tools/tests/test_agent_tools.py
    TOOLS_IMAGE=tools/out/tools.cpio INITRAMFS=userspace/out/initramfs.cpio ...   other images
"""
import json, os, re, subprocess, sys
RT = os.environ["COLLABO_RUNTIME"]
fails = []
def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f"\n     {str(detail)[-1500:]}"))
    if not ok: fails.append(name)

entry = json.load(open(os.path.join(RT, "manifest.json")))["entry"]
images = {"--tools-image": os.environ.get("TOOLS_IMAGE"), "--initramfs": os.environ.get("INITRAMFS")}
argv, words = [os.path.join(RT, entry[0])], iter(entry[1:])
for word in words:
    argv.append(word)
    if word.startswith("--"):
        value = next(words)
        argv.append(images.get(word) or (os.path.join(RT, value) if value.startswith("app/") else value))

def guest(script, timeout=600):
    """Output of a shell script run in the guest (after the prompt)."""
    out = subprocess.run([argv[0], "exec"] + argv[1:] + ["--no-network", "--", "sh", "-c", script],
                         capture_output=True, text=True, timeout=timeout).stdout.replace("\r", "")
    return out.split("root@collabo:~# ", 1)[-1]

out = guest(r'''
cd /tmp
mkdir -p proj/src proj/node_modules/x && cd proj && git init -q
printf 'def hello():\n    return 1\n' > src/a.py; printf 'foo(bar)\n' > src/b.js; echo hello > node_modules/x/c.py
printf 'node_modules\n' > .gitignore; printf '한글 검색\n' > k.txt
echo "== rg"; rg --version | head -1
rg -n hello | sort; rg -c -P 'foo(?=\()' src/b.js; rg -l '검색'; rg nothing-here; echo "rg-miss=$?"
echo "== column"
printf 'name size\nalpha 10\nbeta 2000\n' | column -t
printf 'a:b\n1:22\n' | column -t -s: -o '|'
printf 'a 1\n' | column -t -N k,v -J | tr -d ' \n'; echo
cd /tmp
echo "== scp"
printf '#!/bin/sh\nwhile [ "${1#-}" != "$1" ]; do shift; done; shift; exec sh -c "$*"\n' > fakessh; chmod +x fakessh
mkdir -p s1/sub s2; echo one > s1/f1; echo two > s1/sub/f2; chmod 640 s1/f1
scp -S ./fakessh -r -p s1 host:/tmp/s2/; echo "scp-r=$?"; cat s2/s1/sub/f2; stat -c %a s2/s1/f1
scp -S ./fakessh host:/tmp/s1/f1 /tmp/got; echo "scp-get=$? $(cat got)"
scp -S ./fakessh host:/tmp/nope /tmp/x; echo "scp-missing=$?"
echo "== zip"
mkdir -p z/d/sub && cd z && seq 1 3000 > d/a.txt && echo hi > d/sub/b.txt && ln -s a.txt d/link
zip -qr t d; echo "zip=$?"; zip -u t d/a.txt; echo "zip-u=$?"; zip -d t d/link; echo "zip-d=$?"
zip -sf t | tail -1
zip -q -P secret e d/a.txt; echo piped | zip -q s -; zip -q - d/sub/b.txt | cat > stream.zip
zip nothing nope; echo "zip-nothing=$?"
mkdir out && (cd out && unzip -q ../t.zip && wc -l < d/a.txt)
python3 -c '
import zipfile
for n, pw in (("t.zip", None), ("e.zip", b"secret"), ("s.zip", None), ("stream.zip", None)):
    z = zipfile.ZipFile(n); z.setpassword(pw)
    print(n, "ok" if z.testzip() is None else "BAD")
'
cd /tmp
echo "== perl"
perl -e 'printf "%s %d-bit\n", $^V, 8*length(pack "j", 0)'
perl -le 'print 2**40'
perl -e 'my $r = system("sh", "-c", "exit 3"); print "system=", $? >> 8, "\n"'
perl -e 'print "qx=", `echo hi | tr a-z A-Z`'
perl -e 'open(my $f, "-|", "printf", "a\nb\n") or die; my @l = <$f>; print "pipe=", scalar(@l), "\n"'
perl -e 'my $r = system("no-such-cmd-xyz"); print "missing=$r\n"' 2>/dev/null
perl -e 'defined(my $p = fork) or print "fork: unavailable\n"' 2>&1 | sed 's/ at -e.*//'
perl -MIPC::Open3 -e 'my $pid = open3(my $i, my $o, undef, "tr", "a-z", "A-Z"); print $i "open3\n"; close $i; print <$o>; waitpid $pid, 0'
perl -MIPC::Cmd=run -e 'my ($ok, $e, $full) = run(command => "echo ipc-cmd"); print $ok ? "@$full" : "ipc-cmd failed: $e\n"'
perl -MData::Dumper -e '$Data::Dumper::Indent = 0; print Dumper([1, {a => 2}]), "\n"'
perl -MEncode -le 'print join " ", map { length encode($_, "\x{d55c}\x{ae00}") } qw(UTF-8 euc-kr UTF-16LE)'
perl -MPOSIX=floor -MList::Util=sum -MDigest::SHA=sha1_hex -MJSON::PP -le 'print floor(3.7), " ", sum(1..4), " ", substr(sha1_hex("a"), 0, 8), " ", JSON::PP->new->canonical->encode({b=>1,a=>2})'
perl -e 'use utf8; binmode STDOUT, ":utf8"; print scalar reverse("한글"), "\n"'
mkdir -p pt/t pt/lib && cd pt && printf 'package Foo; sub add { $_[0] + $_[1] } 1;\n' > lib/Foo.pm
printf 'use Test::More tests => 1; use Foo; is(Foo::add(2, 3), 5);\n' > t/a.t
prove -l t 2>&1 | tail -1; echo "prove=$?"
printf 'use Test::More; ok(0); done_testing;\n' > t/b.t
prove -l t > /dev/null 2>&1; echo "prove-failing=$?"
cd /tmp; perldoc -T -f sprintf | head -1
''')

def section(name):
    m = re.search(rf"^== {name}\n(.*?)(?=^== |\Z)", out, re.M | re.S)
    return m.group(1) if m else ""

rg = section("rg")
check("rg 15.2", "ripgrep 15.2.0" in rg, rg)
check("rg honours .gitignore", "src/a.py:1:def hello():" in rg and "node_modules" not in rg, rg)
check("rg -P (PCRE2 lookahead)", re.search(r"^1$", rg, re.M), rg)
check("rg UTF-8 search", "k.txt" in rg, rg)
check("rg exit 1 on no match", "rg-miss=1" in rg, rg)

col = section("column")
check("column -t", "alpha  10\nbeta   2000" in col, col)
check("column -s -o", "a|b\n1|22" in col, col)
check("column -J", '{"table":[{"k":"a","v":"1"}]}' in col, col)

scp = section("scp")
check("scp -r -p up", "scp-r=0\ntwo\n640" in scp, scp)
check("scp down", "scp-get=0 one" in scp, scp)
check("scp missing remote file", "scp-missing=1" in scp, scp)

z = section("zip")
check("zip add/update/delete", "zip=0" in z and "zip-u=12" in z and "deleting: d/link" in z and "zip-d=0" in z, z)
check("zip -sf", re.search(r"Total 4 entries", z), z)
check("zip nothing to do", "zip error: Nothing to do! (nothing.zip)" in z and "zip-nothing=12" in z, z)
check("busybox unzip reads it", re.search(r"^3000$", z, re.M), z)
check("zipfile reads them (encrypted, stdin, streamed)", all(f"{n} ok" in z for n in ("t.zip", "e.zip", "s.zip", "stream.zip")), z)

pl = section("perl")
check("perl 5.44, 64-bit IV", "v5.44.0 64-bit" in pl and "1099511627776" in pl, pl)
check("perl system/qx/piped open", "system=3" in pl and "qx=HI" in pl and "pipe=2" in pl and "missing=-1" in pl, pl)
check("perl fork unavailable", "fork: unavailable" in pl or "unimplemented" in pl, pl)
check("perl IPC::Open3 and IPC::Cmd", "OPEN3" in pl and "ipc-cmd" in pl and "ipc-cmd failed" not in pl, pl)
check("perl Data::Dumper (XS)", "$VAR1 = [1,{'a' => 2}];" in pl, pl)
check("perl Encode UTF-8/euc-kr/UTF-16", "6 4 4" in pl, pl)
check("perl core modules", "3 10 86f7e437 {\"a\":2,\"b\":1}" in pl, pl)
check("perl utf8", "글한" in pl, pl)
check("prove passes and fails", "Result: PASS" in pl and "prove=0" in pl and "prove-failing=1" in pl, pl)
check("perldoc -f", "sprintf FORMAT, LIST" in pl, pl)

print(f"{'all passed' if not fails else str(len(fails)) + ' failed'}")
sys.exit(1 if fails else 0)
