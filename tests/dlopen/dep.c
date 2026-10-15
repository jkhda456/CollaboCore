/* A library tests/dlopen/top.c needs (DT_NEEDED): loaded with it, before it. Its constructor
 * holds the C library's lock a while when DLOPEN_TEST_SLOW is set (constructors run under it),
 * for tests/dlopen/main.c's concurrent loads. */
#include <stdlib.h>
#include <unistd.h>

int dep_value(void) { return 5; }

__attribute__((constructor)) static void slow(void) {
	if (getenv("DLOPEN_TEST_SLOW")) usleep(400000);
}
