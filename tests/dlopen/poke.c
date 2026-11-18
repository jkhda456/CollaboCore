/* A library function that writes where it is told (tests/dlopen/fault.c). */
void lib_poke(volatile int *p) { *p = 1; }
