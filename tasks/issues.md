# Nebula — Open Issues & Decisions To Discuss

Running log of problems, surprises, and deferred decisions hit during implementation.
Newest at the top within each section. (Items here need Suyog's input or are accepted
limitations; routine TODOs live in code.)

## Open (being worked / next phase)

- **(2026-06-12) dockerd fd limit caps idle-container density at ~680,
  before memory does (container-scale sweep).** At 8 GiB max, container #685
  failed with `pipe2: too many open files` inside dockerd's iptables setup
  (676 still running, guest memory fine). 4 GiB broke at 480 on memory; every
  ceiling ≥8 GiB will flatten at the fd wall instead. Fix candidate:
  vessel-init should raise RLIMIT_NOFILE for dockerd/containerd (and slimd)
  well above the Alpine default before exec. Worth re-running the idle line
  after the fix to find the real memory-bound curve.

- **(2026-06-12) Balloon battle-test characterization (nebula-battletest,
  full suite, 128 GiB M-series Mac).** All 19 contract checks pass; baseline
  committed at `bench/baselines/Suyogs-MacBook-Pro.json` (±15% gate, run
  `scripts/battletest.sh balloon` to compare). Numbers worth knowing:
  * 6 GiB hog → footprint peaks ~8.0 GiB, re-inflate ~36 s, settled footprint
    = peak (high-water-mark semantics, as characterized in Phase 4).
  * 10 hog cycles: idle balloon level drifts **0.05%** — no controller leak.
  * A hog sized to **95% of available** (16 GiB engine) *completes* with zero
    guest OOM-kills — deflate + DEFLATE_ON_OOM absorb the whole spike.
  * Sawtooth (4 GiB hog / 30 s idle, 10 min): **0.7 balloon resizes/cycle**,
    confirming the one-jump-per-workload-change claim under repetition.
  * Surprise fixed in the harness: "guest available" right after a workload
    races the re-inflate window (45 s) — anything sizing itself off avail
    must settle first. Also: agent-healthy ≠ dockerd-ready; dockerd answers
    ~seconds after the agent, poll `docker version` (phase scripts knew this).

- **(2026-06-10 PM) VZ memory-RESTORE suddenly fails with "permission
  denied" on the dev Mac — environmental, evidence says reboot first.**
  saveMachineStateToURL still works (200ms); restoreMachineStateFromURL
  fails for freshly saved state, same binary, seconds apart. Bisect facts:
  the commit that passed 24/24 last night fails identically today; a
  Developer-ID-signed (hardened runtime) binary fails identically to
  ad-hoc; no macOS update (same 25F80, uptime spans the working period);
  no sandbox denials in the unified log; nothing logged by the
  Virtualization subsystem during the failure. Day's churn included ~14
  hard-killed VMs (pkill -9 cycles during Linux bring-up) — prime suspect
  is wedged per-VM `com.apple.Virtualization.VirtualMachine` service state.
  NEXT: reboot the Mac, rerun scripts/test-vessels.sh (expect 24/24);
  if it persists, write a 50-line standalone VZ save/restore repro for
  Apple feedback. Note for the roadmap: this is exactly the
  Apple-opacity class of failure that the fork-native krun-snapshot track
  (above) eliminates. The OS-aware suite passes 17/17 on macOS for
  everything except the restore-dependent checks; Linux passes 17/17.

- **(2026-06-10) Fork snapshot/restore ("krun-snapshot") — the Firecracker
  design, agreed as the next fork track.** Supersedes the old "memory-state
  snapshots are vz-only" limitation with a cross-platform plan:
  * Snapshot = guest RAM written to a file + serialized vCPU state
    (KVM get/set APIs on Linux, hv_vcpu get/set on macOS/HVF) + virtio
    device state (our device surface is small: blk/vsock/net/fs/rng/console).
  * Restore = `mmap(snapshot, MAP_PRIVATE)` + load the vCPU/device blob —
    target <10ms, nothing eagerly copied (pages fault in via page cache).
  * **CoW page sharing**: N clones restored from one snapshot file share
    physical pages until written (MAP_PRIVATE is POSIX — works on macOS
    too). Marginal memory per idle clone ≈ dirtied pages (single-digit MB).
    This is the Lambda/Fargate density model and the engine of
    "microVMs for almost everything" + tasks/microvm-k8s-brief.md.
  * Disk-side analog: split `convert-image` output into a shared read-only
    base.img (per base layer chain) + small per-app upper disk joined by
    overlayfs in-guest — restores containerd-style layer dedup for vessels.
  * Order: KVM first (ubuntu box, Firecracker as reference map), HVF second
    (gives GPU vessels snapshots — impossible on vz). VZ vessels keep
    Apple's save/restore as the slow tier (~400-600ms, opaque sharing).

