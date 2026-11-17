#!/usr/bin/env python3
"""The network policy on a real guest, by address rather than by name:

  - privateNetworks (--private-networks allow|deny|only): its sockets (wget, nc, nslookup: net.rs)
    and the request API (hfetch: http.rs), by address, by a name that resolves into a private
    network (nip.io), through a redirect, the exemption of a host the allow list names exactly,
    loopback left to --allow-loopback, and the app's start and policy.update (--stdio);
  - the lists' address rules (--allow/--deny IP, CIDR block or IPv4 wildcard such as 192.168.1.*,
    network.allow/deny): by address and
    by a name that resolves into the block, an allow block's exemption from privateNetworks, a
    name still needing a pattern, and a rule written wrong refused (engine, start, policy.update);
  - the request API's loopback by the address a name resolves to (127.0.0.1.nip.io);
  - redirects the request API follows, each one checked like the first (allow/deny), the app's
    secrets only for their own host, the guest's own keys only within their origin, the methods
    ureq used, and the limit.

  python3 tests/test_network_policy.py   engine/target/release/collabo-core-engine and the
                                         build's images, or the runtime COLLABO_RUNTIME names

Needs a private IPv4 address on this computer (the LAN); the checks by name and the public ones
need DNS and the internet, and are skipped without them.
"""
import base64
import http.server
import ipaddress
import json
import os
import socket
import ssl
import subprocess
import sys
import threading
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
runtime = os.environ.get("COLLABO_RUNTIME")
if runtime:
    images = Path(runtime) / "app" / "images"
    ENGINE = [str(Path(runtime) / "bin" / "collabo-core-engine"), "--kernel", str(images / "vmlinux.wasm"),
              "--initramfs", str(images / "initramfs.cpio")]
else:
    ENGINE = [str(ROOT / "engine/target/release/collabo-core-engine"), "--kernel", str(ROOT / "kernel/linux/vmlinux.wasm"),
              "--initramfs", str(ROOT / "userspace/out/initramfs.cpio")]

failures = 0


def check(ok, what, detail=""):
    global failures
    print(("ok   " if ok else "FAIL ") + what)
    if not ok:
        failures += 1
        if detail:
            print("     " + str(detail)[-2000:].replace("\n", "\n     "))


def skip(what, why):
    print(f"skip {what} ({why})")


def net(err):
    """The network log's lines and hfetch's errors out of the guest's stderr (no boot messages)."""
    return "\n".join(line for line in err.splitlines() if "[network]" in line or "hfetch:" in line or "agent:" in line)


def guest(script, *options):
    """`sh -c script` in the guest with the network log on stderr: (exit status, stdout, stderr)."""
    run = subprocess.run(ENGINE[:1] + ["exec"] + ENGINE[1:] + ["--log-network", "-", *options, "--cwd", "/tmp", "--", "sh", "-c", script],
                         capture_output=True, timeout=300)
    return run.returncode, run.stdout.decode(errors="replace"), run.stderr.decode(errors="replace")


# This computer's LAN address (the interface the default route leaves by).
probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
try:
    probe.connect(("10.255.255.255", 1))
    LAN = probe.getsockname()[0]
except OSError:
    LAN = None
finally:
    probe.close()
if not LAN or not ipaddress.ip_address(LAN).is_private or ipaddress.ip_address(LAN).is_loopback:
    print(f"skip: no private IPv4 address on this computer ({LAN})")
    sys.exit(0)


# Where /cross redirects: another origin (set below, once the second server has a port).
CROSS = {}


