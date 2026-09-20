// Wires the guest's HTTP request API (http-protocol.js) to a virtio-vsock device.
//
//   const bridge = createHttpBridge({ log });        // log: the request panel (optional)
//   await bootMachine({ plugins: [..., bridge.device] });
//
// The guest connects to vsock CID 2 (the host), port `bridge.port`, sends one request and
// reads one response; see http-protocol.js for the wire format. In the guest, `hfetch` does it.
import { vsockDevice } from "@lowland/kernel";
import { DEFAULT_ALLOWED_REQUEST_HEADERS, DEFAULT_PORT, handleConnection } from "./http-protocol.js";

/**
 * @param {{
 *   device?: object,                      // an existing vsockDevice to listen on (the guest has one
 *                                         // vsock transport, so all host services share a device)
 *   fetch?: typeof fetch,                 // how requests are really made (default: the page's fetch)
 *   log?: object,                         // request panel: start/finish/fail
 *   port?: number,                        // vsock port the guest connects to
 *   allowHeaders?: string[] | (() => string[]), // request headers the guest may set
 *   maxRequestBody?: number,              // bytes
 *   maxConcurrent?: number,               // simultaneous requests; more get `ERROR busy`
 *   timeoutMs?: number,                   // limit for the response head to arrive
 *   authorize?: Function,                 // policy / secret injection, see handleConnection
 * }} options
 */
export function createHttpBridge({
  device = vsockDevice(),
  fetch: doFetch = globalThis.fetch.bind(globalThis),
  log,
  port = DEFAULT_PORT,
  allowHeaders = DEFAULT_ALLOWED_REQUEST_HEADERS,
  maxRequestBody = 32 * 1024 * 1024,
  maxConcurrent = 8,
  timeoutMs = 60_000,
  authorize,
} = {}) {
  const ownsDevice = arguments[0]?.device === undefined;
  let active = 0;

  const stop = device.listen(port, (conn) => {
    if (active >= maxConcurrent) {
      const line = new TextEncoder().encode(`ERROR busy: more than ${maxConcurrent} requests in flight\n`);
      void conn.write(line).catch(() => {}).finally(() => conn.close());
      return;
    }
    active++;
    void handleConnection(conn, {
      fetch: doFetch,
      log,
      // A function is read per request, so a policy change applies to the next one.
      allowHeaders: typeof allowHeaders === "function" ? allowHeaders() : allowHeaders,
      maxRequestBody,
      timeoutMs,
      authorize,
    }).finally(() => {
      active--;
    });
  });

  return {
    /** Add this to the machine's plugins. */
    device,
    port,
    close() {
      stop();
      if (ownsDevice) device.close();
    },
  };
}
