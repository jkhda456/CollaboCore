"""GNU screen in the guest, in a terminal: the engine in a pseudo-terminal (raw mode), as a person
uses it. Every step that forks in upstream screen is here: the backend (screen, screen -dm),
window shells (screen, C-a c), the backtick pipe and an `exec` filter.

    COLLABO_RUNTIME=dist/runtime/collabo-core-linux-x64 python3 nettools/tests/test_screen.py
"""
import fcntl, json, os, re, signal, struct, sys, termios, time
S = os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0, S)
import ptydrive
from ptydrive import plain
RT = os.environ["COLLABO_RUNTIME"]
fails = []
def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f"\n     {str(detail)[-1500:]}"))
    if not ok: fails.append(name)

PROMPT = r"root@collabo:[^\n#]*#"   # no space after it: a redraw moves the cursor instead
entry = json.load(open(os.path.join(RT, "manifest.json")))["entry"]
argv = [os.path.join(RT, entry[0])] + [os.path.join(RT, a) if a.startswith("app/") else a for a in entry[1:]]
t = ptydrive.Session(argv + ["--no-network", "--arg", "collabo.quiet=1"], cols=100, rows=30, env={"TERM": "xterm-256color"})
def run(cmd, until, timeout=30):
    """Type cmd, wait for until (a regex, matched without the escapes); the text up to it."""
    global seen
    t.send(cmd)
    rx, end = re.compile(until, re.S), time.time() + timeout
    while time.time() < end:
        text = plain(t.all.decode(errors="replace"))
        if m := rx.search(text, seen):
            out, seen = text[seen:m.end()], m.end()
            return out
        t.read(0.2)
    raise TimeoutError(f"no {until!r} in:\n{text[seen:][-3000:]}")
seen = 0    # how much of the session's text run() has gone past
try:
    run("", PROMPT, 90)
    out = run("tty; screen -v\r", r"Screen version \S+")
    check("the console has a name (tty), which screen's session name is made of", "/dev/console" in out, out)

    t0 = time.time()
    out = run("screen\r", PROMPT, 30)
    check("screen starts straight into a shell (/etc/screenrc: no startup message)", "Copyright" not in out, out)
    out = run("echo sty=$STY win=$WINDOW term=$TERM; tty\r", r"/dev/pts/\d+")
    check("window 0: STY, WINDOW and TERM set, on a pty", "win=0 term=screen" in out and ".console.collabo" in out, out)
    print(f"     (backend and first window up in {time.time() - t0:.1f}s)")

    run("\x01c", PROMPT)
    out = run("echo win=$WINDOW; sleep 30\r", r"win=1")
    time.sleep(1)
    t0 = time.time()
    run("\x03", PROMPT, 10)
    check("window 1: Ctrl-C stops its foreground job (the pty is its controlling terminal)", time.time() - t0 < 5, time.time() - t0)

    fcntl.ioctl(t.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    os.kill(t.pid, signal.SIGWINCH)
    time.sleep(2)
    out = run("stty size\r", r"\d+ \d+\s")
    check("a resized terminal reaches the window", "40 120" in out or "39 120" in out, out)

    out = run("\x01d", r"\[detached from [^\]]+\]", 20)
    run("", PROMPT, 10)
    out = run("screen -ls\r", PROMPT)
    check("detach: the session goes on without a terminal", "(Detached)" in out, out)
    out = run("screen -r\r", PROMPT, 30)
    out = run("echo back in $WINDOW\r", r"back in \d")
    check("screen -r: back in the window last used", "back in 1" in out, out)
    run("exit\r", r"sty=")    # window 1 ends; window 0 is redrawn
    time.sleep(1)
    out = run("exit\r", r"screen is terminating", 20)
    run("", PROMPT, 10)
    check("the last window's exit ends the session", "screen is terminating" in out, out)

    # backtick: a readpipe child; exec !..: a filter window's child, with stdout into the window
    run("printf 'backtick 1 0 0 echo pipe-ok\\nhardstatus alwayslastline \"%%1` %%n\"\\n' > /tmp/rc\r", PROMPT)
    out = run("screen -c /tmp/rc -S f\r", r"pipe-ok", 30)
    check("backtick output in the status line", "pipe-ok" in out, out)
    run("", PROMPT, 10)
    # (a filter that prints and exits at once can lose its output: upstream screen closes the
    # filter's pty when it sees the exit, without reading what is left. Hence the sleep.)
    out = run("\x01:exec !.. sh -c 'echo filter-$((6*7)); sleep 1'\r", r"filter-42", 20)
    check("exec !..: the filter's output goes into the window", "filter-42" in out, out)
    time.sleep(2)    # the filter has gone
    run("\x01\x1c", r"Really quit", 10)    # C-a C-\ (5.0 has no C-a \)
    run("y", r"screen is terminating", 20)

    out = run("screen -dmS bg sh -c 'echo started > /tmp/bg; sleep 300'; sleep 2; cat /tmp/bg; screen -ls\r", r"Sockets? in /root/\.screen\.")
    check("screen -dmS: a detached session from the start", "started" in out and ".bg\t(Detached)" in out.replace("  ", "\t"), out)
    out = run("screen -S bg -X quit; sleep 1; screen -ls\r", r"Sockets? (found )?in /root/\.screen\.")
    check("screen -X quit", "No Sockets found" in out, out)
except TimeoutError as e:
    check("no step timed out", False, plain(str(e)))
finally:
    t.close()
print(f"{'FAILED ' + str(len(fails)) if fails else 'all passed'}")
sys.exit(1 if fails else 0)
