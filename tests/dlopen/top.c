/* A library linked against tests/dlopen/dep.c (libdep.so, which the C library's dlopen loads
 * first, from beside it), with thread-local storage. */
extern int dep_value(void);

static _Thread_local int storage;

int *top_tls_address(void) { return &storage; }
int top_value(void) { return dep_value() * 2; }
