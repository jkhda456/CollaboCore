/*
** WAL for the guest: the wal-index ("-shm") without shared memory.
**
** SQLite's unix VFS keeps a WAL database's wal-index in the -shm file mapped MAP_SHARED, so that
** every connection, in every process, sees one copy. The guest has no shared mappings: each
** process is a wasm instance with its own linear memory, and the C library has no mmap at all.
** WAL was compiled out (SQLITE_OMIT_WAL), and a database in WAL mode could not be opened ("file is
** not a database"). Now SQLite is built with SQLITE_WASI, os_unix.c's build without mmap, which
** leaves its files without shared-memory methods, and these provide them.
**
** This provides the four shared-memory methods. A process keeps the wal-index in its
** own memory, per region, with a second copy of what the file held when it last looked (the
** shadow), and syncs with the -shm file at SQLite's shm lock calls, which is where the WAL
** protocol synchronizes:
**
**   - Before any lock change: what this process changed (region != shadow) is written to the
**     file. SQLite changes a part of the wal-index only while it holds the exclusive lock that
**     guards that part, so this happens before that lock is let go: whoever takes it next
**     reads what was written under it.
**   - After a lock is obtained (and before it counts as held): the file is read in. Parts this
**     process owns are left alone: a thread of this process may be writing them, and nobody else
**     can. What SQLite reads without a lock (the header, read marks) it checks again after
**     locking, as it does with real shared memory, and retries when it changed.
**
** Which locks make which parts this process's: ownedHere(). Every lock obtained reads the whole
** wal-index in (bar those parts): a few 32 KiB regions, so WAL here costs 0 to 20 % over the
** host's shared memory in python/tests/sqlite_wal_stress.c.
**
** File locks (fcntl, which the guest kernel has, across processes and on mounted folders) are
** SQLite's own bytes: 120..127 the eight shm locks, 128 the dead-man switch (the first
** connection to open the -shm empties it). Byte 129 is this file's: held briefly around each
** write to and read of the file, so that neither sees the other's half done.
**
** It is the default VFS, named "unix" (the original wrapped, its other methods as they are), set
** up by sqlite3_initialize through SQLITE_EXTRA_INIT=collabo_shm_init. Python's _sqlite3
** (python/build-deps.sh) and the sqlite3 shell (tools/build.sh) are built with it. A host
** process using the same database at the same time does not share the guest's file locks: that
** was so before for every journal mode.
*/
#include "sqlite3.h"

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define NLOCK SQLITE_SHM_NLOCK /* 8 */
#define LOCK_BASE 120          /* the byte of shm lock 0 */
#define DMS_BYTE (LOCK_BASE + NLOCK)
#define SYNC_BYTE (DMS_BYTE + 1)

/* SQLite's shm lock numbers (wal.c). */
#define WRITE_LOCK 0
#define CKPT_LOCK 1
#define READ_LOCK(i) (3 + (i))

/* The wal-index header: two copies of WalIndexHdr (48 bytes), then WalCkptInfo (40). */
#define HDR_SIZE 96
#define INFO_END 136

typedef struct ShmNode ShmNode;
struct ShmNode {
  ShmNode *pNext;
  dev_t dev;
  ino_t ino;
  int fd;
  int isReadonly;
  int isUnlocked; /* read-only, and nobody had it open to set it up: try again at the next map */
  int nRef;
  int szRegion;
  int nRegion;
  char **apRegion; /* what SQLite reads and writes */
  char **apShadow; /* the file's bytes, as last read or written */
  /* This process's holds: > 0 shared (that many connections), -1 exclusive. */
  int aLock[NLOCK];
  sqlite3_mutex *mutex;
  char zPath[1];
};

typedef struct ShimFile {
  sqlite3_file base;
  sqlite3_file *pReal;
  const char *zName; /* the database's path, for its -shm (main databases only) */
  ShmNode *pNode;
  unsigned short sharedMask;
  unsigned short exclMask;
} ShimFile;

