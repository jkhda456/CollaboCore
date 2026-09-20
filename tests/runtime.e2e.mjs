// End-to-end tests of the packaged desktop runtime (dist/runtime/collabo-core-<platform>), driven
// through its stdio control protocol exactly as the Dart package drives it.
//   node --test tests/runtime.e2e.mjs
// Portable: needs nothing on the host but Node (no openssl, python, sh), so it runs unchanged on
// Linux, macOS and Windows. COLLABO_CORE_RUNTIME may point at the runtime folder (or its parent).
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync, existsSync, mkdirSync } from "node:fs";
import { createServer as createHttpsServer } from "node:https";
import { createServer as createTcpServer } from "node:net";
import { tmpdir, platform, arch } from "node:os";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";
import { fileURLToPath } from "node:url";
import { readZip } from "../host/zip.js";

const PLATFORM = `${platform() === "win32" ? "win" : platform()}-${arch()}`;
const RUNTIME = (() => {
  const base = process.env.COLLABO_CORE_RUNTIME ?? fileURLToPath(new URL("../dist/runtime/", import.meta.url));
  return existsSync(join(base, "manifest.json")) ? base : join(base, `collabo-core-${PLATFORM}`);
})();
// The host program the sandbox is allowed to run in the tests: this Node itself, which exists on
// every platform (unlike sh, echo or touch on Windows).
const HOST_NODE = process.execPath;
const FIXTURES = fileURLToPath(new URL("./fixtures/", import.meta.url));
// How to start it is the manifest's business (the native engine takes its images as
// arguments), so the test reads `entry` exactly as the Dart package does.
const MANIFEST = JSON.parse(readFileSync(join(RUNTIME, "manifest.json"), "utf8"));
const PROGRAM = join(RUNTIME, MANIFEST.entry[0]);
const ENTRY_ARGS = MANIFEST.entry.slice(1);
const SECRET = "sk-test-0123456789abcdef";
const b64 = (s) => Buffer.from(s).toString("base64");
const unb64 = (s) => Buffer.from(s, "base64").toString();

class Client {
  constructor(env = {}) {
    this.proc = spawn(PROGRAM, [...ENTRY_ARGS, "--stdio"], {
      cwd: RUNTIME,
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, ...env },
    });
    this.nextId = 1;
    this.pending = new Map();
    this.events = [];
    this.waiters = [];
    this.stderr = "";
    this.console = "";
    this.proc.stderr.on("data", (d) => ((this.stderr += d), process.env.COLLABO_DEBUG && process.stderr.write(d)));
    let buf = "";
    this.proc.stdout.on("data", (d) => {
      buf += d;
      let nl;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl);
        buf = buf.slice(nl + 1);
        const msg = JSON.parse(line);
        if (msg.event) {
          if (msg.event === "console") this.console += unb64(msg.dataBase64);
          if (msg.event === "exit") this.exitEvent = msg;
          this.events.push(msg);
          this.waiters = this.waiters.filter((w) => !(w.match(msg) && (w.resolve(msg), true)));
          this.onEvent?.(msg);
        } else {
          const p = this.pending.get(msg.id);
          this.pending.delete(msg.id);
          msg.error ? p.reject(Object.assign(new Error(this.explain(msg.error.message)), { kind: msg.error.kind })) : p.resolve(msg.result);
        }
      }
    });
    this.exited = new Promise((resolve) => this.proc.once("exit", (code) => resolve(code)));
  }
  /** Adds why the machine is gone (kernel console, runtime stderr) to an error message. */
  explain(message) {
    if (!this.exitEvent && !this.proc.exitCode) return message;
    const console_tail = this.console.split("\n").slice(-12).join("\n");
    return `${message}\n  [machine ${this.exitEvent ? `${this.exitEvent.reason}: ${this.exitEvent.message ?? ""}` : `runtime exited ${this.proc.exitCode}`}]` +
      `\n  [guest console tail]\n${console_tail}\n  [runtime stderr]\n${this.stderr.split("\n").slice(-12).join("\n")}`;
  }

  call(method, params = {}) {
    const id = this.nextId++;
    this.proc.stdin.write(JSON.stringify({ id, method, params }) + "\n");
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }
  waitEvent(match, timeoutMs = 30_000) {
    const hit = this.events.find(match);
    if (hit) return Promise.resolve(hit);
    return new Promise((resolve, reject) => {
      const t = setTimeout(() => reject(new Error("event timeout")), timeoutMs);
      this.waiters.push({ match, resolve: (m) => (clearTimeout(t), resolve(m)) });
    });
  }
  async sh(command, extra = {}) {
    const r = await this.call("exec", { command, ...extra }).catch((e) => {
      throw Object.assign(new Error(this.explain(e.message)), { kind: e.kind });
    });
    return { ...r, stdout: unb64(r.stdoutBase64), stderr: unb64(r.stderrBase64) };
  }
}

