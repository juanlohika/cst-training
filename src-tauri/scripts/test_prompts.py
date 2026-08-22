#!/usr/bin/env python3
"""
Backend prompt tuning harness for CST Studio.

Runs scan + plan prompts against real frames WITHOUT launching the Tauri app.
Edit the PROMPT constants below, run `python3 test_prompts.py`, see results
in ~10 seconds per frame.

Once a prompt looks good, copy it verbatim into src-tauri/src/lib.rs
(build_ocr_classify_prompt or build_plan_prompt).
"""

import json
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

# ─── CONFIG ───────────────────────────────────────────────────────────────
CLIP_DIR = Path("/Users/tarkielester/Downloads/Promoter Operator Promo Implementation Form/clips/01")
TESSERACT = "/opt/homebrew/bin/tesseract"
OLLAMA_URL = "http://127.0.0.1:11434/api/chat"
SCAN_MODEL = "llama3.2:3b"
PLAN_MODEL = "minicpm-v:8b"

MAIN_PROMPT = (
    "This is a training video for Promoter Operators using the Tarkie App. "
    "Note that this is step by step guide. Start with selecting the Promo "
    "Implementation Form from App Forms menu."
)

# Frames to test — pick a representative mix
SAMPLE_FRAMES = [
    "0001.jpg",  # title slide
    "0002.jpg",  # likely title slide
    "0004.jpg",  # the one that dumped all OCR
    "0013.jpg",  # FF Store Surveyed dropdown
    "0019.jpg",  # CC OFFERS section
    "0051.jpg",  # was misclassified as SECTION
    "0058.jpg",  # was misclassified as STEP but described title
    "0073.jpg",  # Refused to Sign Memo
]

# ─── PROMPTS (edit these — copy the winners back to lib.rs) ───────────────

def build_ocr_classify_prompt(main_prompt: str, ocr_text: str) -> str:
    """Extract structured fields from OCR rather than free-text summary.
    A small model (llama3.2:3b) latches onto example text in prompts, so we
    force it to fill slots with values it must READ from the OCR."""
    return f"""You read OCR text from one frame of a mobile-app training video and extract structured information.

Project context: {main_prompt}

OCR text (status bar gibberish like "9:24 tas ll" and the screen's HEADER like "AMIl Operator Promo Implementation - June 2026" appear on every frame — they are NOT the focus, they only tell you which form is open):

\"\"\"
{ocr_text}
\"\"\"

EXTRACTION RULES — fill these JSON fields by reading the OCR above:

  "screen_header": copy the form/screen header text (the line near the top showing which form is open). If none, write "".
  "body_focus": the SINGLE most prominent body element — a field name, list heading, or section title visible in the body. Use the EXACT label from the OCR. STRIP any trailing question/help text (e.g. "FF STORE SURVEYED / Did you visit or speak..." → "FF STORE SURVEYED"). For a question-style field, just the field NAME.
  "body_kind": one of:
       "field"      — body shows a single field/dropdown. SIGNALS: a label (often ALL CAPS or with a trailing "*") followed by help text or a question, followed by a NUMERIC value like "1", "0", or a "v"/"Vv" dropdown indicator. Frames showing one form question with its answer go here.
       "list"       — body shows MULTIPLE choice items as a list (3+ items stacked), like banks (BPI, China Bank, HSBC), promo non-implementation reasons (Refused to Sign Memo, Dealer's Restriction, N/A), or product variants stacked with dropdown arrows.
       "form_list"  — body shows the "App Forms" listing — multiple FORM NAMES the user picks one of (e.g. AMII Operator Promo Implementation, Brandshop Asset Database, CIA Form, Competitor Deployment).
       "instructions" — body shows ONLY explanatory help copy ("To begin your day, please tap on Menu..."), no field label, no value, no list.
       "heading"    — body has ONLY a short heading/title (1-6 words), nothing else interactive.
  "visible_value": if body_kind="field" and a value like "1", "0", or "Vv" appears near the field label, copy it EXACTLY. Otherwise "".
  "visible_options": if body_kind="list" or "form_list", list up to 5 option labels from the OCR, comma-separated. Otherwise "".
  "is_section_divider": true ONLY when body_kind="heading". False for everything else (fields, lists, instructions all have content so they are NOT section dividers).

DECISION HINTS:
- The mere fact a label is ALL CAPS does NOT mean it's a heading — most form fields in this app are ALL CAPS labels (e.g. "FF STORE SURVEYED", "TV/LFD", "CC OFFERS IMPLEMENTATION"). If a value "1"/"0"/"Vv" appears nearby in the OCR → it is body_kind="field".
- "App Forms" specifically (with multiple form-name entries listed) → body_kind="form_list", body_focus="App Forms".
- A page showing 3+ option strings stacked vertically with arrow indicators → body_kind="list".
- Help/instruction copy ("To begin your day...", "Please tap...", "Don't forget to...") with NO field label or value → body_kind="instructions".

CRITICAL:
- Read the OCR. Do not invent fields not in the OCR. Do not copy field names from these instructions ("FF Store Surveyed" is mentioned here as an example, but only put it in your answer if it actually appears in the OCR text above).
- Status-bar gibberish at line starts ("9:24 tas ll", "Mull ©", "tama Ml ©") is NOISE — ignore.
- Bottom-nav gibberish at the very end ("Ul O <", "Il O K") is NOISE — ignore.
- The screen_header (form name near top) goes in screen_header field, NOT body_focus.

Reply with JSON only, no preamble:
{{"screen_header": "...", "body_focus": "...", "body_kind": "...", "visible_value": "...", "visible_options": "...", "is_section_divider": false}}"""


