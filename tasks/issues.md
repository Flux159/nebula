# Nebula — Open Issues & Decisions To Discuss

Running log of problems, surprises, and deferred decisions hit during implementation.
Newest at the top within each section. (Items here need Suyog's input or are accepted
limitations; routine TODOs live in code.)

## Open (being worked / next phase)

- **(2026-06-09, Phase 0) Stock Alpine kernel panics at early boot under libkrun
  (fork 1.18.0), boots fine under VZ.** Guest dies before any console exists
  (libkrun has no earlycon/pl011, virtio-MMIO console needs `virtio_mmio` which
  Alpine builds `=m`, and the panic precedes module load), so the panic message is
  unobtainable with stock kernels. Verified initrd/FDT plumbing in the fork looks
  correct (`linux,initrd-start/end` written, memory regions sized). Resolution path:
  the Phase 1 **custom kernel** (VIRTIO_MMIO=y, VIRTIO_CONSOLE=y) gives us console
  output during boot and is what the product needs anyway. Only gates the krun
  sidecar engine (Phase 7), not the VZ Vessel (Phases 1–6). VZ spike passes
  end-to-end (357ms boot→poweroff).

## Needs discussion

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
