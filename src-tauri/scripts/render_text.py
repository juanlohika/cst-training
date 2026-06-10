#!/usr/bin/env python3
"""
Text rendering helper for CST Studio's video pipeline.

This script renders text to PNG images (with alpha channel) that the
Rust core then composites into video via ffmpeg overlay. We use Pillow
rather than ffmpeg's drawtext for two reasons:

  1. Homebrew's stock ffmpeg ships without freetype/libfreetype, so
     drawtext is unavailable on developer Macs.
  2. Pillow gives us proper word-wrapping, multi-line layout, and
     cross-platform font resolution — important for Phase 2 multilingual
     support (Tagalog, Korean, etc.) and for Windows compatibility.

Three commands, all driven by JSON on stdin so Rust doesn't have to
escape shell args:

    {"mode": "title_card", ...}    → write a full-screen title-card PNG
    {"mode": "caption", ...}        → write a transparent caption PNG
                                      (alpha-only, to be overlaid on frames)

The Rust core writes one of these JSONs to a temp file and runs:
    python3 render_text.py <json_path> <output_png_path>
"""

import json
import os
import sys
from PIL import Image, ImageDraw, ImageFont, ImageFilter, ImageColor


# Font resolution: try preferred fonts in order, fall back to a known
# default that Pillow can always find. Designed to work on Mac AND
# Windows — see lib.rs Project::language comment for why this matters.
PREFERRED_FONTS = [
    # macOS modern
    "/System/Library/Fonts/Helvetica.ttc",
    "/System/Library/Fonts/SFNS.ttf",
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    # Windows
    "C:\\Windows\\Fonts\\Arial.ttf",
    "C:\\Windows\\Fonts\\segoeui.ttf",
    # Linux / Pillow-bundled
    "DejaVuSans.ttf",
]


def find_font(size, bold=False):
    """Return an ImageFont, trying preferred system fonts before falling
    back to Pillow's default. Bold variant uses index 1 of Helvetica.ttc
    on Mac or the regular Arial Bold on Windows."""
    if bold:
        # Try the macOS bold variant first.
        try:
            return ImageFont.truetype("/System/Library/Fonts/Helvetica.ttc", size, index=1)
        except (IOError, OSError):
            pass
        try:
            return ImageFont.truetype("C:\\Windows\\Fonts\\arialbd.ttf", size)
        except (IOError, OSError):
            pass

    for path in PREFERRED_FONTS:
        try:
            return ImageFont.truetype(path, size)
        except (IOError, OSError):
            continue
    return ImageFont.load_default()


def wrap_text(draw, text, font, max_width):
    """Wrap text to fit within max_width pixels. Returns list of lines."""
    if not text:
        return []
    words = text.split()
    lines = []
    current = []
    for word in words:
        candidate = (" ".join(current + [word])).strip()
        bbox = draw.textbbox((0, 0), candidate, font=font)
        if bbox[2] - bbox[0] <= max_width or not current:
            current.append(word)
        else:
            lines.append(" ".join(current))
            current = [word]
    if current:
        lines.append(" ".join(current))
    return lines


