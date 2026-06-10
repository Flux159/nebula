# Nebula — Feature & Implementation Plan

**Nebula** is an open source, simple, and performant container, Kubernetes, and microVM
manager for macOS (Linux and possibly Windows later — macOS first).

It combines the best ideas from three worlds:

- **OrbStack** — single shared Linux VM, elastic memory (ballooning), "just works"
  developer experience, near-zero idle footprint, native UI.
- **libkrun / krunkit** — millisecond-boot microVMs on Apple's Hypervisor.framework,
  GPU acceleration via virtio-gpu Venus (Vulkan → Metal), open source, rust-vmm based.
- **microVM ecosystem (Firecracker, muvm, microsandbox)** — strong isolation primitives,
  on-demand VM spawning, embeddability, client SDKs.

The manager is written in **Rust**, on a **dual-backend VMM architecture**: the
primary Vessel VM runs on **Virtualization.framework** (Rosetta amd64, first-party
balloon — the OrbStack-proven stack), and **libkrun** (our vendored fork) powers the
GPU/sandbox sidecar engine (Venus GPU, ms-boot isolated microVMs, games) plus the
KVM path on Linux hosts and the future WHP path on Windows. `nebula-core` abstracts
both behind a VMM-backend trait from day one.

---

## Vision

> Run tens to hundreds of services on a local Mac — Docker containers, Kubernetes
> workloads, GPU-accelerated AI models — inside one elastically-sized microVM stack,
> with zero configuration, out-of-the-box compatibility with `docker` / `nerdctl` /
> `kubectl`, and a memory footprint that tracks what's actually running rather than a
> fixed pre-allocation.

### Goals (in priority order)

1. **Reliability first.** A developer with a 128 GB Mac should be able to run hundreds
   of services locally and trust it like they trust `launchd`. "Just works" beats
   feature count.
2. **Elastic resources.** The user sets a *maximum* RAM/CPU budget; Nebula uses
   virtio-balloon + guest pressure monitoring to only hold what's actually needed and
   return the rest to macOS.
3. **Out-of-the-box tooling compatibility.** `docker`, `nerdctl`, and `kubectl` work
   immediately after `nebula use <tool>` — and `nebula revert <tool>` restores the
   user's previous configuration exactly (these tools can target other VMs, remote
   hosts, or **production clusters**, so reverts must be safe and lossless).
4. **GPU workloads.** Vulkan→Metal translation via libkrun's virtio-gpu Venus path, so
   local AI models (llama.cpp, etc.) run with near-native GPU performance — a leg up
   over OrbStack.
5. **Embeddable.** Nebula's core is a Rust library + local API so other apps (e.g. an
   agent orchestrator) can embed the container/VM orchestrator, customize the
   containers they run, and build their own UI on top.
6. **Native UI later.** CLI-first; SwiftUI (or Tauri) menu-bar app comes after the core
   is solid. Devs can live entirely in the CLI.

### Secondary / stretch goals

- Linux desktop apps and **games** (Steam) via sommelier/cross-domain Wayland
  passthrough — a natural consequence of the libkrun + Venus stack (see muvm).
- Linux host support (libkrun already supports KVM).
- Hosted/remote version of the same control plane ("run the hosted version for your
  company").
- Windows is explicitly out of scope for now. Upstream libkrun is Linux/macOS only,
  full Hyper-V is Pro/Server-only, and WSL2 nested virt is poor — but owning a libkrun
  fork makes a **WHP backend** the credible long-term path (see 11.5).

### x86/amd64 containers — decided approach (dual backend)

amd64 images are a hard requirement (corp registries are full of amd64-only images).
**Rosetta is impossible on the libkrun stack**: the Rosetta Linux binary verifies it
is running inside a Virtualization.framework VM (an ioctl routed through VZ's virtiofs
device to the host), and Apple does not expose the TSO CPU flags Rosetta depends on to
third-party VMMs via Hypervisor.framework — the OrbStack author hit this exact wall
and uses Virtualization.framework as a result. Forking libkrun doesn't help (the
blockers live in Rosetta and macOS), and hacking Rosetta is a DMCA/EULA minefield.