# ─── HELPERS ──────────────────────────────────────────────────────────────

def run_ocr(frame_path: Path) -> str:
    """Run Tesseract on a frame and return the extracted text."""
    out = subprocess.run(
        [TESSERACT, str(frame_path), "-", "--psm", "6"],
        capture_output=True,
        text=True,
        timeout=30,
    )
    return out.stdout.strip()


def call_ollama(model: str, prompt: str, format_json: bool = True) -> dict | str:
    """POST to Ollama /api/chat with optional JSON schema enforcement."""
    payload = {
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": False,
        "options": {"temperature": 0.2},
    }
    if format_json:
        payload["format"] = "json"

    req = urllib.request.Request(
        OLLAMA_URL,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=420) as resp:
        data = json.loads(resp.read())
    content = data["message"]["content"]
    if format_json:
        try:
            return json.loads(content)
        except json.JSONDecodeError:
            return {"_parse_error": True, "raw": content}
    return content


# ─── CRITERION DERIVATION (runs in code, not LLM) ─────────────────────────

def derive_criterion(focus: str, kind: str) -> str:
    """Deterministic criterion-phrase derivation. Runs in code, not the LLM,
    so we never see the wrong criterion copied across frames."""
    f = focus.lower()
    if "ff store surveyed" in f:
        return "based on whether you surveyed the FF store at this location"
    if "tv/lfd" in f or "tv lfd" in f:
        return "based on the actual TV/LFD condition at the store"
    if "cc offers" in f:
        return "based on the actual CC Offers implementation status at the location"
    if "time in" in f:
        return "based on the actual time you arrived at the location"
    if "promo non-implementation reason" in f:
        return "matching the actual reason the promo was not implemented at this store"
    if "ultra color" in f or "s26 series" in f or "zfold" in f or "feature kv" in f:
        return f"based on actual deployment of the {focus} display at the store"
    if "competitor deployment" in f or "competitor inventory" in f:
        return "matching what you observed of competitor activity at the location"
    if kind == "form_list":
        return ""
    if kind == "list":
        return "matching what you observed at the store"
    return "based on what you observed on site"


# ─── MAIN ─────────────────────────────────────────────────────────────────

