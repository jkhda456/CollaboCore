"""smbclient in the guest (tools.cpio, tools/build-samba.sh) against a Samba server on this machine:
every dialect from SMB1 (NT1) to SMB3.11, signing and encryption, NTLMSSP and guest logins, the
share list (DCE/RPC over a named pipe), a large file both ways, UTF-8 (Korean) names, recursion,
tar (-Tc/-Tx), the error paths; then in a terminal: the prompt, tab completion, Korean typed at
the prompt (libedit), `!` shell escapes, and no busy loop while it waits for a key.

The server is Ubuntu's Samba (smbd with samba-dcerpcd for the RPC services), unpacked from the
packages `apt-get download` fetches into tools/build/test-smbd (no install, no root: uid_wrapper
makes it believe it is root), on 127.0.0.1, which the guest reaches as 192.0.2.1
(--allow-loopback). Without apt-get the test says SKIP.

    COLLABO_RUNTIME=dist/runtime/collabo-core-linux-x64 python3 tools/tests/test_smbclient.py
    TOOLS_IMAGE=tools/out/tools.cpio ...   another tools image
"""
import hashlib, json, os, re, shutil, signal, socket, subprocess, sys, tempfile, time
S = os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0, S)
import ptydrive
from ptydrive import plain
ROOT = os.path.dirname(os.path.dirname(S))
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

# The server: Ubuntu's packages, unpacked (they need nothing installed beyond what a desktop has)
T = os.path.join(ROOT, "tools", "build", "test-smbd")
PKGS = ["samba", "samba-libs", "samba-common", "samba-common-bin", "libldb2", "libtalloc2", "libtdb1",
        "libtevent0t64", "libwbclient0", "libavahi-client3", "libavahi-common3", "libavahi-common-data",
        "libcups2t64", "liburing2", "libuid-wrapper"]
R = os.path.join(T, "root")
L = os.path.join(R, "usr/lib/x86_64-linux-gnu")
if not os.path.exists(os.path.join(R, "usr/sbin/smbd")):
    debs = os.path.join(T, "debs")
    os.makedirs(debs, exist_ok=True)
    if not shutil.which("apt-get") or subprocess.run(["apt-get", "download"] + PKGS, cwd=debs,
                                                    capture_output=True).returncode != 0:
        print("SKIP: smbclient (no Samba server here: `apt-get download samba` did not work)")
        sys.exit(0)
    for d in sorted(os.listdir(debs)):
        if d.endswith(".deb"):
            subprocess.run(["dpkg-deb", "-x", os.path.join(debs, d), R], check=True)

W = tempfile.mkdtemp(prefix="run-", dir=T)
share = os.path.join(W, "share")
for d in ("private", "lock", "state", "cache", "run", "ncalrpc", "log", "share"):
    os.makedirs(os.path.join(W, d), mode=0o700 if d == "private" else 0o755)
    os.chmod(os.path.join(W, d), 0o700 if d == "private" else 0o755)
os.chmod(W, 0o755)
with socket.socket() as s:
    s.bind(("127.0.0.1", 0)); PORT = s.getsockname()[1]
open(os.path.join(share, "hello.txt"), "w").write("hello from smbd\n")
os.makedirs(os.path.join(share, "dir/sub"))
for name in ("dir/a.txt", "dir/sub/b.txt"):
    open(os.path.join(share, name), "w").write(name + "\n")
