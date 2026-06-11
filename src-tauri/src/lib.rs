// CST Studio — Tauri Rust core.
//
// Phase 1 step (a): project model. A "project" is a folder on disk with
// project.json + a clips/ subdir. Each clip is a numbered subfolder with
// the source file copied in. Frames/narration/audio land in clip folders
// in later steps.
//
// Phase 1 step (b): frame extraction. ffmpeg samples 1 fps from videos,
// soffice (LibreOffice) renders one JPG per slide for PPTX. Both write
// into clips/<id>/frames/*.jpg.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Per-clip cancellation flags for the narration loop. The clip key is
/// "<project_dir>::<clip_id>" so two projects' clips don't collide. The
/// narrate_clip loop polls its flag every iteration; cancel_narration
/// flips it. We use Arc so we can hold a clone in the running task even
/// after the lock is released.
fn narration_cancels() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    static CANCELS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
    CANCELS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cancel_key(project_dir: &str, clip_id: &str) -> String {
    format!("{project_dir}::{clip_id}")
}

fn register_narration(project_dir: &str, clip_id: &str) -> Arc<AtomicBool> {
    let key = cancel_key(project_dir, clip_id);
    let flag = Arc::new(AtomicBool::new(false));
    narration_cancels()
        .lock()
        .unwrap()
        .insert(key, flag.clone());
    flag
}

fn unregister_narration(project_dir: &str, clip_id: &str) {
    let key = cancel_key(project_dir, clip_id);
    narration_cancels().lock().unwrap().remove(&key);
}

const PROJECT_FILE: &str = "project.json";
const CLIPS_DIR: &str = "clips";
const FRAMES_DIR: &str = "frames";
const PROJECT_SCHEMA_VERSION: u32 = 1;

// Step (b) constraints. See conversation context for the rationale.
const MAX_CLIP_SECONDS: f64 = 600.0; // 10 minutes
const JPEG_QUALITY: u8 = 2; // ffmpeg -q:v scale (2 = ~q90, lower=better)
/// ffmpeg scene-detection threshold. 0.0–1.0 — higher means "needs a bigger
/// visual change to count as a new scene." Tuned empirically on a real
/// mobile-app screen recording: changes are visually subtle (a tooltip
/// appears, a row highlights), so scene scores rarely exceed ~0.07 even
/// for meaningful UI transitions. 0.03 catches roughly all the moments a
/// human would describe; lower values like 0.01 over-sample (1000+ frames
/// for a 2:37 video).
const SCENE_THRESHOLD: f64 = 0.03;
/// We always keep the first frame regardless of scene score, then add
/// the scene-change frames. Plus a final "is anything important" guarantee:
/// if scene detection produced fewer than this many frames, we fall back
/// to extracting one frame every N seconds so short or static videos
/// still get some narration coverage.
const MIN_FRAMES_FALLBACK: usize = 4;
/// Pre-resize frames during extraction so qwen3-vl doesn't pay for the
/// full source resolution. The model resizes to ~1120px internally anyway.
const RESIZE_LONG_EDGE: u32 = 1120;
/// Seconds to advance past a detected scene change before sampling the
/// frame. ffmpeg flags a scene change at the EXACT transition pixel-diff
/// peak, which is often mid-animation (a dropdown half-open, a tooltip
/// still appearing). Waiting half a second lets the UI settle so we
/// capture a clean, fully-rendered state.
const SCENE_OFFSET_SECONDS: f64 = 0.5;

// External tool paths. Currently hard-coded to brew + LibreOffice.app
// locations on this Mac (Phase 1 is single-developer). Phase 3 will swap
// these to Tauri sidecar binaries via a small lookup helper — keeping the
// indirection so that swap stays a one-place change.
fn ffmpeg_path() -> PathBuf {
    PathBuf::from("/opt/homebrew/bin/ffmpeg")
}
fn ffprobe_path() -> PathBuf {
    PathBuf::from("/opt/homebrew/bin/ffprobe")
}
fn soffice_path() -> PathBuf {
    PathBuf::from("/Applications/LibreOffice.app/Contents/MacOS/soffice")
}

#[tauri::command]
fn ping() -> String {
    format!("Rust core alive. cst-studio v{}", env!("CARGO_PKG_VERSION"))
}

#[derive(Serialize, Deserialize, Clone)]
struct Project {
    version: u32,
    name: String,
    opening_title_text: String,
    main_prompt: String,
    /// BCP-47 language tag for the narration ("en", "tl" for Tagalog,
    /// "ko" for Korean, "tl-en" for Taglish, etc.). Currently fixed to
    /// "en" — UI hides this field for Phase 1. Phase 2 will expose a
    /// language selector that drives:
    ///   - the vision narration prompt (instructs minicpm to write in
    ///     the target language, with explicit "keep English UI labels"
    ///     for code-switched languages like Taglish)
    ///   - TTS engine + voice selection
    ///   - the caption font (CJK fonts for ko/ja/zh)
    /// Schema field added in v1 so v2-migration isn't needed later.
    #[serde(default = "default_language")]
    language: String,
    created_at: DateTime<Utc>,
    clips: Vec<Clip>,
    /// Absolute path to this project's folder on disk. Re-derived on load
    /// (so moving a project folder doesn't break it) and stripped before
    /// writing to project.json by write_project_file. Always present when
    /// crossing the Tauri IPC boundary so the JS side can use it.
    #[serde(default)]
    dir: String,
}

fn default_language() -> String {
    "en".to_string()
}

#[derive(Serialize, Deserialize, Clone)]
struct Clip {
    /// Two-digit zero-padded clip ID matching its folder name ("01", "02", ...).
    id: String,
    /// Original filename for display (e.g. "Operator Promo - June.mp4").
    source_name: String,
    bytes: u64,
    /// Probed duration in seconds. None until step (b) runs ffprobe.
    duration_seconds: Option<f64>,
    /// Editable; AI fills this in step (d). Empty by default.
    title: String,
    /// Overview narration: a single-paragraph "what this section covers"
    /// summary that plays during the section title card. Lets per-frame
    /// narration focus on JUST the step action without restating context.
    /// Editable. Generated by generate_overview after the clip is narrated.
    #[serde(default)]
    overview: String,
    status: ClipStatus,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ClipStatus {
    Draft,
    FramesExtracted,
    Narrated,
    AudioReady,
    Rendered,
}

/// Create a new project folder. If parent_dir/name already exists, the folder
/// is auto-suffixed with " - YYYYMMDD HHMM" (user picked this collision policy).
#[tauri::command(rename_all = "camelCase")]
fn create_project(parent_dir: String, name: String) -> Result<Project, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Project name cannot be empty".into());
    }
    let parent = Path::new(&parent_dir);
    if !parent.is_dir() {
        return Err(format!("Save location does not exist: {parent_dir}"));
    }

    let now = Utc::now();
    let mut folder_name = name.clone();
    let mut target = parent.join(&folder_name);
    if target.exists() {
        let stamp = now.format("%Y%m%d %H%M").to_string();
        folder_name = format!("{name} - {stamp}");
        target = parent.join(&folder_name);
        // Extremely unlikely, but if the timestamped one also exists, just bail.
        if target.exists() {
            return Err(format!(
                "Folder already exists even with timestamp suffix: {}",
                target.display()
            ));
        }
    }

    fs::create_dir_all(target.join(CLIPS_DIR))
        .map_err(|e| format!("Cannot create project folder: {e}"))?;

    let project = Project {
        version: PROJECT_SCHEMA_VERSION,
        name,
        opening_title_text: String::new(),
        main_prompt: String::new(),
        language: default_language(),
        created_at: now,
        clips: Vec::new(),
        dir: target.to_string_lossy().into_owned(),
    };
    write_project_file(&project)?;
    Ok(project)
}

/// Load an existing project from its folder.
#[tauri::command(rename_all = "camelCase")]
fn load_project(project_dir: String) -> Result<Project, String> {
    let dir = Path::new(&project_dir);
    if !dir.is_dir() {
        return Err(format!("Not a directory: {project_dir}"));
    }
    let json_path = dir.join(PROJECT_FILE);
    let raw = fs::read_to_string(&json_path)
        .map_err(|e| format!("Cannot read {}: {e}", json_path.display()))?;
    let mut project: Project = serde_json::from_str(&raw)
        .map_err(|e| format!("project.json is invalid: {e}"))?;
    if project.version != PROJECT_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported project version {}; this build expects {}",
            project.version, PROJECT_SCHEMA_VERSION
        ));
    }
    project.dir = dir.to_string_lossy().into_owned();
    Ok(project)
}

/// Save project metadata back to project.json. Called on autosave from the UI.
#[tauri::command]
fn save_project(project: Project) -> Result<(), String> {
    write_project_file(&project)
}

/// Copy a source file into the project as a new clip. Returns the updated
/// project (with the new clip appended).
#[tauri::command(rename_all = "camelCase")]
fn add_clip(project_dir: String, source_path: String) -> Result<Project, String> {
    let project_dir_path = Path::new(&project_dir).to_path_buf();
    let mut project = load_project(project_dir.clone())?;

    let src = Path::new(&source_path);
    if !src.is_file() {
        return Err(format!("Source is not a file: {source_path}"));
    }
    let source_name = src
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "Source has no filename".to_string())?
        .to_string();
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| "Source has no extension".to_string())?
        .to_ascii_lowercase();
    if !matches!(ext.as_str(), "mp4" | "mov" | "pptx") {
        return Err(format!("Unsupported extension: {ext}"));
    }

    // Next clip id is one past the current highest. Padded to two digits
    // for natural sort order in Finder.
    let next_id: u32 = project
        .clips
        .iter()
        .filter_map(|c| c.id.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    let clip_id = format!("{next_id:02}");
    let clip_dir = project_dir_path.join(CLIPS_DIR).join(&clip_id);
    fs::create_dir_all(&clip_dir)
        .map_err(|e| format!("Cannot create clip folder: {e}"))?;
    let dest = clip_dir.join(format!("source.{ext}"));

    fs::copy(src, &dest)
        .map_err(|e| format!("Cannot copy source into project: {e}"))?;

    let bytes = fs::metadata(&dest)
        .map(|m| m.len())
        .map_err(|e| format!("Cannot stat copied source: {e}"))?;

    let clip = Clip {
        id: clip_id,
        source_name,
        bytes,
        duration_seconds: None,
        title: String::new(),
        overview: String::new(),
        status: ClipStatus::Draft,
    };
    project.clips.push(clip);
    write_project_file(&project)?;
    Ok(project)
}

/// Remove a clip from the project (deletes the folder + updates project.json).
/// Other clips keep their original IDs — we do NOT renumber, so existing
/// asset paths stay stable across removals.
#[tauri::command(rename_all = "camelCase")]
fn remove_clip(project_dir: String, clip_id: String) -> Result<Project, String> {
    let mut project = load_project(project_dir.clone())?;
    let before = project.clips.len();
    project.clips.retain(|c| c.id != clip_id);
    if project.clips.len() == before {
        return Err(format!("No clip with id {clip_id}"));
    }

    let clip_dir = Path::new(&project_dir).join(CLIPS_DIR).join(&clip_id);
    if clip_dir.exists() {
        fs::remove_dir_all(&clip_dir)
            .map_err(|e| format!("Cannot delete clip folder: {e}"))?;
    }
    write_project_file(&project)?;
    Ok(project)
}

/// Reorder clips. Caller provides the full list of clip IDs in the new order.
/// Folder names stay the same (still "01", "02"...) — only the in-memory order
/// in clips[] changes. The Vec order is what determines render order later.
#[tauri::command(rename_all = "camelCase")]
fn reorder_clips(project_dir: String, ordered_ids: Vec<String>) -> Result<Project, String> {
    let mut project = load_project(project_dir.clone())?;
    if ordered_ids.len() != project.clips.len() {
        return Err(format!(
            "Reorder list has {} ids but project has {} clips",
            ordered_ids.len(),
            project.clips.len()
        ));
    }
    let mut reordered: Vec<Clip> = Vec::with_capacity(project.clips.len());
    for id in &ordered_ids {
        let idx = project
            .clips
            .iter()
            .position(|c| &c.id == id)
            .ok_or_else(|| format!("Unknown clip id in reorder: {id}"))?;
        reordered.push(project.clips.remove(idx));
    }
    project.clips = reordered;
    write_project_file(&project)?;
    Ok(project)
}

