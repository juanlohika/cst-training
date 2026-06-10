# CST Studio — Technical Spec

**Status:** Draft v1 — pending Lester's review before any code lands.

CST Studio is a desktop training-video generator for the Mobile Optima CST
team. Takes a PowerPoint deck or a screen recording, produces an MP4 with
AI-written narration, AI-generated voiceover, and burned-in captions. Runs
on the user's Mac without any cloud account or API key (with one caveat
discussed under TTS below).

The sibling product, [CST OS Training Videos](https://github.com/juanlohika/cst-flow),
keeps running indefinitely as the web-based "easy mode" — CST Studio is
additive, not a replacement.

---

## 1. Why a desktop app

CST OS' web-based Training Videos works but is bottlenecked by Gemini's
free-tier rate limits (~3 TTS RPM, ~15 TTS req/day). For heavy users that
ceiling is two orders of magnitude below what we need. The team's strategic
choice is to keep the web app running for casual use and build a desktop
app that puts the entire pipeline on the user's machine — no rate limits,
no credentials, no cloud quotas to negotiate with.

The desktop app's other natural wins:
- Source files (slides, screen recordings) never leave the user's Mac
- Fully offline — works on a plane, in the field
- Generations are bounded only by the user's CPU/GPU
- No per-video API cost ever

The single hard trade-off: open-source voice quality is below Gemini's
Charon. We accept that, mitigate with multi-engine support, and document
the gap.

---

## 2. Goals & non-goals

### v1 goals
- Drag-and-drop PPTX or screen recording → final MP4
- AI generates per-scene scripts from the source (local Llama vision)
- AI generates per-scene voiceover (local TTS, multi-engine)
- ffmpeg renders the final MP4 with Tarkie-styled captions
- Output orientation: 9:16 / 16:9 / 1:1, per video
- Scene editor with per-scene rewrite, regenerate, edit
- Projects saved to `~/Documents/CST Studio/`, resumable
- Mac (M-series) installer ready for internal beta
- Settings panel (global + per-video overrides)

### v1 non-goals (deferred to v2)
- Windows installer (port comes once Mac is stable)
- Voice cloning UI (the engine supports it, UI ships in v1.1)
- Multi-language captions (English-only for v1)
- Real-time chat refinement (use the web app for that for now)
- Auto-update infrastructure (manual download for v1)
- App Store distribution (GitHub Releases for v1)

---

## 3. Tech stack

| Layer | Choice | Why |
|---|---|---|
| Shell | **Tauri 2** | ~10-20MB base vs Electron's 150MB. Same React UI we already have. Compiles to Mac + Windows from one codebase. Modern, well-maintained. |
| UI framework | React + Tailwind | Same as CST OS — ports most scene-editor code as-is |
| Local AI runtime | **Ollama** (for vision) | Battle-tested, manages model lifecycle, has an HTTP API we already integrate with |
| Vision model | **Llama 3.2 Vision 11B** (default) / **Qwen 2.5 VL 7B** (alternative) | Llama is more accurate on PPTX-style content; Qwen is faster on weaker hardware |
| TTS — default | **StyleTTS 2** (MIT) | Best open-source quality, lively, voice cloning supported |
| TTS — alternative | **Kokoro-82M** (Apache 2.0) | Lighter (1GB), faster, audiobook-narrator quality |
| TTS — fallback | **Piper** (MIT) | Tiny (~50MB), instant, robotic floor option |
| TTS runtime | ONNX Runtime via Node bindings | No Python dependency, runs in Tauri's sidecar process |
| Video render | **ffmpeg** + libass | Same engine as the existing worker. Port renders + caption code as-is. |
| PPTX parsing | **LibreOffice** (headless, bundled binary) | Already what the worker uses for PPTX→PDF→PNG. Bundled because there's no good JS PPTX parser. |
| State | Local SQLite via Tauri SQL plugin | Replaces Drizzle/Turso. Project metadata + scene state. |

### Why Tauri over Electron

- ~10-20MB base download vs ~150MB
- Memory footprint ~half of Electron
- Rust backend lets us bundle ffmpeg/LibreOffice/Ollama as sidecar
  processes with proper lifecycle management
- Code-signing pipeline is simpler on macOS

Risk: Tauri's ecosystem is younger. If we hit a wall with sidecar process
management or model loading, we have the option to fall back to Electron
before Phase 2. Decision point is end of Phase 1.

---

## 4. Architecture overview

```
┌──────────────────────────────────────────────────────────┐
│ CST Studio (Tauri app)                                   │
│ ┌────────────────────────────────────────────────────┐   │
│ │ React UI                                           │   │
│ │ - Scene editor (port from cst-flow)               │   │
│ │ - Settings panel                                   │   │
│ │ - My Projects drawer                               │   │
│ │ - Native <audio> playback                          │   │
│ └────────────────────────────────────────────────────┘   │
│ ┌────────────────────────────────────────────────────┐   │
│ │ Tauri Rust core                                    │   │
│ │ - Project file I/O (~/Documents/CST Studio/)       │   │
│ │ - Sidecar lifecycle (Ollama, ffmpeg, soffice)      │   │
│ │ - TTS via ONNX Runtime                             │   │
│ │ - SQLite project DB                                │   │
│ └────────────────────────────────────────────────────┘   │
│                          │                                │
│ ┌──────────────────┬─────┴────┬─────────────────────┐    │
│ │ Bundled binaries │ Bundled  │ Downloaded on       │    │
│ │ - ffmpeg         │ models   │ first run           │    │
│ │ - LibreOffice    │ - Piper  │ - Llama 3.2 Vision  │    │
│ │ - Ollama runtime │ voices   │ - Kokoro-82M        │    │
│ │                  │          │ - StyleTTS 2        │    │
│ └──────────────────┴──────────┴─────────────────────┘    │
└──────────────────────────────────────────────────────────┘
```

The bundled-vs-downloaded split keeps the installer small (~150-200MB)
while still working out of the box. First-run experience shows a
"Setting up — downloading models" screen with progress. After that, fully
offline forever.

---

## 5. Project file format

Each video project is a directory under `~/Documents/CST Studio/<project-name>/`:

```
2026-06-10 — Operator Promo Implementation/
├── project.json              ← metadata (status, voice, aspect ratio, etc.)
├── scenes.json               ← scene list (title, narration, caption, timing)
├── source/
│   └── original.pptx         ← (or original.mp4 for screen recordings)
├── extracted/
│   ├── slide_01.png          ← (PPTX path) rasterized slides
│   ├── slide_02.png
│   └── ...
│   └── frames.json           ← (video path) keyframes + timestamps + base64
├── audio/
│   ├── scene_01.wav
│   ├── scene_02.wav
│   └── ...
└── output/
    ├── 2026-06-10-vertical.mp4
    └── 2026-06-10-horizontal.mp4
```

- Human-readable, inspectable in Finder, easy to back up
- `project.json` is the canonical state — opening that file in CST Studio
  resumes wherever the project left off
- Multiple output renders can coexist (vertical + horizontal of same source)
- Deleting the folder fully removes the project — no orphan state elsewhere

---

## 6. Stage machine (ported from CST OS)

The web app's stage machine ports directly. Each transition is an internal
function (no HTTP requests since everything is local), but the contract is
the same — each stage is independently retryable, status persists to disk,
refresh-resumes-from-disk works.

```
draft → uploading → source-uploaded → content-extracted →
script-generated → generating-audio → ready → rendering → rendered
```

For desktop, "uploading" becomes "copying source to project folder" and
takes well under a second. Otherwise identical to web.

---

## 7. TTS: multi-engine architecture

Three engines bundled, user picks once in Settings, can override per video:

| Engine | Install size | Speed (M1) | Quality | Voice cloning |
|---|---|---|---|---|
| StyleTTS 2 (default) | ~700MB | ~2-3x real-time | Lively, expressive | Yes |
| Kokoro-82M | ~1GB | Real-time | Audiobook narrator | No |
| Piper | ~50MB per voice | Faster than real-time | Robotic but clear | No |

All three run via ONNX Runtime — no Python dependency. The Tauri Rust core
exposes a `tts(engine, voice, text) → wav bytes` function the UI calls.

Voice cloning (StyleTTS 2 only) ships in **v1.1**, not v1. The architecture
supports it from day one; the UI just isn't there yet.

### Voice options shipping in v1

- **StyleTTS 2**: 4 default voices (2 male, 2 female), bundled as ONNX
- **Kokoro**: 6 default voices (the same `af_bella`, `am_michael`, etc. from HuggingFace)
- **Piper**: 4 default voices (`en_US-lessac-medium`, `en_US-ryan-medium`, etc.)

Total bundled audio model weight: ~2GB. Downloaded on first run, not in
the installer.

### Style-prompt support

StyleTTS 2 takes an "exaggeration" parameter (0.0-1.0) that controls
delivery energy. We expose this as a Settings slider ("Calm" ↔ "Lively"),
default 0.5. Per-video override is supported. Kokoro and Piper don't have
this control; for them the slider is hidden.

---

## 8. Vision: Ollama + Llama 3.2 Vision

The desktop app bundles the Ollama runtime as a sidecar process. It auto-
starts when the app launches, auto-stops when the app quits. The user never
sees "Ollama" as a thing — it's an internal engine.

On first launch:
1. Ollama sidecar starts on port 11434 (or next available if 11434 is taken)
2. App checks if `llama3.2-vision` model is present locally
3. If not, downloads it (~6GB) with a progress bar
4. Once cached, all future launches start instantly

This is the same UX pattern as Stable Diffusion apps and Whisper-based
transcription apps. Users tolerate a one-time 6GB download because they're
getting "no API keys ever" in return.

**Fallback model:** If the user's machine has <16GB RAM, the app can
default to **Qwen 2.5 VL 7B** (~5GB, lighter but slightly lower quality).
Detected at install time from system RAM.

---

## 9. Caption rendering

Ports directly from `cst-flow/worker/src/captions.ts`. ffmpeg + libass +
Quicksand font (bundled). Output styles:

- **Tarkie default** (white Quicksand, dark outline, lower-third)
- **Bold yellow** (high-contrast, mid-screen)
- **Minimal** (small, top, low opacity)
- **None** (skip captions, audio + visuals only)

Caption preview-before-render is new in CST Studio (the web app renders
the full 2-min video before you see captions). UX: a sample frame from
the first scene with caption rendered over it, updates live as you change
the style preset.

---

## 10. Output orientation

Default: **Auto (match source)**. Per-video override available:
- Vertical 9:16 (TikTok / Reels)
- Horizontal 16:9 (YouTube / desktop)
- Square 1:1 (Instagram square)

When source and output aspect mismatch, we letterbox with a brand-colored
background (Tarkie violet gradient) — not black bars. This matches what
the web app does today and looks intentional rather than amateur.

Smart-fit (zoom/pan to keep UI elements visible) is deferred to v2.

---

## 11. Settings architecture

### Global Settings (Settings panel, app-wide)

- TTS engine: StyleTTS 2 (default) / Kokoro / Piper
- TTS voice: default per engine
- Style energy slider (StyleTTS 2 only)
- Default output orientation: Auto / 9:16 / 16:9 / 1:1
- Default caption style: Tarkie default / Bold yellow / Minimal / None
- Projects folder: `~/Documents/CST Studio/` (user can change)
- Performance preset: Quality (slow) / Balanced (default) / Fast (lower quality)
- Vision model: Llama 3.2 Vision (default) / Qwen 2.5 VL
- Language: en-US default

### Per-Video Settings (top of scene editor)

- Output orientation: inherits global, override per video
- Caption style: inherits global, override per video
- TTS voice: inherits global, override per video
- Style energy: inherits global, override per video
- Script style prompt: free-text ("lively, energetic" / "calm, instructional")

---

## 12. v1 feature scope (explicit list)

### What ships

- Drag-and-drop PPTX or MP4/MOV input from local Finder
- Auto-segment into scenes via Llama 3.2 Vision
- Edit script per scene (manual or AI-rewrite)
- Regenerate audio per scene
- Native audio player on each scene card
- "My Projects" drawer with status pills
- Resume in-progress projects from disk
- Output orientation: 9:16 / 16:9 / 1:1 with letterbox
- Caption rendering with 4 preset styles
- Final MP4 export to project's `output/` folder
- Mac M-series installer (signed + notarized)

### What's deferred to v1.1 or v2

- Voice cloning UI (engine supports it; UI in v1.1)
- Windows installer (v1.5)
- Multi-language captions (v2)
- Real-time chat refinement (v2)
- Auto-update (v2)
- Smart-fit aspect transformation (v2)
- Cloud sync between desktop and CST OS (v3 maybe, probably never)

---

## 13. Performance targets

For a typical 10-scene PPTX video on Apple Silicon M1 (16GB RAM):

| Stage | Target time |
|---|---|
| Source copy + extract | < 10s |
| Script generation (all scenes) | < 60s |
| TTS (all 10 scenes, StyleTTS 2) | < 90s |
| Final render (MP4 + captions) | < 90s |
| **Total wall-clock** | **< 4 minutes** |

On M3/M4 Pro/Max: roughly half those numbers.
On Intel Macs: 3-4x slower, still usable but the team will feel it.

Hard requirement: 16GB RAM minimum. App refuses to install on 8GB Macs
and shows a "your Mac doesn't have enough memory" message. We're not going
to ship a degraded experience on machines that can't handle vision models.

---

## 14. Distribution

- **v1:** GitHub Releases on `juanlohika/cst-training`
- **Installer:** `.dmg` for Mac (signed + notarized by Apple Developer account, ~$99/year)
- **No Sparkle/auto-update for v1** — users manually download new versions from GitHub Releases page. Acceptable for an internal team.
- **Code-signing certificate:** Lester's Apple Developer account
- **Crash reporting:** none for v1 (users tell us directly)

v2 adds: auto-update via Tauri's built-in updater, Windows installer with
EV cert (~$200/year), optional Sentry crash reporting.

---

## 15. Build & release pipeline

```bash
# Dev
cd ~/cst-training
npm install
npm run tauri dev          # Hot-reload dev mode

# Build
npm run tauri build        # Produces signed .dmg in src-tauri/target/release/bundle/dmg/
                           # ~150MB without bundled models

# Release
gh release create v1.0.0 \
  src-tauri/target/release/bundle/dmg/CST-Studio_1.0.0_aarch64.dmg
```

First-run flow:
1. User downloads + opens `.dmg`, drags app to Applications
2. First launch: "Welcome — let's download the AI models. ~7GB, takes 5-15 min."
3. Models download to `~/Library/Application Support/CST Studio/models/`
4. App starts. Done.

Subsequent launches: instant.

---

## 16. Open questions for Lester before code starts

These are the spec-level decisions I want explicit answers on before
Phase 1:

1. **Project folder default name format.** Right now I'm assuming
   `2026-06-10 — Operator Promo Implementation/`. Confirm or override.

2. **Tarkie branding inside the app.** Should the app's title bar, splash
   screen, and About box say "CST Studio" with Tarkie violet, or do we
   want a more neutral look (e.g. for if you ever sell it externally one
   day)? I'll go neutral by default — easier to add branding than remove.

3. **License header for source files.** "Proprietary, internal Mobile
   Optima use only" is what I'd default to. Confirm.

4. **Should the app phone home with telemetry (anonymous usage counts,
   crash reports, model version)?** I'd default to NO — privacy is a
   feature of the app. Confirm.

5. **What happens when an Intel Mac user tries to install?** Options:
   (a) refuse, with a "Apple Silicon required" message;
   (b) allow but default to Piper TTS + Qwen vision (lower quality,
       acceptable speed);
   (c) allow but warn about expected slowness.
   Default: **(c)** — don't gatekeep, just be honest.

6. **First-run model download — interruptible?** If user closes the app
   mid-download, can they resume on next launch? Default: yes, resumable
   downloads via HTTP range requests.

---

## 17. Risks & mitigations

| Risk | Mitigation |
|---|---|
| StyleTTS 2 ONNX export is harder than expected | Fall back to bundling Python runtime via PyOxidizer. Adds ~30MB and 1 day of work. |
| Ollama sidecar doesn't play nice with Tauri lifecycle | Run Ollama as a managed child process with explicit start/stop. Worst case: require users to install Ollama separately and configure endpoint. |
| Llama 3.2 Vision quality on PPTX is below the bar | Switch to Qwen 2.5 VL or InternVL by Phase 1. Both are in the same quality range. |
| Mac code-signing breaks for team users | Distribute via "Open Anyway" workaround initially; fix signing post-v1. |
| Final installer is bigger than expected (>500MB) | Move more models to first-run download. Acceptable up to ~7GB total post-download. |
| Lester's Mac is M1 but a teammate has Intel | App still runs (option c above), just slower. Document expected speeds in the README. |

---

## 18. Milestones

| Phase | Duration | Outcome |
|---|---|---|
| Phase 0: spec sign-off | this week | This doc, signed off by Lester |
| Phase 1: walking skeleton | 3-4 days | Tauri app boots, drags in a PPTX, runs Ollama, generates one audio, renders one MP4. No polish. |
| Phase 2: full UI port | 4-5 days | All scene editor features working, multi-engine TTS, Settings panel, project drawer |
| Phase 3: polish + installer | 4-5 days | Signed Mac installer, first-run flow, error handling, internal beta-ready |
| Phase 4 (post-launch): voice cloning UI + Windows | tbd | Ships as v1.1 |

**Realistic total to v1 internal beta: ~3 weeks.**

---

## 19. Appendix: what's NOT going into CST Studio

These get mentioned because someone will ask:

- **No user accounts.** Desktop app, no login.
- **No cloud sync.** Projects are local. If a user wants to share, they zip the folder.
- **No real-time collaboration.** One user per project at a time.
- **No payment / pricing infrastructure.** Internal tool, free forever.
- **No analytics / tracking.** Privacy is a feature.
- **No iOS / Android.** Mobile training-video editing is a different product.

---

## 20. Next steps after Lester signs off

1. Confirm the 6 open questions in section 16
2. I scaffold the Tauri project (`npm create tauri-app@latest`)
3. I commit the bare project to `juanlohika/cst-training`
4. Phase 1 work begins

The walking-skeleton phase is where I'll discover whether StyleTTS 2 ONNX
works as cleanly as I expect. If it does, the rest is execution. If it
doesn't, we'll have a decision point about Python runtime vs swapping the
TTS engine.

---

**Sign-off needed from Lester on:**
- Section 16 (open questions, 6 of them)
- Overall direction confirmed

Once signed off, Phase 1 starts.
