/**
 * Nebula SDK (v1alpha1): talk to a local (or remote, token-authed) Nebula
 * engine — full Nebula and Nebula-slim serve the same API.
 *
 * ```ts
 * import { NebulaClient } from "@nebula-vm/sdk";
 * const nebula = new NebulaClient(); // token: NEBULA_API_TOKEN if set
 * await nebula.exec("uname", ["-a"]);
 *
 * // vessels: create, snapshot, fan out 8 live clones
 * await nebula.vessels.create({ name: "agent0" });
 * await nebula.vessels.snapshot("agent0", "s1");
 * await nebula.vessels.branch("agent0", { newName: "fork", label: "s1", count: 8 });
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

/** One host listener the daemon owns: a service (`api`, `dns`) or a
 * published container port (`port 8080`). An entry with `ok: false` is why
 * an otherwise healthy engine serves nothing on that address. */
export interface PortBinding {
  service: string;
  addr: string;
  ok: boolean;
  error?: string | null;
}

export interface EngineStatus {
  apiVersion: string;
  vmState: string;
  cpus: number;
  memMib: number;
  agent: AgentHealth | null;
  memory: MemStats | null;
  uptimeSecs: number;
  /** Absent on daemons older than 0.1.9. */
  ports?: PortBinding[];
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

// --- vessels ---------------------------------------------------------------

export interface VesselSummary {
  name: string;
  running: boolean;
  cpus: number;
  mem_mib: number;
  gpu: boolean;
  backend: string;
}

export type StartOutcome =
  | { outcome: "already_running" }
  | {
      outcome: "started";
      resumed: boolean;
      boot_ms: number;
      kernel: string;
      agent_version: string;
    };

export type StopOutcome = "not_running" | "stopped" | "forced";

export type SnapshotOutcome =
  | { kind: "memory"; ms: number; state_mb: number }
  | {
      kind: "disk_only";
      ms: number;
      reason: "requested" | "backend_unsupported" | "not_running";
    };

export interface SnapshotInfo {
  label: string;
  /** Carries machine state — restore live-resumes mid-execution. */
  memory: boolean;
}

export type RestoreOutcome =
  | ({ outcome: "live_resume" } & {
      resumed: boolean;
      boot_ms: number;
      kernel: string;
      agent_version: string;
    })
  | { outcome: "cold_boot_fallback"; resume_error: string }
  | { outcome: "disk_restore"; restarted: boolean };

export interface BranchOutcome {
  vessels: { name: string; live: boolean; fallback_error: string | null }[];
  from_memory: boolean;
  ms: number;
}

export interface CreateVesselOptions {
  name: string;
  cpus?: number;
  mem_mib?: number;
  gpu?: boolean;
  data_gib?: number;
  backend?: "krun" | "vz";
  /** "name:GiB" strings, mounted at /mnt/<name>. */
  volumes?: string[];
  /** Build the rootfs from a docker image ref (pulled into the engine). */
  from_image?: string;
  /** Clone the rootfs from a raw .img file on the host. */
  rootfs_img?: string;
  /** Rootfs size in MiB when building from an image. */
  rootfs_mb?: number;
  /** Create only — don't boot. */
  no_start?: boolean;
}

export interface NebulaClientOptions {
  /** Base URL of the engine API. Default: http://127.0.0.1:7440 */
  baseUrl?: string;
  /** Per-request timeout in milliseconds. Default: 30s. Slow vessel
   * operations (create/snapshot/restore/branch) use their own, larger
   * defaults unless this is larger. */
  timeoutMs?: number;
  /** Bearer token. Default: process.env.NEBULA_API_TOKEN when available. */
  token?: string;
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
  private token?: string;

  /** Named-vessel lifecycle (microVMs with snapshots and live branching). */
  readonly vessels: Vessels;

  constructor(opts: NebulaClientOptions = {}) {
    this.baseUrl = (opts.baseUrl ?? "http://127.0.0.1:7440").replace(/\/$/, "");
    this.timeoutMs = opts.timeoutMs ?? 30_000;
    this.token =
      opts.token ??
      (typeof process !== "undefined" ? process.env?.NEBULA_API_TOKEN : undefined);
    this.vessels = new Vessels(this);
  }

  /** Engine + guest agent status. */
  status(): Promise<EngineStatus> {
    return this.request("GET", "/v1alpha1/status");
  }

  /** Live memory/balloon/footprint stats. */
  stats(): Promise<EngineStats> {
    return this.request("GET", "/v1alpha1/stats");
  }

  /** Run a command inside the engine vessel. */
  exec(cmd: string, args: string[] = [], timeoutMs = 30_000): Promise<ExecResult> {
    return this.request(
      "POST",
      "/v1alpha1/exec",
      { cmd, args, timeout_ms: timeoutMs },
      timeoutMs + 5_000,
    );
  }

  /** Set the memory balloon target. */
  balloon(targetMib: number): Promise<{ ok: boolean }> {
    return this.request("POST", "/v1alpha1/balloon", { target_mib: targetMib });
  }

  /** List containers (Docker Engine API shape). */
  containers(): Promise<Container[]> {
    return this.request("GET", "/v1alpha1/containers");
  }

  /**
   * The standalone kubeconfig YAML (k3s on full Nebula; slim's TLS
   * apiserver on slim). Feed it to any Kubernetes client:
   * `new KubeConfig().loadFromString(await nebula.kubeconfig())`.
   */
  async kubeconfig(): Promise<string> {
    return this.requestText("GET", "/v1alpha1/kubeconfig");
  }

