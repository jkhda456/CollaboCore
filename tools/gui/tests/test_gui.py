"""The guest's display end to end: the server, the command line, the C client library and
gui-demo, as one scripted session.

    python3 tests/test_gui.py native          programs built for this machine (tools/build/gui/host)
    python3 tests/test_gui.py guest RUNTIME   in the guest (RUNTIME: dist/runtime/collabo-core-<platform>;
                                              GUI_TOOLS_IMAGE: another tools.cpio than the runtime's)

The steps run as one shell script (one boot in the guest); each step's output and exit status
land in files, and the checks read them, and the screenshots, afterwards."""
import os, shutil, struct, subprocess, sys, tempfile, zlib

A = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ROOT = os.path.dirname(os.path.dirname(A))
mode = sys.argv[1] if len(sys.argv) > 1 else "native"
work = tempfile.mkdtemp(prefix="gui-test-")
fails = []

# (name, shell command). $W is the shared folder, $G the display's runtime folder.
STEPS = [
    ("version", "gui --version"),
    ("noserver_list", "gui list"),
    ("status0", "gui status"),
    # In the guest the engine wrote --gui-config maxCanvases=6 to /etc/collabo/gui.json.
    ("engine_config", "cat /etc/collabo/gui.json 2>/dev/null; true"),
    ("run", "gui run --name demo --size 640x400 --wait -- gui-demo"),
    ("run_dup", "gui run --name demo -- gui-demo"),
    ("list", "gui list"),
    ("list_json", "gui list --json"),
    ("info", "gui info demo"),
    ("shot_initial", "gui screenshot demo -o $W/initial.png"),
    ("click", "gui click demo 100 60"),
    ("type", "gui type demo 'Hi 한글!'"),
    ("enter", "gui key demo Return"),
    ("type2", "gui type demo abc && gui type demo -- '-x' && gui key demo BackSpace BackSpace"),
    ("copy", "gui key demo ctrl+c"),
    ("clip_get", "gui clipboard get"),
    ("clip_types", "gui clipboard types"),
    ("clip_set", "printf 'from-cli' | gui clipboard set"),
    ("paste", "gui key demo ctrl+v"),
    ("commit", "gui type demo --commit '漢字'"),
    ("scroll", "gui scroll demo --at 300 200 3"),
    ("drag", "gui drag demo 60 200 400 260"),
    # Modifiers held around the mouse; keys held across commands; let go of everything.
    ("ctrl_click", "gui click demo 100 60 --mods ctrl"),
    ("shift_drag", "gui keydown demo shift && gui drag demo 60 300 90 300 --steps 2 && gui info demo && gui keyup demo shift && gui info demo"),
    ("held", "gui keydown demo ctrl && gui mousedown demo 70 300 && gui info demo && gui release demo && gui info demo"),
    # Hover: the pointer leaves, and enters again with the next move (over the button).
    ("leave", "gui leave demo && gui info demo && gui move demo 110 70"),
    ("shot_hover", "gui screenshot demo -o $W/hover.png"),
    ("pixels", "gui scroll demo --pixels 7 -2"),
    # A Korean input method: the composition, then the commit.
    ("ime", "gui key demo ctrl+l && gui type demo --ime '한글 ok' && gui key demo Return"),
    ("preedit_cmd", "gui preedit demo '가나' --cursor 3 && gui info demo && gui preedit demo"),
    ("preedit_bad", "gui preedit demo '가' --cursor 1"),
    ("shot_drawn", "gui screenshot demo -o $W/drawn.png"),
    ("shot_region", "gui screenshot demo --region 0,0,100,28 -o $W/region.png"),
    ("shot_scaled", "gui screenshot demo --scale 0.5 -o $W/scaled.png"),
    ("shot_raw", "gui screenshot demo --format raw -o $W/frame.raw"),
    ("shot_stdout", "gui screenshot demo -o - > $W/stdout.png"),
    ("view", "gui view demo --width 40"),
    ("outside", "gui click demo 5000 10"),
    ("resize", "gui resize demo 800x500 --wait"),
    ("resize_big", "gui resize demo 9000x9000 --wait"),
    ("shot_resized", "gui screenshot demo -o $W/resized.png"),
    ("resize_back", "gui resize demo 640x400 --wait"),
    ("run2", "gui run --name multi --size 320x200 --wait -- gui-demo --canvases 2 --title Multi"),
    ("list2", "gui list"),
    ("shot_panel", "gui screenshot multi:2 -o $W/panel.png"),
    ("focus_panel", "gui focus multi:2"),
    ("dot_target", "gui info ."),
    ("nocanvas", "gui screenshot multi:7"),
    ("unknown", "gui screenshot nosuch"),
    ("stop", "gui kill demo --signal STOP"),
    ("hung_click", "gui click demo 10 10 --timeout 1"),
    ("cont", "gui kill demo --signal CONT && gui wait demo --sync"),
    ("close", "gui close multi && gui wait multi --exit"),
    ("self", "COLLABO_GUI_NAME=selfie gui-demo > $W/self.log 2>&1 & "
             "for i in $(seq 100); do gui info selfie >/dev/null 2>&1 && break; sleep 0.2; done; gui wait selfie --timeout 20 && gui list"),
    ("self_kill", "gui kill selfie; sleep 0.5; gui list"),
    # Any program can be started; this one only floods its log, which is kept to a few MiB.
    ("spam", "gui run --name spam -- yes 0123456789abcdef && sleep 3 && gui kill spam && "
             "wc -c < $G/logs/spam.log && wc -c < $G/logs/spam.log.1 && head -1 $G/logs/spam.log && sed -n 2p $G/logs/spam.log"),
    ("kill", "gui kill demo && gui list"),
    ("logs", "gui logs demo"),
    ("logs_multi", "gui logs multi -n 3"),
    ("garbage", "if command -v python3 >/dev/null; then python3 $W/junk.py $G/socket; fi; gui status"),
    ("shutdown", "gui shutdown"),
    # Settings: the host app's limits and switches (as --gui-config writes them).
    ("limited", "printf '{\"maxSize\": \"300x200\", \"defaultSize\": \"200x100\", \"maxCanvases\": 1, \"capture\": false, \"input\": false, \"clipboard\": false}' > $W/gui.json; "
                "export COLLABO_GUI_CONFIG=$W/gui.json; gui run --name small --wait -- gui-demo --canvases 2 && gui list && gui status"),
    ("limited_big", "COLLABO_GUI_CONFIG=$W/gui.json gui resize small 5000x5000 --wait"),
    ("denied_shot", "gui screenshot small -o $W/denied.png"),
    ("denied_click", "gui click small 1 1"),
    ("denied_clip", "gui clipboard get"),
    ("limited_log", "gui logs small"),
    ("shutdown2", "gui shutdown && sleep 0.3 && test ! -e $G/socket"),
    # A frame bigger than one protocol message (32 MiB) still comes out whole.
    ("huge", "printf '{\"maxSize\": \"4096x4096\", \"maxMemoryMB\": 512}' > $W/huge.json; export COLLABO_GUI_CONFIG=$W/huge.json; "
             "gui run --name huge --size 4096x2400 --wait -- gui-demo && gui screenshot huge --format raw -o $W/huge.raw && gui screenshot huge --max-width 512 -o $W/huge.png; gui shutdown"),
]