static sqlite3_vfs *pOrig;
static sqlite3_vfs shimVfs;
static ShmNode *pNodes; /* every node of this process, under the VFS mutex */

static sqlite3_mutex *vfsMutex(void) {
  return sqlite3_mutex_alloc(SQLITE_MUTEX_STATIC_VFS3);
}

/* -------------------------------------------------------------------------------------------- */
/* File locks */

static int fileLock(int fd, int type, int ofst, int n, int wait) {
  struct flock lock;
  memset(&lock, 0, sizeof(lock));
  lock.l_type = (short)type;
  lock.l_whence = SEEK_SET;
  lock.l_start = ofst;
  lock.l_len = n;
  for (;;) {
    if (fcntl(fd, wait ? F_SETLKW : F_SETLK, &lock) == 0) return SQLITE_OK;
    if (errno == EINTR) continue;
    if (errno == EAGAIN || errno == EACCES) return SQLITE_BUSY;
    sqlite3_log(SQLITE_IOERR_SHMLOCK, "fcntl on the -shm, bytes %d+%d: %s", ofst, n, strerror(errno));
    return SQLITE_IOERR_SHMLOCK;
  }
}

static int readAt(int fd, char *buf, int n, off_t ofst) {
  int done = 0;
  while (done < n) {
    ssize_t got = pread(fd, buf + done, (size_t)(n - done), ofst + done);
    if (got < 0 && errno == EINTR) continue;
    if (got < 0) return SQLITE_IOERR_SHMMAP;
    if (got == 0) {
      memset(buf + done, 0, (size_t)(n - done)); /* past the end: zeros, as a mapping has */
      break;
    }
    done += (int)got;
  }
  return SQLITE_OK;
}

static int writeAt(int fd, const char *buf, int n, off_t ofst) {
  int done = 0;
  while (done < n) {
    ssize_t put = pwrite(fd, buf + done, (size_t)(n - done), ofst + done);
    if (put < 0 && errno == EINTR) continue;
    if (put <= 0) return SQLITE_IOERR_SHMMAP;
    done += (int)put;
  }
  return SQLITE_OK;
}

/* -------------------------------------------------------------------------------------------- */
/* Syncing with the -shm file (the node's mutex held) */

/* Is this word of region 0 this process's now: may a connection here be writing it, and can
** nobody else? Then the file's copy cannot have changed, and this process's must not be
** overwritten. (Past INFO_END, and the other regions: the hash pages.)
**
**   header, hash pages         WRITE_LOCK
**   aReadMark[i]               READ_LOCK(i) exclusive
**   nBackfill, and nBackfillAttempted with the unused words around it: the checkpointer writes
**   them holding READ_LOCK(0) exclusive; a writer restarting the log holding WRITE_LOCK and
**   READ_LOCK(0) shared; recovery holding WRITE_LOCK and CKPT_LOCK. WRITE_LOCK alone is not
**   enough: another process's checkpointer may change them meanwhile. */
static int ownedHere(ShmNode *p, int ofst) {
  if (ofst < HDR_SIZE || ofst >= INFO_END) return p->aLock[WRITE_LOCK] < 0;
  if (ofst >= HDR_SIZE + 4 && ofst < HDR_SIZE + 24) return p->aLock[READ_LOCK((ofst - HDR_SIZE - 4) / 4)] < 0;
  return p->aLock[READ_LOCK(0)] < 0 || (p->aLock[WRITE_LOCK] < 0 && (p->aLock[READ_LOCK(0)] != 0 || p->aLock[CKPT_LOCK] < 0));
}

/* The wal-index's fields are 32-bit words (the hash pages' 16-bit slots two to a word), which
** SQLite reads and writes whole, often from other threads of this process at the same time. They
** are copied here a whole word at a time too: a copy a byte at a time lets a thread read a value
** that was never there (half a new nBackfill), which shared memory never shows. */
static unsigned loadWord(const char *at) {
  return __atomic_load_n((const unsigned *)at, __ATOMIC_RELAXED);
}

