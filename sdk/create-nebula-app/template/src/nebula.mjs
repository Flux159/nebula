// Minimal zero-dependency client for the Nebula HTTP API (v1alpha1).
// A vendored subset of @nebula-vm/sdk so the scaffold has no install step;
// swap to the published SDK whenever you prefer. Full API reference:
// docs/httpapi.md in the nebula repo.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const appDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const cfg = JSON.parse(fs.readFileSync(path.join(appDir, "nebula.config.json"), "utf8"));

const BASE = process.env.NEBULA_API_URL ?? `http://127.0.0.1:${cfg.apiPort}`;
const TOKEN = process.env.NEBULA_API_TOKEN;

async function req(method, p, body, timeoutMs = 120_000) {
  const headers = {};
  if (body !== undefined) headers["Content-Type"] = "application/json";
  if (TOKEN) headers["Authorization"] = `Bearer ${TOKEN}`;
  const res = await fetch(`${BASE}${p}`, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
    signal: AbortSignal.timeout(timeoutMs),
  });
  const text = await res.text();
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    parsed = text;
  }
  if (!res.ok) throw new Error(parsed?.error ?? `HTTP ${res.status}`);
  return parsed;
}

export const nebula = {
  status: () => req("GET", "/v1alpha1/status"),
  exec: (cmd, args = []) => req("POST", "/v1alpha1/exec", { cmd, args, timeout_ms: 60_000 }),
  /** Raw Docker Engine API call (dockerd on full, slimd on slim). */
  docker: (method, p, body) => req(method, `/docker${p}`, body),
  /** Raw kubernetes apiserver call (slim only; full uses kubeconfig()). */
  k8s: (method, p, body) => req(method, `/k8s${p}`, body),
  vessels: {
    list: () => req("GET", "/v1alpha1/vessels"),
    create: (opts) => req("POST", "/v1alpha1/vessels", opts, 600_000),
    exec: (name, cmd, args = []) =>
      req("POST", `/v1alpha1/vessels/${name}/exec`, { cmd, args, timeout_ms: 60_000 }),
    snapshot: (name, label, mode = "auto") =>
      req("POST", `/v1alpha1/vessels/${name}/snapshots`, { label, mode }, 300_000),
    restore: (name, label) => req("POST", `/v1alpha1/vessels/${name}/restore`, { label }, 300_000),
    branch: (name, newName, label, count = 1) =>
      req("POST", `/v1alpha1/vessels/${name}/branch`, { new_name: newName, label, count }, 600_000),
    rm: (name) => req("DELETE", `/v1alpha1/vessels/${name}?force=true`, undefined, 120_000),
  },
};