// ---- fixtures: host folders, a local HTTPS echo server with a self-signed cert, a TCP echo ----
const tmp = mkdtempSync(join(tmpdir(), "collabo-rt-"));
const work = join(tmp, "work");
const docs = join(tmp, "docs");
mkdirSync(work);
mkdirSync(docs);
writeFileSync(join(work, "hello.txt"), "from the host\n");
writeFileSync(join(docs, "readme.md"), "read only\n");
const seen = [];
const https = createHttpsServer({ key: readFileSync(join(FIXTURES, "localhost-key.pem")), cert: readFileSync(join(FIXTURES, "localhost-cert.pem")) }, (req, res) => {
  seen.push({ url: req.url, headers: req.headers });
  res.setHeader("content-type", "application/json");
  res.end(JSON.stringify({ path: req.url, hasKey: req.headers["x-api-key"] === SECRET, sawKeyHeader: "x-api-key" in req.headers }));
});
const tcp = createTcpServer((s) => s.on("data", (d) => s.end("echo:" + d)));
let HTTPS_PORT, TCP_PORT;

let c;
before(async () => {
  await new Promise((r) => https.listen(0, "127.0.0.1", r));
  await new Promise((r) => tcp.listen(0, "127.0.0.1", r));
  HTTPS_PORT = https.address().port;
  TCP_PORT = tcp.address().port;
  assert.ok(existsSync(PROGRAM), `no runtime package at ${RUNTIME}; run PLATFORMS=${PLATFORM} scripts/package-runtime.sh`);
  // The engine must trust the test certificate, as it would a real CA-signed one.
  c = new Client({ COLLABO_CORE_CA_FILE: join(FIXTURES, "localhost-ca.pem") });
  c.onEvent = (m) => {
    if (m.event === "hostCall" && m.fn === "app.greet") void c.call("reply", { id: m.callId, result: { greeting: `hello ${m.args.name}` } });
    if (m.event === "hostCall" && m.fn === "app.fail") void c.call("reply", { id: m.callId, error: { kind: "failed", message: "the app said no" } });
  };
  const started = await c.call("start", {
    config: {
      cpus: 2,
      quiet: true,
      mounts: [{ hostPath: work, guestPath: "/work" }, { hostPath: docs, guestPath: "/docs", readOnly: true }],
      network: {
        allow: ["localhost", "*.allowed.test", "example.com"],
        deny: ["blocked.allowed.test"],
        allowHostLoopback: true,
        secrets: [{ host: "localhost", header: "x-api-key", value: SECRET }],
      },
      hostExec: "ask",
      hostFunctions: ["app.greet", "app.fail"],
    },
  });
  assert.equal(started.version, 1);
  assert.deepEqual(started.config.network.secrets, [{ host: "localhost", header: "x-api-key" }], "secret values are not echoed back");
});
after(async () => {
  if (c && c.proc.exitCode === null) {
    await c.call("stop").catch(() => {});
    await c.exited;
  }
  https.close();
  tcp.close();
});

