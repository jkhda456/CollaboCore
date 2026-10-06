# gui — the guest's display

GUI programs in the sandbox, without a screen. Every window a program opens is a **canvas of its
own**: one whole frame of pixels the display server keeps complete at all times — no window
positions, no stacking, nothing covered, so there is never anything to move or uncover. Nothing
is shown anywhere unless someone asks: an agent (in the sandbox, or on the host through `exec`)
takes a screenshot of a canvas, previews it in the terminal, sends it mouse and keyboard input,
moves the focus, reads and sets the clipboard — all from the command line.

It is the groundwork for running a small web browser (a WPE WebKit port) under an agent's
control: the protocol carries what such a port needs (see [Toward WPE](#toward-wpe)).

```
 gui run gui-demo ─┐              ┌─ /usr/bin/gui (Rust, 1 process, 1 thread, poll loop)
 gui screenshot  ──┤  Unix socket │    canvases: demo:1 1024x768, demo:2 300x200, web:1 ...
 gui click / type ─┼──────────────┤    focus, clipboard, per-canvas pointer
 gui clipboard   ──┘              │    programs it started: process groups, logs, exit codes
                                  └─ apps: libcollabo-gui (C) — frames in, input out
```

| File in the guest | |
|---|---|
| `/usr/bin/gui` | the display server and its command line, one binary (Rust, `rust/`, 0.4 MB) |
| `/usr/bin/gui-demo` | a small GUI program: button, text field, drawing area (C, `client/gui-demo.c`) |
| `/usr/include/collabo_gui.h`, `/usr/lib/libcollabo-gui.a` | the client library programs link to (C, no dependencies) |

It is built in: part of the tools image (`tools.cpio`, `tools/build.sh`'s `gui` step, 0.5 MB of
its 14 MB), so every sandbox booted with the tools (the default; not with `tools: false`) has it.
Nothing runs until a GUI program or a `gui` command starts the server.

## Use

```sh
gui run --name demo --size 800x600 --wait -- gui-demo   # start; --wait: until it has drawn
gui list                                                # programs, canvases, sizes, focus (*)
gui screenshot demo -o demo.png                         # the whole canvas, PNG
gui screenshot demo --max-width 640 -o - > small.png    # scaled, to a pipe
gui view demo                                           # a 24-bit colour preview in this terminal
gui click demo 100 60                                   # move there, press, release
gui type demo 'Hello, 세계'                              # any language
gui key demo ctrl+a BackSpace Return                    # key combinations, in order
gui drag demo 50 200 300 260; gui scroll demo --at 300 200 3
gui click demo 100 60 --mods ctrl                       # ctrl+click (any mouse command: --mods)
gui keydown demo shift; gui click demo 300 80; gui keyup demo shift   # keys held across commands
gui release demo                                        # let go of every held key and button
gui type demo --ime '한글'                               # as a Korean input method: ㅎ 하 한, ㄱ 그 글
gui leave demo                                          # the pointer leaves: hover ends
gui clipboard get; echo hi | gui clipboard set
gui resize demo 1024x768 --wait                         # within the limits; --wait: until redrawn
gui close demo                                          # as its close button; gui kill demo to end it
gui logs demo                                           # what it printed
gui help                                                # everything
```

A target is a program's name (its first canvas), `NAME:N` (its canvas N) or `.` (the focused
canvas, else the only program). Input focuses its canvas first (`--no-focus` not to) and waits
until the program has handled it and drawn the result (`--no-sync` not to; a program that does
not answer within `--timeout` seconds, 5 by default, ends the command with status 3). Work a
program finishes later (a paste, a page that loads) is not waited for: `gui screenshot --idle 300`
waits until the canvas has not changed for 300 ms first, `gui wait NAME --idle 300` alone.

Exit status: 0 done, 1 failed, 2 usage, 3 timed out, 4 no such program or canvas, 5 not allowed
by the settings.

The server starts with the first command that needs it (or the first program that connects) and
runs until `gui shutdown`, which ends every program too. Its files are in `/tmp/.collabo-gui`
(`COLLABO_GUI_DIR` moves them): `socket`, `server.log`, and `logs/NAME.log` for each program `gui run`
started — its stdout and stderr, copied there by the server, which moves a log past 4 MiB to
`NAME.log.1` (a program that floods its output waits for the server instead of filling `/tmp`,
which is memory).

From the host, the same commands run through the app's `exec` (`sandbox.exec(['gui', ...])`);
`gui screenshot T -o -` writes the PNG to stdout. The Dart package wraps this as LLM tools,
`GuiTools` (`gui_run`, `gui_list`, `gui_screenshot` — an image block — `gui_input`,
`gui_clipboard`, `gui_window`), next to `SandboxTools`.

## Settings

The app decides what agents may do and how big a canvas can get: `start.config.gui` (Dart:
`CollaboConfig(gui: GuiSettings(...))`), `--gui-config KEY=VALUE` on the engine's command line,
`gui-config = KEY=VALUE` in launcher.conf. The engine writes them to `/etc/collabo/gui.json`;
`gui status` shows them.

| key | default | |
|---|---|---|
| `defaultSize` | `1024x768` | a canvas's size when its program does not ask for one |
| `maxSize` | `1920x1080` | no canvas grows beyond this (at most 8192x8192); requests and `gui resize` are held to it |
| `maxCanvases` | 8 | canvases one program may hold at once |
| `maxMemoryMB` | 256 | all canvases' pixels together |
| `capture` | true | may `gui screenshot` / `gui view` read canvases |
| `input` | true | may `gui click` / `key` / `type` / … send input |
| `clipboard` | true | may `gui clipboard` read and set the clipboard (programs share it regardless) |

These are the app's policy for agents working through the `gui` command; the guest is the agent's
own machine, so a program in it could still speak the protocol directly.

## Writing (or porting) a program

```c
#include <collabo_gui.h>

cg_display *d = cg_connect(NULL);                        // NULL: named after the program
uint32_t c = cg_canvas_create(d, 0, 0, "hello");         // 0, 0: the server's default size
uint32_t w, h;
cg_canvas_size(d, c, &w, &h);
uint32_t *px = malloc(w * h * 4);                        // XRGB8888: 0x00RRGGBB
/* draw */
cg_canvas_present(d, c, px, w * 4, w, h, NULL, 0);       // NULL, 0: the whole frame changed
cg_event ev;
while (cg_next_event(d, &ev, -1) > 0) {
	switch (ev.type) {
	case CG_EVENT_CONFIGURE: /* new size: reallocate, redraw at ev.width x ev.height */ break;
	case CG_EVENT_POINTER_BUTTON: /* ev.x, ev.y, ev.button, ev.pressed */ break;
	case CG_EVENT_KEY: /* ev.keysym (X11), ev.keycode (evdev), ev.modifiers, ev.text */ break;
	case CG_EVENT_CLOSE: return 0;
	}
}
```

Link with `-lcollabo-gui`. The header is the reference (`client/collabo_gui.h`), the wire format
is in [PROTOCOL.md](PROTOCOL.md). Damage rectangles (`cg_canvas_present(..., rects, n)`) send only
what changed. Programs with their own main loop poll `cg_fd()`, dispatch with
`cg_next_event(d, &ev, 0)` and call `cg_flush()` before they go idle.

Pixels go through the socket, copied: there is no shared memory between guest processes (each
WebAssembly program has its own linear memory), so neither `wl_shm` nor a DRM framebuffer could
work here. That is cheap enough: in the guest a whole 1920x1080 frame takes ~12 ms (86 frames/s,
~720 MB/s), 1280x720 ~4 ms; a 1920x1080 screenshot as PNG ~80 ms.

## Toward WPE

The aim is a WPE WebKit port that renders with Skia on the CPU and shows its views here. What its
platform layer (a `WPEDisplay`/`WPEView` implementation, or a libwpe backend) maps to:

| WPE | here |
|---|---|
| a view (`WPEView`), its size, `wpe_view_resized` | a canvas; `CG_EVENT_CONFIGURE` |
| a rendered buffer (SHM, BGRA) and its damage | `cg_canvas_present` with the damage rectangles; `CG_EVENT_FRAME_DONE` is the frame callback |
| pointer, button, axis events | `POINTER_MOTION` / `POINTER_BUTTON` / `SCROLL` (pixels and notches) |
| keyboard events (keysym, hardware keycode, modifiers) | `KEY` (X11 keysym, evdev code, X11 state) |
| input methods: preedit, commit, the caret rectangle | `CG_EVENT_PREEDIT`, `CG_EVENT_TEXT`; `cg_text_input` (enabled + caret) |
| pointer enter / leave (hover) | `CG_EVENT_POINTER_ENTER` / `CG_EVENT_POINTER_LEAVE` |
| focus in / out | `CG_EVENT_FOCUS` |
| clipboard (`WPEClipboard`) | `cg_clipboard_set`, `cg_clipboard_request` / `CG_EVENT_CLIPBOARD_DATA` |
| cursor | `cg_canvas_set_cursor` (CSS names) |
| new windows (`window.open`), popups | more canvases of the same program (`NAME:2`, …) |
| main loop | `cg_fd()` in a `GSource`, `cg_flush()` in its prepare |

## Tests

```sh
tools/gui/build.sh            # -> tools/stage/usr/...; tools/build.sh packs tools.cpio
tools/gui/tests/run.sh        # unit tests; the session natively; the session in the guest
tools/gui/tests/run.sh native # without a runtime
```

`tests/run.sh --all` runs the guest session too.

`tests/test_gui.py` is one scripted session (68 steps, every command and setting — modifier
clicks, held keys, hover, Hangul composition among them —, a stopped program, a program flooding
its output, garbage on the socket, a 38 MiB frame) run the same way natively and in one guest boot.
The Dart package's `gui tools` test drives `GuiTools` against a real sandbox.

## Licenses

`gui` and the client library: MIT (crates: `out/licenses/crates.txt`). gui-demo's font is
Terminus Font's ASCII glyphs, SIL Open Font License 1.1 (`client/OFL.txt`).
