/*
** A WAL database used by several processes, each with several threads and connections, at once:
** what python/patches/sqlite-shm.c (the guest's wal-index without shared memory) must hold up.
**
**   sqlite_wal_stress DB PROCS THREADS TXNS
**
** Each thread moves amounts between accounts in write transactions (the total never changes) and
** logs each move; read transactions check the total, and that what they see does not change under
** them (a snapshot); a few run checkpoints of every kind, which restart the log. A small
** wal_autocheckpoint makes that happen often. At the end: the integrity check, the total, every
** move logged once. The processes are started with posix_spawn (the guest has no fork).
** Exit 0 when all held; it says what did not, otherwise.
*/
#include "sqlite3.h"

#include <pthread.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define ACCOUNTS 50
#define START 1000
#define TOTAL (ACCOUNTS * START)

extern char **environ;
static const char *db_path;
static int worker_id, txns;

static void fail(const char *what, sqlite3 *db) {
  fprintf(stderr, "FAIL worker %d: %s%s%s (%d)\n", worker_id, what, db ? ": " : "", db ? sqlite3_errmsg(db) : "", db ? sqlite3_extended_errcode(db) : 0);
  exit(2);
}

static void exec(sqlite3 *db, const char *sql) {
  char *err = 0;
  if (sqlite3_exec(db, sql, 0, 0, &err) != SQLITE_OK) {
    fprintf(stderr, "FAIL worker %d: %s: %s (%d, errno %d)\n", worker_id, sql, err ? err : "?", sqlite3_extended_errcode(db), sqlite3_system_errno(db));
    exit(2);
  }
}

static sqlite3_int64 one(sqlite3 *db, const char *sql) {
  sqlite3_stmt *st;
  sqlite3_int64 v;
  if (sqlite3_prepare_v2(db, sql, -1, &st, 0) != SQLITE_OK) fail(sql, db);
  if (sqlite3_step(st) != SQLITE_ROW) fail(sql, db);
  v = sqlite3_column_int64(st, 0);
  sqlite3_finalize(st);
  return v;
}

static sqlite3 *open_db(void) {
  sqlite3 *db;
  if (sqlite3_open(db_path, &db) != SQLITE_OK) fail("open", db);
  sqlite3_busy_timeout(db, 60000);
  exec(db, "PRAGMA wal_autocheckpoint=20");
  return db;
}

static void *thread_main(void *arg) {
  int thread = (int)(long)arg, i, writes = 0;
  unsigned seed = (unsigned)(worker_id * 7919 + thread * 104729 + 1);
  sqlite3_int64 seen = 0;
  sqlite3 *db = open_db();
  char sql[256];
  for (i = 0; i < txns; i++) {
    int r = rand_r(&seed) % 100;
    if (r < 65) {
      int a = rand_r(&seed) % ACCOUNTS, b = rand_r(&seed) % ACCOUNTS, x = rand_r(&seed) % 50;
      exec(db, "BEGIN IMMEDIATE");
      snprintf(sql, sizeof sql, "UPDATE acct SET bal = bal - %d WHERE id = %d", x, a);
      exec(db, sql);
      snprintf(sql, sizeof sql, "UPDATE acct SET bal = bal + %d WHERE id = %d", x, b);
      exec(db, sql);
      /* A wide row: the log grows by pages, and the WAL by many frames. */
      snprintf(sql, sizeof sql, "INSERT INTO log(worker, thread, n, pad) VALUES (%d, %d, %d, randomblob(300))", worker_id, thread, i);
      exec(db, sql);
      exec(db, "COMMIT");
      writes++;
    } else if (r < 95) {
      sqlite3_int64 total, n1, n2;
      exec(db, "BEGIN");
      total = one(db, "SELECT sum(bal) FROM acct");
      n1 = one(db, "SELECT count(*) FROM log");
      if (total != TOTAL) {
        snprintf(sql, sizeof sql, "the total is %lld, not %d", (long long)total, TOTAL);
        fail(sql, 0);
      }
      usleep((useconds_t)(rand_r(&seed) % 2000));
      n2 = one(db, "SELECT count(*) FROM log");
      if (n1 != n2) fail("a read transaction's snapshot changed", 0);
      if (n1 < seen) fail("the log went back", 0);
      seen = n1;
      exec(db, "COMMIT");
    } else {
      static const char *kinds[] = {"PASSIVE", "FULL", "RESTART", "TRUNCATE"};
      snprintf(sql, sizeof sql, "PRAGMA wal_checkpoint(%s)", kinds[rand_r(&seed) % 4]);
      exec(db, sql);
    }
  }
  snprintf(sql, sizeof sql, "INSERT INTO done VALUES (%d, %d, %d)", worker_id, thread, writes);
  exec(db, sql);
  sqlite3_close(db);
  return 0;
}

