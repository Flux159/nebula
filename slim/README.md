# Nebula-slim

A tiny, from-scratch container + Kubernetes + Helm engine, built to be
**embedded**. Where full [Nebula](../README.md) ships the real Go stack
(dockerd/containerd, k3s, kubectl, helm) inside a Linux VM, slim is a clean-room
**Rust reimplementation of a useful subset** of those tools — small enough to
drop into an app: **~32 MB** for a working engine + CLIs.

It is not a repackaging of Docker/Kubernetes. It is its own engine (`slimd`) plus
three host CLIs (`docker-slim`, `kubectl-slim`, `helm-slim`) that speak the same
APIs your tools already know.

## Why slim exists — and when to use it

| | **Nebula-slim** | **Nebula (full)** |
|---|---|---|
| Engine | `slimd` — Rust, a subset reimplementation | real dockerd/containerd + k3s |
| Embed footprint | **~32 MB** (engine + 3 CLIs) | ~140 MB+ (the Go stack) |
| `Nebula-Slim.app` on disk / download | **54 MB / 34 MB** | 311 MB / 207 MB |
| Kubernetes | real apiserver-lite + controller bridge (Tiers A+B) | genuine k3s — the whole ecosystem |
| RBAC enforcement / admission & conversion webhooks / port-forward | no | yes |
| Best for | **embedding**, size/RAM budgets, CI, primitives | you need real k8s: CRD ecosystem operators, admission, RBAC enforcement |

**Use slim to embed a container/k8s engine in your own product** — it was built
for that: it's a fraction of the size, has no Go runtime, and the host CLIs are
pure Rust that cross-compile to macOS, Linux, and **Windows without WSL2**. Use
full Nebula when you want the genuine Kubernetes article and don't care about a
few hundred MB.

The two share the same Nebula host (VM lifecycle, networking, virtiofs, the
socket proxy); slim just swaps the guest engine.

## What works

**Containers (`docker-slim`, or the unmodified `docker` CLI via `DOCKER_HOST`)**
- `pull` (Docker Hub + any registry), `images`, content-addressed per-layer store
- `run`/`create`/`start`/`stop`/`rm`, foreground attach with exit-code
  propagation, `-d`, `-t`, `logs`, `exec`, `cp`, `ps`, `inspect -f`
- **host bind mounts** (`-v` and `--mount type=bind`), directories or files,
  read-only or read-write, from any path on the `$HOME` share — **including
  paths with spaces**, which is where a macOS app keeps its state
- **volumes**: named (they outlive the container), anonymous, and the image's
  own `VOLUME`s — a fresh volume is seeded from the image, like docker
- **published ports that honour the host address**: `-p 127.0.0.1:6900:6900`
  stays on loopback (and reports itself that way) instead of quietly binding
  every interface
- `docker load` — a docker-save or OCI-layout archive, plain or gzipped, so a
  packaged app can install its own images with no registry. (`save` is not
  implemented: slim stores layers unpacked, so the original layer tars are
  gone. Produce archives with the real `docker save`.)
- `docker build` — the **classic** builder (multi-step, layer commit). Needs
  `DOCKER_BUILDKIT=0` with the real CLI; `docker-slim build` always uses it
- Validated against **real dockerd** as a compatibility oracle

**Kubernetes (`kubectl-slim`, or stock `kubectl` against the TLS apiserver)**

slim runs a real **apiserver-lite** in `slimd` (on a unix socket beside
`docker.sock`, and TLS on `:6443`) plus a controller bridge — not a CLI-only
facade:
- Discovery, typeless CRUD, **watch** (resourceVersion + `410 Gone`), **dynamic
  CRDs**, merge-patch, status/scale subresources, **OpenAPI v2(proto) + v3** so
  **stock `kubectl apply` works flagless**
- Controller bridge reconciles **Deployments / ReplicaSets / StatefulSets / Jobs
  / Pods** into real engine containers and writes Pod status back
- **Pod fidelity:** per-pod **pause/sandbox** container (kubelet model) owning the
  netns/IP/DNS; `containerStatuses`, **readiness & liveness probes**
  (http/tcp/exec, with restart), **multi-container pods / sidecars** (shared
  netns + localhost), and **init containers** (ordered, sharing the pod sandbox)
- **Volumes:** `emptyDir`, `configMap`, `secret`, `hostPath` (with `subPath` /
  `readOnly`)
- **In-pod operators:** projected ServiceAccount (ca.crt/token/namespace),
  `KUBERNETES_SERVICE_*`, a `kubernetes` Service — in-cluster client-go connects
  over CA-verified TLS