static void storeWord(char *at, unsigned v) {
  __atomic_store_n((unsigned *)at, v, __ATOMIC_RELAXED);
}

/* What this process changed goes to the file: runs of changed words only (one left as it was may
** be another process's to change, and have been changed in the file meanwhile). Only what was
** written is marked as written: a thread of this process may be changing more meanwhile, which
** then goes next time. */
static int flushNode(ShmNode *p) {
  int i, rc = SQLITE_OK, locked = 0;
  unsigned chunk[128];
  if (p->isReadonly || p->fd < 0) return SQLITE_OK;
  for (i = 0; i < p->nRegion && rc == SQLITE_OK; i++) {
    char *mem = p->apRegion[i], *shadow = p->apShadow[i];
    int at = 0, sz = p->szRegion;
    if (memcmp(mem, shadow, (size_t)sz) == 0) continue;
    while (at < sz && rc == SQLITE_OK) {
      int start, n = 0;
      unsigned v;
      while (at < sz && (v = loadWord(mem + at)) == loadWord(shadow + at)) at += 4;
      if (at >= sz) break;
      start = at;
      do {
        chunk[n++] = v;
        at += 4;
      } while (at < sz && n < (int)(sizeof(chunk) / 4) && (v = loadWord(mem + at)) != loadWord(shadow + at));
      if (!locked) {
        rc = fileLock(p->fd, F_WRLCK, SYNC_BYTE, 1, 1);
        if (rc != SQLITE_OK) break;
        locked = 1;
      }
      rc = writeAt(p->fd, (const char *)chunk, n * 4, (off_t)i * p->szRegion + start);
      if (rc == SQLITE_OK) memcpy(shadow + start, chunk, (size_t)n * 4);
    }
  }
  if (locked) fileLock(p->fd, F_UNLCK, SYNC_BYTE, 1, 1);
  return rc;
}

/* The file read in, except what this process owns (ownedHere). */
static int refreshNode(ShmNode *p) {
  int i, at, rc;
  char info[INFO_END] __attribute__((aligned(8)));
  if (p->fd < 0 || p->nRegion == 0) return SQLITE_OK;
  rc = fileLock(p->fd, F_RDLCK, SYNC_BYTE, 1, 1);
  if (rc != SQLITE_OK) return rc;
  rc = readAt(p->fd, info, INFO_END, 0);
  if (rc == SQLITE_OK && p->aLock[WRITE_LOCK] >= 0) {
    for (i = 0; i < p->nRegion && rc == SQLITE_OK; i++) {
      char *mem = p->apRegion[i], *shadow = p->apShadow[i];
      int from = i == 0 ? INFO_END : 0;
      rc = readAt(p->fd, shadow + from, p->szRegion - from, (off_t)i * p->szRegion + from);
      for (at = from; rc == SQLITE_OK && at < p->szRegion; at += 4) {
        unsigned v = loadWord(shadow + at);
        if (loadWord(mem + at) != v) storeWord(mem + at, v);
      }
    }
  }
  if (rc == SQLITE_OK) {
    for (at = 0; at < INFO_END; at += 4) {
      if (!ownedHere(p, at)) {
        unsigned v = loadWord(info + at);
        storeWord(p->apShadow[0] + at, v);
        if (loadWord(p->apRegion[0] + at) != v) storeWord(p->apRegion[0] + at, v);
      }
    }
  }
  fileLock(p->fd, F_UNLCK, SYNC_BYTE, 1, 1);
  return rc;
}

/* -------------------------------------------------------------------------------------------- */
/* Nodes: one per database file in this process, shared by its connections */