JUNK = """import socket, sys
# A run of 0xff (a header naming a huge payload), a message before HELLO, a cut-off message.
for junk in (b"\\xff" * 64, b"\\x10\\x00\\x00\\x00\\x01\\x01\\x00\\x00" + b"x" * 16, b"\\x00\\x00\\x00\\x01\\x01\\x00\\x00\\x00"):
    s = socket.socket(socket.AF_UNIX)
    s.connect(sys.argv[1])
    s.sendall(junk)
    s.close()
"""


def script(wdir, gdir):
    lines = ["W=" + wdir, "G=" + gdir, "mkdir -p $W/out"]
    for name, cmd in STEPS:
        lines.append(f"( {cmd} ) > $W/out/{name}.out 2> $W/out/{name}.err; echo $? > $W/out/{name}.code")
    lines.append("cp $G/server.log $W/out/server.log 2>/dev/null; true")
    return "\n".join(lines) + "\n"


open(os.path.join(work, "junk.py"), "w").write(JUNK)
if mode == "native":
    B = os.path.join(ROOT, "tools", "build", "gui", "host")
    gdir = os.path.join(work, "display")
    env = dict(os.environ, COLLABO_GUI_DIR=gdir, PATH=f"{B}/debug:{B}:{os.environ['PATH']}")
    env.pop("COLLABO_GUI_SOCKET", None)
    env.pop("COLLABO_GUI_CONFIG", None)
    open(os.path.join(work, "t.sh"), "w").write(script(work, gdir))
    p = subprocess.run(["sh", os.path.join(work, "t.sh")], env=env, capture_output=True, text=True, timeout=300)
