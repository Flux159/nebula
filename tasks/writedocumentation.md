# Nebula — "Build Your Own Agent Orchestrator" documentation (plan)

Status: **planned**, not yet written.

> Progress 2026-06-11: deliverable #1 exists in v0 — `sdk/create-nebula-app`
> scaffolds a dependency-free app (slim engine by default, `--full` opt-out)
> with engine bootstrap, a vendored API client, AGENTS.md for coding agents,
> and the components/ contract stub. Verified end-to-end against CI-built
> slim artifacts (engine up 460ms; live VM fork demo green). The first
> component (model-config: llama.cpp + API keys, from the galaxy app) slots
> into components/ per the v0 contract in the template. This is the spec for a guide we publish
**after** Nebula is open-sourced and Luminal ships. It is not the guide itself;
it captures the goal, the deliverables, the outline, and open questions so the
guide can be written (by us or by an agent) without re-deriving the vision.

## Why this exists — the opportunity

Nebula is a substrate, not just a product. The same primitives that make it a
good container/k8s manager — strong per-workload isolation, millisecond microVMs,
live snapshot + branch, an embeddable engine, local-first networking — are
*exactly* what an **agent orchestrator** needs: somewhere safe to run code an LLM
just wrote, a way to fan out and explore many futures, and a way to do it on the
user's own machine without shipping their data or keys to a third party.

We are going to build one such orchestrator. But there is no reason to be the
only one. If we make it genuinely easy to stand up a *custom* orchestrator on top
of Nebula + Luminal for a specific vertical (marketing, security, data, support,
research, …), we unlock an ecosystem of "Nebula apps" the way Docker unlocked an
ecosystem of images. The deliverable below is the on-ramp for that.

Outcomes we want:
- A motivated developer can go from zero to a working, isolated, local-first
  agent app for their niche in an afternoon.
- They can hand a single markdown file to a coding agent and have it scaffold the
  app for them.
- Enough of these exist that we can credibly run / cosponsor hackathons around
  "Nebula apps" and point to real examples.

Luminal = our minimal, Rust, small-binary agent runtime (the default agent we
ship). The guide should work with Luminal *or* a user's own agent loop; Luminal
is the easy path, not a requirement.

## The substrate — what a builder gets from Nebula (and what's still TODO)

