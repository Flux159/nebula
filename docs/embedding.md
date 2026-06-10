# Embedding Nebula in your own app

Nebula is designed to disappear behind another product — an agent
orchestrator, an IDE, a homelab manager — that wants containers/Kubernetes/
microVMs on macOS without shipping Docker Desktop as a dependency. This guide
covers the artifact inventory, the consuming app's responsibilities, and the
multi-instance story.

## 1. The artifact inventory (what you ship)

| Artifact | Size | Where it comes from | Required? |
|---|---|---|---|
| `nebula` (CLI) | 2.5 MB | `cargo build --release -p nebula-cli` / our releases | yes |
| `nebulad` (daemon) | 2.2 MB | `cargo build --release -p nebulad` / our releases | yes |
| `kernel-Image.gz` | 16 MB | CI (`guest-images.yml`) / `vessel/build-kernel.sh` | yes |
| `rootfs.img.gz` | 117 MB (`full`) / 57 MB (`docker`) / 6 MB (`minimal`) | CI / `vessel/build-rootfs.sh FLAVOR=…` | yes (pick a flavor) |
| `libkrun.dylib` (fork) | 5 MB | `scripts/build-libkrun.sh` | only for sandboxes/GPU/named vessels |
| docker / kubectl / helm CLIs | 39/55/59 MB | `scripts/fetch-host-clis.sh` | only if your users need raw CLIs |

Pick the flavor by how you schedule agents. An orchestrator that treats
agents as a mix of workflows and services and wants restarts/health/Jobs
semantics runs them on **Kubernetes**: ship the `full` flavor
(**sidecars + kernel + full rootfs ≈ 138 MB compressed**) and drive k3s
through the standalone kubeconfig — kube-rs in a Rust app, or the bundled
kubectl. Plain-docker embedders use the `docker` flavor (≈ 78 MB total).
No host CLIs needed either way when your code speaks the APIs directly.

Layout inside your `.app` (Tauri example — see our own `tauri.conf.json` +
`ui/src-tauri/src/lib.rs` for working code):

```
YourApp.app/Contents/MacOS/        your-app + nebula + nebulad   (sidecars)
YourApp.app/Contents/Resources/    kernel-Image.gz + rootfs.img.gz
```

Both `nebula` and `nebulad` need the virtualization entitlements when signed
(`scripts/entitlements/dev.entitlements`); your app inherits nothing — the
sidecars are the processes that talk to Virtualization.framework.

## 2. The consuming app's contract

Everything below is one sidecar invocation each; your app never re-implements
engine logic.

**First run (no Docker, no downloads, offline):**

```bash
export NEBULA_HOME="$HOME/Library/Application Support/YourApp/nebula"  # isolation — see §3
mkdir -p "$NEBULA_HOME"
cat > "$NEBULA_HOME/config.toml" <<EOF
api_port = 7461          # your private REST port (0 disables)
dns_port = 42061         # private guest-DNS relay port
k8s_port = 6461          # private k3s API forward
dns_zone = "galaxy.local" # brand the container DNS zone
max_ram_mib = 8192       # ceiling only; ballooning returns idle RAM
cpus = 4
data_disk_gib = 32
EOF
nebula install-image --kernel <Resources>/kernel-Image.gz --rootfs <Resources>/rootfs.img.gz
nebula up                # ~0.5-0.6s to a healthy engine
```

**Steady state:** poll `GET http://127.0.0.1:<api_port>/v1alpha1/status` (or
the TS/Python SDK with `baseUrl`), run containers against
`unix://$NEBULA_HOME/run/docker.sock` with any Docker client library
(bollard for Rust, dockerode for TS, docker-py). Published ports appear on
`localhost` automatically.

**Seeding your agent image (e.g. a Debian devcontainer):** either pull from
Docker Hub on first run, or bundle `docker save`d tarballs and load them
offline:

```bash
DOCKER_HOST=unix://$NEBULA_HOME/run/docker.sock docker load -i <Resources>/devcontainer.tar.gz
```

