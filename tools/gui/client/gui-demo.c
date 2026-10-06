/* gui-demo - a small GUI program for the collaboCore display: a button, a text field, a
 * drawing area and a status line, redrawn at whatever size the canvas is given. It prints every
 * event it handles to stdout (`gui logs demo`), which is what the add-on's tests read.
 *
 *   gui-demo [--title T] [--canvases N] [--size WxH] [--name N] [--frames N]
 *
 * Keys in the text field: typing, BackSpace, Return (prints the line), ctrl+c (copies the
 * field), ctrl+v (pastes), ctrl+l (clears). Left drag draws in the drawing area, right click
 * clears it, the wheel moves a counter, ctrl+click on the button counts ten, the button lights
 * up under the pointer, an input method's composition shows underlined in the field.
 * Canvases after the first are plain colour panels.
 * --frames N: a benchmark instead, N whole frames each waited for (FRAME_DONE), then the rate. */
#define _POSIX_C_SOURCE 200809L
#include "collabo_gui.h"
#include "font8x16.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

struct view {
	uint32_t id, w, h;
	uint32_t *px;
	int focused, dirty, index;
	unsigned char *ink; /* the drawing area, one byte a pixel, at the canvas size */
};

static cg_display *d;
static struct view views[8];
static int n_views;
static char field[1024], preedit[256];
static int hover, caret_x = -1; /* the pointer over the button; the caret last told the server */
static int clicks, scrolled, pressed_on_button;
static int px_, py_, last_x = -1, last_y;
static char status[160] = "ready";
static char clip_types[256];

#define BG 0xf0f0f0u
#define TITLE_BG 0x1f3a60u
#define BUTTON 0x3070c0u
#define BUTTON_DOWN 0x1f4f90u
#define BUTTON_HOVER 0x4a8ae0u
#define FIELD_BG 0xffffffu
#define BORDER 0x707070u
#define INK 0x101010u

static void fill(struct view *v, int x, int y, int w, int h, uint32_t c)
{
	int x0 = x < 0 ? 0 : x, y0 = y < 0 ? 0 : y;
	int x1 = x + w > (int)v->w ? (int)v->w : x + w, y1 = y + h > (int)v->h ? (int)v->h : y + h;
	for (int j = y0; j < y1; j++)
		for (int i = x0; i < x1; i++) v->px[(size_t)j * v->w + i] = c;
}

static void frame_rect(struct view *v, int x, int y, int w, int h, uint32_t c)
{
	fill(v, x, y, w, 1, c);
	fill(v, x, y + h - 1, w, 1, c);
	fill(v, x, y, 1, h, c);
	fill(v, x + w - 1, y, 1, h, c);
}

/* Text in the 8x16 font; characters outside ASCII are drawn as a box. Returns the width. */
static int text(struct view *v, int x, int y, const char *s, uint32_t c, int max_w)
{
	int cx = x;
	const unsigned char *p = (const unsigned char *)s;
	while (*p) {
		if (max_w > 0 && cx + 8 > x + max_w) break;
		unsigned ch = *p;
		int len = ch < 0x80 ? 1 : ch < 0xe0 ? 2 : ch < 0xf0 ? 3 : 4;
		if (ch >= 0x20 && ch < 0x7f) {
			const unsigned char *g = font8x16[ch - 0x20];
			for (int j = 0; j < 16; j++)
				for (int i = 0; i < 8; i++)
					if (g[j] & (0x80 >> i)) fill(v, cx + i, y + j, 1, 1, c);
		} else {
			frame_rect(v, cx + 1, y + 3, 6, 11, c);
		}
		cx += 8;
		for (int k = 0; k < len && *p; k++) p++;
	}
	return cx - x;
}

/* The layout, from the canvas size. */
static void layout(const struct view *v, int *bx, int *by, int *bw, int *bh, int *fx, int *fy, int *fw, int *ax, int *ay, int *aw, int *ah)
{
	*bx = 16, *by = 44, *bw = 176, *bh = 36;
	*fx = 16, *fy = 96, *fw = (int)v->w - 32, *ax = 16, *ay = 148;
	*aw = (int)v->w - 32;
	*ah = (int)v->h - *ay - 40;
	if (*fw < 16) *fw = 16;
	if (*aw < 1) *aw = 1;
	if (*ah < 1) *ah = 1;
}