class Handler(http.server.BaseHTTPRequestHandler):
    def redirect(self, status, location):
        self.send_response(status)
        self.send_header("location", location)
        self.send_header("content-length", "0")
        self.end_headers()

    def reply(self, body):
        self.send_response(200)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def answer(self):
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length) if length else b""
        path = self.path
        if path == "/to-public":
            return self.redirect(302, "https://example.com/")
        if path == "/rel":
            return self.redirect(301, "echo?from=rel")
        if path == "/cross":
            return self.redirect(302, CROSS["url"])
        if path == "/to-other-host":  # https, from localhost to 127.0.0.1: another host, its secret not
            return self.redirect(302, f"https://127.0.0.1:{self.server.server_address[1]}/echo")
        if path == "/to-self":
            return self.redirect(302, "/echo?again")
        if path.startswith("/loop"):
            return self.redirect(302, "/loop")
        if path == "/post302":
            return self.redirect(302, "/echo")
        if path == "/post307":
            return self.redirect(307, "/echo")
        if path.startswith("/echo"):
            seen = {"method": self.command, "path": path, "body": body.decode(errors="replace"),
                    **{name: self.headers.get(name) for name in ("authorization", "x-api-key", "accept", "content-type")}}
            return self.reply((json.dumps(seen) + "\n").encode())
        self.reply(b"lan-ok\n")

    do_GET = do_POST = do_HEAD = answer

    def log_message(self, *args):
        pass


def serve(secure=False):
    server = http.server.ThreadingHTTPServer(("0.0.0.0", 0), Handler)
    if secure:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(ROOT / "tests/fixtures/localhost-cert.pem", ROOT / "tests/fixtures/localhost-key.pem")
        server.socket = context.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


server, other, secure = serve(), serve(), serve(secure=True)
PORT, OTHER, SECURE = server.server_address[1], other.server_address[1], secure.server_address[1]
URL = f"http://{LAN}:{PORT}/"
NAME = f"{LAN}.nip.io"  # a public DNS name for the LAN address
CROSS["url"] = f"http://{LAN}:{OTHER}/echo?crossed"


def resolves(name, want=None):
    try:
        found = {info[4][0] for info in socket.getaddrinfo(name, 80, socket.AF_INET)}
    except OSError:
        return False
    return want in found if want else bool(found)


by_name = resolves(NAME, LAN)
public = resolves("example.com")
print(f"LAN {LAN}, port {PORT}; {NAME}: {'resolves' if by_name else 'no DNS'}; example.com: {'resolves' if public else 'no DNS'}")

# Both ways in: a socket (wget) and the request API (hfetch).
BOTH = f"wget -q -T 5 -O - {URL}; echo wget=$?; hfetch {URL}; echo hfetch=$?"

# --- allow: as before ---------------------------------------------------------------------------
code, out, err = guest(BOTH)
check("lan-ok\nwget=0" in out and "lan-ok\nhfetch=0" in out, "default (allow): the LAN address is reached by socket and request API", out + net(err))

# --- deny: the internet only ------------------------------------------------------------------
code, out, err = guest(BOTH, "--private-networks", "deny")
check("lan-ok" not in out and "wget=0" not in out and "hfetch=2" in out, "deny: the LAN address is refused to both", out)
check(f'reason: Some("{LAN} is in a private network (privateNetworks is deny)")' in err, "deny: the socket's refusal says why (network log)", net(err))
check(f"blocked GET {URL}: blocked by the network policy: {LAN} is in a private network (privateNetworks is deny)" in err,
      "deny: the request API's refusal says why", net(err))
