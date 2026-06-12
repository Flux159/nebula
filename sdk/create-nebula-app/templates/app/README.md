# {{APP_NAME}}

A local-first desktop app on the [Nebula](https://github.com/Flux159/nebula)
engine ({{FLAVOR}} flavor): containers, kubernetes and forkable microVMs on
the user's own machine — no Docker Desktop, no cloud.

Stack: **Tauri 2** shell · **Vite + React** frontend · **hyper** Rust base
layer to the engine's HTTP API.

```sh
npm install
npm run dev     # boots the isolated engine, then vite + the Tauri window
```

Note: a `cargo build` debug binary run directly shows a white window —
debug builds load the vite dev server (`devUrl`), which `npm run dev`
starts for you. Release builds (`npm run tauri build`) embed `dist/`.

First run fetches engine artifacts (~40MB slim). The embedded engine lives
entirely under `.nebula/` with its own ports (see `nebula.config.json`) —
isolated from any standalone Nebula install.

Building with a coding agent? Hand it `AGENTS.md`.
