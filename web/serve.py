#!/usr/bin/env python3
"""Static server for testing this folder locally, with the headers the kernel needs.

The kernel is multi-threaded WebAssembly on SharedArrayBuffer, which browsers only
enable in a cross-origin isolated page, so plain `python -m http.server` is not enough.

    python serve.py [port]      # default 8080, then open http://localhost:8080/

Use http://localhost or http://127.0.0.1: other hostnames over plain http are not a
secure context, so isolation stays off.
"""
import os
import sys
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.abspath(__file__))


class Handler(SimpleHTTPRequestHandler):
    # compileStreaming() rejects anything but application/wasm.
    extensions_map = {**SimpleHTTPRequestHandler.extensions_map, ".wasm": "application/wasm", ".js": "text/javascript"}

    def end_headers(self):
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        self.send_header("Cross-Origin-Resource-Policy", "cross-origin")
        self.send_header("Cache-Control", "no-cache")
        super().end_headers()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    server = ThreadingHTTPServer(("", port), partial(Handler, directory=ROOT))
    print(f"serving {ROOT} at http://localhost:{port}/")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