The guide must lead with *why Nebula*, grounded in real capabilities (link to the
README + SDKs, don't overclaim):

- **Safe execution.** `nebula sandbox run` boots an isolated microVM, runs, and
  tears down in ~250ms (libkrun fork; `--gpu` for virtio-gpu). Agent-generated
  code / shell / tool-calls run in a VM, not on the host — the security boundary
  is the hypervisor, so "the agent rm -rf'd something" stays inside a disposable
  VM.
- **Fan-out & tree-search.** `nebula vessels snapshot` captures live machine
  state (RAM + processes), and `vessels branch --snapshot X --count N` wakes N
  independent clones mid-execution (~600ms/branch). This is the primitive for
  running N agent attempts from a common checkpoint and keeping the best — see
  the MCTS-over-microVMs work in `tasks/microvm-k8s-brief.md` / `spike-notes.md`.
- **Embeddable.** [Nebula-slim](../slim/README.md) is a ~32 MB container + k8s +
  helm engine with no Go runtime; the host CLIs are pure Rust on macOS/Linux/
  Windows (no WSL2). An orchestrator app can bundle the engine instead of asking
  users to install Docker.
- **Programmable.** REST API (`127.0.0.1:7440`, v1alpha1) with TypeScript
  (`sdk/typescript`) and Python (`sdk/python`) clients — the orchestrator drives
  containers/VMs over this, not by shelling out.
- **A clean SDK to build a UI *on* (not the Nebula UI to fork).** The Tauri app
  in `ui/` is a developer-facing **VM / container manager** — it deliberately
  exposes vessels, images, balloons, snapshots. That is the *wrong* mental model
  for an orchestrator and a **bad starting point** (see "The end user is not a
  developer" below). What a builder reuses is the *daemon + SDK underneath* it,
  plus the purpose-built example UIs and `create-nebula-app` template we ship —
  not the manager chrome.
- **Local-first AI.** virtiofs + host-faithful DNS mean an agent in a VM can
  reach a model server running on the host (LM Studio / llama.cpp / Ollama) or a
  model container in the Vessel. Keys and prompts never have to leave the machine.
- **Cross-platform & daemon-first.** Same app on three OSes; the engine keeps
  running if the UI is closed.

TODO for the guide author: confirm each of the above maps to a *documented,
copy-pasteable* command/SDK call at write time. Anything that's still a CLI-only
or unstable surface should be called out as such, not presented as a stable API.

## Design principle: the end user is not a developer

The single most important framing for the guide. The person using an orchestrator
app built on Nebula is **not** the person who built it, and is often **not
technical at all** — a marketer, a recruiter, an analyst, a small-business owner.
They want their *workflow* done; they should never see a VM, a container, an
image tag, a balloon, or a snapshot tree. Those are our internals.

Consequences the guide must enforce:
- **Don't fork the Nebula UI.** It is a manager for *developers* who want to see
  and poke the infrastructure. An orchestrator app should hide all of that. We
  call this out explicitly so nobody starts from `ui/` and inherits the wrong
  abstractions.
- **The UI speaks the user's domain, not ours.** "Draft the September campaign,"
  not "spawn 3 containers." Status is "2 posts awaiting your approval," not
  "vessel `mkt-3` Running." Nebula is plumbing the user never names.
- **Zero-setup.** No "install Docker," no daemon flags. The app bundles
  Nebula-slim and just works on first launch (this is why embeddability matters).
- **Safe by default, invisibly.** Isolation/sandboxing happens because we wired it
  that way, not because the user understands microVMs.

This is the line between a *developer tool* (Nebula itself) and the *apps people
build on it* (consumer-grade workflow software). The whole point of the guide is
to get builders across that line.

(Caveat: this applies to apps *with* a non-technical end user. A whole class of
Nebula apps are headless server/cron automations whose operator is technical and
*wants* to see the infrastructure — see "App shapes" below.)

## Design principle: design your app agentic-CLI-first

The companion principle. The end user gets a no-jargon GUI (above); the **agents**
get a **CLI**. Every orchestrator app should expose its own command-line surface
for the actions you can take in the app — and, crucially, let agents **update app
state** through it. This is exactly how we drive the orchestrator today (and how
Galaxy's CLI will work), generalized so *any* Nebula app gets it.

Why it matters:
- It's the cleanest, most testable contract between the agent (in its sandbox) and
  the app: a typed CLI the agent calls, instead of bespoke RPC per feature.
- The same CLI the agent uses is the one the human power-user / your own scripts /
  the test harness use — one surface, three consumers.
- It maps directly onto the "agentic CLI for your app" component and the agentic-CLI
  reference doc. Build the app's verbs as a CLI first, render the GUI on top.

A corollary that recurs everywhere: an app's agentic CLI (and the tools/MCP
servers it calls) usually has to live **inside the agent's docker image** — so
"customizing the agent image" is the foundational component most things rest on.

## App shapes: GUI, but also headless CLI / daemon / cron

Not every Nebula app has a GUI — or an end user at all. A whole class are
**headless**: a Nebula app is *just a CLI / script* you run on a server, that sets
itself up as a **daemon or cron job** and then runs agents unattended on whatever
keys and config you've handed it. No UI, no human in the loop — a fleet of
scheduled or event-driven agents doing work.

This is the natural extension of the agentic-CLI-first principle: the same CLI that
*agents* use to control the app is the CLI *you* (or cron, or a webhook) use to
**run and schedule the whole app**. One surface, now serving the operator too.

- **The two-developer framing still holds, just shifted.** GUI apps: end user is
  non-technical, agents get a CLI. Headless apps: the "user" *is* the technical
  operator, and the CLI/config *is* the product — there's simply no GUI layer to
  build. The "hide the VMs" rule relaxes here; the operator wants to see them.
- **Deployment shape.** Spin up an environment → run the Nebula CLI app → it
  daemonizes / installs its schedule → agents run on the provided keys. The
  important caveat: on a cloud server you typically need a **VM, not just a
  container**, because **nested container/KVM virt isn't available by default on
  GCP** (and several other clouds) — Nebula needs the hypervisor, so it runs on a
  nested-virt-enabled VM or bare metal, or you use the slim engine where the
  workload allows. The guide must be honest about this rather than implying
  "docker run nebula" works anywhere.
- **Why it matters.** It widens the audience from "people shipping desktop apps to
  non-technical users" to "anyone who wants a fleet of scheduled/triggered agents
  on a box" — and the **component-maintainer app** above (the one that keeps
  `/nebula-components` green) is itself exactly this shape: headless, scheduled, no
  UI. We dogfood the headless path immediately.

Implications for the deliverables: `create-nebula-app` should offer a **headless /
CLI-daemon template** (not just GUI templates), the build-kits need a
"no-UI, runs-on-a-server, schedules-itself" path, and the packaging step must cover
**server deployment + the nested-virt caveat**, not only desktop bundling.

## Testing — the harness builders (and their coding agent) need

This surfaces immediately the moment you build a real orchestrator, and it is
**critical**, not a nice-to-have. Two reasons it's load-bearing:

1. Orchestrator apps are the *worst case* for testing — **nondeterministic** (LLM
   output varies run to run), **stateful and slow** (VMs/containers/snapshots),
   **async + multi-step** (fan-out, hand-offs, queues), with a human-in-the-loop
   UI on top. Naive "assert exact string" tests are useless here.
2. The build-kit's premise is that a **coding agent** scaffolds and *verifies* the
   app. That only works if the agent can run a suite **headlessly**, **fast**, and
   get an unambiguous **green/red** — no GUI clicking, no "looks right to me." If
   the agent can't self-verify, it ships broken apps confidently. So the test
   harness isn't just for the human builder; it's how the *agent* knows it's done.

What the template ships (and the build-kit instructs the agent to use):

- **A deterministic model backend.** A record-replay ("cassette") layer over the
  model: capture real responses once, replay them byte-for-byte in CI so logic
  tests don't depend on a live LLM. Plus a scripted/stub model for pure unit
  tests, and seed/`temperature=0` for the few real end-to-end runs. **This is the
  single most important piece** — without it nothing else is repeatable.
- **Ephemeral, isolated Nebula fixtures.** Spin up an isolated engine
  (`NEBULA_HOME`-scoped), run, tear down — reusing the pattern the
  [nebula-slim test harness](../slim/test) already proves (isolated home,
  registry mirror to dodge Docker Hub rate limits, per-test staging). Tests never
  touch the user's real engine.
- **Snapshot-based world reset.** Branch each test from a known-good snapshot for
  a clean, *fast*, reproducible starting state. This is a genuine Nebula edge: the
  whole world (disk + RAM + running processes) resets cheaply, which most agent
  frameworks simply cannot do — worth calling out in the guide as a reason to
  build agent systems here.
- **Contract / structural assertions, not exact-match.** Assert the *shape* and
  *side effects* of agent output (a file was written, a ticket got tagged, N
  drafts exist, the JSON validates), not the exact prose. Offer optional
  LLM-as-judge for genuinely fuzzy checks — clearly flagged as itself
  nondeterministic and not for gating CI.
- **Headless end-to-end.** Drive a full workflow through the orchestration layer
  directly (no GUI). Optional UI smoke via Tauri's webdriver / Playwright, but the
  agent's self-verification leans on the headless path.
- **`npm test` / `cargo test` green out of the box** — the template includes one
  real end-to-end example test so a freshly scaffolded app is provably working
  before the builder changes a line.

The pyramid the guide teaches:
- **Unit** — orchestration logic with a stubbed model + stubbed Nebula client.
  Milliseconds, fully deterministic.
- **Integration** — real ephemeral engine + replayed model: proves the
  VM/sandbox/snapshot/log-streaming wiring actually works.
- **E2E** — real engine + seeded/replayed model: one full workflow, the smoke the
  agent runs before declaring done.

The build-kit's **Definition of done** must require the agent to run the suite
(and the E2E smoke) and report machine-readable pass/fail — handing back a red
suite is a failure, not a "mostly works."

Open design point: how much of this harness lives in `create-nebula-app`'s
template vs a shared `@nebula/testing` (or crate) the template depends on. Lean
toward a shared package so the cassette/fixture/snapshot-reset machinery is
maintained once and every app inherits fixes.

## Deliverables

The artifacts we produce, in rough order of leverage:

1. **`create-nebula-app`** — the on-ramp. `npx create-nebula-app` (and/or
   `cargo create-nebula-app`) scaffolds a working orchestrator app from a
   template that runs out of the box: a Tauri shell wired to the Nebula SDK, the
   sandbox/isolation layer pre-wired, a local-model connector, and a *consumer*
   starter UI (not the manager UI). Pick a template (`--template grid`,
   `--template kanban`, `--template chat`, and a **`--template headless`** CLI-
   daemon/cron shape with no UI), `npm run dev` (or run the CLI), and you have a
   running app to customize. This is the "it just works in 60 seconds" moment that
   gets people in the door — treat it as the headline deliverable.

2. **Example apps in-repo** (`examples/`) — a handful of small, *good* starting
   UIs that demonstrate the consumer-grade framing: each hides VMs/containers
   entirely and speaks a domain. These double as the `create-nebula-app`
   templates and as reference reading. (Candidates in "Examples of apps you can
   create" below.)

2b. **A test harness / `@nebula/testing` package** — the cassette (record-replay
   model), ephemeral-engine fixtures, and snapshot-reset machinery from the
   "Testing" section, shipped so every scaffolded app gets `npm test`/`cargo test`
   green out of the box and the coding agent can self-verify. Maintained once,
   inherited by all apps. Arguably co-headline with `create-nebula-app` —
   scaffolding without verifiable tests is half a product.

3. **The copy-paste agent build-kits** — markdown a user pastes into a coding
   agent (Claude Code, Cursor, …) to have it build/customize the app. **Two
   files**, because the two starting points need different instructions:
   - `build-from-scratch.md` — for someone with an existing/opinionated codebase
     or who wants no template: the full capability cheat-sheet + scaffold steps
     from zero.
   - `customize-create-nebula-app.md` — for the (recommended) path: "you already
     ran `create-nebula-app`; here is the project shape and how to extend it for
     vertical X" — much shorter, because the template did the plumbing.
   Both share one versioned capability cheat-sheet (generated from the SDK so it
   can't drift).

4. **Nebula components (cookbooks).** A library of composable reference
   implementations for the common-but-non-trivial features *every* orchestrator
   ends up wanting (secrets vault, agent terminal/chat in the UI, OAuth, local
   model runner, …). Each is a copy-in recipe with a README/skill an agent can
   read and wire up in minutes — *not* baked into `create-nebula-app` because the
   integration is too app-specific to template, but common enough that nobody
   should reinvent it. This is its own big section below — and plausibly the
   network-effect artifact (the "Docker Hub") we discussed.

5. **The how-to guide / blog post** — narrative, skimmable, screenshots, links to
   a finished example. "Build a local, isolated workflow app on Nebula in an
   afternoon." The human-readable front door that points at 1–4.

All live in-repo (`examples/`, `components/`, `docs/orchestrators/`) and mirror to
the website/blog. Polished example apps can also get their own public repos to
point hackathon participants at.

## Nebula components (cookbooks)

The shower-thought that ties the whole ecosystem together. Every orchestrator app
re-needs the same handful of capabilities, and each is *common but not trivial* —
too app-specific to bake into the scaffold, too fiddly to expect every builder to
get right from scratch. So we ship them as **cookbook components**: self-contained
reference implementations a developer (or their coding agent) reads and grafts in,
getting a working integration in **2–3 minutes** instead of re-deriving it.

**What a component is**
- A directory under `components/<name>/` with: a **README.md / agent skill** that
  states what it does, what it *spans* (API route(s) · UI piece(s) · sqlite
  tables · changes to the in-Nebula agent image), **copy-in steps**, **how to
  verify it works in the UI** (and the tests to run via `@nebula/testing`), and
  its **dependencies on other components**.
- Designed for an agent to consume: terse, imperative, self-contained, with the
  exact files to copy and the exact wiring edits to make. The README *is* the
  interface.
- Honest scope: many of these aren't Nebula-specific (OAuth, an xterm panel, a
  chat schema). We ship them anyway as the *blessed way to do it on Nebula* so
  devs/agents don't reinvent — the Nebula-specific value is the integration with
  the agent environment, the in-VM image, isolation, and the test harness.

**Three cross-cutting facts the section must hammer**
1. **Most components touch three layers at once:** an **API** route in the app, a
   **UI** piece, and often **sqlite** state — plus, frequently, a change to the
   **docker image running inside Nebula** (to add a CLI, an MCP server, a runtime,
   or model weights the agent needs). "Customizing the agent image" is therefore a
   *foundational* component most others depend on.
2. **Your app should be designed agentic-CLI-first** (its own component/principle,
   below): the cleanest way for agents to read and *mutate* app state is a CLI the
   app exposes and the agent calls — the orchestrator/Galaxy-cli pattern,
   abstracted for any Nebula app.
3. These exist to make **live demos and fast prototyping** real: "in 10 minutes,
   live on stage, I'll build a custom agent orchestrator on Nebula" is only
   credible if OAuth, a chat UI, secrets, and a local model are each a 2-minute
   graft, not an afternoon each.

### The starter catalog (from the shower thoughts + obvious neighbors)

**Foundational (nearly every app pulls these in)**
- **Customizing the agent image** — extend the docker image that runs inside
  Nebula with the agent's CLIs, tool binaries, MCP servers, runtimes, or weights.
  Prerequisite for most components below; the guide's first "real" customization.
- **Secrets & tokens vault** — securely store and **inject API keys / tokens into
  the agent environment** so the agent's CLIs, tool calls, and MCP servers can
  authenticate. API + UI to manage them; mounted (not baked) into the agent image.
  Nearly universal — almost every app needs this on day one.
- **Agentic CLI for your app** — a reference for giving your app a CLI surface the
  agents call to **read and update app state** (the orchestrator/Galaxy-cli
  pattern). Pairs with a "design your app agentic-CLI-first" reference doc.

**Talking to the agent (UI surfaces)**
- **Agent terminal in the UI** — stream the agent's TTY into the app (xterm.js ↔ a
  pty stream over the API ↔ the agent image). The "watch it work" view.
- **Chat interface** — chat with the agent instead of a terminal: API + UI + sqlite
  (threads/messages). The consumer-friendly default for most non-technical users.

**Auth & sync**
- **Google OAuth sign-in** — the standard consumer login.
- **Cloud sync** — connect an otherwise-local app to an online server to sync state
  (optional online backend; respects the local-first default).

**Local-first AI runtimes** (so end users need no keys and it runs offline)
- **Embedded llama.cpp + model manager** — run text models locally with a UI to
  browse/download/select models — *a mini LM Studio*. Notably **buildable from the
  chat component + a model/runtime-download component**, which makes it one of the
  best **first getting-started tutorials** an agent can follow end-to-end.
- **Embedded image generation** — stable-diffusion.cpp with download/run + a model
  picker (Klein, etc.); plus connectors to hosted image APIs (gpt-image-2,
  nano-banana-pro, …) for builders who prefer closed models. Same shape as the
  llama.cpp component.

**Novel surfaces**
- **Agentic game canvas** — a three.js / RPGMaker-style component an agent
  integrates into, so **agentic NPCs** can move around and act: the agent analyzes
  game state / screenshots and decides next actions. Makes "AI-driven game
  characters" a copy-in, not a research project. (Like all components: ships with a
  README/skill on efficient copy-over and UI verification.)

