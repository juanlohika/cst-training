import { useCallback, useEffect, useRef, useState } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { homeDir, downloadDir } from "@tauri-apps/api/path";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  Project,
  Clip,
  FrameInfo,
  ExtractResult,
  Narration,
  NarrationEntry,
  NarrationProgress,
  TtsProgress,
  RenderProgress,
  Scan,
  ScanProgress,
  Plan,
  PlanSection,
  ScriptUnit,
  ScriptUnitKind,
} from "./types";
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

  const [generatingTitles, setGeneratingTitles] = useState(false);
  const generateTitles = async () => {
    if (generatingTitles) return;
    setGeneratingTitles(true);
    try {
      const updated: Project = await invoke("generate_titles", {
        projectDir: project.dir,
      });
      // Force-sync local text fields to whatever the AI wrote — the
      // identity-based useEffect above only refreshes on dir change,
      // but we want the new opening title to populate the input box.
      setLocalOpening(updated.opening_title_text);
      onProjectChange(updated);
    } catch (e: any) {
      onError(e?.message || String(e));
    } finally {
      setGeneratingTitles(false);
    }
  };

  const updateClipTitle = useCallback(
    async (clipId: string, title: string) => {
      const next: Project = {
        ...project,
        clips: project.clips.map((c) => (c.id === clipId ? { ...c, title } : c)),
      };
      try {
        await invoke("save_project", { project: next });
        onProjectChange(next);
      } catch (e: any) {
        onError(e?.message || String(e));
      }
    },
    [project, onProjectChange, onError],
  );

  const updateClipOverview = useCallback(
    async (clipId: string, overview: string) => {
      const next: Project = {
        ...project,
        clips: project.clips.map((c) => (c.id === clipId ? { ...c, overview } : c)),
      };
      try {
        await invoke("save_project", { project: next });
        onProjectChange(next);
      } catch (e: any) {
        onError(e?.message || String(e));
      }
    },
    [project, onProjectChange, onError],
  );

  // Only allow Generate titles once at least one clip has narration —
  // otherwise the AI has no context to title from.
  const hasAnyNarration = project.clips.some(
    (c) => c.status === "narrated" || c.status === "audio_ready" || c.status === "rendered",
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

        <div className="row row--between">
          <span className="hint">
            {hasAnyNarration
              ? "Generate titles uses your narrations + the project context to write the opening title and per-clip section titles. You can edit them afterward."
              : "Narrate at least one clip before generating titles — the AI needs that context."}
          </span>
          <button
            onClick={generateTitles}
            disabled={!hasAnyNarration || generatingTitles}
            className="small"
          >
            {generatingTitles ? "Generating…" : "Generate titles"}
          </button>
        </div>

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
            onNarrated={(updated) => onProjectChange(updated)}
            onTitleChange={(title) => updateClipTitle(clip.id, title)}
            onOverviewChange={(overview) => updateClipOverview(clip.id, overview)}
            onRemove={() => removeClip(clip.id)}
            onError={onError}
          />
        ))}
      </div>

      <RenderSection
        project={project}
        onError={onError}
        onProjectChange={onProjectChange}
      />

      <div className="footer">
        Phase 1 milestones:
        <ul>
          <li>✓ Tauri shell + bridge</li>
          <li>✓ Source file picker</li>
          <li>✓ Project model + clip list</li>
          <li>{project.clips.some((c) => c.status !== "draft") ? "✓" : "○"} Extract frames per clip (step b)</li>
          <li>
            {project.clips.some(
              (c) => c.status === "narrated" || c.status === "audio_ready" || c.status === "rendered",
            )
              ? "✓"
              : "○"}{" "}
            AI vision narration per frame (step c)
          </li>
          <li>
            {(project.opening_title_text.trim() ||
              project.clips.some((c) => c.title.trim()))
              ? "✓"
              : "○"}{" "}
            AI-generated titles (step d)
          </li>
          <li>
            {project.clips.some(
              (c) => c.status === "audio_ready" || c.status === "rendered",
            )
              ? "✓"
              : "○"}{" "}
            TTS audio per clip (step e)
          </li>
          <li>
            {project.clips.some((c) => c.status === "rendered") ? "✓" : "○"}{" "}
            Render final MP4 with title cards + crossfades (step f)
          </li>
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
  onNarrated: (project: Project) => void;
  onTitleChange: (title: string) => void;
  onOverviewChange: (overview: string) => void;
  onRemove: () => void;
  onError: (msg: string) => void;
}) {
  const { clip, position, projectDir, onExtracted, onNarrated, onTitleChange, onOverviewChange, onRemove, onError } = props;
  const [busy, setBusy] = useState<"extract" | "narrate" | "audio" | null>(null);
  const [frames, setFrames] = useState<FrameInfo[] | null>(null);
  const [framesExpanded, setFramesExpanded] = useState(false);
  const [narration, setNarration] = useState<Narration | null>(null);
  const [narrationExpanded, setNarrationExpanded] = useState(false);
  const [narrationProgress, setNarrationProgress] = useState<NarrationProgress | null>(null);
  const [ttsProgress, setTtsProgress] = useState<TtsProgress | null>(null);
  const [localTitle, setLocalTitle] = useState(clip.title);
  const [localOverview, setLocalOverview] = useState(clip.overview ?? "");
  const [generatingOverview, setGeneratingOverview] = useState(false);

  // Phase 1.7 — preview-only smart-scan state (temporary smoke-test UI;
  // proper integration into a "Smart narrate" flow comes in 1.7h).
  const [scanning, setScanning] = useState(false);
  const [scanProgress, setScanProgress] = useState<ScanProgress | null>(null);
  const [scan, setScan] = useState<Scan | null>(null);
  const [scanExpanded, setScanExpanded] = useState(false);

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    (async () => {
      unlisten = await listen<ScanProgress>("scan-progress", (event) => {
        if (event.payload.clip_id !== clip.id) return;
        setScanProgress(event.payload);
      });
    })();
    return () => {
      unlisten?.();
    };
  }, [clip.id]);

  // Auto-load any existing scan.json on first expand.
  const ensureScanLoaded = async () => {
    if (scan !== null) return;
    try {
      const result: Scan | null = await invoke("load_scan", {
        projectDir,
        clipId: clip.id,
      });
      if (result) setScan(result);
    } catch (e: any) {
      onError(e?.message || String(e));
    }
  };

  const runSmartScan = async () => {
    if (scanning) return;
    setScanning(true);
    setScanProgress(null);
    try {
      const result: Scan = await invoke("scan_clip", {
        projectDir,
        clipId: clip.id,
      });
      setScan(result);
      setScanExpanded(true);
    } catch (e: any) {
      onError(e?.message || String(e));
    } finally {
      setScanning(false);
      setScanProgress(null);
    }
  };

  // Phase 1.7c — plan state.
  const [planning, setPlanning] = useState(false);
  const [plan, setPlan] = useState<Plan | null>(null);
  const [planExpanded, setPlanExpanded] = useState(false);

  const ensurePlanLoaded = async () => {
    if (plan !== null) return;
    try {
      const result: Plan | null = await invoke("load_plan", {
        projectDir,
        clipId: clip.id,
      });
      if (result) setPlan(result);
    } catch (e: any) {
      onError(e?.message || String(e));
    }
  };

  const runPlanScript = async () => {
    if (planning) return;
    setPlanning(true);
    try {
      const result: Plan = await invoke("plan_script", {
        projectDir,
        clipId: clip.id,
      });
      setPlan(result);
      setPlanExpanded(true);
    } catch (e: any) {
      onError(e?.message || String(e));
    } finally {
      setPlanning(false);
    }
  };

  // Persist plan edits to disk (debounced caller invokes this).
  const savePlan = useCallback(
    async (next: Plan) => {
      try {
        await invoke("update_plan", {
          projectDir,
          clipId: clip.id,
          plan: next,
        });
      } catch (e: any) {
        onError(e?.message || String(e));
      }
    },
    [projectDir, clip.id, onError],
  );

  const toggleFrameExcluded = async (frameName: string, excluded: boolean) => {
    try {
      const result: Plan = await invoke("toggle_frame_excluded", {
        projectDir,
        clipId: clip.id,
        frameName,
        excluded,
      });
      setPlan(result);
    } catch (e: any) {
      onError(e?.message || String(e));
    }
  };

  // Sync title + overview from props when the AI fills them in from outside.
  const lastTitleFromPropRef = useRef(clip.title);
  const lastOverviewFromPropRef = useRef(clip.overview ?? "");
  useEffect(() => {
    if (clip.title !== lastTitleFromPropRef.current) {
      lastTitleFromPropRef.current = clip.title;
      setLocalTitle(clip.title);
    }
  }, [clip.title]);
  useEffect(() => {
    const prop = clip.overview ?? "";
    if (prop !== lastOverviewFromPropRef.current) {
      lastOverviewFromPropRef.current = prop;
      setLocalOverview(prop);
    }
  }, [clip.overview]);

  // Debounce title edits to disk same as the project-level fields.
  useEffect(() => {
    if (localTitle === clip.title) return;
    const t = setTimeout(() => {
      onTitleChange(localTitle);
      lastTitleFromPropRef.current = localTitle;
    }, 600);
    return () => clearTimeout(t);
  }, [localTitle, clip.title, onTitleChange]);

  // Debounce overview edits to disk.
  useEffect(() => {
    if (localOverview === (clip.overview ?? "")) return;
    const t = setTimeout(() => {
      onOverviewChange(localOverview);
      lastOverviewFromPropRef.current = localOverview;
    }, 600);
    return () => clearTimeout(t);
  }, [localOverview, clip.overview, onOverviewChange]);

  const generateOverview = async () => {
    if (generatingOverview) return;
    setGeneratingOverview(true);
    try {
      const updated: Project = await invoke("generate_overview", {
        projectDir,
        clipId: clip.id,
      });
      // Find this clip in the updated project and sync local overview.
      const updatedClip = updated.clips.find((c) => c.id === clip.id);
      if (updatedClip) {
        setLocalOverview(updatedClip.overview ?? "");
        lastOverviewFromPropRef.current = updatedClip.overview ?? "";
      }
      onNarrated(updated);
    } catch (e: any) {
      onError(e?.message || String(e));
    } finally {
      setGeneratingOverview(false);
    }
  };

  // Live-stream narration progress events from Rust.
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    (async () => {
      unlisten = await listen<NarrationProgress>("narration-progress", (event) => {
        if (event.payload.clip_id !== clip.id) return;
        setNarrationProgress(event.payload);
      });
    })();
    return () => {
      unlisten?.();
    };
  }, [clip.id]);

  // Live-stream TTS progress events from Rust.
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    (async () => {
      unlisten = await listen<TtsProgress>("tts-progress", (event) => {
        if (event.payload.clip_id !== clip.id) return;
        setTtsProgress(event.payload);
      });
    })();
    return () => {
      unlisten?.();
    };
  }, [clip.id]);

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

  const ensureNarrationLoaded = useCallback(async () => {
    if (narration !== null) return;
    try {
      const n: Narration = await invoke("load_narration", {
        projectDir,
        clipId: clip.id,
      });
      setNarration(n);
    } catch (e: any) {
      onError(e?.message || String(e));
    }
  }, [narration, projectDir, clip.id, onError]);

  const extract = async () => {
    if (busy) return;
    setBusy("extract");
    try {
      const result: ExtractResult = await invoke("extract_frames", {
        projectDir,
        clipId: clip.id,
      });
      setFrames(result.frames);
      setFramesExpanded(true);
      // Wipe any prior narration since frames have changed.
      setNarration(null);
      onExtracted(result.project);
    } catch (e: any) {
      onError(e?.message || String(e));
    } finally {
      setBusy(null);
    }
  };

  const narrate = async () => {
    if (busy) return;
    setBusy("narrate");
    setNarrationProgress(null);
    setNarration({ version: 1, entries: [] });
    setNarrationExpanded(true);
    try {
      const result: Narration = await invoke("narrate_clip", {
        projectDir,
        clipId: clip.id,
      });
      setNarration(result);
      // Reload project so status flips to "narrated".
      const updated: Project = await invoke("load_project", { projectDir });
      onNarrated(updated);
    } catch (e: any) {
      // "Cancelled by user." is expected when the Stop button is used —
      // don't show it as an error.
      const msg = e?.message || String(e);
      if (!msg.includes("Cancelled by user")) {
        onError(msg);
      }
    } finally {
      setBusy(null);
      setNarrationProgress(null);
    }
  };

  const stopNarration = async () => {
    try {
      await invoke("cancel_narration", { projectDir, clipId: clip.id });
    } catch (e: any) {
      onError(e?.message || String(e));
    }
  };

  const generateAudio = async () => {
    if (busy) return;
    setBusy("audio");
    setTtsProgress(null);
    try {
      await invoke("generate_audio", { projectDir, clipId: clip.id });
      // Status flipped server-side. Reload project so UI reflects audio_ready.
      const updated: Project = await invoke("load_project", { projectDir });
      onNarrated(updated); // reuse the same prop — it just re-applies the project
    } catch (e: any) {
      onError(e?.message || String(e));
    } finally {
      setBusy(null);
      setTtsProgress(null);
    }
  };

  const toggleFrames = async () => {
    if (!framesExpanded) await ensureFramesLoaded();
    setFramesExpanded((v) => !v);
  };

  const toggleNarration = async () => {
    if (!narrationExpanded) await ensureNarrationLoaded();
    setNarrationExpanded((v) => !v);
  };

  const hasFrames = clip.status !== "draft";
  const hasNarration =
    clip.status === "narrated" ||
    clip.status === "audio_ready" ||
    clip.status === "rendered";

  // Build a live "fresh" view of the narration during streaming so the
  // user sees text materializing rather than only at the very end.
  const liveEntries: NarrationEntry[] = narration?.entries ?? [];

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
            {hasNarration && narration && ` · ${narration.entries.filter((e) => e.text).length} narrated`}
          </div>
          <input
            className="clip-row__title-input"
            value={localTitle}
            onChange={(e) => setLocalTitle(e.target.value)}
            placeholder="Section title (appears on the title card before this clip)"
          />
          {hasNarration && (
            <div className="clip-row__overview">
              <div className="clip-row__overview-label">
                <span>Section overview (plays during the title card)</span>
                <button
                  className="link"
                  onClick={generateOverview}
                  disabled={generatingOverview}
                >
                  {generatingOverview
                    ? "Generating…"
                    : (clip.overview ?? "").trim()
                    ? "Regenerate overview"
                    : "Generate overview"}
                </button>
              </div>
              <textarea
                className="clip-row__overview-input"
                value={localOverview}
                onChange={(e) => setLocalOverview(e.target.value)}
                placeholder="The narrator's intro to this section (auto-generated by 'Generate overview', editable)."
                rows={2}
              />
            </div>
          )}
        </div>
        <div className="clip-row__actions">
          {!hasFrames && (
            <button onClick={extract} disabled={busy != null} className="small">
              {busy === "extract" ? "Extracting…" : "Extract frames"}
            </button>
          )}
          {hasFrames && (
            <>
              <button onClick={toggleFrames} className="small">
                {framesExpanded ? "Hide frames" : "View frames"}
              </button>
              <button onClick={extract} disabled={busy != null} className="small">
                {busy === "extract" ? "Re-extracting…" : "Re-extract"}
              </button>
              <button
                onClick={runSmartScan}
                disabled={busy != null || scanning}
                className="small"
                title="Phase 1.7 preview — AI looks at all thumbnails and picks the key frames."
              >
                {scanning
                  ? scanProgress
                    ? `Scanning… (${scanProgress.stage})`
                    : "Scanning…"
                  : scan
                  ? "Re-scan"
                  : "Smart scan (preview)"}
              </button>
              {scan && !scanning && (
                <button
                  onClick={async () => {
                    if (!scanExpanded) await ensureScanLoaded();
                    setScanExpanded((v) => !v);
                  }}
                  className="small"
                >
                  {scanExpanded ? "Hide scan" : "View scan"}
                </button>
              )}
              {scan && !scanning && (
                <button
                  onClick={runPlanScript}
                  disabled={busy != null || planning}
                  className="small"
                  title="Phase 1.7c — turn the scan into a per-section script plan (editable)."
                >
                  {planning ? "Planning…" : plan ? "Re-plan" : "Plan script"}
                </button>
              )}
              {plan && !planning && (
                <button
                  onClick={async () => {
                    if (!planExpanded) await ensurePlanLoaded();
                    setPlanExpanded((v) => !v);
                  }}
                  className="small"
                >
                  {planExpanded ? "Hide plan" : "View plan"}
                </button>
              )}
              <button onClick={narrate} disabled={busy != null} className="small">
                {busy === "narrate"
                  ? narrationProgress
                    ? `Narrating ${narrationProgress.index}/${narrationProgress.total}…`
                    : "Starting…"
                  : hasNarration
                  ? "Re-narrate"
                  : "Narrate clip"}
              </button>
              {busy === "narrate" && (
                <button onClick={stopNarration} className="small danger">
                  Stop
                </button>
              )}
              {hasNarration && busy !== "narrate" && (
                <button onClick={toggleNarration} className="small">
                  {narrationExpanded ? "Hide script" : "View script"}
                </button>
              )}
              {hasNarration && (
                <button
                  onClick={generateAudio}
                  disabled={busy != null}
                  className="small"
                >
                  {busy === "audio"
                    ? ttsProgress
                      ? ttsProgress.stage === "loading"
                        ? "Loading TTS model…"
                        : ttsProgress.stage === "loaded"
                        ? "Generating audio…"
                        : ttsProgress.stage === "progress"
                        ? `Audio ${ttsProgress.index}/${ttsProgress.total}…`
                        : "Finishing…"
                      : "Starting…"
                    : clip.status === "audio_ready" || clip.status === "rendered"
                    ? "Re-generate audio"
                    : "Generate audio"}
                </button>
              )}
            </>
          )}
          <button className="small danger" onClick={onRemove} disabled={busy != null}>
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

      {(narrationExpanded || busy === "narrate") && (
        <div className="narration-panel">
          {liveEntries.length === 0 && busy !== "narrate" && (
            <p className="muted">No narration yet.</p>
          )}
          {liveEntries
            .filter((e) => e.text != null)
            .map((e) => (
              <NarrationEditableLine
                key={e.name}
                entry={e}
                projectDir={projectDir}
                clipId={clip.id}
                onChange={(updated) => setNarration(updated)}
                onError={onError}
              />
            ))}
          {busy === "narrate" && narrationProgress?.inherited && (
            <p className="hint">
              Frame {narrationProgress.index}/{narrationProgress.total}: identical to previous,
              reusing narration…
            </p>
          )}
        </div>
      )}

      {scanning && scanProgress && (
        <div className="scan-progress">
          <span className="render-progress__stage">{scanProgress.stage}</span>{" "}
          {scanProgress.detail}
        </div>
      )}

      {scanExpanded && scan && (
        <div className="scan-panel">
          <div className="scan-panel__section">
            <div className="scan-panel__label">Inferred mode</div>
            <div className="scan-panel__mode">{scan.inferred_mode}</div>
          </div>
          <div className="scan-panel__section">
            <div className="scan-panel__label">Narrative arc</div>
            <div className="scan-panel__arc">{scan.narrative_arc}</div>
          </div>
          <div className="scan-panel__section">
            <div className="scan-panel__label">
              Key frames ({scan.key_frames.length})
            </div>
            {scan.key_frames.map((kf) => (
              <div key={kf.name} className={`scan-keyframe scan-keyframe--${kf.type}`}>
                <span className="scan-keyframe__name">{kf.name.replace(/\.jpg$/, "")}</span>
                <span className={`scan-keyframe__type scan-keyframe__type--${kf.type}`}>
                  {kf.type === "section_divider" ? "section" : "step"}
                </span>
                <div className="scan-keyframe__body">
                  {kf.title && (
                    <div className="scan-keyframe__title">"{kf.title}"</div>
                  )}
                  {kf.ui_action && (
                    <div className="scan-keyframe__action">→ {kf.ui_action}</div>
                  )}
                  <div className="scan-keyframe__summary">{kf.summary}</div>
                  {kf.implicit_topic && (
                    <div className="scan-keyframe__topic">topic: {kf.implicit_topic}</div>
                  )}
                </div>
              </div>
            ))}
          </div>
        </div>
      )}

      {planExpanded && plan && (
        <PlanEditor
          plan={plan}
          onChange={(p) => {
            setPlan(p);
            savePlan(p);
          }}
          onToggleExcluded={toggleFrameExcluded}
        />
      )}
    </div>
  );
}