static void render(struct view *v)
{
	char line[256];
	if (v->index > 0) {
		static const uint32_t colors[] = {0xc04040u, 0x40a040u, 0x4040c0u, 0xc0a040u};
		fill(v, 0, 0, (int)v->w, (int)v->h, colors[(v->index - 1) % 4]);
		snprintf(line, sizeof line, "panel %d  %ux%u%s", v->index + 1, v->w, v->h, v->focused ? "  (focus)" : "");
		text(v, 12, 12, line, 0xffffffu, 0);
		return;
	}
	int bx, by, bw, bh, fx, fy, fw, ax, ay, aw, ah;
	layout(v, &bx, &by, &bw, &bh, &fx, &fy, &fw, &ax, &ay, &aw, &ah);
	fill(v, 0, 0, (int)v->w, (int)v->h, BG);
	fill(v, 0, 0, (int)v->w, 28, TITLE_BG);
	snprintf(line, sizeof line, "GUI demo  %ux%u  %s", v->w, v->h, v->focused ? "focused" : "not focused");
	text(v, 8, 6, line, 0xffffffu, (int)v->w - 16);
	fill(v, bx, by, bw, bh, pressed_on_button ? BUTTON_DOWN : hover ? BUTTON_HOVER : BUTTON);
	snprintf(line, sizeof line, "Click me (%d)", clicks);
	text(v, bx + (bw - 8 * (int)strlen(line)) / 2, by + 10, line, 0xffffffu, 0);
	fill(v, fx, fy, fw, 32, FIELD_BG);
	frame_rect(v, fx, fy, fw, 32, v->focused ? BUTTON : BORDER);
	int tw = text(v, fx + 8, fy + 8, field, INK, fw - 24);
	if (preedit[0]) { /* the composition, underlined, after the text */
		int pw = text(v, fx + 8 + tw, fy + 8, preedit, BUTTON_DOWN, fw - 24 - tw);
		fill(v, fx + 8 + tw, fy + 25, pw, 2, BUTTON_DOWN);
		tw += pw;
	}
	if (v->focused) fill(v, fx + 8 + tw + 1, fy + 6, 2, 20, INK);
	/* Tell the server where typing goes (gui info shows it), when that changes. */
	int cx = v->focused ? fx + 8 + tw : -1;
	if (cx != caret_x) {
		cg_text_input(d, v->id, v->focused, cx, fy + 6, 2, 20);
		caret_x = cx;
	}
	fill(v, ax, ay, aw, ah, FIELD_BG);
	for (int j = 0; j < ah; j++)
		for (int i = 0; i < aw; i++)
			if (v->ink[(size_t)(ay + j) * v->w + ax + i]) v->px[(size_t)(ay + j) * v->w + ax + i] = INK;
	frame_rect(v, ax, ay, aw, ah, BORDER);
	snprintf(line, sizeof line, "pointer %d,%d  scroll %d  clipboard [%s]  %s", px_, py_, scrolled, clip_types, status);
	text(v, 16, (int)v->h - 28, line, INK, (int)v->w - 32);
}

static struct view *find(uint32_t id)
{
	for (int i = 0; i < n_views; i++)
		if (views[i].id == id) return &views[i];
	return NULL;
}

static int resize(struct view *v, uint32_t w, uint32_t h)
{
	uint32_t *px = calloc((size_t)w * h, 4);
	unsigned char *ink = calloc((size_t)w * h, 1);
	if (!px || !ink) {
		free(px);
		free(ink);
		return -1;
	}
	if (v->ink) /* keep the drawing where the sizes overlap */
		for (uint32_t j = 0; j < h && j < v->h; j++) memcpy(ink + (size_t)j * w, v->ink + (size_t)j * v->w, w < v->w ? w : v->w);
	free(v->px);
	free(v->ink);
	v->px = px;
	v->ink = ink;
	v->w = w;
	v->h = h;
	v->dirty = 1;
	return 0;
}

