"""collabo_core - talk to the host page from inside the sandbox.

The sandbox has no network of its own. The page around it performs HTTP(S) requests in the
browser on the program's behalf, over a vsock connection; this module is the client for that.
One request per call, at the level of a browser fetch(): a method, a URL (http: or https:,
used as written), a few request headers, an optional body; back come a status, headers and a
streamed body.

    import collabo_core

    r = collabo_core.get("https://api.example.com/v1/models", headers={"x-api-key": key})
    r.raise_for_status()
    data = r.json()

    with collabo_core.post(url, json={"q": "hi"}, headers={"authorization": "Bearer ..."}) as r:
        for chunk in r.iter_bytes():        # streaming, e.g. server-sent events
            ...

Only a few request headers are accepted by the host (by default accept, content-type,
authorization, x-api-key); anything else raises HostError("header-not-allowed") naming the
header. Cookies are never sent and redirects are followed. For code that already uses urllib:

    collabo_core.install_urllib()               # urllib.request.urlopen() now goes through the host

httpx2 (and so the openai SDK) can use the host the same way:

    client = collabo_core.openai_client()       # openai.OpenAI; the host adds the API key
    httpx2.Client(transport=collabo_core.HostTransport())

The wire format is documented in release/app/http-protocol.js.
"""
import io
import json as _json
import os
import socket

__all__ = ["request", "get", "post", "put", "delete", "head", "patch", "Response", "Headers",
           "HostError", "IncompleteResponse", "StatusError", "install_urllib",
           "HostTransport", "AsyncHostTransport", "openai_client"]

VMADDR_CID_HOST = 2
DEFAULT_PORT = 1080
_CHUNK = 64 * 1024


class HostError(OSError):
    """The host refused or could not perform the request. `kind` says why, e.g.
    'header-not-allowed', 'scheme-not-allowed', 'request-too-large', 'network', 'timeout', 'busy'."""

    def __init__(self, kind, message):
        super().__init__(f"{kind}: {message}")
        self.kind = kind
        self.message = message


class IncompleteResponse(HostError):
    """The status and headers arrived but the body was cut off: what was read is not all of it."""

    def __init__(self, message, kind="incomplete"):
        super().__init__(kind, message)


class StatusError(OSError):
    """raise_for_status() on a 4xx or 5xx response."""

    def __init__(self, response):
        super().__init__(f"HTTP {response.status} {response.reason}".rstrip())
        self.response = response


class Headers(dict):
    """Response headers: names are lower case, lookups are case-insensitive."""

    def __getitem__(self, key):
        return super().__getitem__(key.lower())

    def get(self, key, default=None):
        return super().get(key.lower(), default)

    def __contains__(self, key):
        return super().__contains__(key.lower())


def _split_error(text):
    kind, _, message = text.partition(": ")
    return (kind, message) if message else ("error", text)


class Response:
    """A response whose body is read on demand from the connection."""

    def __init__(self, sock, status, reason, headers, buffer):
        self._sock = sock
        self._buffer = buffer          # bytes received but not yet consumed
        self._done = False
        self._error = None
        self._body = None              # the whole body, once read() has fetched it
        self._started = False          # iter_bytes() has begun: the body can no longer be replayed
        self.status = status
        self.reason = reason
        self.headers = headers

    # -- reading the framed body -------------------------------------------------------------

    def _fill(self):
        chunk = self._sock.recv(_CHUNK)
        if not chunk:
            raise IncompleteResponse("the connection closed before the body ended")
        self._buffer += chunk

    def _line(self):
        while True:
            end = self._buffer.find(b"\n")
            if end >= 0:
                line = bytes(self._buffer[:end])
                del self._buffer[:end + 1]
                return line.decode("utf-8", "replace")
            self._fill()

    def _exactly(self, n):
        while len(self._buffer) < n:
            self._fill()
        data = bytes(self._buffer[:n])
        del self._buffer[:n]
        return data

    def iter_bytes(self):
        """Yields the body in the pieces it arrives in. Raises IncompleteResponse if it is cut off."""
        if self._body is not None:         # already read in full: replay it
            if self._body:
                yield self._body
            return
        if self._error:
            raise self._error
        self._started = True
        try:
            while not self._done:
                try:
                    size = int(self._line(), 16)
                except ValueError:
                    raise IncompleteResponse("malformed response from the host", "protocol") from None
                if size == 0:
                    trailer = self._line()
                    self._done = True
                    if trailer != "OK":
                        kind, message = _split_error(trailer[6:] if trailer.startswith("ERROR ") else trailer)
                        raise IncompleteResponse(message, kind)
                    break
                remaining = size
                while remaining:
                    piece = self._exactly(min(remaining, _CHUNK))
                    remaining -= len(piece)
                    yield piece
        except (IncompleteResponse, OSError) as error:
            self._error = error if isinstance(error, IncompleteResponse) else IncompleteResponse(str(error))
            self.close()
            raise self._error from None
        finally:
            if self._done:
                self.close()

    def read(self):
        """The whole body as bytes. Reading it again (or .text / .json()) gives the same bytes;
        after iter_bytes() has started, only what is left can be returned."""
        if self._body is None:
            streamed = self._started
            data = b"".join(self.iter_bytes())
            if streamed:
                return data
            self._body = data
        return self._body

    @property
    def text(self):
        content_type = self.headers.get("content-type", "")
        charset = "utf-8"
        for part in content_type.split(";")[1:]:
            name, _, value = part.strip().partition("=")
            if name.lower() == "charset" and value:
                charset = value.strip("\"'")
        return self.read().decode(charset, "replace")

    def json(self):
        return _json.loads(self.read())

    @property
    def ok(self):
        return self.status < 400

    def raise_for_status(self):
        if not self.ok:
            raise StatusError(self)
        return self

    def close(self):
        try:
            self._sock.close()
        except OSError:
            pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

    def __iter__(self):
        return self.iter_bytes()

    def __repr__(self):
        return f"<Response {self.status} {self.reason}>".replace(" >", ">")


