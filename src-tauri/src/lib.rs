// CST Studio — Tauri Rust core.
//
// Phase 1: ping + inspect_source. Real commands (run_ollama,
// synthesize_tts, render_mp4) land in Phase 1.2+ as we wire up each
// piece of the pipeline.

use serde::Serialize;
use std::path::Path;

#[tauri::command]
fn ping() -> String {
    format!(
        "Rust core alive. cst-studio v{}",
        env!("CARGO_PKG_VERSION")
    )
}

/// Result of inspecting a source file the user just dropped/picked.
/// Phase 1.1 — confirms we can read file metadata; later phases hand the
/// path off to LibreOffice (PPTX) or ffmpeg (MP4) for extraction.
#[derive(Serialize)]
struct SourceInspection {
    path: String,
    file_name: String,
    bytes: u64,
    kind: String,        // "pptx" | "video" | "unknown"
}

#[tauri::command]
fn inspect_source(path: String) -> Result<SourceInspection, String> {
    let p = Path::new(&path);
    let meta = std::fs::metadata(p).map_err(|e| format!("Cannot read file: {e}"))?;
    if !meta.is_file() {
        return Err("Not a regular file".into());
    }
    let file_name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("(no name)")
        .to_string();
    let ext_lower = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let kind = match ext_lower.as_str() {
        "pptx" => "pptx",
        "mp4" | "mov" => "video",
        _ => "unknown",
    }
    .to_string();
    Ok(SourceInspection {
        path: p.to_string_lossy().into_owned(),
        file_name,
        bytes: meta.len(),
        kind,
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![ping, inspect_source])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
