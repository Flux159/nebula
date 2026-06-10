# Nebula — Open Issues & Decisions To Discuss

Running log of problems, surprises, and deferred decisions hit during implementation.
Newest at the top within each section. (Items here need Suyog's input or are accepted
limitations; routine TODOs live in code.)

## Open (being worked / next phase)

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

- **(2026-06-09, Phase 8) GPU shipped at device level; Venus userspaceis
  follow-up.** `nebula sandbox run --gpu` attaches virtio-gpu via our GPU=1
  fork build (card0 + renderD128 visible, virtio driver bound; brew
  virglrenderer provides Venus host-side). Remaining for the headline AI
  use case: a GPU guest image with mesa-vulkan(venus) + vulkan-tools, then
  the llama.cpp Vulkan benchmark vs native Metal vs colima. Also note the
  GPU=1 dylib is a local build artifact — distribution needs us to ship it.

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
