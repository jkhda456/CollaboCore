// Tests for host/ or web/zip-store.js: the FS contract called directly (no guest needed),
// persistence, and the IndexedDB backing. Run with the import-map loader:
//   node --import ./tests/importmap-register.mjs --test tests/zip-store.test.mjs
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { test } from "node:test";
import { FSError } from "@lowland/kernel";
import { indexedDB as fakeIndexedDB, IDBFactory } from "fake-indexeddb";
import { ZipStore, indexedDbBacking } from "../host/zip-store.js";
import { readZip, writeZip, ZipError } from "../host/zip.js";

const enc = new TextEncoder();
const dec = new TextDecoder();
const ctx = { mode: 0o644, uid: 0, gid: 0 };
const dirCtx = { mode: 0o755, uid: 0, gid: 0 };
const ERRNO = { ENOENT: 2, EIO: 5, EEXIST: 17, ENOTDIR: 20, EISDIR: 21, EINVAL: 22, ENOSPC: 28, ENOTEMPTY: 39 };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Asserts that `fn` throws an FSError with the given errno name. */
function fails(fn, code) {
  assert.throws(fn, (e) => e instanceof FSError && e.errno === ERRNO[code], `expected ${code}`);
}

/** Writes `text` to a new file `name` in `dir`, the way the guest would (create, write, release). */
function put(store, dir, name, text, context = ctx) {
  const { node, handle } = store.create(dir, name, 0, context);
  store.write(node, handle, 0n, enc.encode(text));
  store.release(node, handle);
  return node;
}
const cat = (store, node) => dec.decode(store.read(node, node, 0n, 1 << 20));

function memoryBacking({ failSaves = false } = {}) {
  const b = {
    saved: undefined,
    saves: 0,
    failSaves,
    async load() {
      return b.saved;
    },
    async save(blob) {
      b.saves++;
      if (b.failSaves) throw new Error("disk full");
      b.saved = blob;
    },
  };
  return b;
}

// ---- files --------------------------------------------------------------------------------

test("create, write, read back, and attributes carry the file type", () => {
  const s = new ZipStore();
  const f = put(s, s.root, "a.txt", "hello");
  assert.equal(s.lookup(s.root, "a.txt"), f, "lookup returns the same node object every time");
  assert.equal(cat(s, f), "hello");
  const attr = s.getattr(f);
  assert.equal(attr.mode, 0o100644);
  assert.equal(attr.size, 5n);
  assert.equal(s.lookup(s.root, "missing"), undefined);
});

test("writes past the end zero-fill; appends and overwrites land where asked", () => {
  const s = new ZipStore();
  const { node, handle } = s.create(s.root, "f", 0, ctx);
  s.write(node, handle, 10n, enc.encode("X"));
  assert.equal(s.getattr(node).size, 11n);
  assert.deepEqual([...s.read(node, handle, 0n, 11)], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 88]);
  s.write(node, handle, 0n, enc.encode("abc"));
  s.write(node, handle, 11n, enc.encode("!"));
  assert.equal(dec.decode(s.read(node, handle, 0n, 12)).replace(/\0/g, "."), "abc.......X!");
  assert.equal(s.read(node, handle, 50n, 10).length, 0, "reading past the end returns nothing");
  assert.equal(s.read(node, handle, 9n, 100).length, 3, "a read is cut at the end of file");
});

test("truncate shrinks, and a later extension reads zeros, not stale data", () => {
  const s = new ZipStore();
  const f = put(s, s.root, "f", "0123456789");
  s.setattr(f, { size: 4n });
  assert.equal(cat(s, f), "0123");
  s.setattr(f, { size: 8n });
  assert.deepEqual([...s.read(f, f, 0n, 8)], [48, 49, 50, 51, 0, 0, 0, 0]);
  fails(() => s.setattr(s.root, { size: 0n }), "EISDIR");
});

test("open with O_TRUNC empties the file", () => {
  const s = new ZipStore();
  const f = put(s, s.root, "f", "content");
  const h = s.open(f, 0o1000);
  assert.equal(s.getattr(f).size, 0n);
  s.release(f, h);
});