open(os.path.join(share, "서버의 파일.txt"), "w").write("한글 내용\n")
big = os.urandom(1 << 20) * 32   # 32 MiB
open(os.path.join(share, "big.bin"), "wb").write(big)
BIG_SHA = hashlib.sha256(big).hexdigest()
conf = os.path.join(W, "smb.conf")
open(conf, "w").write(f"""[global]
  log level = {os.environ.get("SERVER_LOG_LEVEL", "1")}
  workgroup = TESTGRP
  netbios name = TESTSRV
  server role = standalone server
  smb ports = {PORT}
  interfaces = 127.0.0.1
  bind interfaces only = yes
  private dir = {W}/private
  lock directory = {W}/lock
  state directory = {W}/state
  cache directory = {W}/cache
  pid directory = {W}/run
  ncalrpc dir = {W}/ncalrpc
  log file = {W}/log/log.%m
  passdb backend = tdbsam:{W}/private/passdb.tdb
  server min protocol = NT1
  map to guest = Bad User
  disable netbios = yes
  load printers = no
  printcap name = /dev/null
  disable spoolss = yes
  rpc start on demand helpers = no
[share]
  path = {share}
  read only = no
  guest ok = yes
[enc]
  path = {share}
  read only = no
  smb encrypt = required
""")
env = {**os.environ, "LD_LIBRARY_PATH": f"{L}:{L}/samba", "UID_WRAPPER": "1", "UID_WRAPPER_ROOT": "1",
       "LD_PRELOAD": f"{L}/libuid_wrapper.so"}
user = os.environ.get("USER") or "work"
subprocess.run([f"{R}/usr/bin/pdbedit", "-s", conf, "-a", "-u", user, "-t"], input="secret1\nsecret1\n",
               env=env, capture_output=True, text=True, check=True)
helpers = [f"{R}/usr/libexec/samba/{h}" for h in ("rpcd_classic", "rpcd_epmapper", "rpcd_lsad", "rpcd_winreg")]
# stdin from /dev/null: smbd started on a socket takes itself for inetd's child and serves that one.
# samba-dcerpcd first, and smbd once it serves srvsvc. Now and then it stops halfway through
# starting (its log ends at "Starting process"; the share list then comes back empty): it is
# started again.
servers = []
def start(argv, log):
    p = subprocess.Popen(argv, env=env, stdin=subprocess.DEVNULL, stdout=open(os.path.join(W, log), "a"),
                         stderr=subprocess.STDOUT, start_new_session=True)
    servers.append(p)
    return p
def stop():
    for p in servers:
        try: os.killpg(p.pid, signal.SIGTERM)
        except ProcessLookupError: pass
srvsvc = os.path.join(W, "ncalrpc", "np", "srvsvc")
for attempt in range(1, 4):
    dcerpcd = start([f"{R}/usr/libexec/samba/samba-dcerpcd", "-F", "--no-process-group", "-s", conf] + helpers, "dcerpcd.out")
    for _ in range(100):
        if os.path.exists(srvsvc): break
        time.sleep(0.1)
    if os.path.exists(srvsvc): break
    os.killpg(dcerpcd.pid, signal.SIGKILL); dcerpcd.wait(); servers.remove(dcerpcd)
if attempt > 1:
    print(f"     (samba-dcerpcd started at attempt {attempt})")
start([f"{R}/usr/sbin/smbd", "-F", "--no-process-group", "-s", conf, "--debug-stdout", "-d1"], "smbd.out")
errors = []
for _ in range(100):
    try:
        socket.create_connection(("127.0.0.1", PORT), 1).close(); break
    except OSError as e:
        errors.append(repr(e)); time.sleep(0.1)
else:
    stop()
    print(f"FAIL the test server (smbd) did not start: {sorted(set(errors))}\n" + open(os.path.join(W, "smbd.out")).read()[-3000:])
    shutil.rmtree(W, ignore_errors=True)
    sys.exit(1)

def guest(script, timeout=600):
    """Output of a shell script run in the guest, which reaches this machine's loopback."""
    p = subprocess.run([argv[0], "exec"] + argv[1:] + ["--allow-loopback", "--", "sh", "-c", script],
                       capture_output=True, text=True, timeout=timeout)
    return p.stdout.replace("\r", "")