static void ink_at(struct view *v, int x, int y)
{
	int bx, by, bw, bh, fx, fy, fw, ax, ay, aw, ah;
	layout(v, &bx, &by, &bw, &bh, &fx, &fy, &fw, &ax, &ay, &aw, &ah);
	for (int j = -2; j <= 2; j++)
		for (int i = -2; i <= 2; i++) {
			int xx = x + i, yy = y + j;
			if (xx > ax && yy > ay && xx < ax + aw - 1 && yy < ay + ah - 1) v->ink[(size_t)yy * v->w + xx] = 1;
		}
}

static void mark_all(void)
{
	for (int i = 0; i < n_views; i++) views[i].dirty = 1;
}

static void delete_last_char(void)
{
	size_t n = strlen(field);
	while (n > 0 && ((unsigned char)field[n - 1] & 0xc0) == 0x80) n--;
	if (n > 0) n--;
	field[n] = 0;
}

static void append(const char *s)
{
	size_t n = strlen(field), m = strlen(s);
	if (n + m < sizeof field) memcpy(field + n, s, m + 1);
}

static void handle(const cg_event *ev)
{
	struct view *v = find(ev->canvas);
	switch (ev->type) {
	case CG_EVENT_CONFIGURE:
		printf("configure %d %ux%u\n", v ? v->index + 1 : 0, ev->width, ev->height);
		if (v && (v->w != ev->width || v->h != ev->height)) resize(v, ev->width, ev->height);
		break;
	case CG_EVENT_CLOSE:
		printf("close requested %d\n", v ? v->index + 1 : 0);
		fflush(stdout);
		cg_disconnect(d);
		exit(0);
	case CG_EVENT_FOCUS:
		printf("focus %d %u\n", v ? v->index + 1 : 0, ev->focused);
		if (v) v->focused = (int)ev->focused, v->dirty = 1;
		break;
	case CG_EVENT_POINTER_ENTER:
	case CG_EVENT_POINTER_LEAVE:
		if (ev->type == CG_EVENT_POINTER_ENTER) printf("enter %d,%d\n", ev->x, ev->y);
		else printf("leave\n");
		hover = 0;
		mark_all();
		break;
	case CG_EVENT_PREEDIT:
		printf("preedit \"%s\" %d %d\n", ev->text, ev->cursor_begin, ev->cursor_end);
		snprintf(preedit, sizeof preedit, "%s", ev->text);
		mark_all();
		break;
	case CG_EVENT_POINTER_MOTION:
		px_ = ev->x, py_ = ev->y;
		printf("motion %d,%d buttons=0x%x mods=0x%x\n", ev->x, ev->y, ev->buttons, ev->modifiers);
		if (v && v->index == 0) {
			int bx, by, bw, bh, fx, fy, fw, ax, ay, aw, ah;
			layout(v, &bx, &by, &bw, &bh, &fx, &fy, &fw, &ax, &ay, &aw, &ah);
			hover = ev->x >= bx && ev->x < bx + bw && ev->y >= by && ev->y < by + bh;
		}
		if (v && v->index == 0) {
			if (ev->buttons & CG_BUTTON_LEFT) {
				/* A line from the last point, in dots a pixel apart. */
				int n = last_x < 0 ? 0 : abs(ev->x - last_x) > abs(ev->y - last_y) ? abs(ev->x - last_x) : abs(ev->y - last_y);
				for (int i = 1; i <= n; i++) ink_at(v, last_x + (ev->x - last_x) * i / n, last_y + (ev->y - last_y) * i / n);
				ink_at(v, ev->x, ev->y);
				last_x = ev->x, last_y = ev->y;
			}
			v->dirty = 1;
		}
		break;
	case CG_EVENT_POINTER_BUTTON: {
		printf("button %u %s %d,%d mods=0x%x\n", ev->button, ev->pressed ? "down" : "up", ev->x, ev->y, ev->modifiers);
		if (!v || v->index != 0) break;
		int bx, by, bw, bh, fx, fy, fw, ax, ay, aw, ah;
		layout(v, &bx, &by, &bw, &bh, &fx, &fy, &fw, &ax, &ay, &aw, &ah);
		int on_button = ev->x >= bx && ev->x < bx + bw && ev->y >= by && ev->y < by + bh;
		if (ev->button == 1 && ev->pressed) {
			pressed_on_button = on_button;
			ink_at(v, ev->x, ev->y);
			last_x = ev->x, last_y = ev->y;
		} else if (ev->button == 1) {
			last_x = -1;
			if (pressed_on_button && on_button) {
				clicks += ev->modifiers & CG_MOD_CONTROL ? 10 : 1; /* ctrl+click counts ten */
				printf("clicked %d\n", clicks);
				snprintf(status, sizeof status, "clicked %d", clicks);
			}
			pressed_on_button = 0;
		} else if (ev->button == 3 && ev->pressed) {
			memset(v->ink, 0, (size_t)v->w * v->h);
			snprintf(status, sizeof status, "cleared");
		}
		v->dirty = 1;
		break;
	}
	case CG_EVENT_SCROLL:
		scrolled += ev->steps_y;
		printf("scroll dx=%d dy=%d steps=%d,%d at %d,%d\n", ev->dx, ev->dy, ev->steps_x, ev->steps_y, ev->x, ev->y);
		mark_all();
		break;
	case CG_EVENT_KEY:
		printf("key 0x%x code=%u %s mods=0x%x text=\"%s\"\n", ev->keysym, ev->keycode, ev->pressed ? "down" : "up", ev->modifiers, ev->text);
		if (!ev->pressed) break;
		if (ev->modifiers & CG_MOD_CONTROL) {
			if (ev->keysym == 'c' || ev->keysym == 'C') {
				cg_clipboard_set_text(d, field);
				printf("copied \"%s\"\n", field);
				snprintf(status, sizeof status, "copied");
			} else if (ev->keysym == 'v' || ev->keysym == 'V') {
				cg_clipboard_request(d, "text/plain");
			} else if (ev->keysym == 'l' || ev->keysym == 'L') {
				field[0] = 0;
			}
		} else if (ev->keysym == 0xff08) {
			delete_last_char();
		} else if (ev->keysym == 0xff0d || ev->keysym == 0xff8d) {
			printf("enter \"%s\"\n", field);
			snprintf(status, sizeof status, "entered %zu bytes", strlen(field));
			field[0] = 0;
		} else if (ev->text[0] && (unsigned char)ev->text[0] >= 0x20) {
			append(ev->text);
		}
		mark_all();
		break;
	case CG_EVENT_TEXT:
		printf("text \"%s\"\n", ev->text);
		preedit[0] = 0; /* a commit replaces the composition */
		append(ev->text);
		mark_all();
		break;
	case CG_EVENT_CLIPBOARD_CHANGED:
		snprintf(clip_types, sizeof clip_types, "%s", ev->text);
		for (char *c = clip_types; *c; c++)
			if (*c == '\n') *c = ' ';
		printf("clipboard changed [%s]\n", clip_types);
		mark_all();
		break;
	case CG_EVENT_CLIPBOARD_DATA:
		printf("pasted found=%u type=%s \"%.*s\"\n", ev->found, ev->mime, (int)ev->length, (const char *)ev->data);
		if (ev->found) {
			char buf[512];
			size_t n = ev->length < sizeof buf - 1 ? ev->length : sizeof buf - 1;
			memcpy(buf, ev->data, n);
			buf[n] = 0;
			append(buf);
		}
		mark_all();
		break;
	case CG_EVENT_ERROR:
		printf("error %u: %s\n", ev->code, ev->text);
		break;
	default:
		break;
	}
}