def request(method, url, headers=None, data=None, json=None, timeout=None, port=None):
    """Performs one request and returns a Response once the status and headers are in.

    headers: dict of request headers. data: bytes or str body. json: a value to send as a JSON
    body (sets content-type if you did not). timeout: seconds to wait on the connection.
    """
    method = method.upper()
    headers = dict(headers or {})
    if json is not None:
        if data is not None:
            raise ValueError("pass either data or json, not both")
        data = _json.dumps(json, ensure_ascii=False)
        if not any(k.lower() == "content-type" for k in headers):
            headers["content-type"] = "application/json"
    body = data.encode("utf-8") if isinstance(data, str) else (data or b"")

    head = [f"{method} {url}"]
    for name, value in headers.items():
        text = f"{name}: {value}"
        if "\n" in text or "\r" in text:
            raise ValueError(f"header {name!r} contains a line break")
        head.append(text)
    if body:
        head.append(f"content-length: {len(body)}")
    payload = ("\n".join(head) + "\n\n").encode("utf-8") + body

    sock = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    try:
        sock.settimeout(timeout)
        sock.connect((VMADDR_CID_HOST, port or int(os.environ.get("HFETCH_PORT", DEFAULT_PORT))))
        sock.sendall(payload)

        buffer = bytearray()
        while b"\n\n" not in buffer and not (buffer.startswith(b"ERROR ") and b"\n" in buffer):
            chunk = sock.recv(_CHUNK)
            if not chunk:
                raise HostError("no-response", "the host closed the connection without answering")
            buffer += chunk
        if buffer.startswith(b"ERROR "):
            line = bytes(buffer).split(b"\n", 1)[0].decode("utf-8", "replace")
            raise HostError(*_split_error(line[6:]))

        head_bytes, _, rest = bytes(buffer).partition(b"\n\n")
        lines = head_bytes.decode("utf-8", "replace").split("\n")
        code, _, reason = lines[0].partition(" ")
        response_headers = Headers()
        for line in lines[1:]:
            name, _, value = line.partition(":")
            response_headers[name.strip().lower()] = value.strip()
        return Response(sock, int(code), reason, response_headers, bytearray(rest))
    except BaseException:
        sock.close()
        raise


def get(url, **kwargs):
    return request("GET", url, **kwargs)


def head(url, **kwargs):
    return request("HEAD", url, **kwargs)


def post(url, **kwargs):
    return request("POST", url, **kwargs)


def put(url, **kwargs):
    return request("PUT", url, **kwargs)


def patch(url, **kwargs):
    return request("PATCH", url, **kwargs)


def delete(url, **kwargs):
    return request("DELETE", url, **kwargs)


# ---- urllib ------------------------------------------------------------------------------------

# Headers urllib adds by itself that the host does not take from a guest (it sets its own).
_URLLIB_DEFAULTS = {"user-agent", "host", "connection", "content-length", "accept-encoding"}