/// Information about a single extracted frame, returned to the UI for
/// the collapsible thumbnail grid. Paths are absolute so the UI can use
/// them with Tauri's convertFileSrc().
#[derive(Serialize, Deserialize, Clone)]
struct FrameInfo {
    /// 1-based frame number, zero-padded to 4 digits ("0001" .. "0120").
    name: String,
    /// Absolute path to the JPG on disk.
    path: String,
    /// Seconds from the start of the clip — null for PPTX slides
    /// (slides don't have a meaningful timestamp).
    timestamp_seconds: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ExtractResult {
    /// Updated project with the clip's status/duration refreshed.
    project: Project,
    /// Per-frame info for the clip we just extracted.
    frames: Vec<FrameInfo>,
}

/// One row in narration.json: either a fresh AI narration or a pointer
/// to a previous identical frame whose narration we inherit.
#[derive(Serialize, Deserialize, Clone)]
struct NarrationEntry {
    /// Matches the frame filename, e.g. "0001.jpg".
    name: String,
    /// Seconds from the start of the clip (1 fps so this is just the index
    /// minus one). None for PPTX slides.
    timestamp_seconds: Option<f64>,
    /// Perceptual hash (16 hex chars) used to dedupe near-identical frames.
    hash: String,
    /// AI narration text. None means "skip this frame, use inherits_from".
    text: Option<String>,
    /// When text is None, this points to the frame whose narration applies
    /// here too (most recent earlier frame with a fresh narration).
    inherits_from: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Narration {
    /// Schema version for forward compatibility.
    version: u32,
    /// One entry per extracted frame, in extraction order.
    entries: Vec<NarrationEntry>,
}

// ============================================================================
// Phase 1.7 — 3-pass scan → plan → script architecture.
//
// The old per-frame `narrate_clip` writes one narration per frame in
// isolation, which produces redundant or out-of-context output for
// multi-section clips. The new pipeline:
//
//   1. scan_clip   — AI looks at ALL thumbnails at once, returns per-frame
//                    semantic summary + section-divider flags + filler
//                    flags + UI actions + narrative arc + inferred mode.
//                    Output: scan.json
//   2. plan_script — AI consumes scan.json + main_prompt and produces a
//                    multi-section plan: each section has a title +
//                    overview + ordered script_units. Each unit can span
//                    multiple frames and has a type (title_card,
//                    section_title, instruction, filler). Also returns
//                    Tier-1 clarification questions for the user.
//                    Output: plan.json
//   3. generate_audio_from_plan — one TTS WAV per non-filler unit.
//   4. render — walks plan.json as chapters → final MP4.
//
// Old narrate_clip is kept for backwards compat with existing projects.
// ============================================================================

const SCAN_SCHEMA_VERSION: u32 = 1;
const PLAN_SCHEMA_VERSION: u32 = 1;

// (Constants from the abandoned batched-vision scan architecture were
// removed when Phase 1.7 switched to OCR + small text LLM. See
// feedback_cst_studio_ocr_hybrid_scan memory.)

/// A "key frame" is one of the meaningful beats the AI identified —
/// either a section divider (chapter intro slide) or a step that
/// warrants its own narration line. Frames NOT in scan.key_frames are
/// implicitly continuity / filler: they appear in the final video
/// briefly while the previous key frame's audio plays.
#[derive(Serialize, Deserialize, Clone)]
struct KeyFrame {
    /// Frame filename in the clip, e.g. "0007.jpg". Must match an
    /// extracted frame on disk.
    name: String,
    /// "section_divider" | "step".
    #[serde(rename = "type")]
    kind: String,
    /// One short sentence of what's happening here (input to plan stage).
    summary: String,
    /// For section_divider: the section title text on screen.
    /// For step: null.
    #[serde(default)]
    title: Option<String>,
    /// For step: what UI action is happening or about to happen,
    /// e.g. "tap Menu icon top-left", "select Store ID from dropdown".
    /// For section_divider: null.
    #[serde(default)]
    ui_action: Option<String>,
    /// AI's grouping hint — short noun phrase. Plan stage uses this to
    /// cluster steps into sections when section_dividers are sparse.
    #[serde(default)]
    implicit_topic: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Scan {
    version: u32,
    /// AI's one-paragraph summary of the whole clip's arc — used as
    /// context when the plan stage writes per-section overviews.
    narrative_arc: String,
    /// AI's inferred mode for this clip: "step_by_step" | "showcase" | "mixed".
    inferred_mode: String,
    /// Only the FRAMES THAT MATTER. Order = frame-order in the clip.
    /// All other frames are continuity filler that play silently or
    /// alongside the previous key frame's narration.
    key_frames: Vec<KeyFrame>,
}

#[derive(Clone, Serialize)]
struct ScanProgress {
    clip_id: String,
    /// "thumbnails" → "calling_ai" → "parsing" → "saving" → "done".
    stage: String,
    detail: String,
    /// 0–1, or -1 for indeterminate.
    fraction: f64,
}

/// Phase 1.7 Pass 1: scan all of a clip's frames using **OCR + text LLM**,
/// not vision LLM. See feedback_cst_studio_ocr_hybrid_scan memory for
/// the rationale (vision LLMs proved too slow on 16GB Macs; OCR captures
/// the on-screen labels we need and a small text LLM classifies cheaply).
///
/// Architecture (no batching, no vision):
///
///   1. Generate 320px thumbnails (cached).
///   2. Compute pHash for each thumbnail.
///   3. For each frame in order:
///        a. If pHash matches the previous frame's hash within
///           BATCH_BREAK_HAMMING_THRESHOLD - 5 → skip (continuation frame).
///        b. Run Tesseract OCR on the thumbnail.
///        c. If OCR text is empty AND it's not the first frame, treat as
///           decorative/continuation, skip.
///        d. Call OLLAMA_SCAN_MODEL (llama3.2:3b) with the OCR text +
///           project context. Get back {label, summary}.
///        e. Append to key_frames if label is A or B.
///   4. After all per-frame classifications, do a single text-LLM call
///      to write the narrative_arc + inferred_mode from the collected
///      key_frame summaries (cheap, ~5s).
///   5. Write final scan.json.
///
/// Progress events stream per-frame so the UI can show "12/75".
#[tauri::command(rename_all = "camelCase")]
async fn scan_clip(
    app: tauri::AppHandle,
    project_dir: String,
    clip_id: String,
) -> Result<Scan, String> {
    use tauri::Emitter;

    let project = load_project(project_dir.clone())?;
    let clip = project
        .clips
        .iter()
        .find(|c| c.id == clip_id)
        .ok_or_else(|| format!("No clip with id {clip_id}"))?;
    if clip.status == ClipStatus::Draft {
        return Err("Extract frames first before scanning.".into());
    }

    let emit = |stage: &str, detail: &str, fraction: f64| {
        let _ = app.emit(
            "scan-progress",
            ScanProgress {
                clip_id: clip_id.clone(),
                stage: stage.to_string(),
                detail: detail.to_string(),
                fraction,
            },
        );
    };

    // Step 1: ensure thumbnails exist (cached).
    emit("thumbnails", "Generating thumbnails…", 0.02);
    let clip_dir = Path::new(&project_dir).join(CLIPS_DIR).join(&clip_id);
    let thumbs_dir = clip_dir.join("thumbnails");
    run_thumbnail_script(&clip_dir).await?;

    let mut thumb_files: Vec<PathBuf> = fs::read_dir(&thumbs_dir)
        .map_err(|e| format!("Cannot read thumbnails dir: {e}"))?
        .filter_map(|r| r.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("jpg"))
                .unwrap_or(false)
        })
        .collect();
    thumb_files.sort();
    if thumb_files.is_empty() {
        return Err("No thumbnails produced — re-extract frames first.".into());
    }
    let frame_names: Vec<String> = thumb_files
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(|s| s.to_string()))
        .collect();
    let total = thumb_files.len();

    // Step 2: pHash every frame to find near-duplicates.
    emit("hashing", &format!("Hashing {total} thumbnails…"), 0.05);
    let mut hashes: Vec<String> = Vec::with_capacity(total);
    for path in &thumb_files {
        let h = compute_phash(&path.to_string_lossy())
            .map_err(|e| format!("Cannot hash {}: {e}", path.display()))?;
        hashes.push(h);
    }

    // Step 3: per-frame OCR + classify loop.
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("Cannot build HTTP client: {e}"))?;

    let mut key_frames: Vec<KeyFrame> = Vec::new();
    let mut last_hash: Option<&str> = None;
    let mut skipped_dup = 0;
    let mut skipped_empty = 0;

    for (i, (path, name)) in thumb_files.iter().zip(frame_names.iter()).enumerate() {
        // 3a: skip near-duplicates of the immediately previous frame.
        if let Some(prev) = last_hash {
            let dist = hamming_distance(prev, &hashes[i]);
            // We use a slightly tighter threshold than batch breaks because
            // here we want to skip ONLY when frames are nearly identical
            // (small dist = same screen); BATCH_BREAK was for "is this
            // visually different enough to be a new section".
            if dist <= 3 {
                skipped_dup += 1;
                last_hash = Some(&hashes[i]);
                continue;
            }
        }
        last_hash = Some(&hashes[i]);

        emit(
            "classify",
            &format!("Frame {} of {total}", i + 1),
            0.10 + 0.80 * (i as f64 / total as f64),
        );

        // 3b: OCR.
        let ocr_text = run_tesseract(path).unwrap_or_default();
        let ocr_clean = ocr_text.trim();

        // 3c: skip if no text and not the first frame (decorative).
        if ocr_clean.is_empty() && i > 0 {
            skipped_empty += 1;
            continue;
        }

        // 3d: classify via llama3.2:3b.
        let snippet: String = ocr_clean.chars().take(400).collect::<String>()
            .replace('\n', " / ");
        let prompt = build_ocr_classify_prompt(&project.main_prompt, &snippet);
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "label": { "type": "string", "enum": ["A", "B"] },
                "summary": { "type": "string" }
            },
            "required": ["label", "summary"]
        });
        let body = serde_json::json!({
            "model": OLLAMA_SCAN_MODEL,
            "prompt": prompt,
            "stream": false,
            "format": schema,
            "options": {
                "num_predict": 100,
                "temperature": 0.2,
            }
        });

        let resp = match http
            .post(OLLAMA_URL)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("scan_clip: HTTP error on {name}: {e} — skipping");
                continue;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            eprintln!("scan_clip: Ollama {status} on {name}: {txt} — skipping");
            continue;
        }
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                eprintln!("scan_clip: JSON error on {name}: {e} — skipping");
                continue;
            }
        };
        let raw = json.get("response").and_then(|v| v.as_str()).unwrap_or("");
        let cleaned = raw.trim();

        #[derive(Deserialize)]
        struct ClassifyResponse {
            label: String,
            summary: String,
        }
        let parsed: ClassifyResponse = match serde_json::from_str(cleaned) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("scan_clip: parse error on {name}: {e} | raw={cleaned} — skipping");
                continue;
            }
        };

        let kind = match parsed.label.trim().to_uppercase().as_str() {
            "A" => "section_divider",
            "B" => "step",
            _ => "step",
        }
        .to_string();

        // For section dividers, use the OCR snippet as the title hint.
        // (Plan stage can clean it up; we just need a starting point.)
        let title = if kind == "section_divider" {
            Some(snippet.chars().take(80).collect::<String>())
        } else {
            None
        };
        // ui_action is left None at the scan stage — the next stage
        // (plan_script with minicpm-v) derives the actual imperative
        // action from the summary + OCR text. Copying summary into
        // ui_action made the UI look like duplicated text and added
        // no real information.
        key_frames.push(KeyFrame {
            name: name.clone(),
            kind,
            summary: parsed.summary,
            title,
            ui_action: None,
            implicit_topic: None,
        });
    }

    emit(
        "merging",
        &format!(
            "Identified {} key frames ({} dup-skipped, {} empty-skipped) — writing arc…",
            key_frames.len(),
            skipped_dup,
            skipped_empty
        ),
        0.92,
    );

    // Step 4: one text-only LLM call for narrative_arc + inferred_mode.
    let (narrative_arc, inferred_mode) =
        merge_ocr_scan_summary(&http, &project.main_prompt, &key_frames)
            .await
            .unwrap_or_else(|e| {
                eprintln!("scan_clip: merge pass failed ({e}) — using fallback values");
                (
                    "(narrative arc unavailable)".to_string(),
                    "mixed".to_string(),
                )
            });

    let scan = Scan {
        version: SCAN_SCHEMA_VERSION,
        narrative_arc,
        inferred_mode,
        key_frames,
    };
    write_scan_file(&clip_dir, &scan)?;
    emit("done", "Done", 1.0);
    Ok(scan)
}

/// Run Tesseract OCR on a thumbnail; return stdout text (may be empty).
fn run_tesseract(image: &Path) -> Result<String, String> {
    let output = Command::new(tesseract_path())
        .arg(image)
        .arg("-")
        .output()
        .map_err(|e| format!("Cannot run tesseract: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tesseract exited with status {}",
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Per-frame OCR-classifier prompt for llama3.2:3b.
fn build_ocr_classify_prompt(main_prompt: &str, ocr_text: &str) -> String {
    let mut p = String::new();
    p.push_str(
        "You are classifying one frame from a training video by reading \
         the on-screen text extracted via OCR. Your output will be used \
         to write a step-by-step training script later.\n\n",
    );
    if !main_prompt.trim().is_empty() {
        p.push_str("Project context: ");
        p.push_str(main_prompt.trim());
        p.push_str("\n\n");
    }
    p.push_str("OCR text from this frame:\n");
    p.push_str("\"\"\"\n");
    p.push_str(ocr_text);
    p.push_str("\n\"\"\"\n\n");
    p.push_str(
        "STEP 1 — Classify:\n\
         A = section divider / title slide (short 1-5 word title centered, \
            intro of a new topic, NO form fields or buttons visible)\n\
         B = action step (UI screen with specific elements: form fields, \
            dropdowns, button labels, list items, menu items)\n\
         \n\
         STEP 2 — For your summary, you MUST:\n\
         - Reference at least ONE specific UI element BY NAME from the OCR text above.\n\
         - Use exact labels from the OCR (e.g. \"FF Store Surveyed\", \"Promo \
            Implementation Form\", \"BPI\", \"China Bank\", \"Trade Presenter\", \
            \"Refused to Sign Memo\") — do NOT genericize to \"UI element\" or \
            \"form field\".\n\
         - Write what the operator is doing or what is on screen, in 10-15 words.\n\
         - When on-screen text is ALL CAPS, write it in Title Case in your summary.\n\
         \n\
         BAD examples (do NOT do this):\n\
         - \"User interacts with UI screen elements\" (too generic)\n\
         - \"User interacts with promo implementation form\" (no specific element)\n\
         \n\
         GOOD examples (DO this):\n\
         - \"Operator opens the App Forms list, AMII Promo Implementation visible\"\n\
         - \"Operator selects 1 in the FF Store Surveyed dropdown\"\n\
         - \"Operator chooses Refused to Sign Memo as the non-implementation reason\"\n\
         - \"Operator confirms the China Bank entry in the bank list\"\n\
         \n\
         Reply with JSON only: {\"label\": \"A\" or \"B\", \"summary\": \"...\"}",
    );
    p
}

/// Merge pass: write narrative_arc + inferred_mode from collected key_frames.
async fn merge_ocr_scan_summary(
    http: &reqwest::Client,
    main_prompt: &str,
    key_frames: &[KeyFrame],
) -> Result<(String, String), String> {
    let mut prompt = String::new();
    prompt.push_str(
        "You just helped scan a training video. Given the key frames \
         identified, produce a brief narrative arc summary and infer the \
         overall mode.\n\n",
    );
    if !main_prompt.trim().is_empty() {
        prompt.push_str("Project context: ");
        prompt.push_str(main_prompt.trim());
        prompt.push_str("\n\n");
    }
    prompt.push_str("Key frames in order:\n");
    for kf in key_frames {
        prompt.push_str("- ");
        prompt.push_str(&kf.name);
        prompt.push_str(" [");
        prompt.push_str(&kf.kind);
        prompt.push_str("]: ");
        prompt.push_str(&kf.summary);
        prompt.push_str("\n");
    }
    prompt.push_str(
        "\nProduce JSON only:\n\
         {\n  \"narrative_arc\": \"<<=60 word paragraph describing the whole clip’s story end-to-end>\",\n  \"inferred_mode\": \"step_by_step|showcase|mixed\"\n}\n",
    );

    let body = serde_json::json!({
        "model": OLLAMA_SCAN_MODEL,
        "prompt": prompt,
        "stream": false,
        "format": {
            "type": "object",
            "properties": {
                "narrative_arc": { "type": "string" },
                "inferred_mode": { "type": "string" }
            },
            "required": ["narrative_arc", "inferred_mode"]
        },
        "options": {
            "num_predict": 250,
            "temperature": 0.3,
        }
    });
    let resp = http
        .post(OLLAMA_URL)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Merge pass: cannot reach Ollama: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Merge pass returned {status}: {text}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Merge pass response not JSON: {e}"))?;
    let raw = json
        .get("response")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Merge pass missing response field".to_string())?;

    #[derive(Deserialize)]
    struct MergeResponse {
        #[serde(default)]
        narrative_arc: Option<String>,
        #[serde(default)]
        inferred_mode: Option<String>,
    }
    let parsed: MergeResponse = serde_json::from_str(raw.trim())
        .map_err(|e| format!("Merge response invalid JSON: {e}"))?;
    Ok((
        parsed.narrative_arc.unwrap_or_else(|| "(no arc)".to_string()),
        parsed.inferred_mode.unwrap_or_else(|| "mixed".to_string()),
    ))
}

fn write_scan_file(clip_dir: &Path, scan: &Scan) -> Result<(), String> {
    let path = clip_dir.join("scan.json");
    let tmp = clip_dir.join("scan.json.tmp");
    let pretty = serde_json::to_string_pretty(scan)
        .map_err(|e| format!("Cannot serialize scan: {e}"))?;
    fs::write(&tmp, pretty).map_err(|e| format!("Cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .map_err(|e| format!("Cannot finalize {}: {e}", path.display()))?;
    Ok(())
}

/// Load scan.json for a clip if it exists. Used by the UI to populate
/// the scan-preview panel without re-running the scan.
#[tauri::command(rename_all = "camelCase")]
fn load_scan(project_dir: String, clip_id: String) -> Result<Option<Scan>, String> {
    let path = Path::new(&project_dir)
        .join(CLIPS_DIR)
        .join(&clip_id)
        .join("scan.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let scan: Scan = serde_json::from_str(&raw)
        .map_err(|e| format!("scan.json is invalid: {e}"))?;
    Ok(Some(scan))
}

// ============================================================================
// Phase 1.7c — plan_script
// ============================================================================

/// One narratable unit in the plan. Each unit has:
///   - a stable id (so edits target the right thing across reloads)
///   - a type (title_card / instruction / filler) that drives render
///   - the frames it covers (multiple frames can share one unit's narration)
///   - the text the narrator says (null for filler / title_card types)
#[derive(Serialize, Deserialize, Clone)]
struct ScriptUnit {
    id: String,
    /// "title_card" | "instruction" | "filler"
    /// - title_card: shown silently as a section opener (overview is the audio)
    /// - instruction: narrated step. Frames held while text is read aloud.
    /// - filler: frames briefly shown, no audio.
    #[serde(rename = "type")]
    kind: String,
    /// Ordered frame names (e.g. ["0007.jpg", "0008.jpg"]). Multiple frames
    /// can share one narration when the action spans several frames.
    frames: Vec<String>,
    /// The narrator's line. None for filler/title_card units.
    #[serde(default)]
    text: Option<String>,
}

/// One section of the training video — a chapter with its own title + intro.
#[derive(Serialize, Deserialize, Clone)]
struct PlanSection {
    id: String,
    /// On-screen section title (shown on the section title card).
    title: String,
    /// Narrator's intro paragraph for this section. Plays during the
    /// section title card. Editable.
    overview: String,
    /// Ordered narration units. Walking this in order = the section's script.
    units: Vec<ScriptUnit>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Plan {
    version: u32,
    /// Ordered sections; this is the chapter list of the final video.
    sections: Vec<PlanSection>,
    /// Frames the user explicitly excluded from the final video. They
    /// stay on disk + still appear in scan.json, but render skips them.
    /// This is the "delete a frame" feature.
    #[serde(default)]
    excluded_frames: Vec<String>,
}

/// Phase 1.7c: build a plan.json from scan.json + main_prompt + clip
/// title/overview. Uses minicpm-v:8b (text-only — no images needed since
/// the scan already extracted what each frame is about).
///
/// The plan groups scan's key_frames into sections. Each section gets:
///   - A title (from the first section_divider in that section, OR derived)
///   - An overview paragraph (AI-written from the section's frame summaries)
///   - Ordered script_units (one per "step" key_frame, plus title_card units
///     wrapping section_dividers)
///
/// Editable downstream via update_plan / toggle_frame_excluded.
#[tauri::command(rename_all = "camelCase")]
async fn plan_script(
    project_dir: String,
    clip_id: String,
) -> Result<Plan, String> {
    let project = load_project(project_dir.clone())?;
    let clip = project
        .clips
        .iter()
        .find(|c| c.id == clip_id)
        .ok_or_else(|| format!("No clip with id {clip_id}"))?;

    let scan = load_scan(project_dir.clone(), clip_id.clone())?
        .ok_or_else(|| "Smart scan must be run first.".to_string())?;
    if scan.key_frames.is_empty() {
        return Err("Scan produced no key frames — nothing to plan.".into());
    }

    let prompt = build_plan_prompt(&project.main_prompt, &clip.title, &clip.overview, &scan);
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| format!("Cannot build HTTP client: {e}"))?;

    // Schema: AI produces section groupings only. We stamp the ids ourselves
    // after parsing.
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "sections": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "overview": { "type": "string" },
                        "units": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "type": {
                                        "type": "string",
                                        "enum": ["title_card", "instruction", "filler"]
                                    },
                                    "frames": {
                                        "type": "array",
                                        "items": { "type": "string" }
                                    },
                                    "text": { "type": "string" }
                                },
                                "required": ["type", "frames"]
                            }
                        }
                    },
                    "required": ["title", "overview", "units"]
                }
            }
        },
        "required": ["sections"]
    });

    let body = serde_json::json!({
        "model": OLLAMA_VISION_MODEL, // minicpm-v:8b — text-only invocation
        "prompt": prompt,
        "stream": false,
        "format": schema,
        "options": {
            // Plan output can be substantial: 4-8 sections * 3-6 units each.
            // 2000 tokens covers a 20-section plan with room to spare.
            "num_predict": 2000,
            "temperature": 0.3,
        }
    });

    let resp = http
        .post(OLLAMA_URL)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Cannot reach Ollama at {OLLAMA_URL}: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Ollama returned {status}: {text}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Ollama response not JSON: {e}"))?;
    let raw = json
        .get("response")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Ollama response missing 'response' field".to_string())?;

    #[derive(Deserialize)]
    struct PlanResponse {
        sections: Vec<PlanSectionResponse>,
    }
    #[derive(Deserialize)]
    struct PlanSectionResponse {
        title: String,
        overview: String,
        units: Vec<UnitResponse>,
    }
    #[derive(Deserialize)]
    struct UnitResponse {
        #[serde(rename = "type")]
        kind: String,
        frames: Vec<String>,
        #[serde(default)]
        text: Option<String>,
    }

    let parsed: PlanResponse = serde_json::from_str(raw.trim()).map_err(|e| {
        format!(
            "Plan AI returned invalid JSON: {e}\nRaw (first 500 chars):\n{}",
            &raw.chars().take(500).collect::<String>()
        )
    })?;

    // Validate frame names against scan's key_frames; drop unknowns.
    let valid_frames: std::collections::HashSet<&String> =
        scan.key_frames.iter().map(|kf| &kf.name).collect();

    let mut sections: Vec<PlanSection> = Vec::with_capacity(parsed.sections.len());
    for (si, s) in parsed.sections.into_iter().enumerate() {
        let mut units: Vec<ScriptUnit> = Vec::with_capacity(s.units.len());
        for (ui, u) in s.units.into_iter().enumerate() {
            let valid_unit_frames: Vec<String> = u
                .frames
                .into_iter()
                .filter(|f| valid_frames.contains(f))
                .collect();
            if valid_unit_frames.is_empty() {
                continue; // skip empty units
            }
            let kind = match u.kind.as_str() {
                "title_card" | "instruction" | "filler" => u.kind,
                _ => "instruction".to_string(),
            };
            // Filler + title_card never have text.
            let text = if kind == "filler" || kind == "title_card" {
                None
            } else {
                u.text.filter(|t| !t.trim().is_empty())
            };
            units.push(ScriptUnit {
                id: format!("u{si:02}_{ui:02}"),
                kind,
                frames: valid_unit_frames,
                text,
            });
        }
        if units.is_empty() {
            continue;
        }
        sections.push(PlanSection {
            id: format!("s{si:02}"),
            title: s.title.trim().to_string(),
            overview: s.overview.trim().to_string(),
            units,
        });
    }

    if sections.is_empty() {
        return Err("Plan AI produced no valid sections.".into());
    }

    let plan = Plan {
        version: PLAN_SCHEMA_VERSION,
        sections,
        excluded_frames: Vec::new(),
    };
    let clip_dir = Path::new(&project_dir).join(CLIPS_DIR).join(&clip_id);
    write_plan_file(&clip_dir, &plan)?;
    Ok(plan)
}

