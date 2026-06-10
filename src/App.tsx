import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import "./App.css";

/**
 * CST Studio — desktop training video generator.
 *
 * Phase 1: walking skeleton. We're proving out each piece of the
 * pipeline one at a time. Right now: bridge + file-picker. Next:
 * Ollama vision → TTS → ffmpeg render.
 */

interface SourceInspection {
  path: string;
  file_name: string;
  bytes: number;
  kind: "pptx" | "video" | "unknown";
}

function App() {
  const [bridge, setBridge] = useState<string>("Tauri bridge not pinged yet");
  const [source, setSource] = useState<SourceInspection | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const pingRust = async () => {
    try {
      const result: string = await invoke("ping");
      setBridge(`✓ ${result}`);
    } catch (e: any) {
      setBridge(`✗ ${e?.message || String(e)}`);
    }
  };

  const pickSource = async () => {
    setError(null);
    setBusy(true);
    try {
      const selected = await open({
        multiple: false,
        directory: false,
        filters: [
          { name: "Training source", extensions: ["pptx", "mp4", "mov"] },
        ],
      });
      if (!selected) {
        setBusy(false);
        return;
      }
      const path = typeof selected === "string" ? selected : (selected as any).path;
      const inspection: SourceInspection = await invoke("inspect_source", { path });
      setSource(inspection);
    } catch (e: any) {
      setError(e?.message || String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <main className="container">
      <div className="brand">
        <h1>CST Studio</h1>
        <p className="tagline">Desktop training video generator</p>
      </div>

      <div className="status-card">
        <div className="status-label">Tauri ↔ Rust bridge</div>
        <div className="status-value">{bridge}</div>
        <button onClick={pingRust}>Ping Rust core</button>
      </div>

      <div className="status-card">
        <div className="status-label">Pick a source file</div>
        <div className="status-value">
          {source
            ? `${source.file_name} (${formatBytes(source.bytes)}, kind: ${source.kind})`
            : "No file picked yet"}
        </div>
        <button onClick={pickSource} disabled={busy}>
          {busy ? "Opening…" : "Pick PPTX, MP4, or MOV"}
        </button>
        {source?.kind === "unknown" && (
          <div className="warn">
            ⚠ This file's extension isn't pptx/mp4/mov. We can read it but the pipeline may not know what to do with it.
          </div>
        )}
        {error && <div className="error">✗ {error}</div>}
      </div>

      <div className="footer">
        Phase 1 milestones:
        <ul>
          <li>✓ Tauri shell boots</li>
          <li>✓ React UI renders</li>
          <li>✓ Rust ↔ JS bridge</li>
          <li>{source ? "✓" : "○"} Pick a source file from disk</li>
          <li>○ Run Ollama Llama 3.2 Vision</li>
          <li>○ Generate one TTS clip</li>
          <li>○ Render one MP4 with ffmpeg</li>
        </ul>
      </div>
    </main>
  );
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n}B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)}KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)}MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)}GB`;
}

export default App;
