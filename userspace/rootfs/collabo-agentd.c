// collabo-agentd: runs commands in the guest for the host (the agent's "exec" tool).
//
// Listens on vsock port 1024. The host opens one connection per command and speaks in frames:
//   frame = type (1 byte) + length (u32 little endian) + payload
//
//   host -> guest   'A' argument (repeat; argv[0] is looked up in PATH)
//                   'E' "NAME=value" environment entry (added to / overriding the agent's own)
//                   'C' working directory
//                   'S' start (empty)                    — everything above must come first
//                   'I' stdin bytes; an empty 'I' closes the child's stdin
//                   'K' signal number (u32) to send to the child
//   guest -> host   '1' stdout bytes, '2' stderr bytes
//                   'X' exit: i32, >= 0 exit code, < 0 killed by signal -n    (then the agent closes)
//                   'F' the command could not be started: error message      (then the agent closes)
//
// There is no fork() here: children are started with posix_spawn, one thread per connection.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef AF_VSOCK
#define AF_VSOCK 40
#endif
#define VMADDR_CID_ANY 0xffffffffu
#define PORT 1024
#define MAX_FRAME (4u << 20)

extern char **environ;

struct vsock_addr {
	unsigned short family, reserved1;
	unsigned int port, cid;
	unsigned char flags, zero[3];
};

static int read_full(int fd, void *buf, size_t n)
{
	char *p = buf;
	while (n) {
		ssize_t r = read(fd, p, n);
		if (r < 0 && errno == EINTR)
			continue;
		if (r <= 0)
			return -1;
		p += r;
		n -= (size_t)r;
	}
	return 0;
}

static int write_full(int fd, const void *buf, size_t n)
{
	const char *p = buf;
	while (n) {
		ssize_t w = write(fd, p, n);
		if (w < 0 && errno == EINTR)
			continue;
		if (w < 0 && errno == EAGAIN) {
			struct pollfd pf = { .fd = fd, .events = POLLOUT };
			poll(&pf, 1, 1000);
			continue;
		}
		if (w <= 0)
			return -1;
		p += w;
		n -= (size_t)w;
	}
	return 0;
}

static int send_frame(int fd, char type, const void *data, uint32_t len)
{
	unsigned char head[5] = { (unsigned char)type, len & 0xff, (len >> 8) & 0xff, (len >> 16) & 0xff, len >> 24 };
	if (write_full(fd, head, 5))
		return -1;
	return len ? write_full(fd, data, len) : 0;
}

// Reads one frame into a malloc'd, NUL-terminated buffer.
static int recv_frame(int fd, char *type, char **data, uint32_t *len)
{
	unsigned char head[5];
	if (read_full(fd, head, 5))
		return -1;
	*type = (char)head[0];
	*len = head[1] | head[2] << 8 | head[3] << 16 | (uint32_t)head[4] << 24;
	if (*len > MAX_FRAME)
		return -1;
	*data = malloc(*len + 1);
	if (!*data)
		return -1;
	if (*len && read_full(fd, *data, *len)) {
		free(*data);
		return -1;
	}
	(*data)[*len] = 0;
	return 0;
}

struct list {
	char **items;
	size_t n;
};

static void push(struct list *l, char *s)
{
	l->items = realloc(l->items, (l->n + 2) * sizeof *l->items);
	l->items[l->n++] = s;
	l->items[l->n] = NULL;
}

static void fail(int fd, const char *what, int err)
{
	char msg[512];
	snprintf(msg, sizeof msg, "%s: %s", what, strerror(err));
	send_frame(fd, 'F', msg, (uint32_t)strlen(msg));
}

// The environment for the child: ours, with the host's entries added or replacing ours.
static char **merge_env(struct list *extra)
{
	struct list env = { 0 };
	for (char **e = environ; *e; e++) {
		size_t k = strcspn(*e, "=");
		int overridden = 0;
		for (size_t i = 0; i < extra->n; i++)
			if (!strncmp(extra->items[i], *e, k + 1))
				overridden = 1;
		if (!overridden)
			push(&env, *e);
	}
	for (size_t i = 0; i < extra->n; i++)
		push(&env, extra->items[i]);
	if (!env.items)
		push(&env, NULL), env.n = 0;
	return env.items;
}

