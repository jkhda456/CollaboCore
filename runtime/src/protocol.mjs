// The control protocol between an app (the Flutter/Dart package) and the runtime process:
// newline-delimited JSON on the runtime's stdin/stdout. stderr is free-form diagnostics.
//
//   app -> runtime   {"id": 1, "method": "exec", "params": {...}}
//   runtime -> app   {"id": 1, "result": {...}}   or   {"id": 1, "error": {"kind": "...", "message": "..."}}
//   runtime -> app   {"event": "console", ...}     (no id; see EVENTS below)
//
// Methods
//   start          {config}          boot; answers once the guest agent is ready -> {version, config}
//   exec           {argv | command, cwd?, env?, stdin?, stdinBase64?, timeoutMs?, stream?}
//                                    -> {exitCode, signal, timedOut, truncated, durationMs,
//                                        stdoutBase64, stderrBase64}; with stream: true also
//                                        "execOutput" events {execId: id, stream, dataBase64}
//                                    `command` is a string run by /bin/sh -c
//   readFile       {path}            -> {dataBase64}               (a guest path)
//   writeFile      {path, data? | dataBase64?, mode?}  -> {}       (creates parent directories)
//   console.write  {data? | dataBase64?}      console.resize {cols, rows}
//   policy.update  {network?, hostExec?, hostFunctions?}
//   reply          {id, result? | error? | allow?}   answer a hostCall or permission event
//   exportZip      {guestPath, outFile}   zip a mounted folder (stored, uncompressed) -> {entries, bytes}
//   stop           {}                -> {} and the process exits
//
// Events: ready, console {dataBase64}, network {...}, hostCall {callId, fn, args},
//         permission {requestId, kind, argv?, cwd?, gui?, target?}, execOutput, exit {reason, message?}
import { createWriteStream } from "node:fs";
import { lstat, readdir, readFile, readlink, stat } from "node:fs/promises";
import { join, relative, sep } from "node:path";
import { createInterface } from "node:readline";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { writeZip } from "../host/zip.js";
import { Sandbox } from "./sandbox.mjs";

export const PROTOCOL_VERSION = 1;
const b64 = (bytes) => Buffer.from(bytes).toString("base64");
const unb64 = (text) => new Uint8Array(Buffer.from(text, "base64"));

class ProtocolError extends Error {
  constructor(kind, message) {
    super(message);
    this.kind = kind;
  }
}

/** Walks a host folder into zip entries (files, directories, symlinks). */
async function* folderEntries(root) {
  const walk = async function* (dir) {
    for (const name of (await readdir(dir)).sort()) {
      const full = join(dir, name);
      const path = relative(root, full).split(sep).join("/");
      const info = await lstat(full);
      const common = { path, mode: info.mode & 0o7777, mtime: Math.floor(info.mtimeMs / 1000) };
      if (info.isSymbolicLink()) yield { ...common, type: "symlink", data: new TextEncoder().encode(await readlink(full)) };
      else if (info.isDirectory()) {
        yield { ...common, type: "dir" };
        yield* walk(full);
      } else if (info.isFile()) yield { ...common, type: "file", data: new Uint8Array(await readFile(full)) };
    }
  };
  yield* walk(root);
}

