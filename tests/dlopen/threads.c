/* Threads started at once while large libraries are loaded (tests/test_dlopen.py): each new
 * thread instantiates every library before it runs (__dl_sync at its start), which the engine
 * does under the process's library log, a host lock (engine/src/dylink.rs). The machine hung
 * when a tick preempted the thread holding it (addons/mod/0037).
 *
 *   threads ROUNDS THREADS LIB...   each library's big_call(i, x), from every thread
 */
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

typedef int (*call_fn)(int, int);
static call_fn calls[16];
static int nlibs;

static void *work(void *arg)
{
	long k = (long)arg;
	int sum = 0;
	for (int i = 0; i < nlibs; i++) sum += calls[i]((int)k * 97 + i, 1);
	return (void *)(long)sum;
}

int main(int argc, char **argv)
{
	if (argc < 4 || argc - 3 > 16) return 2;
	int rounds = atoi(argv[1]), threads = atoi(argv[2]);
	if (threads > 16) return 2;
	for (int i = 3; i < argc; i++) {
		void *h = dlopen(argv[i], RTLD_NOW);
		if (!h) {
			printf("dlopen failed: %s\n", dlerror());
			return 1;
		}
		calls[nlibs++] = (call_fn)dlsym(h, "big_call");
	}
	for (int r = 0; r < rounds; r++) {
		pthread_t t[16];
		for (long k = 0; k < threads; k++)
			if (pthread_create(&t[k], 0, work, (void *)k)) {
				printf("pthread_create failed in round %d\n", r);
				return 1;
			}
		for (int k = 0; k < threads; k++) pthread_join(t[k], 0);
	}
	printf("threads: %d rounds of %d\n", rounds, threads);
	return 0;
}
