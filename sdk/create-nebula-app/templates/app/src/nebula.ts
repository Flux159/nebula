// Typed client for the Nebula HTTP API (v1alpha1) — webview side.
// Reads go straight to the engine (CORS is open on loopback); actions that
// belong in the app's Rust base layer go through Tauri commands (see
// src-tauri/src/nebula.rs — that is where components like model-config
// plug in). Full API reference: docs/httpapi.md in the nebula repo.

import { invoke } from "@tauri-apps/api/core";
import cfg from "../nebula.config.json";

const BASE = `http://127.0.0.1:${cfg.apiPort}`;

async function req<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    method,
    headers: body !== undefined ? { "Content-Type": "application/json" } : undefined,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const parsed = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error((parsed as { error?: string }).error ?? `HTTP ${res.status}`);
  return parsed as T;
}

export interface EngineStatus {
  vmState: string;
  cpus: number;
  memMib: number;
  agent: { kernel: string; agent_version: string } | null;
}

export const nebula = {
  status: () => req<EngineStatus>("GET", "/v1alpha1/status"),
  containers: () => req<unknown[]>("GET", "/docker/v1.43/containers/json?all=true"),
  /** Runs in the Rust base layer (hyper) — the pattern components extend. */
  forkDemo: () => invoke<string>("fork_demo"),
};
