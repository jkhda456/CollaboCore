"""busybox vi with UTF-8 text (Korean, combining characters, bytes that are not UTF-8), in a terminal.
What vi draws goes through a screen model (vtscreen) that places wide characters as xterm does, so a
character drawn in the wrong column or cut in half shows up as a wrong line.

    COLLABO_RUNTIME=dist/runtime/collabo-core-linux-x64 python3 tests/test_vi_utf8.py   # in the guest
    VI=/path/to/busybox-vi python3 tests/test_vi_utf8.py                               # a host build
"""
import json, os, re, sys, tempfile, time
T = os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0, T)
sys.path.insert(0, os.path.join(os.path.dirname(T), "tools", "tests"))
import ptydrive, vtscreen
from ptydrive import plain

ROWS, COLS = 10, 60    # wide enough for vi's undo messages (a longer one waits for Return)
fails = []
def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f"\n     {str(detail)[-1500:]}"))
    if not ok: fails.append(name)

LONG = "가나다라마바사아자차카타파하" * 3     # 84 columns, wider than the screen
TEXT = ("가나다 abc\n"
        "한글 테스트\n"
        "e\u0301x \udcff end\n"       # e + combining acute; byte ff, which is not UTF-8
        "\t탭가\n"
        + LONG + "\n"
        "a" + LONG + "\n"                # a wide character across the right edge
        + LONG + "abc\n").encode("utf-8", "surrogateescape")

GUEST = "VI" not in os.environ
QUIET = 1.0 if GUEST else 0.25
PROMPT = r"root@collabo:[^\n#]*#"

if GUEST:
    RT = os.environ["COLLABO_RUNTIME"]
    entry = json.load(open(os.path.join(RT, "manifest.json")))["entry"]
    argv = [os.path.join(RT, entry[0])] + [os.path.join(RT, a) if a.startswith("app/") else a for a in entry[1:]]
    t = ptydrive.Session(argv + ["--no-network", "--arg", "collabo.quiet=1"], cols=COLS, rows=ROWS, env={"TERM": "xterm"})
    path = "/tmp/k.txt"
else:
    fd, path = tempfile.mkstemp(suffix=".txt"); os.close(fd)
    open(path, "wb").write(TEXT)
    t = None
scr = vtscreen.Screen(ROWS, COLS)

def settle(timeout=20):
    """Read until the output has been quiet for a while; feed it to the screen model."""
    end, last = time.time() + timeout, time.time()
    while time.time() < end:
        n = len(t.all)
        t.read(0.1)
        if len(t.all) > n:
            scr.feed(t.all[n:]); last = time.time()
        elif time.time() - last > QUIET:
            return