code, out, err = guest("hfetch http://[fe80::1]:9/; echo hfetch=$?", "--private-networks", "deny")
check("hfetch=2" in out and "fe80::1 is in a private network" in err, "deny: a link-local IPv6 address in the URL too", out + net(err))
if by_name:
    script = f"nslookup {NAME} 192.0.2.1; echo nslookup=$?; wget -q -T 5 -O - http://{NAME}:{PORT}/; echo wget=$?; hfetch http://{NAME}:{PORT}/; echo hfetch=$?"
    code, out, err = guest(script, "--private-networks", "deny")
    check(f"Address: {LAN}" not in out.split("nslookup=")[0] and "lan-ok" not in out and "hfetch=2" in out,
          "deny: a name that resolves into the LAN gets no address, and the request API refuses it", out)
    check(f'Dns {{ host: "{NAME.lower()}", addresses: [], blocked: true, reason: Some("\\"{NAME.lower()}\\" is {LAN}, which is in a private network' in err,
          "deny: the guest's DNS answer says why", net(err))
    check(f'"{NAME}" is {LAN}, which is in a private network (privateNetworks is deny)' in err, "deny: the request API names the name and its address", net(err))
    # A host the allow list names exactly is exempt; its address by itself is not.
    code, out, err = guest(f"wget -q -T 5 -O - http://{NAME}:{PORT}/; echo wget=$?; hfetch http://{NAME}:{PORT}/; echo hfetch=$?; hfetch {URL}; echo literal=$?",
                           "--private-networks", "deny", "--allow", "*", "--allow", NAME)
    check("lan-ok\nwget=0" in out and "lan-ok\nhfetch=0" in out and "literal=2" in out,
          "deny + --allow NAME (exactly): that name is reached, the bare address still refused", out + net(err))
    code, out, err = guest(f"hfetch http://{NAME}:{PORT}/; echo hfetch=$?", "--private-networks", "deny", "--allow", "*.nip.io")
    check("hfetch=2" in out and f'"{NAME}" is {LAN}, which is in a private network' in err, "deny + --allow *.nip.io (a wildcard): not exempt", out + net(err))
else:
    skip("deny by name, and the exemption", f"{NAME} does not resolve here")
if public:
    code, out, err = guest("hfetch -i https://example.com/ | head -1; nslookup example.com 192.0.2.1 | grep -c '^Address' ", "--private-networks", "deny")
    check(out.startswith("200") and out.split()[-1] != "0", "deny: the internet still answers (request API and DNS)", out + net(err))
else:
    skip("deny: the internet still answers", "example.com does not resolve here")

# --- only: the local networks only ------------------------------------------------------------
code, out, err = guest(BOTH, "--private-networks", "only")
check("lan-ok\nwget=0" in out and "lan-ok\nhfetch=0" in out, "only: the LAN address is reached by both", out + net(err))
code, out, err = guest("nc -w 3 1.1.1.1 80 </dev/null; echo nc=$?; hfetch http://1.1.1.1/; echo hfetch=$?", "--private-networks", "only")
check("nc=0" not in out and "hfetch=2" in out and 'reason: Some("1.1.1.1 is not in a private network (privateNetworks is only)")' in err
      and "blocked GET http://1.1.1.1/: blocked by the network policy: 1.1.1.1 is not in a private network" in err,
      "only: a public address is refused to both, saying why", out + net(err))
if by_name:
    code, out, err = guest(f"wget -q -T 5 -O - http://{NAME}:{PORT}/; echo wget=$?", "--private-networks", "only")
    check("lan-ok\nwget=0" in out, "only: a name that resolves into the LAN is reached", out + net(err))
if public:
    code, out, err = guest(f"nslookup example.com 192.0.2.1; echo nslookup=$?; hfetch https://example.com/; echo hfetch=$?; hfetch {URL}to-public; echo redirect=$?",
                           "--private-networks", "only")
    check("hfetch=2" in out and f'blocked GET https://example.com/: blocked by the network policy: "example.com" is ' in err,
          "only: a public name is refused by the address it resolves to", out + net(err))
    check("redirect=2" in out and f"GET {URL}to-public -> 302" in err
          and f'blocked GET https://example.com/: blocked by the network policy: "example.com" is ' in err and f"(a redirect from {URL}to-public)" in err,
          "only: a LAN server's redirect to a public name is refused at the redirect", out + net(err))
    check('Dns { host: "example.com", addresses: [], blocked: true' in err, "only: the guest's DNS gives a public name no address", net(err))
    code, out, err = guest("hfetch -i https://example.com/ | head -1", "--private-networks", "only", "--allow", "*", "--allow", "example.com")
    check(out.startswith("200"), "only + --allow example.com (exactly): that host is reached", out + net(err))
