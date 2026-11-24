#!/usr/bin/env python3
"""Generate synthetic webtoon-style test chapters (needs Pillow).

Usage: tools/gen_test_strips.py [OUTPUT_DIR]   (default: ~/Manga/TestSeri)

Every page has a ruler every 100 px with its absolute y position, a page
label and colored bands, so scroll jumps, seams between tiles and wrong
heights are easy to spot by eye. Widths and formats are mixed on purpose.
"""
import os
import random
import sys

from PIL import Image, ImageDraw, ImageFont

out = os.path.expanduser(sys.argv[1] if len(sys.argv) > 1 else "~/Manga/TestSeri")
rng = random.Random(7)
try:
    font = ImageFont.load_default(size=28)
    big = ImageFont.load_default(size=64)
except TypeError:  # old Pillow
    font = big = ImageFont.load_default()

# (chapter, [(width, height, format)])
CHAPTERS = {
    "Chapter 001": [(800, 12000, "jpg")] + [(800, rng.randint(1800, 4200), "jpg") for _ in range(7)],
    "Chapter 002": [(720, 3000, "png"), (1000, 6000, "jpg"), (800, 20000, "jpg")]
    + [(800, rng.randint(2000, 5000), "webp") for _ in range(5)],
    "Chapter 003": [(900, rng.randint(1500, 3500), "jpg") for _ in range(10)],
}


def make(path, w, h, label):
    hue = rng.random()
    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    band = 400
    for i, y in enumerate(range(0, h, band)):
        shade = 200 + (i * 17) % 55
        col = tuple(int(c * shade) for c in __import__("colorsys").hsv_to_rgb((hue + i * 0.07) % 1, 0.35, 1))
        d.rectangle([0, y, w, min(y + band, h)], fill=col)
    for y in range(0, h, 100):
        major = y % 500 == 0
        d.line([0, y, w if major else w // 5, y], fill=(0, 0, 0), width=3 if major else 1)
        d.text((w // 5 + 8 if not major else 12, y + 4), f"y={y}", fill=(0, 0, 0), font=font)
    d.text((w // 2 - 200, 60), label, fill=(20, 20, 20), font=big)
    d.rectangle([0, 0, w - 1, h - 1], outline=(255, 0, 0), width=4)
    img.save(path, quality=85)


for chap, pages in CHAPTERS.items():
    os.makedirs(os.path.join(out, chap), exist_ok=True)
    for n, (w, h, fmt) in enumerate(pages, 1):
        make(os.path.join(out, chap, f"{n:03}.{fmt}"), w, h, f"{chap} / page {n}")
print("wrote test chapters to", out)
