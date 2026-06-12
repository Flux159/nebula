# {{APP_NAME}} — agent notes

You are extending a **Nebula app**: a Tauri 2 desktop app embedding the
Nebula engine ({{FLAVOR}} flavor) — a real Linux microVM giving this app
containers, kubernetes, and isolated forkable microVMs ("vessels"), all on
the user's machine, no Docker Desktop, no cloud.

## Project shape

```
nebula.config.json    engine settings (flavor, private ports, RAM ceiling) —
                      single source of truth: read by engine.mjs, the
                      frontend, AND compiled into the Rust layer
scripts/engine.mjs    engine lifecycle: up / down / status (+ first-run fetch)
index.html, src/      Vite + React frontend
  src/nebula.ts       typed client; reads hit the engine API directly,
                      privileged actions go through Tauri commands
src-tauri/            the Rust side (Tauri 2)
  src/nebula.rs       hyper client for the engine API — the BASE LAYER that
                      components and your features extend
  src/lib.rs          Tauri commands (see fork_demo for the pattern)
components/           drop-in feature implementations (components/README.md)
.nebula/              the embedded engine's home (gitignored, disposable)
```

`npm run dev` = engine up → vite dev server → Tauri window.

## The API you build on

Plain HTTP on `http://127.0.0.1:<apiPort>` (bearer auth via NEBULA_API_TOKEN
when set). Full reference: `docs/httpapi.md` in https://github.com/Flux159/nebula.

- `/v1alpha1/exec` — run commands in the engine VM
- `/v1alpha1/vessels…` — isolated microVMs: create/exec/snapshot/restore/
  **branch** (fork a RUNNING machine — RAM, processes, sockets — into N live
  clones in ~1s; use it for agent sandboxes, tree search, checkpoints)
- `/docker/…` — the engine's Docker API, verbatim
- `/k8s/…` — kubernetes apiserver (slim) or `/v1alpha1/kubeconfig` (full)

## Where code goes

- **Frontend reads** (status, lists, logs): `src/nebula.ts` → fetch directly.
- **Privileged/multi-step/secret-touching work**: a Tauri command in
  `src-tauri/src/lib.rs` built on `nebula::Nebula` (hyper). `fork_demo`
  is the worked example of the whole pattern.
- **Components** (model connectors, vaults, terminals): read
  `components/README.md`, copy the component in, follow its COMPONENT.md —
  they extend exactly the `nebula.rs` + Tauri-command seam.

## Rules of the road

- Keep all engine state under `.nebula/`; never touch `~/.nebula` (that is
  the user's standalone Nebula, a different instance).
- Untrusted/generated code runs in a vessel or container, never on the host.
- Vessels are cheap (~0.5-6s create, ~1s branch). Prefer
  snapshot+branch+discard over reuse-and-cleanup.
- The engine survives app restarts; stop it only when the user quits
  (wire `engine.mjs down` / a Rust shutdown hook into Tauri's exit event).

## Known v0 gaps (fine to fix)

- Packaging: `tauri build` bundles the webview app but does NOT yet bundle
  the nebula binaries + guest images as resources — dev mode fetches them
  via scripts/engine.mjs. See "externalBin" in the nebula repo's own
  ui/src-tauri/tauri.conf.json for the working sidecar pattern.
- bundle.icon is empty (add icons before shipping).
