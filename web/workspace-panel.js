// The "Workspace" panel: what is in /work right now, how it is saved, and a button that
// downloads the whole thing as a .zip. The list is the ZipStore's own view of its tree.
//
// Paths come from the guest and are untrusted: they are only ever written with textContent.

const MAX_ROWS = 300;

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

export function formatBytes(n) {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB"];
  let value = n / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

function defaultDownload(blob, filename) {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  document.body.append(link);
  link.click();
  link.remove();
  // The download has started by the time the click returns; free the blob a little later.
  setTimeout(() => URL.revokeObjectURL(url), 30_000);
}

/**
 * @param {HTMLElement} root
 * @param {import("./zip-store.js").ZipStore} store
 * @param {{ download?: (blob: Blob, filename: string) => void, filename?: string, refreshMs?: number,
 *           notice?: string }} options `notice` is shown above the list (e.g. a failed restore).
 */
export function createWorkspacePanel(root, store, { download = defaultDownload, filename = "workspace.zip", refreshMs = 300, notice } = {}) {
  root.classList.add("workspace");

  const head = el("div", "ws-head");
  const title = el("strong", "", "Workspace");
  const path = el("code", "ws-path", "/work");
  const button = el("button", "ws-download", "Download zip");
  button.type = "button";
  head.append(title, path, button);

  const summary = el("div", "ws-summary");
  const noticeLine = el("div", "ws-notice", notice ?? "");
  noticeLine.hidden = !notice;
  const empty = el("div", "ws-empty", "Empty. Files the agent writes under /work show up here.");
  const list = el("ol", "ws-list");
  root.append(head, summary, noticeLine, empty, list);

  button.addEventListener("click", () => {
    try {
      download(store.toZipBlob(), filename);
    } catch (error) {
      // e.g. more than 65535 entries: say so instead of failing silently.
      noticeLine.textContent = `Could not build the zip: ${error?.message ?? error}`;
      noticeLine.hidden = false;
    }
  });

  function saveStatus(stats) {
    if (!stats.persistent) return "not saved between visits (memory only)";
    if (stats.lastError) return `save failed: ${stats.lastError?.message ?? stats.lastError}`;
    if (stats.dirty) return "unsaved changes";
    return stats.lastSaved ? `saved ${new Date(stats.lastSaved).toTimeString().slice(0, 8)}` : "saved";
  }

  function render() {
    const stats = store.stats();
    const entries = store.list();
    const files = entries.filter((e) => e.type !== "dir").length;
    summary.textContent = `${files} file${files === 1 ? "" : "s"} · ${formatBytes(stats.bytes)} of ${formatBytes(stats.maxBytes)} · ${saveStatus(stats)}`;
    summary.classList.toggle("bad", Boolean(stats.lastError));
    empty.hidden = entries.length > 0;

    const rows = entries.slice(0, MAX_ROWS).map((entry) => {
      const row = el("li", `ws-row ${entry.type}`);
      row.append(el("span", "ws-name", entry.type === "dir" ? `${entry.path}/` : entry.path));
      if (entry.type === "file") row.append(el("span", "ws-size", formatBytes(entry.size)));
      else if (entry.type === "symlink") row.append(el("span", "ws-size", "link"));
      return row;
    });
    if (entries.length > MAX_ROWS) rows.push(el("li", "ws-more", `… and ${entries.length - MAX_ROWS} more (all are in the zip)`));
    list.replaceChildren(...rows);
  }

  // Changes arrive in bursts (every write); redraw at most once per `refreshMs`.
  let timer;
  const unsubscribe = store.subscribe(() => {
    timer ??= setTimeout(() => {
      timer = undefined;
      render();
    }, refreshMs);
  });
  render();

  return {
    render,
    /** Shows a message above the list. */
    setNotice(text) {
      noticeLine.textContent = text;
      noticeLine.hidden = !text;
    },
    dispose() {
      clearTimeout(timer);
      unsubscribe();
    },
  };
}
