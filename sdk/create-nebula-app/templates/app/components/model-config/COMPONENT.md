# model-config

Connect your app to AI models — cloud keys (OpenRouter/Anthropic/OpenAI/
Google) and/or a **local OpenAI-compatible server** (llama.cpp, LM Studio,
Ollama) — with a settings API, a settings UI, and a deterministic **mock
model server** for testing without keys or token spend.

Extracted from **Galaxy** (the agent-orchestrator nebula app), where this
exact layer runs agents on OpenRouter by default and on llama.cpp/a mock in
tests. Everything here is read-copy-adapt (components/README.md contract).

## What you get

```
src/settings.rs    SQLite-backed store: secret keys (never echoed back;
                   last-4 hint) + plain settings, with a connections list
                   describing what each key unlocks
src/routes.rs      hyper 1.x handlers: GET /api/settings (bulk: connections
                   with set/hint/unlocks + plain settings), PATCH
                   /api/settings — both templates serve plain HTTP from
                   Rust (src-tauri/src/server.rs in the app template), so
                   these wire straight in; hyper is the component standard
src/mock_model.rs  `mock-model` subcommand: OpenAI-compatible
                   /v1/chat/completions (stream + non-stream); the reply is
                   deterministic, and `[[reply:XYZ]]` anywhere in the last
                   user message scripts the exact output — your e2e tests
                   drive exact agent behavior with zero tokens
ui/ModelConfig.tsx React settings page section: connection rows (status
                   pill, what-it-unlocks, save) + model-provider picker
                   (cloud vs local endpoint) — Tailwind, dark theme. Talks
                   fetch to /api/settings (pass apiBase
                   `http://127.0.0.1:<appPort>` from the vite dev server);
                   a Transport prop overrides for tests
```

## Hard-won conventions (bake these in, they all cost a debugging session)

1. **Never echo secrets.** GET returns `set: bool` + a last-4 `hint`, never
   the value. Secrets live in SQLite locally (move to the OS keychain when
   you package; note it in your README until then).
2. **Per-feature unlocks, not global errors.** Every key row says exactly
   which feature it unlocks; every key-gated feature degrades with a
   message naming the key — never a stack trace.
3. **Local base URL = server ROOT, no `/v1`.** Clients (luminal included)
   append `/v1/chat/completions` themselves. llama.cpp and LM Studio both
   serve that path from the root.
4. **Containers can't see `localhost`.** If your app runs workloads in the
   engine's containers and they need to reach a model server on the host:
   plain linux docker → `host.docker.internal` (add the
   `host.docker.internal:host-gateway` extra-host), nebula engine on macOS
   → the vz NAT gateway `192.168.64.1` (host-gateway resolves to the
   engine VM, not the mac — measured). Probe both, in that order, like
   Galaxy's e2e does.
5. **Provider allowlists bite image models.** If you use OpenRouter image
   models (e.g. `openai/gpt-5.4-image-2` via chat/completions with
   `"modalities": ["image","text"]`), accounts with an allowed-providers
   list need `google-ai-studio`/`openai` enabled — surface the API error's
   `metadata.available_providers` to the user.

## Wiring steps (for a coding agent)

**App template (src-tauri/src/server.rs):**

1. Copy `src/settings.rs` + `src/routes.rs` into
   `src-tauri/src/model_config/` (add a `mod.rs` declaring both). The
   scaffold already has rusqlite + serde_json + hyper.
2. Edit the `KEYS`/`PLAIN` tables in `settings.rs` for your app. The
   scaffold's `db.rs` already gives the config-file overlay
   (`~/.<app>/config` beats the DB) — `settings.rs::get` here is the plain
   DB accessor; route reads through your `Db::get_setting` if you want the
   overlay (recommended; one-line swap in routes.rs).
3. In server.rs `route()`, add two match arms before the per-key settings
   routes (the component responses use `Full<Bytes>`; the scaffold uses
   BoxBody — map with `.map(|b| b.map_err(|n| match n {}).boxed())`):
   ```rust
   (Method::GET, "/api/settings") => { /* call model_config::routes::get_settings */ }
   (Method::PATCH, "/api/settings") => { /* …patch_settings(conn, &body) */ }
   ```
4. Drop `ui/ModelConfig.tsx` into `src/`; render with
   `<ModelConfig apiBase={`http://127.0.0.1:${appPort}`} />`.
5. Consume via `settings::model_invocation(&conn)` wherever you launch
   model work.

**Headless / other HTTP apps:**

1. Copy `src/*.rs` into your Rust crate; add deps:
   `hyper = { version = "1", features = ["full"] }`, `http-body-util`,
   `hyper-util = { version = "0.1", features = ["tokio"] }` (server I/O adapter), `rusqlite` (bundled), `serde_json`, `tokio`.
2. Edit the `KEYS` table at the top of `settings.rs`: one row per secret
   your app uses — `(ENV_VAR_NAME, "what it unlocks")`. Edit `PLAIN` for
   non-secret settings. The model-provider trio
   (`model_provider`, `local_model_base_url`, `local_model_name`) is
   already there.
3. Mount the routes: in your hyper service fn, before your other routes:
   ```rust
   if let Some(resp) = model_config::routes::handle(&req, &body, &db).await {
       return Ok(resp);
   }
   ```
   (axum host instead? wrap each handler in an axum route — they're plain
   `(method, path, json) -> json` functions.)
4. Consume the config wherever you launch model work:
   `settings::model_invocation(&conn)` returns the resolved endpoint —
   either `Cloud { env: Vec<(key, value)> }` (inject the env vars) or
   `Local { base_url, model }` (for luminal:
   `--base-url <url> --api openai --auth none --model <model>`).
5. Add the `mock-model` subcommand to your binary's arg dispatch
   (`mock_model::serve(port)`), and point your e2e at it:
   PATCH `{"model_provider":"local","local_model_base_url":"http://<host>:9123","local_model_name":"mock"}`.
6. Drop `ui/ModelConfig.tsx` into your settings page; it only needs a
   `fetch`-style helper and Tailwind.

## Testing pattern (what Galaxy's CI does)

Spin up `mock-model` → configure provider=local pointing at it → run your
real workload (real containers, real agent binary) → assert on
`[[reply:…]]`-scripted outputs. Same script runs locally against the
nebula engine and in CI against plain dockerd — zero keys either way.
