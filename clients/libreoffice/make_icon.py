"""Writes extension/icons/deck.png: the sidebar deck's icon (a small bar
chart), drawn with the standard library only. Run it after changing it."""

import os
import struct
import zlib

SIZE = 24
BLUE = (0x38, 0x84, 0xFF, 255)
DARK = (0x1A, 0x4F, 0xA0, 255)


def pixels():
    img = [[(0, 0, 0, 0)] * SIZE for _ in range(SIZE)]
    # Three bars of a chart on a base line, like a cube's values.
    for x0, height, color in ((4, 9, BLUE), (10, 15, DARK), (16, 12, BLUE)):
        for x in range(x0, x0 + 4):
            for y in range(SIZE - 3 - height, SIZE - 3):
                img[y][x] = color
    for x in range(2, SIZE - 2):
        img[SIZE - 3][x] = DARK
    return img


def png(img):
    raw = b"".join(b"\x00" + b"".join(bytes(p) for p in row) for row in img)

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0)  # 8-bit RGBA
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


if __name__ == "__main__":
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "extension", "icons", "deck.png")
    with open(out, "wb") as f:
        f.write(png(pixels()))
    print(f"wrote {out}")
