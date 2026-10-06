/* collabo_gui.c - the client side of the collaboCore display protocol (see collabo_gui.h and
 * tools/gui/PROTOCOL.md). Plain C99 + POSIX, no other dependencies. */
#define _GNU_SOURCE
#include "collabo_gui.h"

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <spawn.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

enum {
	OP_HELLO = 0x0001,
	OP_WELCOME = 0x0002,
	OP_ERROR = 0x0003,
	OP_CANVAS_CREATE = 0x0101,
	OP_CANVAS_DESTROY = 0x0102,
	OP_CANVAS_TITLE = 0x0103,
	OP_CANVAS_UPDATE = 0x0104,
	OP_CANVAS_COMMIT = 0x0105,
	OP_CANVAS_REQUEST_SIZE = 0x0106,
	OP_CANVAS_CURSOR = 0x0107,
	OP_FOCUS_REQUEST = 0x0108,
	OP_CLIPBOARD_SET = 0x0109,
	OP_CLIPBOARD_GET = 0x010a,
	OP_PONG = 0x010b,
	OP_TEXT_INPUT = 0x010c,
	OP_CONFIGURE = 0x0201,
	OP_FRAME_DONE = 0x0202,
	OP_CLOSE = 0x0203,
	OP_FOCUS = 0x0204,
	OP_POINTER_MOTION = 0x0205,
	OP_POINTER_BUTTON = 0x0206,
	OP_SCROLL = 0x0207,
	OP_KEY = 0x0208,
	OP_TEXT = 0x0209,
	OP_CLIPBOARD_CHANGED = 0x020a,
	OP_CLIPBOARD_DATA = 0x020b,
	OP_PING = 0x020c,
	OP_POINTER_ENTER = 0x020d,
	OP_POINTER_LEAVE = 0x020e,
	OP_PREEDIT = 0x020f,
};

#define HEADER 8
#define MAX_PAYLOAD (32u << 20)
/* Pixel updates are sent in bands of rows of at most this many bytes. */
#define BAND_BYTES (8u << 20)

struct canvas {
	uint32_t id, w, h;
};

struct cg_display {
	int fd;
	int broken;
	char name[64];
	uint32_t def_w, def_h, max_w, max_h, max_canvases;
	struct canvas *canvases;
	size_t n_canvases, cap_canvases;
	uint32_t next_canvas, next_serial, next_request;
	uint32_t pong_due; /* a PING handled, answered once its effects are drawn (cg_flush) */
	unsigned char *rbuf;
	size_t rlen, rcap;
	size_t drop; /* bytes of the message the last event came from */
	char *text, *mime;
	size_t text_cap, mime_cap;
	char err[256];
};

static char connect_error[256];

const char *cg_connect_error(void) { return connect_error; }
const char *cg_error(const cg_display *d) { return d->err; }
int cg_fd(const cg_display *d) { return d->fd; }
const char *cg_app_name(const cg_display *d) { return d->name; }

void cg_limits(const cg_display *d, uint32_t *dw, uint32_t *dh, uint32_t *mw, uint32_t *mh)
{
	if (dw) *dw = d->def_w;
	if (dh) *dh = d->def_h;
	if (mw) *mw = d->max_w;
	if (mh) *mh = d->max_h;
}

static int fail(cg_display *d, const char *fmt, ...)
{
	va_list ap;
	va_start(ap, fmt);
	vsnprintf(d->err, sizeof d->err, fmt, ap);
	va_end(ap);
	return -1;
}

/* ── output ──────────────────────────────────────────────────────────────────────────────── */

struct msg {
	unsigned char *p;
	size_t len, cap;
	int oom;
};

static void put(struct msg *m, const void *v, size_t n)
{
	if (m->oom) return;
	if (m->len + n > m->cap) {
		size_t cap = m->cap ? m->cap * 2 : 64;
		while (cap < m->len + n) cap *= 2;
		unsigned char *p = realloc(m->p, cap);
		if (!p) { m->oom = 1; return; }
		m->p = p;
		m->cap = cap;
	}
	memcpy(m->p + m->len, v, n);
	m->len += n;
}

