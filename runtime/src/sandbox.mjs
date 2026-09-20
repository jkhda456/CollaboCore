// A collaboCore sandbox: the WebAssembly Linux kernel booted in this process, with
//   - a console (the guest's login-free root shell),
//   - local disk folders mounted into the guest (virtiofs, NodeFS backend),
//   - a managed network (network.mjs) and host functions (host-functions.mjs),
//   - collabo-agentd, through which `exec` runs commands in the guest.
//
//   const sb = await Sandbox.start(config, { onEvent })
//   await sb.ready                        // guest agent is up
//   await sb.exec({ argv: ["python3", "-V"] })
//   sb.stop()
import { EventEmitter } from "node:events";
import { existsSync, readFileSync, statSync } from "node:fs";
import { availableParallelism } from "node:os";
import { isAbsolute, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { bootMachine, consoleDevice, entropyDevice, fileSystemDevice, MachinePanicError, vsockDevice } from "@lowland/kernel";
import { NodeFS } from "@lowland/guest/node";
import { execInGuest, waitForAgent } from "./agent.mjs";
import { createHostFunctions } from "./host-functions.mjs";
import { createManagedNetwork } from "./network.mjs";
import { normalizeNetworkPolicy } from "./policy.mjs";

const IMAGES = fileURLToPath(new URL("../images/", import.meta.url));
const HOST_EXEC_MODES = new Set(["deny", "ask", "allow"]);
const SAFE_GUEST_PATH = /^\/[A-Za-z0-9._\/-]*$/; // goes on the kernel command line: no spaces or ':'

/** Validates and fills in a sandbox configuration (see readme.detail.md, "설정"). */
export function normalizeConfig(input = {}) {
  const cpus = input.cpus ?? Math.min(4, Math.max(1, availableParallelism()));
  if (!Number.isInteger(cpus) || cpus < 1 || cpus > 64) throw new TypeError("cpus must be an integer from 1 to 64");

  const mounts = (input.mounts ?? []).map((m, i) => {
    if (typeof m?.hostPath !== "string" || typeof m?.guestPath !== "string") throw new TypeError(`mounts[${i}] needs hostPath and guestPath`);
    const hostPath = resolve(m.hostPath);
    if (!existsSync(hostPath) || !statSync(hostPath).isDirectory()) throw new TypeError(`mounts[${i}]: ${hostPath} is not a directory`);
    if (!isAbsolute(m.guestPath) || !m.guestPath.startsWith("/") || !SAFE_GUEST_PATH.test(m.guestPath) || m.guestPath === "/" ||
        /^\/(proc|sys|dev|bin|sbin|usr|etc|lib)(\/|$)/.test(m.guestPath)) {
      throw new TypeError(`mounts[${i}]: guestPath "${m.guestPath}" must be an absolute path of letters, digits, . _ - / outside system directories`);
    }
    return { hostPath, guestPath: m.guestPath.replace(/\/+$/, ""), readOnly: Boolean(m.readOnly), tag: `mount${i}` };
  });
  const guestPaths = new Set();
  for (const m of mounts) {
    if (guestPaths.has(m.guestPath)) throw new TypeError(`two mounts at ${m.guestPath}`);
    guestPaths.add(m.guestPath);
  }

  const hostExec = input.hostExec ?? "deny";
  if (!HOST_EXEC_MODES.has(hostExec)) throw new TypeError(`hostExec must be one of ${[...HOST_EXEC_MODES].join(", ")}`);
  const hostFunctions = input.hostFunctions ?? [];
  if (!Array.isArray(hostFunctions) || !hostFunctions.every((f) => typeof f === "string" && /^[A-Za-z0-9_.-]+$/.test(f))) {
    throw new TypeError("hostFunctions must be a list of names (letters, digits, _ . -)");
  }
  if (hostFunctions.some((f) => ["list", "info", "exec", "open"].includes(f))) throw new TypeError("hostFunctions cannot redefine list, info, exec or open");

  return {
    cpus,
    python: input.python ?? true,
    network: input.network === false ? false : normalizeNetworkPolicy(input.network ?? {}),
    mounts,
    hostExec,
    hostFunctions,
    permissionTimeoutMs: input.permissionTimeoutMs ?? 120_000,
    hostCallTimeoutMs: input.hostCallTimeoutMs ?? 120_000,
    kernelArgs: input.kernelArgs ?? [],
    quiet: input.quiet ?? false,
    consoleSize: input.consoleSize ?? { cols: 120, rows: 40 },
  };
}

export class Sandbox extends EventEmitter {
  #config;
  #machine;
  #vsock;
  #network;
  #hostFunctions;
  #consoleInput;
  #console;
  #stopped = false;

  /** Boots a sandbox. Events: "console" (Uint8Array), "network", "hostCall", "permission", "exit". */
  static async start(config) {
    const sb = new Sandbox(normalizeConfig(config));
    await sb.#boot();
    return sb;
  }

  constructor(config) {
    super();
    this.#config = config;
    /** Resolves when the guest agent accepts commands. */
    this.ready = undefined;
  }

  get config() {
    return this.#config;
  }

  async #boot() {
    const config = this.#config;
    const encoder = new TextEncoder();
    const toEvent = (name) => new WritableStream({ write: (chunk) => void this.emit(name, chunk) });

    this.#consoleInput = new TransformStream();
    this.#console = consoleDevice(this.#consoleInput.readable, toEvent("console"));
    this.#console.resize(config.consoleSize.cols, config.consoleSize.rows);
    this.#consoleWriter = this.#consoleInput.writable.getWriter();

    this.#vsock = vsockDevice();
    const plugins = [this.#console, entropyDevice(), this.#vsock];
    const args = ["collabo.agent=1", ...(config.quiet ? ["collabo.quiet=1"] : [])];

    const getPolicy = () => this.#policy();
    if (config.network) {
      this.#network = createManagedNetwork({ vsock: this.#vsock, getPolicy: () => this.#config.network, emit: (e) => this.emit("network", e) });
      plugins.push(...this.#network.devices);
      args.push(...this.#network.kernelArgs);
    }
    this.#hostFunctions = createHostFunctions({
      vsock: this.#vsock,
      getPolicy,
      emit: (e) => this.emit(e.type, e),
    });
    for (const m of config.mounts) {
      plugins.push(fileSystemDevice(new NodeFS(m.hostPath, { readOnly: m.readOnly }), { tag: m.tag }));
      args.push(`collabo.mount=${m.tag}:${m.guestPath}${m.readOnly ? ":ro" : ""}`);
    }
    args.push(...config.kernelArgs);

    const images = [readFileSync(IMAGES + "initramfs.cpio")];
    if (config.python && existsSync(IMAGES + "python.cpio")) images.push(readFileSync(IMAGES + "python.cpio"));
    const initcpio = new Uint8Array(images.reduce((n, b) => n + b.length, 0));
    images.reduce((at, b) => (initcpio.set(b, at), at + b.length), 0);

    this.#machine = await bootMachine({ cpus: config.cpus, args, plugins, initcpio });
    void this.#machine.bootConsole.pipeTo(toEvent("console")).catch(() => {});
    this.#machine.closed.then(
      () => this.#exited({ reason: "stopped" }),
      (error) => this.#exited({ reason: error instanceof MachinePanicError ? "panic" : "error", message: String(error?.message ?? error) }),
    );
    this.ready = waitForAgent(this.#vsock);
    this.ready.catch(() => {});
    void encoder;
  }

  #consoleWriter;

  #policy() {
    return {
      hostExec: this.#config.hostExec,
      hostFunctions: this.#config.hostFunctions,
      permissionTimeoutMs: this.#config.permissionTimeoutMs,
      hostCallTimeoutMs: this.#config.hostCallTimeoutMs,
    };
  }

  #exited(detail) {
    if (this.#stopped) return;
    this.#stopped = true;
    this.#hostFunctions?.close();
    this.#network?.close();
    this.emit("exit", detail);
  }

  /** Runs a command in the guest and collects its output. See agent.mjs for the options. */
  async exec(request) {
    if (this.#stopped) throw new Error("the sandbox has stopped");
    await this.ready;
    return execInGuest(this.#vsock, request);
  }

  /** Types into the guest console. */
  writeConsole(data) {
    const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
    return this.#consoleWriter.write(bytes);
  }

  resizeConsole(cols, rows) {
    this.#console.resize(cols, rows);
  }

  /** Changes the network policy and host access rules while running. */
  updatePolicy({ network, hostExec, hostFunctions } = {}) {
    if (network !== undefined) {
      if (!this.#config.network) throw new Error("this sandbox was started without a network");
      this.#config.network = normalizeNetworkPolicy({ ...this.#config.network, ...network });
    }
    if (hostExec !== undefined) {
      if (!HOST_EXEC_MODES.has(hostExec)) throw new TypeError(`hostExec must be one of ${[...HOST_EXEC_MODES].join(", ")}`);
      this.#config.hostExec = hostExec;
    }
    if (hostFunctions !== undefined) this.#config.hostFunctions = normalizeConfig({ hostFunctions }).hostFunctions;
  }

  /** Answers a "hostCall" ({callId, result|error}) or "permission" ({requestId, allow}) event. */
  reply(id, answer) {
    return this.#hostFunctions.reply(id, answer);
  }

  stop() {
    this.#machine?.close();
    this.#exited({ reason: "stopped" });
  }
}
