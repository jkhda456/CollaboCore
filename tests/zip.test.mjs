// Tests for host/ or web/zip.js, cross-checked against Python's zipfile (an independent reader
// and writer). Run through the import-map loader:
//   node --import ./tests/importmap-register.mjs --test tests/zip.test.mjs
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { test } from "node:test";
import { crc32, readZip, writeZip, ZipError } from "../host/zip.js";

const enc = new TextEncoder();
const dec = new TextDecoder();
const bytesOf = async (blob) => new Uint8Array(await blob.arrayBuffer());

function python(script, input) {
  const r = spawnSync("python3", ["-c", script], { input, maxBuffer: 64 * 1024 * 1024 });
  if (r.status !== 0) throw new Error(`python failed: ${r.stderr}`);
  return r.stdout;
}

const sample = [
  { path: "readme.txt", type: "file", data: enc.encode("hello\n"), mode: 0o644, mtime: 1_789_000_001 }, // odd second
  { path: "bin", type: "dir", mode: 0o755, mtime: 1_789_000_002 },
  { path: "bin/run.sh", type: "file", data: enc.encode("#!/bin/sh\necho hi\n"), mode: 0o755, mtime: 1_789_000_003 },
  { path: "empty", type: "file", data: new Uint8Array(0), mode: 0o600, mtime: 1_789_000_004 },
  { path: "dir/한글 파일.txt", type: "file", data: enc.encode("유니코드\n"), mode: 0o644, mtime: 1_789_000_005 },
  { path: "link", type: "symlink", data: enc.encode("readme.txt"), mode: 0o777, mtime: 1_789_000_006 },
];

test("crc32 matches the standard check value and is chainable", () => {
  assert.equal(crc32(enc.encode("123456789")), 0xcbf43926);
  assert.equal(crc32(new Uint8Array(0)), 0);
  assert.equal(crc32(enc.encode("6789"), crc32(enc.encode("12345"))), 0xcbf43926);
});

test("round trip through our own reader keeps everything, including exact mtimes", async () => {
  const back = readZip(await bytesOf(writeZip(sample)));
  assert.equal(back.length, sample.length);
  for (const [i, want] of sample.entries()) {
    const got = back[i];
    assert.equal(got.path, want.path);
    assert.equal(got.type, want.type);
    assert.equal(got.mode, want.mode);
    assert.equal(got.mtime, want.mtime, `${want.path}: mtime is exact (odd seconds survive)`);
    assert.deepEqual([...got.data], [...(want.data ?? [])]);
  }
});

test("Python's zipfile reads our archive: valid CRCs, stored, names, modes, contents", async () => {
  const zip = await bytesOf(writeZip(sample));
  const out = python(
    `
import sys, io, json, zipfile
z = zipfile.ZipFile(io.BytesIO(sys.stdin.buffer.read()))
assert z.testzip() is None, "bad CRC"
info = []
for i in z.infolist():
    info.append({"name": i.filename, "stored": i.compress_type == zipfile.ZIP_STORED,
                 "mode": (i.external_attr >> 16) & 0o177777, "size": i.file_size,
                 "utf8": bool(i.flag_bits & 0x800),
                 "data": z.read(i.filename).decode("utf-8") if not i.is_dir() else None})
print(json.dumps(info, ensure_ascii=False))
`,
    zip,
  );
  const info = JSON.parse(dec.decode(out));
  assert.deepEqual(
    info.map((i) => i.name),
    ["readme.txt", "bin/", "bin/run.sh", "empty", "dir/한글 파일.txt", "link"],
  );
  assert.ok(info.every((i) => i.stored), "nothing is compressed");
  assert.ok(info.every((i) => i.utf8));
  const byName = Object.fromEntries(info.map((i) => [i.name, i]));
  assert.equal(byName["bin/run.sh"].mode, 0o100755, "executable bit and file type survive");
  assert.equal(byName["bin/"].mode & 0o170000, 0o040000);
  assert.equal(byName["link"].mode & 0o170000, 0o120000);
  assert.equal(byName["link"].data, "readme.txt", "a symlink's data is its target");
  assert.equal(byName["dir/한글 파일.txt"].data, "유니코드\n");
  assert.equal(byName["empty"].size, 0);
});

