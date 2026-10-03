/* sbrk up to the end of the 4 GiB wasm32 memory (tests/run.sh --boot): all 65536 pages come
 * in, the bytes left below 4 GiB are still handed out, and a step past 4 GiB is refused, not
 * wrapped around to low addresses (userspace/patches/musl-brk-4gib.patch; musl's brk added
 * pointers, and clang -O2 dropped the overflow check, so the loop below never ended). */
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
	/* Big steps, then smaller ones: each size goes on until the memory refuses it. */
	for (intptr_t step = 256 << 20; step >= 16; step >>= 4)
		for (long n = 0; sbrk(step) != (void *)-1; n++)
			if (n > 4096) {
				printf("brk-runaway step=%ld\n", (long)step);
				return 1;
			}
	void *end = sbrk(0);
	unsigned long pages = __builtin_wasm_memory_size(0);
	uintptr_t left = UINTPTR_MAX - (uintptr_t)end;
	printf("brk-pages=%lu brk-left=%lu\n", pages, (unsigned long)left);
	return end != (void *)-1 && pages == 65536 && left < 16 ? 0 : 1;
}
