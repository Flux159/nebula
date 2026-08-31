# Nebula

**Nebula is an embeddable microVM and container runtime that lets you ship fast
Linux microVMs and containers with your applications.**

*What Electron did for web frontends, Nebula does for backends.* Electron let you
ship the web app you already wrote as a desktop app. Nebula lets you ship the
*server* you already wrote — the same containers, the same database, the same
compose file — inside an app that runs on a machine with no Docker installed and
no terminal open.

It boots a Linux virtual machine on your platform's native hypervisor
(Virtualization.framework, KVM, or Hyper-V — no WSL2) and makes what runs inside
look local: `docker` and `kubectl` work unchanged, published ports appear on
`localhost`, and DNS resolves through your own resolver. The VM is up in about
**0.6 seconds** and gives idle memory back to the host, so a 32 GiB engine sits
at roughly 1 GiB when nothing is running.

Embedding is what Nebula is built for. The same engine is also a good everyday
one, so it is two things:

## Which do you want?

| I want to… | Go to |
|---|---|
| Ship a container engine inside an app I distribute | [**Embed it**](#embed-it) ↓ |
| Run containers and Kubernetes on my own machine | [**Use it**](#use-it) ↓ |

These are genuinely different products sharing a host. Read one.

## How it compares

| | Embed in your app | Platforms | `docker` + `kubectl` API |
|---|---|---|---|
| **Nebula** | **yes** — ~32 MB kit, no Go runtime | macOS, Linux, Windows (no WSL2) | both, out of the box |
| smolvm | yes — libkrun microVMs | macOS, Linux, Windows | no — boots OCI images, but no docker/kubectl API |
| Docker Desktop / Rancher Desktop | no — an app your users install | macOS, Linux, Windows | both |
| OrbStack | no — proprietary, installed | macOS only | both |
| Colima / Lima | no — a developer's CLI | macOS, Linux | docker; k8s via extras |

Docker Desktop, OrbStack and Colima are good at what they are for: being
installed on a developer's machine. None of them is something you can put inside
your own product and hand to someone who has never heard of a container.

[smolvm](https://github.com/smol-machines/smolvm) is the closest in spirit —
libkrun microVMs on the same three hypervisors, booting OCI images, and meant to
be embedded. The difference is the API surface above the VM. It gives you
machines; Nebula gives you a docker socket and a Kubernetes apiserver, so the
compose file, the client library and the `kubectl` invocation you already have
keep working unchanged. That compatibility layer is most of the work, and it is
the reason your existing server code ports without being rewritten.

---

# Embed it

Shipping an app that needs to run containers on a machine you do not control —
where "install Docker Desktop first" is not an acceptable first-run experience.

For this, use **[Nebula-slim](slim/README.md)**: a clean-room Rust
reimplementation of a useful container + Kubernetes subset, built to be embedded
rather than installed.

| | Nebula-slim | Nebula (full) |
|---|---|---|
| Engine | `slimd` — Rust | real dockerd/containerd + k3s |
| Embed footprint | **~32 MB** | ~140 MB+ (the Go stack) |
| Kubernetes | apiserver-lite + controller bridge | genuine k3s, whole ecosystem |
| Best for | **embedding**, size and RAM budgets, CI | you need real k8s: operators, admission, RBAC |

Slim has no Go runtime, and its host CLIs are pure Rust that cross-compile to
Windows **without WSL2** — which is what makes one codebase cover three
platforms.

## Integrate

Each release publishes a ready-made kit per host triple —
`nebula-slim-embed-<triple>.tar.gz`, carrying the binaries and the guest
`images/`. Contents differ by platform — macOS includes the slim CLIs, Linux and
Windows include `lib/` with the libkrun build — so unpack the kit and take what
is in it rather than assuming a fixed layout. Ship those inside your app, then:

```bash
export NEBULA_HOME="$HOME/Library/Application Support/YourApp/nebula"
bin/nebula install-image --kernel images/kernel-Image.gz --rootfs images/rootfs.img.gz
bin/nebula up
```

and talk to it however you already talk to Docker:

```
docker   unix://$NEBULA_HOME/run/docker.sock     (any docker client library)
REST     http://127.0.0.1:<api_port>/v1alpha1/…  (SDKs in sdk/typescript, sdk/python)
k8s      KUBECONFIG=$NEBULA_HOME/kubeconfig
```

`NEBULA_HOME` is what keeps your embedded engine separate from a developer's own
Nebula install, so neither one's `down` stops the other.

Full guide: **[docs/embedding.md](docs/embedding.md)**. What slim does and does
not implement: **[slim/README.md](slim/README.md)**.

## Who ships on it

**[Ragnarok Offline](https://github.com/Flux159/ragnarokoffline.app)** — a game
server, database and client in one double-clickable app, on all three platforms,
with no Docker on the user's machine. rAthena and MariaDB run unmodified in
containers, exactly as they would on a Linux server.

---

# Use it

Containers and Kubernetes on your own development machine.

## Install

**macOS / Linux**

```bash
curl -fsSL https://flux159.github.io/nebula/install.sh | bash
```

**Windows (PowerShell)**

```powershell
irm https://flux159.github.io/nebula/install.ps1 | iex
```

It detects your platform, installs the latest release — `Nebula.app` on macOS,
the engine under `~/.nebula` on Linux and Windows — and puts `nebula` on your
PATH. Set `NEBULA_VERSION` to pin a release rather than take the newest.

Or download a build yourself from
[Releases](https://github.com/Flux159/nebula/releases): a `.dmg` for macOS
(Apple Silicon, signed and notarized), `nebula-<version>-linux-<arch>.tar.gz`,
or `nebula-<version>-windows-x86_64.zip`.

The first `nebula up` downloads a guest kernel and root filesystem (~16 MB and
~160 MB, checksum-verified) and installs them to `~/.nebula`. You do not need
Docker installed to get started — the images are built by CI, not on your
machine.

## Use

```
nebula up                          # boots the VM — ~0.6s to a healthy engine
nebula setup docker                # point your docker CLI at Nebula
docker run -d -p 8080:80 nginx     # localhost:8080 just works

nebula setup kubectl               # local k3s
kubectl get nodes

nebula stats                       # guest usage, balloon, real host footprint
nebula revert --all                # put docker/kubectl back exactly as they were
```

Prefer not to touch your existing contexts? Run one-offs instead — `nebula
docker ps`, `nebula kubectl get pods -A`, `nebula helm install …` — which use
environment overrides and change nothing.

## What you get that you may not have now

- **Idle memory comes back.** Set a ceiling; a balloon controller returns RAM
  the workloads are not using. A 32 GiB engine idles around 1–2 GiB.
- **amd64 on Apple Silicon.** The VM mounts Rosetta, so mixed arm64/amd64
  compose stacks run in one place at near-native speed.
- **Your DNS, not the VM's.** Containers resolve through the host resolver —
  VPN and split-horizon included — plus a `*.nebula.local` zone.
- **Reversible.** `nebula setup` records what your CLIs pointed at and `nebula
  revert` restores it, with loud warnings before it touches anything that looks
  like production.
- **The engine outlives the app.** `nebulad` is a daemon: close the UI or the
  terminal and your containers keep running. `nebula autostart enable` starts
  it at login and restarts it on failure.
- **Sandboxes.** `nebula sandbox run -- uname -a` boots an isolated microVM,
  runs, and tears it down in about 250 ms. `--gpu` attaches virtio-gpu.
- **Snapshots that include running memory.** `nebula vessels snapshot` captures
  disks *and* live machine state without stopping the VM (~360 ms), and
  `vessels branch` fans out clones that each resume mid-execution.

---

## Platforms

Tested and built for macOS (Apple Silicon, Virtualization.framework), Linux
(x86_64, KVM) and Windows (x86_64, Hyper-V/WHP — no WSL2). CI covers all three;
each release ships a signed, notarized `Nebula.app` and DMG on macOS, packages
for Linux and Windows, and the four embed kits.

## Benchmarks

M-series MacBook Pro, 16 cores, release build:

| What | Time |
|---|---|
| `nebula up` → healthy engine | **0.62 s** |
| `nebula sandbox run` boot → run → teardown | ~250 ms |
| Live memory snapshot (VM never stops) | ~360 ms |
| Restore to a live snapshot, resuming mid-execution | ~850 ms |
| Idle host footprint, 32 GiB ceiling | ~1–2 GiB |
| 50 concurrent containers started | 14–20 s |
| Max containers in one VM | 1,022 |
| Concurrent VMs (macOS caps the system at 128) | 124 |

Reproduce with `scripts/test-phase*.sh` and `scripts/battletest.sh`; raw data and
charts in [`bench/report/`](bench/report/report.md).

## Building from source

Requires Rust stable with the `aarch64-unknown-linux-musl` target, Docker for
guest image builds, and `zig` + `llvm` for the libkrun fork.

```bash
vessel/build-kernel.sh          # guest kernel (~10 min)
vessel/build-rootfs.sh          # guest rootfs
scripts/build-libkrun.sh GPU=1  # sidecar engine (vendored fork)
cargo build
scripts/sign-dev.sh target/debug/nebula target/debug/nebulad
target/debug/nebula up
```

That recipe is for macOS. The Linux and Windows toolchains, the libkrun
`.so`/`krun.dll` builds and the packaging steps are defined in
[`.github/workflows/release.yml`](.github/workflows/release.yml) — treat those
jobs as the source of truth rather than reconstructing them by hand.

To cut a release, set the version everywhere it is written down and tag:

```bash
scripts/set-version.sh 0.1.7     # root Cargo.toml, ui/src-tauri, Cargo.lock
git commit -am "release: 0.1.7" && git tag v0.1.7 && git push --follow-tags
```

The tag triggers `guest-images.yml` and the three kit builds, which wait for
those images rather than taking the newest run.

## Documentation

- [`slim/README.md`](slim/README.md) — Nebula-slim: what it supports, and why it is the path for embedding
- [`docs/embedding.md`](docs/embedding.md) — the full embedding guide
- [`docs/httpapi.md`](docs/httpapi.md) — REST API reference
- [`tasks/features.md`](tasks/features.md) — architecture and the phased plan
- [`CLAUDE.md`](CLAUDE.md) — contributor and agent working notes

## Status

Beta releases. Interfaces may still change between `0.x` releases; pin a version
if you are embedding.

License: MIT
