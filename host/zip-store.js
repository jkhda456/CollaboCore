// A guest-visible filesystem that lives in the browser and is a ZIP archive at rest.
//
//   const store = new ZipStore({ backing: indexedDbBacking() });
//   await store.load();                                   // restore the last saved archive
//   plugins.push(fileSystemDevice(store, { tag: "work" }));  // guest: mount -t virtiofs work /work
//   store.toZipBlob()                                     // the whole tree as a .zip, any time
//
// While the machine runs the tree is plain JS objects in memory (fast, no per-write archive
// rewrite). It is written out as one stored (uncompressed) ZIP, at most once per `saveDelayMs`
// after a change and immediately on fsync, to `backing`; `load()` reads it back.
//
// Implements the FS contract of @lowland/kernel's fileSystemDevice: only what an agent needs
// (files, directories, symlinks, rename, truncate, times, modes). No hard links, no xattrs,
// no special files. Ownership is kept while running but is not stored in the archive.

import { FSError } from "@lowland/kernel";
import { crc32, readZip, writeZip, ZipError } from "./zip.js";

const S_IFDIR = 0o040000;
const S_IFREG = 0o100000;
const S_IFLNK = 0o120000;
const O_EXCL = 0o200;
const O_TRUNC = 0o1000;
const TYPE_BITS = { dir: S_IFDIR, file: S_IFREG, symlink: S_IFLNK };
const encoder = new TextEncoder();

class Node {
  constructor(type, mode, now, parent) {
    this.type = type; // "file" | "dir" | "symlink"
    this.mode = mode & 0o7777;
    this.uid = 0;
    this.gid = 0;
    this.mtime = now; // ms
    this.ctime = now;
    this.parent = parent;
    this.opens = 0; // open file handles, so an unlinked file lives until its last close
    this.gone = false; // removed from the tree
    this.freed = false; // its bytes/entry no longer count against the limits
    if (type === "dir") this.children = new Map();
    else {
      this.data = new Uint8Array(0); // capacity >= size
      this.size = 0;
      this.crc = 0; // crc32 of data[0, size); valid while crcValid
      this.crcValid = true;
      this.target = ""; // symlinks
    }
  }
}

/**
 * @param {{ backing?: { load(): Promise<Blob|Uint8Array|undefined>, save(blob: Blob): Promise<void> },
 *           maxBytes?: number, maxEntries?: number, saveDelayMs?: number, clock?: () => number }} options
 */
export class ZipStore {
  #root;
  #backing;
  #clock;
  #listeners = new Set();
  #used = 0; // bytes in live files/links
  #entries = 0; // files + dirs + links, root excluded
  #dirty = false;
  #timer;
  #chain = Promise.resolve();
  #lastSaved;
  #lastError;

  constructor({ backing, maxBytes = 256 * 1024 * 1024, maxEntries = 60_000, saveDelayMs = 750, clock = Date.now } = {}) {
    this.#backing = backing;
    this.maxBytes = maxBytes;
    this.maxEntries = Math.min(maxEntries, 65_000); // a ZIP without ZIP64 holds at most 65535
    this.saveDelayMs = saveDelayMs;
    this.#clock = clock;
    this.#root = new Node("dir", 0o755, clock(), undefined);
    this.#root.parent = this.#root;
  }

  // ---- what the host page uses ----------------------------------------------------------

