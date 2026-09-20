// Tests for host/ or web/workspace-panel.js in jsdom.
//   node --import ./tests/importmap-register.mjs --test tests/workspace-panel.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";
import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><body></body>");
globalThis.document = dom.window.document;
const { createWorkspacePanel, formatBytes } = await import("../web/workspace-panel.js");
const { ZipStore } = await import("../host/zip-store.js");
const { readZip } = await import("../host/zip.js");

const enc = new TextEncoder();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const ctx = { mode: 0o644, uid: 0, gid: 0 };

function put(store, dir, name, text) {
  const { node, handle } = store.create(dir, name, 0, ctx);
  store.write(node, handle, 0n, enc.encode(text));
  store.release(node, handle);
  return node;
}
function mount(store, options = {}) {
  const root = document.createElement("section");
  document.body.append(root);
  const panel = createWorkspacePanel(root, store, { refreshMs: 10, ...options });
  const q = (selector) => root.querySelector(selector);
  const names = () => [...root.querySelectorAll(".ws-row .ws-name")].map((n) => n.textContent);
  return { root, panel, q, names };
}

test("formatBytes", () => {
  assert.equal(formatBytes(0), "0 B");
  assert.equal(formatBytes(1023), "1023 B");
  assert.equal(formatBytes(1536), "1.5 KB");
  assert.equal(formatBytes(20 * 1024), "20 KB");
  assert.equal(formatBytes(3 * 1024 * 1024), "3.0 MB");
  assert.equal(formatBytes(256 * 1024 * 1024), "256 MB");
  assert.equal(formatBytes(5 * 1024 ** 3), "5.0 GB");
});

test("empty workspace shows the hint and no list", () => {
  const { q } = mount(new ZipStore());
  assert.equal(q(".ws-empty").hidden, false);
  assert.match(q(".ws-summary").textContent, /^0 files · 0 B of 256 MB · not saved between visits/);
  assert.equal(q(".ws-list").childElementCount, 0);
});

test("lists files, directories and links with sizes; the summary counts files, not directories", () => {
  const s = new ZipStore();
  const d = s.mkdir(s.root, "src", { mode: 0o755, uid: 0, gid: 0 });
  put(s, d, "main.c", "int main(){}\n");
  put(s, s.root, "notes.txt", "x".repeat(2048));
  s.symlink(s.root, "link", "notes.txt", ctx);
  const { q, names, root } = mount(s);
  assert.deepEqual(names(), ["link", "notes.txt", "src/", "src/main.c"]);
  assert.equal(q(".ws-empty").hidden, true);
  assert.match(q(".ws-summary").textContent, /^3 files · 2\.0 KB of 256 MB/);
  const rows = [...root.querySelectorAll(".ws-row")];
  assert.match(rows.find((r) => r.textContent.startsWith("notes.txt")).textContent, /2\.0 KB/);
  assert.match(rows.find((r) => r.textContent.startsWith("link")).querySelector(".ws-size").textContent, /^link$/);
  assert.ok(rows.find((r) => r.textContent.startsWith("src/")).classList.contains("dir"));
});

test("the list follows changes, redrawn in bursts rather than once per write", async () => {
  const s = new ZipStore();
  const { names, panel, root } = mount(s);
  let redraws = 0;
  new dom.window.MutationObserver(() => redraws++).observe(root.querySelector(".ws-list"), { childList: true });
  for (let i = 0; i < 30; i++) put(s, s.root, `f${String(i).padStart(2, "0")}`, "x");
  assert.deepEqual(names(), [], "not redrawn synchronously");
  await sleep(60);
  assert.equal(names().length, 30);
  assert.ok(redraws <= 2, `30 writes caused ${redraws} redraws`);
  s.unlink(s.root, "f00");
  await sleep(40);
  assert.equal(names().length, 29);
  panel.dispose();
  put(s, s.root, "after-dispose", "x");
  await sleep(40);
  assert.equal(names().length, 29, "a disposed panel stops updating");
});

