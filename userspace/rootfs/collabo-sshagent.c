// collabo-sshagent: the guest end of the host's ssh-agent.
//
//   collabo-sshagent [SOCKET]      (default /run/collabo/ssh-agent.sock)
//
// Listens on a unix socket, where SSH_AUTH_SOCK points, and carries each connection to the host
// over vsock (port 1082). The engine passes the agent protocol on to the ssh-agent of the
// computer the app runs on — under the app's `sshAgent` policy, which may ask the user before
// each signature, and which only lets listing keys and signing through (no adding, removing or
// locking). The private keys never enter the sandbox. /etc/rc starts this when the kernel
// command line says collabo.sshagent=1.
//
// No fork on wasm Linux: one thread per connection.
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#ifndef AF_VSOCK
#define AF_VSOCK 40
#endif
#define VMADDR_CID_HOST 2
#define PORT 1082

// <linux/vm_sockets.h> conflicts with musl's socket headers; this is the same 16-byte layout.
struct vsock_addr {
	unsigned short family;
	unsigned short reserved1;
	unsigned int port;
	unsigned int cid;
	unsigned char flags;
	unsigned char zero[3];
};

// Copies what one side sends to the other until either closes.
static void *relay(void *arg)
{
	int client = (int)(long)arg;
	struct vsock_addr addr = { .family = AF_VSOCK, .cid = VMADDR_CID_HOST, .port = PORT };
	int host = socket(AF_VSOCK, SOCK_STREAM, 0);
	if (host < 0 || connect(host, (struct sockaddr *)&addr, sizeof addr) < 0) {
		fprintf(stderr, "collabo-sshagent: cannot reach the host on vsock port %d: %s\n", PORT, strerror(errno));
		if (host >= 0)
			close(host);
		close(client);
		return NULL;
	}
	struct pollfd fds[2] = { { .fd = client, .events = POLLIN }, { .fd = host, .events = POLLIN } };
	char buffer[16384];
	for (;;) {
		if (poll(fds, 2, -1) < 0) {
			if (errno == EINTR)
				continue;
			break;
		}
		int from = (fds[0].revents & (POLLIN | POLLHUP | POLLERR)) ? 0 : 1;
		if (!(fds[from].revents & (POLLIN | POLLHUP | POLLERR)))
			continue;
		ssize_t n = read(fds[from].fd, buffer, sizeof buffer);
		if (n <= 0)
			break;
		for (ssize_t done = 0; done < n;) {
			ssize_t w = write(fds[1 - from].fd, buffer + done, n - done);
			if (w <= 0)
				goto out;
			done += w;
		}
	}
out:
	close(host);
	close(client);
	return NULL;
}

int main(int argc, char **argv)
{
	const char *path = argc > 1 ? argv[1] : "/run/collabo/ssh-agent.sock";
	signal(SIGPIPE, SIG_IGN);

	struct sockaddr_un addr = { .sun_family = AF_UNIX };
	if (strlen(path) >= sizeof addr.sun_path) {
		fprintf(stderr, "collabo-sshagent: socket path too long\n");
		return 1;
	}
	strcpy(addr.sun_path, path);
	// Its directory, as `mkdir -p` would, owner-only like ssh-agent's.
	char dir[sizeof addr.sun_path];
	strcpy(dir, path);
	for (char *p = dir + 1; (p = strchr(p, '/')); p++) {
		*p = 0;
		mkdir(dir, 0700);
		*p = '/';
	}
	unlink(path);

	int listener = socket(AF_UNIX, SOCK_STREAM, 0);
	if (listener < 0 || bind(listener, (struct sockaddr *)&addr, sizeof addr) < 0 || listen(listener, 16) < 0) {
		fprintf(stderr, "collabo-sshagent: %s: %s\n", path, strerror(errno));
		return 1;
	}
	chmod(path, 0600);

	for (;;) {
		int client = accept(listener, NULL, NULL);
		if (client < 0) {
			if (errno == EINTR)
				continue;
			perror("collabo-sshagent: accept");
			sleep(1);
			continue;
		}
		pthread_t thread;
		pthread_attr_t attr;
		pthread_attr_init(&attr);
		pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
		pthread_attr_setstacksize(&attr, 64 * 1024);
		if (pthread_create(&thread, &attr, relay, (void *)(long)client) != 0) {
			fprintf(stderr, "collabo-sshagent: no thread for a connection\n");
			close(client);
		}
		pthread_attr_destroy(&attr);
	}
}
