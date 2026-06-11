#!/usr/bin/env python3
"""
Thumbnail generator for CST Studio's Phase 1.7 bulk-scan pipeline.

The 3-pass narration architecture (scan → plan → script) needs to feed
ALL of a clip's frames to the AI in one call. Full-res frames (often
1080×2400 mobile or 1080×1920 PPTX) at ~150 KB each would balloon a
75-frame clip to ~10 MB of base64 — too big for a single LLM call.

This script generates 320-px-wide thumbnails (~10-20 KB each) so 75
thumbnails total under 1.5 MB. Lossy JPEG quality 75 is plenty for the
AI to identify section dividers and group similar frames.

Usage:
    python make_thumbnails.py <clip_dir>

Reads frames from <clip_dir>/frames/*.jpg, writes thumbnails to
<clip_dir>/thumbnails/*.jpg with the same basenames. Skips thumbnails
that already exist (caching), so re-runs are cheap.
"""

import json
import os
import sys

from PIL import Image


THUMB_WIDTH = 320
THUMB_QUALITY = 75


def main():
    if len(sys.argv) < 2:
        print(json.dumps({"error": "usage: make_thumbnails.py <clip_dir>"}), flush=True)
        sys.exit(2)

    clip_dir = sys.argv[1]
    frames_dir = os.path.join(clip_dir, "frames")
    thumbs_dir = os.path.join(clip_dir, "thumbnails")

    if not os.path.isdir(frames_dir):
        print(json.dumps({"error": f"frames dir missing: {frames_dir}"}), flush=True)
        sys.exit(3)

    os.makedirs(thumbs_dir, exist_ok=True)

    jpgs = sorted(
        f for f in os.listdir(frames_dir)
        if f.lower().endswith(".jpg")
    )
    if not jpgs:
        print(json.dumps({"error": "no jpg frames to thumbnail"}), flush=True)
        sys.exit(4)

    print(json.dumps({"kind": "loading", "total": len(jpgs)}), flush=True)

    made = 0
    skipped = 0
    for i, name in enumerate(jpgs, start=1):
        src = os.path.join(frames_dir, name)
        dest = os.path.join(thumbs_dir, name)

        # Cache: skip if thumb exists and is newer than the source.
        if os.path.exists(dest) and os.path.getmtime(dest) >= os.path.getmtime(src):
            skipped += 1
        else:
            try:
                img = Image.open(src)
                # Preserve aspect: only specify target width.
                if img.width > THUMB_WIDTH:
                    ratio = THUMB_WIDTH / img.width
                    new_h = int(round(img.height * ratio))
                    # Use LANCZOS for high-quality downscale (text stays legible).
                    img = img.resize((THUMB_WIDTH, new_h), Image.LANCZOS)
                # Strip EXIF / metadata; convert to RGB if needed (some JPGs
                # come in as RGBA from upstream tools).
                if img.mode != "RGB":
                    img = img.convert("RGB")
                img.save(dest, "JPEG", quality=THUMB_QUALITY, optimize=True)
                made += 1
            except Exception as e:
                print(
                    json.dumps({
                        "kind": "warn",
                        "name": name,
                        "message": f"thumbnail failed: {e}",
                    }),
                    flush=True,
                )

        if (i % 10) == 0 or i == len(jpgs):
            print(json.dumps({
                "kind": "progress",
                "index": i,
                "total": len(jpgs),
            }), flush=True)

    print(json.dumps({
        "kind": "done",
        "made": made,
        "skipped": skipped,
        "total": len(jpgs),
    }), flush=True)


if __name__ == "__main__":
    main()
