import { useCallback, useEffect, useRef, useState } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { homeDir, downloadDir } from "@tauri-apps/api/path";
import type { Project, Clip, FrameInfo, ExtractResult } from "./types";
import "./App.css";

/**
 * CST Studio — desktop training video generator.
 *
 * Phase 1 step (a): project model. The app is either on the landing screen
 * (no project loaded) or in the project view (a Project object in memory,
 * mirrored to project.json on disk). Steps (b)+ add frame extraction,
 * vision, TTS, and rendering.
 */

type AppState =
  | { kind: "landing" }
  | { kind: "creating" }
  | { kind: "project"; project: Project };

function App() {
  const [state, setState] = useState<AppState>({ kind: "landing" });
  const [error, setError] = useState<string | null>(null);

  return (
    <main className="container">
      <header className="brand">
        <h1>CST Studio</h1>
        <p className="tagline">Desktop training video generator</p>
      </header>

      {error && <div className="error error--top">✗ {error}</div>}

      {state.kind === "landing" && (
        <Landing
          onNewProject={() => setState({ kind: "creating" })}
          onOpenProject={async () => {
            setError(null);
            try {
              const dir = await open({ multiple: false, directory: true });
              if (!dir) return;
              const path = typeof dir === "string" ? dir : (dir as any).path;
              const project: Project = await invoke("load_project", {
                projectDir: path,
              });
              setState({ kind: "project", project });
            } catch (e: any) {
              setError(e?.message || String(e));
            }
          }}
        />
      )}

      {state.kind === "creating" && (
        <NewProjectModal
          onCancel={() => setState({ kind: "landing" })}
          onCreated={(project) => setState({ kind: "project", project })}
          onError={setError}
        />
      )}

      {state.kind === "project" && (
        <ProjectView
          project={state.project}
          onProjectChange={(p) => setState({ kind: "project", project: p })}
          onClose={() => setState({ kind: "landing" })}
          onError={setError}
        />
      )}
    </main>
  );
}

/* ---------- Landing screen ---------- */

function Landing(props: {
  onNewProject: () => void;
  onOpenProject: () => void;
}) {
  return (
    <div className="status-card">
      <div className="status-label">Get started</div>
      <p className="muted">
        Create a new project to start a training video, or open an existing
        project folder.
      </p>
      <div className="row">
        <button onClick={props.onNewProject}>New project</button>
        <button onClick={props.onOpenProject}>Open project…</button>
      </div>
    </div>
  );
}

/* ---------- New project modal ---------- */

