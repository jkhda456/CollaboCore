"""Guest-side test of `collabo_core` against the page's request bridge. Run with the boot harness in
API=mock mode, which serves a fake internet (echo, status codes, big, stream, cut, fail)."""
import sys, json, time
import collabo_core

fails = []
def check(name):
    def deco(fn):
        try:
            fn(); print(f"ok    {name}", flush=True)
        except BaseException as e:      # noqa: BLE001
            fails.append(name); print(f"FAIL  {name}: {type(e).__name__}: {e}", flush=True)
        return fn
    return deco

def raises(exc, fn, check_fn=None):
    try:
        fn()
    except exc as e:
        if check_fn: assert check_fn(e), repr(e)
        return e
    raise AssertionError(f"expected {exc.__name__}")

@check("GET: status, headers, json body; cookies off and redirects on (browser defaults)")
def _():
    r = collabo_core.get("https://mock.test/echo?x=1")
    assert (r.status, r.reason, r.ok) == (200, "OK", True), r
    assert r.headers["Content-Type"] == "application/json" and r.headers.get("X-MOCK") == "1"
    d = r.json()
    assert d["method"] == "GET" and d["url"] == "https://mock.test/echo?x=1"
    assert d["credentials"] == "omit" and d["redirect"] == "follow"

@check("the body can be read more than once (.text, .json(), .read() agree)")
def _():
    r = collabo_core.get("https://mock.test/echo")
    first = r.read()
    assert r.text == first.decode() and r.json()["method"] == "GET" and r.read() == first
    assert list(r.iter_bytes()) == [first], "iterating after a full read replays the body"
    part = collabo_core.get("https://mock.test/stream")
    it = part.iter_bytes(); head = next(it)
    assert head == b"one\n" and part.read() == b"two\nthree\n", "a body already streamed cannot be replayed"

@check("POST json=: body and allowed headers arrive unchanged; content-type is set")
def _():
    d = collabo_core.post("https://mock.test/echo", json={"q": "héllo 한글", "n": [1, 2]}, headers={"x-api-key": "K"}).json()
    assert d["method"] == "POST" and json.loads(d["body"]) == {"q": "héllo 한글", "n": [1, 2]}
    assert d["headers"]["x-api-key"] == "K" and d["headers"]["content-type"] == "application/json"

@check("POST data= (str and bytes), explicit content-type kept")
def _():
    a = collabo_core.post("https://mock.test/echo", data="plain text", headers={"content-type": "text/plain"}).json()
    assert a["body"] == "plain text" and a["headers"]["content-type"] == "text/plain"
    assert collabo_core.put("https://mock.test/echo", data=b"bytes").json()["method"] == "PUT"
    assert collabo_core.delete("https://mock.test/echo").json()["method"] == "DELETE"
    assert collabo_core.patch("https://mock.test/echo", data="p").json()["method"] == "PATCH"

@check("a header the host does not allow is refused by name")
def _():
    e = raises(collabo_core.HostError, lambda: collabo_core.get("https://mock.test/echo", headers={"cookie": "s=1"}),
               lambda e: e.kind == "header-not-allowed" and '"cookie"' in e.message)
    assert isinstance(e, OSError)

@check("bad requests are refused: scheme, method, URL")
def _():
    raises(collabo_core.HostError, lambda: collabo_core.get("file:///etc/passwd"), lambda e: e.kind == "scheme-not-allowed")
    raises(collabo_core.HostError, lambda: collabo_core.request("TRACE", "https://mock.test/"), lambda e: e.kind == "bad-request")
    raises(collabo_core.HostError, lambda: collabo_core.get("https://user:pw@mock.test/"), lambda e: e.kind == "bad-request")
    raises(ValueError, lambda: collabo_core.get("https://mock.test/", headers={"x-a": "b\nc: d"}))
    raises(ValueError, lambda: collabo_core.post("https://mock.test/", data="x", json={}))

@check("HTTP errors are responses; raise_for_status turns them into StatusError")
def _():
    r = collabo_core.get("https://mock.test/status/404")
    assert (r.status, r.ok) == (404, False) and r.text.strip() == "status 404"
    e = raises(collabo_core.StatusError, lambda: collabo_core.get("https://mock.test/status/500").raise_for_status(),
               lambda e: e.response.status == 500)
    assert collabo_core.get("https://mock.test/echo").raise_for_status().status == 200

