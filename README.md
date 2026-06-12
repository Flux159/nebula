# Nebula

Open source, simple, and performant container, Kubernetes & microVM manager for
macOS, Linux, and Windows.

Nebula runs one elastically-sized Linux VM (the **Vessel**) on the platform's
native hypervisor for your everyday containers and Kubernetes, plus
millisecond-boot isolated microVMs on a vendored [libkrun](https://github.com/containers/libkrun)
fork for sandboxes and GPU workloads — with memory ballooning so the whole
stack only holds the RAM your workloads actually use.

Runs on **macOS** (Virtualization.framework), **Linux** (KVM), and **Windows**
(Hyper-V/WHP) — no WSL2 — with CI/CD release builds for all three.

**Two flavors share one host.** *Full* Nebula ships the real Go stack
(dockerd/containerd, k3s, kubectl, helm) — the genuine article.
**[Nebula-slim](slim/README.md)** swaps the guest for `slimd`, a from-scratch
Rust reimplementation of a useful container + Kubernetes + Helm subset that's
small enough to **embed** (~32 MB, no Go runtime). Pick full when you need real
k8s; pick slim to embed an engine or when size/RAM is the budget. See
**[slim/README.md](slim/README.md)**.

```
brew install … (packaging WIP — build from source below)

nebula up                 # boots the Vessel (~0.6s to a healthy engine)
nebula setup docker       # point docker at Nebula (revert anytime)
docker run -d -p 8080:80 nginx     # localhost:8080 just works
docker run --platform linux/amd64 alpine uname -m   # x86_64 via Rosetta

nebula setup kubectl      # local k3s, prod-safe context switching
kubectl get nodes

# or one-off, without touching your contexts at all:
nebula docker ps
nebula kubectl get pods -A
nebula helm install my-redis oci://registry-1.docker.io/bitnamicharts/redis

nebula sandbox run -- uname -a     # isolated microVM, ~250ms total
nebula sandbox run --gpu -- ls /dev/dri    # virtio-gpu (Venus)

nebula stats              # guest use, balloon, honest host footprint
nebula revert --all       # put docker/nerdctl/kubectl back exactly

nebula autostart enable   # start the engine at login, restart on failure
nebula ui                 # open the desktop app (a client of the engine)
```

## Highlights

- **Elastic memory.** Set a max; a balloon controller (deflate-fast,
  inflate-slow) returns idle RAM. A 32 GiB Vessel idles at ~1.1 GiB
  host-visible footprint.
- **Out-of-the-box tooling.** `nebula setup docker|nerdctl|kubectl` configures
  the standard CLIs; `nebula revert` restores your previous contexts exactly
  (revert stack, loud warnings when switching away from anything that looks
  like production). `nebula docker|kubectl|helm <cmd>` runs a single command
  against Nebula via environment overrides — your contexts never change.
- **amd64 via Rosetta.** The Vessel mounts Apple's Rosetta share — mixed
  arm64/amd64 compose stacks in one VM at near-native speed.
- **Host-faithful DNS.** Guest and container DNS resolve through the Mac's own
  resolver (VPN/split-horizon included) plus a `*.nebula.local` zone; published
  ports appear on `localhost` automatically.
- **Sandbox microVMs.** `nebula sandbox run` boots, runs, and tears down an
  isolated VM in ~250ms; `--gpu` attaches virtio-gpu (Vulkan→Metal via Venus).
- **Snapshots & live branching.** On `--backend vz` vessels,
  `nebula vessels snapshot` captures disks **and** the live machine state
  (RAM, running processes, open sockets) by default — ~360ms, without
  stopping the vessel (`--no-memory` for a ~10ms disk-only APFS clone).
  `vessels branch --snapshot x --count N` fans out N independent clones —
  from a memory snapshot each wakes mid-execution (~600ms per branch), the
  primitive for tree-search over agent runs. `vessels new --from-image
  debian:bookworm-slim` boots any arm64 docker image as a snapshot-capable
  microVM.
- **Apps platform.** A catalog of one-click installs in the UI — pick an app and
  it runs, no compose-file wrangling. Docker images and raw YAML/compose "boxes"
  install the same way.
- **Embeddable.** REST API (`127.0.0.1:7440`, v1alpha1) with TypeScript
  (`sdk/typescript`) and Python (`sdk/python`) clients; Tauri UI in `ui/`. For
  embedding into your *own* app, **[Nebula-slim](slim/README.md)** is the
  purpose-built path: ~32 MB, no Go runtime, CLIs on macOS/Linux/Windows.
- **Signed & notarized.** Releases are Developer ID–signed, notarized, and
  stapled (local + CI), so the `.app` and CLIs run without Gatekeeper prompts.
- **Daemon-first.** The engine (`nebulad`) runs independently of the app and
  CLI — close either and your containers keep running. `nebula autostart
  enable` installs a launchd agent (start at login + crash restart); the app
  offers a one-click "Start engine" when the daemon is down.

## Benchmarks

Measured on an M-series MacBook Pro (16 cores), release build, 2026-06:

| What | Time / number |
|---|---|
| `nebula up` → healthy engine (wall clock) | **0.62 s** |
| └ VZ virtual machine create→running | 80–96 ms |
| └ kernel boot + init + agent ready (vsock) | 580–595 ms total |
| `nebula sandbox run` boot→run→teardown (libkrun) | **~250 ms** |
| Vessel disk snapshot (APFS clone) | 5–12 ms |
| Live memory snapshot (vz, vessel never stops) | **~360 ms** |
| Restore to a live memory snapshot (resume mid-execution) | ~850 ms |
| 3-way live branch fan-out from a memory snapshot | 1.8 s |
| Idle host footprint, 32 GiB max engine | **~1–2 GiB** (balloon holds ~30 GiB) |
| Balloon resizes at steady state | 0/hour (one jump per workload change) |
| virtiofs (`$HOME` share) sequential write | ~1.3 GB/s |
| virtiofs small files (1000 creates) | 0.30 s |
| virtio-blk (data disk) direct write | ~276 MB/s |
| 50 concurrent containers started | 14–20 s |
| Max containers in one vessel (kernel 1024-ports-per-bridge bound) | **1,022** |
| Container density, 256 MiB workloads | memory-linear: 10 / 20 / 50 / 119 / 230 @ 4–64 GiB max |
| Concurrent vessels (macOS hypervisor cap: 128 VMs system-wide) | **124** |
| Idle host cost per extra vessel (any `--mem`, ballooned) | ~50–90 MiB |
| Balloon contract suite (idle reclaim, hogs, drift, sawtooth) | 19/19 checks pass |

Reproduce with `scripts/test-phase*.sh` and `scripts/battletest.sh` (raw data
+ charts in [`bench/report/`](bench/report/report.md)); details in
`tasks/spike-notes.md` and `tasks/nebulabattletest.md`.

## How installs bootstrap (no Docker required)

The guest kernel + rootfs are built by CI on arm64 Linux runners
(`.github/workflows/guest-images.yml`) and attached to GitHub Releases as
gzip artifacts (~16 MB kernel + ~160 MB rootfs — the 2 GB ext4 image is
mostly sparse zeros). On first `nebula up`, the CLI downloads them, verifies
SHA-256 checksums, and installs to `~/.nebula` (a pristine copy is kept for
`nebula vessels reset`). Developers working from a checkout build the same
images locally with Docker via `vessel/build-*.sh`.

## Building from source

Requirements: Apple Silicon Mac, Rust stable + `aarch64-unknown-linux-musl`
target, Docker (any engine) for guest image builds, `zig` + `llvm` (brew) for
the libkrun fork.

```bash
vessel/build-kernel.sh          # guest kernel (container build, ~10 min)
vessel/build-rootfs.sh          # guest rootfs (Alpine + containerd/dockerd/k3s)
scripts/build-libkrun.sh GPU=1  # sidecar engine (vendored fork)
cargo build
scripts/sign-dev.sh target/debug/nebula target/debug/nebulad
target/debug/nebula up
```

Acceptance suites: `scripts/test-phase{1..10}.sh`.

For **Linux** (KVM) and **Windows** (Hyper-V/WHP) the build recipes — toolchain,
libkrun `.so`/`krun.dll`, and packaging — are the source of truth in
[`.github/workflows/release.yml`](.github/workflows/release.yml) (the
`linux-release` and `windows-release` jobs).

## Documentation

- [`slim/README.md`](slim/README.md) — **Nebula-slim**: the embeddable Rust engine, what it supports, and why it's the path for embedding
- [`tasks/features.md`](tasks/features.md) — the full phased plan and architecture
- [`tasks/issues.md`](tasks/issues.md) — open questions, characterizations, incidents
- [`tasks/spike-notes.md`](tasks/spike-notes.md) — VMM backend findings and perf numbers
- [`CLAUDE.md`](CLAUDE.md) — contributor/agent working notes

## Status

Phases 0–10 of the plan are implemented and tested (VMM backends, Vessel,
docker/nerdctl, networking/virtiofs, elastic memory, k3s, reliability rig,
sandboxes, GPU device support, REST API + SDKs, UI), plus the Apps platform and
a signed/notarized release pipeline.

**Cross-platform.** Tested on macOS (Apple Silicon, Virtualization.framework),
Linux (x86_64, KVM), and Windows (x86_64, Hyper-V/WHP — no WSL2). CI builds and
tests all three (`.github/workflows/ci.yml`); `release.yml` ships artifacts for
each: a signed/notarized `Nebula.app` + DMG on macOS, and `nebula`/`nebulad`
packages (with the libkrun fork) on Linux and Windows.
**[Nebula-slim](slim/README.md)**, the embeddable Rust engine, is validated on
the same three.

Remaining stretch tracks — games (sommelier), hosted Nebula — are tracked in the
plan as Phase 11.

License: MIT
