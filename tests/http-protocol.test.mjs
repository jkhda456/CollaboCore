// Unit tests for host/ or web/http-protocol.js: no kernel, no network, no browser.
//   node --test tests/http-protocol.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  BridgeError,
  decodeResponse,
  DEFAULT_ALLOWED_REQUEST_HEADERS,
  encodeRequest,
  handleConnection,
  parseRequestHead,
} from "../host/http-protocol.js";

const enc = new TextEncoder();
const dec = new TextDecoder();

/** A vsock-connection stand-in: `input` chunks are what the guest sends. */
function fakeConn(input, { failWritesAfter = Infinity } = {}) {
  const chunks = [...input];
  const written = [];
  return {
    written,
    closed: false,
    async read() {
      return chunks.shift() ?? new Uint8Array(0);
    },
    async write(data) {
      if (written.length >= failWritesAfter) throw new Error("vsock connection is closed");
      written.push(data.slice());
    },
    close() {
      this.closed = true;
    },
    output() {
      const total = written.reduce((n, c) => n + c.length, 0);
      const out = new Uint8Array(total);
      let at = 0;
      for (const c of written) {
        out.set(c, at);
        at += c.length;
      }
      return out;
    },
  };
}

function recorder() {
  const events = [];
  return {
    events,
    start(method, url, info) {
      events.push({ type: "start", method, url, ...info });
      return events.length - 1;
    },
    finish(entry, status, label) {
      events.push({ type: "finish", entry, status, label });
    },
    fail(entry, error) {
      events.push({ type: "fail", entry, message: String(error?.message ?? error) });
    },
  };
}

const bodyOf = (chunks) =>
  new ReadableStream({
    start(controller) {
      for (const c of chunks) controller.enqueue(typeof c === "string" ? enc.encode(c) : c);
      controller.close();
    },
  });

// ---- parsing ------------------------------------------------------------------------------

test("parses a request and normalises header names", () => {
  const r = parseRequestHead("POST https://api.example.com/v1/x?y=1\nContent-Type: application/json\nX-API-Key:  k \ncontent-length: 5");
  assert.equal(r.method, "POST");
  assert.equal(r.url, "https://api.example.com/v1/x?y=1");
  assert.equal(r.headers.get("content-type"), "application/json");
  assert.equal(r.headers.get("x-api-key"), "k", "value is trimmed");
  assert.equal(r.contentLength, 5);
  assert.equal(r.headers.has("content-length"), false, "content-length is framing, not forwarded");
});

test("http: and https: are both taken literally (no upgrade)", () => {
  assert.equal(parseRequestHead("GET http://internal.example:8080/a").url, "http://internal.example:8080/a");
  assert.equal(parseRequestHead("GET https://example.com/").url, "https://example.com/");
});

for (const [name, head, kind, message] of [
  ["unknown method", "TRACE https://a.example/", "bad-request", /not supported/],
  ["lowercase method", "get https://a.example/", "bad-request", /not supported/],
  ["no url", "GET", "bad-request", /METHOD URL/],
  ["relative url", "GET /path", "bad-request", /invalid URL/],
  ["file scheme", "GET file:///etc/passwd", "scheme-not-allowed", /file:/],
  ["data scheme", "GET data:text/plain,hi", "scheme-not-allowed", /data:/],
  ["url credentials", "GET https://user:pw@a.example/", "bad-request", /credentials in the URL/],
  ["CRLF line ends", "GET https://a.example/\r\naccept: */*", "bad-request", /LF/],
  ["header without colon", "GET https://a.example/\nnonsense", "bad-request", /malformed/],
  ["bad header name", "GET https://a.example/\nbad name: v", "bad-request", /invalid header name/],
  ["cookie", "GET https://a.example/\ncookie: s=1", "header-not-allowed", /"cookie" is not allowed/],
  ["host", "GET https://a.example/\nhost: evil.example", "header-not-allowed", /"host"/],
  ["user-agent", "GET https://a.example/\nuser-agent: x", "header-not-allowed", /"user-agent"/],
  ["proxy header", "GET https://a.example/\nproxy-authorization: x", "header-not-allowed", /proxy-authorization/],
  ["duplicate content-length", "POST https://a.example/\ncontent-length: 1\ncontent-length: 1", "bad-request", /duplicate/],
  ["non-numeric content-length", "POST https://a.example/\ncontent-length: -1", "bad-request", /invalid content-length/],
  ["GET with body", "GET https://a.example/\ncontent-length: 3", "bad-request", /cannot have a body/],
  ["HEAD with body", "HEAD https://a.example/\ncontent-length: 3", "bad-request", /cannot have a body/],
]) {
  test(`rejects: ${name}`, () => {
    assert.throws(
      () => parseRequestHead(head),
      (e) => e instanceof BridgeError && e.kind === kind && message.test(e.message),
    );
  });
}