Note: image tarballs are full-weight (a Debian devcontainer is often 1-3 GB),
so most embedders bundle only a slim base and pull/refresh the heavy image in
the background. Both paths hit the same containerd store; user-configurable
images are just "any ref the engine can pull or load".

**Injecting a host binary (your 3.5 MB `luminal` agent):** `$HOME` is mounted
into the engine vessel at the identical path, so the simplest pattern is a
bind mount: `-v ~/Library/Application Support/YourApp/bin:/opt/yourapp:ro`.
No image rebuild when the agent updates.

**The "small insight" surface for end users:** `status` (API), and two
sidecar commands worth exposing as buttons — `nebula down && nebula up`
(restart) and `nebula vessels reset vessel` (restore the engine OS to
pristine while keeping container data; 0.9s). Your app stays the only UI.

**Lifecycle:** your app owns it. Spawn `nebula up` on launch (or lazily);
either leave the engine running on quit (containers keep working) or
`nebula down`. For start-at-login, `nebula autostart enable` works per
instance (the launchd label derives from `NEBULA_HOME`).

**Rust in-process option:** `nebula-core` is a library crate (VMM backends,
specs, vsock). Embedding it directly instead of sidecars is possible but you
take on the daemon's job (balloon loop, proxies, DNS). The sidecar pattern is
the supported path; in-process is for when you outgrow it.

## 3. Multiple Nebulas — the isolation story

`NEBULA_HOME` isolates an instance completely: its own engine VM, disks,
images, unix sockets, control socket, and (via `api_port`) its own REST port.
Verified: an embedded instance boots in ~0.5s **next to** a user's standalone
Nebula; each runs its own docker daemon; neither sees the other.

```
standalone user install:   ~/.nebula            api 7440
your embedded instance:    $NEBULA_HOME         api 7461 (config.toml)
```

Resource math still works in your favor: each engine has its own balloon, so
an idle embedded engine costs ~1-2 GB host-visible regardless of its
configured ceiling.

Everything that was port- or name-shaped is per-instance config:

- **DNS:** `dns_zone` brands the container zone (`api.galaxy.local`), and
  `dns_port` gives each instance its own resolver — the engine passes the
  port to the guest via kernel cmdline, so the whole chain follows config.
- **Kubernetes:** `k8s_port` moves the k3s API forward; kubeconfigs (both the
  merged context and `$NEBULA_HOME/kubeconfig`) are written against it
  automatically — clients read the effective value from
  `/v1alpha1/status`/`nebula status` rather than assuming 6443.
- **Autostart:** the launchd label derives from `NEBULA_HOME`
  (`dev.nebula.nebulad.<hash>`) and the agent carries `NEBULA_HOME` in its
  environment — each embedded product can independently start at login.

## 4. Worked example: the agent-orchestrator shape

A thin local webapp (its own UI, SQLite state) scheduling agents on the
embedded Kubernetes — agents as Jobs when they look like workflows,
Deployments/Services when they look like services:

1. Bundle: your webapp binary, `nebula` + `nebulad` sidecars, kernel +
   `full`-flavor rootfs (≈ 138 MB of Nebula payload), optionally a slim
   devcontainer tarball.
2. First launch: write `config.toml` (private `NEBULA_HOME`, ports,
   `dns_zone = "galaxy.local"`), `install-image`, `up`; first k8s call
   starts k3s (~20s once, persisted across boots).
3. Seed the agent image: `docker load` a bundled tarball or background-pull
   from Docker Hub — k3s shares the engine's containerd store, so images
   loaded via the docker socket are schedulable without a registry.
4. Each agent: a Job (workflow-shaped) or Deployment+Service
   (service-shaped) from the devcontainer image, with `luminal`
   bind-mounted via hostPath (the user's `$HOME` is mounted at identical
   paths) and the workspace as a hostPath volume. Services resolve at
   `<name>.galaxy.local`; NodePorts appear on localhost.
5. Health panel: proxy `/v1alpha1/status` + `/stats`; "Repair" =
   `vessels reset vessel` (k8s state lives on the data disk and survives);
   "Restart engine" = `down` + `up`; k3s restarts automatically.