#[tauri::command(rename_all = "camelCase")]
fn load_plan(project_dir: String, clip_id: String) -> Result<Option<Plan>, String> {
    let path = Path::new(&project_dir)
        .join(CLIPS_DIR)
        .join(&clip_id)
        .join("plan.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let plan: Plan = serde_json::from_str(&raw)
        .map_err(|e| format!("plan.json is invalid: {e}"))?;
    Ok(Some(plan))
}

/// Save an edited plan back to disk. The UI calls this whenever the user
/// edits a section title / overview / unit text / unit kind / frame
/// membership / section order. Full-document replace — small file so it
/// doesn't matter perf-wise.
#[tauri::command(rename_all = "camelCase")]
fn update_plan(project_dir: String, clip_id: String, plan: Plan) -> Result<(), String> {
    let clip_dir = Path::new(&project_dir).join(CLIPS_DIR).join(&clip_id);
    write_plan_file(&clip_dir, &plan)
}

/// Toggle a frame's "excluded from final video" flag. The frame stays on
/// disk + in scan.json + (potentially) referenced from script_units in
/// plan.json — but render will skip it entirely. This is the "delete a
/// frame I don't want" feature.
#[tauri::command(rename_all = "camelCase")]
fn toggle_frame_excluded(
    project_dir: String,
    clip_id: String,
    frame_name: String,
    excluded: bool,
) -> Result<Plan, String> {
    let mut plan = load_plan(project_dir.clone(), clip_id.clone())?
        .ok_or_else(|| "No plan to update.".to_string())?;
    if excluded {
        if !plan.excluded_frames.contains(&frame_name) {
            plan.excluded_frames.push(frame_name);
        }
    } else {
        plan.excluded_frames.retain(|f| f != &frame_name);
    }
    let clip_dir = Path::new(&project_dir).join(CLIPS_DIR).join(&clip_id);
    write_plan_file(&clip_dir, &plan)?;
    Ok(plan)
}

fn write_plan_file(clip_dir: &Path, plan: &Plan) -> Result<(), String> {
    let path = clip_dir.join("plan.json");
    let tmp = clip_dir.join("plan.json.tmp");
    let pretty = serde_json::to_string_pretty(plan)
        .map_err(|e| format!("Cannot serialize plan: {e}"))?;
    fs::write(&tmp, pretty).map_err(|e| format!("Cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .map_err(|e| format!("Cannot finalize {}: {e}", path.display()))?;
    Ok(())
}

fn build_plan_prompt(
    main_prompt: &str,
    clip_title: &str,
    clip_overview: &str,
    scan: &Scan,
) -> String {
    let mut p = String::new();
    p.push_str(
        "You are writing the script plan for a training video clip. You'll \
         decide how to group the scanned key frames into sections, what each \
         section should be titled, and what the narrator says for each step.\n\n",
    );
    if !main_prompt.trim().is_empty() {
        p.push_str("PROJECT CONTEXT:\n");
        p.push_str(main_prompt.trim());
        p.push_str("\n\n");
    }
    if !clip_title.trim().is_empty() {
        p.push_str("CLIP TITLE: ");
        p.push_str(clip_title.trim());
        p.push('\n');
    }
    if !clip_overview.trim().is_empty() {
        p.push_str("CLIP OVERVIEW: ");
        p.push_str(clip_overview.trim());
        p.push_str("\n\n");
    }
    p.push_str("SCAN OUTPUT — narrative arc the scan stage produced:\n");
    p.push_str(scan.narrative_arc.trim());
    p.push_str("\n\n");

    p.push_str(&format!(
        "KEY FRAMES ({} total, in order):\n",
        scan.key_frames.len()
    ));
    for kf in &scan.key_frames {
        p.push_str("- ");
        p.push_str(&kf.name);
        p.push_str(" [");
        p.push_str(&kf.kind);
        p.push_str("]: ");
        p.push_str(kf.summary.trim());
        if let Some(t) = &kf.title {
            p.push_str(" (title hint: \"");
            p.push_str(t.trim());
            p.push_str("\")");
        }
        p.push('\n');
    }

    p.push_str(
        "\nYOUR JOB:\n\
         1. Group the key frames into SECTIONS. Use section_divider frames \
            as natural chapter breaks. If there are no section_divider \
            frames or only one, infer sections from topic shifts.\n\
         2. For each section: give it a short title (3-6 words), and write \
            a 2-3 sentence overview that the narrator says to introduce it.\n\
         3. Within each section, write ordered SCRIPT UNITS:\n\
            - type=\"title_card\": ONE per section, wraps the section_divider \
              frame(s) if any. No text — the section overview is the audio.\n\
            - type=\"instruction\": narrated step. Each instruction unit \
              has a 'text' field with the imperative-voice narration line. \
              Multiple frames can share one instruction unit when the action \
              spans several frames (e.g. typing into a field across 3 frames).\n\
            - type=\"filler\": frames briefly shown with no narration (e.g. \
              transition frames, near-duplicates). No text needed.\n\
         4. NARRATION STYLE — REQUIRED:\n\
            - Imperative voice — address the viewer directly. Start with a \
              verb (Select, Tap, Open, Enter, Confirm, Choose, Scroll).\n\
            - Reference SPECIFIC UI elements by name (use the labels from \
              the scan summaries).\n\
            - DON'T repeat the app name or form name every step — that's \
              already covered in the overview.\n\
            - DON'T use \"the user\" / \"the operator\" — speak TO the viewer.\n\
            - When on-screen text is ALL CAPS, write it in Title Case.\n\
            - 10-20 words per instruction line.\n\
         5. RULES:\n\
            - Every frame from the input list MUST appear in exactly one \
              unit (don't drop frames; use 'filler' if no narration needed).\n\
            - Frame names MUST match exactly (e.g. \"0007.jpg\").\n\
            - Number of sections is your call — usually 2-6 for a typical \
              training clip.\n\
         \n\
         GOOD example narration lines:\n\
         - \"Tap App Forms from the menu, then select Promo Implementation Form.\"\n\
         - \"Choose 1 in the FF Store Surveyed dropdown if you visited the store.\"\n\
         - \"Confirm by tapping Submit at the bottom of the form.\"\n\
         \n\
         BAD examples to AVOID:\n\
         - \"The user is selecting...\" (observer voice)\n\
         - \"This screen shows the form\" (describes instead of instructs)\n\
         - \"In Tarkie App's Promo Implementation Form, tap the field\" (repeats context)\n\
         \n\
         Respond with JSON only, matching the schema. No code fences, no preamble.",
    );
    p
}

async fn run_thumbnail_script(clip_dir: &Path) -> Result<(), String> {
    use std::io::{BufRead, BufReader};
    let clip_dir_str = clip_dir.to_string_lossy().into_owned();
    let manifest = env!("CARGO_MANIFEST_DIR");
    let script = PathBuf::from(manifest)
        .join("scripts")
        .join("make_thumbnails.py");
    if !script.exists() {
        return Err(format!(
            "Thumbnail script not found at {}",
            script.display()
        ));
    }
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut child = std::process::Command::new(python_path())
            .arg(&script)
            .arg(&clip_dir_str)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Cannot spawn python: {e}"))?;
        let stdout = child.stdout.take().ok_or_else(|| "no stdout".to_string())?;
        let reader = BufReader::new(stdout);
        for _ in reader.lines().flatten() {}
        let status = child
            .wait()
            .map_err(|e| format!("thumbnail wait: {e}"))?;
        if !status.success() {
            return Err(format!("thumbnail script exited {status}"));
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("Thumbnail task panicked: {e}"))?
}

/// Extract frames (or slide images) for a single clip. For MP4/MOV: probe
/// duration, refuse if > 10min, then sample 1 fps as JPEG. For PPTX: hand
/// off to soffice. Both write into clips/<id>/frames/*.jpg. Updates the
/// clip's status to `frames_extracted` and persists the project.
///
/// This is synchronous and may take 10-30s on a 2-min MP4. The UI shows
/// a spinner while it runs (step b2 decision: blocking, not async).
#[tauri::command(rename_all = "camelCase")]
fn extract_frames(project_dir: String, clip_id: String) -> Result<ExtractResult, String> {
    let mut project = load_project(project_dir.clone())?;
    let clip_idx = project
        .clips
        .iter()
        .position(|c| c.id == clip_id)
        .ok_or_else(|| format!("No clip with id {clip_id}"))?;

    // Find the source file inside the clip folder (we copied it in
    // add_clip as "source.<ext>" — but the extension varies).
    let clip_dir = Path::new(&project_dir).join(CLIPS_DIR).join(&clip_id);
    let source = find_clip_source(&clip_dir)?;
    let frames_dir = clip_dir.join(FRAMES_DIR);

    // Clean any previous extraction so re-running doesn't leave stale frames.
    if frames_dir.exists() {
        fs::remove_dir_all(&frames_dir)
            .map_err(|e| format!("Cannot clear frames dir: {e}"))?;
    }
    fs::create_dir_all(&frames_dir)
        .map_err(|e| format!("Cannot create frames dir: {e}"))?;

    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let duration_seconds = match ext.as_str() {
        "mp4" | "mov" => Some(extract_video_frames(&source, &frames_dir)?),
        "pptx" => {
            extract_pptx_slides(&source, &frames_dir)?;
            None
        }
        other => return Err(format!("Unsupported source extension: {other}")),
    };

    let frames = collect_frame_info(&frames_dir, duration_seconds)?;
    if frames.is_empty() {
        return Err("No frames produced. Check that the source is a valid video/PPTX.".into());
    }

    let clip = &mut project.clips[clip_idx];
    clip.duration_seconds = duration_seconds;
    clip.status = ClipStatus::FramesExtracted;
    write_project_file(&project)?;

    Ok(ExtractResult { project, frames })
}

/// List frames for a clip that has already been extracted (clip.status
/// >= FramesExtracted). Used by the UI when re-opening a project to
/// re-populate the thumbnail grid without re-running ffmpeg.
#[tauri::command(rename_all = "camelCase")]
fn list_frames(project_dir: String, clip_id: String) -> Result<Vec<FrameInfo>, String> {
    let project = load_project(project_dir.clone())?;
    let clip = project
        .clips
        .iter()
        .find(|c| c.id == clip_id)
        .ok_or_else(|| format!("No clip with id {clip_id}"))?;
    let frames_dir = Path::new(&project_dir)
        .join(CLIPS_DIR)
        .join(&clip_id)
        .join(FRAMES_DIR);
    if !frames_dir.is_dir() {
        return Ok(Vec::new());
    }
    collect_frame_info(&frames_dir, clip.duration_seconds)
}

/// Generate AI narration for every frame in a clip. Uses Ollama's
/// llama3.2-vision model. For each frame, we compute a perceptual hash
/// (8x8 average hash) and compare against the previous frame's hash. If
/// the two are close enough, we skip the vision call and inherit the
/// earlier frame's text — that's the "smart sampling" from the conversation.
///
/// Rolling context: each vision call sees the main prompt + the last
/// NARRATION_CONTEXT_ENTRIES fresh narrations so the AI can write
/// continuous prose rather than describing every frame in isolation.
///
/// Progress is streamed via Tauri events on the "narration-progress"
/// channel — the React side listens and updates the script panel live.
#[tauri::command(rename_all = "camelCase")]
async fn narrate_clip(
    app: tauri::AppHandle,
    project_dir: String,
    clip_id: String,
) -> Result<Narration, String> {
    use tauri::Emitter;
    let mut project = load_project(project_dir.clone())?;
    let clip_idx = project
        .clips
        .iter()
        .position(|c| c.id == clip_id)
        .ok_or_else(|| format!("No clip with id {clip_id}"))?;
    let clip = &project.clips[clip_idx];
    if clip.status == ClipStatus::Draft {
        return Err("Extract frames first before narrating.".into());
    }

    let frames = list_frames(project_dir.clone(), clip_id.clone())?;
    if frames.is_empty() {
        return Err("No frames found on disk to narrate.".into());
    }

    // 10-min timeout: on RAM-constrained Macs, vision inference under swap
    // pressure can take 2-5 min per frame (vs the ~10s ceiling on Macs that
    // can hold the model fully in RAM). 120s was too aggressive.
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| format!("Cannot build HTTP client: {e}"))?;

    // Cancellation flag — Stop button in the UI flips this via
    // cancel_narration. We poll at the top of each loop iteration AND
    // after each Ollama call returns, so cancellation latency is at most
    // one frame's inference time.
    let cancel = register_narration(&project_dir, &clip_id);

    let mut entries: Vec<NarrationEntry> = Vec::with_capacity(frames.len());
    let mut last_fresh_idx: Option<usize> = None;
    let mut rolling_context: Vec<String> = Vec::new();

    for (i, f) in frames.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            // Save whatever we've done so far + exit cleanly.
            let partial = Narration {
                version: NARRATION_SCHEMA_VERSION,
                entries: entries.clone(),
            };
            let _ = write_narration_file(&project_dir, &clip_id, &partial);
            unregister_narration(&project_dir, &clip_id);
            return Err("Cancelled by user.".into());
        }
        let hash = compute_phash(&f.path)?;

        // Compare to previous frame's hash; if close enough, inherit.
        let inherit_from = match last_fresh_idx {
            Some(prev) => {
                let prev_hash = &entries[prev].hash;
                let distance = hamming_distance(&hash, prev_hash);
                if distance <= PHASH_DISTANCE_THRESHOLD {
                    Some(entries[prev].name.clone())
                } else {
                    None
                }
            }
            None => None,
        };

        if let Some(src_name) = inherit_from {
            // Skip Llama call — same content as the previous fresh frame.
            entries.push(NarrationEntry {
                name: f.name.clone(),
                timestamp_seconds: f.timestamp_seconds,
                hash,
                text: None,
                inherits_from: Some(src_name),
            });
            let _ = app.emit(
                "narration-progress",
                NarrationProgress {
                    clip_id: clip_id.clone(),
                    index: i + 1,
                    total: frames.len(),
                    name: f.name.clone(),
                    text: None,
                    inherited: true,
                },
            );
            continue;
        }

        // Fresh narration via Ollama vision.
        let text = match call_ollama_vision(&http, &project.main_prompt, &rolling_context, &f.path).await {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                return Err(format!("Ollama call failed on frame {}: {e}", f.name));
            }
        };

        rolling_context.push(text.clone());
        if rolling_context.len() > NARRATION_CONTEXT_ENTRIES {
            rolling_context.remove(0);
        }

        entries.push(NarrationEntry {
            name: f.name.clone(),
            timestamp_seconds: f.timestamp_seconds,
            hash,
            text: Some(text.clone()),
            inherits_from: None,
        });
        last_fresh_idx = Some(entries.len() - 1);

        let _ = app.emit(
            "narration-progress",
            NarrationProgress {
                clip_id: clip_id.clone(),
                index: i + 1,
                total: frames.len(),
                name: f.name.clone(),
                text: Some(text),
                inherited: false,
            },
        );

        // Save partial progress periodically so a crash doesn't lose work.
        if (i + 1) % NARRATION_AUTOSAVE_EVERY == 0 {
            let partial = Narration {
                version: NARRATION_SCHEMA_VERSION,
                entries: entries.clone(),
            };
            let _ = write_narration_file(&project_dir, &clip_id, &partial);
        }
    }

    let narration = Narration {
        version: NARRATION_SCHEMA_VERSION,
        entries,
    };
    write_narration_file(&project_dir, &clip_id, &narration)?;

    // Update the clip's status.
    project.clips[clip_idx].status = ClipStatus::Narrated;
    write_project_file(&project)?;
    unregister_narration(&project_dir, &clip_id);

    Ok(narration)
}