def install_urllib():
    """Makes urllib.request.urlopen() (and everything built on it) use the host for http: and
    https: URLs. Status errors surface as urllib.error.HTTPError, host failures as URLError."""
    import email.message
    import urllib.error
    import urllib.request
    import urllib.response

    class _Body(io.RawIOBase):
        def __init__(self, response):
            self._it = response.iter_bytes()
            self._left = b""

        def readable(self):
            return True

        def readinto(self, buffer):
            while not self._left:
                try:
                    self._left = next(self._it)
                except StopIteration:
                    return 0
            n = min(len(buffer), len(self._left))
            buffer[:n] = self._left[:n]
            self._left = self._left[n:]
            return n

    class _Opened(urllib.response.addinfourl):
        """What urlopen() returns for http.client's HTTPResponse: the same reading methods plus
        the header accessors code written against http.client tends to use."""

        version = 11

        def getheader(self, name, default=None):
            return self.headers.get(name, default)

        def getheaders(self):
            return list(self.headers.items())

    class HostHandler(urllib.request.BaseHandler):
        handler_order = 100          # ahead of urllib's own HTTP handlers (order 500)

        def _open(self, req):
            headers = {k: v for k, v in req.header_items() if k.lower() not in _URLLIB_DEFAULTS}
            try:
                # req.timeout is a number, or a sentinel object meaning "no timeout was given".
                timeout = req.timeout if isinstance(req.timeout, (int, float)) else None
                response = request(req.get_method(), req.full_url, headers=headers, data=req.data, timeout=timeout)
            except HostError as error:
                raise urllib.error.URLError(error) from error
            message = email.message.Message()
            for name, value in response.headers.items():
                message[name] = value
            opened = _Opened(io.BufferedReader(_Body(response)), message, req.full_url, response.status)
            opened.msg = opened.reason = response.reason
            return opened

        http_open = https_open = _open

    urllib.request.install_opener(urllib.request.build_opener(HostHandler))


# ---- httpx (the openai SDK) --------------------------------------------------------------------

# Headers httpx and SDKs built on it add by themselves: the host sets its own transport headers,
# and SDK telemetry (x-stainless-*) is not the program's to send. Any other header is passed on,
# so the host's allow-list still refuses what it does not accept, with a message.
_HTTPX_DEFAULTS = _URLLIB_DEFAULTS | {"transfer-encoding"}
# The host hands over bodies already decoded, so these no longer describe what arrives.
_DECODED = {"content-encoding", "content-length", "transfer-encoding"}


def _host_transports():
    """HostTransport and AsyncHostTransport, built on first use (httpx2 is imported only then)."""
    import asyncio
    import httpx2

    send = request  # the module's request(); inside the transports `request` is httpx's

    def forward(request):
        return {name: value for name, value in request.headers.items()
                if name.lower() not in _HTTPX_DEFAULTS and not name.lower().startswith("x-stainless-")}

    def timeout_of(request):
        timeouts = request.extensions.get("timeout") or {}
        values = [t for t in (timeouts.get("connect"), timeouts.get("read")) if t is not None]
        return max(values) if values else None

    def raise_as_httpx(error, request):
        if isinstance(error, IncompleteResponse):
            raise httpx2.RemoteProtocolError(str(error), request=request) from error
        if error.kind == "timeout":
            raise httpx2.ReadTimeout(str(error), request=request) from error
        if error.kind in ("network", "no-response", "busy"):
            raise httpx2.ConnectError(str(error), request=request) from error
        raise httpx2.RequestError(str(error), request=request) from error

    def to_response(response, stream):
        headers = [(k, v) for k, v in response.headers.items() if k not in _DECODED]
        return httpx2.Response(response.status, headers=headers, stream=stream,
                               extensions={"reason_phrase": (response.reason or "").encode()})

    class _Body(httpx2.SyncByteStream):
        def __init__(self, response, request):
            self.response, self.request = response, request

        def __iter__(self):
            try:
                yield from self.response.iter_bytes()
            except HostError as error:
                raise_as_httpx(error, self.request)

        def close(self):
            self.response.close()

    class HostTransport(httpx2.BaseTransport):
        """An httpx2 transport that sends every request through the host (see the module doc).
        The host applies its network policy and adds the secrets it holds for the destination."""

        def handle_request(self, request):
            try:
                response = send(request.method, str(request.url), headers=forward(request),
                                data=request.read(), timeout=timeout_of(request))
            except HostError as error:
                raise_as_httpx(error, request)
            return to_response(response, _Body(response, request))

    class _AsyncBody(httpx2.AsyncByteStream):
        def __init__(self, response, request):
            self.response, self.request = response, request

        async def __aiter__(self):
            chunks = self.response.iter_bytes()
            while True:
                try:
                    chunk = await asyncio.to_thread(next, chunks, None)
                except HostError as error:
                    raise_as_httpx(error, self.request)
                if chunk is None:
                    return
                yield chunk

        async def aclose(self):
            self.response.close()

    class AsyncHostTransport(httpx2.AsyncBaseTransport):
        """HostTransport for httpx2.AsyncClient: the blocking vsock exchange runs in a thread."""

        async def handle_async_request(self, request):
            body = await request.aread()
            try:
                response = await asyncio.to_thread(send, request.method, str(request.url),
                                                   headers=forward(request), data=body,
                                                   timeout=timeout_of(request))
            except HostError as error:
                raise_as_httpx(error, request)
            return to_response(response, _AsyncBody(response, request))

    return HostTransport, AsyncHostTransport


