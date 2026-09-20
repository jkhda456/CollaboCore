import { JSDOM } from "jsdom";
import assert from "node:assert/strict";
const dom = new JSDOM('<!doctype html><aside id="r"></aside>');
globalThis.document = dom.window.document;
const { createRequestLog } = await import("../web/http-log.js");

const root = document.getElementById("r");
const log = createRequestLog(root);
const $ = (s) => root.querySelector(s);
const rows = () => [...root.querySelectorAll(".reqlog-row")];
const count = () => $(".reqlog-count").textContent;

// 1. initial state
assert.equal(count(), "0");
assert.equal($(".reqlog-empty").hidden, false, "empty hint shown");
assert.equal(rows().length, 0);

// 2. a pending request
const a = log.start("GET", "https://api.example.com/v1/models?limit=2");
assert.equal(rows().length, 1);
assert.equal(count(), "1");
assert.equal($(".reqlog-empty").hidden, true, "hint hidden once a row exists");
assert.match(rows()[0].className, /pending/);
assert.equal($(".reqlog-method").textContent, "GET");
assert.equal($(".reqlog-url").textContent, "https://api.example.com/v1/models?limit=2");
assert.equal($(".reqlog-url").title, "https://api.example.com/v1/models?limit=2");
assert.equal($(".reqlog-status").textContent, "…");

// 3. completion states
log.finish(a, 200);
assert.match(rows()[0].className, /\bok\b/);
assert.doesNotMatch(rows()[0].className, /pending/);
assert.match($(".reqlog-status").textContent, /^200 · \d+ ms$/);
const b = log.start("POST", "https://api.example.com/v1/messages", { bodyBytes: 42 });
assert.match(rows()[0].querySelector(".reqlog-meta").textContent, /42 bytes/);
assert.equal(rows()[0].querySelector(".reqlog-method").textContent, "POST", "newest row first");
log.finish(b, 429);
assert.match(rows()[0].className, /\bbad\b/);
const c = log.start("GET", "https://x.example/redirect");
log.finish(c, 0, "redirect (not followed)");
assert.match(rows()[0].className, /\bbad\b/, "a label marks the row bad even with a low status");
assert.match(rows()[0].querySelector(".reqlog-status").textContent, /^redirect \(not followed\) · \d+ ms$/);
const d = log.start("GET", "https://down.example/");
log.fail(d, new TypeError("Failed to fetch (network error or blocked by CORS)"));
assert.match(rows()[0].className, /\bbad\b/);
assert.match(rows()[0].querySelector(".reqlog-status").textContent, /^failed · \d+ ms$/);
assert.match(rows()[0].querySelector(".reqlog-meta").textContent, /CORS/);
assert.equal(count(), "4");

// 3b. which guest path a request came in on
const viaApi = log.start("GET", "https://api.example/x", { source: "api" });
assert.equal(rows()[0].querySelector(".reqlog-source").textContent, "api");
const viaNet = log.start("GET", "https://net.example/x", { source: "net" });
assert.equal(rows()[0].querySelector(".reqlog-source").textContent, "net");
const noSource = log.start("GET", "https://plain.example/x");
assert.equal(rows()[0].querySelector(".reqlog-source"), null, "no badge when no source is given");
log.finish(viaApi, 200); log.finish(viaNet, 200); log.finish(noSource, 200);
assert.equal(count(), "7");

// 4. untrusted text is never parsed as HTML
const evil = '"><img src=x onerror=alert(1)><script>window.pwned=1</script>';
const e = log.start("<b>GET</b>", "https://evil.example/" + evil, { source: "<i>api</i>" });
log.fail(e, evil);
assert.equal(root.querySelectorAll("img, script, b, i").length, 0, "no elements created from guest strings");
assert.equal(rows()[0].querySelector(".reqlog-url").textContent, "https://evil.example/" + evil);
assert.equal(rows()[0].querySelector(".reqlog-method").textContent, "<b>GET</b>");

// 5. clear
root.querySelector(".reqlog-clear").click();
assert.equal(rows().length, 0);
assert.equal(count(), "0");
assert.equal($(".reqlog-empty").hidden, false);

// 6. cap: keep the newest 500 rows, but count everything
for (let i = 0; i < 505; i++) log.finish(log.start("GET", `https://bulk.example/${i}`), 200);
assert.equal(rows().length, 500);
assert.equal(count(), "505");
assert.equal(rows()[0].querySelector(".reqlog-url").textContent, "https://bulk.example/504", "newest kept");
assert.equal(rows().at(-1).querySelector(".reqlog-url").textContent, "https://bulk.example/5", "oldest dropped");

console.log("http-log: all assertions passed");