This list is the seed, not the ceiling — components are exactly what the community
contributes and hackathons produce.

### Where they live & who maintains them

- **First-party components live in the repo**, in a top-level **`/nebula-components`**
  folder. No separate registry to build: like the apps-platform catalog (which is
  just JSON in the repo), the component "catalog" is the folder itself. Keep it all
  in one place for simplicity.
- **Third-party components get their own repos** — e.g. `nebula-component-oauth` on
  GitHub, published to npm (or crates). Only third-party ones live outside; we don't
  fragment our own.
- **Maintenance burden is real — and it's a dogfooding opportunity.** Components
  rot as the SDK/image base moves. Rather than treat that as a tax, it's a job for
  **one of our own Nebula apps**: an agent orchestrator that watches the components,
  runs their tests (the `@nebula/testing` harness), and opens fixes/updates when
  something breaks. The thing that keeps the ecosystem alive is *built on* the
  ecosystem — a strong proof point in its own right.

## Guide outline (sections both artifacts cover)

Worked example throughout: **a marketing-agent app** (briefs in → a small team of
agents researches, drafts, critiques, and schedules content → human approves).

1. **What you're building & why on Nebula** — the substrate pitch above, the
   isolation/fan-out/local-first story, the finished-app screenshot.
2. **Prereqs & install** — install Nebula (full) or embed Nebula-slim; verify the
   daemon + SDK; `nebula up`.
