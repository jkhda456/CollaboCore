// Host functions for the guest (vsock 1081; guest tools `hostcall`, python `collabo_core.host`).
// This is the sandbox's only way to touch the host computer, and every call is checked here:
//
//   list                       the functions below plus those the app registered
//   info                       { platform, arch } of the host (nothing identifying)
//   exec {argv, cwd?, stdin?, timeoutMs?, gui?}
//                              run a host program (shell tools or GUI apps). Governed by
//                              policy.hostExec: "deny" (default) | "ask" | "allow"
//   open {target}              open a file or URL with the host's default app (same policy)
//   <name>                     any function the app registered (config.hostFunctions): forwarded
//                              to the app as a "hostCall" event, answered with hostCall.reply
//
// "ask" sends a "permission" event to the app and waits for permission.reply (or refuses after
// permissionTimeoutMs). One JSON line each way; see userspace/rootfs/hostcall.c.
import { spawn } from "node:child_process";
import { homedir, platform as hostPlatform, arch as hostArch } from "node:os";

export const HOST_FUNCTIONS_PORT = 1081;
const MAX_REQUEST = 1024 * 1024;
const MAX_OUTPUT = 8 * 1024 * 1024;

class CallError extends Error {
  constructor(kind, message) {
    super(message);
    this.kind = kind;
  }
}

function readLine(conn) {
  return (async () => {
    let text = "";
    const decoder = new TextDecoder();
    for (;;) {
      const chunk = await conn.read();
      if (chunk.length === 0) throw new CallError("bad-request", "connection closed before the request line ended");
      text += decoder.decode(chunk, { stream: true });
      const nl = text.indexOf("\n");
      if (nl >= 0) return text.slice(0, nl);
      if (text.length > MAX_REQUEST) throw new CallError("bad-request", "request too large");
    }
  })();
}

function runHostProgram({ argv, cwd, stdin, timeoutMs = 60_000, gui = false }) {
  if (!Array.isArray(argv) || argv.length === 0 || !argv.every((a) => typeof a === "string")) {
    throw new CallError("bad-request", "argv must be a non-empty list of strings");
  }
  const options = { cwd: cwd || homedir(), windowsHide: !gui, shell: false };
  if (gui) {
    // Detached: the app outlives the call; nothing is captured.
    return new Promise((resolve, reject) => {
      const child = spawn(argv[0], argv.slice(1), { ...options, detached: true, stdio: "ignore" });
      child.once("error", (e) => reject(new CallError("failed", `${argv[0]}: ${e.message}`)));
      child.once("spawn", () => {
        child.unref();
        resolve({ pid: child.pid });
      });
    });
  }
  return new Promise((resolve, reject) => {
    const child = spawn(argv[0], argv.slice(1), { ...options, stdio: ["pipe", "pipe", "pipe"] });
    const out = [], err = [];
    let outSize = 0, errSize = 0, timedOut = false;
    const collect = (list, add) => (d) => {
      if (add(d.length) <= MAX_OUTPUT) list.push(d);
    };
    child.stdout.on("data", collect(out, (n) => (outSize += n)));
    child.stderr.on("data", collect(err, (n) => (errSize += n)));
    const timer = setTimeout(() => ((timedOut = true), child.kill("SIGKILL")), Math.min(timeoutMs, 10 * 60_000));
    child.once("error", (e) => {
      clearTimeout(timer);
      reject(new CallError("failed", `${argv[0]}: ${e.message}`));
    });
    child.once("close", (code, sig) => {
      clearTimeout(timer);
      // Key order matters for the C client: exitCode first, stdout last.
      resolve({
        exitCode: code ?? (sig ? 128 + 9 : 1),
        timedOut,
        stderr: Buffer.concat(err).toString("utf8"),
        stdout: Buffer.concat(out).toString("utf8"),
      });
    });
    child.stdin.on("error", () => {});
    child.stdin.end(typeof stdin === "string" ? stdin : undefined);
  });
}