  /** The tree as a ZIP archive. Cheap: file buffers are referenced, not copied. */
  toZipBlob() {
    return writeZip(this.#zipEntries());
  }

  /** Every entry, parents first, sorted by name: `{ path, type, size, mode, mtime }`. */
  list() {
    return [...this.#walk()].map(({ path, node }) => ({
      path,
      type: node.type,
      size: node.type === "dir" ? 0 : node.size,
      mode: node.mode,
      mtime: Math.floor(node.mtime / 1000),
    }));
  }

  /** Reads a file's bytes by path (host-side access), or undefined. Not the FS `read`. */
  readFile(path) {
    const node = this.#find(path);
    return node && node.type !== "dir" ? node.data.slice(0, node.size) : undefined;
  }

  stats() {
    return {
      bytes: this.#used,
      entries: this.#entries,
      maxBytes: this.maxBytes,
      maxEntries: this.maxEntries,
      persistent: Boolean(this.#backing),
      dirty: this.#dirty,
      lastSaved: this.#lastSaved,
      lastError: this.#lastError,
    };
  }

  /** Calls `fn` after every change (and after saves). Returns an unsubscribe function. */
  subscribe(fn) {
    this.#listeners.add(fn);
    return () => this.#listeners.delete(fn);
  }

  /** Restores the saved archive. The store must be empty. Throws ZipError if it is unusable. */
  async load() {
    const saved = await this.#backing?.load();
    if (!saved) return;
    const bytes = saved instanceof Uint8Array ? saved : new Uint8Array(await saved.arrayBuffer());
    this.importZip(bytes);
    this.#dirty = false;
    this.#emit();
  }

  /** Merges an archive into the tree (files with the same path are replaced). */
  importZip(bytes) {
    for (const entry of readZip(bytes)) {
      const parts = entry.path.split("/");
      const name = parts.pop();
      let dir = this.#root;
      for (const part of parts) {
        let next = dir.children.get(part);
        if (!next) next = this.#add(dir, part, new Node("dir", 0o755, this.#clock(), dir));
        if (next.type !== "dir") throw new ZipError(`"${entry.path}": "${part}" is a file, not a directory`);
        dir = next;
      }
      const existing = dir.children.get(name);
      if (entry.type === "dir") {
        if (existing && existing.type !== "dir") throw new ZipError(`"${entry.path}" is both a file and a directory`);
        if (!existing) this.#add(dir, name, new Node("dir", entry.mode, entry.mtime * 1000, dir));
        continue;
      }
      if (existing) {
        if (existing.type === "dir") throw new ZipError(`"${entry.path}" is both a file and a directory`);
        this.#remove(dir, name);
      }
      const node = new Node(entry.type, entry.mode, entry.mtime * 1000, dir);
      if (entry.type === "symlink") {
        node.target = new TextDecoder().decode(entry.data);
        node.size = entry.data.length;
      } else {
        node.data = entry.data.slice();
        node.size = node.data.length;
        node.crc = entry.crc;
      }
      this.#reserve(node.size);
      this.#add(dir, name, node);
      this.#used += node.size;
    }
  }

  /** Writes the archive to `backing` now (coalescing with a save already under way). */
  saveNow() {
    clearTimeout(this.#timer);
    this.#timer = undefined;
    const run = this.#chain.then(() => this.#saveOnce());
    this.#chain = run.catch(() => {});
    return run;
  }

  // ---- FS contract (see @lowland/kernel FS) ---------------------------------------------

  get root() {
    return this.#root;
  }

  lookup(parent, name) {
    if (parent.type !== "dir") throw new FSError("ENOTDIR");
    return parent.children.get(name);
  }

  getattr(node) {
    const time = (ms) => ({ seconds: BigInt(Math.floor(ms / 1000)), nanoseconds: (ms % 1000) * 1_000_000 });
    return {
      mode: TYPE_BITS[node.type] | node.mode,
      size: BigInt(node.type === "dir" ? 4096 : node.size),
      atime: time(node.mtime),
      mtime: time(node.mtime),
      ctime: time(node.ctime),
      blocks: BigInt(node.type === "dir" ? 8 : Math.ceil(node.size / 512)),
      nlink: node.type === "dir" ? 2 + [...node.children.values()].filter((c) => c.type === "dir").length : 1,
      uid: node.uid,
      gid: node.gid,
      blockSize: 4096,
    };
  }

  setattr(node, changes) {
    const now = this.#clock();
    if (changes.size !== undefined) {
      if (node.type === "dir") throw new FSError("EISDIR");
      if (node.type !== "file") throw new FSError("EINVAL");
      this.#truncate(node, Number(changes.size));
      node.mtime = now;
    }
    if (changes.mode !== undefined) node.mode = changes.mode & 0o7777;
    if (changes.uid !== undefined) node.uid = changes.uid;
    if (changes.gid !== undefined) node.gid = changes.gid;
    if (changes.mtime !== undefined) {
      node.mtime = changes.mtime === "now" ? now : Number(changes.mtime.seconds) * 1000 + Math.floor((changes.mtime.nanoseconds ?? 0) / 1e6);
    }
    node.ctime = now;
    this.#changed();
    return this.getattr(node);
  }

  readlink(node) {
    if (node.type !== "symlink") throw new FSError("EINVAL");
    return node.target;
  }

  symlink(parent, name, target, context) {
    const node = this.#create(parent, name, "symlink", 0o777, context);
    node.target = target;
    node.size = encoder.encode(target).length;
    this.#reserve(node.size);
    this.#used += node.size;
    return node;
  }

  mkdir(parent, name, context) {
    return this.#create(parent, name, "dir", context.mode, context);
  }

  unlink(parent, name) {
    const child = this.#existing(parent, name);
    if (child.type === "dir") throw new FSError("EISDIR");
    this.#remove(parent, name);
  }

  rmdir(parent, name) {
    const child = this.#existing(parent, name);
    if (child.type !== "dir") throw new FSError("ENOTDIR");
    if (child.children.size > 0) throw new FSError("ENOTEMPTY");
    this.#remove(parent, name);
  }

  rename(oldParent, oldName, newParent, newName) {
    const node = this.#existing(oldParent, oldName);
    if (newParent.type !== "dir") throw new FSError("ENOTDIR");
    const target = newParent.children.get(newName);
    if (target === node) return;
    if (node.type === "dir") {
      // A directory cannot move into itself or below itself.
      for (let up = newParent; ; up = up.parent) {
        if (up === node) throw new FSError("EINVAL");
        if (up === up.parent) break;
      }
    }
    if (target) {
      if (target.type === "dir" && node.type !== "dir") throw new FSError("EISDIR");
      if (target.type !== "dir" && node.type === "dir") throw new FSError("ENOTDIR");
      if (target.type === "dir" && target.children.size > 0) throw new FSError("ENOTEMPTY");
      this.#remove(newParent, newName);
    }
    oldParent.children.delete(oldName);
    newParent.children.set(newName, node);
    node.parent = newParent;
    const now = this.#clock();
    node.ctime = oldParent.mtime = oldParent.ctime = newParent.mtime = newParent.ctime = now;
    this.#changed();
  }

  open(node, flags) {
    if (node.type === "dir") throw new FSError("EISDIR");
    if (node.type !== "file") throw new FSError("EINVAL");
    if (flags & O_TRUNC) {
      this.#truncate(node, 0);
      node.mtime = node.ctime = this.#clock();
      this.#changed();
    }
    node.opens++;
    return node;
  }

  create(parent, name, flags, context) {
    const existing = parent.type === "dir" ? parent.children.get(name) : undefined;
    if (existing) {
      if (flags & O_EXCL) throw new FSError("EEXIST");
      return { node: existing, handle: this.open(existing, flags) };
    }
    const node = this.#create(parent, name, "file", context.mode, context);
    node.opens++;
    return { node, handle: node };
  }

  read(node, _handle, offset, length) {
    const start = Number(offset);
    if (start >= node.size) return new Uint8Array(0);
    return node.data.subarray(start, Math.min(node.size, start + length));
  }

  write(node, _handle, offset, bytes) {
    const start = Number(offset);
    const end = start + bytes.length;
    if (end > node.size) {
      this.#reserve(end - node.size);
      if (end > node.data.length) {
        const grown = new Uint8Array(Math.max(end, Math.min(node.data.length * 2, this.maxBytes)));
        grown.set(node.data.subarray(0, node.size));
        node.data = grown;
      }
      this.#used += end - node.size;
      node.size = end; // bytes between the old size and `start` are already zero
    }
    node.data.set(bytes, start);
    node.crcValid = false;
    node.mtime = node.ctime = this.#clock();
    this.#changed();
    return bytes.length;
  }

  flush() {}

  async fsync() {
    try {
      await this.saveNow();
    } catch (error) {
      throw new FSError("EIO", `saving the workspace failed: ${error?.message ?? error}`);
    }
  }

  release(node) {
    node.opens--;
    if (node.gone) this.#free(node);
  }

  opendir(node) {
    if (node.type !== "dir") throw new FSError("ENOTDIR");
    return node;
  }

  readdir(node) {
    return [...node.children].map(([name, child]) => ({ name, node: child }));
  }

  statfs() {
    const block = 4096;
    const free = Math.max(0, this.maxBytes - this.#used);
    return {
      blocks: BigInt(Math.ceil(this.maxBytes / block)),
      blocksFree: BigInt(Math.floor(free / block)),
      blocksAvailable: BigInt(Math.floor(free / block)),
      files: BigInt(this.maxEntries),
      filesFree: BigInt(Math.max(0, this.maxEntries - this.#entries)),
      blockSize: block,
      nameLength: 255,
    };
  }

  async destroy() {
    await this.saveNow().catch(() => {});
  }

  // ---- internals ---------------------------------------------------------------------------

  #existing(parent, name) {
    if (parent.type !== "dir") throw new FSError("ENOTDIR");
    const child = parent.children.get(name);
    if (!child) throw new FSError("ENOENT");
    return child;
  }

  /** Checks the limits before `bytes` more are stored. */
  #reserve(bytes) {
    if (this.#used + bytes > this.maxBytes) throw new FSError("ENOSPC", `workspace is full (${this.maxBytes} bytes)`);
  }

  #create(parent, name, type, mode, context) {
    if (parent.type !== "dir") throw new FSError("ENOTDIR");
    if (parent.children.has(name)) throw new FSError("EEXIST");
    const node = new Node(type, mode, this.#clock(), parent);
    node.uid = context?.uid ?? 0;
    node.gid = context?.gid ?? 0;
    return this.#add(parent, name, node);
  }

  #add(parent, name, node) {
    if (this.#entries >= this.maxEntries) throw new FSError("ENOSPC", `workspace has too many files (${this.maxEntries})`);
    parent.children.set(name, node);
    node.parent = parent;
    this.#entries++;
    parent.mtime = parent.ctime = this.#clock();
    this.#changed();
    return node;
  }

  #remove(parent, name) {
    const node = parent.children.get(name);
    parent.children.delete(name);
    parent.mtime = parent.ctime = this.#clock();
    node.gone = true;
    this.#free(node);
    this.#changed();
  }

  /** Stops counting a removed node once nothing has it open any more. */
  #free(node) {
    if (node.freed || node.opens > 0 || !node.gone) return;
    node.freed = true;
    this.#entries--;
    if (node.type !== "dir") this.#used -= node.size;
  }

  #truncate(node, size) {
    if (size > node.size) {
      this.#reserve(size - node.size);
      if (size > node.data.length) {
        const grown = new Uint8Array(size);
        grown.set(node.data.subarray(0, node.size));
        node.data = grown;
      }
    } else {
      node.data.fill(0, size, node.size); // a later extension must read zeros
    }
    this.#used += size - node.size;
    node.size = size;
    node.crcValid = false;
  }

  *#walk(dir = this.#root, prefix = "") {
    for (const name of [...dir.children.keys()].sort()) {
      const node = dir.children.get(name);
      const path = prefix + name;
      yield { path, node };
      if (node.type === "dir") yield* this.#walk(node, `${path}/`);
    }
  }

  #find(path) {
    let node = this.#root;
    for (const part of path.split("/").filter(Boolean)) {
      if (node.type !== "dir") return undefined;
      node = node.children.get(part);
      if (!node) return undefined;
    }
    return node;
  }

  *#zipEntries() {
    for (const { path, node } of this.#walk()) {
      if (node.type === "dir") {
        yield { path, type: "dir", mode: node.mode, mtime: Math.floor(node.mtime / 1000) };
      } else if (node.type === "symlink") {
        yield { path, type: "symlink", data: encoder.encode(node.target), mode: node.mode, mtime: Math.floor(node.mtime / 1000) };
      } else {
        const data = node.data.subarray(0, node.size);
        if (!node.crcValid) {
          node.crc = crc32(data);
          node.crcValid = true;
        }
        yield { path, type: "file", data, mode: node.mode, mtime: Math.floor(node.mtime / 1000), crc: node.crc };
      }
    }
  }

  #changed() {
    this.#dirty = true;
    if (this.#backing && !this.#timer) {
      this.#timer = setTimeout(() => {
        this.#timer = undefined;
        this.saveNow().catch(() => {}); // the failure is kept in stats().lastError
      }, this.saveDelayMs);
    }
    this.#emit();
  }

  #emit() {
    for (const fn of this.#listeners) fn();
  }

  async #saveOnce() {
    if (!this.#dirty || !this.#backing) return;
    this.#dirty = false;
    try {
      await this.#backing.save(this.toZipBlob());
      this.#lastSaved = this.#clock();
      this.#lastError = undefined;
    } catch (error) {
      this.#dirty = true; // try again on the next change
      this.#lastError = error;
      throw error;
    } finally {
      this.#emit();
    }
  }
}

/**
 * Keeps the archive in IndexedDB, the one browser store that every browser offers for a
 * multi-megabyte blob (OPFS write support still varies).
 */
export function indexedDbBacking({ database = "collaboCore", key = "workspace.zip", indexedDB = globalThis.indexedDB } = {}) {
  const open = () =>
    new Promise((resolve, reject) => {
      const request = indexedDB.open(database, 1);
      request.onupgradeneeded = () => request.result.createObjectStore("files");
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    });
  const run = async (mode, action) => {
    const db = await open();
    try {
      return await new Promise((resolve, reject) => {
        const tx = db.transaction("files", mode);
        const request = action(tx.objectStore("files"));
        tx.oncomplete = () => resolve(request.result);
        tx.onerror = tx.onabort = () => reject(tx.error);
      });
    } finally {
      db.close();
    }
  };
  return {
    load: () => run("readonly", (store) => store.get(key)),
    save: (blob) => run("readwrite", (store) => store.put(blob, key)),
  };
}