test("create on an existing name: O_EXCL fails, otherwise it opens the file", () => {
  const s = new ZipStore();
  const f = put(s, s.root, "f", "x");
  fails(() => s.create(s.root, "f", 0o200, ctx), "EEXIST");
  const again = s.create(s.root, "f", 0, ctx);
  assert.equal(again.node, f);
  s.release(again.node, again.handle);
});

test("mode, times and ownership can be changed", () => {
  const s = new ZipStore({ clock: () => 5_000_000 });
  const f = put(s, s.root, "f", "x");
  s.setattr(f, { mode: 0o100755, mtime: { seconds: 1_700_000_000n, nanoseconds: 500_000_000 }, uid: 7, gid: 8 });
  const a = s.getattr(f);
  assert.equal(a.mode & 0o7777, 0o755);
  assert.equal(a.mtime.seconds, 1_700_000_000n);
  assert.equal(a.uid, 7);
  assert.equal(a.gid, 8);
  s.setattr(f, { mtime: "now" });
  assert.equal(s.getattr(f).mtime.seconds, 5_000n);
});

// ---- directories --------------------------------------------------------------------------

test("mkdir, nested files, readdir, nlink", () => {
  const s = new ZipStore();
  const d = s.mkdir(s.root, "d", dirCtx);
  s.mkdir(d, "sub", dirCtx);
  put(s, d, "b", "2");
  put(s, d, "a", "1");
  assert.deepEqual(s.readdir(d).map((e) => e.name).sort(), ["a", "b", "sub"]);
  assert.equal(s.getattr(d).mode, 0o040755);
  assert.equal(s.getattr(d).nlink, 3, "2 + one subdirectory");
  fails(() => s.mkdir(s.root, "d", dirCtx), "EEXIST");
  fails(() => s.lookup(s.lookup(d, "a"), "x"), "ENOTDIR");
  fails(() => s.opendir(s.lookup(d, "a")), "ENOTDIR");
});

test("unlink and rmdir report the right errors", () => {
  const s = new ZipStore();
  const d = s.mkdir(s.root, "d", dirCtx);
  put(s, d, "f", "x");
  fails(() => s.unlink(s.root, "d"), "EISDIR");
  fails(() => s.rmdir(s.root, "d"), "ENOTEMPTY");
  fails(() => s.rmdir(d, "f"), "ENOTDIR");
  fails(() => s.unlink(d, "nope"), "ENOENT");
  s.unlink(d, "f");
  s.rmdir(s.root, "d");
  assert.equal(s.lookup(s.root, "d"), undefined);
  assert.equal(s.stats().entries, 0);
});

test("rename follows POSIX rules", () => {
  const s = new ZipStore();
  const d1 = s.mkdir(s.root, "d1", dirCtx);
  const d2 = s.mkdir(s.root, "d2", dirCtx);
  const f = put(s, d1, "f", "data");
  s.rename(d1, "f", d2, "g");
  assert.equal(s.lookup(d1, "f"), undefined);
  assert.equal(s.lookup(d2, "g"), f, "the node moves, identity is kept");

  put(s, d2, "h", "old");
  s.rename(d2, "g", d2, "h");
  assert.equal(cat(s, s.lookup(d2, "h")), "data", "a file replaces an existing file");
  assert.equal(s.stats().entries, 3, "the replaced file no longer counts");

  s.mkdir(d1, "inner", dirCtx);
  fails(() => s.rename(s.root, "d1", s.lookup(d1, "inner"), "d1"), "EINVAL"); // into itself
  fails(() => s.rename(d2, "h", s.root, "d1"), "EISDIR"); // file over directory
  fails(() => s.rename(s.root, "d1", d2, "h"), "ENOTDIR"); // directory over file
  fails(() => s.rename(s.root, "d2", s.root, "d1"), "ENOTEMPTY"); // directory over non-empty directory
  s.rename(d2, "h", d2, "h"); // onto itself: nothing happens
  fails(() => s.rename(d1, "missing", d2, "x"), "ENOENT");
});

test("symlinks round trip", () => {
  const s = new ZipStore();
  const l = s.symlink(s.root, "link", "target/file", ctx);
  assert.equal(s.readlink(l), "target/file");
  assert.equal(s.getattr(l).mode & 0o170000, 0o120000);
  assert.equal(s.getattr(l).size, BigInt("target/file".length));
  fails(() => s.readlink(s.root), "EINVAL");
  fails(() => s.open(l, 0), "EINVAL");
});