else:
    skip("only: public names, redirects and the exemption", "example.com does not resolve here")

# --- loopback stays --allow-loopback's ---------------------------------------------------------
code, out, err = guest(f"wget -q -T 5 -O - http://192.0.2.1:{PORT}/; echo wget=$?; hfetch http://127.0.0.1:{PORT}/; echo hfetch=$?",
                       "--private-networks", "only", "--allow-loopback")
check("lan-ok\nwget=0" in out and "lan-ok\nhfetch=0" in out, "only + --allow-loopback: this computer's own services, by gateway and 127.0.0.1", out + net(err))
code, out, err = guest(f"hfetch http://127.0.0.1:{PORT}/; echo hfetch=$?", "--private-networks", "deny")
check("hfetch=2" in out and "allowHostLoopback is off" in err, "deny without --allow-loopback: loopback is refused by its own rule", out + net(err))

# --- a wrong value -----------------------------------------------------------------------------
code, out, err = guest("true", "--private-networks", "block")
check(code != 0 and "--private-networks block: allow, deny or only" in err, "--private-networks block: refused, the engine does not start", net(err))

# --- address rules in the lists: an IP or a CIDR block, by address -------------------------------
BLOCK = str(ipaddress.ip_network(f"{LAN}/24", strict=False))  # the LAN's /24, e.g. 192.168.0.0/24
code, out, err = guest(BOTH, "--deny", BLOCK)
check("lan-ok" not in out and "wget=0" not in out and "hfetch=2" in out, f"--deny {BLOCK}: the LAN address is refused to both", out + net(err))
check(f'reason: Some("{LAN} matches the deny rule \\"{BLOCK}\\"")' in err
      and f'blocked GET {URL}: blocked by the network policy: "{LAN}" matches the deny rule "{BLOCK}"' in err,
      "--deny CIDR: both refusals name the rule", net(err))
if by_name:
    script = f"nslookup {NAME} 192.0.2.1; echo nslookup=$?; wget -q -T 5 -O - http://{NAME}:{PORT}/; echo wget=$?; hfetch http://{NAME}:{PORT}/; echo hfetch=$?"
    code, out, err = guest(script, "--deny", BLOCK)
    check(f"Address: {LAN}" not in out.split("nslookup=")[0] and "lan-ok" not in out and "hfetch=2" in out,
          "--deny CIDR: a name that resolves into the block gets no address, and the request API refuses it", out + net(err))
    check(f'"{NAME.lower()}\\" is {LAN}, which matches the deny rule \\"{BLOCK}\\"' in err
          and f'"{NAME}" is {LAN}, which matches the deny rule "{BLOCK}"' in err,
          "--deny CIDR: the DNS answer and the request API say which name, address and rule", net(err))
    # A bare IP in the deny list is an address too: its names do not get around it.
    code, out, err = guest(f"wget -q -T 5 -O - http://{NAME}:{PORT}/; echo wget=$?; hfetch http://{NAME}:{PORT}/; echo hfetch=$?", "--deny", LAN)
    check("lan-ok" not in out and "hfetch=2" in out and f'which matches the deny rule "{LAN}"' in err, "--deny IP: refused by a name that resolves to it", out + net(err))
    # The exemption from privateNetworks: the addresses an allow CIDR block holds, by any name.
    code, out, err = guest(f"{BOTH}; wget -q -T 5 -O - http://{NAME}:{PORT}/; echo name=$?",
                           "--private-networks", "deny", "--allow", "*", "--allow", BLOCK)
    check(out.count("lan-ok") == 3 and "wget=0" in out and "hfetch=0" in out and "name=0" in out,
          f"--private-networks deny + --allow {BLOCK}: that subnet is reached, by address and by name", out + net(err))
else:
    skip("--deny CIDR by name, the allow block's exemption by name", f"{NAME} does not resolve here")