test("the default allow-list is minimal", () => {
  assert.deepEqual(DEFAULT_ALLOWED_REQUEST_HEADERS, ["accept", "content-type", "authorization", "x-api-key"]);
});

test("allowHeaders extends the allow-list, case-insensitively", () => {
  const head = "GET https://a.example/\nAnthropic-Version: 2023-06-01";
  assert.throws(() => parseRequestHead(head), /not allowed/);
  const r = parseRequestHead(head, { allowHeaders: ["Anthropic-Version"] });
  assert.equal(r.headers.get("anthropic-version"), "2023-06-01");
  assert.throws(() => parseRequestHead("GET https://a.example/\naccept: */*", { allowHeaders: ["x-only"] }), /"accept" is not allowed/);
});

test("the error names the rejected header and what is allowed", () => {
  try {
    parseRequestHead("GET https://a.example/\ncookie: x");
    assert.fail("should throw");
  } catch (e) {
    assert.match(e.message, /cookie/);
    assert.match(e.message, /x-api-key/);
    assert.deepEqual(e.request, { method: "GET", url: "https://a.example/" });
  }
});

test("request-too-large is decided from content-length, before reading the body", () => {
  assert.throws(
    () => parseRequestHead("POST https://a.example/\ncontent-length: 11", { maxRequestBody: 10 }),
    (e) => e.kind === "request-too-large",
  );
  parseRequestHead("POST https://a.example/\ncontent-length: 10", { maxRequestBody: 10 });
});

// ---- executing requests -------------------------------------------------------------------

test("GET: request is a plain fetch with browser defaults; response is framed and complete", async () => {
  let call;
  const log = recorder();
  const conn = fakeConn([encodeRequest({ url: "https://api.example.com/v1/models", headers: { accept: "application/json" } })]);
  await handleConnection(conn, {
    log,
    fetch: async (url, init) => {
      call = { url, init };
      return new Response(bodyOf(['{"data":', "[]}"]), {
        status: 200,
        statusText: "OK",
        headers: { "content-type": "application/json", "x-request-id": "abc", "content-encoding": "gzip", "content-length": "999" },
      });
    },
  });

  assert.equal(call.url, "https://api.example.com/v1/models");
  assert.equal(call.init.method, "GET");
  assert.equal(call.init.headers.get("accept"), "application/json");
  assert.equal(call.init.body, undefined);
  assert.equal(call.init.credentials, "omit", "no cookies");
  assert.equal(call.init.redirect, "follow");
  assert.equal(call.init.referrerPolicy, "no-referrer");
  assert.ok(call.init.signal instanceof AbortSignal);

  const r = decodeResponse(conn.output());
  assert.equal(r.status, 200);
  assert.equal(r.statusText, "OK");
  assert.equal(r.complete, true);
  assert.equal(dec.decode(r.body), '{"data":[]}');
  assert.equal(r.headers["content-type"], "application/json");
  assert.equal(r.headers["x-request-id"], "abc");
  assert.equal("content-encoding" in r.headers, false, "fetch already decoded the body");
  assert.equal("content-length" in r.headers, false);
  assert.equal(conn.closed, true);
  assert.deepEqual(
    log.events.map((e) => e.type),
    ["start", "finish"],
  );
  assert.equal(log.events[0].source, "api");
  assert.equal(log.events[1].status, 200);
});