describe("exec", () => {
  test("runs a command; output, exit code and duration come back", async () => {
    const r = await c.sh("echo out; echo err >&2; exit 3");
    assert.equal(r.stdout, "out\n");
    assert.equal(r.stderr, "err\n");
    assert.equal(r.exitCode, 3);
    assert.equal(r.timedOut, false);
    assert.ok(r.durationMs >= 0);
  });
  test("argv without a shell, cwd, env, stdin", async () => {
    const r = await c.call("exec", { argv: ["sh", "-c", 'pwd; echo "$FOO"; cat'], cwd: "/work", env: { FOO: "bar baz" }, stdin: "piped\n" });
    assert.equal(unb64(r.stdoutBase64), "/work\nbar baz\npiped\n");
  });
  test("binary stdin and stdout are exact", async () => {
    const bytes = Buffer.from(Array.from({ length: 70000 }, (_, i) => (i * 7) & 255));
    const r = await c.call("exec", { argv: ["cat"], stdinBase64: bytes.toString("base64") });
    assert.ok(Buffer.from(r.stdoutBase64, "base64").equals(bytes));
  });
  test("a missing program is an error, not a hang", async () => {
    await assert.rejects(c.call("exec", { argv: ["no-such-program"] }), /cannot run no-such-program/);
  });
  test("timeout kills the command", async () => {
    const t0 = Date.now();
    const r = await c.sh("sleep 30", { timeoutMs: 800 });
    assert.equal(r.timedOut, true);
    assert.equal(r.signal, 9);
    assert.ok(Date.now() - t0 < 10_000);
  });
  test("streaming output arrives as execOutput events", async () => {
    const before = c.events.length;
    const r = await c.call("exec", { command: "for i in 1 2 3; do echo line$i; sleep 0.2; done", stream: true });
    const chunks = c.events.slice(before).filter((e) => e.event === "execOutput" && e.stream === "stdout").map((e) => unb64(e.dataBase64));
    assert.equal(chunks.join(""), "line1\nline2\nline3\n");
    assert.ok(chunks.length >= 2, "arrived in pieces, not all at the end");
    assert.equal(unb64(r.stdoutBase64), "line1\nline2\nline3\n");
  });
  test("commands run at the same time", async () => {
    // Proven by overlap, not by the clock (a slow or single-core host would make timing useless):
    // while two long commands are running, a third one sees both of their marker files.
    await c.sh("rm -f /tmp/mark-*");
    const long = ["a", "b"].map((name) => c.sh(`touch /tmp/mark-${name}; sleep 5; echo done-${name}`));
    let marks = "";
    try {
      for (let i = 0; i < 50; i++) {
        marks = (await c.sh("ls /tmp/mark-* 2>/dev/null | tr '\\n' ' '")).stdout;
        if (marks.includes("mark-a") && marks.includes("mark-b")) break;
        await new Promise((r) => setTimeout(r, 200));
      }
      assert.ok(marks.includes("mark-a") && marks.includes("mark-b"),
        `a third command saw "${marks.trim()}" while two others were running`);
      const rs = await Promise.all(long);
      assert.deepEqual(rs.map((r) => r.stdout.trim()), ["done-a", "done-b"]);
    } finally {
      await Promise.allSettled(long); // never leave commands running into the next test
    }
  });

  test("python is there", async () => {
    const r = await c.sh("python3 -c 'import sys, collabo_core; print(sys.version_info[:2])'");
    assert.equal(r.stdout.trim(), "(3, 13)");
  });
});

