# Phase 0 spike notes

Findings from the dual-backend bring-up (0.3/0.4). Living doc.

## Kernel format

- Alpine v3.22 aarch64 `vmlinuz-virt` ships in **EFI zboot** format: `MZ` header,
  `"zimg"` magic at offset 4, payload offset/size at 8/12, `"gzip"` tag at 0x18,
  gzip-compressed arm64 `Image` inside. VZLinuxBootLoader rejects it as-is
  ("Internal Virtualization error") — must unwrap to the raw `Image`
  (`ARM\x64` magic at 0x38). Handled in `extract_arm64_image`.
- libkrun's `krun_set_kernel` accepts RAW / IMAGE_GZ / IMAGE_ZSTD etc., so the
  unwrapped Image works for both backends. One kernel, two backends — confirmed plan.

## libkrun (brewed `slp/krunkit` tap)

- `libkrun-efi` 1.16.0 bottle ships `krun_set_kernel` (external kernel) +
  `krun_add_virtiofs` + `krun_add_disk2` + `krun_set_console_output` — the EFI
  flavor is NOT limited to EFI disk boot, so sidecars can boot our kernel directly.
- `krun_start_enter` takes over the process → worker-subprocess model (like krunkit).
  Implemented as a hidden `nebula krun-worker` subcommand.
- `virglrenderer` (Venus) comes from the same tap for Phase 8.

## VZ backend

- `VZFileSerialPortAttachment` (append=false) needs the file pre-created or it
  errors; we truncate-create it before attach.
- VZVirtualMachine is queue-bound: all ops via `dispatch_sync` onto a dedicated
  serial queue (see threading note in `backend/vz.rs`).
- Entitlements: ad-hoc codesign with `com.apple.security.virtualization` is enough
  for dev (`scripts/sign-dev.sh`); unsigned binaries fail at `start` with a vague
  internal error.

## libkrun console plumbing (fork 1.18.0)

- `krun_set_console_output` only feeds the *implicit* console; with an explicit
  `krun_add_virtio_console_default(ctx, in, out, err)` and a **non-tty** output fd,
  guest hvc0 output goes to the VMM **log** (`output_to_log_as_err`) and the fd is
  wired to a *separate* virtio port named `krun-stdout` instead. Guests must write
  to `/sys/class/virtio-ports/*/name == "krun-stdout"` → `/dev/vportXpY`.
  vessel-init prefers that port over hvc0. Fork TODO: add an fd-direct console
  output mode so hvc0 lands in our file.
- libkrun's macOS epoll shim panics if the same fd is registered twice — dup()
  fds when passing one file to multiple console slots.
- aarch64 guest layout needs ≥ ~1 GiB RAM for the initramfs window
  (`InvalidGuestAddress(0x80000000)` below that).
- Stock Alpine kernels panic at early boot under libkrun before any console
  exists (see issues.md) — blocked on the Phase 1 custom kernel (VIRTIO_MMIO=y).
- Building the fork on macOS: `scripts/build-libkrun.sh` (zig as CC_LINUX for
  init.c, brew llvm libclang for bindgen via ~/lib symlink — DYLD env is stripped
  by SIP across /usr/bin/make).

## Gotchas hit

- Piping both stdin and stdout of `gzip` deadlocks on >64KB payloads (pipe buffer);
  decompress via temp file instead.
- Alpine `CONFIG_VIRTIO_MMIO=m` (both virt and lts kernels): any virtio device
  under libkrun needs the module first; we bundle+insmod it in the spike
  initramfs. Our Phase 1 kernel builds it `=y`.

## Phase 3 perf snapshot (M-series, 2026-06-09)

- virtiofs (home share) sequential write: ~1.3 GB/s (buffered)
- virtio-blk (data disk) direct write: ~276 MB/s
- virtiofs small-file churn: 1000 creates in 0.30s (~3.3k/s)
- Vessel cold boot to healthy agent: ~620ms; spike VMs: vz ~330ms, krun ~230ms

## Phase 4: VZ balloon characterization (macOS 26)

- `VZVirtioTraditionalMemoryBalloonDevice` works as hoped: with the guest
  idle and the balloon holding ~19 GiB of a 32 GiB VM, the VM's
  host-visible phys_footprint sat at ~1.1 GiB.
- Guest memory is charged to a per-VM `com.apple.Virtualization.
  VirtualMachine` XPC process, NOT the process that created the VM —
  footprint must be measured there (proc_listallpids + proc_name match).
- Guest /proc/meminfo keeps MemTotal at the configured max; balloon pages
  vanish from MemAvailable, so workload use = total - available - balloon.