def __getattr__(name):
    if name in ("HostTransport", "AsyncHostTransport"):
        sync, async_ = _host_transports()
        globals().update(HostTransport=sync, AsyncHostTransport=async_)
        return globals()[name]
    raise AttributeError(f"module 'collabo_core' has no attribute {name!r}")


def openai_client(*, api_key=None, async_=False, **kwargs):
    """An openai.OpenAI (or AsyncOpenAI) client whose requests go through the host.

    The key usually stays with the host: it adds the secret it holds for the API's host (the
    app's `secrets` setting), replacing whatever the guest sent, so the key given here — or
    OPENAI_API_KEY, or a placeholder — never needs to be the real one. Other arguments go to
    the client as usual (base_url, timeout, max_retries, ...)."""
    import httpx2
    import openai

    if api_key is None:
        api_key = os.environ.get("OPENAI_API_KEY") or "host-managed"
    if async_:
        return openai.AsyncOpenAI(api_key=api_key,
                                  http_client=httpx2.AsyncClient(transport=__getattr__("AsyncHostTransport")()),
                                  **kwargs)
    return openai.OpenAI(api_key=api_key, http_client=httpx2.Client(transport=__getattr__("HostTransport")()),
                         **kwargs)


# ---- host functions ----------------------------------------------------------------------------

HOST_FUNCTIONS_PORT = 1081


class HostCallError(OSError):
    """A host function failed or was refused. `kind`: unknown-function, denied, bad-request,
    failed, timeout."""

    def __init__(self, kind, message):
        super().__init__(f"{kind}: {message}")
        self.kind = kind
        self.message = message


class _Host:
    """Functions the host application offers to the sandbox (see `hostcall --list`).

        collabo_core.host.list()                        names of the available functions
        collabo_core.host.call("clipboard.read")        an app-defined function
        collabo_core.host.exec(["git", "status"], cwd="/Users/me/project")
        collabo_core.host.exec(["code", "."], gui=True) start a host GUI app, detached
        collabo_core.host.open("https://example.com")   open with the host's default app

    Running host programs is off unless the app allows it (hostExec "allow", or "ask" and the
    user agrees); otherwise HostCallError(kind="denied").
    """

    def call(self, fn, args=None, timeout=None, **kwargs):
        request = {"fn": fn, "args": {**(args or {}), **kwargs}}
        sock = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
        try:
            sock.settimeout(timeout)
            try:
                sock.connect((VMADDR_CID_HOST, HOST_FUNCTIONS_PORT))
            except OSError as e:
                raise HostCallError("unavailable", f"the host offers no functions to this sandbox ({e})") from None
            sock.sendall((_json.dumps(request, ensure_ascii=False) + "\n").encode("utf-8"))
            data = bytearray()
            while b"\n" not in data:
                chunk = sock.recv(_CHUNK)
                if not chunk:
                    break
                data += chunk
        finally:
            sock.close()
        if not data:
            raise HostCallError("no-response", "the host did not answer")
        response = _json.loads(bytes(data).split(b"\n", 1)[0])
        if not response.get("ok"):
            error = response.get("error") or {}
            raise HostCallError(error.get("kind", "failed"), error.get("message", "unknown error"))
        return response.get("result")

    def list(self):
        return self.call("list")

    def info(self):
        return self.call("info")

    def exec(self, argv, cwd=None, gui=False, input=None, timeout=None, check=False):
        """Runs a program on the host. Returns subprocess.CompletedProcess (text output), or for
        gui=True a dict with the pid of the started app."""
        import subprocess
        args = {"argv": list(argv), "gui": bool(gui)}
        if cwd is not None:
            args["cwd"] = cwd
        if input is not None:
            args["stdin"] = input
        if timeout is not None:
            args["timeoutMs"] = int(timeout * 1000)
        result = self.call("exec", args)
        if gui:
            return result
        done = subprocess.CompletedProcess(list(argv), result["exitCode"], result.get("stdout", ""), result.get("stderr", ""))
        if check:
            done.check_returncode()
        return done

    def open(self, target):
        return self.call("open", {"target": target})


host = _Host()
__all__ += ["host", "HostCallError"]
