#!/usr/bin/env python3
"""Draws the committed raster art from the SVGs generate.py writes.

    python3 brand/render.py brand

Needs rsvg-convert (librsvg) and Pillow. Writes:
  icon/png/asagiri-app-icon-{16..1024}.png     the app icon (macOS, Linux)
  icon/png/asagiri-menubar-template@2x.png     the menu bar mark, black on clear
  icon/windows/asagiri-{16..256}.png, .ico     the Windows icon
  installer/windows/wix-dialog.bmp, wix-banner.bmp   the MSI's art, without
      a version (make-bitmaps.ps1 -Stamp writes the version at build time)
"""
import io
import os
import subprocess
import sys

from PIL import Image

OUT = sys.argv[1]
INK, INK_HI, CHALK = (0x16, 0x1B, 0x33), (0x23, 0x2A, 0x4D), (0xF3, 0xF1, 0xF7)


def draw(svg, width=None, height=None):
    """The SVG as an RGBA image, `width` or `height` pixels."""
    size = ["-w", str(width)] if width else ["-h", str(height)]
    png = subprocess.run(["rsvg-convert", *size, os.path.join(OUT, svg)], check=True, capture_output=True).stdout
    return Image.open(io.BytesIO(png)).convert("RGBA")


def save(img, path, **kw):
    full = os.path.join(OUT, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    img.save(full, **kw)
    print("wrote", full)


# App icon: each size drawn from the SVG, not scaled from a larger PNG.
for px in (16, 32, 64, 128, 256, 512, 1024):
    save(draw("icon/asagiri-app-icon.svg", width=px), f"icon/png/asagiri-app-icon-{px}.png", optimize=True)

# Menu bar: 22 x 18 points at 2x, the mark 16 points tall, centred. macOS
# colours a template image itself, so only its alpha matters.
mark = draw("logo/asagiri-mark-mono.svg", height=32)
alpha = mark.getchannel("A")
bar = Image.new("RGBA", (44, 36), (0, 0, 0, 0))
bar.paste(Image.merge("RGBA", (*Image.new("RGB", mark.size, (0, 0, 0)).split(), alpha)), ((44 - mark.width) // 2, (36 - mark.height) // 2), alpha)
save(bar, "icon/png/asagiri-menubar-template@2x.png", optimize=True)

# Windows: PNGs at the sizes it uses, and an .ico holding them.
sizes = (16, 20, 24, 32, 40, 48, 64, 256)
for px in sizes:
    save(draw("icon/asagiri-app-icon-windows.svg", width=px), f"icon/windows/asagiri-{px}.png", optimize=True)
# Each size in the .ico is its own drawing too (Pillow would otherwise scale the largest).
frames = [draw("icon/asagiri-app-icon-windows.svg", width=px) for px in sizes]
save(frames[-1], "icon/windows/asagiri.ico", sizes=[(s, s) for s in sizes], append_images=frames[:-1])

# MSI dialog, 493 x 312: a 164 px night panel with the stacked lockup, an
# empty line for the version at y = 225 under it, and a white field for the
# installer's own text.
dialog = Image.new("RGB", (493, 312), (255, 255, 255))
panel = Image.new("RGB", (164, 312))
for y in range(312):
    t = y / 311
    panel.paste(tuple(round(a + (b - a) * t) for a, b in zip(INK_HI, INK)), (0, y, 164, y + 1))
lockup = draw("logo/asagiri-stacked-dark.svg", width=112)
panel.paste(lockup, ((164 - lockup.width) // 2, 225 - 18 - lockup.height), lockup)
dialog.paste(panel, (0, 0))
save(dialog, "installer/windows/wix-dialog.bmp")

# MSI banner, 493 x 58: mist, with the horizontal lockup at the right (the
# installer draws its title in black at the left, so the strip stays light).
banner = Image.new("RGB", (493, 58), CHALK)
lockup = draw("logo/asagiri-horizontal-light.svg", height=26)
banner.paste(lockup, (493 - 14 - lockup.width, 16), lockup)
save(banner, "installer/windows/wix-banner.bmp")
