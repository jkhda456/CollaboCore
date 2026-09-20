#!/usr/bin/env node
// collaboCore runtime: the sandbox as a standalone process (no browser).
//
//   node main.mjs --stdio [--config FILE]          control protocol on stdin/stdout (for the app;
//                                                  see protocol.mjs); the config may also come in
//                                                  the first "start" request
//   node main.mjs shell [options]                  the guest's root shell in this terminal
//   node main.mjs exec [options] -- CMD [ARG...]   run one command in a fresh sandbox, print its
//                                                  output, exit with its status
//
// options: --config FILE         JSON configuration (readme.detail.md, "설정")
//          --mount HOST:GUEST[:ro] mount a local folder (repeatable)
//          --allow HOST          network allow pattern (repeatable; replaces the default "*")
//          --deny HOST           network deny pattern (repeatable)
//          --no-network          no network at all
//          --host-exec MODE      deny | ask | allow  (ask = refuse in this mode: nobody to ask)
//          --no-python           boot without the Python overlay
//          --cpus N              virtual CPUs (default: min(4, host CPUs))
//          --quiet               no guest banner
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { Sandbox } from "./sandbox.mjs";
import { serveStdio } from "./protocol.mjs";

/** Parses the command line. Exported for tests (Windows paths carry drive letters). */
export function parseArgs(argv) {
  const opts = { mode: "shell", config: {}, command: [] };
  const cfg = opts.config;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => {
      if (i + 1 >= argv.length) throw new Error(`${a} needs a value`);
      return argv[++i];
    };
    if (a === "--stdio") opts.mode = "stdio";
    else if (a === "shell" || a === "exec") opts.mode = a;
    else if (a === "--config") Object.assign(cfg, JSON.parse(readFileSync(next(), "utf8")));
    else if (a === "--mount") {
      const spec = next();
      // HOST may contain ':' (Windows drive letters), so take GUEST and :ro from the right.
      const ro = spec.endsWith(":ro");
      const body = ro ? spec.slice(0, -3) : spec;
      const cut = body.lastIndexOf(":");
      if (cut <= 0) throw new Error(`--mount expects HOST:GUEST[:ro], got ${spec}`);
      (cfg.mounts ??= []).push({ hostPath: body.slice(0, cut), guestPath: body.slice(cut + 1), readOnly: ro });
    } else if (a === "--allow") (((cfg.network ??= {}).allow ??= [])).push(next());
    else if (a === "--deny") (((cfg.network ??= {}).deny ??= [])).push(next());
    else if (a === "--no-network") cfg.network = false;
    else if (a === "--host-exec") cfg.hostExec = next();
    else if (a === "--no-python") cfg.python = false;
    else if (a === "--cpus") cfg.cpus = Number(next());
    else if (a === "--quiet") cfg.quiet = true;
    else if (a === "--") {
      opts.command = argv.slice(i + 1);
      break;
    } else if (a === "-h" || a === "--help") opts.mode = "help";
    else throw new Error(`unknown argument: ${a}`);
  }
  return opts;
}

async function shell(config) {
  const sb = await Sandbox.start({ ...config, consoleSize: { cols: process.stdout.columns || 100, rows: process.stdout.rows || 30 } });
  sb.on("console", (b) => process.stdout.write(b));
  sb.on("exit", (e) => {
    if (process.stdin.isTTY) process.stdin.setRawMode(false);
    process.stderr.write(`\r\n[collaboCore] machine ${e.reason}${e.message ? `: ${e.message}` : ""}\r\n`);
    process.exit(e.reason === "stopped" ? 0 : 1);
  });
  sb.on("network", (e) => {
    if (e.blocked || e.phase === "failed") process.stderr.write(`\r\n[network] ${e.via} ${e.url ?? e.host ?? e.ip}: ${e.reason ?? e.error}\r\n`);
  });
  sb.on("permission", (e) => {
    process.stderr.write(`\r\n[collaboCore] refused a host ${e.kind} request (nobody to ask in shell mode)\r\n`);
    sb.reply(e.requestId, { allow: false });
  });
  process.stdout.on("resize", () => sb.resizeConsole(process.stdout.columns, process.stdout.rows));
  if (process.stdin.isTTY) process.stdin.setRawMode(true);
  // Ctrl-] leaves, like telnet: Ctrl-C and Ctrl-D belong to the guest.
  process.stdin.on("data", (d) => (d.includes(0x1d) ? sb.stop() : void sb.writeConsole(d)));
}

async function execOnce(config, command) {
  if (command.length === 0) throw new Error("exec needs a command after --");
  const sb = await Sandbox.start({ quiet: true, ...config });
  sb.on("network", (e) => {
    if (e.blocked) process.stderr.write(`[network] blocked ${e.url ?? e.host ?? e.ip}: ${e.reason}\n`);
  });
  sb.on("permission", (e) => sb.reply(e.requestId, { allow: false }));
  sb.on("exit", (e) => {
    if (e.reason !== "stopped") {
      process.stderr.write(`[collaboCore] machine ${e.reason}: ${e.message ?? ""}\n`);
      process.exit(125);
    }
  });
  const r = await sb.exec({
    argv: command,
    cwd: config.mounts?.[0]?.guestPath,
    timeoutMs: 0,
    onStdout: (b) => process.stdout.write(b),
    onStderr: (b) => process.stderr.write(b),
  });
  sb.stop();
  process.exitCode = r.exitCode ?? 128 + (r.signal ?? 0);
  setTimeout(() => process.exit(), 50).unref();
}

// Only when started as a program: importing this module (tests do) must not boot anything.
const isEntryPoint =
  import.meta.main ?? (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href);

if (isEntryPoint) {
  try {
    const opts = parseArgs(process.argv.slice(2));
    if (opts.mode === "help") {
      process.stdout.write(readFileSync(new URL(import.meta.url), "utf8").split("\n").slice(1, 21).map((l) => l.replace(/^\/\/ ?/, "")).join("\n") + "\n");
    } else if (opts.mode === "stdio") {
      serveStdio(opts.config);
    } else if (opts.mode === "exec") {
      await execOnce(opts.config, opts.command);
    } else {
      await shell(opts.config);
    }
  } catch (error) {
    process.stderr.write(`collaboCore: ${error?.message ?? error}\n`);
    process.exit(2);
  }
}