code, out, err = guest(f"hfetch http://169.254.169.254/; echo metadata=$?", "--private-networks", "deny", "--allow", "*", "--allow", BLOCK)
check("metadata=2" in out and "169.254.169.254 is in a private network" in err, "--private-networks deny + --allow CIDR: the other private ones stay refused", out + net(err))
# In the allow list alone: those addresses and no other, and no name.
code, out, err = guest(f"{BOTH}; nc -w 3 1.1.1.1 80 </dev/null; echo nc=$?; hfetch http://1.1.1.1/; echo public=$?", "--allow", BLOCK)
check("lan-ok\nwget=0" in out and "lan-ok\nhfetch=0" in out and "nc=0" not in out and "public=2" in out
      and '"1.1.1.1" is not in the allow list' in err, f"--allow {BLOCK} alone: the LAN address is reached, a public one is not", out + net(err))
if public:
    code, out, err = guest("hfetch https://example.com/; echo hfetch=$?", "--allow", BLOCK)
    check("hfetch=2" in out and '"example.com" is not in the allow list' in err, "--allow CIDR alone: a name still needs a pattern", out + net(err))
# An IPv4 address with * for its last octets is that CIDR block.
WILD = ".".join(LAN.split(".")[:3]) + ".*"  # e.g. 192.168.0.*
code, out, err = guest(BOTH, "--deny", WILD)
check("lan-ok" not in out and "hfetch=2" in out and f'reason: Some("{LAN} matches the deny rule \\"{WILD}\\"")' in err
      and f'"{LAN}" matches the deny rule "{WILD}"' in err, f"--deny {WILD}: the LAN address is refused to both, naming the rule", out + net(err))
if by_name:
    code, out, err = guest(f"wget -q -T 5 -O - http://{NAME}:{PORT}/; echo wget=$?; hfetch http://{NAME}:{PORT}/; echo hfetch=$?", "--deny", WILD)
    check("lan-ok" not in out and "hfetch=2" in out and f'"{NAME}" is {LAN}, which matches the deny rule "{WILD}"' in err,
          "--deny IPv4 wildcard: refused by a name that resolves into it too", out + net(err))
code, out, err = guest(f"{BOTH}; hfetch http://169.254.169.254/; echo metadata=$?", "--private-networks", "deny", "--allow", "*", "--allow", WILD)
check(out.count("lan-ok") == 2 and "metadata=2" in out, f"--private-networks deny + --allow {WILD}: that subnet is exempt, the others not", out + net(err))
# Written wrong: the engine does not start, rather than leave a rule that matches nothing.
for wrong, says in ((f"{LAN}/16", "has bits set past its /16"), ("10.0.0.0/33", "the prefix length is 0 to 32"),
                    ("10.*.0.1", "* stands for whole octets at the end"), ("fd00::*", "IPv6 takes no *"), ("api*.example.com", "a host is")):
    code, out, err = guest("true", "--deny", wrong)
    check(code != 0 and f"--deny \"{wrong}\"" in err and says in err, f"--deny {wrong}: refused, the engine does not start", err[-300:])

