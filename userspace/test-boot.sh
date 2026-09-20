#!/usr/bin/env bash
# Boot release/static/kernel/vmlinux.wasm with an initramfs under Node and drive the
# shell over the virtio console. No browser needed. Uses dist/engine and host/*.js as shipped.
#
#   ./test-boot.sh [initramfs.cpio] [cmd ...]     # default cpio: out/initramfs.cpio
#   NET=mock ./test-boot.sh out/initramfs.cpio 'wget -qO- http://example.com/x'
#
# NET=mock  attach the virtual NIC; the host fetch() callback answers with a canned body
# NET=real  the page's own app/host-fetch.js (as shipped in release/) doing real fetch()es
# EXTRA_CPIO=a.cpio:b.cpio  concatenate these overlay archives after the base one, as the page does
# WORK=1    mount a ZipStore at /work (virtiofs); at the end the store is written to
#           $WORK_OUT (default: work.zip next to the boot script) for external checking
#           WORK_STATE=file  persist the workspace there, as the page does in IndexedDB: it is
#           loaded at boot and saved at the end (two boots with the same file = two visits)
#           WORK_MAX_BYTES=n  capacity limit of the workspace
# API=mock  attach the HTTP request bridge (vsock, `hfetch` in the guest) backed by a fake fetch
# API=real  same, with the real fetch()
# Every request that reaches the host callback is printed as `[host-fetch] METHOD URL`.
#
# Each cmd is typed into the shell in order; after the last one the script prints a
# marker and exits 0 if it comes back. Exit 3 on timeout, 1 on host error/panic.
set -euo pipefail
U="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$U")"
export PATH="$ROOT/.tools/node/bin:$PATH"

