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

Minimum viable embed for an orchestrator that runs agents in docker
containers: **`nebula` + `nebulad` + kernel + `docker`-flavor rootfs ≈ 78 MB
compressed.** Your app's UI talks to the REST API; no host CLIs needed
(your code uses a docker client library against the socket).

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
api_port = 7461          # your private port (0 disables the REST API)
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
`nebula down`. Do NOT use `nebula autostart` from an embedded instance — the
launchd label is global (see seams below).

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

Known seams when N > 1 (single-instance users unaffected):

- **Guest DNS resolver (UDP 42053):** first daemon binds it; later daemons
  log a bind failure. Harmless for public names (any guest's queries reach
  the bound daemon, which resolves via the host), but `*.nebula.local`
  answers reflect the first engine's containers only.
- **k3s API forward (127.0.0.1:6443):** fixed port — only one engine should
  enable Kubernetes today. Embedders on the docker flavor are unaffected.
- **`nebula autostart` / launchd:** one global label (`dev.nebula.nebulad`),
  always the standalone instance. Embedded instances manage their own
  lifecycle.

Making those per-instance (ports in config, label derived from NEBULA_HOME)
is tracked in tasks/issues.md.

## 4. Worked example: the agent-orchestrator shape

A thin local webapp (its own UI, SQLite state) running agents in containers:

1. Bundle: your webapp binary, `nebula` + `nebulad` sidecars, kernel +
   `docker`-flavor rootfs (≈ 78 MB of Nebula payload), optionally a slim
   devcontainer tarball.
2. First launch: write `config.toml` (private `NEBULA_HOME` + `api_port`),
   `install-image`, `up`, `docker load` or pull your agent image.
3. Each agent run: create a container from the devcontainer image with
   `luminal` bind-mounted, workspace dir bind-mounted (virtiofs, host-path
   identical), ports published as needed (they appear on localhost).
4. Health panel: proxy `/v1alpha1/status` + `/stats`; "Repair" button =
   `vessels reset vessel`; "Restart engine" = `down` + `up`.
5. Later, k8s-scheduled agents: same engine, `nebula setup kubectl`
   equivalent via the sidecar, helm charts against the local k3s.