static void *serve(void *arg)
{
	int sock = (int)(intptr_t)arg;
	struct list argv = { 0 }, extra_env = { 0 };
	char *cwd = NULL, type, *data;
	uint32_t len;

	for (;;) {
		if (recv_frame(sock, &type, &data, &len))
			goto out;
		if (type == 'A')
			push(&argv, data);
		else if (type == 'E')
			push(&extra_env, data);
		else if (type == 'C')
			free(cwd), cwd = data;
		else if (type == 'S') {
			free(data);
			break;
		} else {
			free(data); // unknown before start: ignore
		}
	}
	if (!argv.n) {
		fail(sock, "no command", EINVAL);
		goto out;
	}

	int in[2], outp[2], errp[2];
	if (pipe2(in, O_CLOEXEC) || pipe2(outp, O_CLOEXEC) || pipe2(errp, O_CLOEXEC)) {
		fail(sock, "pipe", errno);
		goto out;
	}
	posix_spawn_file_actions_t fa;
	posix_spawn_file_actions_init(&fa);
	posix_spawn_file_actions_adddup2(&fa, in[0], 0);
	posix_spawn_file_actions_adddup2(&fa, outp[1], 1);
	posix_spawn_file_actions_adddup2(&fa, errp[1], 2);
	if (cwd)
		posix_spawn_file_actions_addchdir_np(&fa, cwd);
	posix_spawnattr_t attr;
	posix_spawnattr_init(&attr);
	// A new session: the child is not in the agent's process group, and kill(-pid) reaches its children.
	posix_spawnattr_setflags(&attr, POSIX_SPAWN_SETSID);

	char **env = merge_env(&extra_env);
	pid_t pid;
	int err = posix_spawnp(&pid, argv.items[0], &fa, &attr, argv.items, env);
	posix_spawn_file_actions_destroy(&fa);
	posix_spawnattr_destroy(&attr);
	free(env);
	close(in[0]), close(outp[1]), close(errp[1]);
	if (err) {
		close(in[1]), close(outp[0]), close(errp[0]);
		char what[300];
		snprintf(what, sizeof what, "cannot run %s", argv.items[0]);
		fail(sock, what, err);
		goto out;
	}
	fcntl(in[1], F_SETFL, O_NONBLOCK);

	// Pump: host frames -> stdin/signals, child stdout/stderr -> frames, until the child has exited
	// and its output is drained. Output left in pipes kept open by a background grandchild is not
	// waited for beyond a short grace period after the child itself exits.
	int status = 0, exited = 0, sock_open = 1;
	int fds_out[2] = { outp[0], errp[0] };
	char pending_buf[65536];
	size_t pending = 0;
	int stdin_fd = in[1], close_stdin_after_pending = 0;
	int grace = 0;
	char buf[65536];
	for (;;) {
		struct pollfd p[4];
		int np = 0, i_sock = -1, i_out[2] = { -1, -1 }, i_in = -1;
		if (sock_open && pending == 0)
			i_sock = np, p[np++] = (struct pollfd){ .fd = sock, .events = POLLIN };
		for (int k = 0; k < 2; k++)
			if (fds_out[k] >= 0)
				i_out[k] = np, p[np++] = (struct pollfd){ .fd = fds_out[k], .events = POLLIN };
		if (pending && stdin_fd >= 0)
			i_in = np, p[np++] = (struct pollfd){ .fd = stdin_fd, .events = POLLOUT };
		if (fds_out[0] < 0 && fds_out[1] < 0 && exited)
			break;
		int r = poll(p, np, 100);
		if (r < 0 && errno != EINTR)
			break;

		if (!exited) {
			pid_t w = waitpid(pid, &status, WNOHANG);
			if (w == pid)
				exited = 1;
		} else if (++grace > 20) {
			break; // 2 s after exit: stop waiting for descendants holding the pipes
		}

		for (int k = 0; k < 2; k++) {
			if (i_out[k] < 0 || !(p[i_out[k]].revents & (POLLIN | POLLHUP | POLLERR)))
				continue;
			ssize_t n = read(fds_out[k], buf, sizeof buf);
			if (n > 0) {
				if (send_frame(sock, k ? '2' : '1', buf, (uint32_t)n))
					sock_open = 0;
			} else if (n == 0 || (errno != EINTR && errno != EAGAIN)) {
				close(fds_out[k]);
				fds_out[k] = -1;
			}
		}

		if (i_in >= 0 && (p[i_in].revents & (POLLOUT | POLLERR | POLLHUP))) {
			ssize_t w = write(stdin_fd, pending_buf, pending);
			if (w > 0) {
				memmove(pending_buf, pending_buf + w, pending - (size_t)w);
				pending -= (size_t)w;
			} else if (w < 0 && errno != EAGAIN && errno != EINTR) {
				pending = 0; // child closed its stdin: drop the rest
			}
			if (!pending && close_stdin_after_pending)
				close(stdin_fd), stdin_fd = -1;
		}

		if (i_sock >= 0 && (p[i_sock].revents & (POLLIN | POLLHUP | POLLERR))) {
			if (recv_frame(sock, &type, &data, &len)) {
				sock_open = 0; // host gone: stop the command
				kill(-pid, SIGKILL);
				kill(pid, SIGKILL);
				continue;
			}
			if (type == 'I') {
				if (len == 0) {
					if (pending)
						close_stdin_after_pending = 1;
					else if (stdin_fd >= 0)
						close(stdin_fd), stdin_fd = -1;
				} else if (stdin_fd >= 0) {
					if (len > sizeof pending_buf)
						len = sizeof pending_buf; // the host sends stdin in chunks below this size
					memcpy(pending_buf, data, len);
					pending = len;
				}
			} else if (type == 'K' && len >= 4) {
				uint32_t sig = (unsigned char)data[0] | (unsigned char)data[1] << 8 | (unsigned char)data[2] << 16 | (uint32_t)(unsigned char)data[3] << 24;
				kill(-pid, (int)sig);
				kill(pid, (int)sig);
			}
			free(data);
		}
	}
	if (!exited)
		waitpid(pid, &status, 0);
	if (stdin_fd >= 0)
		close(stdin_fd);
	for (int k = 0; k < 2; k++)
		if (fds_out[k] >= 0)
			close(fds_out[k]);

	int32_t code = WIFEXITED(status) ? WEXITSTATUS(status) : WIFSIGNALED(status) ? -WTERMSIG(status) : -1;
	unsigned char c[4] = { code & 0xff, (code >> 8) & 0xff, (code >> 16) & 0xff, ((uint32_t)code >> 24) & 0xff };
	send_frame(sock, 'X', c, 4);
out:
	for (size_t i = 0; i < argv.n; i++)
		free(argv.items[i]);
	for (size_t i = 0; i < extra_env.n; i++)
		free(extra_env.items[i]);
	free(argv.items);
	free(extra_env.items);
	free(cwd);
	close(sock);
	return NULL;
}

int main(void)
{
	signal(SIGPIPE, SIG_IGN);
	int fd = socket(AF_VSOCK, SOCK_STREAM | SOCK_CLOEXEC, 0);
	struct vsock_addr addr = { .family = AF_VSOCK, .port = PORT, .cid = VMADDR_CID_ANY };
	if (fd < 0 || bind(fd, (struct sockaddr *)&addr, sizeof addr) || listen(fd, 16)) {
		perror("collabo-agentd: vsock");
		return 1;
	}
	fprintf(stderr, "collabo-agentd: listening on vsock port %d\n", PORT);
	for (;;) {
		int c = accept4(fd, NULL, NULL, SOCK_CLOEXEC);
		if (c < 0) {
			if (errno == EINTR || errno == ECONNABORTED)
				continue;
			perror("collabo-agentd: accept");
			sleep(1);
			continue;
		}
		pthread_t t;
		pthread_attr_t a;
		pthread_attr_init(&a);
		pthread_attr_setdetachstate(&a, PTHREAD_CREATE_DETACHED);
		if (pthread_create(&t, &a, serve, (void *)(intptr_t)c))
			close(c);
		pthread_attr_destroy(&a);
	}
}