/// Generate the opening title text + a per-clip section title for the
/// whole project in a single LLM pass. Reads each clip's narration.json
/// so the AI sees what each clip actually contains, then writes titles
/// that fit the project's main_prompt and the order of clips.
///
/// We use the same vision model as a text-only LLM (no image) — it's
/// already loaded, so no extra RAM cost. Returns the updated Project
/// (titles filled in), which the caller can then edit before render.
#[tauri::command(rename_all = "camelCase")]
async fn generate_titles(project_dir: String) -> Result<Project, String> {
    let mut project = load_project(project_dir.clone())?;
    if project.clips.is_empty() {
        return Err("Add at least one clip before generating titles.".into());
    }

    // Gather a brief excerpt of each clip's narration. We use the first
    // and last fresh narrations as a "what this clip is about" summary.
    let mut clip_summaries: Vec<String> = Vec::new();
    for clip in &project.clips {
        let narration = load_narration(project_dir.clone(), clip.id.clone()).unwrap_or(Narration {
            version: NARRATION_SCHEMA_VERSION,
            entries: Vec::new(),
        });
        let fresh: Vec<&str> = narration
            .entries
            .iter()
            .filter_map(|e| e.text.as_deref())
            .filter(|t| !t.is_empty())
            .collect();
        let summary = if fresh.is_empty() {
            format!("(clip {} has no narration yet)", clip.id)
        } else if fresh.len() <= 3 {
            fresh.join(" ")
        } else {
            format!(
                "{} … {}",
                fresh[0],
                fresh[fresh.len() - 1]
            )
        };
        clip_summaries.push(format!("Clip {}: {}", clip.id, summary));
    }

    let prompt = build_titles_prompt(&project.name, &project.main_prompt, &clip_summaries);

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| format!("Cannot build HTTP client: {e}"))?;

    let body = serde_json::json!({
        "model": OLLAMA_VISION_MODEL,
        "prompt": prompt,
        "stream": false,
        "format": "json",
        "options": {
            "num_predict": 400,
            "temperature": 0.5,
        }
    });

    let resp = http
        .post(OLLAMA_URL)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Cannot reach Ollama at {OLLAMA_URL}: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Ollama returned {status}: {body}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Ollama response not JSON: {e}"))?;
    let raw_text = json
        .get("response")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Ollama response missing 'response' field".to_string())?;

    // Parse the model's JSON. Be lenient: strip code fences if it added them.
    let cleaned = raw_text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let parsed: TitlesResponse = serde_json::from_str(cleaned)
        .map_err(|e| format!("Could not parse titles JSON from model: {e}\nRaw: {cleaned}"))?;

    // Apply the AI's titles, but never clobber a non-empty user edit.
    if project.opening_title_text.trim().is_empty() {
        project.opening_title_text = parsed.opening_title;
    }
    for (i, ai_title) in parsed.clip_titles.iter().enumerate() {
        if let Some(clip) = project.clips.get_mut(i) {
            if clip.title.trim().is_empty() {
                clip.title = ai_title.clone();
            }
        }
    }
    write_project_file(&project)?;
    Ok(project)
}

#[derive(Deserialize)]
struct TitlesResponse {
    opening_title: String,
    clip_titles: Vec<String>,
}

fn build_titles_prompt(project_name: &str, main_prompt: &str, clip_summaries: &[String]) -> String {
    let mut p = String::new();
    p.push_str(
        "You are writing on-screen titles for a training video. Each title \
         appears as a large text card the viewer reads for 2-3 seconds before \
         that section plays.\n\n",
    );
    p.push_str("PROJECT NAME (internal, not shown on screen): ");
    p.push_str(project_name);
    p.push_str("\n\nPROJECT CONTEXT:\n");
    if main_prompt.trim().is_empty() {
        p.push_str("(no context provided)\n");
    } else {
        p.push_str(main_prompt.trim());
        p.push('\n');
    }
    p.push_str("\nCLIPS IN ORDER (narration excerpts):\n");
    for s in clip_summaries {
        p.push_str("- ");
        p.push_str(s);
        p.push('\n');
    }
    p.push_str(
        "\nWrite:\n\
         1. ONE opening title for the whole video — short and welcoming, \
         like a training-video chapter heading (5-10 words).\n\
         2. ONE section title per clip — each describes that section's topic \
         in 3-6 words. Be specific about the workflow step (e.g. \"Time-In\", \
         \"Submitting the Form\") rather than generic (\"Step One\", \"Section\").\n\
         \n\
         Respond with JSON only, exactly this shape:\n\
         {\"opening_title\": \"...\", \"clip_titles\": [\"...\", \"...\"]}\n\
         The clip_titles array must have exactly ",
    );
    p.push_str(&clip_summaries.len().to_string());
    p.push_str(
        " entries, one per clip in order. No prose around the JSON, no code \
         fences, no preamble.",
    );
    p
}

/// Generate a per-clip OVERVIEW narration — 2-3 sentences that introduce
/// what this clip is about and what the operator will learn. This is
/// the "main concept" voiceover that plays during the section title
/// card, so the per-frame narrations can focus on just the step actions.
///
/// Reads all the step narrations from narration.json + main_prompt + the
/// clip title, then asks the LLM to produce one cohesive intro paragraph.
#[tauri::command(rename_all = "camelCase")]
async fn generate_overview(
    project_dir: String,
    clip_id: String,
) -> Result<Project, String> {
    let mut project = load_project(project_dir.clone())?;
    let clip_idx = project
        .clips
        .iter()
        .position(|c| c.id == clip_id)
        .ok_or_else(|| format!("No clip with id {clip_id}"))?;

    // Pull step narrations (only fresh ones, in time order).
    let narration = load_narration(project_dir.clone(), clip_id.clone())?;
    let steps: Vec<String> = narration
        .entries
        .iter()
        .filter_map(|e| e.text.clone())
        .filter(|t| !t.trim().is_empty())
        .collect();
    if steps.is_empty() {
        return Err("Narrate the clip before generating an overview.".into());
    }

    let clip = &project.clips[clip_idx];
    let mut prompt = String::new();
    prompt.push_str(
        "You are writing the OVERVIEW narration for one section of a training \
         video. This is what the narrator says at the START of this section, \
         BEFORE the step-by-step walkthrough begins. Goal: tell the viewer \
         what they're about to learn in this section, in 2-3 sentences.\n\n",
    );
    prompt.push_str("PROJECT CONTEXT:\n");
    if project.main_prompt.trim().is_empty() {
        prompt.push_str("(none)\n");
    } else {
        prompt.push_str(project.main_prompt.trim());
        prompt.push('\n');
    }
    prompt.push_str("\nSECTION TITLE: ");
    if clip.title.trim().is_empty() {
        prompt.push_str("(no title)\n");
    } else {
        prompt.push_str(clip.title.trim());
        prompt.push('\n');
    }
    prompt.push_str("\nSTEPS COVERED IN THIS SECTION:\n");
    for s in &steps {
        prompt.push_str("- ");
        prompt.push_str(s);
        prompt.push('\n');
    }
    prompt.push_str(
        "\nWrite the overview narration. STRICT RULES:\n\
         - 2-3 sentences, ~40 words max.\n\
         - Frame it as \"In this section…\" or similar — viewer is about to watch.\n\
         - Summarize the GOAL of the section (what they'll be able to do).\n\
         - Do NOT enumerate steps. Just the big picture.\n\
         - Do NOT use filler like \"this video will show\".\n\
         - Output only the narration text. No JSON, no labels.",
    );

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("Cannot build HTTP client: {e}"))?;
    let body = serde_json::json!({
        "model": OLLAMA_VISION_MODEL,
        "prompt": prompt,
        "stream": false,
        "options": {
            "num_predict": 120,
            "temperature": 0.5,
        }
    });
    let resp = http
        .post(OLLAMA_URL)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Cannot reach Ollama: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Ollama returned {status}: {body}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Ollama response not JSON: {e}"))?;
    let overview = json
        .get("response")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Ollama response missing 'response' field".to_string())?
        .trim()
        .to_string();

    project.clips[clip_idx].overview = overview;
    write_project_file(&project)?;
    Ok(project)
}

/// Cancel an in-flight narrate_clip. Sets the per-clip flag the loop
/// polls; the loop saves partial progress + returns an error. The
/// already-narrated frames are kept on disk so the user can resume by
/// clicking "Narrate" again (currently restarts from frame 1; resume is
/// a Phase 2 enhancement).
#[tauri::command(rename_all = "camelCase")]
fn cancel_narration(project_dir: String, clip_id: String) -> Result<(), String> {
    let key = cancel_key(&project_dir, &clip_id);
    if let Some(flag) = narration_cancels().lock().unwrap().get(&key) {
        flag.store(true, Ordering::Relaxed);
        Ok(())
    } else {
        Err("No active narration to cancel for that clip.".into())
    }
}

#[derive(Clone, Serialize)]
struct TtsProgress {
    clip_id: String,
    index: usize,
    total: usize,
    name: String,
    /// Stage: "loading" (importing torch + loading model — slow first
    /// call), "loaded" (model ready), "progress" (per-frame WAV done),
    /// "done" (all WAVs written).
    stage: String,
    duration_seconds: Option<f64>,
}