// ---- limits -------------------------------------------------------------------------------

test("the byte limit is enforced, atomically, and freed by unlink", () => {
  const s = new ZipStore({ maxBytes: 100 });
  const f = put(s, s.root, "a", "x".repeat(60));
  const { node, handle } = s.create(s.root, "b", 0, ctx);
  fails(() => s.write(node, handle, 0n, new Uint8Array(41)), "ENOSPC");
  assert.equal(s.getattr(node).size, 0n, "a refused write changes nothing");
  s.write(node, handle, 0n, new Uint8Array(40)); // exactly full
  fails(() => s.setattr(node, { size: 41n }), "ENOSPC");
  s.unlink(s.root, "a");
  assert.equal(s.stats().bytes, 40);
  s.write(node, handle, 40n, new Uint8Array(60));
  assert.equal(s.stats().bytes, 100);
  const st = s.statfs();
  assert.equal(st.blocksFree, 0n);
  void f;
});

test("an unlinked file that is still open keeps counting until it is closed", () => {
  const s = new ZipStore({ maxBytes: 1000 });
  const { node, handle } = s.create(s.root, "tmp", 0, ctx);
  s.write(node, handle, 0n, new Uint8Array(400));
  s.unlink(s.root, "tmp");
  assert.equal(s.stats().bytes, 400, "still open, still using memory");
  s.write(node, handle, 400n, new Uint8Array(100)); // usable while unlinked
  s.release(node, handle);
  assert.equal(s.stats().bytes, 0);
  assert.equal(s.stats().entries, 0);
});

test("the entry limit is enforced", () => {
  const s = new ZipStore({ maxEntries: 3 });
  s.mkdir(s.root, "a", dirCtx);
  s.mkdir(s.root, "b", dirCtx);
  s.mkdir(s.root, "c", dirCtx);
  fails(() => s.mkdir(s.root, "d", dirCtx), "ENOSPC");
  fails(() => s.create(s.root, "f", 0, ctx), "ENOSPC");
  assert.ok(new ZipStore({ maxEntries: 1_000_000 }).maxEntries <= 65_000, "never beyond what a ZIP can hold");
});

// ---- the archive --------------------------------------------------------------------------

function sampleStore() {
  let t = 1_789_000_000_000;
  const s = new ZipStore({ clock: () => (t += 1000) });
  const d = s.mkdir(s.root, "src", dirCtx);
  put(s, d, "main.c", "int main(){}\n");
  put(s, s.root, "run.sh", "#!/bin/sh\n", { mode: 0o755, uid: 0, gid: 0 });
  s.mkdir(s.root, "empty", dirCtx);
  s.symlink(s.root, "link", "src/main.c", ctx);
  put(s, s.root, "한글.txt", "안녕\n");
  return s;
}

test("toZipBlob lists parents first in sorted order and reads back through our reader", async () => {
  const s = sampleStore();
  const entries = readZip(new Uint8Array(await s.toZipBlob().arrayBuffer()));
  assert.deepEqual(
    entries.map((e) => `${e.type}:${e.path}`),
    ["dir:empty", "symlink:link", "file:run.sh", "dir:src", "file:src/main.c", "file:한글.txt"],
  );
  const run = entries.find((e) => e.path === "run.sh");
  assert.equal(run.mode, 0o755);
  assert.equal(dec.decode(entries.find((e) => e.path === "link").data), "src/main.c");
});

test("Python's zipfile accepts the exported archive and sees the same files", async () => {
  const s = sampleStore();
  const zip = Buffer.from(await s.toZipBlob().arrayBuffer());
  const r = spawnSync(
    "python3",
    ["-c", `import sys,io,json,zipfile
z=zipfile.ZipFile(io.BytesIO(sys.stdin.buffer.read())); assert z.testzip() is None
print(json.dumps({i.filename:(z.read(i.filename).decode() if not i.is_dir() else None) for i in z.infolist()},ensure_ascii=False))`],
    { input: zip },
  );
  assert.equal(r.status, 0, String(r.stderr));
  const files = JSON.parse(r.stdout.toString());
  assert.equal(files["src/main.c"], "int main(){}\n");
  assert.equal(files["한글.txt"], "안녕\n");
  assert.equal(files["link"], "src/main.c");
});

