# Nebula — "Build Your Own Agent Orchestrator" documentation (plan)

Status: **planned**, not yet written. This is the spec for a guide we publish
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
- **A UI starting point.** The Tauri app in `ui/` is a *client* of the daemon.
  A builder forks the patterns (or starts a fresh Tauri app against the SDK)
  rather than building VM management from scratch.
- **Local-first AI.** virtiofs + host-faithful DNS mean an agent in a VM can
  reach a model server running on the host (LM Studio / llama.cpp / Ollama) or a
  model container in the Vessel. Keys and prompts never have to leave the machine.
- **Cross-platform & daemon-first.** Same app on three OSes; the engine keeps
  running if the UI is closed.

TODO for the guide author: confirm each of the above maps to a *documented,
copy-pasteable* command/SDK call at write time. Anything that's still a CLI-only
or unstable surface should be called out as such, not presented as a stable API.

## Deliverables

Two artifacts, same content at two levels of formality:

1. **The how-to guide / blog post** — narrative, skimmable, with screenshots and
   a finished example repo. "Build a local, isolated *marketing-agent* app on
   Nebula." Aimed at a human reading top-to-bottom.

2. **The copy-paste agent build-kit** (`build-your-orchestrator.md`) — a single
   self-contained markdown file a user pastes into a coding agent (Claude Code,
   Cursor, etc.) that instructs it to scaffold *their* orchestrator for *their*
   use case. It encodes the same steps as imperative, parameterized instructions
   ("Ask the user for: vertical, model backend, tools the agents may call, …;
   then scaffold a Tauri app wired to the Nebula SDK with …"). This is the
   higher-leverage artifact — most builders will start here.

Both should live in-repo (e.g. `docs/orchestrators/`) and be mirrored to the
website/blog. The example app gets its own public repo we can point hackathon
participants at.

## Guide outline (sections both artifacts cover)

Worked example throughout: **a marketing-agent app** (briefs in → a small team of
agents researches, drafts, critiques, and schedules content → human approves).

1. **What you're building & why on Nebula** — the substrate pitch above, the
   isolation/fan-out/local-first story, the finished-app screenshot.
2. **Prereqs & install** — install Nebula (full) or embed Nebula-slim; verify the
   daemon + SDK; `nebula up`.
3. **Scaffold the app** — start a Tauri app (fork `ui/` patterns or fresh); wire
   the TypeScript SDK; "hello, daemon" call.
4. **Connect to Nebula** — open the SDK client to `127.0.0.1:7440`; list/create
   workloads; the difference between a long-lived Vessel container and an
   ephemeral `sandbox run`.
5. **Run an agent in isolation** — drop a Luminal container (or your own agent
   image) into a sandbox; pass it a task; stream logs/results back over the SDK;
   tear down. Emphasize: untrusted tool-calls run *here*.
6. **Wire up the model** — connect agents to a **local** model server: LM Studio,
   llama.cpp (`server`), or Ollama on the host, reachable from the VM via the
   host DNS/virtiofs path; or a model container in the Vessel. Show the env/URL
   plumbing for each. Note the privacy property (nothing leaves the machine).
7. **Secrets & auth** — where API keys live (host keychain / `.env` mounted into
   the VM, not baked into images), how to scope a key per agent, how to *not*
   commit them. Least-privilege: an agent gets only the creds its task needs.
8. **Orchestrate many agents** — the patterns: a team of role-specialized agents
   (researcher/writer/critic), fan-out attempts via snapshot+branch and pick the
   best, a reconcile/queue loop. Keep it to primitives the SDK exposes.
9. **Build the custom UI** — the part that's *yours*: the workflow surface for the
   vertical (for marketing: a content calendar + approve/reject queue + per-agent
   transcript). How to subscribe to agent/workload events and render them.
10. **Package & ship** — bundle Nebula-slim so users don't need Docker; sign/
    notarize (point at `.github/workflows/release.yml`); cross-platform notes.
11. **Where to go next** — the example repo, the SDK reference, the community.

## The copy-paste agent build-kit (what `build-your-orchestrator.md` contains)

Sketch of the file we ship for pasting into an agent:

- **Preamble**: "You are building a custom, local-first, sandboxed agent
  orchestrator on top of Nebula. Here is everything you need."
- **Interview step**: questions the agent must ask the user first — vertical/use
  case, which model backend (LM Studio/llama.cpp/Ollama/cloud), what tools the
  agents may call, single-agent vs team vs fan-out, UI surface they want.
- **Capability cheat-sheet**: the exact SDK calls / CLI commands for: start
  daemon, create sandbox, run agent image, stream logs, snapshot+branch, mount
  secrets, reach the local model. (Kept in sync with the SDK — versioned.)
- **Scaffold instructions**: generate a Tauri app, add the SDK dependency, wire
  the daemon client, generate the isolation layer, generate a starter UI for the
  chosen vertical.
- **Safety rails**: agents run in sandboxes; secrets are mounted not baked; the
  generated app must default to local models unless the user opts into cloud.
- **Definition of done / smoke test** the agent should run before handing back.

Design goal: the file is *self-contained and current* — pasting it should not
require the agent to go read five other docs. Keep a single source of truth and
generate the cheat-sheet from the SDK so it can't drift.

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

- **Blog post** on launch (link from the Nebula README + docs site).
- **In-repo docs** (`docs/orchestrators/`) for the guide + the build-kit + the
  SDK reference it points to.
- **Example repo(s)** — at minimum the marketing-agent app, ideally 2–3 across
  different interface metaphors (a grid one, a tree-search one).
- **Hackathons / cosponsorship** — once a couple of polished examples exist, run
  or sponsor a "build a Nebula app" hackathon; the build-kit is the starter, the
  examples are the inspiration, winners become more examples. Flywheel.

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

## Out of scope for now

- The orchestrator we're building ourselves (tracked elsewhere).
- Hosted/multi-tenant operations of third-party Nebula apps.
- Any guide content that depends on surfaces not yet stable — flag and defer
  rather than document something that will change under readers.
