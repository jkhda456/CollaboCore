// PID 1 for the agent sandbox. The machine is already isolated, so there is no
// login: mount the pseudo filesystems and drop straight into a root shell.
//
// It also shuts the machine down: busybox poweroff, halt and reboot (and /usr/sbin/shutdown)
// signal PID 1 with SIGUSR2, SIGUSR1 and SIGTERM. Every program is stopped, the disks synced,
// and reboot(2) ends the machine, which the kernel reports to the engine as a clean exit
// (arch/wasm machine_power_off). The engine cannot restart a machine, so reboot also ends it.
//
// No fork() on wasm Linux; children are created with posix_spawn().
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

// The signal that asked for a shutdown, 0 until one does.
static volatile sig_atomic_t stop_signal;

static void on_stop(int sig)
{
	stop_signal = sig;
}

// Reap every child that has left, without waiting.
static void reap(void)
{
	while (waitpid(-1, NULL, WNOHANG) > 0)
		;
}

// Whether PID 1 still has children (orphans are reparented to it, so this is every program).
static int children_left(void)
{
	reap();
	return !(waitpid(-1, NULL, WNOHANG) < 0 && errno == ECHILD);
}

static void shut_down(int sig)
{
	const char *what = sig == SIGTERM ? "reboot" : sig == SIGUSR1 ? "halt" : "power off";
	fprintf(stderr, "\ninit: %s: stopping every program\n", what);
	// SIGTERM first (and SIGHUP, which an interactive shell, ignoring SIGTERM, leaves on), and
	// up to 3 s to leave; then SIGKILL for the rest.
	kill(-1, SIGTERM);
	kill(-1, SIGHUP);
	for (int i = 0; i < 30 && children_left(); i++)
		usleep(100 * 1000);
	kill(-1, SIGKILL);
	for (int i = 0; i < 20 && children_left(); i++)
		usleep(50 * 1000);
	// Writes to the shared folders still in the page cache reach the host now.
	sync();
	reboot(sig == SIGTERM ? RB_AUTOBOOT : sig == SIGUSR1 ? RB_HALT_SYSTEM : RB_POWER_OFF);
	fprintf(stderr, "init: reboot: %s\n", strerror(errno));
	for (;;)
		pause();
}

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
	// Without SA_RESTART, so a wait() in progress returns EINTR and the loop below sees it.
	struct sigaction stop = { .sa_handler = on_stop };
	sigemptyset(&stop.sa_mask);
	sigaction(SIGUSR1, &stop, NULL);
	sigaction(SIGUSR2, &stop, NULL);
	sigaction(SIGTERM, &stop, NULL);

	mnt("proc", "/proc", "proc", 0);	// busybox (NOMMU) re-execs /proc/self/exe
	mnt("sysfs", "/sys", "sysfs", 0);
	mnt("devtmpfs", "/dev", "devtmpfs", 0);
	// The kernel opened fds 0-2 on the initramfs's /dev/console, which devtmpfs now covers:
	// the same terminal, but ttyname() compares inodes, so `tty`, screen and Python's
	// os.ttyname() would find no name for it. Everything after this inherits devtmpfs's.
	int console = open("/dev/console", O_RDWR | O_NOCTTY);
	if (console >= 0) {
		for (int fd = 0; fd < 3; fd++)
			dup2(console, fd);
		if (console > 2)
			close(console);
	}
	// Without it os.openpty() is ENOENT and Python's pty falls back to the BSD /dev/ttyp*
	// pairs, whose slave it opens without O_NOCTTY.
	mnt("devpts", "/dev/pts", "devpts", 0);
	mnt("tmpfs", "/tmp", "tmpfs", 0);
	mkdir("/root", 0700);
	sethostname("collabo", 7);

	setenv("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin", 1);
	setenv("HOME", "/root", 1);
	setenv("TERM", "xterm-256color", 1);
	setenv("USER", "root", 1);
	// UTF-8 everywhere (musl's only multibyte encoding anyway): Python, git, curl and less
	// look at LANG to decide how to treat text.
	setenv("LANG", "C.UTF-8", 1);
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

	// The shell needs the console as its controlling terminal, or ^C has no foreground group
	// to signal. posix_spawn can start a session but not take a terminal (TIOCSCTTY), so
	// busybox `setsid -c` does both and then execs the shell in the same process.
	char *session[] = { "setsid", "-c", "sh", "-l", NULL };
	char *plain[] = { "sh", "-l", NULL };
	int ctty = access("/usr/bin/setsid", X_OK) == 0;
	const char *path = ctty ? "/usr/bin/setsid" : "/bin/sh";
	char **argv = ctty ? session : plain;

	for (;;) {
		if (stop_signal)
			shut_down(stop_signal);
		pid_t pid;
		int err = posix_spawn(&pid, path, NULL, NULL, argv, environ);
		if (err) {
			fprintf(stderr, "init: cannot spawn %s: %s\n", path, strerror(err));
			sleep(1);
			continue;
		}
		// PID 1 reaps orphans too; only respawn the shell when it is the one that left.
		for (;;) {
			int status;
			pid_t w = wait(&status);
			if (stop_signal)
				shut_down(stop_signal);
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