test("POST: body bytes and allowed headers reach fetch unchanged", async () => {
  let call;
  const payload = '{"model":"m","messages":[{"role":"user","content":"héllo"}]}';
  const conn = fakeConn([
    encodeRequest({ method: "POST", url: "https://api.example.com/v1/messages", headers: { "content-type": "application/json", "x-api-key": "secret" }, body: payload }),
  ]);
  await handleConnection(conn, {
    log: recorder(),
    fetch: async (url, init) => {
      call = { url, init };
      return new Response("ok", { status: 201 });
    },
  });
  assert.equal(dec.decode(call.init.body), payload);
  assert.equal(call.init.headers.get("x-api-key"), "secret");
  assert.equal(decodeResponse(conn.output()).status, 201);
});

test("input arriving in tiny pieces (head and body split anywhere) is reassembled", async () => {
  const bytes = encodeRequest({ method: "PUT", url: "https://a.example/x", headers: { "content-type": "text/plain" }, body: "hello world" });
  const pieces = Array.from(bytes, (b) => Uint8Array.of(b));
  let got;
  const conn = fakeConn(pieces);
  await handleConnection(conn, {
    fetch: async (url, init) => {
      got = dec.decode(init.body);
      return new Response(null, { status: 204 });
    },
  });
  assert.equal(got, "hello world");
  assert.equal(decodeResponse(conn.output()).status, 204);
});

test("HEAD and empty bodies produce an empty, complete body", async () => {
  const conn = fakeConn([encodeRequest({ method: "HEAD", url: "https://a.example/" })]);
  await handleConnection(conn, { fetch: async () => new Response(null, { status: 200, headers: { etag: '"v1"' } }) });
  const r = decodeResponse(conn.output());
  assert.equal(r.complete, true);
  assert.equal(r.body.length, 0);
  assert.equal(r.headers.etag, '"v1"');
});

test("error statuses are responses, not errors (1:1 with fetch)", async () => {
  const conn = fakeConn([encodeRequest({ url: "https://a.example/missing" })]);
  await handleConnection(conn, { fetch: async () => new Response("nope", { status: 404, statusText: "Not Found" }) });
  const r = decodeResponse(conn.output());
  assert.equal(r.status, 404);
  assert.equal(r.statusText, "Not Found");
  assert.equal(dec.decode(r.body), "nope");
  assert.equal(r.complete, true);
});

test("a large streamed body arrives complete and in order", async () => {
  const parts = Array.from({ length: 200 }, (_, i) => enc.encode(`chunk-${i};`.padEnd(1000, "x")));
  const conn = fakeConn([encodeRequest({ url: "https://a.example/big" })]);
  await handleConnection(conn, { fetch: async () => new Response(bodyOf(parts), { status: 200 }) });
  const r = decodeResponse(conn.output());
  assert.equal(r.complete, true);
  assert.equal(r.body.length, 200 * 1000);
  assert.equal(dec.decode(r.body.subarray(1000, 1008)), "chunk-1;");
});

test("status text and header values cannot inject lines into the response head", async () => {
  const conn = fakeConn([encodeRequest({ url: "https://a.example/" })]);
  const headers = new Headers({ "x-note": "a b" });
  await handleConnection(conn, { fetch: async () => ({ status: 200, statusText: "OK\nx-injected: 1", headers, body: null }) });
  const text = dec.decode(conn.output());
  assert.doesNotMatch(text, /^x-injected/m);
  assert.match(text.split("\n")[0], /^200 OK x-injected: 1$/);
});

// ---- failures -----------------------------------------------------------------------------

test("header not allowed: ERROR replaces the status line, fetch is never called, panel shows the attempt", async () => {
  let called = false;
  const log = recorder();
  const conn = fakeConn([encodeRequest({ url: "https://a.example/", headers: { cookie: "s=1" } })]);
  await handleConnection(conn, { log, fetch: async () => ((called = true), new Response("x")) });
  assert.equal(called, false);
  const r = decodeResponse(conn.output());
  assert.match(r.error, /^header-not-allowed: request header "cookie" is not allowed/);
  assert.deepEqual(
    log.events.map((e) => e.type),
    ["start", "fail"],
    "a rejected request is still visible",
  );
  assert.equal(log.events[0].url, "https://a.example/");
  assert.equal(conn.closed, true);
});

test("request too large: rejected before the body is read", async () => {
  const conn = fakeConn([enc.encode("POST https://a.example/\ncontent-length: 100\n\n")]);
  await handleConnection(conn, { maxRequestBody: 10, fetch: async () => assert.fail("must not fetch") });
  assert.match(decodeResponse(conn.output()).error, /^request-too-large/);
});