3. **Scaffold the app** — `npx create-nebula-app my-app --template <x>`; what the
   template gives you (SDK wired, isolation layer, model connector, consumer UI
   shell); `npm run dev`. Explicitly: **do not start from the Nebula `ui/`** —
   that's the manager, not an app.
4. **Connect to Nebula** — open the SDK client to `127.0.0.1:7440`; list/create
   workloads; the difference between a long-lived Vessel container and an
   ephemeral `sandbox run`.
5. **Run an agent in isolation** — drop a Luminal container (or your own agent
   image) into a sandbox; pass it a task; stream logs/results back over the SDK;
   tear down. Emphasize: untrusted tool-calls run *here*.
6. **Customize the agent image** — extend the docker image running inside Nebula
   with the agent's CLIs, tool binaries, MCP servers, and runtimes (the
   "customizing the agent image" component). The foundational customization most
   later steps depend on.
7. **Wire up the model** — connect agents to a **local** model: LM Studio,
   llama.cpp, or Ollama on the host (via host DNS/virtiofs), a model container in
   the Vessel, or the **embedded llama.cpp component** for a fully self-contained
   app. Note the privacy property (nothing leaves the machine).
8. **Secrets & auth** — the *secrets vault* component: store keys/tokens and inject
   them into the agent environment (mounted, not baked) for CLIs/tool-calls/MCP;
   least-privilege per agent; how to *not* commit them. Add **Google OAuth** when
   the app needs user sign-in.