def main():
    frames_dir = CLIP_DIR / "frames"
    if not frames_dir.exists():
        print(f"❌ frames dir not found: {frames_dir}", file=sys.stderr)
        sys.exit(1)

    cache_path = Path(__file__).parent / ".scan_cache.json"
    cache_enabled = "--fresh" not in sys.argv  # pass --fresh to skip cache
    cached = {}
    if cache_enabled and cache_path.exists():
        cached = json.loads(cache_path.read_text())
        print(f"  📁 using scan cache ({len(cached)} frames). Pass --fresh to re-scan.")

    print("═" * 80)
    print(f"  PROMPT TEST HARNESS — {len(SAMPLE_FRAMES)} frames")
    print(f"  scan model: {SCAN_MODEL}")
    print(f"  clip:       {CLIP_DIR.name}")
    print("═" * 80)

    results = []

    for frame_name in SAMPLE_FRAMES:
        if frame_name in cached:
            r = cached[frame_name]
            print(f"\n── {frame_name} (cached) ─────────────────────────")
            print(f"  → [{r['label']}] {r['kind']}: {r['focus']}")
            results.append(r)
            continue

        frame_path = frames_dir / frame_name
        if not frame_path.exists():
            print(f"\n⚠ {frame_name} not found, skipping")
            continue

        print(f"\n── {frame_name} ────────────────────────────────────────")
        t0 = time.time()
        ocr_text = run_ocr(frame_path)
        t_ocr = time.time() - t0

        ocr_preview = ocr_text.replace("\n", " ⏎ ")[:200]
        print(f"  OCR ({t_ocr:.1f}s): {ocr_preview}")

        prompt = build_ocr_classify_prompt(MAIN_PROMPT, ocr_text)
        t0 = time.time()
        result = call_ollama(SCAN_MODEL, prompt, format_json=True)
        t_llm = time.time() - t0

        if isinstance(result, dict) and not result.get("_parse_error"):
            kind = result.get("body_kind", "?")
            focus = result.get("body_focus", "?")
            val = result.get("visible_value", "")
            opts = result.get("visible_options", "")
            is_sect = result.get("is_section_divider", False)
            label = "A" if is_sect else "B"
            extras = []
            if val:
                extras.append(f"value={val!r}")
            if opts:
                extras.append(f"options=[{opts}]")
            extras_str = "  " + " ".join(extras) if extras else ""
            print(f"  → [{label}] {kind}: {focus}{extras_str}  ({t_llm:.1f}s)")
            results.append({
                "frame": frame_name, "label": label, "kind": kind,
                "focus": focus, "value": val, "options": opts,
            })
        else:
            print(f"  ❌ parse error: {result}")

    # Persist scan cache so plan-only iterations are fast.
    if cache_enabled:
        cache_data = {r["frame"]: r for r in results}
        cache_path.write_text(json.dumps(cache_data, indent=2))

    print("\n" + "═" * 80)
    print("  SCAN SUMMARY")
    print("═" * 80)
    for r in results:
        print(f"  {r['frame']}  [{r['label']}]  {r['kind']:12s}  {r['focus']}")

    # ─── PLAN STAGE TEST ───────────────────────────────────────────────
    print("\n" + "═" * 80)
    print(f"  PLAN STAGE ({PLAN_MODEL}) — building instructions from scan output")
    print("═" * 80)

    plan_input_lines = []
    for r in results:
        # Sanitize: strip required-field asterisks and obvious OCR garbage.
        focus = r["focus"].rstrip("*").strip()
        line = f"- {r['frame']} [{r['kind']}]: {focus}"
        if r["value"]:
            v = r["value"].strip()
            if v not in {"Vv", "v.", "ie", "ie)", "1 €", "0 €"} and len(v) <= 4:
                line += f" (demo value shown: {v} — NOTE: this is one example; trainee picks based on their situation)"
        if r["options"]:
            opts_raw = [o.strip().rstrip("*").strip() for o in r["options"].split(",")]
            noise = {"Vv", "v.", "v", "Mother's Day²", "Mother's Day"}
            opts_clean = []
            for o in opts_raw:
                if o and o not in noise and o not in opts_clean:
                    opts_clean.append(o)
            if opts_clean:
                line += f" (choice options: {', '.join(opts_clean[:5])})"
        # Pre-compute the criterion deterministically — keeps the LLM from
        # copying example criteria. The model just slots it in.
        criterion = derive_criterion(focus, r["kind"])
        if criterion:
            line += f" (criterion to use: {criterion})"
        plan_input_lines.append(line)

    print(f"\n  Plan input (what the script-writer sees per frame):\n")
    for ln in plan_input_lines:
        print(f"    {ln}")

    # Per-frame plan generation. Most kinds are produced deterministically
    # from templates in code (no LLM). The LLM is only called for "instructions"
    # kind which needs free-form orienting narration.
    print(f"\n  Generating plan per-frame ({len(results)} frames)...\n")
    instructions = []
    total_plan_t = 0.0
    for r in results:
        focus_clean = title_case_label(r["focus"].rstrip("*").strip())
        criterion = derive_criterion(focus_clean, r["kind"])
        has_value = bool(r.get("value", "").strip())
        kind = r["kind"]

        text = ""
        dt = 0.0
        source = "template"

        if kind == "field" and has_value:
            text = f"In the {focus_clean} field, select 1 or 0, {criterion}."
        elif kind == "field":
            text = f"Tap the {focus_clean} field to set its value."
        elif kind == "list":
            opts_raw = r.get("options", "")
            opts = [o.strip().rstrip("*").strip() for o in opts_raw.split(",")]
            opts = [o for o in opts if o and o not in {"Vv", "v.", "v"}][:3]
            opts_str = ", ".join(opts)
            if opts_str:
                text = f"From the {focus_clean} options, select the one {criterion}. Choices include {opts_str}."
            else:
                text = f"From the {focus_clean} options, select the one {criterion}."
        elif kind == "form_list":
            text = f"Tap {focus_clean} from the App Forms list to open it."
        elif kind == "heading":
            text = ""  # no narration
        elif kind == "instructions":
            # Only kind that needs the LLM — free-form orienting sentence.
            source = "llm"
            prompt = (
                f"Write ONE training-video narration sentence (12-20 words) that orients "
                f"the trainee about what they see. Frame context: '{focus_clean}'. "
                f"Address the trainee directly. Do not write 'Operator selects' or 'User taps'. "
                f'Output ONLY: {{"text": "your sentence"}}'
            )
            t0 = time.time()
            out = call_ollama(PLAN_MODEL, prompt, format_json=True)
            dt = time.time() - t0
            total_plan_t += dt
            text = out.get("text", str(out)) if isinstance(out, dict) else f"❌ {out}"
        else:
            text = focus_clean

        instructions.append({"frame": r["frame"], "text": text, "secs": dt, "source": source})
        tag = f"[{source}, {dt:.1f}s]" if source == "llm" else "[template]"
        print(f"    {r['frame']}  {tag}  {text}")

    print(f"\n  Total plan time: {total_plan_t:.1f}s ({total_plan_t/max(len(results),1):.1f}s/frame avg)")
    print()