/* The first connection of any process to open the -shm empties it (the dead-man switch: what a
** crashed process left is not trusted), then every one holds a shared lock on byte 128. */
static int lockDeadManSwitch(ShmNode *p) {
  struct flock lock;
  memset(&lock, 0, sizeof(lock));
  lock.l_type = F_WRLCK;
  lock.l_whence = SEEK_SET;
  lock.l_start = DMS_BYTE;
  lock.l_len = 1;
  if (fcntl(p->fd, F_GETLK, &lock) != 0) return SQLITE_IOERR_LOCK;
  if (lock.l_type == F_UNLCK) {
    int rc;
    if (p->isReadonly) return SQLITE_READONLY_CANTINIT;
    rc = fileLock(p->fd, F_WRLCK, DMS_BYTE, 1, 0);
    if (rc != SQLITE_OK) return rc;
    /* 3 bytes, as SQLite's own: less than a header, and telling of who did it. */
    if (ftruncate(p->fd, 3) != 0) return SQLITE_IOERR_SHMOPEN;
  } else if (lock.l_type == F_WRLCK) {
    return SQLITE_BUSY;
  }
  return fileLock(p->fd, F_RDLCK, DMS_BYTE, 1, 0);
}

static void freeNode(ShmNode *p) {
  int i;
  for (i = 0; i < p->nRegion; i++) {
    sqlite3_free(p->apRegion[i]);
    sqlite3_free(p->apShadow[i]);
  }
  sqlite3_free(p->apRegion);
  sqlite3_free(p->apShadow);
  if (p->fd >= 0) close(p->fd); /* lets go of this process's locks on it */
  sqlite3_mutex_free(p->mutex);
  sqlite3_free(p);
}

