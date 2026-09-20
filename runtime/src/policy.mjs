// Network policy: which hosts the sandbox may reach, and which secrets the host adds to requests.
//
//   {
//     allow: ["*"],                  host patterns; "*" = everything
//     deny: [],                      checked first; a match always wins
//     allowHostLoopback: false,      may the guest reach this computer's 127.0.0.1 (via 192.0.2.1)?
//     secrets: [{ host: "api.anthropic.com", header: "x-api-key", value: "..." }],
//     extraAllowedHeaders: [],       request headers the guest may set, beyond the defaults
//   }
//
// Patterns: "example.com" matches exactly that host; "*.example.com" matches its subdomains (not
// example.com itself); "*" matches any host. IP addresses are matched as written. Case-insensitive.
// Secrets are only ever added to https: requests, so a key never travels in plain text.

export const DEFAULT_NETWORK_POLICY = Object.freeze({
  allow: ["*"],
  deny: [],
  allowHostLoopback: false,
  secrets: [],
  extraAllowedHeaders: [],
});

export function normalizeNetworkPolicy(input = {}) {
  const p = { ...DEFAULT_NETWORK_POLICY, ...input };
  const list = (v, name) => {
    if (!Array.isArray(v) || !v.every((x) => typeof x === "string")) throw new TypeError(`network.${name} must be a list of strings`);
    return v.map((x) => x.trim().toLowerCase()).filter(Boolean);
  };
  const secrets = (Array.isArray(p.secrets) ? p.secrets : []).map((s, i) => {
    if (!s || typeof s.host !== "string" || typeof s.header !== "string" || typeof s.value !== "string") {
      throw new TypeError(`network.secrets[${i}] needs string host, header and value`);
    }
    if (/[\r\n]/.test(s.value) || !/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(s.header)) throw new TypeError(`network.secrets[${i}] is not a valid header`);
    return { host: s.host.toLowerCase(), header: s.header.toLowerCase(), value: s.value };
  });
  return {
    allow: list(p.allow, "allow"),
    deny: list(p.deny, "deny"),
    allowHostLoopback: Boolean(p.allowHostLoopback),
    secrets,
    extraAllowedHeaders: list(p.extraAllowedHeaders ?? [], "extraAllowedHeaders"),
  };
}

export function hostMatches(pattern, host) {
  host = host.toLowerCase().replace(/\.$/, "");
  if (pattern === "*") return true;
  if (pattern.startsWith("*.")) return host.endsWith(pattern.slice(1)) && host.length > pattern.length - 1;
  return host === pattern;
}

/** Why `host` is refused, or undefined if it is allowed. */
export function hostDenied(policy, host) {
  const hit = policy.deny.find((p) => hostMatches(p, host));
  if (hit) return `"${host}" matches the deny rule "${hit}"`;
  if (!policy.allow.some((p) => hostMatches(p, host))) return `"${host}" is not in the allow list`;
  return undefined;
}

/** The secret headers to add to a request for `url`. Never for plain http. */
export function secretHeadersFor(policy, url) {
  const u = new URL(url);
  if (u.protocol !== "https:") return [];
  return policy.secrets.filter((s) => hostMatches(s.host, u.hostname));
}

/** Replaces secret values in `text` (e.g. an error message) with a placeholder. */
export function redact(policy, text) {
  let out = String(text);
  for (const s of policy.secrets) if (s.value.length >= 4) out = out.split(s.value).join("[secret]");
  return out;
}

/** Is `host` this computer (localhost, 127.0.0.0/8, ::1, 0.0.0.0)? */
export function isLoopbackHost(host) {
  const h = host.toLowerCase().replace(/^\[|\]$/g, "").replace(/\.$/, "");
  return h === "localhost" || h.endsWith(".localhost") || /^127\.\d+\.\d+\.\d+$/.test(h) || h === "::1" || h === "0.0.0.0" || h === "::";
}
