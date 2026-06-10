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

## Needs discussion

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
