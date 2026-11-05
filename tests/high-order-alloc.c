/*
 * Puts the guest kernel's memory in the state a long agent session leaves it in, for
 * tests/test_high_order_alloc.py (kernel/patches/nommu-high-order-alloc.patch).
 *
 *   high-order-alloc fragment DIR FILE PAGES
 *       Writes a 64 KiB file in DIR (tmpfs, which is ramfs here) and reads one 64 KiB page of FILE
 *       (a shared folder: clean page cache) by turns, PAGES times, so their pages alternate in
 *       memory and fill it; reads FILE's pages again in a random order (they become active in that
 *       order, so reclaim later takes them at random addresses); deletes the files. The free memory
 *       is then gigabytes of single pages between cached ones, and nothing larger.
 *
 *   high-order-alloc hold N
 *       Takes up to N free 256 KiB (order-2) blocks and keeps them while it sleeps: one unix
 *       datagram socket pair per block, each with an unread 140 KB message, whose linear data is a
 *       256 KiB kmalloc (GFP_KERNEL: it may reclaim, or call the OOM killer). Prints how many.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static char page[65536];

static int read_page(int fd, long i)
{
	if (pread(fd, page, sizeof page, i * (off_t)sizeof page) == sizeof page)
		return 0;
	perror("pread");
	return 1;
}

static int fragment(const char *dir, const char *file, long n)
{
	char name[4096];
	int cached = open(file, O_RDONLY);
	long *order = malloc(n * sizeof *order);

	if (cached < 0 || !order) {
		perror(file);
		return 1;
	}
	/* one page per read: readahead would put runs of the file next to each other */
	posix_fadvise(cached, 0, 0, POSIX_FADV_RANDOM);
	for (long i = 0; i < n; i++) {
		snprintf(name, sizeof name, "%s/f%ld", dir, i);
		int fd = open(name, O_WRONLY | O_CREAT | O_TRUNC, 0600);
		if (fd < 0 || write(fd, page, sizeof page) != sizeof page) {
			perror(name);
			return 1;
		}
		close(fd);
		if (read_page(cached, i))
			return 1;
	}
	for (long i = 0; i < n; i++)
		order[i] = i;
	srandom(12345);
	for (long i = n - 1; i > 0; i--) {
		long j = random() % (i + 1), t = order[i];
		order[i] = order[j];
		order[j] = t;
	}
	for (long i = 0; i < n; i++)
		if (read_page(cached, order[i]))
			return 1;
	for (long i = 0; i < n; i++) {
		snprintf(name, sizeof name, "%s/f%ld", dir, i);
		unlink(name);
	}
	printf("fragmented: %ld pages\n", n);
	return 0;
}

static int hold(long n)
{
	static char message[140000];
	long held = 0;

	for (; held < n; held++) {
		int pair[2];
		if (socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) ||
		    send(pair[0], message, sizeof message, MSG_DONTWAIT) != sizeof message) {
			perror("hold");
			break;
		}
	}
	printf("held=%ld\n", held);
	fflush(stdout);
	pause();
	return 0;
}

int main(int argc, char **argv)
{
	if (argc == 5 && !strcmp(argv[1], "fragment"))
		return fragment(argv[2], argv[3], atol(argv[4]));
	if (argc == 3 && !strcmp(argv[1], "hold"))
		return hold(atol(argv[2]));
	fprintf(stderr, "usage: high-order-alloc fragment DIR FILE PAGES | hold N\n");
	return 2;
}