- ~~docker attach/exec streams half-close saga~~ **RESOLVED (2026-06-10):
  two fork patches.** Hijacked HTTP connections (docker
  run/exec output) lost server->guest bytes: docker CLI half-closes its
  write side when stdin isn't attached, and libkrun's host-side unix proxy
  answered read-EOF with a full VSOCK RST, killing dockerd's response.
  Fork patch adds MuxerRx::ShutdownSend (VSOCK_OP_SHUTDOWN +
  SHUTDOWN_SEND flags) so host EOF half-closes the guest side instead.
  Patch 1 alone HUNG instead: the ShutdownSend packet copied Reset's
  buf_alloc=0/fwd_cnt=0 — and virtio-vsock updates peer credit from EVERY
  packet, so the guest's send window froze and the response never flowed.
  Patch 2 carries live credit (CONN_TX_BUF_SIZE + tx_cnt) in the shutdown
  packet. Verified: docker run / run -i / exec all stream correctly; pulls,
  ports, k8s unaffected. Watch-item: one transient guest-DNS timeout on the
  first query right after boot (relay/UDP-flow race; retry clean).

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

- **(2026-06-10) WHV_REGISTER_VALUE alignment bug — upstream candidate.** The
  Windows SDK declares `WHV_REGISTER_VALUE` as `DECLSPEC_ALIGN(16)`, but the
  windows-sys 0.61 binding is plain `repr(C)` (8-aligned). WinHvPlatform
  touches caller arrays with aligned vector loads, so any locally-built
  register array works or access-violates depending on where the stack
  landed: lstocchi's `setup_msrs_on_real_vcpu` unit test passes by luck while
  the same call AV'd (0xC0000005) on the first
  `WHvSetVirtualProcessorRegisters` of a real boot. Fixed in our fork with a
  `#[repr(C, align(16))] AlignedRegisterValue` wrapper used by
  `get_registers`/`set_registers`/`set_registers64` (commit in fork subtree).
  **TODO**: report to lstocchi/upstream libkrun (their whp crate has the same
  latent bug) and possibly to microsoft/windows-rs (binding misses the
  alignment attribute).

- **(2026-06-10) Windows epoll bridge: one spurious completion per re-arm.**
  The IOCP wait-completion-packet epoll re-arms while a manual-reset event is
  still signaled, so device workers wake once more after draining their queue
  EventFd. Handled by treating `WouldBlock` queue-event reads as quiet no-ops
  (block/vsock/console). If we ever see sustained spinning, the fix is to
  re-arm only after the consumer resets the event.

- **(2026-06-10) winsock SD_RECEIVE shutdown RSTs with queued data.** The guest
  closes exec streams with both vsock shutdown flags; mapping that to
  `Shutdown::Both` on the host loopback TcpStream made winsock send RST when
  inbound data was still queued, destroying the final output + exit status of
  short commands race-dependently (unix sockets never do this). The Windows
  vsock backend now propagates only the FIN (SD_SEND) and ignores the
  receive-half shutdown.

