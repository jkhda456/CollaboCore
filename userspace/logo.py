#!/usr/bin/env python3
"""Make the logo of the `version` command (rootfs/version) and write it into that script, between
its `# logo:` and `# logo end` lines: a 24-bit BMP beside the word CollaboCore in large letters,
as terminal text. Two pixel rows per line, as lower half blocks (U+2584) in 24-bit colour, `\\e`
for ESC; the letters in the colours of the logo, on the terminal's own background. A 32x32 image
is 32 columns by 16 lines, the letters 45 columns beside it (79 in all).

Every cell's background is its top pixel and the ▄ its bottom one (`half_cell`): macOS's terminals
draw block characters from the font, short of the cell height, and the strip left over (at the
top of the cell there) shows the cell's background. So it continues the top pixel instead of
leaving a line of the terminal's background between every two pixel rows. A cell whose bottom
pixel is the terminal's own background is a reversed ▄ (SGR 7: the ▄ in that background on the
top pixel's colour); one pixel in both halves, a space. Where a terminal draws the blocks itself
(most do) this looks the same as any other way of using half blocks.

The ▄ is an East Asian "ambiguous" character, which a terminal set to draw those wide (an option
of Terminal.app and iTerm2, often on in Korean, Chinese and Japanese setups) gives two columns:
every line then wraps half way. So the script also carries the logo in coloured spaces only
(`logo_cells`), which `version` draws when the terminal says a ▄ took two columns. Each column of
the image one cell, the two pixels of a cell averaged; the letters in their own font at cell
resolution (`CELL_GLYPHS`), as high as in half blocks. Same 79 columns, 16 lines (`version`
checks the width: `logo_width`). Run it again when the image changes:

    python3 userspace/logo.py CollaboCore32.bmp userspace/rootfs/version
"""
import argparse
import re
import struct
import sys

# The letters, 7 pixels high (lower case 5). "#" is a pixel.
GLYPHS = {
    "C": [".##", "#..", "#..", "#..", "#..", "#..", ".##"],
    "o": ["....", "....", ".##.", "#..#", "#..#", "#..#", ".##."],
    "l": ["#"] * 7,
    "a": ["....", "....", ".##.", "...#", ".###", "#..#", ".###"],
    "b": ["#...", "#...", "###.", "#..#", "#..#", "#..#", "###."],
    "r": ["...", "...", "#.#", "##.", "#..", "#..", "#.."],
    "e": ["....", "....", ".##.", "#..#", "####", "#...", ".###"],
}
# The same letters for `logo_cells`, where a pixel is a whole cell, about twice as high as wide:
# GLYPHS there would stand twice as high as in half blocks. So 4 cells high (lower case 3), the
# 4 lines of the half-block letters, and as wide. A horizontal stroke is a cell row, twice as
# thick as a vertical one, and 3 rows leave the e no room for its eye: a round E.
CELL_GLYPHS = {
    "C": [".##", "#..", "#..", ".##"],
    "o": ["....", ".##.", "#..#", ".##."],
    "l": ["#"] * 4,
    "a": ["....", ".###", "#..#", ".###"],
    "b": ["#...", "###.", "#..#", "###."],
    "r": ["...", ".##", "#..", "#.."],
    "e": ["....", ".###", "##..", ".###"],
}
WORD = "CollaboCore"
INFO_LINES = 5  # the lines `version` adds under the word (more where it wraps them)
# Left to right across the word: the logo's pink, purple and blue.
GRADIENT = [(0xE4, 0x81, 0xE0), (0x8A, 0x5C, 0xD0), (0x3B, 0x9E, 0xDA)]
GAP = "  "  # between the image and the letters, and the letters and the lines `version` adds


def read_bmp(path):
    data = open(path, "rb").read()
    if data[:2] != b"BM":
        sys.exit(f"{path}: not a BMP")
    offset = struct.unpack_from("<I", data, 10)[0]
    width, height = struct.unpack_from("<ii", data, 18)
    bpp, compression = struct.unpack_from("<HI", data, 28)
    if bpp != 24 or compression != 0:
        sys.exit(f"{path}: {bpp} bits per pixel, compression {compression}; expected 24, none")
    stride = (width * 3 + 3) // 4 * 4
    # Rows are stored bottom-up unless the height is negative; pixels as B, G, R.
    rows = []
    for y in range(abs(height)):
        start = offset + (abs(height) - 1 - y if height > 0 else y) * stride
        rows.append([tuple(data[start + x * 3:start + x * 3 + 3][::-1]) for x in range(width)])
    return rows


def half_cell(top, bottom):
    """A cell of two pixels, each a colour or None (the terminal's background): (character,
    reversed, foreground, background), the background the top pixel's (the module's doc)."""
    if top == bottom:
        return (" ", False, None, top)
    if bottom is None:
        return ("▄", True, top, None)  # reversed: the top pixel the background, the ▄ the terminal's
    return ("▄", False, bottom, top)


def half_line(cells):
    """Cells of half_cell as text, each attribute set where it changes, the terminal's background
    cells at the end left out."""
    while cells and cells[-1] == (" ", False, None, None):
        cells.pop()
    line, (rev, fg, bg) = [], (False, None, None)
    for ch, r, f, b in cells:
        codes = []
        if r != rev:
            codes.append("7" if r else "27")
        if f != fg and ch != " ":  # a space shows no foreground: leave it for the next ▄
            codes.append("38;2;%d;%d;%d" % f if f else "39")
            fg = f
        if b != bg:
            codes.append("48;2;%d;%d;%d" % b if b else "49")
        if codes:
            line.append("\\e[%sm" % ";".join(codes))
        rev, bg = r, b
        line.append(ch)
    return "".join(line) + "\\e[0m"


