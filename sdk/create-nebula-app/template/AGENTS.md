# {{APP_NAME}} — agent notes

You are extending a **Nebula app**: a local-first application embedding the
Nebula engine ({{FLAVOR}} flavor) — a real Linux microVM giving this app
containers, kubernetes, and isolated forkable microVMs ("vessels"), all on
the user's machine, no Docker Desktop, no cloud.

## Project shape

```
nebula.config.json   engine settings (flavor, private ports, RAM ceiling)
scripts/engine.mjs   engine lifecycle: up / down / status (+ first-run fetch)
src/nebula.mjs       zero-dep client for the engine's HTTP API
src/index.mjs        the app (currently a demo — replace it)
components/          drop-in feature implementations (see components/README.md)
.nebula/             the embedded engine's home (gitignored, disposable)
```

The engine is isolated: its VM, disks and ports live under `.nebula/` and
`nebula.config.json`. Deleting `.nebula/` factory-resets the app's engine.

## The API you build on

Everything is plain HTTP on `http://127.0.0.1:<apiPort>` (see
`nebula.config.json`; bearer auth via NEBULA_API_TOKEN when set):

- `/v1alpha1/exec` — run commands in the engine VM
- `/v1alpha1/vessels…` — create/start/stop/exec/snapshot/restore/branch
  isolated microVMs. **branch** is the superpower: fork a RUNNING machine
  (RAM, processes, sockets) into N live clones in ~1s. Use it for
  agent sandboxes, tree search, checkpointed long jobs.
- `/docker/…` — the engine's Docker API, verbatim (run/build containers)
- `/k8s/…` — kubernetes apiserver (slim) or `/v1alpha1/kubeconfig` (full)

Full reference: `docs/httpapi.md` in https://github.com/Flux159/nebula.
`src/nebula.mjs` wraps the common calls; extend it freely (it is a vendored
subset of `@nebula-vm/sdk`).

## Rules of the road

- Keep all engine state under `.nebula/`; never touch `~/.nebula` (that is
  the user's standalone Nebula, a different instance).
- Untrusted/generated code runs in a vessel or container, never on the host.
- Vessels are cheap (~0.5-6s create, ~1s branch). Prefer
  snapshot+branch+discard over reuse-and-cleanup.
- The engine survives app restarts; `engine:down` only when the user quits.

## Adding a component

Components are reference implementations of common features (model
connectors, secret vaults, terminals). To add one: read
`components/README.md`, copy the component folder in, follow its
`COMPONENT.md` wiring steps. Components are written to be read and adapted
by you, not installed as black-box dependencies.