/// Generate per-frame TTS audio for a clip. Calls the Python helper
/// (StyleTTS 2) as a subprocess and streams its stdout JSON lines as
/// Tauri events on "tts-progress". Writes one WAV per fresh narration
/// entry into clips/<id>/audio/, plus an audio/manifest.json with per-
/// entry duration for step (f) to use as frame timing.
///
/// Honest known-limit: StyleTTS 2's first call is slow because PyTorch
/// + model checkpoints take ~5-15s to load. After the first frame the
/// loop is ~1-2 sec per WAV on this Mac.
#[tauri::command(rename_all = "camelCase")]
async fn generate_audio(
    app: tauri::AppHandle,
    project_dir: String,
    clip_id: String,
) -> Result<(), String> {
    use tauri::Emitter;
    use std::io::{BufRead, BufReader};

    let mut project = load_project(project_dir.clone())?;
    let clip_idx = project
        .clips
        .iter()
        .position(|c| c.id == clip_id)
        .ok_or_else(|| format!("No clip with id {clip_id}"))?;
    let clip = &project.clips[clip_idx];
    if clip.status == ClipStatus::Draft || clip.status == ClipStatus::FramesExtracted {
        return Err("Narrate this clip before generating audio.".into());
    }

    let clip_dir = Path::new(&project_dir).join(CLIPS_DIR).join(&clip_id);
    let script_path = tts_script_path()?;

    // Run the Python helper. We use std::process::Command (not the async
    // tokio::process flavor) so we can use BufReader::lines synchronously
    // in a blocking thread — simpler than wiring up async stream readers.
    let clip_id_owned = clip_id.clone();
    let app_clone = app.clone();
    let clip_dir_str = clip_dir.to_string_lossy().into_owned();

    let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut child = std::process::Command::new(python_path())
            .arg(&script_path)
            .arg(&clip_dir_str)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Cannot spawn python: {e}"))?;

        let stdout = child.stdout.take().ok_or_else(|| "no stdout".to_string())?;
        let stderr = child.stderr.take().ok_or_else(|| "no stderr".to_string())?;

        // Drain stderr in a side thread so it doesn't fill its pipe and
        // block the python script.
        let stderr_collector = std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            let mut buf = String::new();
            for line in reader.lines().flatten() {
                buf.push_str(&line);
                buf.push('\n');
                if buf.len() > 8192 {
                    // Keep last 8KB only.
                    let cut = buf.len() - 8192;
                    buf = buf[cut..].to_string();
                }
            }
            buf
        });

        let reader = BufReader::new(stdout);
        let mut last_error: Option<String> = None;
        for line in reader.lines().flatten() {
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let kind = msg.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "loading" | "loaded" | "progress" | "done" => {
                    let progress = TtsProgress {
                        clip_id: clip_id_owned.clone(),
                        index: msg.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                        total: msg.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                        name: msg
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        stage: kind.to_string(),
                        duration_seconds: msg.get("duration_seconds").and_then(|v| v.as_f64()),
                    };
                    let _ = app_clone.emit("tts-progress", progress);
                }
                "error" => {
                    last_error = Some(
                        msg.get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error")
                            .to_string(),
                    );
                }
                _ => {}
            }
        }

        let status = child.wait().map_err(|e| format!("python wait failed: {e}"))?;
        let stderr_text = stderr_collector.join().unwrap_or_default();
        if !status.success() {
            let detail = last_error.unwrap_or_else(|| stderr_text.trim().to_string());
            return Err(format!("TTS script exited {status}: {detail}"));
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("TTS task panicked: {e}"))?;

    result?;

    // Update clip status on success.
    project.clips[clip_idx].status = ClipStatus::AudioReady;
    write_project_file(&project)?;
    Ok(())
}

/// Read the audio manifest written by tts_styletts2.py. Used by step (f)
/// to know frame timing in the final render. Returns an empty manifest
/// if audio hasn't been generated yet.
#[tauri::command(rename_all = "camelCase")]
fn load_audio_manifest(project_dir: String, clip_id: String) -> Result<AudioManifest, String> {
    let path = Path::new(&project_dir)
        .join(CLIPS_DIR)
        .join(&clip_id)
        .join("audio")
        .join("manifest.json");
    if !path.exists() {
        return Ok(AudioManifest {
            version: 1,
            entries: Vec::new(),
        });
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("audio manifest invalid: {e}"))
}

#[derive(Serialize, Deserialize, Clone)]
struct AudioManifest {
    version: u32,
    entries: Vec<AudioManifestEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct AudioManifestEntry {
    frame_name: String,
    audio_name: String,
    timestamp_seconds: Option<f64>,
    duration_seconds: f64,
}

// ---------- Step (f): final video render ----------

/// Render dimensions are derived per-clip from the source aspect ratio
/// rather than forced into a standard 1080×1920 / 1920×1080. The output
/// canvas matches the source frame's width + a caption strip below the
/// frame. No letterbox/pillarbox bars.
///
/// Caption strip height is ~9% of the source frame's HEIGHT, so the
/// frame keeps ~92% of the vertical space. Tuned empirically: 17.5%
/// (initial value) was too tall — captions felt like billboards.
/// 9% gives ~140px of strip on a 1600px-tall mobile frame, room for
/// 2 short lines at ~32px font.
const CAPTION_STRIP_RATIO: f64 = 0.09;
/// Minimum frame width — if the source is tiny we upscale to this so the
/// output is still watchable. Output width then = max(source_w, this).
const MIN_FRAME_WIDTH: u32 = 720;
const RENDER_FPS: u32 = 30;
/// Tarkie brand blue (#044be4). Used for the title card background and
/// the caption box behind the bottom-third text. ffmpeg color strings
/// use `0xRRGGBB`; for Pillow we emit the same value with `#` prefix.
const BRAND_BLUE_FFMPEG: &str = "0x044be4";
const TITLE_DURATION_OPENING: f64 = 3.0;
const TITLE_DURATION_SECTION: f64 = 2.5;
const TRANSITION_DURATION: f64 = 0.5;

#[derive(Clone, Serialize)]
struct RenderProgress {
    stage: String,
    detail: String,
    /// 0.0–1.0; UI shows a progress bar. -1 = indeterminate.
    fraction: f64,
}

/// Render the final MP4 by assembling title cards, per-clip narrated
/// segments, and crossfade transitions via ffmpeg. Writes to
/// {project_dir}/output.mp4. Streams progress via "render-progress" events.
///
/// Pipeline overview (one big concat job):
///   1. Pick output resolution from project.language?... no, from clip
///      aspect (Phase 1: vertical 1080x1920 default; override available
///      via a hidden setting later).
///   2. For each clip with audio_ready or rendered status:
///       a. Build a per-frame segment: image + caption box + duration
///          equal to its WAV. Background = scaled+letterboxed frame
///          on Tarkie blue.
///       b. Concat the clip's segments into one clip.mp4 (intermediate).
///       c. Mux the clip's audio.wav as the soundtrack.
///   3. Build opening title card and per-clip section cards.
///   4. Concat: opening_title → section1 → clip1 → section2 → clip2 → ...
///      with xfade transitions between sections.
///   5. Write output.mp4 H.264, AAC audio.
#[tauri::command(rename_all = "camelCase")]
async fn render_video(
    app: tauri::AppHandle,
    project_dir: String,
) -> Result<(), String> {
    use tauri::Emitter;

    let project = load_project(project_dir.clone())?;
    if project.clips.is_empty() {
        return Err("Add at least one clip before rendering.".into());
    }
    let ready_clips: Vec<&Clip> = project
        .clips
        .iter()
        .filter(|c| c.status == ClipStatus::AudioReady || c.status == ClipStatus::Rendered)
        .collect();
    if ready_clips.is_empty() {
        return Err("No clips have audio yet. Generate audio first.".into());
    }

    // Decide render layout from the first clip's source dimensions.
    // The layout includes the canvas (full output), the frame zone (top),
    // and the caption strip (below the frame). No letterbox.
    let layout = detect_render_layout(&project_dir, &ready_clips[0])?;

    let work_dir = Path::new(&project_dir).join(".render");
    if work_dir.exists() {
        fs::remove_dir_all(&work_dir).map_err(|e| format!("Cannot clean work dir: {e}"))?;
    }
    fs::create_dir_all(&work_dir).map_err(|e| format!("Cannot create work dir: {e}"))?;

    let app_clone = app.clone();
    let emit_progress = move |stage: &str, detail: &str, fraction: f64| {
        let _ = app_clone.emit(
            "render-progress",
            RenderProgress {
                stage: stage.to_string(),
                detail: detail.to_string(),
                fraction,
            },
        );
    };

    let total_clips = ready_clips.len();
    let mut segment_paths: Vec<PathBuf> = Vec::new();

    // 1. Opening title card (only if there's text).
    if !project.opening_title_text.trim().is_empty() {
        emit_progress("title", "Rendering opening title", 0.0);
        let title_path = work_dir.join("00_opening_title.mp4");
        render_title_card(
            &project.opening_title_text,
            "",
            TITLE_DURATION_OPENING,
            None,
            &layout,
            &title_path,
        )?;
        segment_paths.push(title_path);
    }

    // 2. Per-clip: section title card + the narrated clip video.
    for (i, clip) in ready_clips.iter().enumerate() {
        let clip_label = format!("Clip {}/{} ({})", i + 1, total_clips, clip.id);

        // Section title card before the clip (only if title is set).
        if !clip.title.trim().is_empty() {
            emit_progress(
                "section_title",
                &format!("Section title — {clip_label}"),
                (i as f64) / (total_clips as f64),
            );
            let st_path = work_dir.join(format!("{:02}_section_{}.mp4", i + 1, clip.id));

            // Synthesize the clip's overview narration if present. Caching:
            // we save it as clips/<id>/overview.wav so subsequent renders
            // skip the TTS step if the overview text hasn't changed.
            let overview_audio = if !clip.overview.trim().is_empty() {
                let clip_dir = Path::new(&project_dir).join(CLIPS_DIR).join(&clip.id);
                let wav_path = clip_dir.join("overview.wav");
                let txt_path = clip_dir.join("overview.txt");
                let needs_resynth = match fs::read_to_string(&txt_path) {
                    Ok(prev) => prev.trim() != clip.overview.trim() || !wav_path.exists(),
                    Err(_) => true,
                };
                if needs_resynth {
                    emit_progress(
                        "overview_audio",
                        &format!("Synthesizing overview — {clip_label}"),
                        (i as f64) / (total_clips as f64),
                    );
                    synthesize_overview_audio(&clip.overview, &wav_path).await?;
                    let _ = fs::write(&txt_path, clip.overview.trim());
                }
                Some(wav_path)
            } else {
                None
            };

            render_title_card(
                &clip.title,
                &format!("Section {}", i + 1),
                TITLE_DURATION_SECTION,
                overview_audio.as_deref(),
                &layout,
                &st_path,
            )?;
            segment_paths.push(st_path);
        }

        // The narrated clip video.
        emit_progress(
            "clip",
            &format!("Rendering {clip_label}"),
            (i as f64 + 0.5) / (total_clips as f64),
        );
        let clip_path = work_dir.join(format!("{:02}_clip_{}.mp4", i + 1, clip.id));
        render_clip_video(&project_dir, clip, &layout, &clip_path)?;
        segment_paths.push(clip_path);
    }

    // 3. Concat all segments into the final output, with xfade transitions.
    emit_progress("concat", "Concatenating segments", 0.9);
    let output_path = Path::new(&project_dir).join("output.mp4");
    concat_segments_with_crossfade(&segment_paths, &output_path, &layout)?;

    emit_progress("done", "Done", 1.0);

    // Update clip statuses to Rendered. We update the project file in place.
    let mut project = load_project(project_dir.clone())?;
    for clip in project.clips.iter_mut() {
        if clip.status == ClipStatus::AudioReady {
            clip.status = ClipStatus::Rendered;
        }
    }
    write_project_file(&project)?;

    Ok(())
}

/// A render layout describes the output canvas dimensions and where the
/// frame + caption strip sit inside it. The caption is its own zone
/// BELOW the frame — no letterbox/pillarbox bars.
#[derive(Clone, Copy)]
struct RenderLayout {
    canvas_w: u32,
    canvas_h: u32,
    frame_w: u32,
    frame_h: u32,
    /// Height of the caption strip below the frame.
    strip_h: u32,
}

/// Derive a render layout from the first clip's source dimensions.
/// Output canvas = source_w × (source_h + caption_strip). If the source
/// is narrower than MIN_FRAME_WIDTH, both frame and strip scale up
/// proportionally. PPTX sources without a source video (clip duration
/// is None) fall back to a 1080-wide vertical-ish layout.
fn detect_render_layout(project_dir: &str, first_clip: &Clip) -> Result<RenderLayout, String> {
    let clip_dir = Path::new(project_dir).join(CLIPS_DIR).join(&first_clip.id);
    // ffprobe the source's dimensions.
    let source = find_clip_source(&clip_dir).ok();
    let (src_w, src_h) = if let Some(s) = source.as_ref() {
        let output = Command::new(ffprobe_path())
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "csv=p=0:s=x",
            ])
            .arg(s)
            .output();
        if let Ok(out) = output {
            let dims = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let mut parts = dims.split('x');
            let w: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(720);
            let h: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1280);
            (w, h)
        } else {
            (720, 1280) // sensible mobile default
        }
    } else {
        (1080, 1080) // PPTX or other — square-ish fallback
    };

    // Ensure even numbers — H.264 requires even dimensions.
    let frame_w = (src_w.max(MIN_FRAME_WIDTH) / 2) * 2;
    // If we upscaled the width, scale height proportionally too.
    let frame_h = if src_w < MIN_FRAME_WIDTH {
        let scale = (frame_w as f64) / (src_w as f64);
        (((src_h as f64) * scale) as u32 / 2) * 2
    } else {
        (src_h / 2) * 2
    };
    let strip_h = ((frame_h as f64 * CAPTION_STRIP_RATIO) as u32 / 2) * 2;
    let canvas_w = frame_w;
    let canvas_h = frame_h + strip_h;

    Ok(RenderLayout {
        canvas_w,
        canvas_h,
        frame_w,
        frame_h,
        strip_h,
    })
}

/// Build a single title card video.
///
/// Architecture: Pillow renders the title text as a PNG (we can't use
/// ffmpeg drawtext because brew's ffmpeg ships without libfreetype). Then
/// ffmpeg loops that PNG for the duration, applies a fade-in/fade-out
/// filter, and mixes in silent audio. Output is at RENDER_FPS.
/// Render a title card. If `narration_audio` is Some, the card's duration
/// becomes the audio length (+ a 0.5s pad on each side) and the audio
/// plays during the card. If None, the card is silent at `duration`.
fn render_title_card(
    main_text: &str,
    subtitle: &str,
    duration: f64,
    narration_audio: Option<&Path>,
    layout: &RenderLayout,
    out_path: &Path,
) -> Result<(), String> {
    // 1. Render the title PNG via the Pillow helper.
    let png_path = out_path.with_extension("png");
    let spec = serde_json::json!({
        "mode": "title_card",
        "main_text": main_text,
        "subtitle": subtitle,
        "width": layout.canvas_w,
        "height": layout.canvas_h,
        "bg_color": "#044be4",
        "text_color": "#ffffff",
    });
    render_text_png(&spec, &png_path)?;

    // Resolve final card duration: if we have narration audio, the card
    // lasts the audio's length plus a 0.5s tail (gives the voice time to
    // breathe before the crossfade out).
    let actual_duration = if let Some(aud) = narration_audio {
        let audio_dur = probe_audio_duration(aud).unwrap_or(duration);
        audio_dur + 0.5
    } else {
        duration
    };

    let fade_dur = 0.5_f64.min(actual_duration / 3.0);
    let fade_out_start = (actual_duration - fade_dur).max(0.0);
    let filter = format!(
        "fade=t=in:st=0:d={fd},fade=t=out:st={fo}:d={fd},format=yuv420p",
        fd = fade_dur,
        fo = fade_out_start
    );

    let mut cmd = Command::new(ffmpeg_path());
    cmd.args(["-y", "-hide_banner", "-loglevel", "error"]);
    cmd.args(["-loop", "1", "-t", &actual_duration.to_string(), "-i"])
        .arg(&png_path);

    if let Some(aud) = narration_audio {
        cmd.args(["-i"]).arg(aud);
    } else {
        cmd.args([
            "-f",
            "lavfi",
            "-t",
            &actual_duration.to_string(),
            "-i",
            "anullsrc=channel_layout=stereo:sample_rate=24000",
        ]);
    }

    cmd.args([
        "-vf",
        &filter,
        "-r",
        &RENDER_FPS.to_string(),
        "-c:v",
        "libx264",
        "-preset",
        "fast",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
        "-shortest",
    ])
    .arg(out_path);

    let status = cmd
        .status()
        .map_err(|e| format!("ffmpeg title-card failed: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg title-card exited {status}"));
    }
    let _ = fs::remove_file(&png_path);
    Ok(())
}