int main(int argc, char **argv)
{
	const char *title = "GUI demo", *name = NULL;
	int count = 1, bench = 0;
	uint32_t want_w = 0, want_h = 0;
	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--title") && i + 1 < argc) title = argv[++i];
		else if (!strcmp(argv[i], "--name") && i + 1 < argc) name = argv[++i];
		else if (!strcmp(argv[i], "--canvases") && i + 1 < argc) count = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--size") && i + 1 < argc) sscanf(argv[++i], "%ux%u", &want_w, &want_h);
		else if (!strcmp(argv[i], "--frames") && i + 1 < argc) bench = atoi(argv[++i]);
		else {
			fprintf(stderr, "usage: gui-demo [--title T] [--canvases N] [--size WxH] [--name N] [--frames N]\n");
			return 2;
		}
	}
	if (count < 1) count = 1;
	if (count > 8) count = 8;
	setvbuf(stdout, NULL, _IOLBF, 0);
	d = cg_connect(name);
	if (!d) {
		fprintf(stderr, "gui-demo: %s\n", cg_connect_error());
		return 1;
	}
	printf("connected as %s\n", cg_app_name(d));
	for (int i = 0; i < count; i++) {
		char t[160];
		snprintf(t, sizeof t, i ? "%s (%d)" : "%s", title, i + 1);
		uint32_t id = cg_canvas_create(d, want_w, want_h, t);
		if (!id) {
			fprintf(stderr, "gui-demo: canvas %d: %s\n", i + 1, cg_error(d));
			if (i == 0) return 1;
			break;
		}
		struct view *v = &views[n_views++];
		memset(v, 0, sizeof *v);
		v->id = id;
		v->index = i;
		uint32_t w, h;
		cg_canvas_size(d, id, &w, &h);
		if (resize(v, w, h) < 0) {
			fprintf(stderr, "gui-demo: out of memory\n");
			return 1;
		}
		printf("canvas %d %ux%u\n", i + 1, w, h);
	}
	cg_canvas_set_cursor(d, views[0].id, "default");
	if (bench > 0) {
		struct view *v = &views[0];
		struct timespec t0, t1;
		clock_gettime(CLOCK_MONOTONIC, &t0);
		for (int f = 0; f < bench; f++) {
			for (size_t i = 0; i < (size_t)v->w * v->h; i++) v->px[i] = (uint32_t)(i * 2654435761u + (unsigned)f * 40503u);
			int64_t serial = cg_canvas_present(d, v->id, v->px, v->w * 4, v->w, v->h, NULL, 0);
			if (serial < 0) {
				fprintf(stderr, "gui-demo: %s\n", cg_error(d));
				return 1;
			}
			cg_event ev;
			while (cg_next_event(d, &ev, 5000) > 0 && !(ev.type == CG_EVENT_FRAME_DONE && ev.serial == (uint32_t)serial)) {}
		}
		clock_gettime(CLOCK_MONOTONIC, &t1);
		double s = (double)(t1.tv_sec - t0.tv_sec) + (double)(t1.tv_nsec - t0.tv_nsec) / 1e9;
		printf("%d frames of %ux%u in %.3f s: %.1f frames/s, %.1f MB/s\n", bench, v->w, v->h, s, bench / s,
		       (double)bench * v->w * v->h * 4 / s / 1e6);
		cg_disconnect(d);
		return 0;
	}
	for (;;) {
		int any = 0;
		for (int i = 0; i < n_views; i++) any |= views[i].dirty;
		/* Handle everything that is waiting, then draw once. */
		cg_event ev;
		int r = cg_next_event(d, &ev, any ? 0 : -1);
		if (r < 0) {
			fprintf(stderr, "gui-demo: %s\n", cg_error(d));
			return 1;
		}
		if (r > 0) {
			handle(&ev);
			continue;
		}
		for (int i = 0; i < n_views; i++) {
			struct view *v = &views[i];
			if (!v->dirty) continue;
			render(v);
			if (cg_canvas_present(d, v->id, v->px, v->w * 4, v->w, v->h, NULL, 0) < 0) {
				fprintf(stderr, "gui-demo: %s\n", cg_error(d));
				return 1;
			}
			v->dirty = 0;
		}
	}
}
