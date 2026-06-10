// Mirror of the Rust Project/Clip types. Keep in sync with src-tauri/src/lib.rs.

export type ClipStatus =
  | "draft"
  | "frames_extracted"
  | "narrated"
  | "audio_ready"
  | "rendered";

export interface Clip {
  id: string; // "01", "02", ...
  source_name: string;
  bytes: number;
  duration_seconds: number | null;
  title: string;
  status: ClipStatus;
}

export interface Project {
  version: number;
  name: string;
  opening_title_text: string;
  main_prompt: string;
  created_at: string; // ISO8601 UTC
  clips: Clip[];
  // `dir` is intentionally absent in the serialized JSON — Rust adds it on load.
  // We rely on it being present on every Project the UI holds.
  dir: string;
}

export interface FrameInfo {
  name: string; // "0001.jpg"
  path: string; // absolute path to the JPG on disk
  timestamp_seconds: number | null; // null for PPTX slides
}

export interface ExtractResult {
  project: Project;
  frames: FrameInfo[];
}