test("connection closed mid-body is a bad-request", async () => {
  const conn = fakeConn([enc.encode("POST https://a.example/\ncontent-length: 10\n\nabc")]);
  await handleConnection(conn, { fetch: async () => assert.fail("must not fetch") });
  assert.match(decodeResponse(conn.output()).error, /^bad-request: connection closed before the request body ended/);
});

test("an oversized head is refused", async () => {
  const conn = fakeConn([enc.encode("GET https://a.example/" + "a".repeat(20000))]);
  await handleConnection(conn, { fetch: async () => assert.fail("must not fetch") });
  assert.match(decodeResponse(conn.output()).error, /^bad-request: request head too large/);
});

test("network failure before any response: ERROR network, logged as failed", async () => {
  const log = recorder();
  const conn = fakeConn([encodeRequest({ url: "https://down.example/" })]);
  await handleConnection(conn, { log, fetch: async () => { throw new TypeError("Failed to fetch"); } });
  const r = decodeResponse(conn.output());
  assert.match(r.error, /^network: Failed to fetch \(network error or blocked by CORS\)/);
  assert.deepEqual(
    log.events.map((e) => e.type),
    ["start", "fail"],
  );
});

test("stream breaking after the head: the guest can tell the body is incomplete", async () => {
  const conn = fakeConn([encodeRequest({ url: "https://a.example/stream" })]);
  let sent = false;
  const broken = new ReadableStream({
    pull(controller) {
      if (!sent) {
        sent = true;
        controller.enqueue(enc.encode("partial"));
      } else {
        controller.error(new Error("connection reset"));
      }
    },
  });
  await handleConnection(conn, { log: recorder(), fetch: async () => new Response(broken, { status: 200 }) });
  const r = decodeResponse(conn.output());
  assert.equal(r.status, 200);
  assert.equal(dec.decode(r.body), "partial");
  assert.equal(r.complete, false, "a cut-off body must not look complete");
  assert.match(r.error, /^ERROR network: response body interrupted: connection reset/);
});

test("timeout: aborts the fetch and reports it", async () => {
  let signal;
  const conn = fakeConn([encodeRequest({ url: "https://slow.example/" })]);
  await handleConnection(conn, {
    timeoutMs: 20,
    fetch: (url, init) =>
      new Promise((_, reject) => {
        signal = init.signal;
        init.signal.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")));
      }),
  });
  assert.match(decodeResponse(conn.output()).error, /^timeout: no response within 20 ms/);
  assert.equal(signal.aborted, true);
});

test("the timeout covers the response head only, not a long stream", async () => {
  const conn = fakeConn([encodeRequest({ url: "https://sse.example/" })]);
  const slow = new ReadableStream({
    async start(controller) {
      controller.enqueue(enc.encode("event: a\n\n"));
      await new Promise((r) => setTimeout(r, 80));
      controller.enqueue(enc.encode("event: b\n\n"));
      controller.close();
    },
  });
  await handleConnection(conn, { timeoutMs: 20, fetch: async () => new Response(slow, { status: 200 }) });
  const r = decodeResponse(conn.output());
  assert.equal(r.complete, true);
  assert.equal(dec.decode(r.body), "event: a\n\nevent: b\n\n");
});

test("guest disconnects mid-stream: fetch is aborted and the body reader cancelled", async () => {
  let signal;
  let cancelled = false;
  const endless = new ReadableStream({
    pull(controller) {
      controller.enqueue(enc.encode("data\n"));
    },
    cancel() {
      cancelled = true;
    },
  });
  const conn = fakeConn([encodeRequest({ url: "https://a.example/endless" })], { failWritesAfter: 6 });
  await handleConnection(conn, {
    log: recorder(),
    fetch: async (url, init) => {
      signal = init.signal;
      return new Response(endless, { status: 200 });
    },
  });
  assert.equal(cancelled, true, "stopped downloading for a guest that is gone");
  assert.equal(signal.aborted, true);
  assert.equal(conn.closed, true);
});

test("handleConnection never throws, even when the connection is unusable", async () => {
  const conn = fakeConn([enc.encode("garbage")], { failWritesAfter: 0 });
  await handleConnection(conn, { fetch: async () => assert.fail("must not fetch") });
  assert.equal(conn.closed, true);
});