describe("files and local disk", () => {
  test("readFile / writeFile round trip, including binary and new directories", async () => {
    const data = Buffer.from([0, 1, 2, 250, 255, 10]);
    await c.call("writeFile", { path: "/tmp/a/b/c.bin", dataBase64: data.toString("base64"), mode: 0o600 });
    const back = await c.call("readFile", { path: "/tmp/a/b/c.bin" });
    assert.ok(Buffer.from(back.dataBase64, "base64").equals(data));
    assert.equal((await c.sh("stat -c %a /tmp/a/b/c.bin")).stdout.trim(), "600");
    await assert.rejects(c.call("readFile", { path: "/nope" }), /No such file/);
  });
  test("a mounted host folder: guest sees host files, host sees guest writes", async () => {
    assert.equal((await c.sh("cat /work/hello.txt")).stdout, "from the host\n");
    await c.sh("mkdir -p /work/out && echo guest-wrote > /work/out/result.txt");
    assert.equal(readFileSync(join(work, "out", "result.txt"), "utf8"), "guest-wrote\n");
    writeFileSync(join(work, "late.txt"), "written by the host while running\n");
    assert.equal((await c.sh("cat /work/late.txt")).stdout, "written by the host while running\n");
  });
  test("symbolic links in a mounted folder", { skip: platform() === "win32" && "creating symlinks on Windows needs Developer Mode or admin rights" }, async () => {
    const r = await c.sh("ln -s out/result.txt /work/link && cat /work/link && readlink /work/link");
    assert.equal(r.stdout, "guest-wrote\nout/result.txt\n", r.stderr);
  });
  test("a read-only mount refuses writes", async () => {
    assert.equal((await c.sh("cat /docs/readme.md")).stdout, "read only\n");
    const r = await c.sh("echo x > /docs/new.txt");
    assert.notEqual(r.exitCode, 0);
    assert.match(r.stderr, /Read-only file system/);
    assert.equal(existsSync(join(docs, "new.txt")), false);
  });
  test("python works on the mounted folder", async () => {
    const r = await c.sh("cd /work && python3 -c \"import pathlib; p = pathlib.Path('py.txt'); p.write_text('héllo'); print(sorted(x.name for x in pathlib.Path('.').iterdir()))\"");
    assert.match(r.stdout, /'py\.txt'/);
    assert.equal(readFileSync(join(work, "py.txt"), "utf8"), "héllo");
  });
  test("exportZip writes the mounted folder as a stored zip", async () => {
    const out = join(tmp, "work.zip");
    const r = await c.call("exportZip", { guestPath: "/work", outFile: out });
    assert.ok(r.entries >= 5);
    // readZip checks every CRC and refuses compressed entries (it is cross-checked against
    // Python's zipfile in zip.test.mjs).
    const listed = readZip(new Uint8Array(readFileSync(out))).map((e) => e.path);
    assert.ok(listed.includes("hello.txt"), listed.join(" "));
    assert.ok(listed.includes("out/result.txt"), listed.join(" "));
    await assert.rejects(c.call("exportZip", { guestPath: "/nope", outFile: out }), /not a mounted folder/);
  });
});

