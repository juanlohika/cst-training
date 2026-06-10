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
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    created_at: DateTime<Utc>,
    clips: Vec<Clip>,
    /// Absolute path to this project's folder on disk. Re-derived on load
    /// (so moving a project folder doesn't break it) and stripped before
    /// writing to project.json by write_project_file. Always present when
    /// crossing the Tauri IPC boundary so the JS side can use it.
    #[serde(default)]
    dir: String,
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

    let mut entries: Vec<NarrationEntry> = Vec::with_capacity(frames.len());
    let mut last_fresh_idx: Option<usize> = None;
    let mut rolling_context: Vec<String> = Vec::new();

    for (i, f) in frames.iter().enumerate() {
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

    Ok(narration)
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
/// Vision model name as it appears in `ollama list`. We use Qwen3-VL because
/// Ollama's 0.30.x release broke Llama 3.2 Vision (see issue #16490). Qwen3-VL
/// is also better-suited to UI-narration: it's the model purpose-built for
/// visual-agent benchmarks like OS World.
const OLLAMA_VISION_MODEL: &str = "qwen3-vl:8b";
const OLLAMA_URL: &str = "http://localhost:11434/api/generate";

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
        "Describe what is happening in this frame as if you are explaining it \
         to a new operator. Be specific about UI elements they should notice \
         and what action is happening. STRICT LIMIT: 1-2 short sentences, \
         no more than 40 words total. No filler phrases like \"in this frame\" \
         or \"this image shows\" — write what you would actually say in the \
         voiceover. Do not think out loud. Output only the narration text."
    );

    // Cap output tokens to keep per-frame inference time tractable on
    // RAM-constrained Macs. 80 tokens ≈ 60 words, enough headroom over our
    // 40-word target for the model to land a clean stop.
    let body = serde_json::json!({
        "model": OLLAMA_VISION_MODEL,
        "prompt": prompt,
        "images": [b64],
        "stream": false,
        "options": {
            "num_predict": 80,
            "temperature": 0.4,
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

    // Scene-detection pass. ffmpeg's `select='gt(scene,T)'` keeps only
    // frames whose pixel-difference score from the previous frame exceeds
    // T. We pair it with `showinfo` to emit per-frame metadata that we
    // parse for timestamps. -vsync vfr preserves the original frame
    // timing so the timestamps are real (not renumbered to N fps).
    //
    // scale=W:-2 pre-resizes to RESIZE_LONG_EDGE on the long edge; the
    // model would downscale internally anyway, so doing it now saves
    // ~30% per-frame inference time on RAM-constrained Macs.
    let pattern = frames_dir.join("%04d.jpg");
    let vf = format!(
        "select='gt(scene\\,{thr})+eq(n\\,0)',scale='if(gt(iw\\,ih)\\,{lo}\\,-2)':'if(gt(iw\\,ih)\\,-2\\,{lo})',showinfo",
        thr = SCENE_THRESHOLD,
        lo = RESIZE_LONG_EDGE,
    );
    let output = Command::new(ffmpeg_path())
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "info",
            "-i",
        ])
        .arg(source)
        .args([
            "-vf",
            &vf,
            "-vsync",
            "vfr",
            "-q:v",
            &JPEG_QUALITY.to_string(),
        ])
        .arg(&pattern)
        .output()
        .map_err(|e| format!("Cannot run ffmpeg: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffmpeg exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Parse the showinfo log for per-frame PTS timestamps. Each frame
    // produces a line like: "[Parsed_showinfo_X] n: 0 pts:N pts_time:1.234 ..."
    // We grab pts_time in order, one per output JPG.
    let log = String::from_utf8_lossy(&output.stderr);
    let mut timestamps: Vec<f64> = Vec::new();
    for line in log.lines() {
        if !line.contains("showinfo") {
            continue;
        }
        if let Some(idx) = line.find("pts_time:") {
            let rest = &line[idx + "pts_time:".len()..];
            let end = rest.find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-').unwrap_or(rest.len());
            if let Ok(t) = rest[..end].parse::<f64>() {
                timestamps.push(t);
            }
        }
    }

    // Count actual JPGs ffmpeg wrote — we use this as the source of truth
    // for which timestamps belong to which frame.
    let mut produced: Vec<PathBuf> = fs::read_dir(frames_dir)
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

    // Trim timestamps to match the JPG count (showinfo emits one entry per
    // selected frame so they should match, but be defensive).
    timestamps.truncate(produced.len());
    while timestamps.len() < produced.len() {
        timestamps.push(0.0);
    }

    // Fallback: if scene detection produced too few frames (e.g. very
    // static video), supplement with frames every N seconds so we have
    // enough coverage for narration to feel paced.
    if produced.len() < MIN_FRAMES_FALLBACK && duration > 8.0 {
        let extra_interval = (duration / MIN_FRAMES_FALLBACK as f64).max(2.0);
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
            .arg(&pattern)
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

/// Convert each PPTX slide to a JPG using LibreOffice headless. LibreOffice
/// writes <source-stem>.jpg per slide automatically when --convert-to jpg
/// is given a multi-slide deck; we then rename to NNNN.jpg for consistency.
fn extract_pptx_slides(source: &Path, frames_dir: &Path) -> Result<(), String> {
    // LibreOffice writes one file per slide, named <stem>.jpg, <stem>-2.jpg…
    // We give it the frames_dir as outdir directly so the files land there.
    let status = Command::new(soffice_path())
        .args(["--headless", "--convert-to", "jpg", "--outdir"])
        .arg(frames_dir)
        .arg(source)
        .status()
        .map_err(|e| format!("Cannot run soffice: {e}"))?;
    if !status.success() {
        return Err(format!("soffice exited with status {status}"));
    }

    // Normalize names: LibreOffice produces "<stem>.jpg", "<stem>-2.jpg",
    // "<stem>-3.jpg" etc. Rename to NNNN.jpg in slide order.
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "PPTX has no filename stem".to_string())?;
    let mut produced: Vec<(u32, PathBuf)> = Vec::new();
    for entry in fs::read_dir(frames_dir).map_err(|e| format!("Cannot scan frames dir: {e}"))? {
        let p = entry.map_err(|e| format!("Frame entry error: {e}"))?.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".jpg") {
            continue;
        }
        let base = name.trim_end_matches(".jpg");
        let slide_n: u32 = if base == stem {
            1
        } else if let Some(suffix) = base.strip_prefix(&format!("{stem}-")) {
            suffix.parse().unwrap_or(0)
        } else {
            continue;
        };
        if slide_n > 0 {
            produced.push((slide_n, p));
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
    Ok(())
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