/* SQLite's log: a failed open or lock says there which file and why (errno). */
static void log_error(void *unused, int code, const char *message) {
  (void)unused;
  if ((code & 0xff) != SQLITE_NOTICE && (code & 0xff) != SQLITE_WARNING) fprintf(stderr, "sqlite log (%d): %s\n", code, message);
}

int main(int argc, char **argv) {
  int procs, threads, i, status, failed = 0;
  sqlite3 *db;
  sqlite3_config(SQLITE_CONFIG_LOG, log_error, (void *)0);
  if (argc == 6 && strcmp(argv[2], "worker") == 0) {
    pthread_t t[16];
    db_path = argv[1];
    worker_id = atoi(argv[3]);
    threads = atoi(argv[4]);
    txns = atoi(argv[5]);
    for (i = 0; i < threads && i < 16; i++) pthread_create(&t[i], 0, thread_main, (void *)(long)i);
    for (i = 0; i < threads && i < 16; i++) pthread_join(t[i], 0);
    return 0;
  }
  if (argc != 5) {
    fprintf(stderr, "usage: %s DB PROCS THREADS TXNS\n", argv[0]);
    return 64;
  }
  db_path = argv[1];
  procs = atoi(argv[2]);
  threads = atoi(argv[3]);
  unlink(db_path);
  db = open_db();
  {
    /* STRESS_JOURNAL=delete: the same in rollback-journal mode, for comparison. */
    const char *mode = getenv("STRESS_JOURNAL") ? getenv("STRESS_JOURNAL") : "wal";
    char sql[64];
    sqlite3_stmt *st;
    snprintf(sql, sizeof sql, "PRAGMA journal_mode=%s", mode);
    sqlite3_prepare_v2(db, sql, -1, &st, 0);
    if (sqlite3_step(st) != SQLITE_ROW || strcmp((const char *)sqlite3_column_text(st, 0), mode) != 0) {
      fprintf(stderr, "FAIL: journal_mode=WAL gave %s\n", sqlite3_column_text(st, 0));
      return 2;
    }
    sqlite3_finalize(st);
  }
  exec(db, "CREATE TABLE acct(id INTEGER PRIMARY KEY, bal INTEGER); CREATE TABLE log(worker, thread, n, pad); CREATE TABLE done(worker, thread, writes)");
  exec(db, "BEGIN");
  for (i = 0; i < ACCOUNTS; i++) {
    char sql[80];
    snprintf(sql, sizeof sql, "INSERT INTO acct VALUES (%d, %d)", i, START);
    exec(db, sql);
  }
  exec(db, "COMMIT");
  {
    pid_t pids[64];
    for (i = 0; i < procs && i < 64; i++) {
      char id[16];
      char *args[] = {argv[0], argv[1], "worker", id, argv[3], argv[4], 0};
      snprintf(id, sizeof id, "%d", i);
      if (posix_spawn(&pids[i], argv[0], 0, 0, args, environ) != 0) {
        perror("posix_spawn");
        return 2;
      }
    }
    for (i = 0; i < procs && i < 64; i++) {
      if (waitpid(pids[i], &status, 0) < 0 || !WIFEXITED(status) || WEXITSTATUS(status) != 0) failed++;
    }
  }
  if (failed) {
    fprintf(stderr, "FAIL: %d worker(s) failed\n", failed);
    return 2;
  }
  {
    sqlite3_stmt *st;
    const char *ok;
    sqlite3_prepare_v2(db, "PRAGMA integrity_check", -1, &st, 0);
    sqlite3_step(st);
    ok = (const char *)sqlite3_column_text(st, 0);
    if (!ok || strcmp(ok, "ok") != 0) {
      fprintf(stderr, "FAIL: integrity_check: %s\n", ok ? ok : "?");
      return 2;
    }
    sqlite3_finalize(st);
  }
  if (one(db, "SELECT sum(bal) FROM acct") != TOTAL) fail("the total at the end", 0);
  if (one(db, "SELECT count(*) FROM done") != (sqlite3_int64)procs * threads) fail("a thread did not finish", 0);
  if (one(db, "SELECT count(*) FROM log") != one(db, "SELECT sum(writes) FROM done")) fail("moves lost or logged twice", 0);
  if (one(db, "SELECT count(*) FROM (SELECT 1 FROM log GROUP BY worker, thread, n HAVING count(*) > 1)") != 0) fail("a move logged twice", 0);
  printf("ok: %d processes x %d threads, %lld writes, integrity ok\n", procs, threads, (long long)one(db, "SELECT count(*) FROM log"));
  sqlite3_close(db);
  return 0;
}
