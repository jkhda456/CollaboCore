"""Subversion in the guest: tools.cpio's svn and the other nine commands (one program, links to
it), over file://, svn:// (svnserve, a thread per connection) and svn+ssh:// (the tunnel is a
child started without fork, tools/patches/apr-wasm.patch), with hook scripts, an external diff
and editor, dump and load, svnsync and svnrdump; then in a terminal, where `svn commit` opens vi
when no editor is set. With NETWORK=1 also https:// and http:// (serf) to svn.apache.org.

    COLLABO_RUNTIME=dist/runtime/collabo-core-linux-x64 python3 tools/tests/test_svn.py
    TOOLS_IMAGE=tools/out/tools.cpio ...   another tools image
"""
import json, os, re, subprocess, sys, time
S = os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0, S)
import ptydrive
from ptydrive import plain
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

def guest(script, network=False, timeout=300):
    """Output of a shell script run in the guest (after the prompt)."""
    out = subprocess.run([argv[0], "exec"] + argv[1:] + ([] if network else ["--no-network"]) + ["--", "sh", "-c", script],
                         capture_output=True, text=True, timeout=timeout).stdout.replace("\r", "")
    return out.split("root@collabo:~# ", 1)[-1]

out = guest(r'''
set -u
cd /tmp
for c in svn svnadmin svndumpfilter svnfsfs svnlook svnmucc svnrdump svnserve svnsync svnversion; do
  echo "version $c $($c --version | head -1)"
done
svn --version | grep -E "^\* ra_|handles"
svnadmin create repo
cat > repo/hooks/pre-commit <<'EOF'
#!/bin/sh
echo "pre-commit cwd=$(pwd)" >> /tmp/hook.log
case "$(svnlook log -t "$2" "$1")" in *reject*) echo "the hook says no" >&2; exit 1;; esac
EOF
chmod +x repo/hooks/pre-commit
svnmucc -m layout -U file:///tmp/repo mkdir trunk mkdir branches put /etc/hostname trunk/host.txt > /dev/null && echo mucc-ok
svnmucc --no-such-option 2>&1 | head -2 | sed 's/^/bad-option /'
svn co -q file:///tmp/repo/trunk wc && cd wc
printf 'one\n' > f.txt && svn add -q f.txt
svn ci -q -m "please reject" 2>&1; echo "rejected rc=$?"
svn ci -q -m "add f" && echo committed
head -1 /tmp/hook.log
printf 'one\ntwo\n' > f.txt
svn diff --diff-cmd diff -x -u | grep -E "^[-+@ ]" | sed 's/^/ext-diff /'
printf '#!/bin/sh\necho "message from the editor" > "$1"\n' > /tmp/ed.sh && chmod +x /tmp/ed.sh
SVN_EDITOR=/tmp/ed.sh svn ci -q --force-interactive && svn log -l1 ^/trunk | grep "^message from the editor"
svn up -q && echo "svnversion $(svnversion)"
svn cp -q -m branch ^/trunk ^/branches/b && svn sw -q ^/branches/b && echo three >> f.txt && svn ci -q -m "on b"
svn sw -q ^/trunk && svn merge -q ^/branches/b && svn ci -q -m merged && echo "merged: $(tail -1 f.txt)"
svn st -v f.txt | sed 's/^/status /'
cd /tmp
svnadmin verify -q repo && echo verified
svnadmin dump -q repo > d.dump && svnadmin create r2 && svnadmin load -q r2 < d.dump && echo "loaded $(svnlook youngest r2)"
echo "filtered $(svndumpfilter include /trunk --quiet --skip-missing-merge-sources < d.dump | grep -c '^Node-path')"
svnlook tree repo /trunk | sed 's/^/tree /'
svnfsfs stats repo | grep -q "Reading revisions" && echo fsfs-stats
printf '[general]\nanon-access = write\n' > repo/conf/svnserve.conf
svnserve -d --foreground --listen-host 127.0.0.1 -r /tmp &
for i in 1 2 3 4 5 6 7 8 9 10; do svn info -q svn://127.0.0.1/repo >/dev/null 2>&1 && break; sleep 1; done
svn ls svn://127.0.0.1/repo/trunk | sed 's/^/svn-ls /'
svn co -q svn://127.0.0.1/repo/trunk wc2 && echo four >> wc2/f.txt && svn ci -q -m "over svn://" wc2 && echo "svn-commit $(svnlook youngest repo)"
echo "rdump $(svnrdump dump -q svn://127.0.0.1/repo | grep -c '^Revision-number')"
svnadmin create mirror && printf '#!/bin/sh\nexit 0\n' > mirror/hooks/pre-revprop-change && chmod +x mirror/hooks/pre-revprop-change
svnsync init -q file:///tmp/mirror svn://127.0.0.1/repo && svnsync sync -q file:///tmp/mirror && echo "synced $(svnlook youngest mirror)"
kill %1
printf '#!/bin/sh\nshift\nexec "$@"\n' > fakessh && chmod +x fakessh
SVN_SSH=/tmp/fakessh svn ls svn+ssh://localhost/tmp/repo/branches | sed 's/^/tunnel /'
SVN_SSH=/tmp/no-such-ssh svn ls svn+ssh://localhost/tmp/repo 2>&1 | sed 's/^/no-tunnel /'
svn ls svn+ssh://nowhere.invalid/tmp/repo 2>&1 | sed 's/^/dbclient /'
svnserve 2>&1 | sed 's/^/no-mode /'
''')
commands = "svn svnadmin svndumpfilter svnfsfs svnlook svnmucc svnrdump svnserve svnsync svnversion".split()
check("all ten commands are 1.15", all(re.search(rf"^version {c} {c}, version 1\.15\.\d+ ", out, re.M) for c in commands), out)
check("ra_svn, ra_local and ra_serf (http, https)", all(s in out for s in
      ["* ra_svn", "* ra_local", "* ra_serf", "handles 'https' scheme"]), out)
