// The guest's HTTP request API, host side: wire protocol + request execution.
//
// One request per connection, at the level of a browser fetch() call, not TCP: the guest
// names a method, a URL (http: or https:, taken literally) and a few headers, and gets back
// a status, headers and a streamed body. Everything is text plus length-delimited bytes so a
// guest client is a few dozen lines of C, shell or anything else that can open a vsock.
//
//   request   METHOD URL\n
//             name: value\n            (only allow-listed names; see DEFAULT_ALLOWED_REQUEST_HEADERS)
//             content-length: N\n      (framing only, not forwarded; required iff there is a body)
//             \n
//             <N bytes>
//
//   response  STATUS STATUS-TEXT\n
//             name: value\n            (all response headers fetch() exposes, minus framing ones)
//             \n
//             <hex-length>\n<bytes>    repeated; hex-length >= 1
//             0\n
//             OK\n   |   ERROR kind: message\n      <- tells a complete body from a cut-off one
//
//   failure   ERROR kind: message\n    instead of a status line, when nothing was fetched
//
// Kinds: bad-request, header-not-allowed, scheme-not-allowed, request-too-large, busy,
// denied (the host's policy refused it), network, timeout, internal.
//
// Fixed to browser defaults, deliberately not adjustable by the guest: credentials "omit"
// (no cookies), redirect "follow", referrer none, mode "cors". Response content-encoding /
// content-length / transfer-encoding are dropped: fetch() has already decoded the body.

export const DEFAULT_PORT = 1080;

/** The only request headers the guest may set. Add more via the `allowHeaders` option. */
export const DEFAULT_ALLOWED_REQUEST_HEADERS = ["accept", "content-type", "authorization", "x-api-key"];