def build_per_frame_plan_prompt(main_prompt: str, scan_record: dict, criterion: str) -> str:
    """Per-frame plan call — we pre-select ONE template in code and ask the
    LLM only to render it. No pattern-choosing for the model."""
    kind = scan_record["kind"]
    focus_clean = title_case_label(scan_record["focus"].rstrip("*").strip())
    has_value = bool(scan_record.get("value", "").strip())

    if kind == "field" and has_value:
        template = f'In the {focus_clean} field, select 1 or 0, {criterion}.'
        instruction = "Polish this for natural narration. Do not change the field name. Do not change the criterion phrase. Do not turn 'select 1 or 0' into 'select 1'. You may smooth grammar."
    elif kind == "field":
        template = f'Tap the {focus_clean} field to set its value.'
        instruction = "Use this as-is or smooth slightly."
    elif kind == "list":
        opts_raw = scan_record.get("options", "")
        opts = [o.strip().rstrip("*").strip() for o in opts_raw.split(",")]
        opts = [o for o in opts if o and o not in {"Vv", "v.", "v"}][:3]
        opts_str = ", ".join(opts) if opts else ""
        if opts_str:
            template = f'From the {focus_clean} options, {criterion}. Choices include {opts_str}.'
        else:
            template = f'From the {focus_clean} options, {criterion}.'
        instruction = "Polish for natural narration. Keep the field name, criterion, and option list. Do not invent new options."
    elif kind == "form_list":
        template = f'Tap {focus_clean} from the App Forms list to open it.'
        instruction = "Use this as-is."
    elif kind == "instructions":
        template = ""
        instruction = f"Write ONE short orienting sentence for what the trainee sees on this screen. Frame context: {scan_record['focus']}. 12-20 words. Tutorial voice (address the trainee, not 'the user')."
    elif kind == "heading":
        return ""  # caller skips
    else:
        template = focus_clean
        instruction = "Write ONE short instruction sentence (12-20 words)."

    return f"""You are polishing one narration sentence for a training video.

PROJECT: {main_prompt}

DRAFT SENTENCE: {template}

INSTRUCTION: {instruction}

GLOBAL RULES:
- Address the TRAINEE — not "the operator" / "the user" / "the recorder".
- Never write "Operator selects", "User taps".
- Output 12-22 words.

Output ONLY:
{{"text": "your final sentence"}}"""