def wait_for(rx, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        if re.search(rx, plain(t.all.decode(errors="replace"))): return
        t.read(0.2)
    raise TimeoutError(f"no {rx!r} in:\n{plain(t.all.decode(errors='replace'))[-2000:]}")

def keys(k):
    t.send(k.encode() if isinstance(k, str) else k)
    settle()

def screen_is(name, lines, cursor=None):
    got = [scr.line(y) for y in range(len(lines))]
    ok = got == lines and (cursor is None or scr.cursor == cursor)
    check(name, ok, f"lines {got}\n     want  {lines}\n     cursor {scr.cursor} want {cursor}")

def cursor_is(name, cursor):
    check(name, scr.cursor == cursor, f"cursor {scr.cursor} want {cursor}; line {scr.line(scr.cursor[0])!r}")

try:
    if GUEST:
        t.read(0.5); wait_for(PROMPT, 120)
        # the file, in pieces (the shell's line editor takes 1024 bytes)
        for n, i in enumerate(range(0, len(TEXT), 100)):
            esc = "".join(f"\\x{b:02x}" for b in TEXT[i:i + 100])
            t.all = b""
            t.send(f"printf '%b' '{esc}' {'>' if n == 0 else '>>'} {path}\r")
            wait_for(r"\n" + PROMPT, 30)
        t.send(f"vi {path}\r")
    else:
        t = ptydrive.Session([os.environ["VI"], path], cols=COLS, rows=ROWS, env={"TERM": "xterm"})
    time.sleep(1); settle(30)

    SHOWN = ["가나다 abc", "한글 테스트", "e\u0301x . end", "        탭가", LONG[:30], "a" + LONG[:29] + ">", LONG[:30]]
    screen_is("Korean, combining and non-UTF-8 bytes are drawn in their real widths", SHOWN, (0, 0))

    keys("lll"); cursor_is("l moves a character (2 columns) at a time", (0, 6))
    keys("hh"); cursor_is("h too", (0, 2))
    keys("0w"); cursor_is("w from a Korean word to the next word", (0, 7))
    keys("b"); cursor_is("b back to the start of the Korean word", (0, 0))
    keys("e"); cursor_is("e to the last character of the Korean word", (0, 4))
    keys("e"); cursor_is("e again goes on to the next word (does not stick)", (0, 9))
    keys("0E"); cursor_is("E stops on the last character too", (0, 4))
    keys("j"); cursor_is("j keeps the column", (1, 4))
    keys("k"); cursor_is("k back", (0, 4))
    keys("$"); cursor_is("$ to the last character", (0, 9))
    keys("2j0"); cursor_is("combining character line", (2, 0))
    keys("l"); cursor_is("e + combining accent is one character", (2, 1))
    keys("w"); cursor_is("w lands on the non-UTF-8 byte", (2, 3))
    keys("jl"); cursor_is("tab then a Korean character", (3, 8))
    keys("l"); cursor_is("next Korean character after the tab", (3, 10))

    # horizontal scrolling: the long line is 84 columns, the screen 60
    keys("j$")
    cursor_is("$ on a line wider than the screen: the cursor on the last character", (4, 58))
    check("the scrolled line shows whole characters, up to the last one", scr.line(4) == LONG[12:], scr.line(4))
    keys("jj$")
    cursor_is("$ on a line with ASCII at the end", (6, 59))
    check("wide characters cut by the left edge are drawn as <",
          [scr.line(y) for y in (4, 5, 6)] == ["<" + LONG[14:], LONG[13:], "<" + LONG[14:] + "abc"],
          [scr.line(y) for y in (4, 5, 6)])
    keys("0"); screen_is("back to column 0", SHOWN, (6, 0))

    # editing
    keys("gg0x"); screen_is("x deletes a whole Korean character", ["나다 abc"], (0, 0))
    keys("u"); screen_is("u restores it", ["가나다 abc"])
    keys("0r하"); screen_is("r with a Korean replacement", ["하나다 abc"], (0, 0))
    keys("f다"); cursor_is("f with a Korean character", (0, 4))
    keys("0t다"); cursor_is("t with a Korean character", (0, 2))
    keys("$F나"); cursor_is("F with a Korean character", (0, 2))
    keys("X"); screen_is("X deletes the Korean character before the cursor", ["나다 abc"], (0, 0))
    keys("i새로운\x7f\x1b"); screen_is("typing Korean, backspace removes a whole character", ["새로나다 abc"], (0, 2))
    keys("u"); screen_is("u puts back what backspace removed", ["새로운나다 abc"])
    keys("u"); screen_is("u again removes the typed text", ["나다 abc"])
    keys("0dw"); screen_is("dw deletes a Korean word", ["abc"], (0, 0))
    keys("u0de"); screen_is("de deletes to the end of a Korean word", [" abc"], (0, 0))
    keys("u0cw바꿈\x1b"); screen_is("cw changes a Korean word", ["바꿈 abc"], (0, 2))
    keys("0dl"); screen_is("dl deletes one Korean character", ["꿈 abc"], (0, 0))
    keys("$Ra가나\x1b"); screen_is("R replaces with Korean characters", ["꿈 aba가나"], (0, 8))
    keys("A끝\x1b"); screen_is("A appends a Korean character", ["꿈 aba가나끝"], (0, 10))
    keys("0R가\x7f\x1b"); screen_is("backspace in R mode puts the replaced character back", ["꿈 aba가나끝"])
    keys("0yl$p"); screen_is("yl / p of a Korean character", ["꿈 aba가나끝꿈"], (0, 12))
    keys("0s하\x1b"); screen_is("s substitutes a Korean character", ["하 aba가나끝꿈"], (0, 0))
    keys("/글\r"); cursor_is("/ search for Korean", (1, 2))
    keys(":s/테스트/시험/\r"); screen_is(":s with Korean", ["하 aba가나끝꿈", "한글 시험"])
    keys("dd"); screen_is("dd", ["하 aba가나끝꿈", "e\u0301x . end"])
    keys(":wq\r")

    want = TEXT.decode("utf-8", "surrogateescape").split("\n")
    want[0:2] = ["하 aba가나끝꿈"]
    want = "\n".join(want).encode("utf-8", "surrogateescape")
    if GUEST:
        wait_for(PROMPT + r"[^\n]*$", 30)
        t.all = b""
        t.send(f"od -An -tx1 -v {path} | tr -d ' \\n'; echo\r")
        wait_for(r"[0-9a-f]{40,}\s", 30)
        saved = bytes.fromhex(re.search(r"([0-9a-f]{40,})", plain(t.all.decode())).group(1))
    else:
        time.sleep(0.5); saved = open(path, "rb").read()
    check("the saved file has the edits, and the other bytes as they were", saved == want, f"{saved!r}\n     want {want!r}")
except TimeoutError as e:
    check("no step timed out", False, plain(str(e)))
finally:
    if t: t.close()
    if not GUEST: os.unlink(path)
print(f"{'FAILED ' + str(len(fails)) if fails else 'all passed'}")
sys.exit(1 if fails else 0)