check("svnmucc commits", "mucc-ok" in out, out)
check("an unknown option is reported (APR getopt's errfn)", re.search(r"^bad-option svnmucc: .*no-such-option", out, re.M)
      and "Segmentation fault" not in out, out)
check("a pre-commit hook (started without fork) rejects", "the hook says no" in out and "rejected rc=1" in out, out)
check("and accepts", "committed" in out and "pre-commit cwd=/tmp" in out, out)
check("an external diff (--diff-cmd)", "ext-diff +two" in out and "ext-diff --- f.txt" in out, out)
check("SVN_EDITOR writes the log message", "message from the editor" in out, out)
check("svnversion", "svnversion 3" in out, out)
check("branch, switch, merge", "merged: three" in out, out)
check("svn status -v", re.search(r"^status\s+\d+\s+\d+ root\s+f\.txt$", out, re.M), out)
check("svnadmin verify, dump, load", "verified" in out and "loaded 6" in out, out)
check("svndumpfilter", re.search(r"^filtered [1-9]", out, re.M), out)
check("svnlook tree", "tree  f.txt" in out, out)
check("svnfsfs stats", "fsfs-stats" in out, out)
check("svnserve (svn://)", "svn-ls f.txt" in out and "svn-commit 7" in out, out)
check("svnrdump over svn://", "rdump 8" in out, out)
check("svnsync over svn://", "synced 7" in out, out)
check("svn+ssh:// through a tunnel program", "tunnel b/" in out, out)
check("a tunnel that cannot start says why (errfn in the child)",
      "no-tunnel svn: E000002: Error in child process: exec of '/tmp/no-such-ssh' failed" in out, out)
check("svn+ssh:// runs dbclient without its warning about --",
      "dbclient ssh: Connection to root@nowhere.invalid:22 exited" in out and "unknown option" not in out, out)
check("svnserve without a mode", "no-mode You must specify exactly one of -d, -i, -t or -X." in out, out)

if os.environ.get("NETWORK") == "1":
    out = guest(r'''
cd /tmp
svn ls https://svn.apache.org/repos/asf/subversion/trunk/ | grep -x README
svn info http://svn.apache.org/repos/asf/subversion/trunk/README | grep "^Relative URL"
svn co -q https://svn.apache.org/repos/asf/subversion/trunk/doc doc && ls doc | grep -x README
''', network=True, timeout=600)
    check("https:// (serf, OpenSSL, the guest's CA bundle)", out.count("README") >= 2, out)
    check("http://", "Relative URL: ^/subversion/trunk/README" in out, out)

# In a terminal: no editor set, so `svn commit` opens vi (--with-editor=vi), as `git commit` does.
PROMPT = r"root@collabo:[^\n#]*#"
t = ptydrive.Session(argv + ["--no-network", "--arg", "collabo.quiet=1"], cols=100, rows=30, env={"TERM": "xterm-256color"})
seen = 0
def run(keys, until, timeout=30):
    global seen
    t.send(keys)
    rx, end = re.compile(until, re.S), time.time() + timeout
    while time.time() < end:
        text = plain(t.all.decode(errors="replace"))
        if m := rx.search(text, seen):
            out, seen = text[seen:m.end()], m.end()
            return out
        t.read(0.2)
    raise TimeoutError(f"no {until!r} in:\n{text[seen:][-3000:]}")
try:
    run("", PROMPT, 90)
    run("cd /tmp && svnadmin create r && svn co -q file:///tmp/r w && cd w && echo x > x && svn add -q x\r", PROMPT, 60)
    out = run("svn ci\r", r"will be ignored--.*?A\s+x")
    check("svn commit opens vi", "--This line, and those below, will be ignored--" in out, out)
    # Escape on its own: vi would take it and a ':' read with it for an escape sequence
    run("ia message typed in vi", r"I svn-commit\.tmp.*?a message typed in vi")
    run("\x1b", r"- svn-commit\.tmp")
    run(":wq\r", r"Committed revision 1\..*?" + PROMPT)
    out = run("svn log -l1 ^/\r", PROMPT)
    check("the message from vi is the log", "a message typed in vi" in out, out)
except TimeoutError as e:
    check("terminal session", False, e)
finally:
    t.close()

print(f"{'FAIL' if fails else 'PASS'}: svn ({len(fails)} failed)")
sys.exit(1 if fails else 0)