def title_case_label(s: str) -> str:
    """Title Case a label, preserving short product codes."""
    KEEP_UPPER = {"FF", "TV", "LFD", "CC", "BPI", "HSBC", "PNB", "FSM", "MCS", "OIC", "AMII", "CIA", "DPOoP", "AS7", "A37"}
    out = []
    for w in s.split():
        bare = w.strip(".,*").strip()
        if bare.upper() in KEEP_UPPER or (len(bare) <= 3 and bare.isupper()):
            out.append(w)
        elif w.isupper():
            out.append(w.title())
        else:
            out.append(w)
    return " ".join(out)


def build_plan_prompt(main_prompt: str, scan_summary: str) -> str:
    return f"""You are writing narrated training instructions for a mobile-app demo recording. The TRAINEE is who you address — NOT the person in the recording.

PROJECT CONTEXT: {main_prompt}

SCAN OUTPUT — each frame's UI focus extracted from on-screen text:
{scan_summary}

YOUR JOB — for each frame, write ONE narration line. Use the right TEMPLATE per scan kind:

TEMPLATES:

[field] with a numeric demo value (1, 0, etc.):
   → "In the [field name] field, select 1 or 0 based on [decision criterion]."
   Decision-criterion hints (derive from the field name):
     • "FF Store Surveyed" → "based on whether you surveyed the FF store at this location"
     • "TV/LFD" → "based on the actual TV/LFD condition at the store"
     • "$26 Ultra Color w/Effects" → "based on actual deployment of the $26 Ultra Color w/Effects display"
     • "CC Offers Implementation" → "based on the actual CC Offers implementation status"
     • Generic fallback: "based on what you observed on site"

[field] WITHOUT a demo value:
   → "Tap the [field name] field to set its value."

[list] (multi-choice list — Bank names, Non-Implementation Reasons, etc.):
   → "From the [list name] options, select the one matching [decision criterion]: [option 1], [option 2], or [option 3]."
   Decision-criterion hints:
     • Banks → "matching the actual bank at the location"
     • Non-Implementation Reason → "matching the actual reason the promo was not implemented"
     • Product variants → "matching the actual product deployment"

[form_list] (App Forms list — picking which form to open):
   → "Tap [form-name] from the App Forms list."

[instructions] (onboarding tooltip with no field):
   → Write a short orienting sentence about what the trainee sees / can do next.

[heading] (true section divider):
   → No narration needed — leave text="".

STRICT RULES:
1. NEVER write "Operator selects", "User taps", "The operator chooses". TRAINEE voice only.
2. NEVER lock in the demo value. If demo shows value 1, narrate "select 1 or 0" — never "select 1".
3. NEVER mention the wrapping form (e.g. "Promo Implementation Form"). Reference fields directly.
4. Strip asterisks (*) from field names — those are required-field markers, not part of the name.
5. Convert ALL-CAPS to Title Case (e.g. "FF STORE SURVEYED" → "FF Store Surveyed").
6. 12-22 words per line.

Reply with JSON only:
{{
  "instructions": [
    {{"frame": "0001.jpg", "text": "..."}},
    ...
  ]
}}"""


if __name__ == "__main__":
    main()