function NewProjectModal(props: {
  onCancel: () => void;
  onCreated: (p: Project) => void;
  onError: (msg: string) => void;
}) {
  const [name, setName] = useState("");
  const [saveLocation, setSaveLocation] = useState<string>("");
  const [creating, setCreating] = useState(false);

  // Default to ~/Downloads on first render.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const dl = await downloadDir();
        if (!cancelled) setSaveLocation(dl);
      } catch {
        try {
          const home = await homeDir();
          if (!cancelled) setSaveLocation(home);
        } catch {
          if (!cancelled) setSaveLocation("");
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const create = async () => {
    if (!name.trim() || !saveLocation || creating) return;
    setCreating(true);
    try {
      const project: Project = await invoke("create_project", {
        parentDir: saveLocation,
        name: name.trim(),
      });
      props.onCreated(project);
    } catch (e: any) {
      props.onError(e?.message || String(e));
    } finally {
      setCreating(false);
    }
  };

  const chooseLocation = async () => {
    const dir = await open({ multiple: false, directory: true });
    if (!dir) return;
    setSaveLocation(typeof dir === "string" ? dir : (dir as any).path);
  };

  return (
    <div className="status-card">
      <div className="status-label">New project</div>
      <label className="field">
        <span className="field-label">Project name</span>
        <input
          className="text-input"
          autoFocus
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="e.g. AMII Operator Promo - June"
          onKeyDown={(e) => {
            if (e.key === "Enter") create();
          }}
        />
      </label>

      <label className="field">
        <span className="field-label">Save location</span>
        <div className="row">
          <input
            className="text-input mono"
            value={saveLocation}
            onChange={(e) => setSaveLocation(e.target.value)}
            spellCheck={false}
          />
          <button onClick={chooseLocation}>Choose…</button>
        </div>
        <span className="hint">
          A folder will be created here named after the project.
        </span>
      </label>

      <div className="row row--end">
        <button onClick={props.onCancel} disabled={creating}>
          Cancel
        </button>
        <button
          className="primary"
          onClick={create}
          disabled={!name.trim() || !saveLocation || creating}
        >
          {creating ? "Creating…" : "Create"}
        </button>
      </div>
    </div>
  );
}

/* ---------- Project view ---------- */

function ProjectView(props: {
  project: Project;
  onProjectChange: (p: Project) => void;
  onClose: () => void;
  onError: (msg: string) => void;
}) {
  const { project, onProjectChange, onClose, onError } = props;

  // Local working copy for text fields so typing isn't lagged by Rust IPC.
  // We push to Rust on a debounced timer.
  const [localName, setLocalName] = useState(project.name);
  const [localOpening, setLocalOpening] = useState(project.opening_title_text);
  const [localPrompt, setLocalPrompt] = useState(project.main_prompt);

  // If the project prop changes (e.g. after add_clip), refresh the local
  // text. We only do this when the project's identity (dir) changes — not
  // when clips change — so the user's mid-edit text isn't clobbered.
  const lastDirRef = useRef(project.dir);
  useEffect(() => {
    if (lastDirRef.current !== project.dir) {
      lastDirRef.current = project.dir;
      setLocalName(project.name);
      setLocalOpening(project.opening_title_text);
      setLocalPrompt(project.main_prompt);
    }
  }, [project.dir, project.name, project.opening_title_text, project.main_prompt]);

  // Debounced autosave for text fields. 600ms after the last keystroke we
  // push the whole project back to Rust. Conservative: we never lose work
  // because the input controls remain controlled.
  useEffect(() => {
    const dirty =
      localName !== project.name ||
      localOpening !== project.opening_title_text ||
      localPrompt !== project.main_prompt;
    if (!dirty) return;
    const t = setTimeout(async () => {
      const next: Project = {
        ...project,
        name: localName,
        opening_title_text: localOpening,
        main_prompt: localPrompt,
      };
      try {
        await invoke("save_project", { project: next });
        onProjectChange(next);
      } catch (e: any) {
        onError(e?.message || String(e));
      }
    }, 600);
    return () => clearTimeout(t);
  }, [localName, localOpening, localPrompt, project, onProjectChange, onError]);

  const addClip = async () => {
    try {
      const selected = await open({
        multiple: false,
        directory: false,
        filters: [
          { name: "Training source", extensions: ["mp4", "mov", "pptx"] },
        ],
      });
      if (!selected) return;
      const path = typeof selected === "string" ? selected : (selected as any).path;
      const updated: Project = await invoke("add_clip", {
        projectDir: project.dir,
        sourcePath: path,
      });
      onProjectChange(updated);
    } catch (e: any) {
      onError(e?.message || String(e));
    }
  };

  const removeClip = useCallback(
    async (clipId: string) => {
      if (!confirm(`Remove clip ${clipId}? Its folder and contents will be deleted.`)) {
        return;
      }
      try {
        const updated: Project = await invoke("remove_clip", {
          projectDir: project.dir,
          clipId,
        });
        onProjectChange(updated);
      } catch (e: any) {
        onError(e?.message || String(e));
      }
    },
    [project.dir, onProjectChange, onError],
  );

  return (
    <>
      <div className="status-card">
        <div className="row row--between">
          <div className="status-label">Project</div>
          <button className="small" onClick={onClose}>
            Close project
          </button>
        </div>

        <label className="field">
          <span className="field-label">Name</span>
          <input
            className="text-input"
            value={localName}
            onChange={(e) => setLocalName(e.target.value)}
          />
        </label>

        <label className="field">
          <span className="field-label">Opening title (shown on the first card of the video)</span>
          <input
            className="text-input"
            value={localOpening}
            onChange={(e) => setLocalOpening(e.target.value)}
            placeholder="Leave empty to skip the opening title card"
          />
        </label>

        <label className="field">
          <span className="field-label">Main prompt (context for the AI)</span>
          <textarea
            className="textarea"
            value={localPrompt}
            onChange={(e) => setLocalPrompt(e.target.value)}
            placeholder="Describe what this training video is about and who it's for. The AI uses this to narrate each clip."
            rows={4}
          />
        </label>

        <div className="hint mono">{project.dir}</div>
      </div>

      <div className="status-card">
        <div className="row row--between">
          <div className="status-label">Clips ({project.clips.length})</div>
          <button onClick={addClip}>Add clip…</button>
        </div>

        {project.clips.length === 0 && (
          <p className="muted">
            No clips yet. Add one MP4/MOV per section of the training video
            (e.g. one for each step in the workflow).
          </p>
        )}

        {project.clips.map((clip, i) => (
          <ClipRow
            key={clip.id}
            clip={clip}
            position={i + 1}
            projectDir={project.dir}
            onExtracted={(updated) => onProjectChange(updated)}
            onRemove={() => removeClip(clip.id)}
            onError={onError}
          />
        ))}
      </div>

      <div className="footer">
        Phase 1 milestones:
        <ul>
          <li>✓ Tauri shell + bridge</li>
          <li>✓ Source file picker</li>
          <li>✓ Project model + clip list</li>
          <li>{project.clips.some((c) => c.status !== "draft") ? "✓" : "○"} Extract frames per clip (step b)</li>
          <li>○ AI vision narration per frame (step c)</li>
          <li>○ AI-generated titles (step d)</li>
          <li>○ TTS audio per clip (step e)</li>
          <li>○ Render final MP4 with title cards + crossfades (step f)</li>
        </ul>
      </div>
    </>
  );
}

/* ---------- Clip row ---------- */

function ClipRow(props: {
  clip: Clip;
  position: number;
  projectDir: string;
  onExtracted: (project: Project) => void;
  onRemove: () => void;
  onError: (msg: string) => void;
}) {
  const { clip, position, projectDir, onExtracted, onRemove, onError } = props;
  const [busy, setBusy] = useState(false);
  const [frames, setFrames] = useState<FrameInfo[] | null>(null);
  const [framesExpanded, setFramesExpanded] = useState(false);

  // If the clip is already extracted (e.g. project just loaded from disk),
  // fetch the frame list lazily once — only when the user expands the grid,
  // so opening a project doesn't fan out a Rust call per clip.
  const ensureFramesLoaded = useCallback(async () => {
    if (frames !== null) return;
    try {
      const list: FrameInfo[] = await invoke("list_frames", {
        projectDir,
        clipId: clip.id,
      });
      setFrames(list);
    } catch (e: any) {
      onError(e?.message || String(e));
    }
  }, [frames, projectDir, clip.id, onError]);

  const extract = async () => {
    if (busy) return;
    setBusy(true);
    try {
      const result: ExtractResult = await invoke("extract_frames", {
        projectDir,
        clipId: clip.id,
      });
      setFrames(result.frames);
      setFramesExpanded(true);
      onExtracted(result.project);
    } catch (e: any) {
      onError(e?.message || String(e));
    } finally {
      setBusy(false);
    }
  };

  const toggleFrames = async () => {
    if (!framesExpanded) await ensureFramesLoaded();
    setFramesExpanded((v) => !v);
  };

  const hasFrames = clip.status !== "draft";

  return (
    <div className="clip-row clip-row--block">
      <div className="clip-row__head">
        <div className="clip-row__num">{String(position).padStart(2, "0")}</div>
        <div className="clip-row__body">
          <div className="clip-row__name">{clip.source_name}</div>
          <div className="clip-row__meta">
            {formatBytes(clip.bytes)}
            {clip.duration_seconds != null && ` · ${fmtDuration(clip.duration_seconds)}`}
            {" · "}
            <span className={`status status--${clip.status}`}>
              {clip.status.replace("_", " ")}
            </span>
            {hasFrames && frames !== null && ` · ${frames.length} frames`}
          </div>
          {clip.title && <div className="clip-row__title">"{clip.title}"</div>}
        </div>
        <div className="clip-row__actions">
          {!hasFrames && (
            <button onClick={extract} disabled={busy} className="small">
              {busy ? "Extracting…" : "Extract frames"}
            </button>
          )}
          {hasFrames && (
            <>
              <button onClick={toggleFrames} className="small">
                {framesExpanded ? "Hide frames" : "View frames"}
              </button>
              <button onClick={extract} disabled={busy} className="small">
                {busy ? "Re-extracting…" : "Re-extract"}
              </button>
            </>
          )}
          <button className="small danger" onClick={onRemove} disabled={busy}>
            Remove
          </button>
        </div>
      </div>

      {framesExpanded && frames !== null && (
        <div className="frames-grid">
          {frames.map((f) => (
            <figure key={f.name} className="thumb">
              <img
                src={convertFileSrc(f.path)}
                alt={f.name}
                loading="lazy"
              />
              <figcaption>
                {f.timestamp_seconds != null
                  ? fmtDuration(f.timestamp_seconds)
                  : `slide ${f.name.replace(/\.jpg$/i, "")}`}
              </figcaption>
            </figure>
          ))}
        </div>
      )}
    </div>
  );
}

/* ---------- helpers ---------- */

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

function fmtDuration(s: number): string {
  const m = Math.floor(s / 60);
  const sec = Math.floor(s % 60);
  return `${m}:${String(sec).padStart(2, "0")}`;
}

export default App;
