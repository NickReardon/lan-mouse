#!/usr/bin/env python3
"""Generate the committed app icon assets from the Lan Mouse SVG.

Outputs (paths relative to the repository root):
  build-aux/windows/lan-mouse.ico
      Embedded in lan-mouse.exe. Windows uses it for Explorer, the taskbar,
      the title bar and the notification-area (tray) icon.
  lan-mouse-gtk/resources/macos/app-icon.png
      macOS Dock icon while the window is open. Set at runtime so the Dock
      shows it even when the binary is run outside an .app bundle.
  lan-mouse-gtk/resources/macos/menubar-template.png
      macOS menu bar template image (black glyph with alpha; macOS tints it).

The artwork is a black glyph on transparency, which disappears on dark
taskbars and Docks, so app icons put it on a white rounded square.

Requires rsvg-convert on PATH and Pillow (pip install pillow).
Run from the repository root: python scripts/make-app-icons.py
"""

import io
import os
import shutil
import subprocess
import sys

from PIL import Image, ImageDraw

SVG = "lan-mouse-gtk/resources/de.feschber.LanMouse.svg"
ICO = "build-aux/windows/lan-mouse.ico"
DOCK_PNG = "lan-mouse-gtk/resources/macos/app-icon.png"
MENUBAR_PNG = "lan-mouse-gtk/resources/macos/menubar-template.png"

WINDOWS_ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
DOCK_SIZE = 512
# 22pt menu bar height at 2x; the glyph keeps a small margin inside it.
MENUBAR_SIZE = 44
MENUBAR_GLYPH = 38
# Rounded-square backgrounds are drawn larger and downscaled for antialiasing.
SUPERSAMPLE = 4


def render_svg(size):
    """Render the artwork trimmed to its visible bounds, fit into a square."""
    glyph = rasterize_svg(1024)
    glyph = glyph.crop(glyph.getbbox())
    scale = size / max(glyph.size)
    glyph = glyph.resize(
        (max(1, round(glyph.width * scale)), max(1, round(glyph.height * scale))),
        Image.LANCZOS,
    )
    square = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    square.alpha_composite(glyph, ((size - glyph.width) // 2, (size - glyph.height) // 2))
    return square


def rasterize_svg(size):
    rsvg = shutil.which("rsvg-convert")
    if rsvg is None:
        sys.exit("rsvg-convert not found on PATH")
    png = subprocess.run(
        [rsvg, "-w", str(size), "-h", str(size), SVG],
        check=True,
        capture_output=True,
    ).stdout
    return Image.open(io.BytesIO(png)).convert("RGBA")


def rounded_square(canvas, inset, radius):
    big = canvas * SUPERSAMPLE
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (
            inset * SUPERSAMPLE,
            inset * SUPERSAMPLE,
            big - inset * SUPERSAMPLE - 1,
            big - inset * SUPERSAMPLE - 1,
        ),
        radius=radius * SUPERSAMPLE,
        fill=255,
    )
    background = Image.new("RGBA", (canvas, canvas), (255, 255, 255, 0))
    background.putalpha(mask.resize((canvas, canvas), Image.LANCZOS))
    return background


def app_icon(canvas, inset, radius, content, trim=True):
    icon = rounded_square(canvas, inset, radius)
    glyph = render_svg(content) if trim else rasterize_svg(content)
    offset = (canvas - content) // 2
    icon.alpha_composite(glyph, (offset, offset))
    return icon


def windows_icon(size):
    # Windows icons fill their canvas; keep only a hairline inset.
    inset = max(0, round(size / 64))
    return app_icon(size, inset, round(size * 0.22), round(size * 0.80))


def dock_icon():
    # Same Big Sur+ layout as the bundle icon from scripts/makeicns.sh, so
    # the Dock and Finder agree: on a 1024 canvas, an 824 squircle inset 100
    # with radius 185 and the untrimmed artwork at 560.
    scale = DOCK_SIZE / 1024
    return app_icon(
        DOCK_SIZE, round(100 * scale), round(185 * scale), round(560 * scale), trim=False
    )


def menubar_template():
    # Template images use only alpha. Map dark artwork to opaque and light
    # details (the eye and ear highlights) to transparent so they stay
    # visible after macOS tints the glyph.
    glyph = Image.new("RGBA", (MENUBAR_SIZE, MENUBAR_SIZE), (0, 0, 0, 0))
    offset = (MENUBAR_SIZE - MENUBAR_GLYPH) // 2
    glyph.alpha_composite(render_svg(MENUBAR_GLYPH), (offset, offset))
    pixels = []
    for r, g, b, a in glyph.getdata():
        luminance = (299 * r + 587 * g + 114 * b) / 1000 / 255
        pixels.append((0, 0, 0, round(a * (1 - luminance))))
    glyph.putdata(pixels)
    return glyph


def main():
    if not os.path.exists(SVG):
        sys.exit(f"{SVG} not found; run from the repository root")
    for path in (ICO, DOCK_PNG, MENUBAR_PNG):
        os.makedirs(os.path.dirname(path), exist_ok=True)

    icons = [windows_icon(size) for size in WINDOWS_ICO_SIZES]
    # Pillow writes the largest image it is given and downsamples the rest
    # unless every size is supplied through append_images.
    icons[-1].save(
        ICO,
        format="ICO",
        sizes=[(s, s) for s in WINDOWS_ICO_SIZES],
        append_images=icons[:-1],
    )
    dock_icon().save(DOCK_PNG, optimize=True)
    menubar_template().save(MENUBAR_PNG, optimize=True)
    for path in (ICO, DOCK_PNG, MENUBAR_PNG):
        print(f"wrote {path} ({os.path.getsize(path)} bytes)")


if __name__ == "__main__":
    main()