- `pods/{}/log` and `pods/{}/exec` (real `v4.channel.k8s.io` WebSocket) served by
  the apiserver, so `kubectl logs`/`kubectl exec [-it]` work
- Built-in static `nebula/pause:slim` image baked into the rootfs (no app-image
  or external-pull dependency)

**Helm (`helm-slim`)**
- `install`, `upgrade`, `template`, `uninstall`, `list`; charts from a dir or
  `.tgz`; values merge (`chart defaults ← -f ← --set`); a Go-template + sprig
  subset (`default`, `quote`, `indent`/`nindent`, `toYaml`/`toJson`, `b64enc`,
  `required`, `include`/`tpl`, …). Renders to manifests, applies via the apiserver

### Honestly out of scope (use full Nebula)
Real **RBAC enforcement**, **admission / conversion webhooks**, multi-version CRD
conversion, **port-forward**, and the full ecosystem-operator tail (cert-manager,
prometheus-operator, …). The apiserver accepts these objects; it won't *enforce*
RBAC or run webhooks. The microVM is the security boundary, so accept-and-ignore
RBAC reaches the same workload end-state — but if you need the real contract, use
k3s in full Nebula. See [`docs/slim-k8s-roadmap.md`](docs/slim-k8s-roadmap.md).

## Size

Measured on macOS (Apple Silicon); the slim engine booted as a real VZ microVM
and driven by the unmodified Docker CLI 27.5.

| component | size |
|---|---|
| kernel (gz) | 15.5 MB |
| rootfs-slim (gz) | 9.0 MB |
| nebula + nebulad (host sidecars) | 4.9 MB |
| docker-slim + kubectl-slim + helm-slim | 2.5 MB |
| **embed core (engine + CLIs)** | **~32 MB** |
| + libkrun (only for ephemeral sandbox/GPU sidecars) | +14 MB |

~32 MB for a working container + Kubernetes + Helm engine, vs ~140 MB+ for the
Go stack — comfortably under the 50 MB goal. Full details:
[`docs/slim-size-and-status.md`](docs/slim-size-and-status.md).

## Cross-platform

`slimd` is Linux-native (it *is* the guest). The portable surface is the **host
CLIs** and **VMM integration** — all pure Rust over a small transport shim (unix
socket on macOS/Linux, loopback TCP / named pipe on Windows; no `cfg` branches
beyond the transport):

| Platform | Status |
|---|---|
| **macOS (arm64)** | ✅ VZ backend; booted as a real microVM, driven by the real Docker CLI |
| **Linux (x86_64)** | ✅ slimd + CLIs cross-built to `*-linux-musl`, validated on Ubuntu 24 / kernel 6.8 (`aarch64-musl` also builds) |
| **Windows (native)** | ✅ host CLIs run on native Windows + Hyper-V, **no WSL2** — validated end-to-end |

## Configuration

`slimd` and the CLIs are configured entirely by environment variables — no config
file. Full reference: [`docs/slim-config.md`](docs/slim-config.md). Highlights:

- `NEBULA_IMAGES_DIR` — relocate the image/layer store; point several slim
  engines at one dir for a **shared, pull-once cross-engine image cache**
- `SLIM_REGISTRY_MIRROR` — pull-through registry mirror (offline / corporate
  mirrors / avoid Docker Hub rate limits)
- `SLIM_SOCKET`, `SLIM_KUBE_SOCKET`, `SLIM_KUBE_API_ADDR`, `DOCKER_HOST`,
  `NEBULA_HOME`, …

## Build & test

```bash
# host CLIs + engine for the guest (musl)
scripts/build-musl.sh                  # builds slimd + pause for aarch64/x86_64-linux-musl
cargo build -p docker-slim -p kubectl-slim -p helm-slim   # host CLIs (native)

# acceptance: docker + kube + helm suites against a booted slim engine
test/run-all.sh                        # macOS; Linux/Windows variants in test/
```

The test harness uses `SLIM_REGISTRY_MIRROR` (default `mirror.gcr.io`) so
back-to-back fresh-engine runs don't hit Docker Hub's anonymous pull rate limit.

## Docs

- [`docs/slim-size-and-status.md`](docs/slim-size-and-status.md) — measured size + what's validated
- [`docs/slim-k8s-roadmap.md`](docs/slim-k8s-roadmap.md) — the apiserver-lite design + what's in/out of scope
- [`docs/slim-k8s-pod-fidelity.md`](docs/slim-k8s-pod-fidelity.md) — the pod-fidelity phases (status, probes, sidecars, init, pause sandbox)
- [`docs/slim-config.md`](docs/slim-config.md) — every environment variable

License: MIT