9. **Add the common features with components** — terminal-in-UI, chat interface,
   cloud sync — each a 2–3 minute graft from `components/` rather than a rebuild.
   This is the section that makes the "10-minute live build" credible.
10. **Give your app an agentic CLI** — expose your app's verbs as a CLI the agents
    call to read/update state (the agentic-CLI-first principle); build the GUI on
    top of it.
11. **Orchestrate many agents** — the patterns: a team of role-specialized agents
    (researcher/writer/critic), fan-out attempts via snapshot+branch and pick the
    best, a reconcile/queue loop. Keep it to primitives the SDK exposes.
12. **Build the custom UI** — the part that's *yours*: the workflow surface for the
    vertical (for marketing: a content calendar + approve/reject queue + per-agent
    transcript). Subscribe to agent/workload events and render them — in the
    *user's* language. Reiterate: no VM/container/snapshot vocabulary reaches the
    end user.
13. **Test it (and let your agent test it)** — the cassette model, ephemeral
    engine fixtures, snapshot-reset, structural assertions; the unit/integration/
    e2e pyramid; running `npm test` and the e2e smoke. How *you* and *your coding
    agent* know the app works. (See the "Testing" section.)
14. **Package & ship** — *desktop:* bundle Nebula-slim so users don't need Docker;
    sign/notarize (point at `.github/workflows/release.yml`); cross-platform notes.
    *Server/headless:* deploy as a CLI daemon/cron — including the **nested-virt
    caveat** (a VM, not a bare container, on GCP and similar; or slim-direct where
    the workload allows).