function openCommand(target) {
  if (hostPlatform() === "darwin") return ["open", target];
  if (hostPlatform() === "win32") return ["explorer.exe", target];
  return ["xdg-open", target];
}

/**
 * @param {{ vsock: object, getPolicy: () => { hostExec: string, hostFunctions: string[], permissionTimeoutMs?: number },
 *           emit: (event: object) => void }} options
 */
export function createHostFunctions({ vsock, getPolicy, emit }) {
  let nextId = 1;
  const waiting = new Map(); // id -> { resolve, reject, timer }

  function awaitReply(kind, event, timeoutMs) {
    const id = nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        waiting.delete(id);
        reject(new CallError(kind === "permission" ? "denied" : "timeout", kind === "permission" ? "nobody answered the permission request" : "the app did not answer"));
      }, timeoutMs);
      waiting.set(id, { resolve, reject, timer, kind });
      emit({ ...event, [kind === "permission" ? "requestId" : "callId"]: id });
    });
  }

  async function checkHostAccess(what, detail) {
    const mode = getPolicy().hostExec;
    if (mode === "allow") return;
    if (mode === "ask") {
      const allowed = await awaitReply("permission", { type: "permission", kind: what, ...detail }, getPolicy().permissionTimeoutMs ?? 120_000);
      if (allowed) return;
      throw new CallError("denied", "the user did not allow it");
    }
    throw new CallError("denied", "running host programs is not allowed for this sandbox (hostExec policy)");
  }

  async function dispatch(fn, args) {
    const policy = getPolicy();
    switch (fn) {
      case "list":
        return ["list", "info", "exec", "open", ...policy.hostFunctions];
      case "info":
        return { platform: hostPlatform(), arch: hostArch(), hostExec: policy.hostExec };
      case "exec":
        await checkHostAccess("exec", { argv: args?.argv, cwd: args?.cwd ?? null, gui: Boolean(args?.gui) });
        return runHostProgram(args ?? {});
      case "open": {
        if (typeof args?.target !== "string" || !args.target) throw new CallError("bad-request", "open needs a target");
        await checkHostAccess("open", { target: args.target });
        return runHostProgram({ argv: openCommand(args.target), gui: true });
      }
      default:
        if (!policy.hostFunctions.includes(fn)) throw new CallError("unknown-function", `the host offers no function "${fn}" (hostcall --list)`);
        return awaitReply("call", { type: "hostCall", fn, args }, policy.hostCallTimeoutMs ?? 120_000);
    }
  }

  const stop = vsock.listen(HOST_FUNCTIONS_PORT, (conn) => {
    void (async () => {
      let response;
      try {
        let request;
        try {
          request = JSON.parse(await readLine(conn));
        } catch (e) {
          throw e instanceof CallError ? e : new CallError("bad-request", `the request is not valid JSON: ${e.message}`);
        }
        if (typeof request?.fn !== "string") throw new CallError("bad-request", 'expected {"fn": "...", "args": {...}}');
        response = { ok: true, result: (await dispatch(request.fn, request.args ?? {})) ?? null };
      } catch (error) {
        response = { ok: false, error: { kind: error.kind ?? "failed", message: String(error.message ?? error) } };
      }
      await conn.write(new TextEncoder().encode(JSON.stringify(response) + "\n")).catch(() => {});
      conn.close();
    })();
  });

  return {
    /** The app's answer to a hostCall or permission event. */
    reply(id, { result, error, allow }) {
      const w = waiting.get(id);
      if (!w) return false;
      waiting.delete(id);
      clearTimeout(w.timer);
      if (w.kind === "permission") w.resolve(Boolean(allow));
      else if (error) w.reject(new CallError(error.kind ?? "failed", error.message ?? String(error)));
      else w.resolve(result);
      return true;
    },
    close() {
      stop();
      for (const w of waiting.values()) clearTimeout(w.timer), w.reject(new CallError("failed", "the sandbox is shutting down"));
      waiting.clear();
    },
  };
}
