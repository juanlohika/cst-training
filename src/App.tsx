import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

/**
 * CST Studio — desktop training video generator.
 *
 * Phase 1: walking skeleton. Just confirms Tauri ↔ Rust bridge is alive
 * and the dev environment runs. UI is intentionally bare; real scene
 * editor comes in Phase 2 (port from cst-flow's training-videos page).
 */
function App() {
  const [status, setStatus] = useState<string>("Tauri bridge not pinged yet");

  const pingRust = async () => {
    try {
      const result: string = await invoke("ping");
      setStatus(`✓ ${result}`);
    } catch (e: any) {
      setStatus(`✗ ${e?.message || String(e)}`);
    }
  };

  return (
    <main className="container">
      <div className="brand">
        <h1>CST Studio</h1>
        <p className="tagline">Desktop training video generator</p>
      </div>

      <div className="status-card">
        <div className="status-label">Walking skeleton — Phase 1</div>
        <div className="status-value">{status}</div>
        <button onClick={pingRust}>Ping Rust core</button>
      </div>

      <div className="footer">
        Phase 1 milestones:
        <ul>
          <li>✓ Tauri shell boots</li>
          <li>✓ React UI renders</li>
          <li>○ Rust ↔ JS bridge (click "Ping Rust core")</li>
          <li>○ Read a PPTX from disk</li>
          <li>○ Run Ollama Llama 3.2 Vision</li>
          <li>○ Generate one TTS clip</li>
          <li>○ Render one MP4 with ffmpeg</li>
        </ul>
      </div>
    </main>
  );
}

export default App;
