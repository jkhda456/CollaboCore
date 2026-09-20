// PID 1 for the agent sandbox. The machine is already isolated, so there is no
// login: mount the pseudo filesystems and drop straight into a root shell.
//
// No fork() on wasm Linux; children are created with posix_spawn().
#include <errno.h>
#include <fcntl.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static void mnt(const char *src, const char *dst, const char *type, unsigned long flags)
{
	mkdir(dst, 0755);
	if (mount(src, dst, type, flags, NULL) < 0 && errno != EBUSY)
		fprintf(stderr, "init: mount %s on %s: %s\n", type, dst, strerror(errno));
}

// Run a program to completion. Used for /etc/rc before the interactive shell.
static void run(const char *path, char *const argv[])
{
	pid_t pid;
	int err = posix_spawn(&pid, path, NULL, NULL, argv, environ);
	if (err) {
		fprintf(stderr, "init: cannot spawn %s: %s\n", path, strerror(err));
		return;
	}
	while (waitpid(pid, NULL, 0) < 0 && errno == EINTR)
		;
}

int main(void)
{
	mnt("proc", "/proc", "proc", 0);	// busybox (NOMMU) re-execs /proc/self/exe
	mnt("sysfs", "/sys", "sysfs", 0);
	mnt("devtmpfs", "/dev", "devtmpfs", 0);
	mnt("tmpfs", "/tmp", "tmpfs", 0);
	mkdir("/root", 0700);
	sethostname("collabo", 7);

	setenv("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin", 1);
	setenv("HOME", "/root", 1);
	setenv("TERM", "xterm-256color", 1);
	setenv("USER", "root", 1);
	// Python would otherwise write __pycache__ next to the agent's scripts, i.e. into the
	// /work archive the user downloads.
	setenv("PYTHONDONTWRITEBYTECODE", "1", 1);
	// The new REPL wants _ctypes (not built: no libffi); say nothing and use the basic one.
	setenv("PYTHON_BASIC_REPL", "1", 1);

	puts("\ninit: collaboCore agent sandbox");
	if (access("/etc/rc", X_OK) == 0) {
		char *rc[] = { "sh", "/etc/rc", NULL };
		run("/bin/sh", rc);
	}
	puts("init: starting /bin/sh");

	for (;;) {
		char *argv[] = { "sh", "-l", NULL };
		pid_t pid;
		int err = posix_spawn(&pid, "/bin/sh", NULL, NULL, argv, environ);
		if (err) {
			fprintf(stderr, "init: cannot spawn /bin/sh: %s\n", strerror(err));
			sleep(1);
			continue;
		}
		// PID 1 reaps orphans too; only respawn the shell when it is the one that left.
		for (;;) {
			int status;
			pid_t w = wait(&status);
			if (w == pid) {
				fprintf(stderr, "init: shell exited (status %d), restarting\n", status);
				break;
			}
			if (w < 0 && errno != EINTR) {
				sleep(1);
				break;
			}
		}
	}
}