15. **Where to go next** — the example repo, the components catalog, the SDK
    reference, the community.

## The copy-paste agent build-kits (what the two files contain)

Both files share a **Preamble** ("You are building a custom, local-first,
sandboxed agent *app* on Nebula — the end user is not a developer, so never expose
VMs/containers"), an **Interview step** (vertical/use case, model backend —
LM Studio/llama.cpp/Ollama/cloud, what tools the agents may call, single-agent vs
team vs fan-out, **GUI app or headless CLI/cron**, which UI metaphor (if GUI),
**which components to pull in** — terminal/chat/OAuth/sync/local-model/image-gen/
game), the versioned **Capability
cheat-sheet** (the exact SDK calls / CLI for: start daemon, create sandbox, run
agent image, stream logs, snapshot+branch, mount secrets, reach the local model)
**plus a component index** (name → what it spans → README path, so the agent
grafts the right cookbook instead of re-deriving it), **Safety rails**
(agents in sandboxes; secrets mounted not baked; local models by default), and a
**Definition of done** that *requires running the test suite + e2e smoke and
reporting machine-readable pass/fail* — a red suite is a failure, not "mostly
works." (The cassette/fixtures make this runnable headlessly; see "Testing".)

They differ in the scaffold half:
- **`customize-create-nebula-app.md`** (recommended) — assumes the user ran
  `create-nebula-app`. Describes the template's project shape and gives extend-it
  instructions for vertical X. Short, because the plumbing already exists.
- **`build-from-scratch.md`** — no template: full scaffold from zero (init the
  Tauri app, add the SDK, build the isolation layer + consumer UI). For people
  with their own codebase or strong opinions.

Design goal: each file is *self-contained and current* — pasting it should not
require the agent to go read five other docs. One source of truth for the cheat-
sheet, generated from the SDK so it can't drift.

## Examples of apps you can create (end-user use cases)

Concrete, consumer-grade apps a builder could ship — framed around the *end
user's* job, not the infrastructure. Each leans on a Nebula property:
🔒 local/private · 🧰 safe isolated execution · 🌿 fan-out/parallel · 📦 embeddable/
zero-setup. These are the candidates for `examples/` templates and hackathon
inspiration; the user here is usually non-technical.

**Marketing & content**
- **Content studio** (running example) — briefs in; agents research, draft,
  critique, and schedule posts; human approves on a calendar. 🌿🧰
- **Newsletter / podcast producer** — turn notes or a feed into a drafted issue or
  episode outline + show notes. 🧰
- **SEO / product-catalog filler** — generate descriptions, alt-text, metadata for
  N products from a spreadsheet. 🌿
- **Social manager** — one agent per channel drafting/scheduling, one inbox to
  approve. 🌿

**Sales, support & ops**
- **Lead enrichment + outreach** — an agent per lead researches and drafts a
  personalized first touch; you approve a queue. 🌿
- **Support triage** — an agent per ticket proposes a reply + tags; humans gate. 🌿🧰
- **Recruiting assistant** — screen resumes against a role, draft outreach, prep
  interview notes. 🔒🌿 (candidate data stays local)

**Personal / professional knowledge work**
- **Deep-research assistant** — fan out browsing/reading agents on a question,
  synthesize a cited brief. 🌿🧰
- **"Talk to your documents/spreadsheet/DB"** — a non-technical analyst asks
  questions; agents run the queries/scripts in a sandbox and answer. 🔒🧰
- **Bookkeeping / finance helper** — categorize transactions, reconcile, draft
  monthly reports. 🔒 (financial data never leaves the machine)
- **Contract / legal review** — summarize, flag clauses, compare to a playbook. 🔒
  (privilege-sensitive; local-first is the selling point)
- **Clinical-note / intake drafting** — structured notes from a transcript. 🔒
  (PHI stays local — a category cloud tools struggle to serve)

