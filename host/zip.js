// A minimal ZIP reader/writer: "stored" entries only (method 0), so no compression code at
// all. That is the whole point: the archive is a plain container, cheap to write and to read,
// and every tool (unzip, Python, the OS) opens it.
//
// Entry model, shared by both directions:
//   { path: "dir/file.txt",            forward slashes, no leading slash, never ".."
//     type: "file" | "dir" | "symlink",
//     data: Uint8Array,                file content, or the UTF-8 link target for a symlink
//     mode: 0o644,                     permission bits only
//     mtime: 1789000000,               seconds since the epoch
//     crc?: number }                   CRC-32 of data, if the caller already has it
//
// Written: UTF-8 names (flag bit 11), Unix mode in the external attributes, and an exact
// mtime in the "UT" extra field (the DOS time field only has 2-second resolution).
// Not supported, by design: compression, encryption, ZIP64 (4 GiB / 65535 entries).

const SIG_LOCAL = 0x04034b50;
const SIG_CENTRAL = 0x02014b50;
const SIG_END = 0x06054b50;
const UNIX = 3;
const S_IFMT = 0o170000;
const S_IFDIR = 0o040000;
const S_IFREG = 0o100000;
const S_IFLNK = 0o120000;
const MAX32 = 0xffffffff;
const MAX16 = 0xffff;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

export class ZipError extends Error {
  constructor(message) {
    super(message);
    this.name = "ZipError";
  }
}

// ---- CRC-32 -------------------------------------------------------------------------------

const TABLE = new Uint32Array(256);
for (let n = 0; n < 256; n++) {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  TABLE[n] = c >>> 0;
}

