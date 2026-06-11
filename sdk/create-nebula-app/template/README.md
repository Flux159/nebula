# {{APP_NAME}}

A local-first app on the [Nebula](https://github.com/Flux159/nebula) engine
({{FLAVOR}} flavor): containers, kubernetes and forkable microVMs on the
user's own machine — no Docker Desktop, no cloud.

```sh
node scripts/engine.mjs up   # first run fetches engine artifacts (~40MB slim)
node src/index.mjs           # starter demo: containers + a live VM fork
```

Building with a coding agent? Hand it `AGENTS.md`.

The embedded engine lives entirely under `.nebula/` with its own ports
(see `nebula.config.json`) — isolated from any standalone Nebula install.
