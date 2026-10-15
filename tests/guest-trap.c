/* A guest program that traps on purpose (tests/test_traps.py): a trap ends the program that
 * made it, with SIGSEGV, and the machine goes on (addons/mod/0009 for the native engine); a
 * thread's trap ends its whole process, not that thread alone (addons/mod/0010, the kernel).
 *
 *   guest-trap ok                a thread that returns, joined: exits 0 (no trap)
 *   guest-trap unreachable       `unreachable` (abort paths, failed assertions)
 *   guest-trap indirect          a call through a pointer of another function type
 *   guest-trap oob               a load past the end of the memory
 *   guest-trap stack             runaway recursion
 *   guest-trap thread            a second thread traps while the main thread waits in read()
 *   guest-trap main-with-thread  the main thread traps while a second thread waits in read()
 *   guest-trap signal            a signal handler traps (it runs inside a system call)
 *   guest-trap siginfo           an SA_SIGINFO handler traps
 *
 * The waits are on a pipe nobody writes to: before 0010 the threads that did not trap waited
 * there forever, and the process never ended.
 */
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int pipe_fds[2];

static void say(const char *what)
{
	puts(what);
	fflush(stdout);
}

static void wait_forever(void)
{
	char c;
	read(pipe_fds[0], &c, 1);
	say("woke up: the process did not end");
}

static void *returns(void *arg)
{
	return arg;
}

static void *traps_soon(void *arg)
{
	(void)arg;
	usleep(200 * 1000);
	say("thread trapping");
	__builtin_trap();
}

static void *waits(void *arg)
{
	(void)arg;
	wait_forever();
	return 0;
}

static int one_parameter(int a)
{
	return a + 1;
}

/* Nothing on the C stack in linear memory: every level is a frame of the engine's native wasm
 * stack, until that runs out (the trap a deep JavaScript or regex recursion makes) */
__attribute__((noinline)) static int recurse(int depth)
{
	if (depth < 0)
		return 0;
	int below = recurse(depth + 1);
	__asm__ volatile("" : "+r"(below)); /* not a tail call: the frame stays */
	return below + depth;
}

static void trapping_handler(int sig)
{
	(void)sig;
	say("handler trapping");
	__builtin_trap();
}

static void trapping_siginfo_handler(int sig, siginfo_t *info, void *context)
{
	(void)context;
	printf("siginfo handler trapping (signal %d, code %d)\n", sig, info->si_code);
	fflush(stdout);
	__builtin_trap();
}

int main(int argc, char **argv)
{
	const char *mode = argc > 1 ? argv[1] : "";
	pthread_t thread;
	if (pipe(pipe_fds))
		return 2;

	if (!strcmp(mode, "ok")) {
		void *result = 0;
		pthread_create(&thread, 0, returns, (void *)42);
		pthread_join(thread, &result);
		printf("joined %ld\n", (long)result);
		return 0;
	}
	if (!strcmp(mode, "unreachable"))
		__builtin_trap();
	if (!strcmp(mode, "indirect")) {
		void *volatile erased = (void *)one_parameter; /* the compiler cannot see what it calls */
		int (*two)(int, int) = (int (*)(int, int))erased;
		return two(1, 2);
	}
	if (!strcmp(mode, "oob")) {
		volatile char *past_the_end = (volatile char *)0xfffffff0u;
		return *past_the_end;
	}
	if (!strcmp(mode, "stack"))
		return recurse(0);
	if (!strcmp(mode, "thread")) {
		pthread_create(&thread, 0, traps_soon, 0);
		wait_forever();
		return 1;
	}
	if (!strcmp(mode, "main-with-thread")) {
		pthread_create(&thread, 0, waits, 0);
		usleep(200 * 1000);
		say("main trapping");
		__builtin_trap();
	}
	if (!strcmp(mode, "signal")) {
		signal(SIGUSR1, trapping_handler);
		raise(SIGUSR1);
		say("handler returned");
		return 1;
	}
	if (!strcmp(mode, "siginfo")) {
		struct sigaction action;
		memset(&action, 0, sizeof action);
		action.sa_sigaction = trapping_siginfo_handler;
		action.sa_flags = SA_SIGINFO;
		sigaction(SIGUSR2, &action, 0);
		raise(SIGUSR2);
		say("handler returned");
		return 1;
	}
	fprintf(stderr, "usage: guest-trap ok|unreachable|indirect|oob|stack|thread|main-with-thread|signal|siginfo\n");
	return 2;
}
