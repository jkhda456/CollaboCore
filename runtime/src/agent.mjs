// Host side of collabo-agentd (userspace/rootfs/collabo-agentd.c): run a command in the guest.
// One vsock connection per command, frames of type(1) + length(u32 LE) + payload.

export const AGENT_PORT = 1024;
const MAX_CAPTURE = 16 * 1024 * 1024; // per stream; more is dropped and reported as truncated
const STDIN_CHUNK = 32 * 1024; // below the agent's buffer

const encoder = new TextEncoder();

function frame(type, payload = new Uint8Array(0)) {
  const bytes = typeof payload === "string" ? encoder.encode(payload) : payload;
  const out = new Uint8Array(5 + bytes.length);
  out[0] = type.charCodeAt(0);
  new DataView(out.buffer).setUint32(1, bytes.length, true);
  out.set(bytes, 5);
  return out;
}

class Capture {
  constructor(onData) {
    this.chunks = [];
    this.size = 0;
    this.truncated = false;
    this.onData = onData;
  }
  push(bytes) {
    this.onData?.(bytes);
    if (this.size >= MAX_CAPTURE) return void (this.truncated = true);
    const take = bytes.subarray(0, MAX_CAPTURE - this.size);
    if (take.length < bytes.length) this.truncated = true;
    this.chunks.push(take.slice());
    this.size += take.length;
  }
  bytes() {
    const out = new Uint8Array(this.size);
    let at = 0;
    for (const c of this.chunks) out.set(c, at), (at += c.length);
    return out;
  }
}

/** Waits until the agent in the guest accepts connections. */
export async function waitForAgent(vsock, { timeoutMs = 120_000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    try {
      const conn = await vsock.connect(AGENT_PORT, { timeoutMs: 2000 });
      conn.close();
      return;
    } catch (error) {
      last = error;
      await new Promise((r) => setTimeout(r, 200));
    }
  }
  throw new Error(`the guest agent did not start within ${timeoutMs} ms (${last?.message ?? last})`);
}

/**
 * Runs a command in the guest.
 * @param {object} vsock  the machine's vsockDevice
 * @param {{ argv: string[], cwd?: string, env?: Record<string,string>, stdin?: Uint8Array,
 *           timeoutMs?: number, onStdout?: (b: Uint8Array) => void, onStderr?: (b: Uint8Array) => void,
 *           signal?: AbortSignal }} request
 * @returns {Promise<{ exitCode: number|null, signal: number|null, stdout: Uint8Array, stderr: Uint8Array,
 *                     durationMs: number, timedOut: boolean, truncated: boolean }>}
 */
export async function execInGuest(vsock, { argv, cwd, env, stdin, timeoutMs = 120_000, onStdout, onStderr, signal }) {
  if (!Array.isArray(argv) || argv.length === 0 || !argv.every((a) => typeof a === "string")) {
    throw new TypeError("argv must be a non-empty list of strings");
  }
  const started = performance.now();
  const conn = await vsock.connect(AGENT_PORT, { timeoutMs: 5000 });
  const out = new Capture(onStdout);
  const err = new Capture(onStderr);
  let timedOut = false;
  let aborted = false;

  const kill = (sig) => {
    const payload = new Uint8Array(4);
    new DataView(payload.buffer).setUint32(0, sig, true);
    conn.write(frame("K", payload)).catch(() => {});
  };
  const timer = timeoutMs > 0 ? setTimeout(() => ((timedOut = true), kill(9)), timeoutMs) : undefined;
  const onAbort = () => ((aborted = true), kill(9));
  signal?.addEventListener("abort", onAbort, { once: true });

  try {
    for (const a of argv) await conn.write(frame("A", a));
    for (const [k, v] of Object.entries(env ?? {})) {
      if (k.includes("=") || k.includes("\0")) throw new TypeError(`invalid environment name: ${k}`);
      await conn.write(frame("E", `${k}=${v}`));
    }
    if (cwd) await conn.write(frame("C", cwd));
    await conn.write(frame("S"));
    const input = stdin ?? new Uint8Array(0);
    for (let at = 0; at < input.length; at += STDIN_CHUNK) {
      await conn.write(frame("I", input.subarray(at, at + STDIN_CHUNK)));
    }
    await conn.write(frame("I")); // end of stdin

    let buffer = new Uint8Array(0);
    for (;;) {
      const chunk = await conn.read();
      if (chunk.length === 0) throw new Error("the guest agent closed the connection unexpectedly");
      const joined = new Uint8Array(buffer.length + chunk.length);
      joined.set(buffer);
      joined.set(chunk, buffer.length);
      buffer = joined;
      while (buffer.length >= 5) {
        const len = new DataView(buffer.buffer, buffer.byteOffset).getUint32(1, true);
        if (buffer.length < 5 + len) break;
        const type = String.fromCharCode(buffer[0]);
        const payload = buffer.subarray(5, 5 + len);
        buffer = buffer.subarray(5 + len);
        if (type === "1") out.push(payload);
        else if (type === "2") err.push(payload);
        else if (type === "F") throw Object.assign(new Error(new TextDecoder().decode(payload)), { kind: "spawn" });
        else if (type === "X") {
          const code = new DataView(payload.buffer, payload.byteOffset).getInt32(0, true);
          return {
            exitCode: code >= 0 ? code : null,
            signal: code < 0 ? -code : null,
            stdout: out.bytes(),
            stderr: err.bytes(),
            durationMs: Math.round(performance.now() - started),
            timedOut,
            aborted,
            truncated: out.truncated || err.truncated,
          };
        }
      }
    }
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener("abort", onAbort);
    conn.close();
  }
}
