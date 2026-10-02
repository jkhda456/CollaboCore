"""A small terminal screen model, enough to check what a full-screen program drew: cursor moves,
erase, autowrap, and wide (CJK, 2 columns) and combining characters as xterm places them.

    s = vtscreen.Screen(rows, cols); s.feed(bytes); s.line(n); s.cursor
"""
import codecs, re, unicodedata

def width(ch):
    if unicodedata.combining(ch) or unicodedata.category(ch) in ("Mn", "Me", "Cf") or "ᅠ" <= ch <= "ᇿ":
        return 0
    return 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1

class Screen:
    CSI = re.compile(r"\x1b\[([0-9;?]*)([@-~])")

    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]   # "" = right half of the wide char before it
        self.y = self.x = 0
        self.wrap = False      # cursor sits past the last column (xterm's pending wrap)
        self.dec = codecs.getincrementaldecoder("utf-8")("replace")
        self.pending = ""

    @property
    def cursor(self):
        return (self.y, min(self.x, self.cols - 1))

    def line(self, y):
        return "".join(self.grid[y]).rstrip()

    def text(self):
        return "\n".join(self.line(y) for y in range(self.rows))

    def feed(self, data):
        s = self.pending + self.dec.decode(data)
        self.pending = ""
        i = 0
        while i < len(s):
            ch = s[i]
            if ch == "\x1b":
                m = self.CSI.match(s, i)
                if m:
                    self.csi(m.group(1), m.group(2)); i = m.end(); continue
                if i + 1 >= len(s) or (s[i + 1] == "[" and not re.match(r"\x1b\[[0-9;?]*[@-~]", s[i:])):
                    self.pending = s[i:]; return        # an escape cut in two by a read
                i += 2 if s[i + 1] in "()78=>" else 1
                if s[i - 1] in "()": i += 1
                continue
            if ch == "\r": self.x = 0; self.wrap = False
            elif ch == "\n": self.lf()
            elif ch == "\b": self.x = max(0, min(self.x, self.cols - 1) - 1); self.wrap = False
            elif ch < " " or ch == "\x7f": pass
            else: self.put(ch)
            i += 1

    def lf(self):
        if self.y == self.rows - 1:
            self.grid.pop(0); self.grid.append([" "] * self.cols)
        else:
            self.y += 1
        self.wrap = False

    def clear_cell(self, y, x):
        """Blank (y, x), and the other half if it is half of a wide char."""
        row = self.grid[y]
        if row[x] == "" and x > 0: row[x - 1] = " "
        if x + 1 < self.cols and row[x + 1] == "": row[x + 1] = " "
        row[x] = " "

    def put(self, ch):
        w = width(ch)
        if w == 0:
            x = self.x - 1 if not self.wrap else self.cols - 1
            if x >= 0:
                if self.grid[self.y][x] == "" and x > 0: x -= 1
                self.grid[self.y][x] += ch
            return
        if self.wrap or self.x + w > self.cols:
            if self.x + w > self.cols and not self.wrap:     # a wide char does not fit: xterm wraps it
                for x in range(self.x, self.cols): self.clear_cell(self.y, x)
            self.x = 0; self.lf()
        self.clear_cell(self.y, self.x)
        if w == 2: self.clear_cell(self.y, self.x + 1)
        self.grid[self.y][self.x] = ch
        if w == 2: self.grid[self.y][self.x + 1] = ""
        self.x += w
        if self.x >= self.cols: self.x = self.cols - 1; self.wrap = True

    def csi(self, params, final):
        priv = params.startswith("?")
        p = [int(v) if v else 0 for v in params.lstrip("?").split(";")] if params.lstrip("?") else []
        n = (p[0] if p else 0) or 1
        self.wrap = False
        if priv: return
        if final in "Hf":
            self.y = min(max((p[0] if p else 1) or 1, 1), self.rows) - 1
            self.x = min(max((p[1] if len(p) > 1 else 1) or 1, 1), self.cols) - 1
        elif final == "A": self.y = max(0, self.y - n)
        elif final == "B": self.y = min(self.rows - 1, self.y + n)
        elif final == "C": self.x = min(self.cols - 1, self.x + n)
        elif final == "D": self.x = max(0, self.x - n)
        elif final == "K":
            mode = p[0] if p else 0
            rng = range(self.x, self.cols) if mode == 0 else range(0, self.x + 1) if mode == 1 else range(self.cols)
            for x in rng: self.clear_cell(self.y, x)
        elif final == "J":
            mode = p[0] if p else 0
            if mode == 0:
                for x in range(self.x, self.cols): self.clear_cell(self.y, x)
                for y in range(self.y + 1, self.rows): self.grid[y] = [" "] * self.cols
            elif mode == 2:
                self.grid = [[" "] * self.cols for _ in range(self.rows)]
