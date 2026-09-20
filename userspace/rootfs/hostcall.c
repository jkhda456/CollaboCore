// hostcall: call a function the host application offers to the sandbox (vsock port 1081).
//
//   hostcall FUNCTION ['{"json":"arguments"}']        prints the JSON result; exit 1 on error
//   hostcall --exec [--gui] [--cwd DIR] -- CMD ARG...  run a program on the host (if the host
//                                                       allows it): its output and exit status
//                                                       become ours; --gui starts it detached
//   hostcall --list                                     the functions the host offers
//
// Protocol: one JSON line each way.
//   request   {"fn":"NAME","args":{...}}
//   response  {"ok":true,"result":...}  |  {"ok":false,"error":{"kind":"...","message":"..."}}
// Error kinds include: unknown-function, denied (the host or its user refused), bad-request, failed.
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef AF_VSOCK
#define AF_VSOCK 40
#endif
#define VMADDR_CID_HOST 2
#define PORT 1081

struct vsock_addr {
	unsigned short family, reserved1;
	unsigned int port, cid;
	unsigned char flags, zero[3];
};

struct buf {
	char *d;
	size_t n, cap;
};

static void add(struct buf *b, const char *s, size_t n)
{
	if (b->n + n + 1 > b->cap) {
		b->cap = (b->n + n + 1) * 2;
		b->d = realloc(b->d, b->cap);
		if (!b->d) {
			fputs("hostcall: out of memory\n", stderr);
			exit(1);
		}
	}
	memcpy(b->d + b->n, s, n);
	b->n += n;
	b->d[b->n] = 0;
}
static void adds(struct buf *b, const char *s) { add(b, s, strlen(s)); }

static void add_json_string(struct buf *b, const char *s)
{
	adds(b, "\"");
	for (const unsigned char *p = (const unsigned char *)s; *p; p++) {
		char esc[8];
		if (*p == '"' || *p == '\\') {
			esc[0] = '\\', esc[1] = (char)*p;
			add(b, esc, 2);
		} else if (*p < 0x20) {
			snprintf(esc, sizeof esc, "\\u%04x", *p);
			adds(b, esc);
		} else {
			add(b, (const char *)p, 1);
		}
	}
	adds(b, "\"");
}

// Minimal JSON reading: enough to pick fields out of the host's response.
static const char *skip_ws(const char *p) { while (*p == ' ' || *p == '\n' || *p == '\t' || *p == '\r') p++; return p; }

// Decodes the JSON string starting at p (which points at the opening quote) into out.
static const char *read_string(const char *p, struct buf *out)
{
	if (*p != '"')
		return NULL;
	for (p++; *p && *p != '"'; p++) {
		if (*p != '\\') {
			add(out, p, 1);
			continue;
		}
		p++;
		char c = *p;
		if (c == 'n') add(out, "\n", 1);
		else if (c == 't') add(out, "\t", 1);
		else if (c == 'r') add(out, "\r", 1);
		else if (c == 'b') add(out, "\b", 1);
		else if (c == 'f') add(out, "\f", 1);
		else if (c == 'u') {
			unsigned cp = 0;
			sscanf(p + 1, "%4x", &cp);
			p += 4;
			if (cp >= 0xd800 && cp < 0xdc00 && p[1] == '\\' && p[2] == 'u') {
				unsigned lo = 0;
				sscanf(p + 3, "%4x", &lo);
				cp = 0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00);
				p += 6;
			}
			char u[4];
			int n = 0;
			if (cp < 0x80) u[n++] = (char)cp;
			else if (cp < 0x800) u[n++] = (char)(0xc0 | cp >> 6), u[n++] = (char)(0x80 | (cp & 0x3f));
			else if (cp < 0x10000) u[n++] = (char)(0xe0 | cp >> 12), u[n++] = (char)(0x80 | ((cp >> 6) & 0x3f)), u[n++] = (char)(0x80 | (cp & 0x3f));
			else u[n++] = (char)(0xf0 | cp >> 18), u[n++] = (char)(0x80 | ((cp >> 12) & 0x3f)), u[n++] = (char)(0x80 | ((cp >> 6) & 0x3f)), u[n++] = (char)(0x80 | (cp & 0x3f));
			add(out, u, (size_t)n);
		} else add(out, &c, 1);
	}
	return *p == '"' ? p + 1 : NULL;
}

// Finds "key": at the top level of the object at p (good enough for the host's flat responses).
static const char *find_key(const char *obj, const char *key)
{
	char pat[64];
	snprintf(pat, sizeof pat, "\"%s\":", key);
	const char *hit = strstr(obj, pat);
	return hit ? skip_ws(hit + strlen(pat)) : NULL;
}