test("the exported CRC follows writes (a stale cached CRC would make an invalid archive)", async () => {
  const s = new ZipStore();
  const f = put(s, s.root, "f", "first");
  await s.toZipBlob().arrayBuffer(); // caches the crc
  const h = s.open(f, 0);
  s.write(f, h, 0n, enc.encode("SECOND"));
  s.release(f, h);
  const [entry] = readZip(new Uint8Array(await s.toZipBlob().arrayBuffer())); // readZip verifies the CRC
  assert.equal(dec.decode(entry.data), "SECONDt".slice(0, 6));
});

test("importZip merges: new files appear, same-path files are replaced", async () => {
  const s = new ZipStore();
  put(s, s.root, "keep.txt", "keep");
  put(s, s.root, "over.txt", "old");
  s.importZip(
    new Uint8Array(
      await writeZip([
        { path: "over.txt", type: "file", data: enc.encode("new") },
        { path: "deep/er/x.txt", type: "file", data: enc.encode("x") },
      ]).arrayBuffer(),
    ),
  );
  assert.equal(dec.decode(s.readFile("keep.txt")), "keep");
  assert.equal(dec.decode(s.readFile("over.txt")), "new");
  assert.equal(dec.decode(s.readFile("deep/er/x.txt")), "x");
  assert.equal(s.stats().entries, 5);
});

test("importZip refuses a file/directory clash and respects the limits", async () => {
  const s = new ZipStore({ maxBytes: 10 });
  const zip = async (entries) => new Uint8Array(await writeZip(entries).arrayBuffer());
  assert.throws(() => s.importZip(new Uint8Array(0)), ZipError);
  await assert.rejects(async () => {
    const big = await zip([{ path: "big", type: "file", data: new Uint8Array(11) }]);
    s.importZip(big);
  }, (e) => e instanceof FSError && e.errno === ERRNO.ENOSPC);
  const clash = await zip([
    { path: "a", type: "file", data: enc.encode("x") },
    { path: "a/b", type: "file", data: enc.encode("y") },
  ]);
  assert.throws(() => new ZipStore().importZip(clash), /is a file, not a directory/);
});

test("host-side helpers: list(), read(), stats(), subscribe()", () => {
  const s = sampleStore();
  assert.deepEqual(s.list().map((e) => e.path), ["empty", "link", "run.sh", "src", "src/main.c", "한글.txt"]);
  assert.equal(s.list().find((e) => e.path === "src/main.c").size, 13);
  assert.equal(dec.decode(s.readFile("src/main.c")), "int main(){}\n");
  assert.equal(s.readFile("nope"), undefined);
  assert.equal(s.readFile("src"), undefined, "a directory has no bytes");
  let calls = 0;
  const off = s.subscribe(() => calls++);
  put(s, s.root, "n", "1");
  assert.ok(calls > 0);
  off();
  const before = calls;
  put(s, s.root, "m", "1");
  assert.equal(calls, before, "unsubscribed");
});

// ---- persistence --------------------------------------------------------------------------

test("saves are throttled: many writes make one save, some time after the first", async () => {
  const backing = memoryBacking();
  const s = new ZipStore({ backing, saveDelayMs: 40 });
  for (let i = 0; i < 20; i++) put(s, s.root, `f${i}`, "x");
  assert.equal(backing.saves, 0, "nothing is written per operation");
  assert.equal(s.stats().dirty, true);
  await sleep(120);
  assert.equal(backing.saves, 1);
  assert.equal(s.stats().dirty, false);
  assert.ok(s.stats().lastSaved);
  put(s, s.root, "more", "x");
  await sleep(120);
  assert.equal(backing.saves, 2);
});

test("fsync saves at once and waits for it", async () => {
  const backing = memoryBacking();
  const s = new ZipStore({ backing, saveDelayMs: 60_000 });
  const f = put(s, s.root, "f", "durable");
  await s.fsync(f, f, false);
  assert.equal(backing.saves, 1);
  assert.equal(readZip(new Uint8Array(await backing.saved.arrayBuffer()))[0].path, "f");
  await s.fsync(f, f, false);
  assert.equal(backing.saves, 1, "nothing changed, nothing to save");
});