else:
    rt = sys.argv[2]
    open(os.path.join(work, "t.sh"), "w").write(script("/work", "/tmp/.collabo-gui"))
    argv = [f"{rt}/bin/collabo-core-engine", "exec", "--kernel", "app/images/vmlinux.wasm", "--initramfs", "app/images/initramfs.cpio",
            *(["--initramfs", "app/images/python.cpio"] if os.path.exists(f"{rt}/app/images/python.cpio") else []),
            "--tools-image", os.environ.get("GUI_TOOLS_IMAGE", "app/images/tools.cpio"), "--gui-config", "maxCanvases=6",
            "--mount", f"{work}:/work",
            "--", "/bin/sh", "/work/t.sh"]
    p = subprocess.run(argv, cwd=rt, capture_output=True, text=True, timeout=900)

out = {}
for name, _ in STEPS:
    def rd(ext):
        try:
            return open(os.path.join(work, "out", f"{name}.{ext}"), encoding="utf-8", errors="replace").read()
        except FileNotFoundError:
            return None
    code = rd("code")
    out[name] = (int(code) if code and code.strip().isdigit() else None, rd("out") or "", rd("err") or "")
server_log = open(os.path.join(work, "out", "server.log")).read() if os.path.exists(os.path.join(work, "out", "server.log")) else ""


def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f"\n     {str(detail)[-1500:]}"))
    if not ok:
        fails.append(name)


def png(path):
    """(width, height, rows of RGB tuples) of an 8-bit RGB PNG, filters undone."""
    data = open(path, "rb").read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n"
    i, idat, w, h = 8, b"", 0, 0
    while i < len(data):
        n = struct.unpack(">I", data[i:i + 4])[0]
        kind, body = data[i + 4:i + 8], data[i + 8:i + 8 + n]
        if zlib.crc32(kind + body) != struct.unpack(">I", data[i + 8 + n:i + 12 + n])[0]:
            raise ValueError("bad crc")
        if kind == b"IHDR":
            w, h, depth, color = struct.unpack(">IIBB", body[:10])
            assert (depth, color) == (8, 2)
        elif kind == b"IDAT":
            idat += body
        i += 12 + n
    raw = zlib.decompress(idat)
    stride, rows, prev = w * 3, [], bytearray(w * 3)
    for y in range(h):
        f, line = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        for x in range(stride):
            a = line[x - 3] if x >= 3 else 0
            b = prev[x]
            if f == 1: line[x] = (line[x] + a) & 255
            elif f == 2: line[x] = (line[x] + b) & 255
            elif f != 0: raise ValueError(f"filter {f}")
        rows.append([tuple(line[x * 3:x * 3 + 3]) for x in range(w)])
        prev = line
    return w, h, rows


def ok(name):
    return out[name][0] == 0


def text(name):
    return out[name][1]


def detail(name):
    return out[name]


demo_log = text("logs")
check("server starts on demand; version", ok("version") and "protocol 1" in text("version"), detail("version"))
check("list with no programs", ok("noserver_list") and "no programs" in text("noserver_list"), detail("noserver_list"))
check("status: limits and access", ok("status0") and "max 1920x1080" in text("status0") and "capture allowed" in text("status0"), detail("status0"))
if mode != "native":
    check("the engine's --gui-config reaches the display", '"maxCanvases": 6' in text("engine_config") and "6 canvases per program" in text("status0"),
          (detail("engine_config"), text("status0")))
