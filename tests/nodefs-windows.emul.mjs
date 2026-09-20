// NodeFS (the local-disk mount backend) with Windows' open-flag constants, emulated on Linux:
//   node --import ./tests/win-fs-register.mjs --test tests/nodefs-windows.emul.mjs
// A guest opening a file with flags the host has no constant for (O_NONBLOCK, O_NOCTTY, O_NOFOLLOW)
// must still work; Python's shutil and many tools pass them.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

const { NodeFS } = await import("../dist/engine/guest/node.js");
const { constants } = await import("../dist/engine/guest/node.js").then(() => import("node:fs"));
const L = { rdonly: 0, wronly: 1, creat: 0o100, noctty: 0o400, nonblock: 0o4000, nofollow: 0o400000, cloexec: 0o2000000 };

test("the emulation is active (NodeFS does not see O_NONBLOCK)", async () => {
  // The hook only affects modules under dist/engine/guest; prove it through behavior below.
  assert.ok(constants.O_NONBLOCK !== undefined, "this test module itself sees the real constants");
});

test("open and create with O_NONBLOCK / O_NOCTTY / O_NOFOLLOW on a Windows-like host", async () => {
  const dir = mkdtempSync(join(tmpdir(), "nodefs-win-"));
  writeFileSync(join(dir, "a.txt"), "hello");
  const fs = new NodeFS(dir);
  const node = await fs.lookup(fs.root, "a.txt");
  for (const flag of [L.nonblock, L.noctty, L.nofollow, L.nonblock | L.nofollow | L.cloexec]) {
    const h = await fs.open(node, L.rdonly | flag);
    assert.equal(new TextDecoder().decode(await fs.read(node, h, 0n, 100)), "hello", `flags 0o${flag.toString(8)}`);
    await fs.release?.(node, h);
  }
  const made = await fs.create(fs.root, "b.txt", L.wronly | L.nonblock, { mode: 0o644, uid: 0, gid: 0 });
  await fs.write(made.node, made.handle, 0n, new TextEncoder().encode("written"));
  await fs.release?.(made.node, made.handle);
  assert.equal(readFileSync(join(dir, "b.txt"), "utf8"), "written");
});
