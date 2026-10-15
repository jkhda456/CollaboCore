/* The program of tests/test_dlopen.py: dlopen in the guest. Each check prints one line. */
#include <dlfcn.h>
#include <fenv.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int program_counter = 100;
int program_add(int a, int b) { return a + b; }
int program_weak(void) { return 2; }    /* the library has a weak one: this one wins */
int (*program_callback)(int);

static void *lib;
static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int loaded;

#define SYM(T, name) ((T)dlsym(lib, name))

static void *count_in_thread(void *p) {
	int (*count)(void) = SYM(int (*)(void), "lib_count");
	int a = count(), b = count();
	return (void *)(long)(a * 10 + b);
}

/* A thread that exists before the library: it gets to it through a mutex. */
static void *early(void *p) {
	pthread_mutex_lock(&m);
	while (!loaded) pthread_cond_wait(&cv, &m);
	pthread_mutex_unlock(&m);
	int (*entry)(int) = SYM(int (*)(int), "lib_entry");
	return (void *)(long)entry(1);
}

/* A thread that exists when another gets a pointer to a library's function from dlsym (which
 * gives the function a table slot), and is handed it through a mutex. */
static pthread_mutex_t handoff_m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t handoff_cv = PTHREAD_COND_INITIALIZER;
static int (*handoff)(void);

static void *receive(void *p) {
	pthread_mutex_lock(&handoff_m);
	while (!handoff) pthread_cond_wait(&handoff_cv, &handoff_m);
	int (*f)(void) = handoff;
	pthread_mutex_unlock(&handoff_m);
	return (void *)(long)f();
}

/* Two threads that load a library each at once: the first top.so, which needs libdep.so, whose
 * constructor holds the C library's lock a while; the second, meanwhile, a copy of lib.so (another
 * library), waiting for the lock: it must catch up on what the first loaded (top.so, with
 * thread-local storage) before it goes on. Then each library's thread-local storage must be a
 * block of its own in either thread. */
static const char *storage_symbol[2] = {"top_tls_address", "lib_tls_address"};
struct concurrent { const char *path; void *handle; int *storage[2]; };
static struct concurrent concurrent[2];
static pthread_barrier_t barrier;

static void *load_at_once(void *p) {
	struct concurrent *c = p;
	pthread_barrier_wait(&barrier);
	if (c == &concurrent[1]) usleep(100000);   /* the first is in libdep.so's constructor by then */
	c->handle = dlopen(c->path, RTLD_NOW);
	pthread_barrier_wait(&barrier);
	for (int i = 0; i < 2; i++) {
		int *(*address)(void) = concurrent[i].handle ? (int *(*)(void))dlsym(concurrent[i].handle, storage_symbol[i]) : 0;
		c->storage[i] = address ? address() : 0;
	}
	return 0;
}

/* library i's storage in both threads: blocks of their own (not the null base of storage never
 * set up, not one shared), never used yet */
static int concurrent_ok(int i) {
	int *a = concurrent[0].storage[i], *b = concurrent[1].storage[i];
	return a && b && a != b && (uintptr_t)a >= 4096 && (uintptr_t)b >= 4096 && !*a && !*b;
}

int main(int argc, char **argv) {
	setvbuf(stdout, 0, _IONBF, 0);
	pthread_t before;
	pthread_create(&before, 0, early, 0);
	usleep(100000);

	printf("missing: %s\n", dlopen("/nonexistent.so", RTLD_NOW) ? "loaded?!" : (strstr(dlerror(), "cannot open") ? "error ok" : "bad error"));
	lib = dlopen(argv[1], RTLD_NOW);
	if (!lib) { printf("dlopen failed: %s\n", dlerror()); return 1; }
	printf("again: %s\n", dlopen(argv[1], RTLD_NOW) == lib ? "same handle" : "another handle");
	pthread_mutex_lock(&m);
	loaded = 1;
	pthread_cond_broadcast(&cv);
	pthread_mutex_unlock(&m);

	printf("entry: %d\n", SYM(int (*)(int), "lib_entry")(5));
	printf("data: %d\n", *SYM(int *, "lib_data"));
	printf("constructor: %d\n", SYM(int (*)(void), "lib_constructed")());
	printf("own pointer: %d\n", (*SYM(int (**)(int), "lib_pointer"))(21));
	printf("program pointer equal: %d\n", SYM(int (*)(void), "lib_compare")());
	SYM(int (*)(void), "lib_install")();
	printf("callback: %d\n", program_callback(8));
	printf("setjmp: %d\n", SYM(int (*)(void), "lib_setjmp")());
	char *s = SYM(char *(*)(int), "lib_format")(3);
	printf("format: %s\n", s);
	free(s);
	printf("weak: %d %d %d\n", SYM(int (*)(void), "lib_weak_calls")(), SYM(int (*)(void), "lib_weak_pointer")(),
	       SYM(int (*)(void), "lib_absent")());
	printf("missing symbol: %s\n", dlsym(lib, "nope") ? "found?!" : "error ok");
	printf("default handle: %s\n", dlsym(RTLD_DEFAULT, "program_add") == (void *)program_add ? "program_add" : "other");

	int (*count)(void) = SYM(int (*)(void), "lib_count");
	int c1 = count(), c2 = count();
	pthread_t t;
	void *r;
	pthread_create(&t, 0, count_in_thread, 0);
	pthread_join(t, &r);
	printf("tls: main %d %d, thread %ld\n", c1, c2, (long)r);
	pthread_join(before, &r);
	printf("early thread: %ld\n", (long)r);

	pthread_t receiver;
	pthread_create(&receiver, 0, receive, 0);
	usleep(50000);                       /* it waits: it exists before the slot does */
	pthread_mutex_lock(&handoff_m);
	handoff = SYM(int (*)(void), "lib_handoff");
	pthread_cond_signal(&handoff_cv);
	pthread_mutex_unlock(&handoff_m);
	pthread_join(receiver, &r);
	printf("handoff: %ld\n", (long)r);

	feclearexcept(FE_ALL_EXCEPT);
	SYM(int (*)(void), "lib_raise")();
	printf("fenv: %s\n", fetestexcept(FE_DIVBYZERO) ? "divbyzero" : "none");

	if (argc > 3) {
		pthread_t loaders[2];
		pthread_barrier_init(&barrier, 0, 2);
		setenv("DLOPEN_TEST_SLOW", "1", 1);
		for (int i = 0; i < 2; i++) {
			concurrent[i].path = argv[2 + i];
			pthread_create(&loaders[i], 0, load_at_once, &concurrent[i]);
		}
		for (int i = 0; i < 2; i++) pthread_join(loaders[i], 0);
		int (*top_value)(void) = concurrent[0].handle ? (int (*)(void))dlsym(concurrent[0].handle, "top_value") : 0;
		printf("needed: %d\n", top_value ? top_value() : -1);
		printf("concurrent loads: %s %s\n", concurrent_ok(0) ? "ok" : "bad", concurrent_ok(1) ? "ok" : "bad");
	}
	printf("done\n");
	return 0;
}