/// Normalize ALL-CAPS words longer than 3 characters to Title Case so
/// the TTS engine reads them as words instead of spelling out letter by
/// letter. Mirror of normalize_caps in tts_styletts2.py — kept in sync.
fn normalize_caps_for_tts(text: &str) -> String {
    // Short acronyms and known brand/UI terms that should stay all-caps
    // (TTS will spell them out letter-by-letter, which is correct for these).
    const ACRONYM_KEEP: &[&str] = &[
        "AI", "UI", "UX", "ID", "OK", "PIN", "MCS", "FF", "FSM", "CC", "SO",
        "BPI", "PNB", "HSBC", "RCBC", "BSP", "AMII", "CST", "OS",
    ];
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut word = String::new();
    while let Some(c) = chars.next() {
        if c.is_alphabetic() || c == '\'' || c == '-' {
            word.push(c);
        } else {
            if !word.is_empty() {
                out.push_str(&transform_caps_word(&word, ACRONYM_KEEP));
                word.clear();
            }
            out.push(c);
        }
    }
    if !word.is_empty() {
        out.push_str(&transform_caps_word(&word, ACRONYM_KEEP));
    }
    out
}

fn transform_caps_word(word: &str, acronym_keep: &[&str]) -> String {
    // Already has any lowercase → leave alone (it's not all-caps).
    if word.chars().any(|c| c.is_lowercase()) {
        return word.to_string();
    }
    let alpha_only: String = word.chars().filter(|c| c.is_alphabetic()).collect();
    if alpha_only.chars().count() < 2 {
        return word.to_string(); // single letter like "I" or "A"
    }
    if acronym_keep
        .iter()
        .any(|k| k.eq_ignore_ascii_case(&alpha_only))
    {
        return word.to_string();
    }
    // Title-case each letter run separated by non-letters (handles "TOP-LEFT").
    let mut out = String::with_capacity(word.len());
    let mut at_word_start = true;
    for c in word.chars() {
        if c.is_alphabetic() {
            if at_word_start {
                out.push(c);
                at_word_start = false;
            } else {
                for lc in c.to_lowercase() {
                    out.push(lc);
                }
            }
        } else {
            out.push(c);
            at_word_start = true;
        }
    }
    out
}

/// Synthesize an overview narration WAV via StyleTTS 2. Used during
/// render to produce voiceover for section title cards.
async fn synthesize_overview_audio(text: &str, out_wav: &Path) -> Result<(), String> {
    use std::io::{BufRead, BufReader};
    let text_owned = normalize_caps_for_tts(text);
    let out_owned = out_wav.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let inline = format!(
            "import sys, json, warnings\n\
             warnings.filterwarnings('ignore')\n\
             from styletts2 import tts as styletts2_tts\n\
             my_tts = styletts2_tts.StyleTTS2()\n\
             my_tts.inference({txt:?}, output_wav_file={out:?})\n\
             print(json.dumps({{'ok': True}}))\n",
            txt = text_owned.trim(),
            out = out_owned.to_string_lossy().into_owned()
        );
        let mut child = std::process::Command::new(python_path())
            .args(["-c", &inline])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Cannot spawn python: {e}"))?;
        let stdout = child.stdout.take().ok_or_else(|| "no stdout".to_string())?;
        let reader = BufReader::new(stdout);
        for _ in reader.lines().flatten() {}
        let status = child.wait().map_err(|e| format!("python wait: {e}"))?;
        if !status.success() {
            return Err("Overview TTS failed".into());
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("Overview task panicked: {e}"))??;
    Ok(())
}

/// Quick ffprobe for an audio file's duration.
fn probe_audio_duration(path: &Path) -> Result<f64, String> {
    let out = Command::new(ffprobe_path())
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe failed: {e}"))?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    s.parse::<f64>().map_err(|e| format!("Non-numeric: {e}"))
}

/// Render one clip as a single MP4: each unique frame held for its
/// audio's duration, captioned with the narration text. Audio track is
/// concatenated WAVs in order. Output is `out_path`.
fn render_clip_video(
    project_dir: &str,
    clip: &Clip,
    layout: &RenderLayout,
    out_path: &Path,
) -> Result<(), String> {
    let clip_dir = Path::new(project_dir).join(CLIPS_DIR).join(&clip.id);
    let audio_dir = clip_dir.join("audio");

    let manifest = load_audio_manifest(project_dir.to_string(), clip.id.clone())?;
    if manifest.entries.is_empty() {
        return Err(format!(
            "Clip {} has no audio manifest. Generate audio first.",
            clip.id
        ));
    }

    let narration = load_narration(project_dir.to_string(), clip.id.clone())?;

    let frames_dir = clip_dir.join(FRAMES_DIR);
    let segment_dir = clip_dir.join(".render_segments");
    if segment_dir.exists() {
        fs::remove_dir_all(&segment_dir).map_err(|e| format!("Cannot clean segments: {e}"))?;
    }
    fs::create_dir_all(&segment_dir).map_err(|e| format!("Cannot create segments: {e}"))?;

    let mut segment_files: Vec<PathBuf> = Vec::new();

    for (i, entry) in manifest.entries.iter().enumerate() {
        let frame_path = frames_dir.join(&entry.frame_name);
        let audio_path = audio_dir.join(&entry.audio_name);
        if !frame_path.exists() || !audio_path.exists() {
            return Err(format!(
                "Missing files for {}: frame={} audio={}",
                entry.frame_name,
                frame_path.display(),
                audio_path.display()
            ));
        }

        // Pull the narration text for this frame for the caption.
        let caption = narration
            .entries
            .iter()
            .find(|e| e.name == entry.frame_name)
            .and_then(|e| e.text.as_ref())
            .map(|s| s.to_string())
            .unwrap_or_default();

        let seg_path = segment_dir.join(format!("{:04}.mp4", i));
        render_frame_segment(
            &frame_path,
            &audio_path,
            entry.duration_seconds,
            &caption,
            layout,
            &seg_path,
        )?;
        segment_files.push(seg_path);
    }

    // Concat the segments with the ffmpeg concat demuxer.
    let list_path = segment_dir.join("concat_list.txt");
    let list_body: String = segment_files
        .iter()
        .map(|p| format!("file '{}'", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&list_path, list_body).map_err(|e| format!("Cannot write concat list: {e}"))?;

    let status = Command::new(ffmpeg_path())
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&list_path)
        .args(["-c", "copy"])
        .arg(out_path)
        .status()
        .map_err(|e| format!("ffmpeg concat failed: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg concat exited {status}"));
    }
    Ok(())
}

/// Render one frame as a video segment with caption + audio.
///
/// Layout (per Phase 1.5a): the frame occupies the TOP of the output
/// canvas, scaled to fit the frame zone exactly with no letterbox bars.
/// The caption sits in its OWN strip below the frame — a separate zone.
/// Audio comes from the per-frame WAV.
///
/// Phase 1.5c — synced caption chunks: instead of showing the full
/// narration text for the whole frame duration (which dwarfs anything
/// you can read), we split the narration into ~6-word chunks and show
/// each one in sequence for an equal slice of the audio duration. This
/// approximates the feel of subtitles synced to speech.
fn render_frame_segment(
    frame_path: &Path,
    audio_path: &Path,
    duration: f64,
    caption: &str,
    layout: &RenderLayout,
    out_path: &Path,
) -> Result<(), String> {
    // 1. Split the caption into chunks (~6 words per chunk so each one
    //    fits comfortably on 1-2 lines of the small caption strip).
    //    Each chunk gets an equal-time slice of the frame duration.
    let chunks = chunk_caption(caption, 6);
    let chunk_duration = if chunks.is_empty() {
        duration
    } else {
        duration / chunks.len() as f64
    };

    // 2. Render one strip PNG per chunk.
    let mut chunk_pngs: Vec<PathBuf> = Vec::new();
    let stem = out_path.file_stem().and_then(|s| s.to_str()).unwrap_or("seg");
    let parent = out_path.parent().unwrap_or_else(|| Path::new("."));
    for (i, chunk_text) in chunks.iter().enumerate() {
        let png = parent.join(format!("{stem}.caption_{i:02}.png"));
        let spec = serde_json::json!({
            "mode": "caption_strip",
            "text": chunk_text,
            "width": layout.canvas_w,
            "height": layout.strip_h,
            "bg_color": "#044be4",
            "text_color": "#ffffff",
        });
        render_text_png(&spec, &png)?;
        chunk_pngs.push(png);
    }
    // If there are no chunks (empty narration), render one empty strip.
    if chunk_pngs.is_empty() {
        let png = parent.join(format!("{stem}.caption_00.png"));
        let spec = serde_json::json!({
            "mode": "caption_strip",
            "text": "",
            "width": layout.canvas_w,
            "height": layout.strip_h,
            "bg_color": "#044be4",
            "text_color": "#ffffff",
        });
        render_text_png(&spec, &png)?;
        chunk_pngs.push(png);
    }

    // 3. Build the ffmpeg filter:
    //    - input 0: looped frame image (becomes the top zone)
    //    - input 1: audio WAV
    //    - inputs 2..: caption-strip PNGs (one per chunk)
    //
    //    Scale + pad the frame to the full canvas, then overlay each
    //    chunk PNG at y=frame_h with an `enable='between(t,a,b)'` time
    //    gate. Chunks switch instantly at the gate boundaries — good
    //    enough for "subtitled" feel.
    let mut filter = format!(
        "[0:v]scale={fw}:{fh}:force_original_aspect_ratio=decrease,\
         pad={cw}:{ch}:0:0:color={bg}[base]",
        fw = layout.frame_w,
        fh = layout.frame_h,
        cw = layout.canvas_w,
        ch = layout.canvas_h,
        bg = BRAND_BLUE_FFMPEG
    );

    let mut last_label = "base".to_string();
    for (i, _png) in chunk_pngs.iter().enumerate() {
        let start = (i as f64) * chunk_duration;
        // Last chunk extends to the very end (avoid floating-point gaps).
        let end = if i + 1 == chunk_pngs.len() {
            duration + 0.5
        } else {
            (i as f64 + 1.0) * chunk_duration
        };
        let input_idx = 2 + i; // 0 = frame, 1 = audio, 2+ = caption PNGs
        let new_label = format!("v{i}");
        filter.push_str(&format!(
            ";[{prev}][{idx}:v]overlay=0:{fh}:enable='between(t,{a},{b})'[{out_lbl}]",
            prev = last_label,
            idx = input_idx,
            fh = layout.frame_h,
            a = start,
            b = end,
            out_lbl = new_label
        ));
        last_label = new_label;
    }
    // The final label becomes the [v] output mapping.
    let final_v = last_label;

    // Build the ffmpeg command. We need to pass all caption PNGs as
    // additional `-loop 1 -t <dur> -i <png>` inputs after the frame
    // and audio inputs.
    let mut cmd = Command::new(ffmpeg_path());
    cmd.args(["-y", "-hide_banner", "-loglevel", "error"]);
    cmd.args(["-loop", "1", "-t", &duration.to_string(), "-i"])
        .arg(frame_path);
    cmd.args(["-i"]).arg(audio_path);
    for png in &chunk_pngs {
        cmd.args(["-loop", "1", "-t", &duration.to_string(), "-i"]).arg(png);
    }
    cmd.args([
        "-filter_complex",
        &filter,
        "-map",
        &format!("[{final_v}]"),
        "-map",
        "1:a",
        "-r",
        &RENDER_FPS.to_string(),
        "-c:v",
        "libx264",
        "-preset",
        "fast",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
        "-shortest",
    ])
    .arg(out_path);

    let status = cmd
        .status()
        .map_err(|e| format!("ffmpeg frame-segment failed: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg frame-segment exited {status}"));
    }
    for png in &chunk_pngs {
        let _ = fs::remove_file(png);
    }
    Ok(())
}

/// Split a narration string into bite-sized caption chunks suitable for
/// subtitle-style display. Each chunk is ~max_words long; we keep
/// sentences together when possible and break on sentence boundaries
/// before falling back to word-count splits.
fn chunk_caption(text: &str, max_words: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }

    // Pre-split on sentence-ending punctuation so caption breaks align
    // with natural speech pauses.
    let sentences: Vec<&str> = text
        .split_inclusive(|c: char| c == '.' || c == '!' || c == '?')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    let mut chunks: Vec<String> = Vec::new();
    for sentence in sentences {
        let words: Vec<&str> = sentence.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        if words.len() <= max_words {
            chunks.push(words.join(" "));
        } else {
            // Sentence too long for one chunk — split it into windows.
            let mut i = 0;
            while i < words.len() {
                let end = (i + max_words).min(words.len());
                chunks.push(words[i..end].join(" "));
                i = end;
            }
        }
    }
    chunks
}

