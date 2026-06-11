# Running TODO (cross-session backlog)

Short items that aren't full features.md phases. Date = when added.

## Snapshots
- [ ] **Windows sparse snapshot files** (2026-06-11): memory.bin is written
  with seek-holes but NTFS doesn't make files sparse unless marked — set
  `FSCTL_SET_SPARSE` on creation in `save_memory` so a 2 GiB idle-VM image
  costs ~100 MB on disk like Linux, not 2 GiB. Same for the disk copies in
  `clone_file` on Windows (`FSCTL_SET_SPARSE` + `FSCTL_SET_ZERO_DATA`).
- [ ] Windows CoW restore (2026-06-11): restore reads memory.bin into
  anonymous memory; switch to `CreateFileMapping` + `MapViewOfFile(FILE_MAP_COPY)`
  for lazy, branch-shareable RAM like the Linux `MAP_PRIVATE` path.
- [ ] aarch64-linux memory snapshots (2026-06-11): VcpuState via
  `KVM_GET_ONE_REG`/`KVM_SET_ONE_REG` reg list + aarch64 VmState (GIC state);
  arm64 hosts currently get boot + pause/resume only.
- [ ] KVM dirty-log incremental saves; quiesce-settle before save.

## API / embedding
- [ ] WebSocket `shell` endpoint (interactive TTY over the HTTP API).
- [ ] kubeconfig `server:` host rewrite (+ cert SANs) for remote embedders.
- [ ] gzip'd `rootfs_img` accepted over the API (raw-only today).
- [ ] Slim e2e tests in CI (corpus/run-all.sh runs nowhere automatically).

## Upstream candidates (libkrun fork -> containers/libkrun)
- [ ] WHV_REGISTER_VALUE 16-byte alignment wrapper (windows-sys is 8-aligned).
- [ ] WHP timekeeping: TIME_REF_COUNT from host QPC, CMOS RTC, HYPERV_TIMER.
- [ ] build.rs target_os via CARGO_CFG_TARGET_OS (host/target cross bug).
- [ ] The snapshot/restore patchset (KVM + WHP).

## Pending / blocked
- [ ] Mac VZ-restore "permission denied" — was awaiting a Mac reboot; retest.
- [ ] Tag v0.1.0 when Suyog says go (all release lanes green incl. slim).
