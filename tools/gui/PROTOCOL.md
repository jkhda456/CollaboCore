# The gui display protocol (version 1)

One Unix stream socket, `$COLLABO_GUI_SOCKET`, else `$COLLABO_GUI_DIR/socket`, else
`/tmp/.collabo-gui/socket`. Three kinds of peer: the **server** (`gui server`), **apps** (programs
that draw: `client/collabo_gui.c` is the reference client) and **control** clients (the `gui`
command line).

## Framing

Every message is an 8-byte header and a payload:

| bytes | field |
|---|---|
| 0–3 | payload length, u32 |
| 4–5 | opcode, u16 |
| 6–7 | reserved, 0 |

All integers are little-endian. In the tables below `u32`/`i32` are 4 bytes; `str` and `bytes`
are a u32 length followed by that many bytes (strings are UTF-8, not NUL-terminated). The server
accepts payloads up to 32 MiB; a peer that sends a longer one, or a malformed message, is
disconnected. Control clients accept results up to one whole frame of the largest canvas
(8192 × 8192 × 4 bytes).

The server never blocks on a peer: it queues output, and disconnects a peer whose unread output
passes 64 MiB plus one frame of the largest canvas. A peer may block in its own writes.

## Handshake

The client's first message is `HELLO`; the server answers `WELCOME` (or `ERROR` and closes).

| op | name | payload |
|---|---|---|
| 0x0001 | HELLO | u32 version (1), u32 role (0 app, 1 control), u32 pid, str name, str token |
| 0x0002 | WELCOME | u32 version, u32 client id, str app name, u32 default width, u32 default height, u32 max width, u32 max height, u32 max canvases |
| 0x0003 | ERROR | u32 code, str message |

An app's `token` is `$COLLABO_GUI_APP`, which `gui run` gives the program it starts: the server
then knows the connection as that program (its name, its pid, its log). Without a token the app
is a new entry named after `name` (letters, digits and `. _ -`, starting with a letter; `-2`, `-3`…
when the name is taken). `WELCOME`'s app name is the name the `gui` commands use.

Error codes: 1 failed, 2 usage, 3 timed out, 4 not found, 5 not allowed (settings), 6 protocol,
7 over a limit.

## Canvases

A canvas is a window: one whole frame of pixels, kept by the server and complete at all times —
no position, no stacking, never covered. Its size is the server's to decide (`CONFIGURE`), within
the limits; the app draws at that size.

Pixels are **XRGB8888**: 4 bytes per pixel in the order B, G, R, X (a little-endian
`0x00RRGGBB`); the X byte is ignored. Rows are packed (`width * 4` bytes).

App → server:

| op | name | payload |
|---|---|---|
| 0x0101 | CANVAS_CREATE | u32 canvas, u32 width, u32 height, str title |
| 0x0102 | CANVAS_DESTROY | u32 canvas |
| 0x0103 | CANVAS_TITLE | u32 canvas, str title |
| 0x0104 | CANVAS_UPDATE | u32 canvas, u32 x, u32 y, u32 w, u32 h, bytes pixels (w·h·4) |
| 0x0105 | CANVAS_COMMIT | u32 canvas, u32 serial, u32 width, u32 height |
| 0x0106 | CANVAS_REQUEST_SIZE | u32 canvas, u32 width, u32 height |
| 0x0107 | CANVAS_CURSOR | u32 canvas, str cursor name (CSS names: `default`, `pointer`, `text`…) |
| 0x0108 | FOCUS_REQUEST | u32 canvas |
| 0x010c | TEXT_INPUT | u32 canvas, u32 enabled, i32 x, i32 y, u32 w, u32 h |

`canvas` is the app's own number for it (any u32, unique per connection). `CANVAS_CREATE`'s size
is a wish (0 = the default); the server answers with `CONFIGURE`. A program started with
`gui run --size` gets that size whatever it asks.

A frame is any number of `CANVAS_UPDATE`s followed by `CANVAS_COMMIT`. Updates are held until the
commit and then applied together, so the canvas never shows half a frame. The commit's size is the
size of the frame (normally the configured one); a commit of a new size resizes the canvas,
keeping the overlapping pixels. The server answers each commit with `FRAME_DONE` once it is on
the canvas.

Server → app:

| op | name | payload |
|---|---|---|
| 0x0201 | CONFIGURE | u32 canvas, u32 width, u32 height |
| 0x0202 | FRAME_DONE | u32 canvas, u32 serial |
| 0x0203 | CLOSE | u32 canvas |
| 0x0204 | FOCUS | u32 canvas, u32 focused (0/1) |

`CLOSE` asks the program to close the window, as a close button would (`gui close`); it may
refuse. Focus: one canvas at most has the keyboard focus; a new canvas gets it when nothing has it,
input sent with the `gui` command moves it to its target (unless `--no-focus`), and
`FOCUS_REQUEST` takes it.

## Input

Server → app; each names the canvas it is for.

| op | name | payload |
|---|---|---|
| 0x0205 | POINTER_MOTION | u32 canvas, i32 x, i32 y, u32 modifiers, u32 buttons |
| 0x0206 | POINTER_BUTTON | u32 canvas, i32 x, i32 y, u32 button, u32 pressed, u32 modifiers, u32 buttons |
| 0x0207 | SCROLL | u32 canvas, i32 x, i32 y, i32 dx, i32 dy, i32 steps x, i32 steps y, u32 modifiers |
| 0x0208 | KEY | u32 canvas, u32 keysym, u32 keycode, u32 pressed, u32 modifiers, str text |
| 0x0209 | TEXT | u32 canvas, str text |
| 0x020d | POINTER_ENTER | u32 canvas, i32 x, i32 y, u32 modifiers, u32 buttons |
| 0x020e | POINTER_LEAVE | u32 canvas |
| 0x020f | PREEDIT | u32 canvas, str text, i32 cursor begin, i32 cursor end |