static void put_u32(struct msg *m, uint32_t v)
{
	unsigned char b[4] = {v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff, v >> 24};
	put(m, b, 4);
}

static void put_bytes(struct msg *m, const void *v, size_t n)
{
	put_u32(m, (uint32_t)n);
	put(m, v, n);
}

static void put_str(struct msg *m, const char *s) { put_bytes(m, s ? s : "", s ? strlen(s) : 0); }

static void begin(struct msg *m, uint16_t op)
{
	m->p = NULL;
	m->len = m->cap = 0;
	m->oom = 0;
	unsigned char h[HEADER] = {0, 0, 0, 0, op & 0xff, op >> 8, 0, 0};
	put(m, h, HEADER);
}

static int write_all(cg_display *d, const void *buf, size_t n)
{
	const unsigned char *p = buf;
	if (d->broken) return fail(d, "disconnected from the display server");
	while (n) {
		ssize_t w = send(d->fd, p, n, MSG_NOSIGNAL);
		if (w < 0 && errno == EINTR) continue;
		if (w <= 0) {
			d->broken = 1;
			return fail(d, "lost the display server: %s", strerror(errno));
		}
		p += w;
		n -= (size_t)w;
	}
	return 0;
}

/* Sends the message; `extra` bytes (announced in its length but not in the buffer) follow. */
static int finish(cg_display *d, struct msg *m, size_t extra)
{
	if (m->oom) {
		free(m->p);
		return fail(d, "out of memory");
	}
	uint32_t len = (uint32_t)(m->len - HEADER + extra);
	m->p[0] = len & 0xff;
	m->p[1] = (len >> 8) & 0xff;
	m->p[2] = (len >> 16) & 0xff;
	m->p[3] = len >> 24;
	int r = write_all(d, m->p, m->len);
	free(m->p);
	return r;
}

/* ── input ───────────────────────────────────────────────────────────────────────────────── */

static uint32_t rd32(const unsigned char *p) { return p[0] | p[1] << 8 | p[2] << 16 | (uint32_t)p[3] << 24; }

struct rd {
	const unsigned char *p;
	size_t n, i;
	int bad;
};

static uint32_t get_u32(struct rd *r)
{
	if (r->i + 4 > r->n) { r->bad = 1; return 0; }
	uint32_t v = rd32(r->p + r->i);
	r->i += 4;
	return v;
}

static const unsigned char *get_bytes(struct rd *r, size_t *len)
{
	size_t n = get_u32(r);
	if (r->bad || n > r->n - r->i) { r->bad = 1; *len = 0; return (const unsigned char *)""; }
	const unsigned char *p = r->p + r->i;
	r->i += n;
	*len = n;
	return p;
}

static void drop_pending(cg_display *d)
{
	if (!d->drop) return;
	memmove(d->rbuf, d->rbuf + d->drop, d->rlen - d->drop);
	d->rlen -= d->drop;
	d->drop = 0;
}

/* Reads what the socket has, waiting up to timeout_ms (-1: for ever). 1 read, 0 timed out. */
static int read_more(cg_display *d, int timeout_ms)
{
	if (d->broken) return fail(d, "disconnected from the display server");
	struct pollfd p = {d->fd, POLLIN, 0};
	int r;
	do r = poll(&p, 1, timeout_ms);
	while (r < 0 && errno == EINTR);
	if (r < 0) return fail(d, "poll: %s", strerror(errno));
	if (r == 0) return 0;
	if (d->rcap - d->rlen < 65536) {
		size_t cap = d->rcap ? d->rcap * 2 : 131072;
		unsigned char *b = realloc(d->rbuf, cap);
		if (!b) return fail(d, "out of memory");
		d->rbuf = b;
		d->rcap = cap;
	}
	ssize_t n;
	do n = recv(d->fd, d->rbuf + d->rlen, d->rcap - d->rlen, MSG_DONTWAIT);
	while (n < 0 && errno == EINTR);
	if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return 1;
	if (n <= 0) {
		d->broken = 1;
		return fail(d, n == 0 ? "the display server closed the connection" : "lost the display server: %s", strerror(errno));
	}
	d->rlen += (size_t)n;
	return 1;
}

