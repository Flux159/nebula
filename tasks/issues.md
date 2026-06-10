# Nebula — Open Issues & Decisions To Discuss

Running log of problems, surprises, and deferred decisions hit during implementation.
Newest at the top within each section. (Items here need Suyog's input or are accepted
limitations; routine TODOs live in code.)

## Open (being worked / next phase)

- **(2026-06-10, Linux) Guest networking strategy for the Linux/krun engine
  needs deciding.** Findings from the KVM spike: our x86_64 vmlinux boots on
  KVM ✓ (reached userspace), but libkrun's TSI on Linux is delivered via a
  `tsi_hijack` LD_PRELOAD helper baked into libkrunfw's initramfs + its
  chroot-mode init — our disk-boot flow (init=/sbin/nebula-init) bypasses
  both, so TSI may not apply to the engine path on Linux the way it does on
  macOS. Options to evaluate once the x86_64 rootfs exists: (a) verify
  whether fork TSI works with custom kernel/init at the vsock layer alone;
  (b) build the fork with NET=1 and use `krun_add_net_unixstream` + passt
  (gives a real NIC; vessel-init's eth0/DHCP path already handles that);
  (c) virtio-net + host TAP (needs privileges). Leaning (b) — passt is
  packaged everywhere and the guest path is identical to the VZ NAT shape.

- **(2026-06-09, Phase 1) libkrun console-to-file needs a fork patch eventually.**
  With a non-tty output fd, libkrun sends guest hvc0 to its *log* and only a
  secondary "krun-stdout" virtio port reaches our fd. Worked around by also
  pointing the worker's stdout at the console file (sufficient for sidecars).
  Fork TODO: an fd-direct console output mode.

## Resolved

- **(2026-06-09, Phase 0→1) Stock Alpine kernel panics at early boot under
  libkrun.** Root cause never directly observed (no console that early), but
  fully resolved by the Phase 1 custom kernel (6.12.58, VIRTIO_MMIO=y et al.):
  the krun spike now passes end-to-end — **227ms boot→poweroff** on the fork
  dylib, same kernel+initramfs as VZ (which passes in ~330ms). Both backends
  boot the same image through the same `VmmBackend` trait.

## Incident log

- **(2026-06-09, Phase 5) Near-miss: acceptance test mutated a real cluster.**
  First run of test-phase5.sh: `nebula use kubectl` failed (k3s binary missing
  from rootfs — a build-context bug had silently produced a stale image), but
  the script kept going, so its `kubectl create deployment nebula-p5` ran
  against the then-current context — your GKE cluster. The test's own cleanup
  deleted it (`kubectl get deploy,svc nebula-p5` → NotFound, verified) and an
  EXIT trap restored the context; no residue. Fixes shipped: the test now
  hard-aborts unless `kubectl config current-context` == nebula before any
  mutating step, and the build scripts fail loudly instead of silently reusing
  a stale image. Lesson for the product: `nebula use kubectl` printing the
  loud prod-context warning is not enough — our own tooling must treat
  "context != nebula" as a stop condition, and the same gate belongs in any
  future `nebula k8s …` convenience commands.

## Needs discussion

- **(2026-06-10) The right way to embed Nebula in another app — discussion
  with Suyog (scheduled 10:00 PST).** Current story is sidecar-shaped
  (embed kit: bin/ + lib/ + images/, NEBULA_HOME isolation, REST/SDKs,
  docs/embedding.md). Open questions to settle:
  * sidecar CLI vs in-process `nebula-core` vs a long-lived supervisor API —
    where's the supported line?
  * engine lifecycle ownership: who starts/stops/updates the engine when
    several apps embed it (shared engine vs per-app NEBULA_HOME — resource
    cost says share, isolation says split);
  * update channel: app ships pinned sidecars — how do guest images +
    binaries upgrade together (version-compat matrix, `install-image` flow);
  * API surface to freeze for embedders (REST v1alpha1 vs gRPC vs typed
    SDKs as the contract), incl. vessel/snapshot APIs which are CLI-only
    today;
  * agent-image distribution: vessel-base on Docker Hub + convert-image vs
    --vessel-image prebaked kits;
  * embeddable UI pieces (status panel / containers list as web components
    the host app drops in?).

- **(2026-06-10) UI direction for the user-facing window — discussion with
  Suyog (scheduled 10:00 PST).** Synology-style app catalog as the headline
  (see features.md Phase 12 update): app manifests over compose/helm,
  one-click install, snapshot-before-upgrade rollback; what the default
  window shows non-technical users vs the current containers/k8s developer
  views; logs/shell affordances (in-app terminal vs copyable commands).

- **(2026-06-10) `nebula sandbox` from the bundled app can't build its
  initramfs.** Ephemeral sandboxes assemble an initramfs from the dev tree's
  musl `vessel-init` binary, which isn't shipped in Nebula.app — so sandbox
  (only) fails from a bundle install with "vessel-init (musl) not built".
  Named vessels/engine are unaffected (they boot rootfs.img directly). Fix
  options: ship a prebuilt `initramfs.cpio.gz` as an app resource +
  `install-image` artifact, or extract /sbin/nebula-init from the installed
  pristine rootfs at first sandbox use. Small, but needs a decision.

- **(2026-06-10) Publish the Nebula base image to Docker Hub.** Agreed
  direction: push versioned `flux159/nebula-base:<ver>-<flavor>` images from
  the guest-images CI so embedders author rootfs customizations as plain
  `FROM flux159/nebula-base` Dockerfiles (standard tooling, layer caching)
  instead of overlay dirs; `vessels convert-image` / `--from-image` already
  handle the conversion, and init/agent injection at conversion keeps the
  boot contract ours. Needs: a Docker Hub org/repo decision, tag<->binary
  version-compat policy (guest agent protocol vs shipped sidecars), CI push
  credentials. Overlay/SETUP hooks stay as the offline path.

- ~~Memory-state snapshots (live fork) — future~~ **SHIPPED for vz vessels
  (2026-06-09):** `vessels new --backend vz` + `vessels snapshot <v> <l>
  --memory` does pause → saveMachineStateToURL → APFS-clone disks → resume
  (~360ms, VM never stops; state file ≈ touched pages only, 33 MiB for an
  idle 2 GiB vessel). restore/branch resume mid-execution (tmpfs contents
  and killed processes verified to come back). Findings & remaining seams:
  * **VZGenericMachineIdentifier must be persisted in the spec** — restore
    under a fresh random identifier fails with "invalid argument". Same for
    the NAT MAC. Both now live in spec.json (vz vessels only).
  * **Memory-branches share network identity** (same MAC + machine id +
    in-guest DHCP lease, by necessity — the saved state embeds them).
    vsock control (exec/shell/agent) is fully per-VM and unaffected;
    outbound NAT worked in testing with 4 same-identity VMs live, but
    concurrent heavy network use across branches is uncharted. Cold-boot
    branches get fresh MAC + machine id.
  * **Guest wall clock after restore is stale** until something re-syncs it
    (untested edge; agents comparing timestamps across a restore boundary
    may see jumps). Candidate fix: agent re-syncs time on resume detection.
  * **krun vessels:** still disk-only — Firecracker-style snapshot/restore
    on HVF is fork-level work. GPU vessels therefore can't memory-snapshot.
  * **Engine vessel:** not wired up; its config (balloon + virtiofs $HOME
    share + Rosetta share) may fail validateSaveRestoreSupport — needs a
    dedicated investigation if "suspend the whole engine" becomes a feature.

- ~~Multi-instance seams~~ **RESOLVED (2026-06-09):** dns_zone/dns_port/
  k8s_port are per-instance config (guest learns the DNS port via kernel
  cmdline; kubeconfigs are written against the configured k8s port; clients
  read effective values from status); launchd label derives from a
  NEBULA_HOME hash with NEBULA_HOME in the agent env. Verified: branded
  galaxy.local instance with private ports running beside the standalone
  engine. Bonus root-cause fix: a starting nebulad now refuses to steal a
  live sibling's control socket (stray daemons used to accumulate and hold
  the shared ports — that's what broke kubectl mid-evening).

- **(2026-06-09, Phase 8) GPU shipped at device level; Venus userspace is
  follow-up.** `nebula sandbox run --gpu` attaches virtio-gpu via our GPU=1
  fork build (card0 + renderD128 visible, virtio driver bound; brew
  virglrenderer provides Venus host-side). Remaining for the headline AI
  use case: a GPU guest image with mesa-vulkan(venus) + vulkan-tools, then
  the llama.cpp Vulkan benchmark vs native Metal vs colima. ~~Also note the
  GPU=1 dylib is a local build artifact — distribution needs us to ship it.~~
  RESOLVED (2026-06-10): `scripts/package-libkrun.sh` makes the full GPU
  closure relocatable (libkrun + virglrenderer + libepoxy + MoltenVK, 14 MB,
  @loader_path rewrites + re-sign); bundle-app.sh ships it in
  Nebula.app/Contents/Frameworks, embed-kit.sh in lib/; dylib resolution
  walks ancestors of the running binary (Frameworks/, lib/, dev tree) before
  brew, so no NEBULA_LIBKRUN_PATH needed anywhere. Verified: sandbox + GPU
  sandbox boot from a bare /tmp copy with no homebrew references.

- **(2026-06-09, Phase 4) VZ balloon = high-water-mark semantics, not true
  reclaim.** Characterized on macOS 26: `VZVirtioTraditionalMemoryBalloonDevice`
  negotiates only MUST_TELL_HOST+DEFLATE_ON_OOM (no FREE_PAGE_HINT/REPORTING,
  even though our kernel has PAGE_REPORTING=y). Consequences, measured:
  * idle Vessel footprint **1.1 GiB** (untouched pages cost nothing) ✓
  * under a 6 GiB workload footprint grows to ~7.2 GiB ✓, balloon deflates
    instantly, no OOM ✓
  * after the workload exits and the balloon re-inflates (24.9 GiB held),
    footprint **stays at ~7.2 GiB** — VZ does not discard the dirty pages;
    they remain "Dirty/app-specific tag 1" (presumably compressible by macOS
    under real host pressure, unverified).
  So: footprint ≈ high-water mark of touched memory, growth is bounded by the
  balloon, but Activity Monitor won't show post-workload shrinkage. Options to
  discuss: (a) accept + document (Docker Desktop behaves the same way),
  (b) periodic planned Vessel reboot/"compaction" for long-lived setups,
  (c) Apple feedback for free-page reporting, (d) note that on the **libkrun
  side we own the VMM** — sidecars (and a hypothetical libkrun-primary mode)
  can MADV_FREE on inflate for true reclaim, which VZ can't do. OrbStack
  claims true shrinkage with their VMM — consistent with (d).

- **(2026-06-09, Phase 2) nerdctl has no native macOS binary.** `nebula use
  nerdctl` writes the host config pointing at our containerd socket, and the
  guest image ships nerdctl (`nebula exec nerdctl …` works), but a first-class
  host UX needs either a lima-style wrapper script on PATH or a streaming exec
  alias. Options to discuss; docker path is unaffected.

- **(2026-06-09, Phase 2) VZ NAT gateway (192.168.64.1) does not serve DNS on
  macOS 26** — guests get the gateway as DHCP nameserver but port 53 is refused
  (outbound IP traffic works fine). Interim: nebula-init pins resolv.conf to
  1.1.1.1/8.8.8.8, which breaks split-horizon/corp-VPN DNS. Proper fix in
  Phase 3: nebulad-side DNS forwarder using macOS's resolver, guest pointed at
  it (also the basis for *.nebula.local).

- **(2026-06-09) Stray `package.json` in repo root.** Untracked file named `nebulas`
  with a placeholder build script predates implementation. Left untracked — delete it,
  or is it intentional (npm SDK placeholder)?

## Accepted limitations (documented, tracking issue to open on GitHub)

- **Sidecar VMs don't share the image store with the Vessel (v1).** Per discussion:
  GPU/sandbox microVMs pull independently; docs + pinned issue when Phase 7 lands.

## Resolved / informational

- **No Homebrew formula for plain `libkrun`/`libkrunfw`.** The `slp/krunkit` tap ships
  `libkrun-efi` (EFI flavor, arm64 bottle) + `virglrenderer` (Venus) + `krunkit`.
  Using `libkrun-efi` for the Phase 0 FFI spike; the libkrunfw-flavor kernel question
  moves to Phase 7 (vendored fork can build either flavor).

- **(2026-06-09) Stock `libkrun-efi` fallback bricked krun vessels without
  `NEBULA_LIBKRUN_PATH`.** dylib resolution preferred the brew candidates, and
  the stock EFI build can't direct-kernel-boot our raw Image
  (`FirmwareInvalidAddress(GuestAddress(0))`) — phase scripts masked it by
  exporting the env var. Fixed: resolution now walks up from the running
  binary and prefers our fork (`Contents/Frameworks/libkrun.dylib` in a
  bundle, `third_party/libkrun/target/release/` in the dev tree) before brew
  paths. Distribution note stands: the app/embed kit should ship the fork
  dylib in Frameworks for sandbox/GPU/krun-vessel features.
