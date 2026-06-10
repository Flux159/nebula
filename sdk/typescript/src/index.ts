/**
 * Nebula SDK (v1alpha1): talk to a local (or, later, remote) Nebula engine.
 *
 * ```ts
 * import { NebulaClient } from "@nebula-vm/sdk";
 * const nebula = new NebulaClient();
 * const status = await nebula.status();
 * const result = await nebula.exec("uname", ["-a"]);
 * ```
 */

export interface AgentHealth {
  proto_version: number;
  agent_version: string;
  kernel: string;
  uptime_secs: number;
  ip?: string | null;
}

export interface MemStats {
  total_kib: number;
  free_kib: number;
  available_kib: number;
  cached_kib: number;
  psi_some_avg10?: number | null;
  psi_full_avg10?: number | null;
}

export interface EngineStatus {
  apiVersion: string;
  vmState: string;
  cpus: number;
  memMib: number;
  agent: AgentHealth | null;
  memory: MemStats | null;
  uptimeSecs: number;
}

export interface EngineStats {
  guest: MemStats | null;
  balloonTargetMib: number;
  maxMib: number;
  hostFootprintMib: number;
}

export interface ExecResult {
  exit_code: number;
  stdout: string;
  stderr: string;
  timed_out: boolean;
}

/** Container shape follows the Docker Engine API ContainerSummary. */
export interface Container {
  Id: string;
  Names: string[];
  Image: string;
  State: string;
  Status: string;
  Ports: { PrivatePort: number; PublicPort?: number; Type: string }[];
  Labels: Record<string, string>;
}

export interface NebulaClientOptions {
  /** Base URL of the engine API. Default: http://127.0.0.1:7440 */
  baseUrl?: string;
  /** Per-request timeout in milliseconds. Default: 30s. */
  timeoutMs?: number;
}

export class NebulaError extends Error {
  constructor(
    message: string,
    public readonly status?: number,
  ) {
    super(message);
    this.name = "NebulaError";
  }
}

export class NebulaClient {
  private baseUrl: string;
  private timeoutMs: number;

  constructor(opts: NebulaClientOptions = {}) {
    this.baseUrl = (opts.baseUrl ?? "http://127.0.0.1:7440").replace(/\/$/, "");
    this.timeoutMs = opts.timeoutMs ?? 30_000;
  }

  /** Engine + guest agent status. */
  status(): Promise<EngineStatus> {
    return this.request("GET", "/v1alpha1/status");
  }

  /** Live memory/balloon/footprint stats. */
  stats(): Promise<EngineStats> {
    return this.request("GET", "/v1alpha1/stats");
  }

  /** Run a command inside the Vessel. */
  exec(cmd: string, args: string[] = [], timeoutMs = 30_000): Promise<ExecResult> {
    return this.request("POST", "/v1alpha1/exec", {
      cmd,
      args,
      timeout_ms: timeoutMs,
    });
  }

  /** List containers (Docker Engine API shape). */
  containers(): Promise<Container[]> {
    return this.request("GET", "/v1alpha1/containers");
  }

  /** True when the engine API is reachable. */
  async isRunning(): Promise<boolean> {
    try {
      await this.request("GET", "/healthz");
      return true;
    } catch {
      return false;
    }
  }

  private async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await fetch(`${this.baseUrl}${path}`, {
      method,
      headers: body !== undefined ? { "Content-Type": "application/json" } : undefined,
      body: body !== undefined ? JSON.stringify(body) : undefined,
      signal: AbortSignal.timeout(this.timeoutMs),
    });
    const text = await res.text();
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      throw new NebulaError(`non-JSON response: ${text.slice(0, 200)}`, res.status);
    }
    if (!res.ok) {
      const message =
        typeof parsed === "object" && parsed !== null && "error" in parsed
          ? String((parsed as { error: unknown }).error)
          : `HTTP ${res.status}`;
      throw new NebulaError(message, res.status);
    }
    return parsed as T;
  }
}
