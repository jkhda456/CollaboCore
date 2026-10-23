#!/usr/bin/env python3
"""Make the logo of the `version` command (rootfs/version) and write it into that script, between
its `# logo:` and `# logo end` lines: a 24-bit BMP beside the word CollaboCore in large letters,
as terminal text. Two pixel rows per line, as half blocks (U+2580, the top pixel its foreground,
the bottom one its background) in 24-bit colour, `\\e` for ESC; the letters in the colours of the
logo, on the terminal's own background. A 32x32 image is 32 columns by 16 lines, the letters 45
columns beside it (79 in all). Run it again when the image changes:

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
WORD = "CollaboCore"
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


def image_lines(rows):
    if len(rows) % 2:
        rows = rows + [None]
    lines = []
    for top, bottom in zip(rows[0::2], rows[1::2]):
        line, last = [], None
        for x, fg in enumerate(top):
            bg = bottom[x] if bottom else None
            if (fg, bg) != last:
                code = "38;2;%d;%d;%d" % fg
                code += ";48;2;%d;%d;%d" % bg if bg else ";49"
                line.append(f"\\e[{code}m")
                last = (fg, bg)
            line.append("▀")
        lines.append("".join(line) + "\\e[0m")
    return lines


def mix(x, width):
    t = x / max(width - 1, 1) * (len(GRADIENT) - 1)
    i = min(int(t), len(GRADIENT) - 2)
    a, b = GRADIENT[i], GRADIENT[i + 1]
    return tuple(round(a[c] + (b[c] - a[c]) * (t - i)) for c in range(3))


def word_lines():
    pixels = ["" for _ in range(7)]
    for i, letter in enumerate(WORD):
        for y in range(7):
            pixels[y] += ("." if i else "") + GLYPHS[letter][y]
    pixels.append("." * len(pixels[0]))
    width = len(pixels[0])
    lines = []
    for top, bottom in zip(pixels[0::2], pixels[1::2]):
        line = []
        for x in range(width):
            cell = " ▄▀█"[(top[x] == "#") * 2 + (bottom[x] == "#")]
            if cell != " ":
                line.append("\\e[38;2;%d;%d;%dm" % mix(x, width))  # each column its own colour
            line.append(cell)
        lines.append("".join(line).rstrip() + "\\e[0m")
    return lines


def block(bmp):
    image = image_lines(read_bmp(bmp))
    word = word_lines()
    # The word and the lines `version` adds (four) in the middle of the image's height, a line
    # apart.
    top = (len(image) - (len(word) + 1 + 4)) // 2
    lines = []
    for y, line in enumerate(image):
        if top <= y < top + len(word):
            line += GAP + word[y - top]
        lines.append(line)
    # A quoted string, not a function with a here-document: the guest's ash fails to run such a
    # function in a pipeline ("unexpected EOF in here document").
    lines[0] = "logo='" + lines[0]
    lines[-1] += "'"
    return [
        "# logo: the image and the word, by userspace/logo.py from CollaboCore32.bmp (run it again,",
        "# not an editor); \\e is ESC. The lines from logo_info on get the info beside them.",
        f"logo_info={top + len(word) + 2}",
        *lines,
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
