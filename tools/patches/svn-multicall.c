/* All of Subversion's commands in one program: they share nearly all their code (the libsvn_*
 * libraries, APR, serf, SQLite, OpenSSL), which the image then carries once, not ten times. Each
 * command's main() is compiled again as svn_main_<command> (tools/build.sh), and the name this
 * program runs as (each command is a link to it) picks one, as with busybox. */
#include <stdio.h>
#include <string.h>

#define COMMANDS(X) X(svn) X(svnadmin) X(svndumpfilter) X(svnfsfs) X(svnlook) X(svnmucc) \
                    X(svnrdump) X(svnserve) X(svnsync) X(svnversion)

#define DECLARE(name) int svn_main_##name(int argc, const char *argv[]);
COMMANDS(DECLARE)

static const struct command {
  const char *name;
  int (*main)(int argc, const char *argv[]);
} commands[] = {
#define ENTRY(name) {#name, svn_main_##name},
  COMMANDS(ENTRY)
};

int main(int argc, const char *argv[])
{
  const char *name = argc > 0 ? argv[0] : "svn";
  const char *slash = strrchr(name, '/');
  size_t i;

  if (slash)
    name = slash + 1;
  for (i = 0; i < sizeof(commands) / sizeof(commands[0]); i++)
    if (strcmp(name, commands[i].name) == 0)
      return commands[i].main(argc, argv);
  fprintf(stderr, "%s: not a Subversion command; this program runs as", name);
  for (i = 0; i < sizeof(commands) / sizeof(commands[0]); i++)
    fprintf(stderr, " %s", commands[i].name);
  fputc('\n', stderr);
  return 1;
}
