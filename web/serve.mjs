#!/usr/bin/env node
// Tiny static server for testing release/ locally with the headers the kernel
// needs (cross-origin isolation, application/wasm). No dependencies.
//
//   node serve.mjs [port]      # default 8080, then open http://localhost:8080/
import { createServer } from "node:http";
import { createReadStream } from "node:fs";
import { stat } from "node:fs/promises";
import { extname, join, normalize, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(fileURLToPath(new URL(".", import.meta.url)));
const port = Number(process.argv[2]) || 8080;

const types = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
  ".wasm": "application/wasm",
};

createServer(async (req, res) => {
  const headers = {
    "Cross-Origin-Embedder-Policy": "require-corp",
    "Cross-Origin-Opener-Policy": "same-origin",
    "Cross-Origin-Resource-Policy": "cross-origin",
    "Cache-Control": "no-cache",
  };
  try {
    // Not `new URL(req.url)`: a request target like "//" parses as a host and throws.
    let path = normalize(decodeURIComponent(req.url.split(/[?#]/)[0]));
    if (path.endsWith(sep) || path.endsWith("/")) path = join(path, "index.html");
    const file = join(root, path);
    if (file !== root && !file.startsWith(root + sep)) throw Object.assign(new Error(), { code: "ENOENT" });
    const info = await stat(file);
    if (!info.isFile()) throw Object.assign(new Error(), { code: "ENOENT" });

    const range = /^bytes=(\d*)-(\d*)$/.exec(req.headers.range ?? "");
    const type = types[extname(file)] ?? "application/octet-stream";
    if (range && (range[1] || range[2])) {
      const start = range[1] ? Number(range[1]) : Math.max(info.size - Number(range[2]), 0);
      const end = range[1] && range[2] ? Math.min(Number(range[2]), info.size - 1) : info.size - 1;
      if (start > end || start >= info.size) {
        res.writeHead(416, { ...headers, "Content-Range": `bytes */${info.size}` }).end();
        return;
      }
      res.writeHead(206, {
        ...headers,
        "Content-Type": type,
        "Accept-Ranges": "bytes",
        "Content-Range": `bytes ${start}-${end}/${info.size}`,
        "Content-Length": end - start + 1,
      });
      createReadStream(file, { start, end }).pipe(res);
      return;
    }
    res.writeHead(200, {
      ...headers,
      "Content-Type": type,
      "Accept-Ranges": "bytes",
      "Content-Length": info.size,
    });
    createReadStream(file).pipe(res);
  } catch (error) {
    res.writeHead(error.code === "ENOENT" ? 404 : 500, headers).end(error.code === "ENOENT" ? "not found" : "error");
  }
}).listen(port, () => console.log(`serving ${root} at http://localhost:${port}/`));
