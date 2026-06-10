# Nebula — Open Issues & Decisions To Discuss

Running log of problems, surprises, and deferred decisions hit during implementation.
Newest at the top within each section. (Items here need Suyog's input or are accepted
limitations; routine TODOs live in code.)

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