- Coordinates are canvas pixels. Each canvas has its own pointer.
- `button`: 1 left, 2 middle, 3 right, 8 back, 9 forward. `buttons` is the mask held *before* the
  event: 0x100 left, 0x200 middle, 0x400 right (X11's).
- `modifiers` (X11's state, before the event): 0x01 Shift, 0x02 Caps Lock, 0x04 Control,
  0x08 Alt, 0x40 Super.
- `SCROLL`: `dx`/`dy` in pixels (40 per wheel notch; positive `dy` scrolls down), `steps` in
  notches.
- `KEY`: `keysym` is the X11 keysym (`0x61` a, `0xff0d` Return, Unicode characters
  `0x01000000 + code point`), `keycode` the Linux evdev key code on a US layout (X11's keycode
  minus 8; 0 for characters no key has), `text` what the key types (empty for keys that type
  nothing and while Control, Alt or Super is held).
- `TEXT`: text committed as an input method would (`gui type --commit`); it replaces the
  composition (`PREEDIT`) if there is one.
- `PREEDIT`: the input method's composition, shown at the caret until the next `PREEDIT` or a
  `TEXT`; an empty one ends it. The cursor is a byte range in `text` (-1, -1: no cursor).
  `gui type --ime` composes Hangul syllable by syllable as a Korean (2-set) input method shows
  it — 한글: `ㅎ`, `하`, `TEXT 한`, `ㄱ`, `그`, `TEXT 글` — and types what it does not compose as keys.
- `POINTER_ENTER` comes before the first pointer event after the canvas was made or the pointer
  left (`POINTER_LEAVE`, `gui leave`): hover starts and ends with them.
- `TEXT_INPUT` (app → server): a text field of the canvas has the keyboard (with the caret's
  rectangle), or none has. The server keeps it for `gui info` (`textInput`), so an agent can see
  that typing goes somewhere; it also says where the composition is shown.
- The server keeps each canvas's held buttons and modifiers across commands (`gui keydown`,
  `gui mousedown`, `--mods`), and every event carries them; `release` lets go of all of them.

## Clipboard

One clipboard, shared by all apps and the command line. Setting it hands the server the data
itself (one or more representations of one thing), so it outlives the program that copied.

| op | name | payload |
|---|---|---|
| 0x0109 | CLIPBOARD_SET | app → server: u32 n, n × (str mime, bytes data); n = 0 empties it |
| 0x010a | CLIPBOARD_GET | app → server: u32 request, str mime |
| 0x020a | CLIPBOARD_CHANGED | server → apps: u32 n, n × str mime |
| 0x020b | CLIPBOARD_DATA | server → app: u32 request, u32 found, str mime, bytes data |

`text/plain` (or `text`, `UTF8_STRING`, `STRING`) finds any `text/plain…` representation.
Data is limited to 16 MiB.

## Synchronisation

| op | name | payload |
|---|---|---|
| 0x020c | PING | server → app: u32 serial |
| 0x010b | PONG | app → server: u32 serial |

The server sends `PING` after input when a command waits for it (`gui wait --sync`, every input
command unless `--no-sync`). An app answers with `PONG` once it has handled every message before
the `PING` **and drawn what they changed**: the reference client sends it right after the app's
next `CANVAS_COMMIT`, or when the app next waits for events, whichever comes first
(`cg_flush()`). Work an app starts and finishes later (a clipboard request, a page load) is not
covered: wait for the canvas to settle (`gui wait --idle MS`, `gui screenshot --idle MS`).

## Control

| op | name | payload |
|---|---|---|
| 0x0301 | CONTROL | u32 request, u32 argc, argc × str, bytes blob |
| 0x0302 | RESULT | u32 request, u32 status, str text, bytes blob |

A control request is a list of words, much like a command line; `RESULT.status` is 0 or an error
code, `text` the answer or the error. Requests that wait (`wait`, `kill`) are answered when done,
so several can be outstanding.

| request | answer |
|---|---|
| `status` | text |
| `list [json]` | text table, or JSON |
| `info TARGET` | JSON |
| `launch [name=N] [size=WxH] [cwd=D] [env=K=V]... -- /ABS/PROGRAM ARGS...` | `NAME PID LOGFILE` |
| `capture TARGET [X Y W H]` | text `W H FRAMES CANVAS FULLW FULLH`, blob: the pixels (XRGB8888) |
| `input TARGET [nofocus] EVENT...` | text `X Y` (the pointer); events: `motion X Y`, `button N down\|up`, `wheel DX DY SX SY`, `key KEYSYM KEYCODE down\|up TEXT`, `text STRING`, `preedit STRING BEGIN END`, `leave`, `release` |
| `focus TARGET`, `unfocus` | — |
| `resize TARGET W H` | text `WxH` (as held to the limits) |
| `close TARGET` | — |
| `kill TARGET [SIGNAL [GRACE_MS]]` | how it ended |
| `clipboard types\|get [MIME]\|set [MIME]\|clear` | `get`: text = mime, blob = data; `set` takes the blob |
| `wait TARGET ready\|exit\|frame\|idle MS\|size W H\|sync TIMEOUT_MS` | when it happens, or status 3 |
| `shutdown` | — |

A TARGET is `NAME` (the program's first canvas), `NAME:N` (its canvas N, numbered from 1 in
the order they were made) or `.` (the focused canvas, else the only program).
