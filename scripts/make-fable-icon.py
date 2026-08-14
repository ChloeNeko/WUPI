#!/usr/bin/env python3
"""Render the FABLE launcher icon: a single 'F' in Uncial Antiqua (the title's
historical F font — see title.js DESIGN HISTORY), light brass with a soft glow
and a thick black outline, centered and maximally filling the canvas.

This REPLACED the wordmark-crop approach (cropping the F out of fable_title.png),
which could never cleanly separate the F from the adjacent A — the two letters
share columns in the wordmark, so any x-cut bled the A into the icon.

Re-run after changing the constants below:
    python scripts/make-fable-icon.py
"""
import sys
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
FONT_PATH = ROOT / "src-tauri" / "icons" / "_srcfont" / "UncialAntiqua.ttf"
OUT_PNG = ROOT / "src-tauri" / "icons" / "fable.png"
OUT_ICO = ROOT / "src-tauri" / "icons" / "fable.ico"

S = 1024                      # master edge length (px)
FONT_SIZE = int(S * 0.82)     # glyph height fraction of the canvas
OUTLINE = int(S * 0.026)      # thick black outline width (~2.6% of canvas)
ICO_SIZES = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]


def brass_gradient(size: int) -> Image.Image:
    """Vertical light-brass gradient: brighter at top, deeper at bottom (metallic sheen).
    Built from a 1px-wide column then scaled up — avoids a per-pixel Python loop."""
    col = bytearray()
    for y in range(size):
        t = y / size  # 0 top .. 1 bottom
        r = max(0, min(255, int(252 - 50 * t)))
        g = max(0, min(255, int(220 - 65 * t)))
        b = max(0, min(255, int(150 - 70 * t)))
        col += bytes((r, g, b, 255))
    return Image.frombytes("RGBA", (1, size), bytes(col)).resize((size, size))


def tint(alpha_mask: Image.Image, rgba) -> Image.Image:
    """A flat color layer with the given alpha mask."""
    layer = Image.new("RGBA", alpha_mask.size, rgba)
    layer.putalpha(alpha_mask)
    return layer


def main() -> int:
    if not FONT_PATH.is_file():
        sys.exit(f"missing font {FONT_PATH} (see header comment for the source)")
    font = ImageFont.truetype(str(FONT_PATH), FONT_SIZE)
    cx = cy = S / 2

    # 1. Brass fill = gradient masked to the glyph shape.
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).text((cx, cy), "F", font=font, anchor="mm", fill=255)
    fill = Image.new("RGBA", (S, S), 0)
    fill.paste(brass_gradient(S), mask=mask)

    # 2. Glow under the glyph: a wide warm outer halo + a tighter bright inner one.
    glow_outer = tint(mask.filter(ImageFilter.GaussianBlur(S * 0.035)), (255, 185, 75, 90))
    glow_inner = tint(mask.filter(ImageFilter.GaussianBlur(S * 0.012)), (255, 240, 185, 165))

    # 3. Thick black outline via Pillow's native text stroke (cheap, no morphology).
    outline_layer = Image.new("RGBA", (S, S), 0)
    ImageDraw.Draw(outline_layer).text(
        (cx, cy), "F", font=font, anchor="mm",
        fill=(0, 0, 0, 255), stroke_width=OUTLINE, stroke_fill=(0, 0, 0, 255),
    )

    # Composite: glow, then outline (borders the glyph), then brass glyph on top.
    canvas = Image.new("RGBA", (S, S), 0)
    canvas.alpha_composite(glow_outer)
    canvas.alpha_composite(glow_inner)
    canvas.alpha_composite(outline_layer)
    canvas.alpha_composite(fill)

    canvas.save(OUT_PNG)
    canvas.save(OUT_ICO, sizes=ICO_SIZES)
    print(f"wrote {OUT_PNG} ({S}x{S}) + {OUT_ICO} ({len(ICO_SIZES)} sizes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