**Maker / technical-but-not-the-end-user-of-the-app**
- **Code-upgrade / migration studio** — point at a repo; agents attempt the
  migration in isolated VMs, you pick the branch that passes tests. 🌿🧰
- **"Try this AI-generated app safely" sandbox** — paste any script/app an LLM
  wrote and run it disposably, nothing touches the host. 🧰📦
- **QA / test-writing studio** — generate and run tests across modules in
  parallel sandboxes. 🌿🧰
- **Local eval bench** — run a model/agent against a task suite in N VMs, see
  scored results. 🌿🧰

**AI-native / novel (showcase the components)**
- **Mini LM Studio** — browse/download/run local text models with a chat UI. Built
  almost entirely from the *chat* + *llama.cpp model-manager* components; the
  flagship "getting started in one tutorial" app. 🔒📦
- **Local image studio** — download/run stable-diffusion.cpp models (or call a
  hosted image API) with a gallery UI; from the *image-gen* component. 🔒🧰
- **Agentic game** — a three.js / RPGMaker app where NPCs are agents that perceive
  game state/screenshots and act; from the *agentic game canvas* component. 🧰🌿
  Turns "AI characters in my game" into a weekend project.

**Headless / server (no UI — CLI daemon or cron)**
- **Component-maintainer** — the dogfood app: watches `/nebula-components`, runs
  their tests, opens fixes when the SDK/image base moves. 🧰🌿
- **Scheduled research/digest** — a cron app that runs research agents nightly and
  emails/posts a brief. 🌿
- **Inbox / webhook responder** — agents triggered by inbound email, GitHub events,
  or queue messages; each firing an isolated run. 🧰🌿
- **Nightly data pipeline** — ETL/enrichment agents on a schedule, scoped creds. 🔒🌿

The thread: most of the business apps are *boring workflows* whose differentiator
on Nebula is **privacy (local model + local data)** and **safety (isolated
execution)** — what makes them hard to ship as pure cloud SaaS. The AI-native ones
show the **components flywheel**: each is mostly "assemble 2–3 components," which is
exactly the demo and prototyping speed we're selling.

## Example orchestrators / interfaces that need many Nebulas

(Brainstorm — seeds for guide variants, example apps, and hackathon tracks. Each
is "a front-end metaphor" × "a reason you need isolation and/or many VMs.")

**Workload patterns that demand many VMs:**
- **Tree-search / MCTS over a task.** Branch N microVMs from one snapshot, let
  each agent explore a different approach, score, keep the winner, re-branch from
  it. Coding, theorem-proving, RL self-play, "best-of-N" anything.
- **Role-specialized agent teams.** Researcher + writer + critic + tester, each
  in its own sandbox with its own tools/creds, passing artifacts between them.
- **One isolated agent per unit of work.** An agent per PR, per support ticket,
  per inbound lead, per spreadsheet row, per dataset — fan out, collect results.
- **Eval / benchmark harness.** Run one agent against N tasks in N parallel
  disposable VMs, collect transcripts and scores. Reproducible because each VM
  starts from the same snapshot.
- **Untrusted-code execution at scale.** Red-team/pentest agents running real
  exploits, malware triage, "run this random repo's tests" — all things you only
  do inside a throwaway VM.
- **Computer-use / browser agents.** Each agent gets its own browser (or whole
  desktop) in a VM; scrape, QA-test, or automate flows without 200 Chrome tabs
  fighting on the host.
- **Long-running autonomous agents with checkpoints.** Snapshot the agent's live
  state, resume after a crash or to roll back a bad decision; multiple such agents
  per user.
- **Multi-tenant personal agents.** One isolated agent (or stack) per end-user in
  a hosted product — the VM boundary is the tenancy boundary.
- **Synthetic-data / pipeline swarms.** Many ETL/generation agents, each scoped to
  its own data + credentials.

**Interfaces (the human-facing surface that drives the orchestration):**
- **Mission-control dashboard** — a fleet view of running agents/VMs with
  live status, cost/resource, kill switch. (The generic orchestrator UI.)
- **Branch-tree / timeline visualizer** — for tree-search: see the fan-out, the
  scores, prune and re-branch by clicking. Maps directly to snapshot+branch.
- **Grid / spreadsheet** — each row is a task; a column triggers an agent and
  fills in the result. Great for batch verticals (lead enrichment, content).
- **Kanban board** — cards are agent jobs moving through stages; humans approve at
  gates. The marketing example fits here.
- **Chat that spawns sub-agents** — a conversational front that silently fans out
  isolated workers per request and streams them back.
- **Terminal multiplexer of agents** — tmux-for-agents, for the power user.
- **Event/webhook-driven (headless)** — no UI; agents triggered by GitHub events,
  inbound email, cron, queue messages. Each firing is an isolated run.
