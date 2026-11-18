/* A program with a library loaded that writes past its memory (tests/test_dlopen.py), in its own
 * code or in the library's: it ends with SIGSEGV, as a program without libraries does
 * (addons/mod/0009), and the machine goes on. wasmtime 37 asserted that one instance of the store
 * claims a faulting address, while the shared memory is defined by an instance per import — the
 * program's and each library's — so the engine aborted
 * (engine/patches/wasmtime-37.0.3-shared-memory-fault.patch).
 *
 *   fault LIB program|library
 */
#include <dlfcn.h>
#include <stdio.h>

int main(int argc, char **argv)
{
	if (argc != 3) return 2;
	void *h = dlopen(argv[1], RTLD_NOW);
	if (!h) {
		printf("dlopen failed: %s\n", dlerror());
		return 1;
	}
	void (*poke)(volatile int *) = (void (*)(volatile int *))dlsym(h, "lib_poke");
	/* past the memory's size, within the 4 GiB it may grow to */
	volatile int *past = (volatile int *)0xF0000000u;
	printf("fault: in the %s\n", argv[2]);
	fflush(stdout);
	if (argv[2][0] == 'l')
		poke(past);
	else
		*past = 1;
	printf("not reached\n");
	return 0;
}