- **(2026-06-10) WHP guests booted with a 1999 wall clock — CMOS had no RTC.**
  The fork's CMOS device only carried memory-size registers; the mc146818
  driver read zeros and Linux fell back to 1999-11-30, breaking every TLS
  handshake (docker pulls failed before the first byte). KVM masked this via
  kvmclock; WHP has no paravirt clock. Fixed by serving live BCD time/date/
  status RTC registers from the host clock.
  **RESOLVED 2026-06-10 (part 2 — drift):** even with a correct boot clock the
  guest lost monotonic+wall time ~1:1 with idle time (clocksource fell back to
  refined-jiffies; ticks dropped during HLT; "tsc: Marking TSC unstable due to
  running on Hyper-V"). Root cause: the WHP partition advertises the Hyper-V
  identity + reference counter/TSC (synthetic features 0xB8F) but the guest
  kernel had no CONFIG_HYPERV/CONFIG_HYPERV_TIMER, so the hypervisor-served
  clocksource didn't exist. Fragment now enables them — which exposed part 3:
  **WHP partition reference time (ref counter AND TSC page) only advances
  while a VP is inside WHvRunVirtualProcessor.** With sleep-per-HLT idle
  handling, both Hyper-V clocksources froze while idle. Final fix: the VMM
  serves HV_X64_MSR_TIME_REF_COUNT itself from host QPC (CPUID drops
  ACCESS_REF_TSC; features bank 0x987 so the MSR exits to userspace).
  Verified: /proc/uptime tracks daemon wall time exactly; wall clock within
  1s of host through boot. Cost: one MSR exit per guest clock read — a
  host-backed TSC page is a future optimization if clock-read overhead ever
  shows up in profiles.

- **(2026-06-10) Force-killing the Windows VM worker corrupts the data disk**
  (expected — dirty ext4, no journal replay survived repeated mid-write
  kills during bring-up; EBADMSG from /var/lib/nebula afterwards). Dev-loop
  hygiene: `nebula down` (graceful) before rebuilds; recovery: delete
  data.img. The guest could run e2fsck -p on mount failure in nebula-init —
  TODO worth doing for crash resilience on all OSes.

- **(2026-06-11) krun-snapshot restore design (capture side DONE).** Capture
  verified on KVM: pause(0ms) → save(0.61s: 2GB RAM → 103MB sparse via
  zero-page holes + vm.state + vcpus.state + devices.state) → resume. Key
  design points for restore (next):
  * Device state WITHOUT per-device hooks: virtio queue config is frozen at
    DRIVER_OK, so MmioTransport clones queue states at activation
    (`activated_queue_states`) and `snapshot_state()` captures transport regs
    + those. Dynamic ring cursors are NOT captured — they're reconstructed at
    restore from the rings in restored guest RAM (`avail.idx`/`used.idx`),
    valid because device workers keep draining while vcpus are paused
    (quiesce), so device-side cursors equal the in-RAM indices.
  * Restore flow: builder gets a restore mode (VmResources.restore_dir):
    map memory.bin regions MAP_PRIVATE (CoW—clones share pages), create
    devices in the SAME order (deterministic MMIO addrs/irqs), walk the bus
    and `restore_from_state()` each transport — which replays activate() at
    DRIVER_OK, respawning workers with restored queues — then Vm::restore_state
    (irqchip/PIT/clock), per-vcpu Vcpu::restore_state before start_threaded,
    start paused, resume.
  * Host-side connections don't survive (vsock proxies/usernet flows reset;
    guest-side listeners + agent survive in RAM; nebulad reconnects).
  * Windows/WHP parity after KVM works: same flow, vcpu state via
    WHvGet/SetVirtualProcessorRegisters (alignment wrapper) + XSAVE state;
    memory restore via MapViewOfFile copy-on-write.