test("guest-controlled names are text, never markup", async () => {
  const s = new ZipStore();
  const evil = '"><img src=x onerror=alert(1)><script>window.pwned=1</script>';
  put(s, s.root, evil, "x");
  const { root, names } = mount(s);
  assert.equal(root.querySelectorAll("img, script").length, 0);
  assert.deepEqual(names(), [evil]);
});

test("save status: memory only, unsaved, saved, and failed", async () => {
  let fail = false;
  const backing = {
    async load() {},
    async save() {
      if (fail) throw new Error("quota exceeded");
    },
  };
  const s = new ZipStore({ backing, saveDelayMs: 60_000 });
  const { q, panel } = mount(s);
  put(s, s.root, "a", "1");
  panel.render();
  assert.match(q(".ws-summary").textContent, /unsaved changes/);
  await s.saveNow();
  panel.render();
  assert.match(q(".ws-summary").textContent, /saved \d\d:\d\d:\d\d/);
  assert.equal(q(".ws-summary").classList.contains("bad"), false);
  fail = true;
  put(s, s.root, "b", "2");
  await assert.rejects(s.saveNow(), /quota exceeded/);
  panel.render();
  assert.match(q(".ws-summary").textContent, /save failed: quota exceeded/);
  assert.equal(q(".ws-summary").classList.contains("bad"), true);
});

test("Download zip hands the whole workspace to the download function as a valid archive", async () => {
  const s = new ZipStore();
  const d = s.mkdir(s.root, "out", { mode: 0o755, uid: 0, gid: 0 });
  put(s, d, "result.json", '{"ok":true}');
  put(s, s.root, "run.sh", "#!/bin/sh\n");
  let got;
  const { q } = mount(s, { download: (blob, filename) => (got = { blob, filename }) });
  q(".ws-download").click();
  assert.equal(got.filename, "workspace.zip");
  assert.equal(got.blob.type, "application/zip");
  const entries = readZip(new Uint8Array(await got.blob.arrayBuffer()));
  assert.deepEqual(entries.map((e) => e.path), ["out", "out/result.json", "run.sh"]);
  assert.equal(new TextDecoder().decode(entries[1].data), '{"ok":true}');

  const named = mount(s, { download: (blob, filename) => (got = { blob, filename }), filename: "agent-run-7.zip" });
  named.q(".ws-download").click();
  assert.equal(got.filename, "agent-run-7.zip");
});

test("a workspace that cannot be zipped says why instead of failing silently", () => {
  const s = new ZipStore();
  s.toZipBlob = () => {
    throw new Error("too many entries for a ZIP archive without ZIP64 (65535)");
  };
  const { q } = mount(s, { download: () => assert.fail("nothing to download") });
  q(".ws-download").click();
  assert.equal(q(".ws-notice").hidden, false);
  assert.match(q(".ws-notice").textContent, /Could not build the zip: too many entries/);
});

test("a notice from the page (failed restore) is shown, and can be replaced", () => {
  const { q, panel } = mount(new ZipStore(), { notice: "The saved workspace could not be restored." });
  assert.equal(q(".ws-notice").hidden, false);
  assert.match(q(".ws-notice").textContent, /could not be restored/);
  panel.setNotice("");
  assert.equal(q(".ws-notice").hidden, true);
});

test("a very long list is cut for display but says the zip has everything", async () => {
  const s = new ZipStore();
  for (let i = 0; i < 350; i++) put(s, s.root, `file-${String(i).padStart(3, "0")}`, "x");
  const { root, q } = mount(s);
  assert.equal(root.querySelectorAll(".ws-row").length, 300);
  assert.match(q(".ws-more").textContent, /and 50 more \(all are in the zip\)/);
  assert.match(q(".ws-summary").textContent, /^350 files/);
});
