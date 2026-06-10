# CST Studio

Desktop training video generator for the Mobile Optima CST team. Takes a
PowerPoint deck or screen recording and produces an MP4 with AI-generated
narration, voiceover, and burned-in captions — all running locally on the
user's Mac. No API keys, no cloud quotas, no rate limits.

**Status:** Phase 1 (walking skeleton). Not yet usable.

See [SPEC.md](./SPEC.md) for the full technical spec.

## Stack

- **Tauri 2** — desktop shell (~150MB installer base)
- **React 19 + TypeScript + Vite** — UI
- **Rust** — file I/O, ffmpeg/Ollama sidecar lifecycle
- **Ollama + Llama 3.2 Vision** — AI script generation (bundled)
- **StyleTTS 2 / Kokoro / Piper** — voice generation (multi-engine, bundled)
- **ffmpeg + libass** — final MP4 render with captions (bundled)
- **LibreOffice** — PPTX parsing (bundled)

## Dev setup

Prerequisites:
- macOS Apple Silicon (M-series) — Phase 1 only targets Mac
- Node 20+
- Rust stable (install via [rustup](https://rustup.rs/))

```bash
# Install dependencies
npm install

# Dev mode (hot-reload, opens Tauri window)
npm run tauri dev

# Production build (creates a .dmg in src-tauri/target/release/bundle/)
npm run tauri build
```

## Repo layout

```
cst-training/
├── src/                  ← React UI
├── src-tauri/            ← Rust core + Tauri config
│   ├── src/lib.rs        ← Tauri commands exposed to JS
│   ├── tauri.conf.json   ← App metadata, bundle config
│   └── Cargo.toml        ← Rust deps
├── SPEC.md               ← Technical spec (architecture, scope, milestones)
└── package.json
```

## Relationship to CST OS

Sibling product to [CST OS Training Videos](https://github.com/juanlohika/cst-flow).
CST OS keeps running indefinitely as the web-based "easy mode" — CST Studio
is additive, not a replacement. Use CST OS when convenient (browser, no
install). Use CST Studio when you need heavy use without rate limits.

## License

Proprietary. Internal Mobile Optima use only. Not for redistribution.