test("concurrent fsyncs are coalesced and none returns before the data is saved", async () => {
  const backing = memoryBacking();
  const slow = backing.save;
  backing.save = async (blob) => {
    await sleep(30);
    return slow(blob);
  };
  const s = new ZipStore({ backing, saveDelayMs: 60_000 });
  const f = put(s, s.root, "f", "1");
  await Promise.all([s.fsync(f, f), s.fsync(f, f), s.fsync(f, f)]);
  assert.ok(backing.saves <= 2);
  assert.ok(backing.saved);
});

test("a failed save is reported, kept dirty, and retried on the next change", async () => {
  const backing = memoryBacking({ failSaves: true });
  const s = new ZipStore({ backing, saveDelayMs: 20 });
  const f = put(s, s.root, "f", "x");
  await assert.rejects(s.fsync(f, f), (e) => e instanceof FSError && e.errno === ERRNO.EIO && /disk full/.test(e.message));
  assert.equal(s.stats().dirty, true, "not silently forgotten");
  assert.match(String(s.stats().lastError), /disk full/);
  backing.failSaves = false;
  put(s, s.root, "g", "y");
  await sleep(80);
  assert.equal(s.stats().dirty, false);
  assert.equal(s.stats().lastError, undefined);
  assert.equal(readZip(new Uint8Array(await backing.saved.arrayBuffer())).length, 2);
});

test("load() restores a saved workspace, and does not mark it dirty", async () => {
  const backing = memoryBacking();
  const first = new ZipStore({ backing, saveDelayMs: 10 });
  const d = first.mkdir(first.root, "proj", dirCtx);
  put(first, d, "notes.md", "# notes");
  await first.saveNow();

  const second = new ZipStore({ backing });
  await second.load();
  assert.equal(dec.decode(second.readFile("proj/notes.md")), "# notes");
  assert.equal(second.stats().dirty, false);
  assert.equal(second.stats().entries, 2);
  await sleep(30);
  assert.equal(backing.saves, 1, "loading does not trigger a save");
});

test("load() with nothing saved is an empty workspace; a corrupt save is an error, not an empty overwrite", async () => {
  const empty = new ZipStore({ backing: memoryBacking() });
  await empty.load();
  assert.equal(empty.stats().entries, 0);

  const backing = memoryBacking();
  backing.saved = new Blob([enc.encode("not a zip")]);
  const s = new ZipStore({ backing });
  await assert.rejects(s.load(), ZipError);
  assert.equal(backing.saves, 0, "the unreadable data was not touched");
});

test("a store without a backing works in memory only", async () => {
  const s = new ZipStore();
  put(s, s.root, "f", "x");
  assert.equal(s.stats().persistent, false);
  await s.saveNow();
  await s.fsync(s.lookup(s.root, "f"), null);
});

// ---- IndexedDB ----------------------------------------------------------------------------

test("indexedDbBacking round-trips a Blob and survives a new store instance", async () => {
  const idb = new IDBFactory();
  const a = new ZipStore({ backing: indexedDbBacking({ indexedDB: idb }) });
  put(a, a.root, "persisted.txt", "still here");
  await a.saveNow();

  const b = new ZipStore({ backing: indexedDbBacking({ indexedDB: idb }) });
  await b.load();
  assert.equal(dec.decode(b.readFile("persisted.txt")), "still here");

  const other = new ZipStore({ backing: indexedDbBacking({ indexedDB: new IDBFactory() }) });
  await other.load();
  assert.equal(other.stats().entries, 0, "a different database is a different workspace");
});

test("indexedDbBacking: separate keys are separate workspaces; save overwrites", async () => {
  const idb = new IDBFactory();
  const one = indexedDbBacking({ indexedDB: idb, key: "one" });
  const two = indexedDbBacking({ indexedDB: idb, key: "two" });
  await one.save(new Blob([enc.encode("1")]));
  await two.save(new Blob([enc.encode("2")]));
  await one.save(new Blob([enc.encode("1b")]));
  assert.equal(dec.decode(new Uint8Array(await (await one.load()).arrayBuffer())), "1b");
  assert.equal(dec.decode(new Uint8Array(await (await two.load()).arrayBuffer())), "2");
  assert.equal(await indexedDbBacking({ indexedDB: idb, key: "none" }).load(), undefined);
  void fakeIndexedDB;
});