/** CRC-32 (IEEE), as ZIP uses. Pass a previous result as `crc` to continue a running checksum. */
export function crc32(bytes, crc = 0) {
  let c = (crc ^ 0xffffffff) >>> 0;
  for (let i = 0; i < bytes.length; i++) c = TABLE[(c ^ bytes[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

// ---- writing ------------------------------------------------------------------------------

function dosDateTime(seconds) {
  const date = new Date(Math.max(seconds, 315532800) * 1000); // DOS time starts in 1980
  const year = Math.min(date.getUTCFullYear(), 2107);
  return {
    time: (date.getUTCHours() << 11) | (date.getUTCMinutes() << 5) | (date.getUTCSeconds() >> 1),
    date: ((year - 1980) << 9) | ((date.getUTCMonth() + 1) << 5) | date.getUTCDate(),
  };
}

const TYPE_BITS = { file: S_IFREG, dir: S_IFDIR, symlink: S_IFLNK };

/**
 * Builds an archive. Returns a Blob assembled from the entries' own buffers, so nothing is
 * copied into one big array and the result can be downloaded or stored as it is.
 * @param {Iterable<{ path: string, type: "file"|"dir"|"symlink", data?: Uint8Array, mode?: number, mtime?: number, crc?: number }>} entries
 */
export function writeZip(entries) {
  const parts = [];
  const central = [];
  let offset = 0;
  let count = 0;

  for (const entry of entries) {
    if (++count > MAX16) throw new ZipError("too many entries for a ZIP archive without ZIP64 (65535)");
    const isDir = entry.type === "dir";
    const name = encoder.encode(isDir ? `${entry.path}/` : entry.path);
    const data = isDir ? new Uint8Array(0) : (entry.data ?? new Uint8Array(0));
    if (data.length > MAX32) throw new ZipError(`"${entry.path}" is too large for a ZIP archive without ZIP64`);
    const crc = isDir ? 0 : (entry.crc ?? crc32(data));
    const mtime = Math.floor(entry.mtime ?? Date.now() / 1000);
    const { time, date } = dosDateTime(mtime);
    const mode = TYPE_BITS[entry.type] | ((entry.mode ?? (isDir ? 0o755 : 0o644)) & 0o7777);

    // "UT" extra field: id 0x5455, size 5, flags 1 (= mtime present), mtime as u32.
    const extra = new Uint8Array(9);
    const ev = new DataView(extra.buffer);
    ev.setUint16(0, 0x5455, true);
    ev.setUint16(2, 5, true);
    extra[4] = 1;
    ev.setUint32(5, mtime >>> 0, true);

    const local = new Uint8Array(30 + name.length + extra.length);
    const lv = new DataView(local.buffer);
    lv.setUint32(0, SIG_LOCAL, true);
    lv.setUint16(4, 20, true); // version needed
    lv.setUint16(6, 0x0800, true); // flags: UTF-8 names
    lv.setUint16(8, 0, true); // method: stored
    lv.setUint16(10, time, true);
    lv.setUint16(12, date, true);
    lv.setUint32(14, crc, true);
    lv.setUint32(18, data.length, true); // compressed size = size
    lv.setUint32(22, data.length, true);
    lv.setUint16(26, name.length, true);
    lv.setUint16(28, extra.length, true);
    local.set(name, 30);
    local.set(extra, 30 + name.length);

    const record = new Uint8Array(46 + name.length + extra.length);
    const cv = new DataView(record.buffer);
    cv.setUint32(0, SIG_CENTRAL, true);
    cv.setUint16(4, (UNIX << 8) | 20, true); // version made by: Unix
    cv.setUint16(6, 20, true);
    cv.setUint16(8, 0x0800, true);
    cv.setUint16(10, 0, true);
    cv.setUint16(12, time, true);
    cv.setUint16(14, date, true);
    cv.setUint32(16, crc, true);
    cv.setUint32(20, data.length, true);
    cv.setUint32(24, data.length, true);
    cv.setUint16(28, name.length, true);
    cv.setUint16(30, extra.length, true);
    // 32: comment length, 34: disk number, 36: internal attributes: all zero
    cv.setUint32(38, ((mode << 16) | (isDir ? 0x10 : 0)) >>> 0, true); // Unix mode + DOS dir bit
    cv.setUint32(42, offset, true);
    record.set(name, 46);
    record.set(extra, 46 + name.length);

    parts.push(local, data);
    central.push(record);
    offset += local.length + data.length;
    if (offset > MAX32) throw new ZipError("archive exceeds 4 GiB (ZIP64 is not supported)");
  }

  const directorySize = central.reduce((sum, r) => sum + r.length, 0);
  const end = new Uint8Array(22);
  const ev = new DataView(end.buffer);
  ev.setUint32(0, SIG_END, true);
  ev.setUint16(8, count, true);
  ev.setUint16(10, count, true);
  ev.setUint32(12, directorySize, true);
  ev.setUint32(16, offset, true);
  return new Blob([...parts, ...central, end], { type: "application/zip" });
}

// ---- reading ------------------------------------------------------------------------------

/** Normalises an entry name; refuses anything that could point outside the archive root. */
function cleanPath(name) {
  const parts = name.split("/").filter((part) => part !== "" && part !== ".");
  if (parts.some((part) => part === "..")) throw new ZipError(`unsafe path in archive: ${name}`);
  if (parts.some((part) => part.includes("\0"))) throw new ZipError(`invalid path in archive: ${name}`);
  return parts.join("/");
}

/** Finds the UT (extended timestamp) mtime in an extra-field block, if present. */
function extraMtime(bytes, start, length) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  for (let at = start; at + 4 <= start + length; ) {
    const id = view.getUint16(at, true);
    const size = view.getUint16(at + 2, true);
    if (id === 0x5455 && size >= 5 && (bytes[at + 4] & 1)) return view.getUint32(at + 5, true);
    at += 4 + size;
  }
  return undefined;
}

/**
 * Parses an archive. Throws ZipError for anything it cannot represent faithfully:
 * compressed or encrypted entries, ZIP64, a corrupt directory, a CRC mismatch, unsafe paths.
 * @param {Uint8Array} bytes
 */
export function readZip(bytes) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const need = (at, length, what) => {
    if (at < 0 || at + length > bytes.length) throw new ZipError(`corrupt archive: ${what} is out of range`);
  };

  let end = -1;
  for (let at = bytes.length - 22; at >= Math.max(0, bytes.length - 22 - MAX16); at--) {
    if (view.getUint32(at, true) === SIG_END) {
      end = at;
      break;
    }
  }
  if (end < 0) throw new ZipError("not a ZIP archive (no end-of-central-directory record)");

  const total = view.getUint16(end + 10, true);
  const directoryOffset = view.getUint32(end + 16, true);
  if (total === MAX16 || directoryOffset === MAX32) throw new ZipError("ZIP64 archives are not supported");

  const entries = [];
  let at = directoryOffset;
  for (let i = 0; i < total; i++) {
    need(at, 46, "central directory");
    if (view.getUint32(at, true) !== SIG_CENTRAL) throw new ZipError("corrupt archive: bad central directory entry");
    const madeBy = view.getUint16(at + 4, true);
    const flags = view.getUint16(at + 8, true);
    const method = view.getUint16(at + 10, true);
    const dosTime = view.getUint16(at + 12, true);
    const dosDate = view.getUint16(at + 14, true);
    const crc = view.getUint32(at + 16, true);
    const size = view.getUint32(at + 24, true);
    const nameLength = view.getUint16(at + 28, true);
    const extraLength = view.getUint16(at + 30, true);
    const commentLength = view.getUint16(at + 32, true);
    const external = view.getUint32(at + 38, true);
    const localOffset = view.getUint32(at + 42, true);
    need(at + 46, nameLength + extraLength, "entry name");
    const rawName = decoder.decode(bytes.subarray(at + 46, at + 46 + nameLength));
    const mtimeExtra = extraMtime(bytes, at + 46 + nameLength, extraLength);
    at += 46 + nameLength + extraLength + commentLength;

    if (flags & 1) throw new ZipError(`"${rawName}" is encrypted; encryption is not supported`);
    if (method !== 0) {
      throw new ZipError(`"${rawName}" is compressed (method ${method}); only stored entries are supported`);
    }
    const path = cleanPath(rawName);
    if (path === "") continue; // "./" or "/" : the root itself

    need(localOffset, 30, `local header of ${rawName}`);
    if (view.getUint32(localOffset, true) !== SIG_LOCAL) throw new ZipError(`corrupt archive: bad local header for ${rawName}`);
    const dataStart = localOffset + 30 + view.getUint16(localOffset + 26, true) + view.getUint16(localOffset + 28, true);
    need(dataStart, size, `data of ${rawName}`);
    const data = bytes.subarray(dataStart, dataStart + size);
    if (crc32(data) !== crc) throw new ZipError(`corrupt archive: CRC mismatch in ${rawName}`);

    const unixMode = madeBy >> 8 === UNIX ? (external >>> 16) & 0xffff : 0;
    const kind = unixMode & S_IFMT;
    const type = rawName.endsWith("/") || kind === S_IFDIR ? "dir" : kind === S_IFLNK ? "symlink" : "file";
    const defaultMode = type === "file" ? 0o644 : 0o755;
    const mode = unixMode & 0o7777 || defaultMode;

    // DOS fields carry no time zone; read them as UTC to match how they are written.
    const dosSeconds =
      Date.UTC(1980 + (dosDate >> 9), ((dosDate >> 5) & 15) - 1, dosDate & 31, dosTime >> 11, (dosTime >> 5) & 63, (dosTime & 31) * 2) / 1000;
    entries.push({ path, type, data: type === "dir" ? new Uint8Array(0) : data, mode, mtime: mtimeExtra ?? dosSeconds, crc });
  }
  return entries;
}
