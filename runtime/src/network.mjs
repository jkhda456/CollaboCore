// Managed network for the sandbox. Two ways out, one policy (policy.mjs), every attempt reported:
//
//  1. Request level (vsock 1080; `hfetch`, python `collabo_core`): one fetch() per request.
//     The host adds secret headers here, so API keys never enter the guest.
//  2. Packet level (virtio-net + a TCP/IP stack in JS): ordinary sockets, e.g. python `socket`,
//     busybox `wget http://…`. DNS is answered by the host; a TCP connection is let through only
//     to an address that DNS resolved for an allowed name (or an allowed literal IP).
//
// Events (via `emit`): { via: "api"|"net", kind: "request"|"dns"|"connect", ... } see below.
import { lookup } from "node:dns/promises";
import { connect as tcpConnect } from "node:net";
import { Duplex } from "node:stream";
import { attachNetworkDevice, createNetwork } from "@lowland/guest";
import { createHttpBridge } from "../host/http-bridge.js";
import { BridgeError, DEFAULT_ALLOWED_REQUEST_HEADERS } from "../host/http-protocol.js";
import { hostDenied, isLoopbackHost, redact, secretHeadersFor } from "./policy.mjs";

let nextId = 1;

/**
 * @param {{ vsock: object, getPolicy: () => object, emit: (event: object) => void, fetch?: typeof fetch }} options
 * @returns {{ devices: object[], kernelArgs: string[], close(): void }}
 */
export function createManagedNetwork({ vsock, getPolicy, emit, fetch: doFetch = globalThis.fetch }) {
  // ---- 1. request level ------------------------------------------------------------------
  const log = {
    start(method, url, info) {
      const id = nextId++;
      emit({ via: "api", kind: "request", id, phase: "start", method, url, bodyBytes: info?.bodyBytes ?? 0, at: Date.now() });
      return { id, method, url, started: performance.now() };
    },
    finish(entry, status) {
      emit({ via: "api", kind: "request", id: entry.id, phase: "response", method: entry.method, url: entry.url, status, durationMs: Math.round(performance.now() - entry.started) });
    },
    fail(entry, error) {
      emit({
        via: "api", kind: "request", id: entry.id, phase: "failed", method: entry.method, url: entry.url,
        error: redact(getPolicy(), error?.message ?? error), errorKind: error?.kind ?? "network",
        durationMs: Math.round(performance.now() - entry.started),
      });
    },
  };

  const authorize = (request) => {
    const policy = getPolicy();
    const host = new URL(request.url).hostname;
    if (isLoopbackHost(host) && !policy.allowHostLoopback) {
      throw new BridgeError("denied", `blocked by the network policy: "${host}" is this computer (allowHostLoopback is off)`);
    }
    const why = hostDenied(policy, host);
    if (why) throw new BridgeError("denied", `blocked by the network policy: ${why}`);
    for (const s of secretHeadersFor(policy, request.url)) request.headers.set(s.header, s.value);
  };

  const bridge = createHttpBridge({
    device: vsock,
    fetch: doFetch,
    log,
    authorize,
    allowHeaders: () => [...DEFAULT_ALLOWED_REQUEST_HEADERS, ...getPolicy().extraAllowedHeaders],
  });

  // ---- 2. packet level -------------------------------------------------------------------
  const resolvedNames = new Map(); // ip -> Set(hostname) that DNS answered with it

  async function resolveDns(hostname) {
    const name = hostname.toLowerCase().replace(/\.$/, "");
    const why = hostDenied(getPolicy(), name);
    if (why) {
      emit({ via: "net", kind: "dns", host: name, blocked: true, reason: why });
      return [];
    }
    try {
      const addresses = (await lookup(name, { all: true, family: 4 })).map((a) => a.address);
      for (const ip of addresses) {
        if (!resolvedNames.has(ip)) resolvedNames.set(ip, new Set());
        resolvedNames.get(ip).add(name);
      }
      emit({ via: "net", kind: "dns", host: name, addresses });
      return addresses;
    } catch (error) {
      emit({ via: "net", kind: "dns", host: name, error: error.code ?? String(error) });
      return [];
    }
  }

  async function connectTcp(session) {
    const policy = getPolicy();
    const { hostname: ip, port } = session.target;
    const id = nextId++;
    // The stack hands us the gateway (192.0.2.1) already mapped to 127.0.0.1: connections to this
    // computer are governed by allowHostLoopback alone, everything else by the names DNS gave out.
    const target = ip === network.gateway ? "127.0.0.1" : ip;
    let reason;
    let names = [];
    if (isLoopbackHost(target)) {
      if (!policy.allowHostLoopback) reason = "the host's own services (192.0.2.1 = its localhost) are not allowed";
    } else {
      names = [...(resolvedNames.get(ip) ?? [])];
      const allowedName = names.find((n) => !hostDenied(policy, n));
      if (!allowedName && hostDenied(policy, ip)) {
        reason = names.length ? hostDenied(policy, names[0]) : `${ip} was not resolved from an allowed name`;
      }
    }
    if (reason) {
      emit({ via: "net", kind: "connect", id, ip, port, names, blocked: true, reason });
      throw new Error(`blocked: ${reason}`); // before any I/O: the guest sees the connection refused
    }

    const started = performance.now();
    emit({ via: "net", kind: "connect", id, ip, port, names, phase: "open" });
    const socket = tcpConnect({ host: target, port });
    const stop = () => socket.destroy();
    session.signal.addEventListener("abort", stop, { once: true });
    let bytesOut = 0, bytesIn = 0;
    socket.on("data", (d) => (bytesIn += d.length));
    try {
      await new Promise((resolve, reject) => {
        socket.once("connect", resolve);
        socket.once("error", reject);
      });
      const web = Duplex.toWeb(socket);
      const counted = new TransformStream({ transform(chunk, c) { bytesOut += chunk.length; c.enqueue(chunk); } });
      await Promise.allSettled([
        session.readable.pipeThrough(counted).pipeTo(web.writable),
        web.readable.pipeTo(session.writable),
      ]);
    } catch (error) {
      emit({ via: "net", kind: "connect", id, ip, port, phase: "failed", error: error.code ?? String(error?.message ?? error) });
      throw error;
    } finally {
      session.signal.removeEventListener("abort", stop);
      socket.destroy();
    }
    emit({ via: "net", kind: "connect", id, ip, port, phase: "closed", bytesIn, bytesOut, durationMs: Math.round(performance.now() - started) });
  }

  const network = createNetwork({ resolveDns, connectTcp });
  const nic = attachNetworkDevice(network);

  return {
    devices: [nic],
    kernelArgs: [`collabo.ip=${nic.address}`, `collabo.gw=${network.gateway}`],
    close() {
      bridge.close();
      network.close();
    },
  };
}