check("run --wait", ok("run") and "demo is up" in text("run"), detail("run"))
check("a second demo is refused", out["run_dup"][0] == 1 and "already running" in out["run_dup"][2], detail("run_dup"))
check("list: canvas, size, focus", ok("list") and "demo:1" in text("list") and "640x400" in text("list") and "* GUI demo" in text("list"), detail("list"))
check("list --json", ok("list_json") and '"width":640' in text("list_json") and '"focus":"demo:1"' in text("list_json"), detail("list_json"))
check("info", ok("info") and '"canvas":{"id":"demo:1"' in text("info") and '"cursor":"default"' in text("info"), detail("info"))

try:
    w, h, rows = png(os.path.join(work, "initial.png"))
    check("screenshot: PNG of the whole canvas", (w, h) == (640, 400), (w, h))
    check("screenshot: title bar and background colours", rows[2][2] == (0x1f, 0x3a, 0x60) and rows[399][639] == (0xf0, 0xf0, 0xf0), (rows[2][2], rows[399][639]))
except Exception as e:  # noqa: BLE001
    check("screenshot: PNG of the whole canvas", False, (e, detail("shot_initial")))

check("click: the button was clicked", ok("click") and "clicked 1" in demo_log, (detail("click"), demo_log[-800:]))
check("type: US keys and Korean", ok("type") and 'enter "Hi 한글!"' in demo_log, demo_log[-1500:])
check("type: shift for '!' and 'H'", "key 0xffe1 code=42 down" in demo_log and 'key 0x48 code=35 down mods=0x1 text="H"' in demo_log, demo_log[:3000])
check("type: Korean as Unicode keysyms", 'key 0x100d55c code=0 down mods=0x0 text="한"' in demo_log, demo_log[:3000])
check("type -- TEXT: text that starts with a dash", ok("type2") and 'key 0x2d code=12 down mods=0x0 text="-"' in demo_log, (detail("type2"), demo_log[-2000:]))
check("key ctrl+c: control held, no text", 'key 0x63 code=46 down mods=0x4 text=""' in demo_log and 'copied "abc"' in demo_log, demo_log[-2000:])
check("clipboard get after the program copied", ok("clip_get") and text("clip_get") == "abc", detail("clip_get"))
check("clipboard types", ok("clip_types") and "text/plain;charset=utf-8\t3" in text("clip_types"), detail("clip_types"))
check("clipboard set, the program pastes", ok("clip_set") and ok("paste") and 'pasted found=1 type=text/plain;charset=utf-8 "from-cli"' in demo_log, demo_log[-2000:])
check("type --commit: one input-method commit", ok("commit") and 'text "漢字"' in demo_log, demo_log[-1500:])
check("scroll: steps and pixels at the pointer", ok("scroll") and "scroll dx=0 dy=120 steps=0,3 at 300,200" in demo_log, demo_log[-1500:])
check("drag: button held through the motion", ok("drag") and "button 1 down 60,200" in demo_log and "motion 400,260 buttons=0x100" in demo_log and "button 1 up 400,260" in demo_log, demo_log[-2500:])
check("ctrl+click: control held through the click", ok("ctrl_click") and "button 1 down 100,60 mods=0x4" in demo_log and "clicked 11" in demo_log, demo_log[-3000:])
sd = text("shift_drag")
check("keydown shift .. keyup: held across commands", ok("shift_drag") and "button 1 down 60,300 mods=0x1" in demo_log
      and '"modifiers":1' in sd.splitlines()[0] and '"modifiers":0' in sd.splitlines()[1], (detail("shift_drag"), demo_log[-3000:]))
hd = text("held").splitlines()
check("release: every held button and modifier up", ok("held") and '"buttons":256,"modifiers":4' in hd[0] and '"buttons":0,"modifiers":0' in hd[1]
      and "button 1 up 70,300 mods=0x4" in demo_log, (detail("held"), demo_log[-3000:]))
