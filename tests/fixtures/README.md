A tiny certificate chain for `tests/runtime.e2e.mjs`, valid ten years and checked in so the test
needs no `openssl` on the target (Windows). Not secret: it protects nothing.

- `localhost-ca.pem` — the root. The engine is told to trust it with `COLLABO_CORE_CA_FILE`.
- `localhost-cert.pem` / `localhost-key.pem` — the server certificate for `localhost`
  (and `127.0.0.1`), signed by that root, for the test's local HTTPS server.

The root and the server certificate are separate because that is how real TLS works, and
because rustls (which the engine uses) refuses a CA certificate presented as a server's own.