**Decision: run the primary Vessel on Virtualization.framework.** Rosetta runs amd64
*binaries* inside the arm64 Vessel via binfmt_misc (VZ's Rosetta directory share), so
amd64 and arm64 containers share one VM, one network, one image store — mixed-arch
compose stacks just work, at Rosetta-class speed with TSO. qemu-user binfmt stays as
the fallback for exotic arches and Rosetta edge cases. VZ also brings a first-party
virtio-balloon device — the same foundation OrbStack ships on.

**libkrun (our fork) remains essential** as the second engine: Venus GPU containers,
ms-boot isolated sandbox microVMs, games (with FEX for x86 Steam, muvm-style), the KVM
backend on Linux hosts, and the future WHP backend on Windows. Routing rule: amd64 is
common and *implicit* → lives in the primary Vessel; GPU/sandbox is rare and
*explicit* (`--gpu`, `nebula sandbox`) → routed to libkrun sidecar VMs.

### Non-goals

- Running Windows guests (libkrun has no UEFI/BIOS; Linux-only guest kernel via
  libkrunfw).
- Being a general-purpose hypervisor (no full hardware emulation à la QEMU).
- Per-container VMs as the default (microsandbox's model). Nebula's default is one
  shared "Vessel" VM; per-workload isolated microVMs are an opt-in for noisy/heavy
  workloads (GPU, games) later.

---

## Architecture

```
+---------------------------------------------------------------------------+
|                                macOS Host                                 |
|                                                                           |
|  [ nebula CLI ]   [ menu-bar UI (later) ]   [ embedding apps via SDK ]    |
|         \                |                        /                       |
|          +-------- nebulad (Rust daemon) --------+                        |
|          |  - VMM backend trait: VZ | libkrun (| KVM, WHP later)          |
|          |  - balloon controller (elastic memory)                         |
|          |  - port forwarding + DNS (*.nebula.local)                      |
|          |  - socket proxies (docker.sock, k8s API) over vsock            |
|          |  - context manager (docker/nerdctl/kubectl use + revert)       |
|          |  - local gRPC/HTTP API (embedding surface)                     |
|          +---------+---------------------------------+                    |
|     Virtualization.framework              Hypervisor.framework (libkrun)  |
|  +-----------------v--------------------+  +----------v-----------------+ |
|  |  The Vessel (primary VM, arm64, VZ)  |  |  GPU / sandbox microVMs    | |
|  |                                      |  |  (our libkrun fork)        | |
|  |  [ vessel-agent (Rust) ]             |  |                            | |
|  |  [ containerd + runc ] — all ctrs    |  |  [ virtio-gpu Venus ]      | |
|  |  [ dockerd ] — Docker API compat     |  |    Vulkan→Metal for AI     | |
|  |  [ k3s ] — k8s, on demand            |  |  [ ms-boot isolation ]     | |
|  |  [ binfmt: Rosetta (amd64),          |  |    `nebula sandbox run`    | |
|  |    qemu-user fallback ]              |  |  [ games: FEX + sommelier ]| |
|  |  [ VZ balloon ] [ virtiofs ] [vsock] |  |    (stretch)               | |
|  +--------------------------------------+  +----------------------------+ |
+---------------------------------------------------------------------------+
```

Key architectural decisions:

| Decision | Choice | Rationale |
|---|---|---|
| VMM | **Dual backend behind a trait**: VZ for the primary Vessel; libkrun fork for GPU/sandbox VMs | VZ unlocks Rosetta + first-party balloon (OrbStack-proven); libkrun unlocks Venus GPU + ms boots; trait keeps KVM (Linux) and WHP (Windows) addable |
| VM model | **Single shared Vessel** by default; explicit sidecar microVMs for GPU/sandbox | OrbStack/WSL2-style; avoids N kernels × fixed RAM; GPU is rare+explicit, amd64 is common+implicit |
| Manager language | Rust | matches libkrun/rust-vmm; safety for a long-running daemon |
| Container runtime | containerd (+ dockerd for Docker API compat) | industry standard; k3s and nerdctl both speak containerd |
| Kubernetes | k3s (in-Vessel, containerd-backed) | lighter than kind-in-docker; single binary; on-demand |
| Guest image | minimal custom rootfs, **Alpine-based** (evaluate Buildroot later), shared by both backends | sub-second boot, small attack surface, we control the kernel config (balloon, virtiofs, venus, 4K pages) |
| x86/amd64 containers | **Rosetta binfmt in the VZ Vessel**; qemu-user fallback | Rosetta-class speed with TSO; one VM/network/image-store for mixed-arch stacks; FEX reserved for games/Linux hosts where Rosetta is unavailable |
| Host↔guest control | virtio-vsock (gRPC) | no network dependency for the control plane |
| Files | virtiofs (host→guest), with perf benchmarking gate | fastest available shared FS on this stack |
| Memory | virtio-balloon + host/guest pressure daemons | the OrbStack "secret sauce", rebuilt in the open |

---

## Workstream overview

| Phase | Name | Outcome |
|---|---|---|
| 0 | Foundations & spike | Rust workspace; VMM backend trait; boot VMs on both VZ and libkrun from Rust |
| 1 | The Vessel | Managed VZ guest + vessel-agent + vsock control channel; `nebula up/down/status` |
| 2 | Containers | containerd/dockerd in Vessel; `docker`/`nerdctl` + Rosetta amd64; `nebula use/revert` |
| 3 | Networking & files | localhost port forwarding, `*.nebula.local` DNS, virtiofs home mounts |
| 4 | Elastic memory | balloon controller; idle Vessel shrinks to near-zero; max-RAM budgets |
| 5 | Kubernetes | k3s on demand; `kubectl` works via `nebula use kubectl` with safe revert |
| 6 | Reliability & scale | hundreds of services, crash recovery, upgrades, telemetry-free diagnostics |
| 7 | libkrun sidecar engine | second backend live: sandbox microVMs, engine routing, image-store sharing |
| 8 | GPU | `--gpu` containers via Venus in sidecar VMs; llama.cpp benchmark vs colima |
| 9 | UI | Tauri menu-bar app on the daemon API |
| 10 | Embedding SDK | Rust crate + gRPC/HTTP API + TS/Python clients; orchestrator-in-an-app use case |
| 11 | Stretch | games (sommelier), per-workload isolated microVMs, Linux host, hosted version |

Phases 2–4 overlap heavily; the ordering below is the order in which each becomes
*usable*, not strictly serial.

---

## Phase 0 — Foundations & VMM spike (both backends)

**Goal:** prove the core stack end-to-end before building anything on top: boot a Linux
VM from Rust on an Apple Silicon Mac via **both** Virtualization.framework and libkrun,
behind one backend trait.

- [ ] **0.1 Repo layout.** Cargo workspace:
  ```
  nebula/
  ├── crates/
  │   ├── nebula-cli/        # `nebula` binary (clap)
  │   ├── nebulad/           # host daemon
  │   ├── nebula-core/       # VM lifecycle, libkrun FFI, shared types (the embeddable crate)
  │   ├── nebula-balloon/    # memory controller
  │   └── vessel-agent/      # guest-side agent (cross-compiled to linux/aarch64)
  ├── vessel/                # guest image build (kernel config, rootfs, build scripts)
  ├── third_party/
  │   ├── libkrun/           # our fork (git subtree or submodule pinned to our branch)
  │   └── libkrunfw/         # forked guest kernel bundle (4K pages, balloon, venus config)
  ├── tasks/                 # this doc & follow-ups
  └── ui/                    # Tauri app (Phase 9)
  ```
- [ ] **0.2 VMM backend trait.** Define the `VmmBackend` abstraction in `nebula-core`
  first (create/boot/shutdown, device attach: disk/net/vsock/virtiofs/balloon/gpu,
  lifecycle events) — both spikes implement it; KVM (Linux) and WHP (Windows) slot in
  later. Keep it honest: only abstract what both backends actually share.
- [ ] **0.3 VZ spike (primary path).** Rust bindings to Virtualization.framework
  (`objc2-virtualization`; Lima/vz-for-Go as reference). Boot an arm64 Linux kernel +
  minimal rootfs via VZLinuxBootLoader; verify virtio-blk/net/vsock/virtiofs, the
  **VZVirtioTraditionalMemoryBalloonDevice**, and the **Rosetta directory share**
  (mount + binfmt + run an x86_64 hello binary). Measure cold boot.
- [ ] **0.4 Fork & vendor libkrun (sidecar path).** Clone libkrun + libkrunfw into
  `third_party/` as our working forks (track upstream via a `vendor` branch so rebases
  stay cheap; we own device wiring and kernel config without waiting on upstream;
  upstream the good parts opportunistically). Build on macOS from vendored source;
  raw bindgen vs. safe wrapper decision; krunkit as device-setup reference. Boot a
  microVM, verify virtio-gpu (Venus) device creation, measure ms-class cold boot.
- [ ] **0.5 Code signing & entitlements.** `com.apple.security.virtualization` (VZ) +
  `com.apple.security.hypervisor` (libkrun) entitlements, signing for local dev and
  for distribution (notarization comes later). Script it.
- [ ] **0.6 CI skeleton.** GitHub Actions: macOS runner builds workspace + clippy + fmt;
  guest-agent cross-compile check. (VM boot tests must run on self-hosted/dev machines —
  GH macOS runners support nested virt inconsistently; verify and document.)

**Exit criteria:** `cargo run -p nebula-cli -- up --dev` boots a throwaway VM on the VZ
backend and runs `uname -a`; the libkrun backend boots the same image through the same
trait; an x86_64 binary runs under Rosetta in the VZ guest. Findings (incl. VZ balloon
behavior) documented in `tasks/spike-notes.md`.

---

## Phase 1 — The Vessel (managed guest + control plane)

**Goal:** one long-lived, managed VZ-backed VM ("the Vessel") with a proper control
channel, lifecycle management, and a purpose-built guest image (built once, bootable by
both backends).

- [ ] **1.1 Guest image v0.** Build a minimal rootfs (Alpine-based to start; evaluate
  Buildroot later for size/boot wins): init (openrc or a tiny custom init),
  vessel-agent, containerd + runc + CNI plugins, virtio drivers. Reproducible build
  script in `vessel/` (containerized build so contributors don't need a Linux box).
- [ ] **1.2 Kernel strategy.** One kernel build, two consumers: the Vessel boots it
  via VZLinuxBootLoader; the libkrun sidecar gets it via our libkrunfw fork (or
  libkrun's external-kernel support). Required config: virtio-balloon, virtiofs,
  overlayfs, nf/iptables for CNI, vsock, binfmt_misc, virtio-gpu. **Must be a 4K-page
  kernel** — x86 emulation assumes 4K guest pages (Rosetta, and FEX for the games
  track; 16K kernels break both). Keep this decision revisitable.
- [ ] **1.3 vessel-agent v0 (Rust, static musl build).** Runs as PID-1-adjacent service.
  gRPC over vsock exposing: `Health`, `Exec`, `MemStats` (parse /proc/meminfo +
  /sys/fs/cgroup memory.pressure/PSI), `Shutdown`, `ServiceCtl` (start/stop containerd,
  k3s).
- [ ] **1.4 nebulad v0.** Host daemon (launchd-managed) owning the Vessel lifecycle:
  start/stop/restart, crash detection + auto-restart with backoff, state machine
  persisted in `~/.nebula/state.json`, structured logs to `~/.nebula/logs/`.
- [ ] **1.5 CLI v0.** `nebula up`, `nebula down`, `nebula status`, `nebula logs`,
  `nebula shell` (exec via agent), `nebula doctor` (env/entitlement/disk checks).
  Zero required flags: `nebula up` with no args does the right thing.
- [ ] **1.6 Config.** `~/.nebula/config.toml`: max-ram (default: min(host/2, 32G) —
  ballooning makes generosity cheap), cpus (default: all, scheduler-shared), disk size
  (sparse, grow-on-demand), image channel. All optional.
- [ ] **1.7 Disk layout.** Sparse data volume (raw or qcow-like via virtio-blk) for
  /var/lib/containerd etc., separate from the read-only(ish) rootfs so image upgrades
  don't destroy user data. Define upgrade path now, not later.

**Exit criteria:** `nebula up` cold-boots the Vessel in ≤2s; survives daemon restarts;
`nebula shell` drops into the guest; agent reports memory stats continuously.

---

## Phase 2 — Containers: docker & nerdctl out of the box

**Goal:** standard `docker` and `nerdctl` CLIs on macOS talk to the Vessel with one
command — and can be pointed back at whatever they used before with one command.

- [ ] **2.1 containerd in the Vessel.** Tuned config (snapshotter: overlayfs; later
  evaluate stargz/nydus for lazy pulls). Managed by vessel-agent.
- [ ] **2.2 Docker API endpoint.** Run dockerd inside the Vessel for full Docker API
  compatibility (compose, buildx, credential helpers, etc.) — this is what OrbStack
  does (it runs the open-source Docker Engine in its VM; its speed comes from the
  VM/file/network layers, not from replacing dockerd, which is a thin management layer
  over containerd+runc anyway). Configure dockerd with the **containerd image store**
  (stable since Docker 23/25-era) and pointed at our shared containerd instance, so
  docker, nerdctl, and k3s see one image/snapshot store (pays off in 5.5). Measure
  dockerd's idle/steady-state overhead in the scale rig (6.1); if the API layer itself
  ever shows up in profiles, a shim becomes a data-driven option, not a guess.
- [ ] **2.3 Socket forwarding over vsock.** nebulad proxies:
  - `~/.nebula/run/docker.sock` ⇄ vsock ⇄ guest `/var/run/docker.sock`
  - `~/.nebula/run/containerd.sock` ⇄ vsock ⇄ guest containerd (for nerdctl)
  Low-latency, high-throughput stream proxy in Rust (this path carries image pulls and
  build contexts — benchmark it).
- [ ] **2.4 `nebula use` / `nebula revert` — context manager.** The flagship DX command:
  - `nebula use docker` — creates/updates a Docker **context** named `nebula` pointing
    at our socket and makes it current. Records the previously-current context in
    `~/.nebula/contexts/docker.prev.json` (a revert stack, not just one slot).
  - `nebula use nerdctl` — writes/updates nerdctl's address config (or emits the env
    var setup if config isn't possible), same recording discipline.
  - `nebula use kubectl` — Phase 5, same pattern.
  - `nebula revert docker|nerdctl|kubectl|--all` — restores the exact previous state.
  - **Safety rules:** never delete or rewrite a user's pre-existing contexts; reverts
    are idempotent; `nebula status` shows which tools currently point at Nebula;
    `nebula use` warns if it's about to change a context that targets something that
    looks remote/prod. These tools can target other VMs or production — treat the
    user's prior config as precious.
- [ ] **2.5 nerdctl convenience.** `nebula install-tools` (optional) to brew-install /
  symlink nerdctl + buildkit client configured for the Vessel.
- [ ] **2.6 Image/volume UX.** `nebula ps`, `nebula images`, `nebula prune` as thin
  conveniences over the runtime (don't rebuild docker's CLI — just the cross-cutting
  views the daemon can do better).
- [ ] **2.7 amd64 via Rosetta.** Wire the VZ Rosetta directory share into the Vessel:
  mount, binfmt_misc registration for x86_64 (Rosetta first), `rosettad` AOT-caching
  daemon if available. qemu-user-static binfmt as fallback for exotic arches and
  Rosetta edge cases. `docker run --platform linux/amd64 …` just works at
  Rosetta-class speed; `nebula stats` marks emulated containers. Handle the
  Rosetta-not-installed case in `nebula doctor` (prompt
  `softwareupdate --install-rosetta`).
- [ ] **2.8 Acceptance tests.** Scripted: `docker run hello-world`, `docker compose up`
  on a realistic multi-service app, `nerdctl run`, `docker build` with a large context,
  `docker run --platform linux/amd64` on a real amd64-only image, bind mounts (pending
  Phase 3 virtiofs), then `nebula revert --all` leaves the machine exactly as found.

**Exit criteria:** a fresh user goes `brew install nebula && nebula up && nebula use
docker && docker compose up` and their app runs. `nebula revert docker` puts their
Docker Desktop / colima / remote context back exactly.

---

## Phase 3 — Networking & filesystem

**Goal:** the seams between macOS and the Vessel disappear.

- [ ] **3.1 Userspace networking.** Integrate gvproxy- or passt-style userspace network
  backend (study what krunkit/podman-machine use with libkrun on macOS). Outbound
  internet from containers, correct MTU, IPv6, VPN coexistence (corp VPN is the #1
  networking bug source — test early).
- [ ] **3.2 Dynamic port forwarding.** Watch container/k8s port events (docker events +
  containerd API + later k8s services) and auto-forward published ports to host
  `localhost`. No manual `-p`-flag mapping at the Nebula layer; `docker -p 3000:3000`
  just appears on `localhost:3000`.
- [ ] **3.3 DNS: `*.nebula.local`.** Local resolver (configured via `/etc/resolver/`
  on macOS) so containers get stable names (`myapp.nebula.local` → container IP /
  forwarded port). Container-to-container DNS handled by CNI inside the Vessel.
- [ ] **3.4 virtiofs home mounts.** Mount `$HOME` (allowlist-based, configurable) into
  the Vessel via virtiofs so bind mounts (`-v ~/code/app:/app`) work transparently.
- [ ] **3.5 File performance & coherence pass.** Benchmark virtiofs (small-file stat
  storms — `npm install`, git status; big sequential I/O) vs. OrbStack/Docker Desktop
  numbers. Verify inotify/file-event propagation host→guest (hot-reload dev servers
  must work). This gate decides whether we need caching tweaks (DAX, attr caching) or
  a fallback strategy.
- [ ] **3.6 Host→guest reachability.** Direct routing or forwarder so macOS can reach
  container IPs (for `kubectl port-forward`-free workflows, debuggers, etc.).

**Exit criteria:** a Node/Python hot-reload dev loop with a bind-mounted source dir
feels native; `localhost:<port>` and `name.nebula.local` both work with zero config;
works on corp VPN.

---

## Phase 4 — Elastic memory (the OrbStack secret sauce, in the open)

**Goal:** Nebula holds only the memory its workloads need. User sets a *max*; idle
Vessel costs near-zero RAM.

- [ ] **4.1 Balloon backend reality check (de-risk first — start this spike during
  Phase 0/1).** Characterize `VZVirtioTraditionalMemoryBalloonDevice` on the Vessel:
  does inflating actually return host-visible memory to macOS (verify with
  `footprint`/Activity Monitor), how fast can we deflate, is there free-page-reporting
  or do we drive everything from our controller? OrbStack ships elastic memory on this
  stack, so it's proven possible — but the *policy* layer is entirely ours. For
  libkrun sidecar VMs, balloon matters less (they're ephemeral); if we want it there,
  implement in our fork (HVF unmap/madvise on the host side).
- [ ] **4.2 Guest pressure signals.** vessel-agent streams MemAvailable, PSI (pressure
  stall info), cgroup-level usage per workload class (containers vs k3s), page cache
  size. Add a `drop_caches`-with-judgment hook (sync + targeted cache drop when
  reclaiming, never blindly).
- [ ] **4.3 Host pressure signals.** nebulad watches macOS memory pressure
  (`host_statistics64` / memory pressure notifications). Policy inputs: host pressure,
  guest pressure, user max, hysteresis.
- [ ] **4.4 Balloon controller.** Closed-loop controller in `nebula-balloon`:
  - inflate when guest has sustained surplus (reclaim to host),
  - deflate *fast* when guest pressure rises (never let workloads OOM because of us —
    deflate latency is the key correctness metric),
  - hysteresis + rate limiting to avoid thrash,
  - free-page-reporting / `MADV_DONTNEED`-style page release on the host side so freed
    guest pages actually return to macOS (verify with `footprint`/Activity Monitor,
    not just internal counters).
- [ ] **4.5 Budgets & CLI.** `nebula config set max-ram 96G`; `nebula stats` showing
  host-visible footprint vs guest-visible usage vs balloon size, live.
- [ ] **4.6 Torture tests.** Sawtooth workloads (alloc/free cycles), 200-container
  idle fleet footprint, sudden 32G allocation while ballooned down, k3s churn. Pass =
  no OOM kills attributable to balloon lag; idle footprint < 1 GB with 200 idle
  containers (target, tune as we learn).

**Exit criteria:** Vessel with max-ram 96G but nothing running shows <1 GB host
footprint; heavy workloads get their memory within milliseconds; numbers verified from
the macOS side.

---

## Phase 5 — Kubernetes out of the box

**Goal:** `nebula use kubectl` gives you a local cluster that feels instant — and never
endangers your prod kubeconfig.

- [ ] **5.1 k3s in the Vessel.** k3s using the Vessel's containerd (no
  docker-in-docker). Disabled by default; `nebula k8s enable` (or first `nebula use
  kubectl`) starts it on demand. Measure incremental RAM cost; ballooning should absorb
  it when idle.
- [ ] **5.2 kubeconfig management.** Generate a `nebula` context inside the user's
  kubeconfig (or a separate file stitched via `KUBECONFIG` — decide based on tool
  compat) with client certs from k3s. **Never** touch other contexts/clusters/users
  entries.
- [ ] **5.3 `nebula use kubectl` / `nebula revert kubectl`.** Same revert-stack
  discipline as Phase 2.4. Extra guardrails here because the previous context may be
  **production**: record previous `current-context`, print it loudly on switch
  (`switched from ctx 'prod-us-east' → 'nebula'`), and `nebula status` always shows it.
- [ ] **5.4 Service exposure.** LoadBalancer services via k3s's servicelb mapped through
  Nebula's port forwarder to `localhost`; Ingress on `*.k8s.nebula.local`.
- [ ] **5.5 Image path.** Local images (docker build / nerdctl build) visible to k3s
  without a registry round-trip (shared containerd namespace or image import pipe) —
  the single-runtime architecture should make this nearly free; make sure it is.
- [ ] **5.6 Multi-node later.** Single node first. (Multi-node via extra microVMs is a
  Phase 11 candidate, useful for scheduling/affinity testing.)
- [ ] **5.7 Acceptance tests.** Deploy a realistic helm chart; `kubectl logs/exec/
  port-forward`; build→deploy local image loop; revert leaves kubeconfig byte-identical
  (modulo our own context entry if the user keeps it).

**Exit criteria:** `nebula use kubectl && kubectl apply -f app.yaml` works on a fresh
machine in seconds; `nebula revert kubectl` restores the previous current-context
exactly; local-build images deploy without a registry.

---

## Phase 6 — Reliability & scale hardening

**Goal:** the "tens to hundreds of services, trust it like launchd" bar.

- [ ] **6.1 Scale test rig.** Reproducible harness that launches 100–500 mixed
  services (web apps, databases, queues, cron-ish jobs, a k3s namespace's worth of
  pods) on a high-RAM Mac; collects boot time, steady-state footprint, FD counts,
  proxy throughput, balloon behavior over 24h+ soak.
- [ ] **6.2 Crash & recovery matrix.** Kill -9 nebulad / vessel-agent / containerd /
  the VM itself; sleep/wake; full-disk; clock jumps. Each must recover to a correct
  state automatically and visibly (`nebula status` explains what happened).
- [ ] **6.3 Upgrades.** `nebula upgrade`: replace rootfs image + agent without losing
  container data (Phase 1.7 disk layout pays off here); daemon self-update strategy;
  versioned state files with migrations.
- [ ] **6.4 Diagnostics.** `nebula doctor` grows teeth (network path checks, virtiofs
  health, balloon sanity); `nebula bugreport` bundles logs/state (local file, no
  telemetry — privacy is a feature of being open source).
- [ ] **6.5 Resource QoS.** CPU shares/cgroup weights between docker workloads and k3s
  so one runaway build doesn't starve the cluster; disk usage quotas + `nebula prune`.
- [ ] **6.6 Security pass.** Socket permissions, vsock service authn between host and
  agent, guest hardening (read-only rootfs where possible), review of what `$HOME`
  virtiofs exposure means for container escape blast radius; document threat model.

**Exit criteria:** 24h soak with 200+ services: zero manual interventions; documented
recovery behavior for every crash class.

---

## Phase 7 — libkrun sidecar engine

**Goal:** the second backend goes from spike to product: on-demand libkrun microVMs
managed by the same daemon, sharing images with the Vessel — the substrate for GPU
(Phase 8), sandboxes, and games (Phase 11).

- [ ] **7.1 Sidecar VM lifecycle.** `nebula sandbox run …` spins a dedicated libkrun
  microVM (ms-boot) through the `VmmBackend` trait: same guest image family as the
  Vessel, minimal device set, auto-teardown on exit, N concurrent sidecars.
- [ ] **7.2 Image store: independent by design (v1).** Sidecar VMs pull and store
  images independently of the Vessel — **accepted limitation** on macOS for now
  (GPU/sandbox users are knowledgeable enough to understand the trade-off). Ship it
  documented: a clear docs page on the limitation (duplicate pulls/disk for images
  used by both engines) and a pinned tracking issue describing future options
  (virtiofs-mounted read-only content store, containerd content sharing over vsock,
  snapshot hand-off). `nebula prune` covers both stores; `nebula stats`/`status`
  attribute disk usage per engine so the cost is visible, not surprising.
- [ ] **7.3 Routing layer.** nebulad routes workloads by request: default → Vessel;
  `--gpu` / `nebula sandbox` → libkrun sidecar. Networking: sidecar containers join
  the same DNS/port-forwarding fabric (`*.nebula.local`) so placement is invisible.
- [ ] **7.4 Resource policy.** Sidecars get bounded CPU/RAM slices; balloon in
  sidecars only if cheap (our fork — see 4.1); otherwise rely on their ephemerality.
- [ ] **7.5 amd64-in-sidecar story.** Rosetta is unavailable outside VZ, so sidecar
  VMs use FEX/qemu-user binfmt for x86_64 binaries (4K-page kernel from 1.2 makes FEX
  viable — same recipe as Asahi's muvm). Document the perf difference vs the Vessel.

**Exit criteria:** `nebula sandbox run alpine uname -a` boots, runs, and disappears in
well under a second; a sidecar container resolves and reaches Vessel services by name;
the independent-image-store limitation is documented with a pinned tracking issue, and
per-engine disk usage is visible in `nebula stats`.

---

## Phase 8 — GPU workloads

**Goal:** `--gpu` containers with near-native performance for local AI, running in
libkrun sidecar VMs (Phase 7 substrate).

- [ ] **8.1 Venus pipeline in sidecar VMs.** Enable virtio-gpu (Venus) in our libkrun
  fork's device setup; guest image gains Mesa with the Venus Vulkan driver. Validate
  with `vulkaninfo` in a container.
- [ ] **8.2 Container GPU UX.** `nebula run --gpu …` and/or a device-request convention
  so `docker run --device nebula.gpu` (exact UX TBD) routes to a GPU sidecar and
  injects the Venus ICD + devices into the container. Zero flags beyond opting in.
- [ ] **8.3 AI benchmark.** llama.cpp (Vulkan backend) in a container vs. native Metal
  vs. colima krunkit. Publish honest numbers; this is a headline feature, it needs
  receipts.
- [ ] **8.4 k8s GPU.** Join a GPU-enabled libkrun sidecar as a second k3s node
  exposing the GPU as an extended resource, so pods can request it (single shared GPU
  semantics documented clearly). Doubles as the multi-node proof (11.6).
- [ ] **8.5 Contention policy.** Document/observe GPU contention between containers
  (and future games); simple priority knob if needed. (Full per-workload isolated-VM
  GPU scheduling is Phase 11.)

**Exit criteria:** a containerized llama.cpp serves tokens at a competitive fraction of
native Metal performance, started with one flag.

---

## Phase 9 — Native UI (Tauri)

**Goal:** a lightweight macOS app for the people who don't live in terminals — strictly
a client of the daemon API (which Phase 10 formalizes; build them together).

- [ ] **9.1 Tech: Tauri (decided).** Rust core matches the rest of the stack, tiny
  footprint vs Electron, and the UI ports to Linux when the Linux host lands (11.3).
  Keep the macOS menu-bar integration native-feeling (tray APIs, no dock-hogging
  window by default).
- [ ] **9.2 Menu bar essentials.** Engine status, RAM footprint (live balloon viz —
  this is the wow moment of the product, show actual host footprint shrinking),
  start/stop, container/pod list with logs, `use/revert` toggles per tool.
- [ ] **9.3 Detail windows.** Container inspect/logs/exec terminal, k8s workloads view,
  volumes/images management, settings (max-ram slider, mounts allowlist).
- [ ] **9.4 Distribution.** Goal: install flawlessly via ANY channel —
  - **Nebula.app — fully self-contained (DECIDED + implemented):** bundles
    `nebula` + `nebulad` as Tauri sidecars AND the guest images as gz
    resources (`scripts/bundle-app.sh`); first launch installs images from
    the bundle — zero downloads, works offline. ~200 MB DMG vs OrbStack
    431 MB / Docker Desktop 583 MB (measured 2026-06). Remaining: first-
    launch PATH offer (symlink /usr/local/bin, user-approved), Developer
    ID signing + notarization.
  - **Homebrew**: cask for the app, formula for CLI-only installs.
  - **curl | sh** installer and **GitHub Releases** artifacts (.dmg + CLI
    tarball) — same binaries, one release pipeline.
  - **Image flavors (DECIDED + implemented, `FLAVOR=` in build-rootfs.sh):**
    measured gz artifacts — `full` 161 MB (docker+k3s+in-guest CLIs, the
    engine default), `docker` 57 MB (containerd+dockerd only; host CLI over
    the socket proxy), `minimal` 6 MB (agent-only Linux microVM). With the
    16 MB kernel: an embedder shipping microVMs pays ~22 MB; with docker
    ~73 MB. Both flavors boot-verified. CI should publish all three.
  - **Host CLI bundling (Rancher Desktop pattern, licensing OK):** docker
    CLI, kubectl, helm, nerdctl are all Apache-2.0 — redistributable in the
    .app (RD ships exactly this set; their 555-byte nerdctl is a shim to the
    guest, their kubectl is an alias to kuberlr, a version-matching kubectl
    fetcher). Plan: Nebula.app/Contents/Resources/bin with docker/kubectl/
    helm for users who lack them; `nebula setup` PATH offer covers them.
    Adds ~60 MB gz — keep optional or lazy-fetch, decide with Phase 12.
  - **Image slimming (download is ~177 MB total today):** the Alpine base
    is ~11 MB; the weight is Go binaries (dockerd 70 MB + k3s 67 MB +
    containerd 40 MB + nerdctl 30 MB + docker cli 29 MB + runc 11 MB).
    Options when it matters: ship k3s as a separate artifact fetched on
    first `nebula setup kubectl` (−67 MB), fetch nerdctl lazily (−30 MB),
    trim kernel config (47 MB Image → 16 MB gz today). Rootfs can also be
    grown post-install (host truncate + guest resize2fs) if 2 GB pinches —
    user data already lives on the separate 64 GB sparse data disk.
  - **Mac App Store (separate, constrained edition — feasible but later):**
    `com.apple.security.virtualization` IS compatible with the App Sandbox
    (precedent: Parallels Desktop App Store Edition), so the VZ engine can
    ship there. What does NOT fit MAS rules: installing a launchd agent,
    placing a CLI on PATH, and writing outside the sandbox container —
    an MAS edition would run nebulad inside the app's sandbox container
    with everything relocated under it, no CLI. Verify the
    `com.apple.security.hypervisor` (libkrun sidecars) + sandbox combo
    separately. Direct distribution stays the primary channel.

**Exit criteria:** install the app, never open a terminal, run containers and see the
memory footprint breathe.

---

## Phase 10 — Embedding SDK & API

**Goal:** other apps embed Nebula as their container/VM orchestrator (the agent-
orchestrator product case: managed containers on local k8s inside another app, with
user-customizable agent containers — and the same API shape against a hosted version).

- [ ] **10.1 API surface.** Formalize nebulad's gRPC (+ REST gateway) API: engine
  lifecycle, container/pod CRUD, exec/logs/events streams, mounts, GPU requests,
  memory/stats. Version it from day one (`v1alpha1`).
- [ ] **10.2 Rust crate.** `nebula-core` published as the embeddable crate: run the
  whole engine in-process (no separate daemon) for apps that want full control.
- [ ] **10.3 Client SDKs.** TypeScript and Python first (the orchestrator/agent
  ecosystem languages; microsandbox's 4-language SDK validated this demand). Generated
  from the API schema + hand-polished ergonomics.
- [ ] **10.4 Multi-tenancy within the Vessel.** Namespacing so an embedding app's
  containers don't collide with the user's dev containers (containerd namespaces +
  separate k8s namespace + scoped API tokens).
- [ ] **10.5 Remote/hosted parity.** The same client SDK pointed at a remote nebulad
  (TLS + auth) — the bridge to the hosted version. Local-first, but don't paint the API
  into a local-only corner (no implicit "same filesystem" assumptions in v1 API).
- [ ] **10.6 Reference app.** Small demo: an "agent runner" that spins up customizable
  agent containers against local k8s through the SDK — the dogfood for the product
  thesis.

**Exit criteria:** a third-party app can `npm install @nebula/sdk`, start the engine,
run a customized container, stream its logs — without shelling out to any CLI.

---

## Phase 11 — Stretch tracks (unordered) — status 2026-06-09

Core phases 0–10 implemented and tested (see scripts/test-phase*.sh). Stretch
status: **11.2 is shipped** (`nebula sandbox run`, 250ms isolated microVMs —
landed early as Phase 7). 11.1 (games) has its substrate ready (GPU=1 fork
build, virtio-gpu attaches; needs sommelier + mesa userspace image). 11.3
(Linux host) is unblocked by the `VmmBackend` trait + the fork's KVM path.
11.4/11.5/11.6 not started. Remaining per-track detail below unchanged.

- [ ] **11.1 Games & desktop apps.** muvm-style: sommelier + cross-domain virtio-gpu
  Wayland passthrough so Linux GUI apps (and Steam + FEX for x86) open as native-feeling
  Mac windows. Likely wants the **per-workload isolated microVM** mode (11.2) rather
  than the shared Vessel — games are the definitive noisy neighbor.
- [ ] **11.2 On-demand isolated microVMs.** `nebula sandbox run …`: spin a dedicated
  microVM (ms boot) for untrusted/heavy workloads — microsandbox-style isolation as an
  opt-in, sharing the image/cache infrastructure with the Vessel.
- [ ] **11.3 Linux host support.** libkrun-on-KVM backend for nebulad; CI on Linux;
  this also unlocks the hosted version's substrate (11.5).
- [ ] **11.4 Hosted Nebula.** Remote fleets of Vessels (Linux/KVM) behind the Phase 10
  API for teams/companies; the local↔hosted seamlessness is the long-term product bet.
- [ ] **11.5 Windows host research.** Windows blocked prior art (go-microvm, the
  microsandbox Windows discussions): full Hyper-V is Pro/Server-only, WSL2 nested virt
  is poor, and upstream libkrun has no Windows backend. Owning a libkrun fork changes
  the calculus: the credible path is adding a **WHP (Windows Hypervisor Platform)
  backend** to our fork — WHP is the userspace virtualization API that ships with the
  hypervisor present on *all* Windows 10/11 editions incl. Home (it's what WSL2,
  QEMU-WHPX, and crosvm's Windows port use; crosvm proves a rust-vmm-style VMM on WHP
  is feasible). Guest side stays our same Linux image (x86_64 build of libkrunfw).
  Major effort (vCPU loop, interrupt handling, all device backends re-validated, no
  Venus GPU story on Windows) — research track only until macOS is shipped.
- [ ] **11.6 Multi-node k8s.** Extra Vessel workers for realistic scheduling tests.

---

## Phase 12 — UI direction & "local apps" platform (to discuss after core work)

Captured from discussion 2026-06-09; refine together before Phase 9 (Tauri UI)
implementation starts, since it shapes the UI's information architecture.

- **Layout inspiration.** Proxmox-style left nav (totals at a glance: RAM, CPU,
  disk; tree of containers/VMs/images) is a proven shape for "infrastructure
  at a glance" — but evaluate nicer modern takes too (OrbStack's minimal list
  + detail pane, Portainer's resource views, Rancher's cluster dash). Bias
  toward developer-platform ergonomics over datacenter-admin ergonomics.
- **Developer-centric per-container detail** (the differentiator vs Proxmox):
  - exposed ports, clickable (localhost:PORT / name.nebula.local)
  - whether the workload is a GPU VM/container or plain
  - image provenance: built locally (possibly untagged/dangling) vs pulled
    from a registry — surfaced clearly, homelab-style
  - what's actually running: compose project grouping, k8s workload grouping
- **"Apps" use case (homelab mode).** Make it trivially easy to run real apps:
  one-click/one-command docker-compose apps and helm-chart apps on the local
  k3s. Curated example configs live in our docs as quick installs.
- **Test scenarios double as marketing.** The example-app gallery is both an
  acceptance-test corpus (real compose/helm workloads) and standalone
  publicity for Nebula, independent of orchestrator/galaxy (the opinionated
  agent-orchestration system in ~/Projects/mystral) which embeds Nebula via
  the Phase 10 SDK.

## Cross-cutting risks (watch from day 1)

1. **Elastic memory on VZ's balloon device** (Phase 4.1). Lower risk than before
   (first-party device, OrbStack proves the stack), but VZ is a black box: if its
   balloon semantics are insufficient (reclaim latency, no free-page reporting), we
   can't patch it — our policy layer has to compensate. Spike in Phase 0; verify with
   host-visible footprint, not internal counters.
2. **virtiofs performance & inotify semantics** (3.5). OrbStack set a high bar with
   proprietary tricks; if virtiofs disappoints, dev-loop UX suffers. Benchmark early.
3. **dockerd compatibility surface** (2.2). Compose/buildx/credential-helpers have long
   tails. Mitigation: run real dockerd, don't shim.
4. **VPN/network coexistence** (3.1). Historically the top bug generator for every tool
   in this space. Get corp-VPN testing into CI-adjacent routine early.
5. **k3s + balloon interaction** (4.6/5.1). Kubelet eviction thresholds vs. balloon
   pressure need tuning so they don't fight.
6. **Dual-engine complexity** (Phase 7). Two VMMs means two device matrices to test
   and routing edge cases (GPU container bind-mounting a Vessel volume,
   sidecar↔Vessel networking). Mitigations: one shared guest image/kernel (1.1/1.2),
   one `VmmBackend` trait (0.2), sidecars stay ephemeral and featureless by default,
   and image-store sharing is explicitly *not* attempted in v1 (7.2 — documented
   limitation + tracking issue instead of machinery). Fork maintenance on libkrun:
   track upstream on a `vendor` branch, rebase regularly.
7. **Apple platform coupling.** VZ features (Rosetta share, balloon, virtiofs
   behavior) shift across macOS releases and Apple could change terms; Rosetta is a
   macOS-only answer for amd64 (Linux hosts get native amd64 or FEX; Windows x86 hosts
   run amd64 natively). The backend trait is the hedge — libkrun-primary remains a
   working degraded mode (FEX for amd64).
8. **Single-VM blast radius.** One kernel panic takes out everything. Mitigation:
   fast recovery (6.2), state on separate disk (1.7), and 11.2/Phase 7 sidecar
   isolation for risky workloads.

## Success metrics (v1.0)

- Cold `nebula up`: ≤ 2s. First container after install: ≤ 60s including image pull.
- Idle footprint with engine running: < 500 MB host-visible; with 200 idle containers:
  < 1 GB (targets — revise with data, but publish whatever we measure).
- `docker compose up` parity with Docker Desktop/OrbStack on a reference 10-service app.
- Zero-loss `nebula revert` in 100% of acceptance scenarios, including prod-pointing
  kubeconfigs.
- amd64 containers at Rosetta-class speed (parity with Docker Desktop/OrbStack on the
  same image), with mixed arm64+amd64 compose stacks in one network.
- llama.cpp GPU throughput within a published, honest factor of native Metal.

## Post-plan additions (shipped 2026-06)

- **Named vessels** (`nebula vessels new/ls/start/stop/rm/exec/shell/info/reset`):
  persistent microVMs beside the engine; engine vessel protected (read ops route,
  destructive ops refuse). Default backend libkrun (~100ms boot); `--backend vz`
  runs a vessel on Virtualization.framework (~550ms boot, NAT NIC, stable
  MAC + machine id persisted in spec.json) via a daemon-free `vz-worker` that
  proxies agent/shell unix sockets onto guest vsock and serves a pause/save/
  resume control socket (`vmm.sock`).
- **Snapshots & branching**: disk snapshots = APFS clones (5–12ms, crash-
  consistent via stop→clone→restart); `--memory` (vz) = live pause →
  saveMachineStateToURL → clone disks → resume (~360ms, VM never stops,
  state ≈ touched pages). `restore` resumes mid-execution (~850ms incl.
  stop); `branch --snapshot L --count N` fans out N live clones (~600ms
  each) — the MCTS/tree-search primitive. `vessels new --from-image <ref>`
  turns any arm64 docker image into a managed, snapshot-capable microVM
  (init/agent injected at conversion).
- **Embedding**: NEBULA_HOME isolation; per-instance dns_zone/dns_port/
  k8s_port/api_port; per-instance launchd labels; `scripts/embed-kit.sh`;
  rootfs customization hooks (OVERLAY= dir copied over /, SETUP= script run
  in the image build; nebula init/agent installed last). docs/embedding.md.
- Acceptance: `scripts/test-vessels.sh` (20 checks, both backends, memory
  snapshot round-trip incl. RAM-only witnesses).