static char *call(const char *request_line)
{
	int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
	struct vsock_addr a = { .family = AF_VSOCK, .port = PORT, .cid = VMADDR_CID_HOST };
	if (fd < 0 || connect(fd, (struct sockaddr *)&a, sizeof a) < 0) {
		fprintf(stderr, "hostcall: the host offers no functions to this sandbox (%s)\n", strerror(errno));
		exit(1);
	}
	size_t len = strlen(request_line);
	for (size_t off = 0; off < len;) {
		ssize_t w = write(fd, request_line + off, len - off);
		if (w <= 0) {
			perror("hostcall: write");
			exit(1);
		}
		off += (size_t)w;
	}
	struct buf resp = { 0 };
	char tmp[65536];
	ssize_t r;
	while ((r = read(fd, tmp, sizeof tmp)) > 0) {
		add(&resp, tmp, (size_t)r);
		if (memchr(tmp, '\n', (size_t)r))
			break;
	}
	close(fd);
	if (!resp.n) {
		fputs("hostcall: no answer from the host\n", stderr);
		exit(1);
	}
	return resp.d;
}

static int report_error(const char *resp)
{
	const char *err = find_key(resp, "error");
	struct buf kind = { 0 }, msg = { 0 };
	const char *k = err ? find_key(err, "kind") : NULL, *m = err ? find_key(err, "message") : NULL;
	if (k) read_string(k, &kind);
	if (m) read_string(m, &msg);
	fprintf(stderr, "hostcall: %s: %s\n", kind.d ? kind.d : "error", msg.d ? msg.d : resp);
	return 1;
}

// Prints the value of "result": the rest of the response line minus the object's closing brace.
static void print_result(const char *res)
{
	size_t n = strlen(res);
	while (n && (res[n - 1] == '\n' || res[n - 1] == '\r' || res[n - 1] == ' '))
		n--;
	if (n && res[n - 1] == '}')
		n--;
	fwrite(res, 1, n, stdout);
	fputc('\n', stdout);
}

static void usage(void)
{
	fputs("usage: hostcall FUNCTION [JSON-ARGS]\n"
	      "       hostcall --exec [--gui] [--cwd DIR] -- CMD [ARG...]\n"
	      "       hostcall --list\n", stderr);
	exit(1);
}

int main(int argc, char **argv)
{
	if (argc < 2 || !strcmp(argv[1], "-h") || !strcmp(argv[1], "--help"))
		usage();
	struct buf req = { 0 };

	if (!strcmp(argv[1], "--list")) {
		adds(&req, "{\"fn\":\"list\",\"args\":{}}\n");
		char *resp = call(req.d);
		if (!strstr(resp, "\"ok\":true"))
			return report_error(resp);
		const char *res = find_key(resp, "result");
		print_result(res ? res : resp);
		return 0;
	}

	if (!strcmp(argv[1], "--exec")) {
		int gui = 0, i = 2;
		const char *cwd = NULL;
		for (; i < argc; i++) {
			if (!strcmp(argv[i], "--gui")) gui = 1;
			else if (!strcmp(argv[i], "--cwd") && i + 1 < argc) cwd = argv[++i];
			else if (!strcmp(argv[i], "--")) { i++; break; }
			else break;
		}
		if (i >= argc)
			usage();
		adds(&req, "{\"fn\":\"exec\",\"args\":{\"argv\":[");
		for (int j = i; j < argc; j++) {
			if (j > i) adds(&req, ",");
			add_json_string(&req, argv[j]);
		}
		adds(&req, "]");
		if (gui) adds(&req, ",\"gui\":true");
		if (cwd) adds(&req, ",\"cwd\":"), add_json_string(&req, cwd);
		adds(&req, "}}\n");
		char *resp = call(req.d);
		if (!strstr(resp, "\"ok\":true"))
			return report_error(resp);
		const char *res = find_key(resp, "result");
		if (!res)
			return report_error(resp);
		if (gui) {
			const char *pid = find_key(res, "pid");
			fprintf(stderr, "hostcall: started on the host%s%.*s\n", pid ? ", pid " : "", pid ? (int)strspn(pid, "0123456789") : 0, pid ? pid : "");
			return 0;
		}
		struct buf out = { 0 }, err = { 0 };
		const char *o = find_key(res, "stdout"), *e = find_key(res, "stderr"), *c = find_key(res, "exitCode");
		if (o) read_string(o, &out);
		if (e) read_string(e, &err);
		if (out.n) fwrite(out.d, 1, out.n, stdout);
		if (err.n) fwrite(err.d, 1, err.n, stderr);
		return c ? atoi(c) & 0xff : 0;
	}

	if (argv[1][0] == '-')
		usage();
	adds(&req, "{\"fn\":");
	add_json_string(&req, argv[1]);
	adds(&req, ",\"args\":");
	adds(&req, argc > 2 ? argv[2] : "{}");
	adds(&req, "}\n");
	if (strchr(argc > 2 ? argv[2] : "", '\n')) {
		fputs("hostcall: JSON-ARGS must be on one line\n", stderr);
		return 1;
	}
	char *resp = call(req.d);
	if (!strstr(resp, "\"ok\":true"))
		return report_error(resp);
	const char *res = find_key(resp, "result");
	if (res)
		print_result(res);
	return 0;
}