def image_lines(rows):
    if len(rows) % 2:
        rows = rows + [[None] * len(rows[0])]
    return [half_line([half_cell(t, b) for t, b in zip(top, bottom)]) for top, bottom in zip(rows[0::2], rows[1::2])]


def image_cells(rows):
    """The image in background colours: a cell per column, two pixel rows averaged in it."""
    if len(rows) % 2:
        rows = rows + [rows[-1]]
    lines = []
    for top, bottom in zip(rows[0::2], rows[1::2]):
        line, last = [], None
        for a, b in zip(top, bottom):
            colour = tuple((a[c] + b[c] + 1) // 2 for c in range(3))
            if colour != last:
                line.append("\\e[48;2;%d;%d;%dm" % colour)
                last = colour
            line.append(" ")
        lines.append("".join(line) + "\\e[0m")
    return lines


def mix(x, width):
    t = x / max(width - 1, 1) * (len(GRADIENT) - 1)
    i = min(int(t), len(GRADIENT) - 2)
    a, b = GRADIENT[i], GRADIENT[i + 1]
    return tuple(round(a[c] + (b[c] - a[c]) * (t - i)) for c in range(3))


def word_pixels(glyphs):
    """The word's rows of pixels ("#"), a column apart between letters."""
    pixels = ["" for _ in glyphs["C"]]
    for i, letter in enumerate(WORD):
        for y in range(len(pixels)):
            pixels[y] += ("." if i else "") + glyphs[letter][y]
    return pixels


def word_lines():
    pixels = word_pixels(GLYPHS)
    pixels.append("." * len(pixels[0]))
    width = len(pixels[0])
    lines = []
    for top, bottom in zip(pixels[0::2], pixels[1::2]):
        # each column its own colour
        colour = lambda row, x: mix(x, width) if row[x] == "#" else None
        lines.append(half_line([half_cell(colour(top, x), colour(bottom, x)) for x in range(width)]))
    return lines


def word_cells():
    """The word in background colours, a cell per pixel of CELL_GLYPHS."""
    pixels = word_pixels(CELL_GLYPHS)
    width = len(pixels[0])
    lines = []
    for row in pixels:
        line, on = [], False
        for x, p in enumerate(row):
            if p == "#":
                line.append("\\e[48;2;%d;%d;%dm" % mix(x, width))
                on = True
            elif on:
                line.append("\\e[49m")
                on = False
            line.append(" ")
        lines.append("".join(line) + "\\e[0m")
    return lines


def columns(line):
    """How many columns a line takes: every character here is one (\\e[...m none)."""
    return len(re.sub(r"\\e\[[0-9;]*m", "", line))


def layout(name, image, word):
    """`name='...'` with the word beside the image, and where the lines `version` adds go."""
    # The word and the lines `version` adds in the middle of the image's height, a line apart.
    top = (len(image) - (len(word) + 1 + INFO_LINES)) // 2
    lines = []
    for y, line in enumerate(image):
        if top <= y < top + len(word):
            line += GAP + word[y - top]
        lines.append(line)
    # A quoted string, not a function with a here-document: the guest's ash fails to run such a
    # function in a pipeline ("unexpected EOF in here document").
    lines[0] = f"{name}='" + lines[0]
    lines[-1] += "'"
    return [f"{name}_info={top + len(word) + 2}", *lines]


def block(bmp):
    rows = read_bmp(bmp)
    variants = [(image_lines(rows), word_lines()), (image_cells(rows), word_cells())]
    # Both the same size, so `version` checks one width: the image, the gap, the word.
    sizes = {(columns(image[0]), max(map(columns, word))) for image, word in variants}
    if len(sizes) != 1:
        sys.exit(f"the two logos differ in size (image, word columns): {sorted(sizes)}")
    image_width, word_width = sizes.pop()
    left = image_width + len(GAP)
    return [
        "# logo: the image and the word, by userspace/logo.py from CollaboCore32.bmp (run it again,",
        "# not an editor); \\e is ESC. In half blocks, each cell's background its top pixel (a ▄",
        "# short of the cell leaves no gap); logo_cells in coloured spaces, for terminals that draw",
        "# the ▄ two wide. Both logo_width columns; the lines from logo_info on are the image alone,",
        "# the info logo_left columns in beside them.",
        f"logo_width={left + word_width} logo_left={left}",
        *layout("logo", *variants[0]),
        *layout("logo_cells", *variants[1]),
        "# logo end",
    ]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("bmp")
    ap.add_argument("script", help="rootfs/version: its # logo: ... # logo end lines are replaced")
    args = ap.parse_args()
    text = open(args.script, encoding="utf-8").read()
    new, count = re.subn(r"^# logo:.*?^# logo end\n", lambda _: "\n".join(block(args.bmp)) + "\n",
                         text, flags=re.M | re.S)
    if count != 1:
        sys.exit(f"{args.script}: no '# logo:' ... '# logo end' block")
    with open(args.script, "w", encoding="utf-8") as out:
        out.write(new)


if __name__ == "__main__":
    main()
