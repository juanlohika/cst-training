#!/usr/bin/env python3
"""
Phase 1.7f — StyleTTS 2 helper, content-hashed edition.

For each section: synthesize ONE audio file from the section's `overview`
text. For each "instruction" unit: synthesize one audio file from the
unit's `text`. Audio FILES are keyed by a SHA-1 hash of the (normalized)
text — so identical text always produces the same filename and we never
regenerate. The manifest still keys entries by unit_id / section_id so
the renderer can look up "what audio goes here" by position, but the
filename it gets back is content-derived.

Result: re-planning, re-ordering, or shifting unit IDs cannot misalign
audio with captions — every WAV's filename IS its text fingerprint.

Usage:
    python tts_from_plan.py <clip_dir>

Outputs:
    <clip_dir>/audio/<sha1[:12]>.wav     (one per unique text)
    <clip_dir>/audio/plan_manifest.json  (manifest with text + audio_name)

Streams JSON-line progress on stdout for Rust to forward as Tauri events.
"""

import hashlib
import json
import os
import re
import sys
import warnings

warnings.filterwarnings("ignore")


def emit(obj):
    print(json.dumps(obj), flush=True)


# Acronyms preserved as-is so TTS spells them out correctly.
ACRONYM_KEEP = {
    "AI", "UI", "UX", "ID", "OK", "PIN", "MCS", "FF", "FSM", "CC", "SO",
    "BPI", "PNB", "HSBC", "RCBC", "BSP", "GCash", "AMII", "CST", "OS",
    "TV", "LFD", "OIC", "CIA", "AS7", "A37", "ZFold7", "S26",
}


def normalize_caps(text: str) -> str:
    """Convert ALL-CAPS words to Title Case so TTS reads them as words."""
    def fix(match: "re.Match[str]") -> str:
        word = match.group(0)
        bare = re.sub(r"[^A-Za-z]", "", word)
        if len(bare) < 2:
            return word
        if bare in ACRONYM_KEEP:
            return word
        return re.sub(r"[A-Za-z]+", lambda m: m.group(0)[0] + m.group(0)[1:].lower(), word)
    return re.sub(r"\b[A-Z][A-Z'\-]*\b", fix, text)


def text_fingerprint(text: str) -> str:
    """Stable filename-safe key for a piece of text. Same text → same key,
    independent of unit_id, section_id, or any plan-level positioning."""
    norm = normalize_caps(text.strip())
    return hashlib.sha1(norm.encode("utf-8")).hexdigest()[:12]


def wav_duration_seconds(path):
    import soundfile as sf
    info = sf.info(path)
    return info.frames / float(info.samplerate)


def collect_jobs(plan):
    """Walk plan.json and return a list of (key, text, meta) playback jobs.

    The key is the LOGICAL key the renderer looks up:
      - "section_<sid>" for overview audio
      - <unit_id> for instruction audio
    Each job will be rendered to a CONTENT-hashed filename, decoupling
    audio files from plan positions.
    """
    jobs = []
    for section in plan.get("sections", []):
        sid = section["id"]
        overview = (section.get("overview") or "").strip()
        if overview:
            jobs.append((f"section_{sid}", overview, {
                "kind": "section_overview",
                "section_id": sid,
                "frames": [],
            }))
        for unit in section.get("units", []):
            if unit["type"] != "instruction":
                continue
            text = (unit.get("text") or "").strip()
            if not text:
                continue
            jobs.append((unit["id"], text, {
                "kind": "instruction",
                "section_id": sid,
                "unit_id": unit["id"],
                "frames": unit.get("frames", []),
            }))
    return jobs


def main():
    if len(sys.argv) < 2:
        emit({"kind": "error", "message": "usage: tts_from_plan.py <clip_dir>"})
        sys.exit(2)

    clip_dir = sys.argv[1]
    plan_path = os.path.join(clip_dir, "plan.json")
    audio_dir = os.path.join(clip_dir, "audio")
    os.makedirs(audio_dir, exist_ok=True)

    if not os.path.isfile(plan_path):
        emit({"kind": "error", "message": f"plan.json missing: {plan_path}"})
        sys.exit(3)

    with open(plan_path) as f:
        plan = json.load(f)

    jobs = collect_jobs(plan)
    if not jobs:
        emit({"kind": "error", "message": "plan has no narratable units"})
        sys.exit(4)

    emit({"kind": "loading", "total": len(jobs)})

    # Lazy-import StyleTTS only if we actually need to render something.
    # That way fully-cached runs return in milliseconds.
    my_tts = None

    def ensure_tts():
        nonlocal my_tts
        if my_tts is None:
            from styletts2 import tts as styletts2_tts
            my_tts = styletts2_tts.StyleTTS2()
            emit({"kind": "loaded", "total": len(jobs)})
        return my_tts

    manifest_entries = []
    used_audio_names = set()  # track which files THIS plan refers to
    for i, (key, text, meta) in enumerate(jobs, start=1):
        normalized = normalize_caps(text)
        fp = text_fingerprint(text)
        wav_name = f"{fp}.wav"
        wav_path = os.path.join(audio_dir, wav_name)
        used_audio_names.add(wav_name)

        cached = os.path.isfile(wav_path)
        if not cached:
            ensure_tts()
            try:
                my_tts.inference(normalized, output_wav_file=wav_path)
            except Exception as e:
                emit({"kind": "error", "message": f"TTS failed on {key}: {e}"})
                sys.exit(5)

        dur = wav_duration_seconds(wav_path)
        manifest_entries.append({
            "key": key,
            "audio_name": wav_name,
            "kind": meta["kind"],
            "section_id": meta["section_id"],
            "unit_id": meta.get("unit_id"),
            "frames": meta.get("frames", []),
            "text": normalized,
            "duration_seconds": dur,
            "cached": cached,
        })
        emit({
            "kind": "progress",
            "index": i,
            "total": len(jobs),
            "name": key,
            "duration_seconds": dur,
            "cached": cached,
        })

    # Garbage-collect WAVs whose fingerprint no longer corresponds to any
    # current plan text. Safe because every file is keyed by content — if
    # the text is gone, nothing in the plan can need it.
    # We KEEP plan_manifest.json and any non-.wav file.
    removed = 0
    for fname in os.listdir(audio_dir):
        if not fname.endswith(".wav"):
            continue
        if fname not in used_audio_names:
            try:
                os.remove(os.path.join(audio_dir, fname))
                removed += 1
            except OSError:
                pass

    manifest_path = os.path.join(audio_dir, "plan_manifest.json")
    with open(manifest_path, "w") as f:
        json.dump({"version": 2, "entries": manifest_entries}, f, indent=2)

    emit({
        "kind": "done",
        "entries": len(jobs),
        "manifest": manifest_path,
        "orphans_removed": removed,
    })


if __name__ == "__main__":
    main()