/* ---------- Plan editor ---------- */

function PlanEditor(props: {
  plan: Plan;
  onChange: (plan: Plan) => void;
  onToggleExcluded: (frameName: string, excluded: boolean) => void;
}) {
  const { plan, onChange, onToggleExcluded } = props;

  // Collect all frames referenced by units so we can show a clean
  // "excluded frames" UI alongside them.
  const allFrames = plan.sections.flatMap((s) =>
    s.units.flatMap((u) => u.frames),
  );
  const isExcluded = (frameName: string) =>
    plan.excluded_frames.includes(frameName);

  const updateSection = (sectionId: string, patch: Partial<PlanSection>) => {
    const next = {
      ...plan,
      sections: plan.sections.map((s) =>
        s.id === sectionId ? { ...s, ...patch } : s,
      ),
    };
    onChange(next);
  };

  const updateUnit = (
    sectionId: string,
    unitId: string,
    patch: Partial<ScriptUnit>,
  ) => {
    const next = {
      ...plan,
      sections: plan.sections.map((s) =>
        s.id === sectionId
          ? {
              ...s,
              units: s.units.map((u) =>
                u.id === unitId ? { ...u, ...patch } : u,
              ),
            }
          : s,
      ),
    };
    onChange(next);
  };

  return (
    <div className="plan-editor">
      <div className="plan-editor__header">
        <div className="status-label">Script plan ({plan.sections.length} section{plan.sections.length === 1 ? "" : "s"})</div>
        {plan.excluded_frames.length > 0 && (
          <span className="plan-editor__excluded-count">
            {plan.excluded_frames.length} frame
            {plan.excluded_frames.length === 1 ? "" : "s"} excluded
          </span>
        )}
      </div>
      <p className="hint">
        Every text field below autosaves on edit. Exclude a frame to drop it
        from the final video (it stays on disk).
      </p>

      {plan.sections.map((section, si) => (
        <div key={section.id} className="plan-section">
          <div className="plan-section__head">
            <span className="plan-section__num">{(si + 1).toString().padStart(2, "0")}</span>
            <div className="plan-section__body">
              <label className="field">
                <span className="field-label">Section title</span>
                <input
                  className="text-input"
                  value={section.title}
                  onChange={(e) =>
                    updateSection(section.id, { title: e.target.value })
                  }
                  placeholder="Section title…"
                />
              </label>
              <label className="field">
                <span className="field-label">
                  Section overview (narrator's intro for this section)
                </span>
                <textarea
                  className="textarea"
                  value={section.overview}
                  onChange={(e) =>
                    updateSection(section.id, { overview: e.target.value })
                  }
                  rows={3}
                  placeholder="The narrator says this when the section begins…"
                />
              </label>
            </div>
          </div>

          <div className="plan-units">
            {section.units.map((unit) => (
              <div
                key={unit.id}
                className={`plan-unit plan-unit--${unit.type}`}
              >
                <div className="plan-unit__head">
                  <select
                    className="plan-unit__type"
                    value={unit.type}
                    onChange={(e) =>
                      updateUnit(section.id, unit.id, {
                        type: e.target.value as ScriptUnitKind,
                        // If switching to filler/title_card, clear text
                        text:
                          e.target.value === "filler" ||
                          e.target.value === "title_card"
                            ? null
                            : unit.text,
                      })
                    }
                  >
                    <option value="instruction">Instruction</option>
                    <option value="title_card">Title card</option>
                    <option value="filler">Filler</option>
                  </select>
                  <div className="plan-unit__frames">
                    {unit.frames.map((f) => (
                      <button
                        key={f}
                        className={`plan-frame-chip ${
                          isExcluded(f) ? "plan-frame-chip--excluded" : ""
                        }`}
                        onClick={() => onToggleExcluded(f, !isExcluded(f))}
                        title={
                          isExcluded(f)
                            ? "Excluded — click to include"
                            : "Click to exclude this frame from the final video"
                        }
                      >
                        {f.replace(/\.jpg$/, "")}
                        {isExcluded(f) && " ✕"}
                      </button>
                    ))}
                  </div>
                </div>
                {unit.type === "instruction" && (
                  <textarea
                    className="plan-unit__text"
                    value={unit.text ?? ""}
                    onChange={(e) =>
                      updateUnit(section.id, unit.id, { text: e.target.value })
                    }
                    rows={2}
                    placeholder="What the narrator says for these frames…"
                  />
                )}
                {unit.type === "title_card" && (
                  <div className="plan-unit__hint">
                    Silent title card. Audio comes from this section's overview.
                  </div>
                )}
                {unit.type === "filler" && (
                  <div className="plan-unit__hint">
                    Filler — frames briefly shown, no narration.
                  </div>
                )}
              </div>
            ))}
          </div>
        </div>
      ))}

      {allFrames.length > 0 && plan.excluded_frames.length > 0 && (
        <div className="plan-editor__excluded">
          <div className="status-label">Excluded frames</div>
          <div className="plan-editor__excluded-list">
            {plan.excluded_frames.map((f) => (
              <button
                key={f}
                className="plan-frame-chip plan-frame-chip--excluded"
                onClick={() => onToggleExcluded(f, false)}
                title="Click to include this frame in the final video"
              >
                {f.replace(/\.jpg$/, "")} ✕
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

/* ---------- Editable narration line ---------- */

function NarrationEditableLine(props: {
  entry: NarrationEntry;
  projectDir: string;
  clipId: string;
  onChange: (narration: Narration) => void;
  onError: (msg: string) => void;
}) {
  const { entry, projectDir, clipId, onChange, onError } = props;
  const [localText, setLocalText] = useState(entry.text ?? "");
  const [saving, setSaving] = useState(false);
  const [regenerating, setRegenerating] = useState(false);
  const lastEntryTextRef = useRef(entry.text ?? "");

  // If the entry's text changes from outside (e.g. AI re-narrate), refresh
  // our local copy unless the user is mid-edit. Detect "user is mid-edit"
  // by checking if the local text differs from the LAST known prop value.
  useEffect(() => {
    const propText = entry.text ?? "";
    const localMatchesLastProp = localText === lastEntryTextRef.current;
    if (propText !== lastEntryTextRef.current && localMatchesLastProp) {
      // Prop changed and user hadn't edited — sync.
      setLocalText(propText);
    }
    lastEntryTextRef.current = propText;
  }, [entry.text, localText]);

  // Debounced save.
  useEffect(() => {
    const propText = entry.text ?? "";
    if (localText === propText) return;
    const t = setTimeout(async () => {
      setSaving(true);
      try {
        const updated: Narration = await invoke("update_narration_entry", {
          projectDir,
          clipId,
          frameName: entry.name,
          newText: localText,
        });
        onChange(updated);
      } catch (e: any) {
        onError(e?.message || String(e));
      } finally {
        setSaving(false);
      }
    }, 600);
    return () => clearTimeout(t);
  }, [localText, entry.text, entry.name, projectDir, clipId, onChange, onError]);

  const regenerateAudio = async () => {
    if (regenerating) return;
    setRegenerating(true);
    try {
      await invoke("regenerate_entry_audio", {
        projectDir,
        clipId,
        frameName: entry.name,
      });
    } catch (e: any) {
      onError(e?.message || String(e));
    } finally {
      setRegenerating(false);
    }
  };

  const isEdited = localText !== (entry.text ?? "") || saving;

  return (
    <div className="narration-line narration-line--editable">
      <span className="narration-line__ts">
        {entry.timestamp_seconds != null
          ? fmtDuration(entry.timestamp_seconds)
          : `slide ${entry.name.replace(/\.jpg$/i, "")}`}
      </span>
      <div className="narration-line__editor">
        <textarea
          className="narration-line__textarea"
          value={localText}
          onChange={(e) => setLocalText(e.target.value)}
          rows={Math.max(2, Math.ceil(localText.length / 60))}
          spellCheck
        />
        <div className="narration-line__actions">
          <span className="narration-line__status">
            {saving ? "Saving…" : isEdited ? "Unsaved" : ""}
          </span>
          <button
            className="small"
            onClick={regenerateAudio}
            disabled={regenerating || saving || !localText.trim()}
          >
            {regenerating ? "Regenerating…" : "Regenerate audio"}
          </button>
        </div>
      </div>
    </div>
  );
}

/* ---------- Render section ---------- */

function RenderSection(props: {
  project: Project;
  onError: (msg: string) => void;
  onProjectChange: (p: Project) => void;
}) {
  const { project, onError, onProjectChange } = props;
  const [rendering, setRendering] = useState(false);
  const [progress, setProgress] = useState<RenderProgress | null>(null);

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    (async () => {
      unlisten = await listen<RenderProgress>("render-progress", (event) => {
        setProgress(event.payload);
      });
    })();
    return () => {
      unlisten?.();
    };
  }, []);

  const readyClips = project.clips.filter(
    (c) => c.status === "audio_ready" || c.status === "rendered",
  );
  const canRender = readyClips.length > 0;
  const allRendered =
    project.clips.length > 0 &&
    project.clips.every((c) => c.status === "rendered");

  const render = async () => {
    if (rendering) return;
    setRendering(true);
    setProgress(null);
    try {
      await invoke("render_video", { projectDir: project.dir });
      // Reload project so statuses flip to "rendered".
      const updated: Project = await invoke("load_project", {
        projectDir: project.dir,
      });
      onProjectChange(updated);
    } catch (e: any) {
      onError(e?.message || String(e));
    } finally {
      setRendering(false);
    }
  };

  const openOutput = async () => {
    try {
      const { openPath } = await import("@tauri-apps/plugin-opener");
      await openPath(`${project.dir}/output.mp4`);
    } catch (e: any) {
      onError(e?.message || String(e));
    }
  };

  const revealOutput = async () => {
    try {
      const { revealItemInDir } = await import("@tauri-apps/plugin-opener");
      await revealItemInDir(`${project.dir}/output.mp4`);
    } catch (e: any) {
      onError(e?.message || String(e));
    }
  };

  return (
    <div className="status-card">
      <div className="status-label">Render</div>

      {!canRender && (
        <p className="muted">
          Generate audio on at least one clip to enable rendering.
        </p>
      )}

      {canRender && !rendering && !allRendered && (
        <p className="muted">
          {readyClips.length} of {project.clips.length} clips ready.
          {readyClips.length < project.clips.length &&
            " (Only clips with audio will be included.)"}
        </p>
      )}

      {allRendered && !rendering && (
        <p className="muted">
          ✓ Video ready: <span className="mono">{project.dir}/output.mp4</span>
        </p>
      )}

      {rendering && progress && (
        <div className="render-progress">
          <div className="render-progress__detail">
            <span className="render-progress__stage">{progress.stage}</span>{" "}
            {progress.detail}
          </div>
          <div className="render-progress__bar">
            <div
              className="render-progress__fill"
              style={{
                width:
                  progress.fraction >= 0
                    ? `${Math.round(progress.fraction * 100)}%`
                    : "100%",
                opacity: progress.fraction >= 0 ? 1 : 0.4,
              }}
            />
          </div>
        </div>
      )}

      <div className="row row--end">
        {allRendered && !rendering && (
          <>
            <button onClick={render} disabled={rendering} className="small">
              Re-render
            </button>
            <button onClick={revealOutput} className="small">
              Show in Finder
            </button>
            <button onClick={openOutput} className="primary">
              ▶ Play output.mp4
            </button>
          </>
        )}
        {(!allRendered || rendering) && (
          <button
            className="primary"
            onClick={render}
            disabled={!canRender || rendering}
          >
            {rendering ? "Rendering…" : "Render video"}
          </button>
        )}
      </div>
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