export const METHODS = new Set(["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]);
const METHODS_WITHOUT_BODY = new Set(["GET", "HEAD"]);
const DROPPED_RESPONSE_HEADERS = new Set(["content-encoding", "content-length", "transfer-encoding", "connection"]);
const MAX_HEAD_BYTES = 16 * 1024;
const TOKEN = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/;

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

export class BridgeError extends Error {
  constructor(kind, message) {
    super(message);
    this.name = "BridgeError";
    this.kind = kind;
  }
}

const oneLine = (text) => String(text).replace(/[\r\n\0]+/g, " ").slice(0, 500);

function concat(a, b) {
  const out = new Uint8Array(a.length + b.length);
  out.set(a);
  out.set(b, a.length);
  return out;
}

// ---- request parsing --------------------------------------------------------------------

async function readHead(conn) {
  let buffer = new Uint8Array(0);
  for (;;) {
    for (let i = 0; i + 1 < buffer.length; i++) {
      if (buffer[i] === 10 && buffer[i + 1] === 10) {
        let text;
        try {
          text = decoder.decode(buffer.subarray(0, i));
        } catch {
          throw new BridgeError("bad-request", "request head is not valid UTF-8");
        }
        return { text, rest: buffer.subarray(i + 2) };
      }
    }
    if (buffer.length > MAX_HEAD_BYTES) throw new BridgeError("bad-request", "request head too large");
    const chunk = await conn.read();
    if (chunk.length === 0) throw new BridgeError("bad-request", "connection closed before the request head ended");
    buffer = concat(buffer, chunk);
  }
}

/**
 * Parses and validates a request head. Pure: no I/O, so it is easy to test.
 * @returns {{ method: string, url: string, headers: Headers, contentLength: number }}
 */
export function parseRequestHead(text, { allowHeaders = DEFAULT_ALLOWED_REQUEST_HEADERS, maxRequestBody = 32 * 1024 * 1024 } = {}) {
  if (text.includes("\r")) throw new BridgeError("bad-request", "lines must end with LF, not CRLF");
  const allowed = new Set(allowHeaders.map((name) => name.toLowerCase()));
  const [first, ...lines] = text.split("\n");

  const space = first.indexOf(" ");
  if (space < 0) throw new BridgeError("bad-request", "first line must be: METHOD URL");
  const method = first.slice(0, space);
  const target = first.slice(space + 1);
  if (!METHODS.has(method)) {
    throw new BridgeError("bad-request", `method ${oneLine(method)} is not supported (${[...METHODS].join(", ")})`);
  }
  let url;
  try {
    url = new URL(target);
  } catch {
    throw new BridgeError("bad-request", `invalid URL: ${oneLine(target)}`);
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new BridgeError("scheme-not-allowed", `only http: and https: URLs are allowed, not ${url.protocol}`);
  }
  if (url.username || url.password) {
    // fetch() turns these into an Authorization header, which would bypass the allow-list.
    throw new BridgeError("bad-request", "credentials in the URL are not allowed; use the authorization header");
  }

  try {
    return parseHeaders(method, url, lines, allowed, maxRequestBody);
  } catch (error) {
    // Lets the caller show a rejected request (e.g. a header that is not allowed) by URL.
    if (error instanceof BridgeError) error.request = { method, url: url.href };
    throw error;
  }
}

function parseHeaders(method, url, lines, allowed, maxRequestBody) {
  const headers = new Headers();
  let contentLength = 0;
  let sawLength = false;
  for (const line of lines) {
    const colon = line.indexOf(":");
    if (colon <= 0) throw new BridgeError("bad-request", `malformed header line: ${oneLine(line)}`);
    const name = line.slice(0, colon).toLowerCase();
    const value = line.slice(colon + 1).replace(/^[ \t]+|[ \t]+$/g, "");
    if (!TOKEN.test(name)) throw new BridgeError("bad-request", `invalid header name: ${oneLine(name)}`);
    if (value.includes("\0")) throw new BridgeError("bad-request", `invalid header value for ${name}`);

    if (name === "content-length") {
      if (sawLength) throw new BridgeError("bad-request", "duplicate content-length");
      if (!/^\d{1,15}$/.test(value)) throw new BridgeError("bad-request", "invalid content-length");
      sawLength = true;
      contentLength = Number(value);
      continue;
    }
    if (!allowed.has(name)) {
      throw new BridgeError(
        "header-not-allowed",
        `request header "${name}" is not allowed (allowed: ${[...allowed].join(", ")})`,
      );
    }
    headers.append(name, value);
  }

  if (METHODS_WITHOUT_BODY.has(method) && contentLength > 0) {
    throw new BridgeError("bad-request", `${method} requests cannot have a body`);
  }
  if (contentLength > maxRequestBody) {
    throw new BridgeError("request-too-large", `request body of ${contentLength} bytes exceeds the ${maxRequestBody} byte limit`);
  }
  return { method, url: url.href, headers, contentLength };
}

async function readBody(conn, rest, length) {
  if (rest.length > length) throw new BridgeError("bad-request", "data after the request body");
  const body = new Uint8Array(length);
  body.set(rest);
  let filled = rest.length;
  while (filled < length) {
    const chunk = await conn.read();
    if (chunk.length === 0) throw new BridgeError("bad-request", "connection closed before the request body ended");
    if (filled + chunk.length > length) throw new BridgeError("bad-request", "data after the request body");
    body.set(chunk, filled);
    filled += chunk.length;
  }
  return body;
}

// ---- request execution ------------------------------------------------------------------

/**
 * Serves one connection: read a request, perform it with `fetch`, stream the response.
 * Never throws; every failure is reported to the guest, or, if the guest is gone, dropped.
 *
 * @param {{ read(): Promise<Uint8Array>, write(data: Uint8Array): Promise<void>, close(): void }} conn
 * `authorize(request)` (optional) runs after parsing and before the fetch, with
 * `{ method, url, headers }`: it may throw a BridgeError (e.g. kind "denied") to refuse, and may
 * add headers the guest must not see (API keys). Its additions are sent but never logged.
 *
 * @param {{ fetch: typeof fetch, log?: object, allowHeaders?: string[], maxRequestBody?: number, timeoutMs?: number,
 *           authorize?: (request: { method: string, url: string, headers: Headers }) => void | Promise<void> }} options
 */
export async function handleConnection(conn, options) {
  const { fetch: doFetch, log, timeoutMs = 60_000 } = options;
  const abort = new AbortController();
  let entry;
  let headSent = false;
  let reader;
  try {
    const { text, rest } = await readHead(conn);
    const request = parseRequestHead(text, options);
    const body = await readBody(conn, rest, request.contentLength);
    entry = log?.start(request.method, request.url, { bodyBytes: body.length, source: "api" });
    await options.authorize?.(request);

    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      abort.abort();
    }, timeoutMs);
    let response;
    try {
      response = await doFetch(request.url, {
        method: request.method,
        headers: request.headers,
        body: body.length > 0 ? body : undefined,
        credentials: "omit",
        redirect: "follow",
        referrerPolicy: "no-referrer",
        signal: abort.signal,
      });
    } catch (error) {
      throw timedOut
        ? new BridgeError("timeout", `no response within ${timeoutMs} ms`)
        : new BridgeError("network", `${error?.message ?? error} (network error or blocked by CORS)`);
    } finally {
      clearTimeout(timer);
    }
    log?.finish(entry, response.status);

    let head = `${response.status} ${oneLine(response.statusText)}\n`;
    for (const [name, value] of response.headers) {
      if (!DROPPED_RESPONSE_HEADERS.has(name)) head += `${name}: ${oneLine(value)}\n`;
    }
    await conn.write(encoder.encode(head + "\n"));
    headSent = true;

    if (response.body && request.method !== "HEAD") {
      reader = response.body.getReader();
      for (;;) {
        let chunk;
        try {
          chunk = await reader.read();
        } catch (error) {
          throw new BridgeError("network", `response body interrupted: ${error?.message ?? error}`);
        }
        const { value, done } = chunk;
        if (done) break;
        if (value.length === 0) continue;
        await conn.write(encoder.encode(value.length.toString(16) + "\n"));
        await conn.write(value);
      }
    }
    await conn.write(encoder.encode("0\nOK\n"));
  } catch (error) {
    const known = error instanceof BridgeError;
    if (entry !== undefined) {
      log?.fail(entry, error); // fetch failed, or the stream broke after the head
    } else if (error?.request) {
      // Rejected before it was sent: still show what the guest tried to do.
      log?.fail(log.start(error.request.method, error.request.url, { source: "api" }), error);
    }
    const kind = known ? error.kind : "internal";
    const line = `ERROR ${kind}: ${oneLine(error?.message ?? error)}\n`;
    // Before the head: the error replaces the status line. After it: it is the trailer.
    await conn.write(encoder.encode(headSent ? `0\n${line}` : line)).catch(() => {});
  } finally {
    abort.abort();
    await reader?.cancel().catch(() => {});
    conn.close();
  }
}

