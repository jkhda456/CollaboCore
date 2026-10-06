/* collabo_gui.h - draw into a collaboCore canvas and receive its input.
 *
 * A canvas is one whole window-sized frame of pixels that the display server keeps for as long
 * as the program holds it. There are no window positions, no stacking and no occlusion: every
 * canvas is always complete, whoever looks at it (`gui screenshot`, `gui view`). The server
 * decides a canvas's size (CG_EVENT_CONFIGURE); draw at that size and present.
 *
 * Pixels are XRGB8888: one uint32_t per pixel, 0x00RRGGBB in native (little-endian) order, i.e.
 * the bytes B, G, R, X — cairo's CAIRO_FORMAT_RGB24, pixman's x8r8g8b8, Skia's BGRA_8888 with
 * opaque alpha, wl_shm's XRGB8888.
 *
 * Every call is synchronous over one Unix socket and is not thread-safe: use a display from one
 * thread at a time. cg_fd() gives the socket for a poll() loop of your own.
 *
 *   cg_display *d = cg_connect(NULL);
 *   uint32_t c = cg_canvas_create(d, 0, 0, "hello");        // 0, 0: the server's choice
 *   uint32_t w, h; cg_canvas_size(d, c, &w, &h);
 *   ... draw w*h pixels ...; cg_canvas_present(d, c, pixels, w * 4, w, h, NULL, 0);
 *   cg_event ev;
 *   while (cg_next_event(d, &ev, -1) > 0) { ... }
 *
 * Link with libcollabo-gui.a (no other dependencies). Protocol: tools/gui/PROTOCOL.md. */
#ifndef COLLABO_GUI_H
#define COLLABO_GUI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define CG_PROTOCOL_VERSION 1

typedef struct cg_display cg_display;

enum cg_event_type {
	CG_EVENT_NONE = 0,
	CG_EVENT_CONFIGURE,         /* canvas, width, height: draw at this size from now on */
	CG_EVENT_FRAME_DONE,        /* canvas, serial: that commit is on the canvas */
	CG_EVENT_CLOSE,             /* canvas: asked to close (gui close) */
	CG_EVENT_FOCUS,             /* canvas, focused (0/1): the keyboard focus */
	CG_EVENT_POINTER_MOTION,    /* canvas, x, y, modifiers, buttons */
	CG_EVENT_POINTER_BUTTON,    /* canvas, x, y, button (1 left, 2 middle, 3 right, 8 back, 9 forward),
	                               pressed, modifiers, buttons (held before this event) */
	CG_EVENT_SCROLL,            /* canvas, x, y, dx, dy (pixels; +dy is down), steps_x, steps_y
	                               (wheel notches), modifiers */
	CG_EVENT_KEY,               /* canvas, keysym (X11), keycode (Linux evdev; X11 keycode - 8),
	                               pressed, modifiers (before this key), text (what it types, or "") */
	CG_EVENT_TEXT,              /* canvas, text: committed text, as from an input method */
	CG_EVENT_CLIPBOARD_CHANGED, /* text: the new clipboard's types, one per line ("" = empty) */
	CG_EVENT_CLIPBOARD_DATA,    /* request, found, mime, data, length: answer to cg_clipboard_request */
	CG_EVENT_ERROR,             /* code, text: the server refused something */
	CG_EVENT_POINTER_ENTER,     /* canvas, x, y, modifiers, buttons: the pointer came in (hover starts) */
	CG_EVENT_POINTER_LEAVE,     /* canvas: the pointer left (hover ends) */
	CG_EVENT_PREEDIT,           /* canvas, text, cursor_begin, cursor_end: the input method's
	                               composition, shown at the caret until replaced; "" ends it.
	                               A CG_EVENT_TEXT commit replaces it too. Cursor: byte offsets
	                               in text, -1 for none */
};

/* Modifier bits (X11's). */
#define CG_MOD_SHIFT 0x01
#define CG_MOD_CAPS_LOCK 0x02
#define CG_MOD_CONTROL 0x04
#define CG_MOD_ALT 0x08
#define CG_MOD_SUPER 0x40
/* Pointer button bits in `buttons`. */
#define CG_BUTTON_LEFT 0x100
#define CG_BUTTON_MIDDLE 0x200
#define CG_BUTTON_RIGHT 0x400

typedef struct cg_event {
	int type;
	uint32_t canvas;
	int32_t x, y;
	uint32_t width, height;
	uint32_t button, pressed, modifiers, buttons;
	int32_t dx, dy, steps_x, steps_y;
	uint32_t keysym, keycode;
	uint32_t serial, focused, request, found, code;
	/* Valid until the next cg_next_event() (or any call that reads from the server). */
	const char *text; /* UTF-8, NUL-terminated, never NULL */
	const char *mime; /* never NULL */
	const void *data;
	size_t length;
	int32_t cursor_begin, cursor_end;
} cg_event;

