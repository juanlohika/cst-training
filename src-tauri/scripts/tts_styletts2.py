#!/usr/bin/env python3
"""
StyleTTS 2 helper invoked by CST Studio's Rust core.

Reads a narration.json from a clip dir and produces one WAV per fresh
entry into the audio/ subdir. Skips inherited entries (their audio comes
from the inherited-from frame, but step (f) skips inherited frames in
the final video so we just don't render them).

Usage:
    python tts_styletts2.py <clip_dir>

Where <clip_dir> is the absolute path to clips/<id>/ inside a project.
The script writes:
    <clip_dir>/audio/0001.wav   (per fresh entry)
    <clip_dir>/audio/manifest.json  (timing data for step f)

Streams progress to stdout as JSON lines so Rust can forward them as
Tauri events:
    {"kind":"progress","index":3,"total":42,"name":"0007.jpg"}
    {"kind":"done","entries":42,"audio_files":31}
"""

import json
import os
import re
import sys
import warnings

warnings.filterwarnings("ignore")


def emit(obj):
    print(json.dumps(obj), flush=True)


# Words on this list are kept as-is when they appear in ALL CAPS (don't
# title-case them, don't lowercase). Used for known acronyms / brand
# names. Extend as needed.
ACRONYM_KEEP = {
    "AI", "UI", "UX", "ID", "OK", "PIN", "MCS", "FF", "FSM", "CC", "SO",
    "BPI", "PNB", "HSBC", "RCBC", "BSP", "GCash", "AMII", "CST", "OS",
}


def normalize_caps(text: str) -> str:
    """Convert ALL-CAPS words to Title Case so the TTS engine reads them
    as words instead of spelling them out letter-by-letter.

    A word is "ALL-CAPS" if it has 2+ letters and contains no lowercase.
    Single letters (e.g. "I", "A") are left alone — they're already correct.
    Any token in ACRONYM_KEEP is preserved (TTS can spell those).
    Hyphenated runs like "TOP-LEFT" are treated as one token.
    """
    def fix(match: "re.Match[str]") -> str:
        word = match.group(0)
        bare = re.sub(r"[^A-Za-z]", "", word)
        if len(bare) < 2:
            return word
        if bare in ACRONYM_KEEP:
            return word
        # Title-case each space- or hyphen-separated piece.
        return re.sub(r"[A-Za-z]+", lambda m: m.group(0)[0] + m.group(0)[1:].lower(), word)
    # Match runs of uppercase letters (with internal hyphens or apostrophes).
    # \b ensures we don't break inside already-mixed-case tokens.
    return re.sub(r"\b[A-Z][A-Z'\-]*\b", fix, text)


def wav_duration_seconds(path):
    """Read WAV duration without loading the audio data. Uses soundfile
    rather than stdlib wave so we support float-PCM (format tag 3) WAVs
    that StyleTTS 2 produces — stdlib wave only knows int-PCM."""
    import soundfile as sf
    info = sf.info(path)
    return info.frames / float(info.samplerate)


def main():
    if len(sys.argv) < 2:
        emit({"kind": "error", "message": "usage: tts_styletts2.py <clip_dir>"})
        sys.exit(2)

    clip_dir = sys.argv[1]
    narration_path = os.path.join(clip_dir, "narration.json")
    audio_dir = os.path.join(clip_dir, "audio")
    os.makedirs(audio_dir, exist_ok=True)

    if not os.path.isfile(narration_path):
        emit({"kind": "error", "message": f"narration.json missing: {narration_path}"})
        sys.exit(3)

    with open(narration_path) as f:
        narration = json.load(f)
    entries = narration.get("entries", [])
    fresh = [e for e in entries if e.get("text") and not e.get("inherits_from")]

    if not fresh:
        emit({"kind": "error", "message": "no fresh narration entries to render"})
        sys.exit(4)

    emit({"kind": "loading", "total": len(fresh)})

    # Import lazily so the "loading" event reaches Rust before the slow
    # PyTorch import.
    from styletts2 import tts as styletts2_tts

    my_tts = styletts2_tts.StyleTTS2()

    emit({"kind": "loaded", "total": len(fresh)})

    manifest_entries = []
    for i, entry in enumerate(fresh, start=1):
        name = entry["name"]  # e.g. "0007.jpg"
        # Normalize ALL-CAPS words BEFORE feeding to TTS so screen labels
        # like "APP FORMS" → "App Forms" instead of being spelled out.
        # See normalize_caps for the rule.
        text = normalize_caps(entry["text"].strip())
        wav_name = name.replace(".jpg", ".wav")
        wav_path = os.path.join(audio_dir, wav_name)

        # Generate. Default StyleTTS 2 voice (no target_voice_path) — we
        # explicitly parked the voice-customization decision per the
        # conversation in step (e).
        try:
            my_tts.inference(text, output_wav_file=wav_path)
        except Exception as e:
            emit({"kind": "error", "message": f"TTS failed on {name}: {e}"})
            sys.exit(5)

        dur = wav_duration_seconds(wav_path)
        manifest_entries.append({
            "frame_name": name,
            "audio_name": wav_name,
            "timestamp_seconds": entry.get("timestamp_seconds"),
            "duration_seconds": dur,
        })

        emit({
            "kind": "progress",
            "index": i,
            "total": len(fresh),
            "name": name,
            "duration_seconds": dur,
        })

    # Write a per-clip audio manifest so step (f) doesn't have to re-probe
    # each WAV. It also serves as the source of truth for which frames
    # appear in the final video (only those with audio).
    manifest_path = os.path.join(audio_dir, "manifest.json")
    with open(manifest_path, "w") as f:
        json.dump({"version": 1, "entries": manifest_entries}, f, indent=2)

    emit({
        "kind": "done",
        "entries": len(entries),
        "audio_files": len(fresh),
        "manifest": manifest_path,
    })


if __name__ == "__main__":
    main()
