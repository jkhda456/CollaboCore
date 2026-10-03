/* Futexes of two processes at the same address are two futexes (tests/run.sh --boot).
 * `futex-procs wait` waits on its word for 3 s; `futex-procs wake`, started after it, wakes its
 * own word at the same address. The kernel has no MMU and keyed futexes by address alone, so the
 * wake took the other process's waiter (addons/mod/0008 puts the process in the key). */
#include <errno.h>
#include <linux/futex.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int word;

int main(int argc, char **argv) {
	if (argc == 2 && !strcmp(argv[1], "wait")) {
		struct timespec timeout = { 3, 0 };
		long ret = syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, &timeout, 0, 0);
		printf("futex-wait=%s\n", ret == 0 ? "woken" : errno == ETIMEDOUT ? "timeout" : strerror(errno));
		return 0;
	}
	if (argc == 2 && !strcmp(argv[1], "wake")) {
		printf("futex-woke=%ld\n", syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0));
		return 0;
	}
	fprintf(stderr, "usage: futex-procs wait|wake\n");
	return 2;
}
