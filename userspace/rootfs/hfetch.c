// hfetch: make one HTTP(S) request through the host page, at fetch() level.
//
//   hfetch [-X METHOD] [-H 'Name: value']... [-d DATA | -d @FILE | -d @-] [-o FILE] [-i] [-f] URL
//
// The guest has no TLS and no route to the internet; the host page performs the request in a
// browser and streams the response back over vsock. Wire format: release/app/http-protocol.js.
//
//   -X METHOD   GET (default, or POST when there is a body), HEAD, POST, PUT, PATCH, DELETE, OPTIONS
//   -H 'N: v'   set a request header; only a few are allowed by the host (see the error if not)
//   -d DATA     request body; DATA is literal, @FILE reads a file, @- reads stdin. Bytes are sent as is.
//   -o FILE     write the body to FILE instead of stdout
//   -i          print "STATUS text" and the response headers before the body
//   -f          fail with exit status 22 on HTTP status >= 400 (body is discarded)
//
// Exit status: 0 ok, 1 usage or local error, 2 refused/failed before a response (host error),
// 3 response cut off (the body is incomplete), 22 HTTP error with -f.
//
// Environment: HFETCH_PORT overrides the vsock port (default 1080).
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef AF_VSOCK
#define AF_VSOCK 40
#endif
#define VMADDR_CID_HOST 2
#define DEFAULT_PORT 1080

// <linux/vm_sockets.h> conflicts with musl's socket headers; this is the same 16-byte layout.
struct vsock_addr {
	unsigned short family;
	unsigned short reserved1;
	unsigned int port;
	unsigned int cid;
	unsigned char flags;
	unsigned char zero[3];
};

static void die(int code, const char *fmt, ...) __attribute__((noreturn, format(printf, 2, 3)));
#include <stdarg.h>
static void die(int code, const char *fmt, ...)
{
	va_list ap;
	fputs("hfetch: ", stderr);
	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	fputc('\n', stderr);
	exit(code);
}

// ---- growable byte buffer -----------------------------------------------------------------

struct buf {
	char *data;
	size_t len, cap;
};

static void buf_add(struct buf *b, const void *p, size_t n)
{
	if (b->len + n + 1 > b->cap) {
		size_t cap = b->cap ? b->cap : 1024;
		while (b->len + n + 1 > cap)
			cap *= 2;
		b->data = realloc(b->data, cap);
		if (!b->data)
			die(1, "out of memory");
		b->cap = cap;
	}
	memcpy(b->data + b->len, p, n);
	b->len += n;
	b->data[b->len] = 0;
}

static void buf_str(struct buf *b, const char *s)
{
	buf_add(b, s, strlen(s));
}

static void read_all(struct buf *b, int fd)
{
	char tmp[8192];
	ssize_t n;
	while ((n = read(fd, tmp, sizeof tmp)) != 0) {
		if (n < 0) {
			if (errno == EINTR)
				continue;
			die(1, "read: %s", strerror(errno));
		}
		buf_add(b, tmp, (size_t)n);
	}
}

static void write_all(int fd, const void *p, size_t n, const char *what)
{
	const char *c = p;
	while (n) {
		ssize_t w = write(fd, c, n);
		if (w < 0) {
			if (errno == EINTR)
				continue;
			die(1, "write %s: %s", what, strerror(errno));
		}
		c += w;
		n -= (size_t)w;
	}
}

// ---- buffered reader over the socket ------------------------------------------------------

struct reader {
	int fd;
	unsigned char buf[16384];
	size_t pos, len;
	int eof;
};

static int fill(struct reader *r)
{
	if (r->pos < r->len)
		return 1;
	if (r->eof)
		return 0;
	for (;;) {
		ssize_t n = read(r->fd, r->buf, sizeof r->buf);
		if (n < 0 && errno == EINTR)
			continue;
		if (n <= 0) {
			r->eof = 1;
			return 0;
		}
		r->pos = 0;
		r->len = (size_t)n;
		return 1;
	}
}

// Reads a line without its LF into a buffer. Returns 0 at EOF before any byte.
static int read_line(struct reader *r, struct buf *line)
{
	line->len = 0;
	if (line->data)
		line->data[0] = 0;
	int any = 0;
	while (fill(r)) {
		any = 1;
		unsigned char c = r->buf[r->pos++];
		if (c == '\n')
			return 1;
		buf_add(line, &c, 1);
		if (line->len > 65536)
			die(1, "response line too long");
	}
	return any;
}

// ---- main ---------------------------------------------------------------------------------

static void usage(FILE *to, int code)
{
	fputs("usage: hfetch [-X METHOD] [-H 'Name: value']... [-d DATA|@FILE|@-] [-o FILE] [-i] [-f] URL\n"
	      "  Sends one HTTP(S) request through the host page and prints the response body.\n"
	      "  -i  include status and headers   -f  exit 22 on HTTP status >= 400   -o FILE  write body to FILE\n"
	      "  exit: 0 ok, 1 usage, 2 host refused or failed, 3 response cut off, 22 HTTP error with -f\n", to);
	exit(code);
}