static int openNode(ShimFile *f) {
  struct stat st;
  ShmNode *p;
  int rc = SQLITE_OK;
  sqlite3_mutex *m = vfsMutex();
  int fd;
  /* Connections to one database share a node: its inode, by its path. */
  if (stat(f->zName, &st) != 0) return SQLITE_IOERR_FSTAT;
  sqlite3_mutex_enter(m);
  for (p = pNodes; p && !(p->dev == st.st_dev && p->ino == st.st_ino); p = p->pNext) {}
  if (p == 0) {
    size_t len = strlen(f->zName);
    p = sqlite3_malloc64(sizeof(*p) + len + 4);
    if (p == 0) {
      sqlite3_mutex_leave(m);
      return SQLITE_NOMEM;
    }
    memset(p, 0, sizeof(*p));
    memcpy(p->zPath, f->zName, len);
    memcpy(p->zPath + len, "-shm", 5);
    p->dev = st.st_dev;
    p->ino = st.st_ino;
    p->fd = -1;
    p->mutex = sqlite3_mutex_alloc(SQLITE_MUTEX_FAST);
    fd = -1;
    if (!sqlite3_uri_boolean(f->zName, "readonly_shm", 0)) fd = open(p->zPath, O_RDWR | O_CREAT | O_NOFOLLOW | O_CLOEXEC, st.st_mode & 0777);
    if (fd < 0) {
      fd = open(p->zPath, O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
      p->isReadonly = 1;
    }
    p->fd = fd;
    if (fd < 0) {
      rc = SQLITE_CANTOPEN;
      sqlite3_log(rc, "cannot open %s: %s", p->zPath, strerror(errno));
    } else {
      rc = lockDeadManSwitch(p);
    }
    if (rc != SQLITE_OK && rc != SQLITE_READONLY_CANTINIT) {
      freeNode(p);
      sqlite3_mutex_leave(m);
      return rc;
    }
    p->isUnlocked = rc == SQLITE_READONLY_CANTINIT;
    p->pNext = pNodes;
    pNodes = p;
  }
  p->nRef++;
  f->pNode = p;
  sqlite3_mutex_leave(m);
  return rc;
}

/* -------------------------------------------------------------------------------------------- */
/* The shared-memory methods */

static int shimShmMap(sqlite3_file *pFile, int iRegion, int szRegion, int bExtend, void volatile **pp) {
  ShimFile *f = (ShimFile *)pFile;
  ShmNode *p;
  int rc = SQLITE_OK;
  *pp = 0;
  if (f->zName == 0) return SQLITE_IOERR_SHMOPEN;
  if (f->pNode == 0) {
    rc = openNode(f);
    if (rc != SQLITE_OK) return rc; /* SQLITE_READONLY_CANTINIT included: wal.c knows what to do */
  }
  p = f->pNode;
  sqlite3_mutex_enter(p->mutex);
  if (p->isUnlocked) {
    rc = lockDeadManSwitch(p);
    if (rc != SQLITE_OK) {
      sqlite3_mutex_leave(p->mutex);
      return rc;
    }
    p->isUnlocked = 0;
  }
  if (p->nRegion > 0 && p->szRegion != szRegion) rc = SQLITE_IOERR_SHMSIZE;
  if (rc == SQLITE_OK && iRegion >= p->nRegion) {
    struct stat st;
    off_t need = (off_t)(iRegion + 1) * szRegion;
    p->szRegion = szRegion;
    if (fstat(p->fd, &st) != 0) {
      rc = SQLITE_IOERR_SHMSIZE;
    } else if (st.st_size < need && !bExtend) {
      goto done; /* not there yet: *pp stays 0 */
    } else if (st.st_size < need && (p->isReadonly || writeAt(p->fd, "", 1, need - 1) != SQLITE_OK)) {
      rc = SQLITE_IOERR_SHMSIZE;
    }
    if (rc == SQLITE_OK) {
      char **apRegion = sqlite3_realloc64(p->apRegion, (iRegion + 1) * sizeof(char *));
      char **apShadow = apRegion ? sqlite3_realloc64(p->apShadow, (iRegion + 1) * sizeof(char *)) : 0;
      if (apRegion) p->apRegion = apRegion;
      if (apShadow) p->apShadow = apShadow;
      if (!apRegion || !apShadow) rc = SQLITE_NOMEM;
      while (rc == SQLITE_OK && p->nRegion <= iRegion) {
        char *mem = sqlite3_malloc(szRegion), *shadow = sqlite3_malloc(szRegion);
        if (!mem || !shadow) {
          sqlite3_free(mem);
          sqlite3_free(shadow);
          rc = SQLITE_NOMEM;
          break;
        }
        rc = fileLock(p->fd, F_RDLCK, SYNC_BYTE, 1, 1);
        if (rc == SQLITE_OK) {
          rc = readAt(p->fd, shadow, szRegion, (off_t)p->nRegion * szRegion);
          fileLock(p->fd, F_UNLCK, SYNC_BYTE, 1, 1);
        }
        if (rc != SQLITE_OK) {
          sqlite3_free(mem);
          sqlite3_free(shadow);
          break;
        }
        memcpy(mem, shadow, (size_t)szRegion);
        p->apRegion[p->nRegion] = mem;
        p->apShadow[p->nRegion] = shadow;
        p->nRegion++;
      }
    }
  }
  if (rc == SQLITE_OK && iRegion < p->nRegion) *pp = p->apRegion[iRegion];
done:
  if (rc == SQLITE_OK && p->isReadonly) rc = SQLITE_READONLY;
  sqlite3_mutex_leave(p->mutex);
  return rc;
}

static int shimShmLock(sqlite3_file *pFile, int ofst, int n, int flags) {
  ShimFile *f = (ShimFile *)pFile;
  ShmNode *p = f->pNode;
  unsigned short mask = (unsigned short)((1 << (ofst + n)) - (1 << ofst));
  int rc = SQLITE_OK, i;
  if (p == 0) return SQLITE_IOERR_SHMLOCK;
  sqlite3_mutex_enter(p->mutex);
  /* Before any change of locks: what was written under them, out. */
  rc = flushNode(p);
  if (rc != SQLITE_OK) {
    sqlite3_mutex_leave(p->mutex);
    return rc;
  }
  if (flags & SQLITE_SHM_UNLOCK) {
    if ((f->exclMask | f->sharedMask) & mask) {
      int release = 1;
      if ((flags & SQLITE_SHM_SHARED) && p->aLock[ofst] > 1) {
        release = 0; /* other connections of this process still hold it */
        p->aLock[ofst]--;
      }
      if (release) rc = fileLock(p->fd, F_UNLCK, LOCK_BASE + ofst, n, 0);
      if (rc == SQLITE_OK) {
        if (release) memset(&p->aLock[ofst], 0, sizeof(int) * n);
        f->sharedMask &= ~mask;
        f->exclMask &= ~mask;
      }
    }
  } else if (flags & SQLITE_SHM_SHARED) {
    if ((f->sharedMask & mask) == 0) {
      if (p->aLock[ofst] < 0) {
        rc = SQLITE_BUSY;
      } else if (p->aLock[ofst] == 0) {
        rc = fileLock(p->fd, F_RDLCK, LOCK_BASE + ofst, n, 0);
      }
      /* Read in before what this lock makes this process's is marked so. */
      if (rc == SQLITE_OK) rc = refreshNode(p);
      if (rc == SQLITE_OK) {
        p->aLock[ofst]++;
        f->sharedMask |= mask;
      }
    }
  } else {
    for (i = ofst; i < ofst + n; i++) {
      if (p->aLock[i]) rc = SQLITE_BUSY;
    }
    if (rc == SQLITE_OK) rc = fileLock(p->fd, F_WRLCK, LOCK_BASE + ofst, n, 0);
    if (rc == SQLITE_OK) {
      /* Read in before the parts become this process's: they are as their last owner left them. */
      rc = refreshNode(p);
      for (i = ofst; i < ofst + n; i++) p->aLock[i] = -1;
      f->exclMask |= mask;
    }
  }
  sqlite3_mutex_leave(p->mutex);
  return rc;
}

static void shimShmBarrier(sqlite3_file *pFile) {
  ShimFile *f = (ShimFile *)pFile;
  /* Between threads of this process the memory is shared: a real barrier. Across processes the
  ** lock calls sync. */
  __sync_synchronize();
  if (f->pNode) {
    sqlite3_mutex_enter(f->pNode->mutex);
    sqlite3_mutex_leave(f->pNode->mutex);
  }
}

static int shimShmUnmap(sqlite3_file *pFile, int deleteFlag) {
  ShimFile *f = (ShimFile *)pFile;
  ShmNode *p = f->pNode, **pp;
  sqlite3_mutex *m;
  if (p == 0) return SQLITE_OK;
  m = vfsMutex();
  sqlite3_mutex_enter(m);
  sqlite3_mutex_enter(p->mutex);
  flushNode(p);
  sqlite3_mutex_leave(p->mutex);
  f->pNode = 0;
  f->sharedMask = f->exclMask = 0;
  if (--p->nRef == 0) {
    for (pp = &pNodes; *pp != p; pp = &(*pp)->pNext) {}
    *pp = p->pNext;
    if (deleteFlag && p->fd >= 0) unlink(p->zPath);
    freeNode(p);
  }
  sqlite3_mutex_leave(m);
  return SQLITE_OK;
}

/* -------------------------------------------------------------------------------------------- */
/* The rest of a file's methods: the real file's */

#define REAL(pFile) (((ShimFile *)(pFile))->pReal)
#define SHIM_SIZE ((sizeof(ShimFile) + 7) & ~(size_t)7)

static int shimClose(sqlite3_file *pFile) {
  ShimFile *f = (ShimFile *)pFile;
  int rc;
  if (f->pNode) shimShmUnmap(pFile, 0);
  rc = f->pReal->pMethods ? f->pReal->pMethods->xClose(f->pReal) : SQLITE_OK;
  f->base.pMethods = 0;
  return rc;
}
static int shimRead(sqlite3_file *pFile, void *buf, int n, sqlite3_int64 ofst) { return REAL(pFile)->pMethods->xRead(REAL(pFile), buf, n, ofst); }
static int shimWrite(sqlite3_file *pFile, const void *buf, int n, sqlite3_int64 ofst) { return REAL(pFile)->pMethods->xWrite(REAL(pFile), buf, n, ofst); }
static int shimTruncate(sqlite3_file *pFile, sqlite3_int64 size) { return REAL(pFile)->pMethods->xTruncate(REAL(pFile), size); }
static int shimSync(sqlite3_file *pFile, int flags) { return REAL(pFile)->pMethods->xSync(REAL(pFile), flags); }
static int shimFileSize(sqlite3_file *pFile, sqlite3_int64 *size) { return REAL(pFile)->pMethods->xFileSize(REAL(pFile), size); }
static int shimLock(sqlite3_file *pFile, int lock) { return REAL(pFile)->pMethods->xLock(REAL(pFile), lock); }
static int shimUnlock(sqlite3_file *pFile, int lock) { return REAL(pFile)->pMethods->xUnlock(REAL(pFile), lock); }
static int shimCheckReservedLock(sqlite3_file *pFile, int *out) { return REAL(pFile)->pMethods->xCheckReservedLock(REAL(pFile), out); }
static int shimFileControl(sqlite3_file *pFile, int op, void *arg) { return REAL(pFile)->pMethods->xFileControl(REAL(pFile), op, arg); }
static int shimSectorSize(sqlite3_file *pFile) { return REAL(pFile)->pMethods->xSectorSize(REAL(pFile)); }
static int shimDeviceCharacteristics(sqlite3_file *pFile) { return REAL(pFile)->pMethods->xDeviceCharacteristics(REAL(pFile)); }
static int shimFetch(sqlite3_file *pFile, sqlite3_int64 ofst, int n, void **pp) {
  sqlite3_file *real = REAL(pFile);
  if (real->pMethods->iVersion < 3 || !real->pMethods->xFetch) {
    *pp = 0;
    return SQLITE_OK;
  }
  return real->pMethods->xFetch(real, ofst, n, pp);
}
static int shimUnfetch(sqlite3_file *pFile, sqlite3_int64 ofst, void *p) {
  sqlite3_file *real = REAL(pFile);
  if (real->pMethods->iVersion < 3 || !real->pMethods->xUnfetch) return SQLITE_OK;
  return real->pMethods->xUnfetch(real, ofst, p);
}

static const sqlite3_io_methods shimMethods = {
    3, shimClose, shimRead, shimWrite, shimTruncate, shimSync, shimFileSize, shimLock, shimUnlock,
    shimCheckReservedLock, shimFileControl, shimSectorSize, shimDeviceCharacteristics,
    shimShmMap, shimShmLock, shimShmBarrier, shimShmUnmap, shimFetch, shimUnfetch,
};

static int shimOpen(sqlite3_vfs *pVfs, sqlite3_filename zName, sqlite3_file *pFile, int flags, int *pOutFlags) {
  ShimFile *f = (ShimFile *)pFile;
  int rc;
  (void)pVfs;
  memset(f, 0, sizeof(*f));
  f->pReal = (sqlite3_file *)((char *)f + SHIM_SIZE);
  rc = pOrig->xOpen(pOrig, zName, f->pReal, flags, pOutFlags);
  if (f->pReal->pMethods == 0) return rc;
  /* Only a main database has a wal-index. */
  if (flags & SQLITE_OPEN_MAIN_DB) f->zName = zName;
  f->base.pMethods = &shimMethods;
  return rc;
}

int collabo_shm_init(const char *unused) {
  sqlite3_vfs *found = sqlite3_vfs_find("unix");
  (void)unused;
  /* After sqlite3_shutdown and a new sqlite3_initialize the unix VFS registers itself again. */
  if (found == &shimVfs) return SQLITE_OK;
  if (found == 0) return SQLITE_ERROR;
  pOrig = found;
  shimVfs = *pOrig; /* the same name, pathnames, randomness, time and system calls */
  shimVfs.pNext = 0;
  shimVfs.szOsFile = (int)SHIM_SIZE + pOrig->szOsFile;
  shimVfs.xOpen = shimOpen;
  sqlite3_vfs_unregister(pOrig);
  return sqlite3_vfs_register(&shimVfs, 1);
}
