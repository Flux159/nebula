# Nebula

Open source, simple, and performant container, Kubernetes & microVM manager for
macOS (Apple Silicon).

Nebula runs one elastically-sized Linux VM (the **Vessel**) on
Virtualization.framework for your everyday containers and Kubernetes, plus
millisecond-boot isolated microVMs on a vendored [libkrun](https://github.com/containers/libkrun)
fork for sandboxes and GPU workloads — with memory ballooning so the whole
stack only holds the RAM your workloads actually use.

```
brew install … (packaging WIP — build from source below)

nebula up                 # boots the Vessel (~0.6s to a healthy engine)
nebula use docker         # point docker at Nebula (revert anytime)
docker run -d -p 8080:80 nginx     # localhost:8080 just works
docker run --platform linux/amd64 alpine uname -m   # x86_64 via Rosetta

nebula use kubectl        # local k3s, prod-safe context switching
kubectl get nodes

nebula sandbox run -- uname -a     # isolated microVM, ~250ms total
nebula sandbox run --gpu -- ls /dev/dri    # virtio-gpu (Venus)

nebula stats              # guest use, balloon, honest host footprint
nebula revert --all       # put docker/nerdctl/kubectl back exactly
```

## Highlights

- **Elastic memory.** Set a max; a balloon controller (deflate-fast,
  inflate-slow) returns idle RAM. A 32 GiB Vessel idles at ~1.1 GiB
  host-visible footprint.
- **Out-of-the-box tooling.** `nebula use docker|nerdctl|kubectl` configures
  the standard CLIs; `nebula revert` restores your previous contexts exactly
  (revert stack, loud warnings when switching away from anything that looks
  like production).
- **amd64 via Rosetta.** The Vessel mounts Apple's Rosetta share — mixed
  arm64/amd64 compose stacks in one VM at near-native speed.
- **Host-faithful DNS.** Guest and container DNS resolve through the Mac's own
  resolver (VPN/split-horizon included) plus a `*.nebula.local` zone; published
  ports appear on `localhost` automatically.
- **Sandbox microVMs.** `nebula sandbox run` boots, runs, and tears down an
  isolated VM in ~250ms; `--gpu` attaches virtio-gpu (Vulkan→Metal via Venus).
- **Embeddable.** REST API (`127.0.0.1:7440`, v1alpha1) with TypeScript
  (`sdk/typescript`) and Python (`sdk/python`) clients; Tauri UI in `ui/`.

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

## Documentation

- [`tasks/features.md`](tasks/features.md) — the full phased plan and architecture
- [`tasks/issues.md`](tasks/issues.md) — open questions, characterizations, incidents
- [`tasks/spike-notes.md`](tasks/spike-notes.md) — VMM backend findings and perf numbers
- [`CLAUDE.md`](CLAUDE.md) — contributor/agent working notes

## Status

Phases 0–10 of the plan are implemented and tested (VMM backends, Vessel,
docker/nerdctl, networking/virtiofs, elastic memory, k3s, reliability rig,
sandboxes, GPU device support, REST API + SDKs, UI). Stretch tracks — Linux
hosts, games (sommelier), hosted Nebula, Windows (WHP) — are tracked in the
plan as Phase 11.

License: MIT