int main(int argc, char **argv)
{
	const char *method = NULL, *url = NULL, *out_path = NULL;
	struct buf headers = { 0 }, body = { 0 };
	int have_body = 0, include = 0, fail_http = 0;

	for (int i = 1; i < argc; i++) {
		const char *a = argv[i];
		if (!strcmp(a, "-X") && i + 1 < argc) {
			method = argv[++i];
		} else if (!strcmp(a, "-H") && i + 1 < argc) {
			const char *h = argv[++i];
			if (strpbrk(h, "\r\n"))
				die(1, "header contains a line break");
			buf_str(&headers, h);
			buf_str(&headers, "\n");
		} else if ((!strcmp(a, "-d") || !strcmp(a, "--data-binary") || !strcmp(a, "--data")) && i + 1 < argc) {
			const char *d = argv[++i];
			have_body = 1;
			body.len = 0;
			if (d[0] == '@') {
				int fd = !strcmp(d, "@-") ? 0 : open(d + 1, O_RDONLY);
				if (fd < 0)
					die(1, "%s: %s", d + 1, strerror(errno));
				read_all(&body, fd);
				if (fd)
					close(fd);
			} else {
				buf_str(&body, d);
			}
		} else if (!strcmp(a, "-o") && i + 1 < argc) {
			out_path = argv[++i];
		} else if (!strcmp(a, "-i")) {
			include = 1;
		} else if (!strcmp(a, "-f")) {
			fail_http = 1;
		} else if (!strcmp(a, "-h") || !strcmp(a, "--help")) {
			usage(stdout, 0);
		} else if (a[0] == '-' && a[1]) {
			usage(stderr, 1);
		} else if (!url) {
			url = a;
		} else {
			usage(stderr, 1);
		}
	}
	if (!url)
		usage(stderr, 1);
	if (strpbrk(url, " \r\n"))
		die(1, "URL contains whitespace");
	if (!method)
		method = have_body ? "POST" : "GET";

	// Request.
	struct buf req = { 0 };
	buf_str(&req, method);
	buf_str(&req, " ");
	buf_str(&req, url);
	buf_str(&req, "\n");
	if (headers.len)
		buf_add(&req, headers.data, headers.len);
	if (body.len) {
		char cl[64];
		snprintf(cl, sizeof cl, "content-length: %zu\n", body.len);
		buf_str(&req, cl);
	}
	buf_str(&req, "\n");
	if (body.len)
		buf_add(&req, body.data, body.len);

	// Connect to the host.
	const char *port_env = getenv("HFETCH_PORT");
	struct vsock_addr addr = { .family = AF_VSOCK, .cid = VMADDR_CID_HOST, .port = port_env ? (unsigned)atoi(port_env) : DEFAULT_PORT };
	int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
	if (fd < 0)
		die(1, "socket(AF_VSOCK): %s (is the host bridge attached?)", strerror(errno));
	if (connect(fd, (struct sockaddr *)&addr, sizeof addr) < 0)
		die(1, "cannot reach the host request bridge on vsock port %u: %s", addr.port, strerror(errno));
	write_all(fd, req.data, req.len, "request");

	// Response head.
	struct reader r = { .fd = fd };
	struct buf line = { 0 };
	if (!read_line(&r, &line))
		die(2, "no response from the host");
	if (!strncmp(line.data, "ERROR ", 6))
		die(2, "%s", line.data + 6);
	int status = atoi(line.data);
	if (status < 100 || status > 599)
		die(1, "malformed response: %s", line.data);

	int out = 1;
	if (out_path) {
		out = open(out_path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
		if (out < 0)
			die(1, "%s: %s", out_path, strerror(errno));
	}
	int discard = fail_http && status >= 400;
	if (include && !discard) {
		write_all(out, line.data, line.len, "output");
		write_all(out, "\n", 1, "output");
	}
	for (;;) {
		if (!read_line(&r, &line))
			die(3, "response cut off in the headers");
		if (line.len == 0)
			break;
		if (include && !discard) {
			write_all(out, line.data, line.len, "output");
			write_all(out, "\n", 1, "output");
		}
	}
	if (include && !discard)
		write_all(out, "\n", 1, "output");

	// Body: <hex length>\n<bytes> ... 0\n then "OK" or "ERROR kind: message".
	for (;;) {
		if (!read_line(&r, &line))
			die(3, "response cut off: the body is incomplete");
		char *end;
		unsigned long n = strtoul(line.data, &end, 16);
		if (end == line.data || *end)
			die(1, "malformed chunk length: %s", line.data);
		if (n == 0)
			break;
		while (n) {
			if (!fill(&r))
				die(3, "response cut off: the body is incomplete");
			size_t take = r.len - r.pos < n ? r.len - r.pos : n;
			if (!discard)
				write_all(out, r.buf + r.pos, take, "output");
			r.pos += take;
			n -= take;
		}
	}
	if (!read_line(&r, &line))
		die(3, "response cut off: the body is incomplete");
	if (strcmp(line.data, "OK"))
		die(3, "%s", !strncmp(line.data, "ERROR ", 6) ? line.data + 6 : line.data);
	if (discard)
		die(22, "HTTP %d", status);
	return 0;
}
