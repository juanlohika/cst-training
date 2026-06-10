// CST Studio — Tauri Rust core.
//
// Phase 1 step (a): project model. A "project" is a folder on disk with
// project.json + a clips/ subdir. Each clip is a numbered subfolder with
// the source file copied in. Frames/narration/audio land in clip folders
// in later steps.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const PROJECT_FILE: &str = "project.json";
const CLIPS_DIR: &str = "clips";
const PROJECT_SCHEMA_VERSION: u32 = 1;

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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
