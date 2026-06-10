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
const FRAMES_PER_SECOND: u32 = 1;
const JPEG_QUALITY: u8 = 2; // ffmpeg -q:v scale (2 = ~q90, lower=better)

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

/// Sample frames from a video at FRAMES_PER_SECOND. Returns the duration.
fn extract_video_frames(source: &Path, frames_dir: &Path) -> Result<f64, String> {
    let duration = probe_duration(source)?;
    if duration > MAX_CLIP_SECONDS {
        return Err(format!(
            "Clip is {:.1} min long; the 10-minute cap was set to keep AI processing tractable. \
             Trim with QuickTime and re-add it.",
            duration / 60.0
        ));
    }

    let pattern = frames_dir.join("%04d.jpg");
    let status = Command::new(ffmpeg_path())
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
        ])
        .arg(source)
        .args([
            "-vf",
            &format!("fps={FRAMES_PER_SECOND}"),
            "-q:v",
            &JPEG_QUALITY.to_string(),
        ])
        .arg(&pattern)
        .status()
        .map_err(|e| format!("Cannot run ffmpeg: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg exited with status {status}"));
    }
    Ok(duration)
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
/// clips, timestamp_seconds is computed from the index (1 fps). For PPTX,
/// timestamp is None — slides don't have timestamps.
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

    let mut frames = Vec::with_capacity(entries.len());
    for (i, path) in entries.into_iter().enumerate() {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let timestamp_seconds = clip_duration_seconds.map(|_| (i as f64) / (FRAMES_PER_SECOND as f64));
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