SRV = "//192.0.2.1"
O = f"-p {PORT} -U {user}%secret1"
try:
    out = guest(rf'''
cd /tmp
smbclient --version
for p in NT1 SMB2_02 SMB2_10 SMB3_00 SMB3_02 SMB3_11; do
  smbclient {SRV}/share {O} -m $p -d10 -c "get hello.txt /tmp/h.$p" > /tmp/log.$p 2>&1
  rc=$?
  echo "dialect $p rc=$rc $(grep -o 'negotiated dialect\[[A-Z0-9_]*\]' /tmp/log.$p) $(cat /tmp/h.$p)"
  [ $rc = 0 ] || tail -3 /tmp/log.$p | sed "s/^/  $p: /"
done
smbclient -L {SRV} {O} 2>&1 | awk '$2 == "Disk" || $2 == "IPC" {{ print "list", $1, $2 }}'
smbclient {SRV}/enc {O} -c "get hello.txt /tmp/h.enc" > /dev/null 2>&1 && echo "enc share: $(cat /tmp/h.enc)"
smbclient {SRV}/enc {O} -m SMB2_10 -c ls 2>&1 | grep -o "NT_STATUS_[A-Z_]*" | head -1 | sed 's/^/enc share over SMB2.1: /'
smbclient {SRV}/share {O} -m SMB2_10 --client-protection=sign -c "get hello.txt /tmp/h.sig" > /dev/null 2>&1 && echo "signed SMB2.1: $(cat /tmp/h.sig)"
smbclient {SRV}/share {O} -m SMB3 --client-protection=encrypt -c "get hello.txt /tmp/h.ce" > /dev/null 2>&1 && echo "client encrypt: $(cat /tmp/h.ce)"
smbclient {SRV}/share {O} -m NT1 --client-protection=encrypt -c "get hello.txt /tmp/h.c1" > /dev/null 2>&1 && echo "client encrypt SMB1: $(cat /tmp/h.c1)"
smbclient {SRV}/share -p {PORT} -N -c "get hello.txt /tmp/h.g" > /dev/null 2>&1 && echo "guest: $(cat /tmp/h.g)"
smbclient {SRV}/enc -p {PORT} -U {user}%wrong -c ls 2>&1 | grep -o "NT_STATUS_[A-Z_]*" | head -1 | sed 's/^/bad password: /'
printf 'username = {user}\npassword = secret1\n' > /tmp/auth && smbclient {SRV}/share -p {PORT} -A /tmp/auth -c "get hello.txt /tmp/h.a" > /dev/null 2>&1 && echo "authentication file: $(cat /tmp/h.a)"
smbclient {SRV}/share -p 1 -U {user}%secret1 -c ls 2>&1 | grep -o "NT_STATUS_[A-Z_]*" | head -1 | sed 's/^/closed port: /'
smbclient {SRV}/nosuchshare {O} -c ls 2>&1 | grep -o "NT_STATUS_[A-Z_]*" | head -1 | sed 's/^/no such share: /'
smbclient {SRV}/share {O} --use-kerberos=required -c ls > /tmp/krb 2>&1; echo "kerberos without a KDC: rc=$? $(grep -c -i -E 'signal|trap|unreachable|segmentation' /tmp/krb) crashes"
echo '192.0.2.1 smbhost' >> /etc/hosts
smbclient //smbhost/share {O} -c "get hello.txt /tmp/h.n" > /tmp/log.n 2>&1 && echo "by name: $(cat /tmp/h.n) $(grep -c -i 'mutex\|mkdir failed' /tmp/log.n) complaints"
start=$(date +%s)
smbclient {SRV}/share {O} -c "get big.bin /tmp/big.bin" > /dev/null 2>&1
echo "big get: $(sha256sum < /tmp/big.bin | cut -d' ' -f1) $(( $(date +%s) - start ))s"
start=$(date +%s)
smbclient {SRV}/share {O} -m SMB3 --client-protection=encrypt -c "put /tmp/big.bin big-up.bin" > /dev/null 2>&1
echo "big put encrypted: rc=$? $(( $(date +%s) - start ))s"
printf '한글 내용 2\n' > '/tmp/올린 파일.txt'
smbclient {SRV}/share {O} -c "put \"/tmp/올린 파일.txt\" \"올린 파일.txt\"; get \"서버의 파일.txt\" /tmp/k.txt; ls" 2>&1 | grep -E '파일' | sed 's/^ *\([^ ]* [^ ]*\) .*/korean ls: \1/'
echo "korean get: $(cat /tmp/k.txt)"
mkdir -p /tmp/m && cd /tmp/m
smbclient {SRV}/share {O} -c "prompt off; recurse on; mget dir" > /dev/null 2>&1; echo "mget: $(find dir -type f | sort | tr '\n' ' ')"
smbclient {SRV}/share {O} -c "mkdir new; cd new; put /etc/hostname h; rename h h2; ls" 2>&1 | grep -o ' h2 ' | sed 's/^/mkdir put rename: /'
smbclient {SRV}/share {O} -c "cd new; del h2; cd ..; rmdir new" > /dev/null 2>&1 && echo "del rmdir: ok"
smbclient {SRV}/share {O} -Tc /tmp/b.tar dir > /dev/null 2>&1; echo "tar c: $(tar -tf /tmp/b.tar | sort | tr '\n' ' ')"
mkdir -p /tmp/t/fromtar && echo restored > /tmp/t/fromtar/r.txt && (cd /tmp/t && tar -cf /tmp/r.tar fromtar)
smbclient {SRV}/share {O} -Tx /tmp/r.tar > /dev/null 2>&1; echo "tar x: rc=$?"
''')
    guest_out = out
    check("smbclient --version", "Version 4.25.1" in out, out)
    for p in ("NT1", "SMB2_02", "SMB2_10", "SMB3_00", "SMB3_02", "SMB3_11"):
        check(f"dialect {p}", f"dialect {p} rc=0 negotiated dialect[{p}] hello from smbd" in out, out)
    check("share list (srvsvc over \\pipe\\srvsvc)", all(f"list {s}" in out for s in ("share Disk", "enc Disk", "IPC$ IPC")), out)
    check("a share that requires encryption (SMB3)", "enc share: hello from smbd" in out, out)
    check("... refused over SMB2.1", "enc share over SMB2.1: NT_STATUS_" in out, out)
    check("signing asked by the client (SMB2.1)", "signed SMB2.1: hello from smbd" in out, out)
    check("encryption asked by the client (SMB3)", "client encrypt: hello from smbd" in out, out)
    check("encryption asked by the client (SMB1, Unix extensions)", "client encrypt SMB1: hello from smbd" in out, out)
    check("guest login", "guest: hello from smbd" in out, out)
    check("a wrong password", "bad password: NT_STATUS_LOGON_FAILURE" in out, out)
    check("an authentication file (-A)", "authentication file: hello from smbd" in out, out)
    check("a closed port", "closed port: NT_STATUS_CONNECTION_REFUSED" in out, out)
    check("a share that is not there", "no such share: NT_STATUS_BAD_NETWORK_NAME" in out, out)
    m = re.search(r"kerberos without a KDC: rc=(\d+) (\d+) crashes", out)
    check("Kerberos required, no KDC: fails, does not crash", m and 0 < int(m.group(1)) < 128 and m.group(2) == "0", out)
    check("a server by name (/etc/hosts), gencache without complaints", "by name: hello from smbd 0 complaints" in out, out)
    m = re.search(r"big get: (\w+) (\d+)s", out)
    check("32 MiB from the server", m and m.group(1) == BIG_SHA, out)
    if m: print(f"     (32 MiB in {m.group(2)} s, whole seconds)")
    up = os.path.join(share, "big-up.bin")
    check("32 MiB to the server, encrypted", os.path.exists(up) and hashlib.sha256(open(up, "rb").read()).hexdigest() == BIG_SHA, out)
    check("Korean names: the server's file listed, ours put", "korean ls: 서버의 파일.txt" in out and "korean ls: 올린 파일.txt" in out, out)
    check("... and the server has it under that name", open(os.path.join(share, "올린 파일.txt")).read() == "한글 내용 2\n", os.listdir(share))
    check("... and its content got", "korean get: 한글 내용" in out, out)
    check("recursive mget", "mget: dir/a.txt dir/sub/b.txt" in out, out)
    check("mkdir, put, rename", "mkdir put rename:  h2 " in out, out)
    check("del, rmdir", "del rmdir: ok" in out and not os.path.exists(os.path.join(share, "new")), out)
    m = re.search(r"tar c: (.*)", out)
    check("tar -Tc (libarchive)", m and "./dir/a.txt" in m.group(1).split() and "./dir/sub/b.txt" in m.group(1).split(), out)
    check("tar -Tx", "tar x: rc=0" in out and open(os.path.join(share, "fromtar/r.txt")).read() == "restored\n", out)

    # In a terminal: libedit's line editing, completion, UTF-8 typed in, a shell escape, idle CPU
    PROMPT = r"root@collabo:[^\n#]*#"
    t = ptydrive.Session(argv + ["--allow-loopback", "--arg", "collabo.quiet=1"], cols=100, rows=30,
                         env={"TERM": "xterm-256color"})
    seen = 0
    def run(keys, until, timeout=30):
        global seen
        t.send(keys)
        rx, end = re.compile(until, re.S), time.time() + timeout
        while time.time() < end:
            # libedit rings the bell after completing even a single match
            text = plain(t.all.decode(errors="replace")).replace("\x07", "")
            if m := rx.search(text, seen):
                out, seen = text[seen:m.end()], m.end()
                return out
            t.read(0.2)
        raise TimeoutError(f"no {until!r} in:\n{text[seen:][-3000:]}")
    SMB = r"smb: \\[^\n>]*> "
    def cpu():   # the engine's CPU seconds so far (the guest's threads are its threads)
        f = open(f"/proc/{t.pid}/stat").read().rsplit(")", 1)[1].split()
        return (int(f[11]) + int(f[12])) / os.sysconf("SC_CLK_TCK")
    try:
        run("", PROMPT, 90)
        run(f"smbclient {SRV}/share {O}\r", SMB, 60)
        out = run("ls\r", SMB)
        check("terminal: the prompt and ls", "hello.txt" in out, out)
        out = run("get hel\t", r"get hello\.txt ?")
        check("terminal: tab completion of a remote name", True)
        run(" /tmp/tty.txt\r", SMB)
        before = cpu(); time.sleep(4); idle = cpu() - before
        check("terminal: waiting for a key takes no CPU", idle < 1.0, f"{idle:.2f} s of CPU in 4 s")
        run("mkdir 터미널에서\r", SMB)
        out = run("ls\r", SMB)
        check("terminal: Korean typed at the prompt", "터미널에서" in out and os.path.isdir(os.path.join(share, "터미널에서")), out)
        out = run("!cat /tmp/tty.txt\r", SMB)
        check("terminal: ! runs a shell command", "hello from smbd" in out, out)
        out = run("\x1b[A\r", SMB)
        check("terminal: history (up arrow)", out.count("hello from smbd") >= 1, out)
        run("exit\r", PROMPT)
    except TimeoutError as e:
        check("terminal session", False, e)
    finally:
        t.close()
finally:
    stop()
    if fails and os.environ.get("KEEP_SERVER_LOGS"):
        shutil.copytree(W, os.environ["KEEP_SERVER_LOGS"], dirs_exist_ok=True, ignore=shutil.ignore_patterns("ncalrpc", "share", "msg.sock"))
        open(os.path.join(os.environ["KEEP_SERVER_LOGS"], "guest.out"), "w").write(globals().get("guest_out", ""))
    shutil.rmtree(W, ignore_errors=True)

print(f"{'FAIL' if fails else 'PASS'}: smbclient ({len(fails)} failed)")
sys.exit(1 if fails else 0)