typedef struct cg_rect {
	uint32_t x, y, width, height;
} cg_rect;

/* Connects to the display server: $COLLABO_GUI_SOCKET, else $COLLABO_GUI_DIR/socket, else
 * /tmp/.collabo-gui/socket. With no server there (and no $COLLABO_GUI_SOCKET), starts one
 * (`gui server --detach`). `name` names the program in `gui list` (NULL: $COLLABO_GUI_NAME or
 * the program's own name). NULL on failure; cg_connect_error() says why. */
cg_display *cg_connect(const char *name);
const char *cg_connect_error(void);
void cg_disconnect(cg_display *d);

int cg_fd(const cg_display *d);
/* The name the server gave this program (what `gui` commands call it). */
const char *cg_app_name(const cg_display *d);
void cg_limits(const cg_display *d, uint32_t *default_width, uint32_t *default_height,
               uint32_t *max_width, uint32_t *max_height);
/* The last error on this display (a failed call, a disconnect). */
const char *cg_error(const cg_display *d);

/* A new canvas; 0 on failure. Width and height are a wish (0: the server's default); the
 * server answers with the size to draw at, which cg_canvas_size() knows on return. */
uint32_t cg_canvas_create(cg_display *d, uint32_t width, uint32_t height, const char *title);
int cg_canvas_destroy(cg_display *d, uint32_t canvas);
/* The size the server last configured. */
int cg_canvas_size(const cg_display *d, uint32_t canvas, uint32_t *width, uint32_t *height);
int cg_canvas_set_title(cg_display *d, uint32_t canvas, const char *title);
int cg_canvas_request_size(cg_display *d, uint32_t canvas, uint32_t width, uint32_t height);
/* A cursor name (CSS's: "default", "pointer", "text", "wait", ...), shown by `gui info`. */
int cg_canvas_set_cursor(cg_display *d, uint32_t canvas, const char *name);
int cg_canvas_request_focus(cg_display *d, uint32_t canvas);
/* Says a text field of the canvas has the keyboard (enabled 1, with its caret's rectangle) or
 * none has (0). The input method's composition then comes as CG_EVENT_PREEDIT; `gui info`
 * shows agents that typing goes somewhere. */
int cg_text_input(cg_display *d, uint32_t canvas, int enabled, int32_t x, int32_t y, uint32_t w, uint32_t h);

/* Sends the rectangle (x, y, w, h) of `pixels` (a frame with `stride` bytes per row; the
 * rectangle is at the same place in the frame and on the canvas). Shown at the next commit. */
int cg_canvas_update(cg_display *d, uint32_t canvas, const void *pixels, uint32_t stride,
                     uint32_t x, uint32_t y, uint32_t w, uint32_t h);
/* Ends a frame `width` x `height` (normally the configured size). Returns its serial (> 0),
 * which a CG_EVENT_FRAME_DONE names once it is on the canvas; -1 on failure. */
int64_t cg_canvas_commit(cg_display *d, uint32_t canvas, uint32_t width, uint32_t height);
/* Update + commit: the damaged rectangles of a whole frame (n_damage 0: all of it). */
int64_t cg_canvas_present(cg_display *d, uint32_t canvas, const void *pixels, uint32_t stride,
                          uint32_t width, uint32_t height, const cg_rect *damage, int n_damage);

/* Puts text (UTF-8) on the clipboard, as text/plain;charset=utf-8 and UTF8_STRING. */
int cg_clipboard_set_text(cg_display *d, const char *utf8);
/* Puts `n` representations of one thing on the clipboard; n = 0 empties it. */
int cg_clipboard_set(cg_display *d, int n, const char *const *mimes, const void *const *data,
                     const size_t *lengths);
/* Asks for the clipboard's `mime` (NULL: text); the answer is a CG_EVENT_CLIPBOARD_DATA with
 * the returned request number. 0 on failure. */
uint32_t cg_clipboard_request(cg_display *d, const char *mime);

/* The next event: 1 (filled in `ev`), 0 (none within timeout_ms; -1 waits for ever, 0 does
 * not wait), -1 (disconnected or failed: cg_error()). */
int cg_next_event(cg_display *d, cg_event *ev, int timeout_ms);

/* Tells the server that everything received so far has been handled and drawn (it answers
 * `gui` commands waiting for that, as after input). cg_canvas_commit() and a cg_next_event()
 * that waits do it already; a program with its own main loop that dispatches with timeout 0
 * calls this before it goes idle (e.g. in a GSource's prepare). */
int cg_flush(cg_display *d);

/* The keysym's text, if it types one (UTF-8 into buf, which holds 8 bytes); 0 if not. */
int cg_keysym_to_utf8(uint32_t keysym, char buf[8]);

#ifdef __cplusplus
}
#endif

#endif