test("we read a stored archive written by Python", async () => {
  const zip = python(
    `
import sys, io, zipfile
buf = io.BytesIO()
with zipfile.ZipFile(buf, "w", zipfile.ZIP_STORED) as z:
    z.writestr("a.txt", "alpha")
    z.writestr(zipfile.ZipInfo("d/", (2026, 9, 19, 12, 0, 0)), "")
    zi = zipfile.ZipInfo("d/x.bin", (2026, 9, 19, 12, 0, 10)); zi.external_attr = 0o100640 << 16
    z.writestr(zi, bytes([0, 1, 2, 255]))
sys.stdout.buffer.write(buf.getvalue())
`,
    "",
  );
  const entries = readZip(new Uint8Array(zip));
  assert.deepEqual(entries.map((e) => [e.path, e.type]), [["a.txt", "file"], ["d", "dir"], ["d/x.bin", "file"]]);
  assert.equal(dec.decode(entries[0].data), "alpha");
  assert.equal(entries[2].mode, 0o640);
  assert.deepEqual([...entries[2].data], [0, 1, 2, 255]);
  assert.equal(entries[2].mtime, Date.UTC(2026, 8, 19, 12, 0, 10) / 1000, "DOS time is read as UTC, 2 s resolution");
});

test("a compressed archive is refused with a reason, not misread", () => {
  const zip = python(
    `
import sys, io, zipfile
buf = io.BytesIO()
with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("big.txt", "a" * 10000)
sys.stdout.buffer.write(buf.getvalue())
`,
    "",
  );
  assert.throws(() => readZip(new Uint8Array(zip)), (e) => e instanceof ZipError && /"big.txt" is compressed \(method 8\)/.test(e.message));
});

test("corruption is detected: CRC, truncation, garbage", async () => {
  const good = await bytesOf(writeZip(sample));
  const flipped = good.slice();
  flipped[good.indexOf(0x68, 30) + 40] ^= 0xff; // somewhere inside the first file's data area
  const dataAt = good.findIndex((_, i) => dec.decode(good.subarray(i, i + 6)) === "hello\n");
  flipped.set([0x48], dataAt); // 'h' -> 'H'
  assert.throws(() => readZip(flipped), (e) => e instanceof ZipError && /CRC mismatch in readme\.txt/.test(e.message));
  assert.throws(() => readZip(good.subarray(0, good.length - 30)), ZipError, "truncated: end record lost");
  assert.throws(() => readZip(enc.encode("this is not a zip file at all, just text")), /not a ZIP archive/);
  assert.throws(() => readZip(new Uint8Array(0)), ZipError);
});

test("unsafe paths are refused; absolute and dotted paths are normalised", () => {
  const make = (name) =>
    new Uint8Array(
      python(`import sys,io,zipfile\nb=io.BytesIO()\nwith zipfile.ZipFile(b,"w",zipfile.ZIP_STORED) as z: z.writestr(zipfile.ZipInfo(${JSON.stringify(name)}, (2026,1,1,0,0,0)), "x")\nsys.stdout.buffer.write(b.getvalue())`, ""),
    );
  assert.throws(() => readZip(make("../evil.txt")), /unsafe path/);
  assert.throws(() => readZip(make("a/../../evil.txt")), /unsafe path/);
  assert.equal(readZip(make("/abs/file.txt"))[0].path, "abs/file.txt", "leading slash cannot escape the root");
  assert.equal(readZip(make("./a//b/./c.txt"))[0].path, "a/b/c.txt");
});

test("encrypted entries are refused", async () => {
  const zip = await bytesOf(writeZip([{ path: "a", type: "file", data: enc.encode("x") }]));
  const view = new DataView(zip.buffer);
  const central = view.getUint32(zip.length - 6, true);
  view.setUint16(central + 8, view.getUint16(central + 8, true) | 1, true); // set the encryption flag
  assert.throws(() => readZip(zip), /is encrypted/);
});

test("more than 65535 entries is an error, not a silently broken archive", () => {
  const many = (n) => Array.from({ length: n }, (_, i) => ({ path: `f${i}`, type: "file", data: new Uint8Array(0) }));
  writeZip(many(65535));
  assert.throws(() => writeZip(many(65536)), /too many entries/);
});

test("a large file is written and read back intact", async () => {
  const big = new Uint8Array(20 * 1024 * 1024);
  for (let i = 0; i < big.length; i += 4099) big[i] = i & 0xff;
  const back = readZip(await bytesOf(writeZip([{ path: "big.bin", type: "file", data: big }])));
  assert.equal(back[0].data.length, big.length);
  assert.equal(crc32(back[0].data), crc32(big));
});

test("the result is a Blob typed application/zip that can be handed to a download", () => {
  const blob = writeZip(sample);
  assert.ok(blob instanceof Blob);
  assert.equal(blob.type, "application/zip");
});

test("an empty archive is valid", async () => {
  const zip = await bytesOf(writeZip([]));
  assert.equal(zip.length, 22);
  assert.deepEqual(readZip(zip), []);
  python(`import sys,io,zipfile; assert zipfile.ZipFile(io.BytesIO(sys.stdin.buffer.read())).namelist()==[]`, zip);
});
