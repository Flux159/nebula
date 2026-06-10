# Nebula — agent working notes

Open source container/Kubernetes/microVM manager for macOS. Rust workspace;
dual VMM backends (Virtualization.framework primary "Vessel" VM, vendored
libkrun fork in `third_party/libkrun` for GPU/sandbox sidecars). Plan lives in
`tasks/features.md`; running issue log in `tasks/issues.md`; backend findings
in `tasks/spike-notes.md`.

## Workflow rules

- **Never block on long-running work.** Kernel/rootfs builds, acceptance
  suites, and soaks run as background processes with a monitor attached;
  keep implementing the next task while they run and react to the
  notification. Ideally tests run in CI while local work continues.
- Each phase: implement → test (`scripts/test-phaseN.sh`) → fix → commit →
  push. Log surprises and deferred decisions in `tasks/issues.md` instead of
  stopping to ask.
- `cargo build` (and clippy --fix) **invalidates the ad-hoc code signature**;
  any binary that touches Virtualization.framework must be re-signed first:
  `scripts/sign-dev.sh target/debug/nebula target/debug/nebulad`. The phase
  test scripts do this themselves — prefer running them over raw binaries.

## Build & test

- Workspace build: `cargo build` (excludes `third_party/`).
- Guest binaries: `cargo build -p vessel-init -p vessel-agent --release
  --target aarch64-unknown-linux-musl`.
- Guest kernel: `vessel/build-kernel.sh` (container build, ~10 min; backgrounds
  well). Rootfs: `vessel/build-rootfs.sh` (~1 min). Install into ~/.nebula:
  `target/debug/nebula install-image`.
- These two build scripts pin `DOCKER_CONTEXT=default` so they keep working
  while `nebula use docker` is active.
- libkrun fork: `scripts/build-libkrun.sh` (zig as Linux CC, brew llvm
  libclang; see script comments for the dyld quirks).
- Acceptance: `scripts/test-phase{1..N}.sh`. Phase suites assume guest images
  are current — rebuild rootfs after touching vessel-init/vessel-agent.

## Gotchas that already bit us

- kconfig keeps the FIRST assignment per symbol — kernel fragment is applied
  with `vessel/kernel/apply-fragment.py`, not concatenation. BRIDGE=y needs
  IPV6=y.
- `nebula status | grep -q` style checks: capture to a file first or rely on
  the CLI's SIGPIPE handling; `set -o pipefail` + early-closing readers cause
  phantom failures.
- VZ NAT gateway serves no DNS on macOS 26; guest DNS goes through the agent
  relay → nebulad resolver (host getaddrinfo). Containers use docker0
  (172.17.0.1) as their nameserver.
- VZ balloon = high-water-mark semantics (no page discard on re-inflate); the
  enforceable contract is bounded growth, not post-workload shrink. Details in
  tasks/issues.md.
- Guest memory is charged to `com.apple.Virtualization.VirtualMachine` XPC
  processes, not nebulad — footprint metrics must read those (proc_pidpath).
