import { invoke } from "@tauri-apps/api/core";

// Mirrors the serde types in src-tauri/src/state.rs.
export type ProjectSummary = { slug: string; name: string };

export type Status = {
  connected: boolean;
  node_name: string;
  pid: number | null;
  updated_at: string;
  // null: the node has not reported the orchestrator's projects (not "it has none").
  projects: ProjectSummary[] | null;
};

export type Mapping = { slug: string; path: string };

export type TrayState = {
  data_dir: string;
  mappings: Mapping[];
  status: Status | null;
  // status.pid names a process that is not running: the status is left over from a stopped node.
  status_stale: boolean;
};

export const getState = () => invoke<TrayState>("get_state");

export const mapProject = (slug: string, path: string) =>
  invoke<void>("map_project", { slug, path });

export const unmapProject = (slug: string) => invoke<void>("unmap_project", { slug });

export const openPath = (path: string) => invoke<void>("open_path", { path });

/** Commands reject with the Rust error string; anything else is unexpected but still shown. */
export function message(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
}
