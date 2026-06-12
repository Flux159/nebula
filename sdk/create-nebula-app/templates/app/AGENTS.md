# {{APP_NAME}} — agent notes

You are extending a **Nebula app**: a Tauri 2 desktop app embedding the
Nebula engine ({{FLAVOR}} flavor) — a real Linux microVM giving this app
containers, kubernetes, and isolated forkable microVMs ("vessels"), all on
the user's machine, no Docker Desktop, no cloud.

## Project shape

```
nebula.config.json    settings (flavor, ports, RAM ceiling) — single source
                      of truth: engine.mjs, the frontend AND the Rust binary
scripts/engine.mjs    engine lifecycle: up / down / status (+ first-run fetch)
index.html, src/      Vite + React frontend (plain fetch — runs in the Tauri
                      webview OR any browser; no invoke() coupling)
src-tauri/            the Rust side (Tauri 2)
  src/server.rs       THE APP'S OWN API — a hyper server on appPort. Your
                      features and components are routes here.
  src/db.rs           sqlite (rusqlite, bundled) — settings + your tables
  src/nebula.rs       hyper client to the Nebula engine API
  src/lib.rs          wires it: spawn server, open the window
components/           drop-in feature implementations (components/README.md)
data/                 app.db (persists across engine resets; gitignored)
.nebula/              the embedded engine's home (gitignored, disposable)
```

`npm run dev` = engine up → vite dev server → Tauri window. The flow is
React → app server (`/api/...`, port `appPort`) → engine (`apiPort`).

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

- **Frontend reads** (status, lists, logs): fetch the engine API directly
  (`src/nebula.ts` → `nebula.*`).
- **Your features** (privileged, multi-step, secret-touching, persistent):
  a route in `src-tauri/src/server.rs` built on `nebula::Nebula` (engine
  calls) and `db::Db` (sqlite). `POST /api/fork-demo` and the
  `/api/settings/<key>` pair are the worked examples of the whole pattern.
- **Components** (model connectors, vaults, terminals): read
  `components/README.md`, copy the component in, follow its COMPONENT.md —
  they add server routes + db tables + UI panels along the same seam
  (model-config stores its API keys via `db.rs`).

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
