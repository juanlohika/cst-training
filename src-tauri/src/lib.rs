// CST Studio — Tauri Rust core.
//
// Phase 1: just a ping handler so the React UI can verify the Rust bridge.
// Real commands (open_project, run_ollama, synthesize_tts, render_mp4)
// land in Phase 1.2+ as we wire up each piece of the pipeline.

#[tauri::command]
fn ping() -> String {
    format!(
        "Rust core alive. cst-studio v{}",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![ping])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
