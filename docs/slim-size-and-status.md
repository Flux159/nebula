# nebula-slim — measured size & status

Everything below is **measured on macOS (Apple Silicon)** on 2026-06-10, not
projected. The slim engine was booted as a real VZ microVM (isolated
`NEBULA_HOME`) and driven by the **unmodified Docker CLI 27.5** through
nebula's normal socket-proxy → vsock path.

## The headline: Nebula-Slim.app

| | Full (`Nebula.app`) | **Slim (`Nebula-Slim.app`)** |
|---|---|---|
| On disk | 311 MB | **54 MB** |
| Compressed download (.zip/.dmg) | 207 MB (dmg) | **34 MB** |

5.7× smaller on disk, 6× smaller to download. The slim app is the full Tauri
UI + nebula/nebulad sidecars + kernel + **slim rootfs** + **slim CLIs**.

## Embeddable payload ("libnebula" — no UI app)

What an app actually bundles to embed the engine:

| component | size |
|---|---|
| kernel (gz) | 15.5 MB |
| rootfs-slim (gz) | 9.0 MB |
| nebula + nebulad (host sidecars) | 4.9 MB |
| docker-slim + kubectl-slim + helm-slim | 2.5 MB |
| **embed core (engine + CLIs)** | **31.9 MB** |
| + libkrun (only for sandbox/GPU sidecars) | 45.7 MB |

**~32 MB to embed a working container + Kubernetes(-facade) + Helm engine** —
versus ~140 MB+ for the Go stack the full flavor ships. Comfortably under the
50 MB goal; the original "≈35 MB reachable" pitch is beaten.

Biggest remaining chunks if we ever want smaller: kernel (15.5 MB — a
slim-specific kernel-config trim is the S11 stretch), libkrun (14 MB, droppable
if you don't need ephemeral sandbox/GPU sidecars), and the UI binary (8 MB, irrelevant to non-UI embedders).

## What works (validated end-to-end via the real Docker CLI)

On the booted slim microVM, `DOCKER_HOST=unix://…/run/docker.sock docker …`:

- `pull` (Docker Hub), `images`, `run -d`, `run` foreground (stdout/stderr
  streamed via attach), exit-code propagation, `logs`, `exec`, `ps`,
  `create`/`start`/`stop`/`rm`, `inspect -f`, volumes.
- `docker build` (multi-step, layer commit, run-the-built-image).
- The slim CLIs (`docker-slim`/`kubectl-slim`/`helm-slim`) — and the
  k8s-facade / helm flows (apply/get/scale/delete, helm install/template/
  list/uninstall) — validated 32/32 in the engine microVM.

The same engine API is also cross-checked against **real dockerd** (docker-slim
as client) as the compatibility oracle.

## Compatibility notes (real Docker CLI ↔ slimd)

- **`docker build` needs `DOCKER_BUILDKIT=0`.** slim implements the *classic*
  builder (`/build`); it does not speak the BuildKit gRPC session protocol the
  modern CLI defaults to. `docker-slim build` always uses the classic path, so
  it needs no flag — only the real `docker` CLI does.
- **k8s/helm**: facade only — CRDs/operators/RBAC are skipped (use `--strict`
  to fail loudly). See `slim-k8s-shim.md` / `slim-k8s-roadmap.md`.
- Engine is one-request-per-connection (`Connection: close`) — fine for a
  single local client; not tuned for thousands of concurrent API callers.

## Going forward — cross-platform

slimd is Linux-native (it *is* the guest). The portability surface is the
**clients** and the **VMM host integration**:

- **macOS (arm64): done** — VZ backend, validated above. (x86_64 macOS should
  be a rebuild; untested.)
- **Linux**: slimd already cross-builds to `x86_64`/`aarch64-musl`. Two modes:
  (a) inside a KVM microVM (nebula's krun backend — the recent Linux/KVM boot
  spike), or (b) potentially slimd directly on the host for the embedding case.
  Client `docker-slim` builds for Linux host triples already. Next step: run
  `scripts/test-slim.sh` on a Linux box.
- **Windows**: the real target is **Hyper-V** (not WSL2 nested virt). Work
  needed: a Hyper-V backend in nebula-core, `docker-slim`/`kubectl-slim`/
  `helm-slim` built for `x86_64-pc-windows-msvc` (the clients are pure Rust +
  a unix-socket transport that must become a **named pipe** on Windows — the
  one place the client code needs a platform branch), and named-pipe ↔ vsock
  proxying. slimd itself is unchanged (Linux guest). Deferred, but no
  architectural blocker.