// ---- reference client (tests, and the spec in executable form) --------------------------

/** Builds the bytes a guest sends. */
export function encodeRequest({ method = "GET", url, headers = {}, body }) {
  const bytes = body === undefined ? new Uint8Array(0) : typeof body === "string" ? encoder.encode(body) : body;
  let head = `${method} ${url}\n`;
  for (const [name, value] of Object.entries(headers)) head += `${name}: ${value}\n`;
  if (bytes.length > 0) head += `content-length: ${bytes.length}\n`;
  return concat(encoder.encode(head + "\n"), bytes);
}

/**
 * Decodes a complete response as a guest would.
 * @returns {{ status: number, statusText: string, headers: Record<string,string>, body: Uint8Array, complete: boolean, error?: string }
 *           | { error: string }}
 */
export function decodeResponse(bytes) {
  const text = new TextDecoder("utf-8");
  let pos = 0;
  const line = () => {
    const end = bytes.indexOf(10, pos);
    if (end < 0) return undefined;
    const value = text.decode(bytes.subarray(pos, end));
    pos = end + 1;
    return value;
  };
  const first = line();
  if (first === undefined) return { error: "empty response" };
  if (first.startsWith("ERROR ")) return { error: first.slice(6) };
  const match = /^(\d{3}) ?(.*)$/.exec(first);
  if (!match) return { error: `bad status line: ${first}` };
  const headers = {};
  for (let l = line(); l !== undefined && l !== ""; l = line()) {
    const colon = l.indexOf(":");
    headers[l.slice(0, colon)] = l.slice(colon + 1).trim();
  }
  const chunks = [];
  for (;;) {
    const size = line();
    if (size === undefined) return { status: +match[1], statusText: match[2], headers, body: concatAll(chunks), complete: false, error: "truncated" };
    const length = parseInt(size, 16);
    if (length === 0) break;
    chunks.push(bytes.subarray(pos, pos + length));
    pos += length;
  }
  const trailer = line();
  const ok = trailer === "OK";
  return { status: +match[1], statusText: match[2], headers, body: concatAll(chunks), complete: ok, ...(ok ? {} : { error: trailer ?? "truncated" }) };
}

function concatAll(chunks) {
  const out = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.length;
  }
  return out;
}
