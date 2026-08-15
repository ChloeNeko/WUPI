#!/usr/bin/env python3
"""Render the FABLE launcher icon: a single 'F' in Uncial Antiqua (the title's
historical F font — see title.js DESIGN HISTORY), light brass with a soft glow
and a thick black outline, ink-centered and as large as the canvas allows.

This REPLACED the wordmark-crop approach (cropping the F out of fable_title.png),
which could never cleanly separate the F from the adjacent A — the two letters
share columns in the wordmark, so any x-cut bled the A into the icon.

LAYOUT (fixed 2026-08-14 — the "bottom glow is cut" bug): the first version
drew the glyph at a fixed 0.82*S size with Pillow's "mm" anchor, which centers
the FONT'S EM BOX, not the ink. The Uncial F's descender tail makes its ink
sit far below the em center, so the ink ran to the canvas's last row (fully
opaque bottom edge — a hard clip, glow and all). Now the script measures the
glyph's true ink bbox (font.getbbox — metric-accurate, never clipped), picks
the LARGEST size whose ink + outline + glow-reach fits the canvas on both
axes, and offsets the draw point so the INK bbox is what's centered. The F is
marginally smaller than the clipped original but complete: full outline, full
glow tail, symmetric margins on every side.

Also writes src/fable_icon.png (the #fable-entry-splash <img> asset, hashed
into assets/ by Vite) so the exe icon + splash can never drift apart.

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
OUT_SPLASH = ROOT / "src" / "fable_icon.png"  # the splash <img> (wupi.html)

S = 1024                      # master edge length (px)
OUTLINE = int(S * 0.026)      # thick black outline width (~2.6% of canvas)
# Per-side room reserved for the outer glow's tail: sigma_outer = 0.035*S, so
# this is ~2.8 sigma — beyond it the halo is under 2% of its peak alpha and
# invisible. Reserving less re-introduces a visible cut; more shrinks the F.
GLOW_REACH = int(S * 0.10)
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


def fit_font(path: Path) -> ImageFont.FreeTypeFont:
    """The largest font size whose ink bbox + outline + glow-reach fits S on
    both axes. Font metrics scale linearly with size, so one trial measurement
    suffices. getbbox is metric-based (never clipped by a canvas), so the
    descender tail is fully accounted for."""
    TRIAL = 256
    trial = ImageFont.truetype(str(path), TRIAL)
    x0, y0, x1, y1 = trial.getbbox("F", anchor="mm")
    avail = S - 2 * (OUTLINE + GLOW_REACH)
    scale = min(avail / (x1 - x0), avail / (y1 - y0))
    return ImageFont.truetype(str(path), int(TRIAL * scale))


def main() -> int:
    if not FONT_PATH.is_file():
        sys.exit(f"missing font {FONT_PATH} (see header comment for the source)")
    font = fit_font(FONT_PATH)
    cx = cy = S / 2

    # Offset the "mm" anchor so the INK bbox is what centers in the canvas
    # (the em box sits ~150px above the ink's optical center in this font —
    # anchoring on it is what clipped the descender against the bottom edge).
    x0, y0, x1, y1 = font.getbbox("F", anchor="mm")
    px, py = cx - (x0 + x1) / 2, cy - (y0 + y1) / 2

    # 1. Brass fill = gradient masked to the glyph shape.
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).text((px, py), "F", font=font, anchor="mm", fill=255)
    fill = Image.new("RGBA", (S, S), 0)
    fill.paste(brass_gradient(S), mask=mask)

    # 2. Glow under the glyph: a wide warm outer halo + a tighter bright inner one.
    glow_outer = tint(mask.filter(ImageFilter.GaussianBlur(S * 0.035)), (255, 185, 75, 90))
    glow_inner = tint(mask.filter(ImageFilter.GaussianBlur(S * 0.012)), (255, 240, 185, 165))

    # 3. Thick black outline via Pillow's native text stroke (cheap, no morphology).
    outline_layer = Image.new("RGBA", (S, S), 0)
    ImageDraw.Draw(outline_layer).text(
        (px, py), "F", font=font, anchor="mm",
        fill=(0, 0, 0, 255), stroke_width=OUTLINE, stroke_fill=(0, 0, 0, 255),
    )

    # Composite: glow, then outline (borders the glyph), then brass glyph on top.
    canvas = Image.new("RGBA", (S, S), 0)
    canvas.alpha_composite(glow_outer)
    canvas.alpha_composite(glow_inner)
    canvas.alpha_composite(outline_layer)
    canvas.alpha_composite(fill)

    # Safety net: if any edge row/column still holds ink, the fit math drifted
    # (font update, constant change) — fail loudly instead of shipping a clip.
    edge = max(
        max(canvas.getpixel((x, 0))[3] for x in range(S)),
        max(canvas.getpixel((x, S - 1))[3] for x in range(S)),
        max(canvas.getpixel((0, y))[3] for y in range(S)),
        max(canvas.getpixel((S - 1, y))[3] for y in range(S)),
    )
    if edge > 0:
        sys.exit(f"icon ink reaches the canvas edge (max edge alpha {edge}) — refusing to write a clipped icon")

    canvas.save(OUT_PNG)
    canvas.save(OUT_ICO, sizes=ICO_SIZES)
    canvas.save(OUT_SPLASH)
    print(f"wrote {OUT_PNG} ({S}x{S}) + {OUT_ICO} ({len(ICO_SIZES)} sizes) + {OUT_SPLASH} (splash)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
