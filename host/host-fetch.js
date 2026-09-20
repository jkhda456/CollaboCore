// The host side of guest HTTP: turns the Request the network adapter builds from the
// guest's plain-HTTP exchange into a real fetch(), and reports it to a log.
//
// Every outgoing guest request passes through the function returned here, so it is the
// place for policy (allow-list, API-key injection) as well as for showing the URL.

const METHODS_WITHOUT_BODY = new Set(["GET", "HEAD"]);

/**
 * @param {{ log: { start(method: string, url: string, info?: { bodyBytes: number, source?: string }): unknown,
 *                  finish(entry: unknown, status: number, label?: string): void,
 *                  fail(entry: unknown, error: unknown): void },
 *           fetch?: typeof fetch }} options
 * @returns {(request: Request) => Promise<Response>}
 */
export function createHostFetch({ log, fetch: doFetch = globalThis.fetch.bind(globalThis) }) {
  return async function hostFetch(request) {
    // Buffer the body: browsers only stream request bodies over HTTP/2 with
    // duplex:"half", so a plain buffered body is the portable choice.
    const body = METHODS_WITHOUT_BODY.has(request.method) ? undefined : await request.arrayBuffer();
    const entry = log.start(request.method, request.url, { bodyBytes: body?.byteLength ?? 0, source: "net" });
    try {
      const response = await doFetch(request.url, {
        method: request.method,
        headers: request.headers,
        body,
        credentials: "omit",
        // Redirects stay manual, as the adapter requests: following one would send the
        // guest's request to a host it never named. The guest gets a 502 instead.
        redirect: "manual",
        referrerPolicy: "no-referrer",
        signal: request.signal,
      });
      if (response.type === "opaqueredirect") {
        log.finish(entry, response.status, "redirect (not followed)");
      } else {
        log.finish(entry, response.status);
      }
      return response;
    } catch (error) {
      log.fail(
        entry,
        error instanceof TypeError ? `${error.message} (network error or blocked by CORS)` : error,
      );
      throw error;
    }
  };
}
