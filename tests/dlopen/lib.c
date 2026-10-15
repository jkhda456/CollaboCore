/* A shared library for tests/test_dlopen.py: what a library uses of the program that loads it
 * (functions, data, the C library), its own data, thread-local storage, a constructor, setjmp,
 * function pointers in both directions, the floating-point flags, weak definitions (its own, and
 * one the program has too) and a weak symbol nothing defines. */
#include <fenv.h>
#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

extern int program_counter;                   /* data in the program */
extern int program_add(int, int);             /* a function in the program */
extern int (*program_callback)(int);          /* a pointer the program calls */

int lib_data = 1234;
static _Thread_local int calls;               /* per thread */
static int constructed;
static jmp_buf jump;

/* (through a volatile: clang would run a constructor that stores a constant itself) */
static volatile int answer = 42;
__attribute__((constructor)) static void construct(void) { constructed = answer; }

static int twice(int x) { return 2 * x; }
int (*lib_pointer)(int) = twice;              /* a pointer to its own function, in its data */
int (*lib_program_add)(int, int) = program_add;  /* to the program's, in its data */

int lib_entry(int x) { return program_add(x, program_counter) + lib_data; }
int lib_constructed(void) { return constructed; }
int lib_count(void) { return ++calls; }
int *lib_tls_address(void) { return &calls; }
int lib_handoff(void) { return 77; }   /* dlsym'd in one thread, called in another */
int lib_compare(void) { return lib_program_add == program_add; }
int lib_install(void) { program_callback = twice; return 0; }

static void jumper(int v) { longjmp(jump, v); }
int lib_setjmp(void) {
	int v = setjmp(jump);
	if (v == 0) jumper(7);
	return v;
}

char *lib_format(int n) {
	char *s = malloc(64);
	snprintf(s, 64, "%s-%d-%zu", "lib", n, strlen("four"));
	return s;
}

int lib_raise(void) { return feraiseexcept(FE_DIVBYZERO); }

/* weak definitions: wasm-ld imports them too; the program's wins, else its own */
__attribute__((weak)) int lib_weak(int x) { return x + 7; }
__attribute__((weak)) int program_weak(void) { return 1; }
extern int absent(void) __attribute__((weak));
int lib_weak_calls(void) { return lib_weak(1) * 10 + program_weak(); }
int lib_weak_pointer(void) { int (*f)(int) = lib_weak; return f(2); }
int lib_absent(void) { return absent ? absent() : -1; }
