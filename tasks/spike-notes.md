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