@check("network failure: HostError kind 'network'")
def _():
    raises(collabo_core.HostError, lambda: collabo_core.get("https://mock.test/fail"), lambda e: e.kind == "network")

@check("large body arrives whole; iter_bytes streams it in pieces")
def _():
    assert len(collabo_core.get("https://mock.test/big").read()) == 204800
    pieces = list(collabo_core.get("https://mock.test/big").iter_bytes())
    assert sum(map(len, pieces)) == 204800 and len(pieces) >= 1

@check("streaming: chunks arrive as the host sends them")
def _():
    t0 = time.monotonic(); seen = []
    with collabo_core.get("https://mock.test/stream") as r:
        assert r.headers["content-type"] == "text/event-stream"
        for chunk in r.iter_bytes():
            seen.append((chunk, time.monotonic() - t0))
    assert b"".join(c for c, _ in seen) == b"one\ntwo\nthree\n"
    assert seen[0][1] < seen[-1][1] - 0.1, "the first chunk was not delayed until the end"

@check("a cut-off body is an error, not a silently short result")
def _():
    r = collabo_core.get("https://mock.test/cut")
    got = []
    e = raises(collabo_core.IncompleteResponse, lambda: [got.append(c) for c in r.iter_bytes()],
               lambda e: "interrupted" in e.message)
    assert b"".join(got) == b"partial"
    raises(collabo_core.IncompleteResponse, r.read)   # stays failed

@check("1 MiB upload")
def _():
    body = ("abcdefg\n" * 131072)
    d = collabo_core.post("https://mock.test/echo", data=body, headers={"content-type": "text/plain"}).json()
    assert len(d["body"]) == 1048576 and d["body"] == body

@check("HEAD has no body; many sequential requests")
def _():
    r = collabo_core.head("https://mock.test/echo")
    assert r.status == 200 and r.read() == b""
    for i in range(15):
        assert collabo_core.get(f"https://mock.test/echo?i={i}").status == 200

@check("threads can make requests at the same time")
def _():
    import threading
    out = []
    def go(i): out.append(collabo_core.get(f"https://mock.test/echo?t={i}").json()["url"])
    ts = [threading.Thread(target=go, args=(i,)) for i in range(4)]
    [t.start() for t in ts]; [t.join() for t in ts]
    assert sorted(out) == [f"https://mock.test/echo?t={i}" for i in range(4)]

@check("asyncio: requests from a thread pool")
def _():
    import asyncio
    async def main():
        rs = await asyncio.gather(*(asyncio.to_thread(collabo_core.get, f"https://mock.test/echo?a={i}") for i in range(3)))
        assert [r.status for r in rs] == [200, 200, 200]
    asyncio.run(main())

@check("urllib: urlopen, POST data, HTTPError, URLError (after install_urllib)")
def _():
    import urllib.request, urllib.error
    collabo_core.install_urllib()
    with urllib.request.urlopen("https://mock.test/echo?via=urllib") as r:
        assert r.status == 200 and r.getheader("x-mock") == "1"
        assert json.loads(r.read())["url"] == "https://mock.test/echo?via=urllib"
    req = urllib.request.Request("https://mock.test/echo", data=b"k=v", headers={"x-api-key": "U"})
    d = json.loads(urllib.request.urlopen(req).read())
    assert d["method"] == "POST" and d["body"] == "k=v" and d["headers"]["x-api-key"] == "U"
    assert "user-agent" not in d["headers"], "urllib's default User-Agent must not reach the host"
    e = raises(urllib.error.HTTPError, lambda: urllib.request.urlopen("https://mock.test/status/404"), lambda e: e.code == 404)
    raises(urllib.error.URLError, lambda: urllib.request.urlopen("https://mock.test/fail"), lambda e: "network" in str(e.reason))
    assert len(urllib.request.urlopen("https://mock.test/big").read()) == 204800

print(f"\nHOSTAPI: {'all passed' if not fails else str(len(fails)) + ' failed: ' + ', '.join(fails)}", flush=True)
sys.exit(1 if fails else 0)
