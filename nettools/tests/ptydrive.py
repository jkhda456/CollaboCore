"""Drive a program in a pseudo-terminal: ptydrive.Session(argv, cols, rows); .send(bytes); .expect(regex, timeout)."""
import os, pty, re, select, struct, fcntl, termios, time, signal

class Session:
    def __init__(self, argv, cols=100, rows=30, cwd=None, env=None):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            if cwd: os.chdir(cwd)
            os.execvpe(argv[0], argv, {**os.environ, **(env or {})})
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.buf = b""
        self.all = b""
    def read(self, timeout=0.1):
        end = time.time() + timeout
        while True:
            r, _, _ = select.select([self.fd], [], [], max(0, end - time.time()))
            if not r: return
            try: data = os.read(self.fd, 65536)
            except OSError: return
            if not data: return
            self.buf += data
            self.all += data
    def send(self, data):
        if isinstance(data, str): data = data.encode()
        os.write(self.fd, data)
    def expect(self, pattern, timeout=30):
        rx = re.compile(pattern.encode() if isinstance(pattern, str) else pattern, re.S)
        end = time.time() + timeout
        while time.time() < end:
            m = rx.search(self.buf)
            if m:
                out = self.buf[:m.end()]; self.buf = self.buf[m.end():]; return out.decode(errors="replace")
            self.read(0.2)
        raise TimeoutError(f"no {pattern!r} in:\n{self.buf[-3000:].decode(errors='replace')}")
    def close(self):
        try: os.kill(self.pid, signal.SIGKILL); os.waitpid(self.pid, 0)
        except Exception: pass

def plain(text):
    """Text without ANSI escapes."""
    return re.sub(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07|\x1b[()][A-Z0-9]|\r", "", text)
