// Clients for the two HTTP layers this app is built on:
//   app    — the app's own hyper server (src-tauri/src/server.rs): your
//            API + sqlite persistence. Components add routes there.
//   nebula — the Nebula engine API directly, for plain reads.
// Both are plain fetch — the same frontend runs in the Tauri webview, a
// browser tab (npm run web:dev), or tests. docs/httpapi.md has the engine
// API reference.

import cfg from "../nebula.config.json";

const ENGINE = `http://127.0.0.1:${cfg.apiPort}`;
const APP = `http://127.0.0.1:${cfg.appPort}`;

async function req<T>(base: string, method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`${base}${path}`, {
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
  status: () => req<EngineStatus>(ENGINE, "GET", "/v1alpha1/status"),
  containers: () => req<unknown[]>(ENGINE, "GET", "/docker/v1.43/containers/json?all=true"),
};

export const app = {
  health: () => req<{ ok: boolean }>(APP, "GET", "/api/health"),
  forkDemo: async () => (await req<{ output: string }>(APP, "POST", "/api/fork-demo")).output,
  getSetting: (key: string) =>
    req<{ value: string }>(APP, "GET", `/api/settings/${key}`).then(
      (r) => r.value,
      () => null,
    ),
  setSetting: (key: string, value: string) =>
    req(APP, "PUT", `/api/settings/${key}`, { value }),
};