check("leave, and enter with the next move", ok("leave") and '"pointerInside":false' in text("leave") and "leave\nenter 110,70\n" in demo_log, (detail("leave"), demo_log[-3000:]))
try:
    w, h, rows = png(os.path.join(work, "hover.png"))
    check("hover: the button lights up under the pointer", rows[48][20] == (0x4a, 0x8a, 0xe0), rows[48][20])
except Exception as e:  # noqa: BLE001
    check("hover: the button lights up under the pointer", False, (e, detail("shot_hover")))
check("scroll --pixels: smooth, no notches", ok("pixels") and "scroll dx=-2 dy=7 steps=0,0" in demo_log, demo_log[-2000:])
ime = 'preedit "ㅎ" 3 3\npreedit "하" 3 3\ntext "한"\npreedit "ㄱ" 3 3\npreedit "그" 3 3\ntext "글"\nkey 0x20 code=57 down'
check("type --ime: Hangul composed, then committed", ok("ime") and ime in demo_log and 'enter "한글 ok"' in demo_log, demo_log[-3000:])
check("preedit: shown, in gui info, cleared", ok("preedit_cmd") and 'preedit "가나" 3 3' in demo_log and '"preedit":"가나"' in text("preedit_cmd")
      and 'preedit "" 0 0' in demo_log, (detail("preedit_cmd"), demo_log[-1500:]))
check("preedit: a cursor inside a character is refused", out["preedit_bad"][0] == 2 and "character boundary" in out["preedit_bad"][2], detail("preedit_bad"))
check("the program says where typing goes (text input caret)", '"textInput":{"caret":[' in text("preedit_cmd"), detail("preedit_cmd"))
try:
    w, h, rows = png(os.path.join(work, "drawn.png"))
    check("screenshot after drag: the line is drawn", rows[230][230] == (0x10, 0x10, 0x10) and rows[180][500] == (0xff, 0xff, 0xff), (rows[230][230], rows[180][500]))
except Exception as e:  # noqa: BLE001
    check("screenshot after drag: the line is drawn", False, e)
try:
    check("screenshot --region", png(os.path.join(work, "region.png"))[:2] == (100, 28) and "region of 640x400" in text("shot_region"), detail("shot_region"))
    check("screenshot --scale 0.5", png(os.path.join(work, "scaled.png"))[:2] == (320, 200), detail("shot_scaled"))
    raw = open(os.path.join(work, "frame.raw"), "rb").read()
    check("screenshot --format raw (BGRX)", len(raw) == 640 * 400 * 4 and raw[(2 * 640 + 2) * 4:(2 * 640 + 2) * 4 + 3] == bytes([0x60, 0x3a, 0x1f]), len(raw))
    check("screenshot -o - (to a pipe)", png(os.path.join(work, "stdout.png"))[:2] == (640, 400), detail("shot_stdout"))
except Exception as e:  # noqa: BLE001
    check("screenshot variants", False, e)
check("view: a terminal preview", ok("view") and "\x1b[38;2;" in text("view") and "▀" in text("view") and "demo:1 640x400" in text("view"), detail("view")[1][-200:])
check("input outside the canvas is refused", out["outside"][0] == 2 and "outside the 640x400 canvas" in out["outside"][2], detail("outside"))
check("resize --wait: the program redraws at the size", ok("resize") and text("resize").strip() == "800x500" and "configure 1 800x500" in demo_log, (detail("resize"), demo_log[-800:]))
check("resize past the limit is held to the maximum", ok("resize_big") and text("resize_big").strip() == "1920x1080" and "outside the limits" in out["resize_big"][2], detail("resize_big"))
try:
    check("screenshot after resize", png(os.path.join(work, "resized.png"))[:2] == (1920, 1080), detail("shot_resized"))
except Exception as e:  # noqa: BLE001
    check("screenshot after resize", False, (e, detail("shot_resized")))
check("several canvases of one program", ok("run2") and "multi:1" in text("list2") and "multi:2" in text("list2") and "320x200" in text("list2"), detail("list2"))
try:
    w, h, rows = png(os.path.join(work, "panel.png"))
    check("the second canvas is its own whole frame", (w, h) == (320, 200) and rows[100][300] == (0xc0, 0x40, 0x40), (w, h, rows[100][300]))