def render_title_card(spec):
    """spec = {
        main_text: str, subtitle: str (optional), width: int,
        height: int, bg_color: str (e.g. '#044be4'),
        text_color: str (e.g. '#ffffff'),
    }
    Returns a PIL.Image RGBA.
    """
    w = spec["width"]
    h = spec["height"]
    bg = ImageColor.getrgb(spec.get("bg_color", "#044be4")) + (255,)
    fg = ImageColor.getrgb(spec.get("text_color", "#ffffff")) + (255,)

    img = Image.new("RGBA", (w, h), bg)
    draw = ImageDraw.Draw(img)

    # Font sizes scale with the short edge — works for both vertical and
    # landscape canvases.
    short_edge = min(w, h)
    main_size = int(short_edge * 0.06)
    sub_size = int(short_edge * 0.025)

    main_font = find_font(main_size, bold=True)
    sub_font = find_font(sub_size, bold=False)

    # Allow text to use 80% of the canvas width.
    max_text_w = int(w * 0.8)

    main_lines = wrap_text(draw, spec["main_text"], main_font, max_text_w)

    sub_text = spec.get("subtitle", "").strip()
    sub_lines = wrap_text(draw, sub_text, sub_font, max_text_w) if sub_text else []

    # Measure block heights.
    line_h_main = int(main_size * 1.25)
    line_h_sub = int(sub_size * 1.4)
    main_block_h = line_h_main * len(main_lines)
    sub_block_h = line_h_sub * len(sub_lines)
    gap = int(short_edge * 0.02) if sub_lines else 0
    total_h = sub_block_h + gap + main_block_h
    cursor_y = (h - total_h) // 2

    # Subtitle (lighter weight, 85% alpha).
    sub_fg = fg[:3] + (217,)  # 85% of 255 ≈ 217
    for line in sub_lines:
        bbox = draw.textbbox((0, 0), line, font=sub_font)
        line_w = bbox[2] - bbox[0]
        draw.text(((w - line_w) // 2, cursor_y), line, font=sub_font, fill=sub_fg)
        cursor_y += line_h_sub
    if sub_lines:
        cursor_y += gap

    # Main title (full weight).
    for line in main_lines:
        bbox = draw.textbbox((0, 0), line, font=main_font)
        line_w = bbox[2] - bbox[0]
        draw.text(((w - line_w) // 2, cursor_y), line, font=main_font, fill=fg)
        cursor_y += line_h_main

    return img


def render_caption_strip(spec):
    """spec = {
        text: str, width: int, height: int,
        bg_color: str, text_color: str, font_size: int (optional),
    }
    Renders a solid-color strip with centered text. Used as the bottom-
    strip caption zone in the new (Phase 1.5a) layout where the caption
    is its own area below the frame, not an overlay.
    Returns an RGB PIL.Image (no alpha — fully opaque).
    """
    w = spec["width"]
    h = spec["height"]
    text = spec.get("text", "").strip()
    bg = ImageColor.getrgb(spec.get("bg_color", "#044be4"))
    fg = ImageColor.getrgb(spec.get("text_color", "#ffffff"))

    img = Image.new("RGB", (w, h), bg)
    draw = ImageDraw.Draw(img)
    if not text:
        return img

    # Font: scaled to fit 1-2 lines inside the strip with readable size.
    # Target ~30% of strip height per line — that leaves room for two
    # lines + a touch of vertical padding. Hard minimum 22px so captions
    # remain readable even on small mobile-sized strips.
    target_line = int(h * 0.30)
    size = max(target_line, 22)
    font = find_font(size, bold=False)

    # Wrap text to 88% of strip width, max 2 lines (Phase 1.5c — strict
    # cap on lines so captions stay legible; the caller is expected to
    # have already chunked narration into bite-sized pieces).
    inner_w = int(w * 0.88)
    lines = wrap_text(draw, text, font, inner_w)
    if len(lines) > 2:
        # Truncate to 2 lines, add ellipsis.
        lines = lines[:2]
        if not lines[-1].endswith("…"):
            lines[-1] = lines[-1].rstrip() + "…"

    # Recompute if 2-line layout doesn't fit vertically; shrink font.
    line_h = int(size * 1.25)
    while len(lines) > 0 and line_h * len(lines) > h - int(h * 0.15) and size > 18:
        size = int(size * 0.92)
        font = find_font(size, bold=False)
        lines = wrap_text(draw, text, font, inner_w)
        if len(lines) > 2:
            lines = lines[:2]
            if not lines[-1].endswith("…"):
                lines[-1] = lines[-1].rstrip() + "…"
        line_h = int(size * 1.25)

    block_h = line_h * len(lines)
    cursor_y = (h - block_h) // 2
    for line in lines:
        bbox = draw.textbbox((0, 0), line, font=font)
        line_w = bbox[2] - bbox[0]
        draw.text(((w - line_w) // 2, cursor_y), line, font=font, fill=fg)
        cursor_y += line_h
    return img


def render_caption(spec):
    """spec = {
        text: str, width: int, height: int,
        box_color: str (rgba semitransparent box),
        text_color: str, font_size: int (optional, auto-scales),
    }
    Returns an RGBA PIL.Image where the FULL frame is transparent and
    only the bottom-third caption box is opaque.
    """
    w = spec["width"]
    h = spec["height"]
    text = spec.get("text", "").strip()

    img = Image.new("RGBA", (w, h), (0, 0, 0, 0))  # fully transparent
    if not text:
        return img

    draw = ImageDraw.Draw(img)

    # Caption font: 2.5% of short edge, bold for legibility on small mobile screens.
    short_edge = min(w, h)
    size = int(spec.get("font_size") or short_edge * 0.025)
    font = find_font(size, bold=False)

    # Wrap text to 75% of frame width (interior of the caption box).
    inner_w = int(w * 0.75)
    lines = wrap_text(draw, text, font, inner_w)
    if not lines:
        return img

    line_h = int(size * 1.45)
    padding = int(short_edge * 0.02)
    text_block_h = line_h * len(lines)

    # Box height = text block + padding top and bottom.
    box_h = text_block_h + padding * 2
    box_w = inner_w + padding * 2 * 2  # bit extra side padding
    box_x = (w - box_w) // 2
    # Bottom-third placement: box top at 78% of height, ~5% margin from bottom.
    box_y = int(h * 0.78)
    # If the box would overflow, clamp it just above the frame bottom.
    if box_y + box_h > h - int(h * 0.03):
        box_y = h - int(h * 0.03) - box_h

    box_color = ImageColor.getrgb(spec.get("box_color", "#044be4"))
    box_alpha = int(spec.get("box_alpha", 217))  # 85% by default
    box_fill = box_color + (box_alpha,)
    text_color = ImageColor.getrgb(spec.get("text_color", "#ffffff")) + (255,)

    # Rounded box.
    radius = int(short_edge * 0.012)
    draw.rounded_rectangle(
        [box_x, box_y, box_x + box_w, box_y + box_h],
        radius=radius,
        fill=box_fill,
    )

    # Draw text centered inside the box.
    text_top = box_y + padding
    for line in lines:
        bbox = draw.textbbox((0, 0), line, font=font)
        line_w = bbox[2] - bbox[0]
        draw.text(((w - line_w) // 2, text_top), line, font=font, fill=text_color)
        text_top += line_h

    return img


def main():
    if len(sys.argv) < 3:
        print(json.dumps({"error": "usage: render_text.py <spec.json> <output.png>"}), flush=True)
        sys.exit(2)

    spec_path = sys.argv[1]
    out_path = sys.argv[2]

    with open(spec_path) as f:
        spec = json.load(f)

    mode = spec.get("mode")
    if mode == "title_card":
        img = render_title_card(spec)
    elif mode == "caption":
        img = render_caption(spec)
    elif mode == "caption_strip":
        img = render_caption_strip(spec)
    else:
        print(json.dumps({"error": f"unknown mode: {mode}"}), flush=True)
        sys.exit(3)

    img.save(out_path, "PNG")
    print(json.dumps({"ok": True, "out": out_path}), flush=True)


if __name__ == "__main__":
    main()