describe("managed network", () => {
  test("request API: allowed host works and the host injects the secret the guest never saw", async () => {
    const r = await c.sh(`hfetch https://localhost:${HTTPS_PORT}/v1/models; echo; env | grep -c ${SECRET} || true`);
    const body = JSON.parse(r.stdout.split("\n")[0]);
    assert.equal(body.hasKey, true, "server received the injected key");
    assert.equal(r.stdout.split("\n")[1], "0", "the key is not in the guest environment");
    const ev = c.events.filter((e) => e.event === "network");
    assert.ok(ev.some((e) => e.via === "api" && e.url?.includes("/v1/models") && e.phase === "response" || e.status === 200));
    assert.ok(!JSON.stringify(c.events).includes(SECRET), "the key never appears in events");
  });
  test("the guest cannot override or read the injected header", async () => {
    const r = await c.sh(`hfetch -H 'x-api-key: guest-value' https://localhost:${HTTPS_PORT}/x`);
    assert.equal(JSON.parse(r.stdout).hasKey, true, "host value wins");
  });
  test("python collabo_core goes through the same policy", async () => {
    const r = await c.sh(`python3 -c "import collabo_core as c; print(c.get('https://localhost:${HTTPS_PORT}/py').json()['hasKey'])"`);
    assert.equal(r.stdout.trim(), "True");
  });
  test("hosts outside the allow list and on the deny list are refused, with a reason", async () => {
    const a = await c.sh("hfetch https://not-listed.test/; echo rc=$?");
    assert.match(a.stderr, /denied: blocked by the network policy: "not-listed.test" is not in the allow list/);
    assert.match(a.stdout, /rc=2/);
    const b = await c.sh("hfetch https://blocked.allowed.test/");
    assert.match(b.stderr, /matches the deny rule "blocked.allowed.test"/);
    assert.ok(c.events.some((e) => e.event === "network" && e.phase === "failed" && e.errorKind === "denied"));
  });
  test("policy.update applies to the next request", async () => {
    await c.call("policy.update", { network: { allow: ["example.com"] } });
    const r = await c.sh(`hfetch https://localhost:${HTTPS_PORT}/after`);
    assert.match(r.stderr, /not in the allow list/);
    await c.call("policy.update", { network: { allow: ["localhost", "*.allowed.test", "example.com"] } });
    assert.equal(JSON.parse((await c.sh(`hfetch https://localhost:${HTTPS_PORT}/again`)).stdout).hasKey, true);
  });
  test("packet level: DNS for a denied name fails, and it is reported", async () => {
    const r = await c.sh("python3 -c \"import socket; socket.getaddrinfo('blocked.allowed.test', 80)\" 2>&1; echo rc=$?");
    assert.match(r.stdout, /rc=1/);
    assert.ok(c.events.some((e) => e.event === "network" && e.via === "net" && e.kind === "dns" && e.host === "blocked.allowed.test" && e.blocked));
  });
  test("packet level: a TCP socket to the host (192.0.2.1 = its localhost) when allowed", async () => {
    const r = await c.sh(`python3 -c "import socket; s = socket.create_connection(('192.0.2.1', ${TCP_PORT}), timeout=10); s.sendall(b'ping'); print(s.recv(100).decode())"`);
    assert.equal(r.stdout.trim(), "echo:ping", `stderr: ${r.stderr}\nnet events: ${JSON.stringify(c.events.filter((e) => e.event === "network" && e.via === "net").slice(-4))}`);
    assert.ok(c.events.some((e) => e.event === "network" && e.via === "net" && e.kind === "connect" && e.phase === "closed" && e.port === TCP_PORT));
  });
  test("packet level and request API: host loopback is refused when not allowed", async () => {
    await c.call("policy.update", { network: { allowHostLoopback: false } });
    const r = await c.sh(`python3 -c "import socket; socket.create_connection(('192.0.2.1', ${TCP_PORT}), timeout=10)" 2>&1 | tail -1`);
    assert.match(r.stdout, /Connection (refused|reset)|Errno/);
    const h = await c.sh(`hfetch https://localhost:${HTTPS_PORT}/`);
    assert.match(h.stderr, /is this computer \(allowHostLoopback is off\)/);
    await c.call("policy.update", { network: { allowHostLoopback: true } });
  });
});