CPIO="$(realpath "${1:-$U/out/initramfs.cpio}")"; shift || true  # node runs from a temp dir
CMDS=("$@")
[[ ${#CMDS[@]} -gt 0 ]] || CMDS=("uname -a" "echo hello from \$0" "ls /" "cat /proc/version" "echo pid1=\$(cat /proc/1/comm)")

# Lay the release bundle out as node_modules so the bare @lowland/* imports resolve
# the same way the page's import map resolves them.
W="${TMPDIR:-/tmp}/collabo-test-boot"
R="$ROOT/dist/engine"
rm -rf "$W"; mkdir -p "$W/node_modules/@lowland"
cp -r "$R/kernel/bytes" "$W/node_modules/@lowland/bytes"
echo '{"name":"@lowland/bytes","type":"module","exports":"./index.js"}' > "$W/node_modules/@lowland/bytes/package.json"
mkdir "$W/node_modules/@lowland/kernel"
cp -r "$R/kernel/dist" "$R/kernel/vmlinux.wasm" "$W/node_modules/@lowland/kernel/"
echo '{"name":"@lowland/kernel","type":"module","exports":{".":"./dist/index.js","./plugin":"./dist/plugin.js"}}' > "$W/node_modules/@lowland/kernel/package.json"
cp -r "$R/guest" "$W/node_modules/@lowland/guest"
echo '{"name":"@lowland/guest","type":"module","exports":{".":"./index.js"}}' > "$W/node_modules/@lowland/guest/package.json"
mkdir -p "$W/app" && cp "$ROOT"/host/*.js "$W/app/"
echo '{"type":"module"}' > "$W/package.json"

cat > "$W/boot.mjs" <<'EOF'
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { bootMachine, consoleDevice, entropyDevice, fileSystemDevice, MachinePanicError } from "@lowland/kernel";
import { attachNetworkDevice, createNetwork, hostFetchNetwork } from "@lowland/guest";

const [cpioPath, ...cmds] = process.argv.slice(2);
const netMode = process.env.NET; // "mock" | "real" | undefined
const MARK = "__DONE_" + Date.now();
const enc = new TextEncoder();
let ctl;
const stdin = new ReadableStream({ start(c) { ctl = c; } });
const send = (s) => ctl.enqueue(enc.encode(s));

let seen = "";
let started = false;
const onOut = (chunk) => {
  const s = Buffer.from(chunk).toString();
  process.stdout.write(s);
  seen += s;
  if (!started && /starting \/bin\/sh/.test(seen)) {
    started = true;
    setTimeout(() => { for (const c of cmds) send(c + "\n"); send(`echo ${MARK}\n`); }, 2500);
  }
  // The echoed command line also contains MARK; the real output is at line start.
  if (new RegExp(`^${MARK}\\r?$`, "m").test(seen)) void finish(0);
};
const stdout = new WritableStream({ write: onOut });
const tty = consoleDevice(stdin, stdout);
const bootOut = new WritableStream({ write: onOut });

const hostRequests = [];
// Stand-in for the page's request-log panel: same interface, prints to the console.
const consoleLog = {
  start(method, url, info) {
    hostRequests.push({ method, url, bodyBytes: info?.bodyBytes ?? 0 });
    console.log(`\n[host-fetch${info?.source ? ":" + info.source : ""}] ${method} ${url}${info?.bodyBytes ? ` (request body ${info.bodyBytes} bytes)` : ""}`);
    return { at: performance.now() };
  },
  finish(entry, status, label) {
    console.log(`[host-fetch]   -> ${label ?? status} (${Math.round(performance.now() - entry.at)} ms)`);
  },
  fail(entry, error) {
    console.log(`[host-fetch]   -> FAILED: ${error?.message ?? error}`);
  },
};
const plugins = [tty, entropyDevice()];
const args = [];

// A fake internet for API=mock: echoes what it was asked, plus a few special paths.
async function mockFetch(url, init) {
  const u = new URL(url);
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  if (u.pathname === "/fail") throw new TypeError("Failed to fetch");
  if (u.pathname.startsWith("/status/")) return new Response(`status ${u.pathname.slice(8)}\n`, { status: +u.pathname.slice(8) });
  if (u.pathname === "/big") return new Response(new Uint8Array(200 * 1024).fill(0x61), { status: 200 });
  if (u.pathname === "/stream") {
    return new Response(new ReadableStream({
      async start(c) {
        for (const part of ["one\n", "two\n", "three\n"]) { c.enqueue(new TextEncoder().encode(part)); await sleep(150); }
        c.close();
      },
    }), { status: 200, headers: { "content-type": "text/event-stream" } });
  }
  if (u.pathname === "/cut") {
    let n = 0;
    return new Response(new ReadableStream({ pull(c) { if (n++ === 0) c.enqueue(new TextEncoder().encode("partial")); else c.error(new Error("connection reset")); } }), { status: 200 });
  }
  const body = init.body ? new TextDecoder().decode(init.body) : "";
  return new Response(JSON.stringify({ method: init.method, url, headers: Object.fromEntries(init.headers), body, credentials: init.credentials, redirect: init.redirect }) + "\n",
    { status: 200, statusText: "OK", headers: { "content-type": "application/json", "x-mock": "1" } });
}
if (process.env.API) {
  const { createHttpBridge } = await import("./app/http-bridge.js");
  const bridge = createHttpBridge({ log: consoleLog, ...(process.env.API === "mock" ? { fetch: mockFetch } : {}) });
  plugins.push(bridge.device);
}
if (netMode) {
  const { createHostFetch } = await import("./app/host-fetch.js");
  const realFetch = createHostFetch({ log: consoleLog });
  const network = createNetwork(
    hostFetchNetwork({
      async fetch(request) {
        if (netMode === "real") return realFetch(request);
        const record = { method: request.method, url: request.url, headers: Object.fromEntries(request.headers) };
        if (request.method !== "GET" && request.method !== "HEAD") record.body = await request.clone().text();
        hostRequests.push(record);
        console.log(`\n[host-fetch] ${request.method} ${request.url}`);
        if (netMode === "mock") {
          return new Response(`mock response for ${request.url}\n`, { status: 200, headers: { "content-type": "text/plain" } });
        }
        throw new Error("unreachable");
      },
    }),
  );
  const nic = attachNetworkDevice(network);
  args.push(`collabo.ip=${nic.address}`, `collabo.gw=${network.gateway}`);
  plugins.push(nic);
}

let store;
if (process.env.WORK) {
  const { ZipStore } = await import("./app/zip-store.js");
  const state = process.env.WORK_STATE;
  const backing = state && {
    async load() { return existsSync(state) ? new Blob([readFileSync(state)]) : undefined; },
    async save(blob) { writeFileSync(state, new Uint8Array(await blob.arrayBuffer())); },
  };
  store = new ZipStore({ backing, maxBytes: Number(process.env.WORK_MAX_BYTES) || undefined });
  await store.load();
  plugins.push(fileSystemDevice(store, { tag: "work" }));
  args.push("collabo.mount=work:/work");
}

async function finish(code) {
  if (store) {
    await store.saveNow();
    const out = process.env.WORK_OUT ?? "work.zip";
    writeFileSync(out, new Uint8Array(await store.toZipBlob().arrayBuffer()));
    console.log(`\n[test] workspace: ${JSON.stringify(store.stats())}\n[test] wrote ${out}`);
  }
  if (hostRequests.length) console.log("\n[test] host saw:\n" + JSON.stringify(hostRequests, null, 1));
  console.log(code === 0 ? "\n[test] shell responded, marker seen" : "\n[test] failed");
  process.exit(code);
}

// Archives placed back to back are unpacked in order by the kernel.
const initcpio = Buffer.concat([cpioPath, ...(process.env.EXTRA_CPIO ?? "").split(":").filter(Boolean)].map((f) => readFileSync(f)));

setTimeout(() => { console.log("\n[test] TIMEOUT"); process.exit(3); }, Number(process.env.TEST_TIMEOUT_MS) || 120000);
try {
  const m = await bootMachine({ cpus: 2, args, plugins, initcpio: initcpio });
  m.bootConsole.pipeTo(bootOut).catch(() => {});
  await m.closed;
  console.log("\n[test] machine closed");
} catch (e) {
  console.log(`\n[test] ${e instanceof MachinePanicError ? "KERNEL PANIC" : "HOST ERROR: " + e?.stack}`);
  process.exit(1);
}
EOF
cd "$W" && exec node boot.mjs "$CPIO" "${CMDS[@]}"