- **IDE / editor plugin** — agents that run in VMs but surface in-editor.

These aren't all guides we write day one — they're the menu the build-kit's
"interview step" picks from, and the list we draw hackathon tracks from.

## Distribution & community

- **`create-nebula-app`** published to npm (and cargo) — the primary entry point;
  every doc and the blog lead with it.
- **Blog post** on launch (link from the Nebula README + docs site).
- **In-repo `examples/`** — the template apps, also the `create-nebula-app`
  template source.
- **In-repo `components/`** — the cookbook library; each with its README/skill.
  This is the most plausible **"Docker Hub" network-effect artifact**: a catalog
  of components (ours + community) that an app pulls from is the flywheel that
  separates this from a one-off scaffolder. Consider whether it eventually wants a
  browsable registry / the existing apps-platform catalog as its home.
- **In-repo docs** (`docs/orchestrators/`) for the guide + the two build-kits +
  the SDK reference they point to.
- **Hackathons / cosponsorship** — once a couple of polished examples exist, run
  or sponsor a "build a Nebula app" hackathon; `create-nebula-app` is the starter,
  the examples are the inspiration, winners become more examples. Flywheel.

## Open questions / dependencies (resolve before writing)

- [ ] Luminal's public API/packaging stable enough to document? (gates §5/§6)
- [ ] Is there a stable, documented SDK surface for `sandbox run` + snapshot/branch
      + log streaming, or do parts still require CLI/unstable calls? (gates §4–§8)
- [ ] Decide the canonical local-model integration path(s) we test and bless
      (LM Studio vs llama.cpp vs Ollama) so the guide isn't three half-tested ones.
- [ ] Secrets story: what we recommend for key storage/mounting per platform.
- [ ] Pick the example vertical(s) to actually build and maintain.
- [ ] Where the build-kit cheat-sheet is generated from so it can't drift.
- [ ] Naming/branding for the ecosystem ("Nebula apps"?) and hackathon framing.
- [ ] `create-nebula-app`: npm-only, cargo-only, or both? Which framework does the
      template assume (Tauri + React? Svelte? plain web?) and how opinionated.
- [ ] Which 3–4 templates ship first (grid / kanban / chat / tree-search?).
- [ ] How much of the agent runtime the template bundles vs pulls (Luminal image).
- [ ] **Testing harness home:** shared `@nebula/testing` package/crate vs baked
      into each template (lean shared). Who maintains the cassette format.
- [ ] **Cassette/determinism strategy:** record-replay vs scripted stub vs a tiny
      local deterministic model for CI — and how cassettes are refreshed when
      prompts change without becoming stale lies.
- [ ] Does the SDK expose snapshot/branch cleanly enough to use as a per-test
      reset primitive yet, or is that CLI-only today? (gates the integration tier)
- [ ] Stance on LLM-as-judge in CI (default off / advisory only?).
- [ ] **Component format:** what makes a component agent-consumable — a strict
      README/skill schema? a manifest (what it spans: API/UI/sqlite/image, deps)?
      a copy-in CLI (`nebula-app add <component>`) vs pure docs the agent reads?
- [ ] Which components ship v1 (secrets vault + agent-image + chat look like the
      irreducible core; llama.cpp model-manager is the best tutorial).
- [ ] llama.cpp / stable-diffusion.cpp embedding: bundled in the agent image, a
      pullable Nebula image, or a host sidecar? (model-weight size vs offline goal)
- [ ] **Headless template + server deploy:** what does the CLI-daemon/cron template
      look like, and what's our blessed way to run Nebula on a server given the
      nested-virt caveat (recommend a nested-virt-enabled VM / which clouds support
      it / when slim-direct is enough without a VM)?

**Decided (from discussion):**
- Components live in-repo in **`/nebula-components`** (first-party); third-party
  components get their own repos + npm/crates. No separate registry to build — the
  folder *is* the catalog, same as the JSON apps-platform catalog.
- Component **staleness/maintenance** is handled by a dogfooded **headless
  agent-orchestrator app** that tests and auto-updates components. (Open: when we
  build it; it depends on the harness + a few components existing first.)
- Non-Nebula-specific components (OAuth, xterm, chat schema) **are** in-scope as
  "the blessed way on Nebula" — the maintainer app is what keeps them from rotting.
- Still open within these: the exact **component manifest/skill schema** and whether
  there's a `nebula-app add <component>` copy-in CLI vs pure agent-read docs.

## Out of scope for now

- The orchestrator we're building ourselves (tracked elsewhere).
- Hosted/multi-tenant operations of third-party Nebula apps.
- Any guide content that depends on surfaces not yet stable — flag and defer
  rather than document something that will change under readers.