except Exception as e:  # noqa: BLE001
    check("the second canvas is its own whole frame", False, (e, detail("shot_panel")))
multi_log = text("logs_multi")
check("focus moves between canvases", ok("focus_panel") and '"focused":true' in text("dot_target") and '"id":"multi:2"' in text("dot_target"), detail("dot_target"))
check("no such canvas: exit 4", out["nocanvas"][0] == 4 and "has no canvas 7" in out["nocanvas"][2], detail("nocanvas"))
check("no such program: exit 4", out["unknown"][0] == 4 and "no program named nosuch" in out["unknown"][2], detail("unknown"))
check("a stopped program: input times out (exit 3)", ok("stop") and out["hung_click"][0] == 3 and "has not handled the input" in out["hung_click"][2], detail("hung_click"))
check("it catches up once continued", ok("cont"), detail("cont"))
check("close: the program ends by itself", ok("close") and text("close").strip() == "exited(0)", detail("close"))
check("close: it saw the request", "close requested 1" in multi_log, multi_log)
check("a program started outside gui run", ok("self") and "selfie:1" in text("self"), (detail("self"), open(os.path.join(work, "self.log")).read() if os.path.exists(os.path.join(work, "self.log")) else ""))
check("kill of a program that connected by itself", ok("self_kill") and "selfie" in text("self_kill") and "selfie:1" not in text("self_kill"), detail("self_kill"))
spam = text("spam").splitlines()[2:]  # after "spam started" and "killed(15)"
check("a flooding program's output is kept to two logs of 4 MiB", ok("spam") and len(spam) >= 4 and int(spam[0]) <= 4 << 20 and int(spam[1]) <= (4 << 20) + 65536
      and "the earlier output is in /" in spam[2] and spam[3] == "0123456789abcdef", detail("spam"))
check("kill: TERM, reported", ok("kill") and "killed(15)" in text("kill"), detail("kill"))
check("garbage on the socket does not hurt the server", ok("garbage") and "server: pid" in text("garbage"), detail("garbage"))
check("shutdown", ok("shutdown"), detail("shutdown"))
check("settings: default and maximum size", ok("limited") and "200x100" in text("limited") and "max 300x200" in text("limited") and "capture denied" in text("limited"), detail("limited"))
check("settings: resize held to maxSize", ok("limited_big") and text("limited_big").strip() == "300x200", detail("limited_big"))
check("settings: capture=false refuses screenshots (exit 5)", out["denied_shot"][0] == 5 and "capture=false" in out["denied_shot"][2], detail("denied_shot"))
check("settings: input=false refuses input (exit 5)", out["denied_click"][0] == 5, detail("denied_click"))
check("settings: clipboard=false (exit 5)", out["denied_clip"][0] == 5, detail("denied_clip"))
check("shutdown removes the socket", ok("shutdown2"), detail("shutdown2"))
check("settings: maxCanvases", "at most 1 canvases per program" in text("limited_log") and "small:2" not in text("limited"), (detail("limited"), text("limited_log")))
try:
    check("a 4096x2400 frame (38 MiB) in one screenshot", ok("huge") and os.path.getsize(os.path.join(work, "huge.raw")) == 4096 * 2400 * 4
          and png(os.path.join(work, "huge.png"))[:2] == (512, 300), detail("huge"))
except Exception as e:  # noqa: BLE001
    check("a 4096x2400 frame (38 MiB) in one screenshot", False, (e, detail("huge")))
check("server log (and a session of its own)", "setsid:" not in server_log and "listening on" in server_log and "started demo" in server_log and "stopped" in server_log, server_log[-1500:])

if fails:
    print(f"\n{len(fails)} failed; work folder kept: {work}")
    if mode != "native":
        print(p.stdout[-3000:], p.stderr[-2000:])
    sys.exit(1)
shutil.rmtree(work, ignore_errors=True)
print(f"all {len(STEPS)} steps, every check passed ({mode})")