describe("host functions", () => {
  test("list shows built-ins and the app's functions", async () => {
    const r = await c.sh("hostcall --list");
    assert.deepEqual(JSON.parse(r.stdout), ["list", "info", "exec", "open", "app.greet", "app.fail"]);
  });
  test("an app function is forwarded to the app and its answer returned", async () => {
    assert.deepEqual(JSON.parse((await c.sh(`hostcall app.greet '{"name":"guest"}'`)).stdout), { greeting: "hello guest" });
    const py = await c.sh(`python3 -c "import collabo_core as c; print(c.host.call('app.greet', name='py')['greeting'])"`);
    assert.equal(py.stdout.trim(), "hello py");
  });
  test("an app error and an unknown function come back as errors", async () => {
    const f = await c.sh("hostcall app.fail; echo rc=$?");
    assert.match(f.stderr, /failed: the app said no/);
    assert.match(f.stdout, /rc=1/);
    const u = await c.sh("hostcall nope");
    assert.match(u.stderr, /unknown-function/);
  });
  test("host exec asks the app; allowed runs on the host, output and status come back", async () => {
    const script = "console.log('host-side'); console.log(process.platform); process.exit(4)";
    const perm = c.waitEvent((e) => e.event === "permission" && e.kind === "exec" && e.argv?.[1] === "-e");
    const run = c.call("exec", { argv: ["hostcall", "--exec", "--", HOST_NODE, "-e", script] });
    const req = await perm;
    assert.deepEqual(req.argv, [HOST_NODE, "-e", script]);
    await c.call("reply", { id: req.requestId, allow: true });
    const r = await run;
    assert.equal(unb64(r.stdoutBase64).replace(/\r/g, ""), `host-side\n${platform()}\n`, "output of the host program, on the host");
    assert.equal(r.exitCode, 4, "its exit status");
  });
  test("host exec refused by the user", async () => {
    const target = join(tmp, "should-not-exist");
    const perm = c.waitEvent((e) => e.event === "permission" && e.argv?.[2]?.includes("should-not-exist"));
    const code = `import collabo_core as c, os, json
try:
    c.host.exec([os.environ["HOST_NODE"], "-e", "require('fs').writeFileSync(" + json.dumps(os.environ["TARGET"]) + ", 'x')"])
except c.HostCallError as e:
    print('refused', e.kind)`;
    const run = c.call("exec", { argv: ["python3", "-c", code], env: { HOST_NODE, TARGET: target } }).then((r) => ({ stdout: unb64(r.stdoutBase64) }));
    await c.call("reply", { id: (await perm).requestId, allow: false });
    assert.equal((await run).stdout.trim(), "refused denied");
    assert.equal(existsSync(target), false);
  });
  test("with hostExec deny nothing is asked; with allow it just runs (python API, GUI mode)", async () => {
    await c.call("policy.update", { hostExec: "deny" });
    const before = c.events.filter((e) => e.event === "permission").length;
    const d = await c.sh(`hostcall --exec -- "$HOST_NODE" -e 0`, { env: { HOST_NODE } });
    assert.match(d.stderr, /denied: running host programs is not allowed/);
    assert.equal(c.events.filter((e) => e.event === "permission").length, before);
    await c.call("policy.update", { hostExec: "allow" });
    const marker = join(tmp, "gui-started");
    const g = await c.call("exec", {
      argv: ["python3", "-c", `import collabo_core as c, os, json
print(c.host.exec([os.environ["HOST_NODE"], "-e", "require('fs').writeFileSync(" + json.dumps(os.environ["MARK"]) + ", 'x')"], gui=True)["pid"] > 0)`],
      env: { HOST_NODE, MARK: marker },
    }).then((r) => ({ stdout: unb64(r.stdoutBase64) }));
    assert.equal(g.stdout.trim(), "True");
    for (let i = 0; i < 50 && !existsSync(marker); i++) await new Promise((r) => setTimeout(r, 100));
    assert.ok(existsSync(marker), "the detached host program ran");
    const p = await c.call("exec", {
      argv: ["python3", "-c", "import collabo_core as c, os; r = c.host.exec([os.environ['HOST_NODE'], '-e', 'console.log(\"from python\")']); print(r.returncode, r.stdout.strip())"],
      env: { HOST_NODE },
    }).then((r) => ({ stdout: unb64(r.stdoutBase64) }));
    assert.equal(p.stdout.trim(), "0 from python");
    await c.call("policy.update", { hostExec: "ask" });
  });
});

describe("console and lifecycle", () => {
  test("the console is the guest's root shell", async () => {
    await c.call("console.write", { data: "echo console-$((6*7))\n" });
    await c.waitEvent(() => c.console.includes("console-42"));
    await c.call("console.resize", { cols: 100, rows: 30 });
  });
  test("protocol errors are answered, not fatal", async () => {
    await assert.rejects(c.call("nope"), /no method "nope"/);
    await assert.rejects(c.call("start", { config: {} }), /already running/);
    await assert.rejects(c.call("reply", { id: 99999, allow: true }), /nothing is waiting/);
  });
  test("bad configuration is refused before booting", async () => {
    const d = new Client();
    await assert.rejects(d.call("start", { config: { mounts: [{ hostPath: "/definitely/missing", guestPath: "/w" }] } }), /is not a directory/);
    await assert.rejects(d.call("start", { config: { mounts: [{ hostPath: tmp, guestPath: "/etc" }] } }), /outside system directories/);
    await assert.rejects(d.call("start", { config: { hostExec: "sometimes" } }), /hostExec must be one of/);
    d.proc.stdin.end();
    assert.equal(await d.exited, 0);
  });
  test("when the app goes away (stdin closes) the runtime stops", async () => {
    const d = new Client();
    await d.call("start", { config: { cpus: 1, quiet: true, python: false, network: false } });
    const t0 = Date.now();
    d.proc.stdin.end();
    await d.exited;
    assert.ok(Date.now() - t0 < 5000);
  });
});