/// Call the Pillow helper to render a text PNG. Spec is a JSON value with
/// either mode="title_card" or mode="caption" and the relevant params.
fn render_text_png(spec: &serde_json::Value, out_png: &Path) -> Result<(), String> {
    let script = text_script_path()?;
    // Write the spec JSON to a tempfile so we don't have to escape it
    // on the command line (filenames can contain special chars, etc.).
    let spec_path = out_png.with_extension("spec.json");
    let spec_str = serde_json::to_string(spec)
        .map_err(|e| format!("Cannot serialize text spec: {e}"))?;
    fs::write(&spec_path, spec_str)
        .map_err(|e| format!("Cannot write text spec: {e}"))?;

    let output = Command::new(python_path())
        .arg(&script)
        .arg(&spec_path)
        .arg(out_png)
        .output()
        .map_err(|e| format!("Cannot run text renderer: {e}"))?;
    let _ = fs::remove_file(&spec_path);
    if !output.status.success() {
        return Err(format!(
            "text renderer exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn text_script_path() -> Result<PathBuf, String> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let script = PathBuf::from(manifest)
        .join("scripts")
        .join("render_text.py");
    if !script.exists() {
        return Err(format!("Text renderer not found at {}", script.display()));
    }
    Ok(script)
}

/// Concatenate the full segment list with crossfade transitions between
/// each. Uses ffmpeg's `xfade` for visual transitions and `acrossfade`
/// for audio, both with TRANSITION_DURATION overlap.
fn concat_segments_with_crossfade(
    segments: &[PathBuf],
    out_path: &Path,
    layout: &RenderLayout,
) -> Result<(), String> {
    let out_w = layout.canvas_w;
    let out_h = layout.canvas_h;
    if segments.is_empty() {
        return Err("No segments to concat".into());
    }
    if segments.len() == 1 {
        // Just copy the single segment.
        fs::copy(&segments[0], out_path)
            .map_err(|e| format!("Cannot copy single segment: {e}"))?;
        return Ok(());
    }

    // Probe each segment's duration so xfade knows where to place transitions.
    let mut durations: Vec<f64> = Vec::new();
    for seg in segments {
        let out = Command::new(ffprobe_path())
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(seg)
            .output()
            .map_err(|e| format!("ffprobe failed: {e}"))?;
        let d: f64 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(1.0);
        durations.push(d);
    }

    // Build the filter_complex: chain xfade between each consecutive pair.
    // Each segment becomes [vN] (video) and [aN] (audio). We chain:
    //   [v0][v1]xfade=...:offset=d0-T → [vx1]
    //   [vx1][v2]xfade=...:offset=d0+d1-2T → [vx2]
    // and similarly for audio with acrossfade.
    let mut input_args: Vec<String> = Vec::new();
    for seg in segments {
        input_args.push("-i".to_string());
        input_args.push(seg.to_string_lossy().into_owned());
    }

    let mut filter = String::new();
    // Normalize each segment to the target size + framerate so xfade works.
    for i in 0..segments.len() {
        filter.push_str(&format!(
            "[{i}:v]scale={w}:{h}:force_original_aspect_ratio=decrease,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color={bg},\
             setsar=1,fps={fps},format=yuv420p[v{i}];",
            i = i,
            w = out_w,
            h = out_h,
            bg = BRAND_BLUE_FFMPEG,
            fps = RENDER_FPS
        ));
    }

    // Video xfade chain.
    let mut last_label = "v0".to_string();
    let mut acc_duration = durations[0];
    for i in 1..segments.len() {
        let offset = acc_duration - TRANSITION_DURATION;
        let out_label = format!("vx{i}");
        filter.push_str(&format!(
            "[{last}][v{i}]xfade=transition=fade:duration={dur}:offset={off}[{out_lbl}];",
            last = last_label,
            i = i,
            dur = TRANSITION_DURATION,
            off = offset,
            out_lbl = out_label
        ));
        last_label = out_label;
        acc_duration += durations[i] - TRANSITION_DURATION;
    }
    let final_v = last_label.clone();

    // Audio acrossfade chain.
    let mut last_alabel = "0:a".to_string();
    for i in 1..segments.len() {
        let out_label = format!("ax{i}");
        filter.push_str(&format!(
            "[{last}][{i}:a]acrossfade=d={dur}[{out_lbl}];",
            last = last_alabel,
            i = i,
            dur = TRANSITION_DURATION,
            out_lbl = out_label
        ));
        last_alabel = out_label;
    }
    let final_a = last_alabel.clone();

    // Trim trailing semicolon.
    let filter = filter.trim_end_matches(';').to_string();

    let mut cmd = Command::new(ffmpeg_path());
    cmd.args(["-y", "-hide_banner", "-loglevel", "error"]);
    for a in &input_args {
        cmd.arg(a);
    }
    cmd.args([
        "-filter_complex",
        &filter,
        "-map",
        &format!("[{}]", final_v),
        "-map",
        &format!("[{}]", final_a),
        "-c:v",
        "libx264",
        "-preset",
        "fast",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
    ])
    .arg(out_path);

    let status = cmd.status().map_err(|e| format!("ffmpeg xfade-concat failed: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg xfade-concat exited {status}"));
    }
    Ok(())
}

/// Path to the Python helper script. Phase 1: relative to the dev build's
/// src-tauri dir. Phase 3 (sidecar packaging): will resolve to the
/// app bundle's Resources/ dir.
fn tts_script_path() -> Result<PathBuf, String> {
    // CARGO_MANIFEST_DIR points to src-tauri/ in dev. In a release bundle
    // we'd resolve to the .app's Resources. For Phase 1 we use the dev
    // location.
    let manifest = env!("CARGO_MANIFEST_DIR");
    let script = PathBuf::from(manifest).join("scripts").join("tts_styletts2.py");
    if !script.exists() {
        return Err(format!("TTS script not found at {}", script.display()));
    }
    Ok(script)
}

/// Path to the Python interpreter. Currently the system python3 — Phase 3
/// packaging will bundle a portable Python runtime so users don't need
/// to install Python separately.
fn python_path() -> PathBuf {
    PathBuf::from("/usr/bin/python3")
}

/// Load narration.json for a clip that has been narrated already. Returns
/// an empty narration (entries=[]) if the file doesn't exist yet.
#[tauri::command(rename_all = "camelCase")]
fn load_narration(project_dir: String, clip_id: String) -> Result<Narration, String> {
    let path = Path::new(&project_dir)
        .join(CLIPS_DIR)
        .join(&clip_id)
        .join("narration.json");
    if !path.exists() {
        return Ok(Narration {
            version: NARRATION_SCHEMA_VERSION,
            entries: Vec::new(),
        });
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let narration: Narration = serde_json::from_str(&raw)
        .map_err(|e| format!("narration.json is invalid: {e}"))?;
    Ok(narration)
}

/// Update one narration entry's text (user edit from the script panel).
/// We do NOT clear the `inherits_from` link for inherited entries —
/// editing an inherited entry will detach it (set inherits_from to None
/// + text to the new string) so the next audio regen treats it as a
/// fresh, unique narration.
#[tauri::command(rename_all = "camelCase")]
fn update_narration_entry(
    project_dir: String,
    clip_id: String,
    frame_name: String,
    new_text: String,
) -> Result<Narration, String> {
    let path = Path::new(&project_dir)
        .join(CLIPS_DIR)
        .join(&clip_id)
        .join("narration.json");
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let mut narration: Narration = serde_json::from_str(&raw)
        .map_err(|e| format!("narration.json is invalid: {e}"))?;
    let entry = narration
        .entries
        .iter_mut()
        .find(|e| e.name == frame_name)
        .ok_or_else(|| format!("No entry with frame_name {frame_name}"))?;
    entry.text = Some(new_text);
    entry.inherits_from = None;
    // Write atomically.
    let tmp = path.with_extension("json.tmp");
    let pretty = serde_json::to_string_pretty(&narration)
        .map_err(|e| format!("Cannot serialize narration: {e}"))?;
    fs::write(&tmp, pretty).map_err(|e| format!("Cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .map_err(|e| format!("Cannot finalize {}: {e}", path.display()))?;
    Ok(narration)
}

/// Generate audio for a SINGLE entry (after the user edited it). Faster
/// than re-generating the whole clip. Returns the new duration so the
/// UI can show it. We also rewrite the audio manifest so step (f) uses
/// the new duration.
#[tauri::command(rename_all = "camelCase")]
async fn regenerate_entry_audio(
    app: tauri::AppHandle,
    project_dir: String,
    clip_id: String,
    frame_name: String,
) -> Result<f64, String> {
    use tauri::Emitter;
    use std::io::{BufRead, BufReader};

    let narration = load_narration(project_dir.clone(), clip_id.clone())?;
    let entry = narration
        .entries
        .iter()
        .find(|e| e.name == frame_name)
        .ok_or_else(|| format!("No entry with frame_name {frame_name}"))?;
    let raw_text = entry
        .text
        .as_ref()
        .ok_or_else(|| "Entry has no text to narrate".to_string())?
        .trim()
        .to_string();
    if raw_text.is_empty() {
        return Err("Cannot generate audio from empty text".into());
    }
    // Normalize ALL-CAPS words → Title Case so TTS doesn't spell them out.
    let text = normalize_caps_for_tts(&raw_text);

    let clip_dir = Path::new(&project_dir).join(CLIPS_DIR).join(&clip_id);
    let audio_dir = clip_dir.join("audio");
    fs::create_dir_all(&audio_dir).map_err(|e| format!("Cannot create audio dir: {e}"))?;
    let audio_name = frame_name.replace(".jpg", ".wav");
    let wav_path = audio_dir.join(&audio_name);

    let script_path = tts_script_path()?;
    let app_clone = app.clone();
    let clip_id_owned = clip_id.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<f64, String> {
        // Reuse the existing TTS helper by invoking it in "single-entry"
        // mode via a small inline Python snippet — cheaper than running
        // the full clip-level script that re-narrates every entry.
        let inline = format!(
            "import sys, json, warnings, os\n\
             warnings.filterwarnings('ignore')\n\
             sys.argv = ['tts_single']\n\
             from styletts2 import tts as styletts2_tts\n\
             my_tts = styletts2_tts.StyleTTS2()\n\
             my_tts.inference({txt:?}, output_wav_file={out:?})\n\
             import soundfile as sf\n\
             info = sf.info({out:?})\n\
             print(json.dumps({{'duration': info.frames/float(info.samplerate)}}))\n",
            txt = text,
            out = wav_path.to_string_lossy().into_owned()
        );
        let _ = script_path;
        let mut child = std::process::Command::new(python_path())
            .args(["-c", &inline])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Cannot spawn python: {e}"))?;
        let stdout = child.stdout.take().ok_or_else(|| "no stdout".to_string())?;
        let reader = BufReader::new(stdout);
        let mut duration: f64 = 0.0;
        for line in reader.lines().flatten() {
            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(d) = msg.get("duration").and_then(|v| v.as_f64()) {
                    duration = d;
                }
            }
        }
        let status = child.wait().map_err(|e| format!("python wait: {e}"))?;
        if !status.success() {
            return Err(format!("TTS failed for {frame_name}"));
        }
        let _ = app_clone.emit(
            "tts-progress",
            TtsProgress {
                clip_id: clip_id_owned.clone(),
                index: 1,
                total: 1,
                name: frame_name.clone(),
                stage: "done".to_string(),
                duration_seconds: Some(duration),
            },
        );
        Ok(duration)
    })
    .await
    .map_err(|e| format!("TTS task panicked: {e}"))?;

    let duration = result?;

    // Update the audio manifest so step (f) uses the new duration.
    let manifest_path = audio_dir.join("manifest.json");
    if manifest_path.exists() {
        if let Ok(raw) = fs::read_to_string(&manifest_path) {
            if let Ok(mut manifest) = serde_json::from_str::<AudioManifest>(&raw) {
                if let Some(m) = manifest
                    .entries
                    .iter_mut()
                    .find(|m| m.frame_name == entry.name)
                {
                    m.duration_seconds = duration;
                }
                let pretty = serde_json::to_string_pretty(&manifest)
                    .map_err(|e| format!("Cannot serialize manifest: {e}"))?;
                fs::write(&manifest_path, pretty)
                    .map_err(|e| format!("Cannot write manifest: {e}"))?;
            }
        }
    }
    Ok(duration)
}

#[derive(Clone, Serialize)]
struct NarrationProgress {
    clip_id: String,
    index: usize,
    total: usize,
    name: String,
    text: Option<String>,
    inherited: bool,
}

const NARRATION_SCHEMA_VERSION: u32 = 1;
/// Number of previous fresh narrations included in each vision call's
/// prompt so the AI writes continuous prose.
const NARRATION_CONTEXT_ENTRIES: usize = 2;
/// Hamming distance threshold for the 64-bit average hash. Two frames
/// with <= this many differing bits are treated as identical. Tuned for
/// mobile-app screen recordings: small UI tooltips show up as ~5-12 bits
/// changed; full screen transitions are >20.
const PHASH_DISTANCE_THRESHOLD: u32 = 5;
/// Save narration.json every N frames in case of crash / quit mid-run.
const NARRATION_AUTOSAVE_EVERY: usize = 10;
/// Vision model name as it appears in `ollama list`. Settled on minicpm-v:8b
/// after testing several alternatives:
///   - llama3.2-vision: broken on Ollama 0.30.x (mllama arch unsupported,
///     see ollama#16490)
///   - qwen3-vl:8b: high quality but 60-90s/frame under swap pressure
///     on 16GB Macs
///   - qwen3-vl:2b: thinking mode is hard-baked (parser is qwen3-vl-thinking)
///     — all output tokens go to thinking, none to the response field
///   - moondream: fast but too shallow ("iphone screen with app icons" on
///     mobile UIs)
///   - minicpm-v:8b ✓: no thinking mode, detailed UI-aware narration,
///     ~5-8s per frame after warmup on 16GB. Reads actual on-screen text
///     (button labels, dates, list items) which is exactly what we need
///     for training-video voiceover.
const OLLAMA_VISION_MODEL: &str = "minicpm-v:8b";
/// Text-only classifier used by Phase 1.7's scan stage.
///
/// We do NOT use a vision model here. The scan pipeline reads each
/// thumbnail with Tesseract OCR first, then feeds the extracted text
/// (plus context) to this small text LLM for classification. Vision
/// models proved too slow on 16GB Macs (5+ min per batch); OCR+text
/// runs the full 75-frame scan in ~3-4 min and classifies more
/// accurately because the model sees the real on-screen labels.
///
/// Why 3b specifically: llama3.2:1b is too small to distinguish a
/// title slide from a form field. 3b correctly classifies both.
///
/// See feedback_cst_studio_ocr_hybrid_scan memory for the experiment log.
const OLLAMA_SCAN_MODEL: &str = "llama3.2:3b";
const OLLAMA_URL: &str = "http://localhost:11434/api/generate";

/// Path to the Tesseract OCR binary. Installed via `brew install tesseract`.
/// Phase 3 sidecar bundling will swap to a bundled binary the same way
/// ffmpeg_path/ffprobe_path/soffice_path do.
fn tesseract_path() -> PathBuf {
    PathBuf::from("/opt/homebrew/bin/tesseract")
}

/// Compute an 8x8 average-hash perceptual hash of a JPEG. Returns 16 hex
/// chars (64 bits). Fast: ~3-5ms per frame on M1.
fn compute_phash(path: &str) -> Result<String, String> {
    let img = image::open(path).map_err(|e| format!("Cannot open {path}: {e}"))?;
    let small = img.resize_exact(8, 8, image::imageops::FilterType::Nearest).to_luma8();
    let pixels: Vec<u8> = small.into_raw();
    let avg: u32 = pixels.iter().map(|&p| p as u32).sum::<u32>() / 64;
    let mut bits: u64 = 0;
    for (i, &p) in pixels.iter().enumerate() {
        if (p as u32) >= avg {
            bits |= 1 << i;
        }
    }
    Ok(format!("{bits:016x}"))
}

fn hamming_distance(a: &str, b: &str) -> u32 {
    let an = u64::from_str_radix(a, 16).unwrap_or(0);
    let bn = u64::from_str_radix(b, 16).unwrap_or(0);
    (an ^ bn).count_ones()
}

/// Call Ollama's /api/generate with the vision model + a frame image.
/// Returns the model's plain-text response.
async fn call_ollama_vision(
    http: &reqwest::Client,
    main_prompt: &str,
    rolling_context: &[String],
    image_path: &str,
) -> Result<String, String> {
    use base64::Engine;
    let bytes = fs::read(image_path).map_err(|e| format!("Cannot read frame: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

    let mut prompt = String::new();
    prompt.push_str("You are narrating a training video.\n\n");
    prompt.push_str("PROJECT CONTEXT (always relevant — do not deviate):\n");
    if main_prompt.trim().is_empty() {
        prompt.push_str("(no project context provided)\n");
    } else {
        prompt.push_str(main_prompt.trim());
        prompt.push('\n');
    }
    prompt.push('\n');
    if !rolling_context.is_empty() {
        prompt.push_str("WHAT YOU JUST SAID (continue naturally, do not repeat):\n");
        for line in rolling_context {
            prompt.push_str("- ");
            prompt.push_str(line);
            prompt.push('\n');
        }
        prompt.push('\n');
    }
    prompt.push_str(
        "Look at this UI screenshot and write ONE step instruction telling \
         the viewer what to DO on this screen. This will be one numbered \
         step in a step-by-step training guide.\n\
         \n\
         STYLE — write it the way a Filipino tech instructor would write \
         actual user-facing steps:\n\
         - Imperative voice — address the viewer directly. Start with a \
           verb (Select, Tap, Open, Enter, Confirm, Choose, Scroll, Review).\n\
         - Reference ONE specific UI element visible on THIS screen by its \
           exact on-screen text.\n\
         - When the on-screen text is in ALL CAPS, write it in Title Case \
           (\"App Forms\" not \"APP FORMS\") so text-to-speech reads it as a \
           word. Keep ≤3-letter acronyms (AI, UI, CC) as-is.\n\
         - Assume the viewer has ALREADY been told what app + form they're in. \
           Do NOT repeat the app name or form name every step.\n\
         \n\
         GOOD examples of the style we want:\n\
         - \"Select the AMLI Promo Implementation form from the list.\"\n\
         - \"Choose the correct Store ID, or confirm it is already filled.\"\n\
         - \"From the dropdown, select 1 or 0 for FF Store Surveyed.\"\n\
         - \"Tap the MCS Site Name field and enter the site name.\"\n\
         \n\
         BAD examples to AVOID:\n\
         - \"The user is selecting...\" (observer voice — wrong)\n\
         - \"This screen shows...\" (describes instead of instructs)\n\
         - \"In Tarkie App's form titled 'AMLI Operator Promo Implementation'...\"\n\
           (repeats context — wrong, that belongs in the overview)\n\
         \n\
         CONSTRAINTS:\n\
         - Maximum 20 words.\n\
         - Only refer to elements ACTUALLY visible on this exact screen.\n\
         - Do not invent buttons/menus/screens that aren't shown.\n\
         - Do not think out loud. Output only the one-sentence step."
    );

    // Tighter cap now that we want one-sentence steps. 40 tokens ≈ 30
    // words headroom over our 18-word target.
    let body = serde_json::json!({
        "model": OLLAMA_VISION_MODEL,
        "prompt": prompt,
        "images": [b64],
        "stream": false,
        "options": {
            "num_predict": 40,
            "temperature": 0.3,
        }
    });
    let resp = http
        .post(OLLAMA_URL)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Cannot reach Ollama at {OLLAMA_URL}: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Ollama returned {status}: {body}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Ollama response not JSON: {e}"))?;
    json.get("response")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "Ollama response missing 'response' field".to_string())
}

fn write_narration_file(
    project_dir: &str,
    clip_id: &str,
    narration: &Narration,
) -> Result<(), String> {
    let dir = Path::new(project_dir).join(CLIPS_DIR).join(clip_id);
    fs::create_dir_all(&dir).map_err(|e| format!("Cannot create clip dir: {e}"))?;
    let tmp = dir.join("narration.json.tmp");
    let final_path = dir.join("narration.json");
    let pretty = serde_json::to_string_pretty(narration)
        .map_err(|e| format!("Cannot serialize narration: {e}"))?;
    fs::write(&tmp, pretty).map_err(|e| format!("Cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &final_path)
        .map_err(|e| format!("Cannot finalize {}: {e}", final_path.display()))?;
    Ok(())
}

/// ffprobe the duration of a video file. Returns seconds as f64.
fn probe_duration(source: &Path) -> Result<f64, String> {
    let output = Command::new(ffprobe_path())
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(source)
        .output()
        .map_err(|e| format!("Cannot run ffprobe: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    s.parse::<f64>()
        .map_err(|e| format!("ffprobe returned non-numeric duration '{s}': {e}"))
}

/// Extract frames from a video using ffmpeg scene detection — keeps only
/// the visually meaningful moments. Returns the source duration. Writes
/// frames.json beside the JPGs with per-frame timestamps for narration.
fn extract_video_frames(source: &Path, frames_dir: &Path) -> Result<f64, String> {
    let duration = probe_duration(source)?;
    if duration > MAX_CLIP_SECONDS {
        return Err(format!(
            "Clip is {:.1} min long; the 10-minute cap was set to keep AI processing tractable. \
             Trim with QuickTime and re-add it.",
            duration / 60.0
        ));
    }

    // PASS 1: detect scene-change timestamps. We use a probe-only ffmpeg
    // run with `-f null -` that writes nothing to disk and just lets the
    // scene detector + showinfo emit timestamp metadata to stderr. This
    // is cheaper than the full extract because no JPEGs are written.
    let detect_vf = format!(
        "select='gt(scene\\,{thr})+eq(n\\,0)',showinfo",
        thr = SCENE_THRESHOLD
    );
    let detect_output = Command::new(ffmpeg_path())
        .args(["-hide_banner", "-loglevel", "info", "-i"])
        .arg(source)
        .args([
            "-vf",
            &detect_vf,
            "-vsync",
            "vfr",
            "-an",
            "-f",
            "null",
            "-",
        ])
        .output()
        .map_err(|e| format!("Cannot run ffmpeg scene detect: {e}"))?;
    if !detect_output.status.success() {
        return Err(format!(
            "ffmpeg scene-detect exited with status {}: {}",
            detect_output.status,
            String::from_utf8_lossy(&detect_output.stderr)
        ));
    }

    // Parse the showinfo log for scene-change timestamps.
    let log = String::from_utf8_lossy(&detect_output.stderr);
    let mut scene_timestamps: Vec<f64> = Vec::new();
    for line in log.lines() {
        if !line.contains("showinfo") {
            continue;
        }
        if let Some(idx) = line.find("pts_time:") {
            let rest = &line[idx + "pts_time:".len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
                .unwrap_or(rest.len());
            if let Ok(t) = rest[..end].parse::<f64>() {
                scene_timestamps.push(t);
            }
        }
    }

    // PASS 2: for each scene-change timestamp, seek SCENE_OFFSET_SECONDS
    // ahead and extract one frame. Skip any offset that would land past
    // the clip's duration. Always include the first frame (timestamp 0)
    // as-is — there's no preceding animation to settle from.
    let mut produced: Vec<PathBuf> = Vec::new();
    let mut timestamps: Vec<f64> = Vec::new();
    let mut frame_idx: u32 = 0;
    for ts in &scene_timestamps {
        let target_t = if *ts < 0.001 {
            // First frame: keep as-is.
            0.0
        } else {
            let candidate = ts + SCENE_OFFSET_SECONDS;
            if candidate >= duration - 0.05 {
                // Offset would land at/past clip end — skip this scene.
                continue;
            }
            candidate
        };
        frame_idx += 1;
        let out_path = frames_dir.join(format!("{frame_idx:04}.jpg"));
        // -ss BEFORE -i = fast seek (less accurate but fine for non-keyframe
        // proximity); 0.5s offset is well within tolerance. Then scale and
        // write one frame.
        let scale_vf = format!(
            "scale='if(gt(iw\\,ih)\\,{lo}\\,-2)':'if(gt(iw\\,ih)\\,-2\\,{lo})'",
            lo = RESIZE_LONG_EDGE
        );
        let status = Command::new(ffmpeg_path())
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-ss",
                &format!("{target_t:.3}"),
                "-i",
            ])
            .arg(source)
            .args([
                "-vf",
                &scale_vf,
                "-frames:v",
                "1",
                "-q:v",
                &JPEG_QUALITY.to_string(),
                "-y",
            ])
            .arg(&out_path)
            .status()
            .map_err(|e| format!("Cannot run ffmpeg seek-extract: {e}"))?;
        if !status.success() || !out_path.exists() {
            // Skip a failure on one frame rather than abort the whole pass.
            continue;
        }
        produced.push(out_path);
        timestamps.push(target_t);
    }

    // Fallback: if scene detection produced too few frames (e.g. very
    // static video), supplement with frames every N seconds so we have
    // enough coverage for narration to feel paced.
    if produced.len() < MIN_FRAMES_FALLBACK && duration > 8.0 {
        let extra_interval = (duration / MIN_FRAMES_FALLBACK as f64).max(2.0);
        let fallback_pattern = frames_dir.join("%04d.jpg");
        let fallback_vf = format!(
            "fps=1/{interval},scale='if(gt(iw\\,ih)\\,{lo}\\,-2)':'if(gt(iw\\,ih)\\,-2\\,{lo})'",
            interval = extra_interval,
            lo = RESIZE_LONG_EDGE,
        );
        // Re-run with simple fps sampling, overwriting whatever was there.
        for p in &produced {
            let _ = fs::remove_file(p);
        }
        let status = Command::new(ffmpeg_path())
            .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(source)
            .args(["-vf", &fallback_vf, "-q:v", &JPEG_QUALITY.to_string()])
            .arg(&fallback_pattern)
            .status()
            .map_err(|e| format!("Cannot run ffmpeg fallback: {e}"))?;
        if !status.success() {
            return Err(format!("ffmpeg fallback exited with status {status}"));
        }
        // Regenerate timestamps for the fallback frames at fixed intervals.
        timestamps.clear();
        produced = fs::read_dir(frames_dir)
            .map_err(|e| format!("Cannot scan frames dir: {e}"))?
            .filter_map(|r| r.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("jpg"))
                    .unwrap_or(false)
            })
            .collect();
        produced.sort();
        for i in 0..produced.len() {
            timestamps.push((i as f64) * extra_interval);
        }
    }

    // Write frames.json so list_frames doesn't need to re-parse ffmpeg
    // logs on subsequent loads.
    let manifest: Vec<FrameManifestEntry> = produced
        .iter()
        .enumerate()
        .map(|(i, p)| FrameManifestEntry {
            name: p.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
            timestamp_seconds: Some(*timestamps.get(i).unwrap_or(&0.0)),
        })
        .collect();
    write_frames_manifest(frames_dir, &manifest)?;

    Ok(duration)
}

#[derive(Serialize, Deserialize, Clone)]
struct FrameManifestEntry {
    name: String,
    timestamp_seconds: Option<f64>,
}

fn write_frames_manifest(frames_dir: &Path, entries: &[FrameManifestEntry]) -> Result<(), String> {
    let path = frames_dir.join("frames.json");
    let pretty = serde_json::to_string_pretty(entries)
        .map_err(|e| format!("Cannot serialize frames manifest: {e}"))?;
    fs::write(&path, pretty).map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
    Ok(())
}

fn read_frames_manifest(frames_dir: &Path) -> Option<Vec<FrameManifestEntry>> {
    let path = frames_dir.join("frames.json");
    let raw = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Convert each PPTX slide to a JPG.
///
/// LibreOffice's `--convert-to jpg` is fundamentally broken for multi-
/// slide decks — it only ever exports the first slide regardless of
/// deck length (well-known LibreOffice limitation). We use a two-step
/// path instead:
///   1. PPTX → PDF (one PDF page per slide) via `soffice --convert-to pdf`
///   2. PDF → JPGs (one per page) via `pdftoppm` from Poppler
fn extract_pptx_slides(source: &Path, frames_dir: &Path) -> Result<(), String> {
    // Step 1: PPTX → PDF in a tmp dir adjacent to frames_dir.
    let tmp_pdf_dir = frames_dir.join(".pdf_tmp");
    fs::create_dir_all(&tmp_pdf_dir).map_err(|e| format!("Cannot create pdf tmp dir: {e}"))?;
    let status = Command::new(soffice_path())
        .args(["--headless", "--convert-to", "pdf", "--outdir"])
        .arg(&tmp_pdf_dir)
        .arg(source)
        .status()
        .map_err(|e| format!("Cannot run soffice: {e}"))?;
    if !status.success() {
        return Err(format!("soffice PPTX→PDF exited {status}"));
    }

    // soffice writes <stem>.pdf into the outdir.
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "PPTX has no filename stem".to_string())?;
    let pdf_path = tmp_pdf_dir.join(format!("{stem}.pdf"));
    if !pdf_path.exists() {
        return Err(format!(
            "soffice did not produce {} — check the deck loads in LibreOffice",
            pdf_path.display()
        ));
    }

    // Step 2: PDF → JPGs via pdftoppm.
    // pdftoppm <pdf> <prefix> -jpeg -r 144 produces <prefix>-1.jpg, <prefix>-2.jpg, …
    // -r 144 = 144 DPI = good quality for slide-as-screenshot output. With
    // a standard 10"×7.5" slide this gives ~1440×1080 — plenty.
    let prefix = frames_dir.join("slide");
    let status = Command::new(pdftoppm_path())
        .args(["-jpeg", "-r", "144"])
        .arg(&pdf_path)
        .arg(&prefix)
        .status()
        .map_err(|e| format!("Cannot run pdftoppm: {e}"))?;
    if !status.success() {
        return Err(format!("pdftoppm exited {status}"));
    }

    // Step 3: rename to NNNN.jpg for consistency with video frame naming.
    let mut produced: Vec<(u32, PathBuf)> = Vec::new();
    for entry in fs::read_dir(frames_dir).map_err(|e| format!("Cannot scan frames dir: {e}"))? {
        let p = entry.map_err(|e| format!("Frame entry error: {e}"))?.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("slide-") || !name.ends_with(".jpg") {
            continue;
        }
        let num_part = name
            .trim_start_matches("slide-")
            .trim_end_matches(".jpg");
        if let Ok(n) = num_part.parse::<u32>() {
            produced.push((n, p));
        }
    }
    produced.sort_by_key(|(n, _)| *n);
    for (n, src) in &produced {
        let dest = frames_dir.join(format!("{n:04}.jpg"));
        if &dest != src {
            fs::rename(src, &dest)
                .map_err(|e| format!("Cannot rename slide {n}: {e}"))?;
        }
    }

    // Clean up the intermediate PDF.
    let _ = fs::remove_dir_all(&tmp_pdf_dir);

    if produced.is_empty() {
        return Err("PDF was produced but no slide images came out of pdftoppm".into());
    }
    Ok(())
}

fn pdftoppm_path() -> PathBuf {
    PathBuf::from("/opt/homebrew/bin/pdftoppm")
}

/// Walk the frames dir and produce a sorted FrameInfo list. For video
/// clips with scene-detection extraction, per-frame timestamps come from
/// frames.json (the manifest written during extraction). For PPTX, the
/// manifest is absent and timestamps are None.
fn collect_frame_info(
    frames_dir: &Path,
    clip_duration_seconds: Option<f64>,
) -> Result<Vec<FrameInfo>, String> {
    let mut entries: Vec<PathBuf> = fs::read_dir(frames_dir)
        .map_err(|e| format!("Cannot scan frames dir: {e}"))?
        .filter_map(|r| r.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("jpg"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();

    // Prefer per-frame timestamps from the manifest written during extraction.
    let manifest = read_frames_manifest(frames_dir);

    let mut frames = Vec::with_capacity(entries.len());
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let timestamp_seconds = manifest
            .as_ref()
            .and_then(|m| m.iter().find(|e| e.name == name))
            .and_then(|e| e.timestamp_seconds)
            .or_else(|| {
                // PPTX slides have no timestamp, so leave it as None.
                if clip_duration_seconds.is_none() {
                    None
                } else {
                    // No manifest but it's a video — fall back to index-based
                    // timing. Shouldn't happen in normal flow.
                    Some(0.0)
                }
            });
        frames.push(FrameInfo {
            name,
            path: path.to_string_lossy().into_owned(),
            timestamp_seconds,
        });
    }
    Ok(frames)
}

/// Find the source.* file inside a clip dir (we copied it as "source.<ext>"
/// in add_clip).
fn find_clip_source(clip_dir: &Path) -> Result<PathBuf, String> {
    for entry in fs::read_dir(clip_dir).map_err(|e| format!("Cannot scan clip dir: {e}"))? {
        let p = entry.map_err(|e| format!("Clip entry error: {e}"))?.path();
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem == "source" && p.is_file() {
            return Ok(p);
        }
    }
    Err(format!(
        "No source.* file found in clip folder {}",
        clip_dir.display()
    ))
}

/// Helper: write project.json atomically-ish (tmp file + rename). Strips
/// the `dir` field before writing — it's only meaningful at runtime and
/// re-derived on load.
fn write_project_file(project: &Project) -> Result<(), String> {
    let dir = PathBuf::from(&project.dir);
    let tmp = dir.join(format!("{PROJECT_FILE}.tmp"));
    let final_path = dir.join(PROJECT_FILE);
    // Serialize to a Value first so we can drop `dir` without changing the
    // Rust struct shape (which the IPC layer relies on).
    let mut value = serde_json::to_value(project)
        .map_err(|e| format!("Cannot serialize project: {e}"))?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("dir");
    }
    let pretty = serde_json::to_string_pretty(&value)
        .map_err(|e| format!("Cannot serialize project: {e}"))?;
    fs::write(&tmp, pretty)
        .map_err(|e| format!("Cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &final_path)
        .map_err(|e| format!("Cannot finalize {}: {e}", final_path.display()))?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![
            ping,
            create_project,
            load_project,
            save_project,
            add_clip,
            remove_clip,
            reorder_clips,
            extract_frames,
            list_frames,
            narrate_clip,
            load_narration,
            update_narration_entry,
            regenerate_entry_audio,
            cancel_narration,
            generate_titles,
            generate_overview,
            generate_audio,
            load_audio_manifest,
            render_video,
            scan_clip,
            load_scan,
            plan_script,
            load_plan,
            update_plan,
            toggle_frame_excluded,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