/* The message at `off` in the buffer, if it is all there: its op, payload and length. */
static int message_at(cg_display *d, size_t off, uint16_t *op, const unsigned char **payload, size_t *len)
{
	if (d->rlen - off < HEADER) return 0;
	uint32_t n = rd32(d->rbuf + off);
	if (n > MAX_PAYLOAD) {
		d->broken = 1;
		fail(d, "the display server sent a message too large");
		return -1;
	}
	if (d->rlen - off < HEADER + (size_t)n) return 0;
	*op = d->rbuf[off + 4] | d->rbuf[off + 5] << 8;
	*payload = d->rbuf + off + HEADER;
	*len = n;
	return 1;
}

static int64_t now_ms(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

static int remaining(int64_t deadline)
{
	if (deadline < 0) return -1;
	int64_t left = deadline - now_ms();
	return left < 0 ? 0 : (int)(left > 0x7fffffff ? 0x7fffffff : left);
}

static const char *own_string(char **buf, size_t *cap, const unsigned char *s, size_t n)
{
	if (n + 1 > *cap) {
		size_t c = n + 1 < 256 ? 256 : n + 1;
		char *b = realloc(*buf, c);
		if (!b) return "";
		*buf = b;
		*cap = c;
	}
	memcpy(*buf, s, n);
	(*buf)[n] = 0;
	return *buf;
}

/* ── connecting ──────────────────────────────────────────────────────────────────────────── */

static void socket_path(char *out, size_t n)
{
	const char *s = getenv("COLLABO_GUI_SOCKET");
	if (s && *s) { snprintf(out, n, "%s", s); return; }
	const char *dir = getenv("COLLABO_GUI_DIR");
	snprintf(out, n, "%s/socket", dir && *dir ? dir : "/tmp/.collabo-gui");
}

static int try_connect(const char *path)
{
	struct sockaddr_un a;
	memset(&a, 0, sizeof a);
	a.sun_family = AF_UNIX;
	if (strlen(path) >= sizeof a.sun_path) { errno = ENAMETOOLONG; return -1; }
	strcpy(a.sun_path, path);
	int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	if (fd < 0) return -1;
	if (connect(fd, (struct sockaddr *)&a, sizeof a) == 0) return fd;
	int e = errno;
	close(fd);
	errno = e;
	return -1;
}

/* No server yet: `gui server --detach`, its output in the runtime folder's server.log. */
static void start_server(void)
{
	char log[512];
	const char *dir = getenv("COLLABO_GUI_DIR");
	dir = dir && *dir ? dir : "/tmp/.collabo-gui";
	snprintf(log, sizeof log, "%s/server.log", dir);
	mkdir(dir, 0755);
	posix_spawn_file_actions_t fa;
	posix_spawnattr_t at;
	posix_spawn_file_actions_init(&fa);
	posix_spawn_file_actions_addopen(&fa, 0, "/dev/null", O_RDONLY, 0);
	posix_spawn_file_actions_addopen(&fa, 1, log, O_WRONLY | O_CREAT | O_APPEND, 0644);
	posix_spawn_file_actions_adddup2(&fa, 1, 2);
	posix_spawnattr_init(&at);
	/* No process group of its own: the server makes itself a new session (setsid), which a
	 * group leader could not. */
	char *argv[] = {"gui", "server", "--detach", NULL};
	pid_t pid;
	posix_spawnp(&pid, "gui", &fa, &at, argv, environ);
	posix_spawn_file_actions_destroy(&fa);
	posix_spawnattr_destroy(&at);
}

cg_display *cg_connect(const char *name)
{
	char path[512];
	socket_path(path, sizeof path);
	int fd = try_connect(path);
	if (fd < 0 && (errno == ENOENT || errno == ECONNREFUSED) && !getenv("COLLABO_GUI_SOCKET")) {
		start_server();
		int64_t deadline = now_ms() + 10000;
		while ((fd = try_connect(path)) < 0 && now_ms() < deadline) {
			struct timespec t = {0, 20 * 1000000};
			nanosleep(&t, NULL);
		}
	}
	if (fd < 0) {
		snprintf(connect_error, sizeof connect_error, "%s: %s (is the gui add-on on?)", path, strerror(errno));
		return NULL;
	}
	cg_display *d = calloc(1, sizeof *d);
	if (!d) { close(fd); snprintf(connect_error, sizeof connect_error, "out of memory"); return NULL; }
	d->fd = fd;
	if (!name || !*name) name = getenv("COLLABO_GUI_NAME");
	if (!name || !*name) name = program_invocation_short_name;
	const char *token = getenv("COLLABO_GUI_APP");
	struct msg m;
	begin(&m, OP_HELLO);
	put_u32(&m, CG_PROTOCOL_VERSION);
	put_u32(&m, 0); /* an app */
	put_u32(&m, (uint32_t)getpid());
	put_str(&m, name);
	put_str(&m, token ? token : "");
	if (finish(d, &m, 0) < 0) goto bad;
	int64_t deadline = now_ms() + 10000;
	for (;;) {
		uint16_t op;
		const unsigned char *p;
		size_t n;
		int got = message_at(d, 0, &op, &p, &n);
		if (got < 0) goto bad;
		if (got) {
			struct rd r = {p, n, 0, 0};
			if (op == OP_ERROR) {
				get_u32(&r);
				size_t l;
				const unsigned char *s = get_bytes(&r, &l);
				fail(d, "the display server refused: %.*s", (int)l, s);
				goto bad;
			}
			if (op != OP_WELCOME) { fail(d, "the display server answered something unexpected"); goto bad; }
			get_u32(&r); /* version */
			get_u32(&r); /* client id */
			size_t l;
			const unsigned char *s = get_bytes(&r, &l);
			snprintf(d->name, sizeof d->name, "%.*s", (int)l, s);
			d->def_w = get_u32(&r);
			d->def_h = get_u32(&r);
			d->max_w = get_u32(&r);
			d->max_h = get_u32(&r);
			d->max_canvases = get_u32(&r);
			if (r.bad) { fail(d, "a malformed WELCOME"); goto bad; }
			d->drop = HEADER + n;
			drop_pending(d);
			return d;
		}
		int rr = read_more(d, remaining(deadline));
		if (rr < 0) goto bad;
		if (rr == 0) { fail(d, "the display server did not answer"); goto bad; }
	}
bad:
	snprintf(connect_error, sizeof connect_error, "%s", d->err);
	cg_disconnect(d);
	return NULL;
}

void cg_disconnect(cg_display *d)
{
	if (!d) return;
	close(d->fd);
	free(d->canvases);
	free(d->rbuf);
	free(d->text);
	free(d->mime);
	free(d);
}

/* ── canvases ────────────────────────────────────────────────────────────────────────────── */

static struct canvas *find(const cg_display *d, uint32_t id)
{
	for (size_t i = 0; i < d->n_canvases; i++)
		if (d->canvases[i].id == id) return &d->canvases[i];
	return NULL;
}

uint32_t cg_canvas_create(cg_display *d, uint32_t width, uint32_t height, const char *title)
{
	if (d->n_canvases == d->cap_canvases) {
		size_t cap = d->cap_canvases ? d->cap_canvases * 2 : 4;
		struct canvas *c = realloc(d->canvases, cap * sizeof *c);
		if (!c) { fail(d, "out of memory"); return 0; }
		d->canvases = c;
		d->cap_canvases = cap;
	}
	uint32_t id = ++d->next_canvas;
	struct msg m;
	begin(&m, OP_CANVAS_CREATE);
	put_u32(&m, id);
	put_u32(&m, width);
	put_u32(&m, height);
	put_str(&m, title);
	drop_pending(d);
	size_t sent_at = d->rlen; /* errors that start after this are answers to us */
	if (finish(d, &m, 0) < 0) return 0;
	/* Wait for the CONFIGURE that answers it. Messages before it stay queued for
	 * cg_next_event (the CONFIGURE included). */
	int64_t deadline = now_ms() + 10000;
	size_t off = 0;
	for (;;) {
		uint16_t op;
		const unsigned char *p;
		size_t n;
		int got = message_at(d, off, &op, &p, &n);
		if (got < 0) return 0;
		if (got) {
			struct rd r = {p, n, 0, 0};
			if (op == OP_CONFIGURE && get_u32(&r) == id) {
				uint32_t w = get_u32(&r), h = get_u32(&r);
				d->canvases[d->n_canvases++] = (struct canvas){id, w, h};
				return id;
			}
			if (op == OP_ERROR && off >= sent_at) {
				get_u32(&r);
				size_t l;
				const unsigned char *s = get_bytes(&r, &l);
				fail(d, "%.*s", (int)l, s);
				return 0;
			}
			off += HEADER + n;
			continue;
		}
		int rr = read_more(d, remaining(deadline));
		if (rr < 0) return 0;
		if (rr == 0) { fail(d, "the display server did not answer"); return 0; }
	}
}

int cg_canvas_destroy(cg_display *d, uint32_t canvas)
{
	struct canvas *c = find(d, canvas);
	if (!c) return fail(d, "no canvas %u", canvas);
	*c = d->canvases[--d->n_canvases];
	struct msg m;
	begin(&m, OP_CANVAS_DESTROY);
	put_u32(&m, canvas);
	return finish(d, &m, 0);
}

int cg_canvas_size(const cg_display *d, uint32_t canvas, uint32_t *w, uint32_t *h)
{
	struct canvas *c = find(d, canvas);
	if (!c) return -1;
	if (w) *w = c->w;
	if (h) *h = c->h;
	return 0;
}

static int canvas_str(cg_display *d, uint16_t op, uint32_t canvas, const char *s)
{
	struct msg m;
	begin(&m, op);
	put_u32(&m, canvas);
	put_str(&m, s);
	return finish(d, &m, 0);
}

int cg_canvas_set_title(cg_display *d, uint32_t canvas, const char *t) { return canvas_str(d, OP_CANVAS_TITLE, canvas, t); }
int cg_canvas_set_cursor(cg_display *d, uint32_t canvas, const char *n) { return canvas_str(d, OP_CANVAS_CURSOR, canvas, n); }

int cg_canvas_request_size(cg_display *d, uint32_t canvas, uint32_t w, uint32_t h)
{
	struct msg m;
	begin(&m, OP_CANVAS_REQUEST_SIZE);
	put_u32(&m, canvas);
	put_u32(&m, w);
	put_u32(&m, h);
	return finish(d, &m, 0);
}

int cg_canvas_request_focus(cg_display *d, uint32_t canvas)
{
	struct msg m;
	begin(&m, OP_FOCUS_REQUEST);
	put_u32(&m, canvas);
	return finish(d, &m, 0);
}

int cg_text_input(cg_display *d, uint32_t canvas, int enabled, int32_t x, int32_t y, uint32_t w, uint32_t h)
{
	struct msg m;
	begin(&m, OP_TEXT_INPUT);
	put_u32(&m, canvas);
	put_u32(&m, enabled ? 1 : 0);
	put_u32(&m, (uint32_t)x);
	put_u32(&m, (uint32_t)y);
	put_u32(&m, w);
	put_u32(&m, h);
	return finish(d, &m, 0);
}

int cg_canvas_update(cg_display *d, uint32_t canvas, const void *pixels, uint32_t stride, uint32_t x, uint32_t y,
                     uint32_t w, uint32_t h)
{
	if (!w || !h) return 0;
	size_t row = (size_t)w * 4;
	uint32_t band = (uint32_t)(BAND_BYTES / row);
	if (band == 0) band = 1;
	const unsigned char *base = (const unsigned char *)pixels + (size_t)y * stride + (size_t)x * 4;
	unsigned char *stage = NULL;
	for (uint32_t y0 = 0; y0 < h; y0 += band) {
		uint32_t bh = h - y0 < band ? h - y0 : band;
		size_t bytes = row * bh;
		struct msg m;
		begin(&m, OP_CANVAS_UPDATE);
		put_u32(&m, canvas);
		put_u32(&m, x);
		put_u32(&m, y + y0);
		put_u32(&m, w);
		put_u32(&m, bh);
		put_u32(&m, (uint32_t)bytes);
		if (finish(d, &m, bytes) < 0) { free(stage); return -1; }
		const unsigned char *src = base + (size_t)y0 * stride;
		if (stride == row) {
			if (write_all(d, src, bytes) < 0) { free(stage); return -1; }
			continue;
		}
		/* Rows with gaps between them: gather them, a few hundred KiB at a time. */
		size_t per = (256u << 10) / row;
		if (per == 0) per = 1;
		if (!stage && !(stage = malloc(per * row))) { d->broken = 1; return fail(d, "out of memory"); }
		for (uint32_t j = 0; j < bh; j += (uint32_t)per) {
			uint32_t k = bh - j < per ? bh - j : (uint32_t)per;
			for (uint32_t i = 0; i < k; i++) memcpy(stage + i * row, src + (size_t)(j + i) * stride, row);
			if (write_all(d, stage, k * row) < 0) { free(stage); return -1; }
		}
	}
	free(stage);
	return 0;
}

int cg_flush(cg_display *d)
{
	if (!d->pong_due) return d->broken ? -1 : 0;
	struct msg m;
	begin(&m, OP_PONG);
	put_u32(&m, d->pong_due);
	d->pong_due = 0;
	return finish(d, &m, 0);
}

int64_t cg_canvas_commit(cg_display *d, uint32_t canvas, uint32_t width, uint32_t height)
{
	uint32_t serial = ++d->next_serial;
	if (!serial) serial = ++d->next_serial;
	struct msg m;
	begin(&m, OP_CANVAS_COMMIT);
	put_u32(&m, canvas);
	put_u32(&m, serial);
	put_u32(&m, width);
	put_u32(&m, height);
	if (finish(d, &m, 0) < 0 || cg_flush(d) < 0) return -1;
	return (int64_t)serial;
}

int64_t cg_canvas_present(cg_display *d, uint32_t canvas, const void *pixels, uint32_t stride, uint32_t width,
                          uint32_t height, const cg_rect *damage, int n_damage)
{
	if (n_damage <= 0) {
		if (cg_canvas_update(d, canvas, pixels, stride, 0, 0, width, height) < 0) return -1;
	} else {
		for (int i = 0; i < n_damage; i++) {
			cg_rect r = damage[i];
			if (r.x >= width || r.y >= height) continue;
			if (r.width > width - r.x) r.width = width - r.x;
			if (r.height > height - r.y) r.height = height - r.y;
			if (cg_canvas_update(d, canvas, pixels, stride, r.x, r.y, r.width, r.height) < 0) return -1;
		}
	}
	return cg_canvas_commit(d, canvas, width, height);
}

/* ── clipboard ───────────────────────────────────────────────────────────────────────────── */

int cg_clipboard_set(cg_display *d, int n, const char *const *mimes, const void *const *data, const size_t *lengths)
{
	struct msg m;
	begin(&m, OP_CLIPBOARD_SET);
	put_u32(&m, n < 0 ? 0 : (uint32_t)n);
	for (int i = 0; i < n; i++) {
		put_str(&m, mimes[i]);
		put_bytes(&m, data[i], lengths[i]);
	}
	return finish(d, &m, 0);
}

int cg_clipboard_set_text(cg_display *d, const char *utf8)
{
	const char *mimes[] = {"text/plain;charset=utf-8", "UTF8_STRING"};
	const void *data[] = {utf8, utf8};
	size_t len[] = {strlen(utf8), strlen(utf8)};
	return cg_clipboard_set(d, 2, mimes, data, len);
}

uint32_t cg_clipboard_request(cg_display *d, const char *mime)
{
	uint32_t req = ++d->next_request;
	if (!req) req = ++d->next_request;
	struct msg m;
	begin(&m, OP_CLIPBOARD_GET);
	put_u32(&m, req);
	put_str(&m, mime ? mime : "text/plain");
	return finish(d, &m, 0) < 0 ? 0 : req;
}

/* ── events ──────────────────────────────────────────────────────────────────────────────── */

/* 1: an event for the caller; 0: handled here (PING); -1: malformed. */
static int decode(cg_display *d, uint16_t op, const unsigned char *p, size_t n, cg_event *ev)
{
	struct rd r = {p, n, 0, 0};
	size_t l;
	const unsigned char *s;
	memset(ev, 0, sizeof *ev);
	ev->text = "";
	ev->mime = "";
	switch (op) {
	case OP_PING:
		/* Answered with the next commit, or when the program next waits for events: then
		 * whatever the events before the PING changed is on the canvas too. */
		d->pong_due = get_u32(&r);
		return r.bad ? -1 : 0;
	case OP_CONFIGURE: {
		ev->type = CG_EVENT_CONFIGURE;
		ev->canvas = get_u32(&r);
		ev->width = get_u32(&r);
		ev->height = get_u32(&r);
		struct canvas *c = find(d, ev->canvas);
		if (c && !r.bad) {
			c->w = ev->width;
			c->h = ev->height;
		}
		break;
	}
	case OP_FRAME_DONE:
		ev->type = CG_EVENT_FRAME_DONE;
		ev->canvas = get_u32(&r);
		ev->serial = get_u32(&r);
		break;
	case OP_CLOSE:
		ev->type = CG_EVENT_CLOSE;
		ev->canvas = get_u32(&r);
		break;
	case OP_FOCUS:
		ev->type = CG_EVENT_FOCUS;
		ev->canvas = get_u32(&r);
		ev->focused = get_u32(&r);
		break;
	case OP_POINTER_MOTION:
		ev->type = CG_EVENT_POINTER_MOTION;
		ev->canvas = get_u32(&r);
		ev->x = (int32_t)get_u32(&r);
		ev->y = (int32_t)get_u32(&r);
		ev->modifiers = get_u32(&r);
		ev->buttons = get_u32(&r);
		break;
	case OP_POINTER_BUTTON:
		ev->type = CG_EVENT_POINTER_BUTTON;
		ev->canvas = get_u32(&r);
		ev->x = (int32_t)get_u32(&r);
		ev->y = (int32_t)get_u32(&r);
		ev->button = get_u32(&r);
		ev->pressed = get_u32(&r);
		ev->modifiers = get_u32(&r);
		ev->buttons = get_u32(&r);
		break;
	case OP_SCROLL:
		ev->type = CG_EVENT_SCROLL;
		ev->canvas = get_u32(&r);
		ev->x = (int32_t)get_u32(&r);
		ev->y = (int32_t)get_u32(&r);
		ev->dx = (int32_t)get_u32(&r);
		ev->dy = (int32_t)get_u32(&r);
		ev->steps_x = (int32_t)get_u32(&r);
		ev->steps_y = (int32_t)get_u32(&r);
		ev->modifiers = get_u32(&r);
		break;
	case OP_KEY:
		ev->type = CG_EVENT_KEY;
		ev->canvas = get_u32(&r);
		ev->keysym = get_u32(&r);
		ev->keycode = get_u32(&r);
		ev->pressed = get_u32(&r);
		ev->modifiers = get_u32(&r);
		s = get_bytes(&r, &l);
		ev->text = own_string(&d->text, &d->text_cap, s, l);
		break;
	case OP_POINTER_ENTER:
		ev->type = CG_EVENT_POINTER_ENTER;
		ev->canvas = get_u32(&r);
		ev->x = (int32_t)get_u32(&r);
		ev->y = (int32_t)get_u32(&r);
		ev->modifiers = get_u32(&r);
		ev->buttons = get_u32(&r);
		break;
	case OP_POINTER_LEAVE:
		ev->type = CG_EVENT_POINTER_LEAVE;
		ev->canvas = get_u32(&r);
		break;
	case OP_PREEDIT:
		ev->type = CG_EVENT_PREEDIT;
		ev->canvas = get_u32(&r);
		s = get_bytes(&r, &l);
		ev->text = own_string(&d->text, &d->text_cap, s, l);
		ev->cursor_begin = (int32_t)get_u32(&r);
		ev->cursor_end = (int32_t)get_u32(&r);
		break;
	case OP_TEXT:
		ev->type = CG_EVENT_TEXT;
		ev->canvas = get_u32(&r);
		s = get_bytes(&r, &l);
		ev->text = own_string(&d->text, &d->text_cap, s, l);
		break;
	case OP_CLIPBOARD_CHANGED: {
		ev->type = CG_EVENT_CLIPBOARD_CHANGED;
		uint32_t count = get_u32(&r);
		/* The types, one per line: built in place in the text buffer. */
		size_t used = 0;
		own_string(&d->text, &d->text_cap, (const unsigned char *)"", 0);
		for (uint32_t i = 0; i < count && !r.bad; i++) {
			s = get_bytes(&r, &l);
			if (used + l + 2 > d->text_cap) {
				char *b = realloc(d->text, used + l + 256);
				if (!b) break;
				d->text = b;
				d->text_cap = used + l + 256;
			}
			if (i) d->text[used++] = '\n';
			memcpy(d->text + used, s, l);
			used += l;
			d->text[used] = 0;
		}
		ev->text = d->text ? d->text : "";
		break;
	}
	case OP_CLIPBOARD_DATA:
		ev->type = CG_EVENT_CLIPBOARD_DATA;
		ev->request = get_u32(&r);
		ev->found = get_u32(&r);
		s = get_bytes(&r, &l);
		ev->mime = own_string(&d->mime, &d->mime_cap, s, l);
		ev->data = get_bytes(&r, &ev->length);
		break;
	case OP_ERROR:
		ev->type = CG_EVENT_ERROR;
		ev->code = get_u32(&r);
		s = get_bytes(&r, &l);
		ev->text = own_string(&d->text, &d->text_cap, s, l);
		snprintf(d->err, sizeof d->err, "%s", ev->text);
		break;
	default:
		return 0; /* from a newer server: skip */
	}
	return r.bad ? -1 : 1;
}

int cg_next_event(cg_display *d, cg_event *ev, int timeout_ms)
{
	int64_t deadline = timeout_ms < 0 ? -1 : now_ms() + timeout_ms;
	drop_pending(d);
	for (;;) {
		uint16_t op;
		const unsigned char *p;
		size_t n;
		int got = message_at(d, 0, &op, &p, &n);
		if (got < 0) return -1;
		if (got) {
			d->drop = HEADER + n;
			int r = decode(d, op, p, n, ev);
			if (r < 0) {
				d->broken = 1;
				return fail(d, "a malformed message (0x%04x) from the display server", op);
			}
			if (r > 0) return 1;
			drop_pending(d);
			continue;
		}
		/* About to wait: the program has handled (and drawn) what came before. */
		if (timeout_ms != 0 && cg_flush(d) < 0) return -1;
		int rr = read_more(d, remaining(deadline));
		if (rr < 0) return -1;
		if (rr == 0) return 0;
	}
}

int cg_keysym_to_utf8(uint32_t keysym, char buf[8])
{
	uint32_t cp;
	if ((keysym >= 0x20 && keysym < 0x7f) || (keysym >= 0xa0 && keysym < 0x100)) cp = keysym;
	else if ((keysym & 0xff000000u) == 0x01000000u) cp = keysym & 0x00ffffffu;
	else return 0;
	if (cp > 0x10ffff || (cp >= 0xd800 && cp < 0xe000)) return 0;
	int n;
	if (cp < 0x80) { buf[0] = (char)cp; n = 1; }
	else if (cp < 0x800) { buf[0] = (char)(0xc0 | cp >> 6); buf[1] = (char)(0x80 | (cp & 0x3f)); n = 2; }
	else if (cp < 0x10000) {
		buf[0] = (char)(0xe0 | cp >> 12);
		buf[1] = (char)(0x80 | ((cp >> 6) & 0x3f));
		buf[2] = (char)(0x80 | (cp & 0x3f));
		n = 3;
	} else {
		buf[0] = (char)(0xf0 | cp >> 18);
		buf[1] = (char)(0x80 | ((cp >> 12) & 0x3f));
		buf[2] = (char)(0x80 | ((cp >> 6) & 0x3f));
		buf[3] = (char)(0x80 | (cp & 0x3f));
		n = 4;
	}
	buf[n] = 0;
	return n;
}