# --- the app's network.privateNetworks (the --stdio protocol the Dart package speaks) -------------
class App:
    """The engine under --stdio: requests by id, events kept."""

    def __init__(self):
        self.process = subprocess.Popen(ENGINE + ["--stdio"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.next, self.events = 0, []

    def call(self, method, params):
        self.next += 1
        self.process.stdin.write((json.dumps({"id": self.next, "method": method, "params": params}) + "\n").encode())
        self.process.stdin.flush()
        for line in self.process.stdout:
            message = json.loads(line)
            if message.get("id") == self.next:
                return message
            self.events.append(message)
        raise RuntimeError("the engine ended")

    def run(self, script):
        answer = self.call("exec", {"argv": ["sh", "-c", script], "cwd": "/tmp"})["result"]
        return base64.b64decode(answer["stdoutBase64"]).decode(errors="replace")

    def close(self):
        self.process.stdin.close()
        self.process.wait(timeout=60)


app = App()
started = app.call("start", {"config": {"python": False, "tools": False, "network": {"privateNetworks": "deny"}}})
out = app.run(f"hfetch {URL}; echo hfetch=$?") if "result" in started else ""
blocked = [e for e in app.events if e.get("event") == "network" and e.get("blocked")]
check("hfetch=2" in out and any("is in a private network (privateNetworks is deny)" in json.dumps(e) for e in blocked),
      "app: start with network.privateNetworks deny refuses the LAN, with a blocked network event", (started, out, blocked[-1:]))
updated = app.call("policy.update", {"network": {"privateNetworks": "only"}})
out = app.run(f"hfetch {URL}; echo hfetch=$?; hfetch http://1.1.1.1/; echo public=$?")
check("result" in updated and "lan-ok\nhfetch=0" in out and "public=2" in out, "app: policy.update to only: the LAN answers, a public address is refused", (updated, out))
wrong = app.call("policy.update", {"network": {"privateNetworks": "block"}})
out = app.run(f"hfetch {URL}; echo hfetch=$?")
check(wrong.get("error", {}).get("kind") == "bad-request" and "lan-ok\nhfetch=0" in out,
      "app: policy.update with a wrong value: bad-request, and the policy stays", (wrong, out))
app.close()
app = App()
wrong = app.call("start", {"config": {"python": False, "tools": False, "network": {"privateNetworks": "lan"}}})
check(wrong.get("error", {}).get("kind") == "bad-request" and "privateNetworks" in wrong["error"].get("message", ""),
      "app: start with a wrong value is refused", wrong)
app.close()

# --- the app's CIDR rules (network.allow / network.deny) ---------------------------------------
app = App()
started = app.call("start", {"config": {"python": False, "tools": False, "network": {"deny": [BLOCK]}}})
out = app.run(f"hfetch {URL}; echo hfetch=$?") if "result" in started else ""
blocked = [e for e in app.events if e.get("event") == "network" and e.get("blocked")]
check("hfetch=2" in out and any(f'matches the deny rule \\"{BLOCK}\\"' in json.dumps(e) for e in blocked),
      "app: start with network.deny [CIDR] refuses the LAN, with a blocked network event", (started, out, blocked[-1:]))
wrong = app.call("policy.update", {"network": {"deny": [], "allow": [f"{LAN}/8"]}})
out = app.run(f"hfetch {URL}; echo hfetch=$?")
check(wrong.get("error", {}).get("kind") == "bad-request" and "network.allow" in wrong["error"].get("message", "") and "hfetch=2" in out,
      "app: policy.update with a CIDR block written wrong: bad-request, and the deny stays", (wrong, out))
updated = app.call("policy.update", {"network": {"deny": [], "allow": [BLOCK]}})
out = app.run(f"hfetch {URL}; echo hfetch=$?; hfetch http://1.1.1.1/; echo public=$?")
check("result" in updated and "lan-ok\nhfetch=0" in out and "public=2" in out, "app: policy.update to allow [CIDR]: the LAN answers, a public address does not", (updated, out))
app.close()

# --- the request API's loopback, by the address a name resolves to ------------------------------
if resolves("127.0.0.1.nip.io", "127.0.0.1"):
    code, out, err = guest(f"hfetch http://127.0.0.1.nip.io:{PORT}/; echo hfetch=$?")
    check("lan-ok" not in out and "hfetch=2" in out and '"127.0.0.1.nip.io" is 127.0.0.1, this computer (allowHostLoopback is off)' in err,
          "a name that resolves to 127.0.0.1 does not reach this computer without --allow-loopback", out + net(err))
    code, out, err = guest(f"hfetch http://127.0.0.1.nip.io:{PORT}/; echo hfetch=$?", "--allow-loopback")
    check("lan-ok\nhfetch=0" in out, "with --allow-loopback it does", out + net(err))
else:
    skip("loopback by a name's address", "127.0.0.1.nip.io does not resolve here")
code, out, err = guest(f"hfetch http://[::ffff:127.0.0.1]:{PORT}/; echo mapped=$?; hfetch http://2130706433:{PORT}/; echo number=$?")
check("lan-ok" not in out and "mapped=2" in out and "number=2" in out and "::ffff:127.0.0.1 is this computer (allowHostLoopback is off)" in err,
      "127.0.0.1 written as ::ffff:127.0.0.1 or 2130706433 is this computer too", out + net(err))

# --- redirects, each checked like the first --------------------------------------------------------
code, out, err = guest(f"hfetch -i {URL}to-public; echo hfetch=$?", "--allow", LAN)
check("hfetch=2" in out and f'blocked GET https://example.com/: blocked by the network policy: "example.com" is not in the allow list (a redirect from {URL}to-public)' in err
      and f"[network] GET {URL}to-public -> 302" in err,
      "a redirect to a host the allow list does not name is refused, after the 302 (network log)", out + net(err))
code, out, err = guest(f"hfetch {URL}to-public; echo hfetch=$?", "--deny", "example.com")
check("hfetch=2" in out and 'matches the deny rule "example.com" (a redirect from' in err, "a redirect to a denied host is refused", out + net(err))
code, out, err = guest(f"hfetch -H 'Authorization: Bearer guest-1' -H 'x-api-key: guest-2' {URL}rel")
seen = json.loads(out) if out.startswith("{") else {}
check(seen.get("path") == "/echo?from=rel" and seen.get("authorization") == "Bearer guest-1" and seen.get("x-api-key") == "guest-2",
      "a relative redirect within the origin keeps the guest's headers", out + net(err))
code, out, err = guest(f"hfetch -H 'Authorization: Bearer guest-1' -H 'x-api-key: guest-2' -H 'accept: text/plain' {URL}cross")
seen = json.loads(out) if out.startswith("{") else {}
check(seen.get("path") == "/echo?crossed" and seen.get("authorization") is None and seen.get("x-api-key") is None and seen.get("accept") == "text/plain",
      "a redirect to another origin drops the guest's keys (accept stays)", out + net(err))
secret = ["--allow-loopback", "--skip-tls-verify", "--secret", "localhost:x-api-key=sk-app-secret"]
code, out, err = guest(f"hfetch https://localhost:{SECURE}/to-self", *secret)
seen = json.loads(out) if out.startswith("{") else {}
check(seen.get("path") == "/echo?again" and seen.get("x-api-key") == "sk-app-secret", "the app's secret goes on a redirect within its host", out + net(err))
code, out, err = guest(f"hfetch https://localhost:{SECURE}/to-other-host", *secret)
seen = json.loads(out) if out.startswith("{") else {}
check(seen.get("path") == "/echo" and "x-api-key" in seen and seen["x-api-key"] is None and "sk-app-secret" not in out + err,
      "the app's secret does not follow a redirect to another host", out + net(err))
code, out, err = guest(f"hfetch -X POST -H 'content-type: text/plain' -d payload {URL}post302; echo; hfetch -i -X POST -d payload {URL}post307 | head -1")
seen = json.loads(out.splitlines()[0]) if out.startswith("{") else {}
check(seen.get("method") == "GET" and seen.get("body") == "" and seen.get("content-type") is None and "307" in out.splitlines()[-1],
      "POST and 302: GET without the body; POST and 307: the 307 itself (as ureq did)", out + net(err))
code, out, err = guest(f"hfetch {URL}loop; echo hfetch=$?")
check("hfetch=2" in out and "too many redirects (more than 10)" in err and err.count(f"GET {URL}loop -> 302") == 11,
      "a redirect loop ends after 10 redirects", out + net(err))

for each in (server, other, secure):
    each.shutdown()
print(f"\n{'all passed' if not failures else str(failures) + ' failed'}")
sys.exit(1 if failures else 0)