- 2026-06-11 — krun restore WORKS on Linux/KVM: full round-trip (snapshot a
  live vessel -> stop -> restore) resumes in ~107ms with tmpfs contents and
  uptime continuity intact. Three findings from getting it green:
  * mmap file offsets must be page-aligned: memory.bin's region data now
    starts at a 4KiB boundary (header padded with a hole). First attempt
    failed with EINVAL mapping region @0x0 at offset 40.
  * Ring-cursor reconstruction from RAM must resume BOTH cursors at
    `used.idx`, not next_avail at `avail.idx`: descriptors the driver posted
    that the device never returned (vsock/net RX buffer pools the device
    holds long-term, in-flight requests) live between used.idx and avail.idx,
    and the device's internal copies die with the old process. Restoring
    next_avail=avail.idx skips them — symptom: guest alive (timer ticks) but
    ALL vsock dead (muxer has no RX buffers to deliver into), agent never
    answers.
  * After the activate() replay, kick every ready queue's eventfd once: the
    driver's notification-suppression state predates the restore, so the
    re-offered descriptors would otherwise sit unprocessed until the next
    organic kick.
  * Bonus fix: libkrun's build.rs used #[cfg(target_os)] (HOST os in a build
    script) and leaked -install_name into mac->windows-gnu cross-links; now
    dispatches on CARGO_CFG_TARGET_OS. Upstream candidate.
  * kvmclock is restored to the saved instant, so guest wall-clock resumes
    behind real time until NTP/agent corrects it — same semantics as VZ.
  * `cargo build -p libkrun` WITHOUT `--features blk,net` produces a
    featureless .so that breaks vessel boot ("NetSpec::Nat needs NET=1") —
    always build the fork on the ubuntu box with `--features blk,net`.

- 2026-06-11 — Windows/WHP restore WORKS end-to-end (resume ~2.2s incl disk
  copies; tmpfs + uptime continuity + agent verified). Three root causes dug
  out of "restored guest wedges silently", in order of discovery:
  * virtio-console: Port IO threads only start on the guest's PORT_OPEN
    control message — a restored guest never repeats the handshake, so its
    first console write spun forever in __send_to_port (IRQs off, port lock
    held) polling a TX ring nobody drains. CROSS-PLATFORM bug; Linux just
    never tripped it because kvmclock restore causes no immediate printk
    while the QPC time jump on Windows does. Fix: VirtioDevice::post_restore
    hook called by the transport after the activate replay; console starts
    all ports. Verified on Linux with a post-restore /dev/kmsg write.
  * WHvX64RegisterTscDeadline wasn't captured: Linux's TSC-deadline LAPIC
    timer never refires post-restore. Restored last (after the interrupt-
    controller blob) so the APIC state restore can't clear it.
  * IA32_XSS (WHvX64RegisterXss) wasn't restored before
    WHvSetVirtualProcessorXsaveState: XSAVES-format task FPU buffers in
    restored RAM misparse -> guest "Bad FPU state detected" panic. XCR0+XSS
    now precede the xsave blob; Xss/Xfd/SpecCtrl are captured singly and
    skipped where unsupported (one bad name fails a whole WHP batch).
  Debug machinery that cracked it (kept, env-gated NEBULA_SNAP_DEBUG):
  whp::debug_sample_vp / debug_sample_apic + a post-restore sampler thread
  and per-vcpu exit logging — plus resolving guest RIPs against the unstripped
  kernel image (nm ~/.nebula/kernel/Image) which turned "spinning at
  0xffffffff8182b9f8" into "__send_to_port+0x108".
  Windows-only nebula fixes along the way: clone_file shelled out to `cp`
  (absent on Windows; named vessels had never run there), the krun-worker CLI
  subcommand was stale-gated to unix, and spawned workers inherited the CLI's
  stdout pipe handle (CreateProcess bInheritHandles) so `vessels new | ...`
  never saw EOF — stdio inherit flags are now stripped before spawning.
  Perf note: Windows snapshots are full-size on disk (no sparse files / no
  reflink on NTFS — 2GiB memory.bin, 7s save vs 1.2s/100MiB on Linux);
  FSCTL_SET_SPARSE is the follow-up.

- 2026-06-11 — arm64 Linux release lane added (native ubuntu-24.04-arm
  runners; no cross toolchain). The krun snapshot machinery is x86_64-only
  (KVM state structs differ per arch), so SaveState/save_snapshot/
  krun_vm_save are now arch-gated; aarch64 builds get boot + pause/resume
  but not memory snapshots until an aarch64 VcpuState is written (kvm_vcpu
  aarch64 regs via GET_ONE_REG list — follow-up). Verified: fork + workspace
  cargo check on aarch64-unknown-linux-gnu both clean.
