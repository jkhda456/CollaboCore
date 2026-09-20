// A live list of the HTTP requests the guest sends out through the host.
//
// The guest speaks plain HTTP to the virtual gateway; the host turns each request into a
// fetch(). This panel shows every one of them, so the person watching can see where the
// agent is talking to. URLs and status text originate in the (untrusted) guest, so
// everything is written with textContent, never innerHTML.

const MAX_ROWS = 500;

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function clock(date) {
  return date.toTimeString().slice(0, 8);
}

export function createRequestLog(root) {
  root.classList.add("reqlog");

  const head = el("div", "reqlog-head");
  const title = el("strong", "", "HTTP requests");
  const count = el("span", "reqlog-count", "0");
  const clear = el("button", "reqlog-clear", "Clear");
  clear.type = "button";
  head.append(title, count, clear);

  const empty = el("div", "reqlog-empty", "No requests yet. From the shell try: hfetch https://example.com/");
  const list = el("ol", "reqlog-list");
  root.append(head, empty, list);

  let total = 0;
  const update = () => {
    count.textContent = String(total);
    empty.hidden = list.childElementCount > 0;
  };
  clear.addEventListener("click", () => {
    list.replaceChildren();
    total = 0;
    update();
  });
  update();

  return {
    /** Records a request that has just left the guest. Returns a handle for `finish`/`fail`. */
    start(method, url, { bodyBytes = 0, source } = {}) {
      const row = el("li", "reqlog-row pending");
      const line1 = el("div", "reqlog-line");
      const time = el("span", "reqlog-time", clock(new Date()));
      const verb = el("span", "reqlog-method", method);
      const status = el("span", "reqlog-status", "…");
      line1.append(time, verb);
      // Which guest path it came in on: "api" (hfetch, request level) or "net" (the NIC).
      if (source) line1.append(el("span", "reqlog-source", source));
      line1.append(status);

      const target = el("div", "reqlog-url", url);
      target.title = url;
      row.append(line1, target);
      if (bodyBytes > 0) row.append(el("div", "reqlog-meta", `request body ${bodyBytes} bytes`));

      list.prepend(row);
      while (list.childElementCount > MAX_ROWS) list.lastElementChild.remove();
      total++;
      update();
      return { row, status, started: performance.now() };
    },

    /** The response head arrived. `label` overrides the numeric status (e.g. blocked redirect). */
    finish(entry, statusCode, label) {
      const ms = Math.round(performance.now() - entry.started);
      entry.status.textContent = `${label ?? statusCode} · ${ms} ms`;
      entry.row.classList.remove("pending");
      const ok = statusCode >= 200 && statusCode < 400 && !label;
      entry.row.classList.add(ok ? "ok" : "bad");
    },

    /** The fetch itself failed (network error, CORS, abort). */
    fail(entry, error) {
      const ms = Math.round(performance.now() - entry.started);
      entry.status.textContent = `failed · ${ms} ms`;
      entry.row.classList.remove("pending");
      entry.row.classList.add("bad");
      entry.row.append(el("div", "reqlog-meta", String(error?.message ?? error)));
    },
  };
}
