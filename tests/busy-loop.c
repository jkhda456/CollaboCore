// Programs that run their own code without syscalls (tests/test_preempt.py): what the engine's
// tick (engine/src/user.rs on_epoch, the kernel's user_interrupt) must still interrupt.
//
//   busy-loop spin      a loop with no syscall in it, for ever
//   busy-loop recurse   calls without a loop (a doubly recursive count): only the checks at function
//                       entry see it
//   busy-loop alarm     SIGALRM in a second, its handler ends the spin: "alarm handled", exit 4
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static volatile sig_atomic_t stop;
static volatile unsigned long sink;

static void on_alarm(int sig)
{
	(void)sig;
	stop = 1;
}

__attribute__((noinline)) static unsigned long count(int n)
{
	return n < 2 ? (unsigned long)n : count(n - 1) + count(n - 2);
}

int main(int argc, char **argv)
{
	const char *mode = argc > 1 ? argv[1] : "spin";

	if (!strcmp(mode, "spin")) {
		for (;;)
			sink++;
	}
	if (!strcmp(mode, "recurse")) {
		sink = count(80); // ~10^16 calls: never ends on its own
		return 0;
	}
	if (!strcmp(mode, "alarm")) {
		signal(SIGALRM, on_alarm);
		alarm(1);
		while (!stop)
			sink++;
		printf("alarm handled\n");
		return 4;
	}
	fprintf(stderr, "usage: busy-loop spin|recurse|alarm\n");
	return 2;
}