export function serveStdio(baseConfig = {}) {
  let sandbox;
  let starting;
  const send = (message) => process.stdout.write(JSON.stringify(message) + "\n");
  const event = (name, fields = {}) => send({ event: name, ...fields });

  function requireSandbox() {
    if (!sandbox) throw new ProtocolError("not-started", "call start first");
    return sandbox;
  }

  async function exec(p, id) {
    const sb = requireSandbox();
    let argv = p.argv;
    if (typeof p.command === "string") argv = ["/bin/sh", "-c", p.command];
    const stdin = p.stdinBase64 !== undefined ? unb64(p.stdinBase64) : p.stdin !== undefined ? new TextEncoder().encode(String(p.stdin)) : undefined;
    const stream = (name) => (p.stream ? (bytes) => event("execOutput", { execId: id, stream: name, dataBase64: b64(bytes) }) : undefined);
    const r = await sb.exec({ argv, cwd: p.cwd, env: p.env, stdin, timeoutMs: p.timeoutMs ?? 120_000, onStdout: stream("stdout"), onStderr: stream("stderr") });
    return {
      exitCode: r.exitCode, signal: r.signal, timedOut: r.timedOut, truncated: r.truncated, durationMs: r.durationMs,
      stdoutBase64: b64(r.stdout), stderrBase64: b64(r.stderr),
    };
  }

  const methods = {
    async start(p) {
      if (sandbox || starting) throw new ProtocolError("already-started", "the sandbox is already running");
      starting = Sandbox.start({ ...baseConfig, ...(p?.config ?? {}) });
      try {
        sandbox = await starting;
      } finally {
        starting = undefined;
      }
      sandbox.on("console", (bytes) => event("console", { dataBase64: b64(bytes) }));
      sandbox.on("network", (e) => event("network", e));
      sandbox.on("hostCall", (e) => event("hostCall", { callId: e.callId, fn: e.fn, args: e.args }));
      sandbox.on("permission", (e) => {
        const { type, ...rest } = e;
        void type;
        event("permission", rest);
      });
      sandbox.on("exit", (e) => {
        event("exit", e);
        setTimeout(() => process.exit(e.reason === "stopped" ? 0 : 1), 50);
      });
      await sandbox.ready;
      event("ready");
      const { network, ...config } = sandbox.config;
      return {
        version: PROTOCOL_VERSION,
        config: { ...config, network: network && { ...network, secrets: network.secrets.map((s) => ({ host: s.host, header: s.header })) } },
      };
    },
    exec,
    async readFile(p) {
      if (typeof p?.path !== "string") throw new ProtocolError("bad-request", "path is required");
      const r = await exec({ argv: ["cat", "--", p.path] });
      if (r.exitCode !== 0) throw new ProtocolError("failed", Buffer.from(r.stderrBase64, "base64").toString().trim() || `cat exited ${r.exitCode}`);
      return { dataBase64: r.stdoutBase64 };
    },
    async writeFile(p) {
      if (typeof p?.path !== "string") throw new ProtocolError("bad-request", "path is required");
      const data = p.dataBase64 !== undefined ? p.dataBase64 : Buffer.from(String(p.data ?? "")).toString("base64");
      const mode = p.mode === undefined ? "" : `chmod ${Number(p.mode).toString(8)} "$1" && `;
      const r = await exec({
        argv: ["/bin/sh", "-c", `mkdir -p "$(dirname "$1")" && cat > "$1" && ${mode}true`, "sh", p.path],
        stdinBase64: data,
      });
      if (r.exitCode !== 0) throw new ProtocolError("failed", Buffer.from(r.stderrBase64, "base64").toString().trim());
      return {};
    },
    "console.write"(p) {
      const sb = requireSandbox();
      void sb.writeConsole(p.dataBase64 !== undefined ? unb64(p.dataBase64) : String(p.data ?? ""));
      return {};
    },
    "console.resize"(p) {
      requireSandbox().resizeConsole(p.cols, p.rows);
      return {};
    },
    "policy.update"(p) {
      requireSandbox().updatePolicy(p ?? {});
      return {};
    },
    reply(p) {
      if (!requireSandbox().reply(p.id, p)) throw new ProtocolError("unknown-id", `nothing is waiting for an answer with id ${p.id}`);
      return {};
    },
    async exportZip(p) {
      const sb = requireSandbox();
      const mount = sb.config.mounts.find((m) => m.guestPath === p?.guestPath);
      if (!mount) throw new ProtocolError("bad-request", `${p?.guestPath} is not a mounted folder (${sb.config.mounts.map((m) => m.guestPath).join(", ") || "none"})`);
      if (typeof p.outFile !== "string") throw new ProtocolError("bad-request", "outFile is required");
      await stat(mount.hostPath);
      const entries = [];
      for await (const e of folderEntries(mount.hostPath)) entries.push(e);
      const blob = writeZip(entries);
      await pipeline(Readable.fromWeb(blob.stream()), createWriteStream(p.outFile));
      return { entries: entries.length, bytes: blob.size };
    },
    stop() {
      if (sandbox) sandbox.stop();
      else setTimeout(() => process.exit(0), 20);
      return {};
    },
  };

  const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
  lines.on("line", async (line) => {
    if (!line.trim()) return;
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      return send({ id: null, error: { kind: "bad-request", message: "not valid JSON" } });
    }
    const { id, method, params } = message;
    const handler = Object.hasOwn(methods, method) ? methods[method] : undefined;
    if (!handler) return send({ id, error: { kind: "unknown-method", message: `no method "${method}"` } });
    try {
      send({ id, result: await handler(params ?? {}, id) });
    } catch (error) {
      send({ id, error: { kind: error.kind ?? "failed", message: String(error?.message ?? error) } });
    }
  });
  // The app went away: do not leave a sandbox running without an owner.
  lines.on("close", () => {
    sandbox?.stop();
    setTimeout(() => process.exit(0), 50);
  });
}