  /**
   * Raw call against the engine's Docker API (`/docker` plane) — paths and
   * payloads are the Docker Engine API verbatim.
   * `nebula.docker("GET", "/v1.43/containers/json?all=true")`
   */
  docker<T = unknown>(method: string, path: string, body?: unknown): Promise<T> {
    return this.request(method, `/docker${path}`, body);
  }

  /**
   * Raw call against the kubernetes apiserver (`/k8s` plane — slim only;
   * k3s answers 501: use kubeconfig() with a real client instead).
   * `nebula.k8s("GET", "/api/v1/namespaces/default/pods")`
   */
  k8s<T = unknown>(method: string, path: string, body?: unknown): Promise<T> {
    return this.request(method, `/k8s${path}`, body);
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

  /** @internal */
  async request<T>(
    method: string,
    path: string,
    body?: unknown,
    timeoutMs?: number,
  ): Promise<T> {
    const text = await this.requestText(method, path, body, timeoutMs);
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      throw new NebulaError(`non-JSON response: ${text.slice(0, 200)}`);
    }
    return parsed as T;
  }

  /** @internal */
  async requestText(
    method: string,
    path: string,
    body?: unknown,
    timeoutMs?: number,
  ): Promise<string> {
    const headers: Record<string, string> = {};
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (this.token) headers["Authorization"] = `Bearer ${this.token}`;
    const res = await fetch(`${this.baseUrl}${path}`, {
      method,
      headers,
      body: body !== undefined ? JSON.stringify(body) : undefined,
      signal: AbortSignal.timeout(Math.max(timeoutMs ?? 0, this.timeoutMs)),
    });
    const text = await res.text();
    if (!res.ok) {
      let message = `HTTP ${res.status}`;
      try {
        const parsed = JSON.parse(text);
        if (parsed && typeof parsed === "object" && "error" in parsed) {
          message = String((parsed as { error: unknown }).error);
        }
      } catch {
        /* keep the status message */
      }
      throw new NebulaError(message, res.status);
    }
    return text;
  }
}

/** `nebula.vessels.*` — named microVMs with snapshots and live branching. */
export class Vessels {
  constructor(private client: NebulaClient) {}

  list(): Promise<VesselSummary[]> {
    return this.client.request("GET", "/v1alpha1/vessels");
  }

  get(name: string): Promise<VesselSummary> {
    return this.client.request("GET", `/v1alpha1/vessels/${enc(name)}`);
  }

  /** Create (and boot, unless no_start). from_image builds can take minutes. */
  create(opts: CreateVesselOptions): Promise<{ created: string; start?: StartOutcome }> {
    const slow = opts.from_image ? 600_000 : 120_000;
    return this.client.request("POST", "/v1alpha1/vessels", opts, slow);
  }

  start(name: string): Promise<StartOutcome> {
    return this.client.request("POST", `/v1alpha1/vessels/${enc(name)}/start`, undefined, 60_000);
  }

  stop(name: string): Promise<StopOutcome> {
    return this.client.request("POST", `/v1alpha1/vessels/${enc(name)}/stop`, undefined, 60_000);
  }

  /** Remove a vessel; force stops it first when running. */
  rm(name: string, force = false): Promise<{ removed: string }> {
    const q = force ? "?force=true" : "";
    return this.client.request("DELETE", `/v1alpha1/vessels/${enc(name)}${q}`, undefined, 60_000);
  }

  exec(name: string, cmd: string, args: string[] = [], timeoutMs = 30_000): Promise<ExecResult> {
    return this.client.request(
      "POST",
      `/v1alpha1/vessels/${enc(name)}/exec`,
      { cmd, args, timeout_ms: timeoutMs },
      timeoutMs + 5_000,
    );
  }

  /** Last `tail` bytes of the vessel's boot/console log. */
  async console(name: string, tail = 16_384): Promise<string> {
    const r = await this.client.request<{ console: string }>(
      "GET",
      `/v1alpha1/vessels/${enc(name)}/console?tail=${tail}`,
    );
    return r.console;
  }

  /** Take a snapshot. Default mode "auto": memory+disks when the vessel is
   * running on a capable backend, else disks only. */
  snapshot(
    name: string,
    label: string,
    mode: "auto" | "memory" | "disk" = "auto",
  ): Promise<SnapshotOutcome> {
    return this.client.request(
      "POST",
      `/v1alpha1/vessels/${enc(name)}/snapshots`,
      { label, mode },
      300_000,
    );
  }

  snapshots(name: string): Promise<SnapshotInfo[]> {
    return this.client.request("GET", `/v1alpha1/vessels/${enc(name)}/snapshots`);
  }

  snapshotRm(name: string, label: string): Promise<{ removed: string }> {
    return this.client.request(
      "DELETE",
      `/v1alpha1/vessels/${enc(name)}/snapshots/${enc(label)}`,
    );
  }

  /** Roll back to a snapshot — memory snapshots live-resume mid-execution. */
  restore(name: string, label: string): Promise<RestoreOutcome> {
    return this.client.request(
      "POST",
      `/v1alpha1/vessels/${enc(name)}/restore`,
      { label },
      300_000,
    );
  }

  /**
   * Fan out clones (the tree-search primitive). With a memory-snapshot
   * label every branch wakes MID-EXECUTION: RAM, processes and open
   * sockets intact, copy-on-write shared with the source on macOS/Linux.
   */
  branch(
    name: string,
    opts: { newName: string; label?: string; count?: number },
  ): Promise<BranchOutcome> {
    const count = opts.count ?? 1;
    return this.client.request(
      "POST",
      `/v1alpha1/vessels/${enc(name)}/branch`,
      { new_name: opts.newName, label: opts.label, count },
      120_000 + count * 30_000,
    );
  }
}

function enc(s: string): string {
  return encodeURIComponent(s);
}
