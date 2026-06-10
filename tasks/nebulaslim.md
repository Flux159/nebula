# nebula-slim — plan & task tracker

Goal: replace the Go container stack (dockerd 69 MB + containerd 40 MB +
k3s 67 MB + host CLIs) with Rust reimplementations so the total embed payload
(kernel + rootfs + sidecars) lands **≤ 50 MB**, while passing ~95% of a
real-world compatibility corpus — scored both with the **unmodified docker
CLI** (the oracle) and with slim's own packaged CLIs — `docker-slim`,
`kubectl-slim`, `helm-slim` — which is what embedders actually ship.
Context and non-negotiable decisions live in `slim/BRIEF.md` — read it first;
this file is the execution plan.

## Overall thoughts

**This is a research bet, so the corpus is the product.** The single number
that decides whether slim ships is "% of corpus passing against slimd vs real
dockerd" alongside "embed MB". Both get measured from week one and reported in
every commit message. If either trend stalls, that's a finding, not a failure.

**The compatibility contract is the socket, not the binary.** slimd serves the
Docker Engine REST API subset on `/var/run/docker.sock` in the guest. The
entire existing host stack — nebulad's unix→vsock proxy
(`$NEBULA_HOME/run/docker.sock` → vsock 2375 → guest socket), `DOCKER_HOST`,
the UI, the REST API, the Apps catalog — works unchanged. We never touch the
host plumbing; we swap what answers on the guest side.

**We ship the client CLIs too — as standalone slim binaries.** `nebula
docker` today just proxies argv to a real docker binary — slim cannot assume
any of docker/kubectl/helm exist on the user's machine, and bundling the real
ones is 150+ MB. So slim ships three host-side binaries built in this
workspace: **`docker-slim`** (S6), **`kubectl-slim`** (S8), **`helm-slim`**
(S9). Nebula itself barely changes: the existing proxy wrappers grow one
check — if the engine is slim, exec the packaged `*-slim` binary instead of
the real CLI. These are HOST binaries (macOS/Windows/Linux host triples, not
guest musl) speaking to the existing `$NEBULA_HOME/run/docker.sock` /
`DOCKER_HOST` like any docker client. The real CLIs remain the
*compatibility oracle*: the corpus runs both clients and diffs output.
(Size option, measure-don't-assume: the three can be hardlinks into one
multi-call binary dispatching on argv[0] if that wins meaningful MB.)

**Exploit the VM boundary everywhere.** The vessel is the security boundary.
That means: no rootless, no userns gymnastics, seccomp/apparmor optional (a
default-allow start is acceptable), no protection against a malicious image
attacking the *guest* — only bounded blast radius. Every one of those
simplifications is lines of code and bytes we don't ship.

**Size discipline is per-dependency, not per-binary.** Every crate added to
slimd gets a before/after `cargo bloat` + stripped-musl-size check recorded in
the size ledger (below). The known temptations: tokio (probably fine if
features are trimmed, but measure), hyper (likely overkill — the Engine API
is HTTP/1.1 over a unix socket with connection hijack for exec/attach;
`httparse` + hand-rolled responses may win), youki's libcontainer (depend on
it ONLY if the static musl build stays small — otherwise a minimal
runc-equivalent is ~2k lines and we control the kernel config).

**Build order vs BRIEF "phase 1".** The BRIEF's "phase 1" is the overall
scope/definition-of-done (build included, no k8s control plane). Internally
that decomposes into milestones S0–S11 below; `docker build` is S5 because it
needs the layer store (S2) and run (S1) underneath it, not because it's
deprioritized.

**Dev loop on macOS.** slimd targets `aarch64-unknown-linux-musl` (same as
vessel-init/agent). Three test tiers, cheapest first:
1. **Unit/host tests** — pure-logic crates (API types, Dockerfile parser,
   tar/layer math) test natively on macOS.
2. **Linux integration without a vessel** — run the test binary inside a
   container on the *existing full-flavor engine* (`nebula docker run -v …`).
   This gives real namespaces/cgroup2/overlayfs in seconds, and real dockerd
   one socket away as the reference implementation for diff-proxy debugging.
3. **In-guest acceptance** — boot a `slim`-flavor vessel, run the real docker
   CLI from the host through the normal proxy. This is `scripts/test-slim.sh`
   territory and the only tier that scores the corpus officially.

## Working directories (multi-agent isolation)

The main repo currently has another agent active on it. Slim development
happens in **`~/Projects/nebula-slim`** as its own git repo and standalone
cargo workspace (git init there; commit/push freely without touching this
tree). Layout mirrors what will land here so syncing is `rsync -a --delete
~/Projects/nebula-slim/ slim/` minus `.git`.

- All slimd/crate code, slim-only docs, and slim unit tests live there.
- Copy into `slim/` at each milestone boundary (S-gates below) once green,
  as a single commit in this repo.
- Changes that must touch the **main repo** (rootfs `FLAVOR=slim`,
  vessel-init services entry, `kube.rs` facade routing, `scripts/test-slim.sh`,
  this file, `tasks/issues.md` entries) are made here directly, kept small,
  and only at integration milestones (S7+) to minimize collision with the
  other agent. Coordinate via `tasks/issues.md` if a conflict appears.

## Workspace layout (slim/ — own cargo workspace, excluded from root)

```
slim/
  Cargo.toml            # workspace; aarch64-unknown-linux-musl primary target
  BRIEF.md
  crates/
    slim-api            # Engine API types (serde), version negotiation, error envelopes
    slim-http           # unix-socket HTTP/1.1 server: httparse, chunked, hijack/upgrade
    slim-image          # registry client (pull/push, auth), layer store, overlayfs graph
    slim-runtime        # create/start/exec: namespaces, cgroup2, pivot_root, console/pty
    slim-net            # bridge + veth + nftables publish; IPAM; DNS wiring
    slim-build          # Dockerfile parser + executor + instruction cache
    slim-client         # Engine API client + docker command surface (lib)
    slim-kube           # k8s YAML parse + facade mapping onto Engine API calls (lib)
    slim-helm           # chart fetch + values merge + Go-template-subset render (lib)
    slim-tmpl           # Go-template-subset engine, shared by slim-helm and `inspect -f`/`--format`
    slimd               # guest daemon binary (musl): wires everything, serves the socket
    docker-slim         # HOST binary: thin main over slim-client
    kubectl-slim        # HOST binary: thin main over slim-kube + slim-client
    helm-slim           # HOST binary: thin main over slim-helm + slim-kube
  corpus/               # compatibility corpus + scoring harness (see S0)
  scripts/              # build-musl.sh, size-report.sh, run-corpus.sh
```

Rust 2021, same toolchain pins as the root workspace. Release profile:
`opt-level = "z"`, thin LTO, `panic = "abort"`, strip — measure each.

## Size ledger (update every milestone; report in commits)

| artifact | today (full) | slim target | current slim (measured) |
|---|---|---|---|
| guest daemons | dockerd 69 + containerd 40 + runc | slimd ≤ 8 MB stripped | **slimd 2.6 MB (1.4 MB gz)** ✅ |
| k8s | k3s 67 | 0 (facade in slim-kube) | **0 in guest** ✅ |
| rootfs gz | 117 MB (full) / 57 (docker) / 6 (minimal) | ≤ 15 MB (slim flavor) | **8.9 MB gz** (alpine+iproute2+slimd) ✅ |
| kernel gz | 16 MB | 16 MB (trim = stretch S11) | 16 MB (unchanged) |
| host sidecars | nebula+nebulad ~4.7 MB | unchanged | unchanged |
| host CLIs | docker 39 / kubectl 55 / helm 59 | docker/kubectl/helm-slim ≤ 6 MB combined | **1.35 MB gz combined** (docker 0.9 / kubectl 0.8 / helm 0.9 MB) ✅ |

**Nebula-Slim.app measured (macOS arm64): 54 MB on disk / 34 MB download**
(vs full 311 MB / 207 MB). Embed core (kernel+rootfs+sidecars+CLIs, no UI):
**31.9 MB**; +libkrun 45.7 MB. Booted as a real VZ microVM and driven by the
**unmodified Docker CLI 27.5** (run/build/logs/exec/exit-codes), plus the slim
CLIs 32/32. `docker build` via the real CLI needs `DOCKER_BUILDKIT=0` (slim is
classic-builder only). Details: docs/slim-size-and-status.md.

**Status: S0–S9 code complete. Validated 32/32 end-to-end in the nebula
microVM on macOS** (slimd in a privileged container = real Linux
namespaces/overlayfs/cgroup2): docker-slim 18/18 (pull/run/logs/exec/stop/rm/
inspect -f/volumes/build), kubectl-slim+helm-slim 14/14 (apply/get/scale/
delete/ConfigMap-env, helm template/install/list/uninstall). Run
`scripts/test-slim.sh`. Measured total embed ≈ 16 (kernel) + 8.9 (slim
rootfs) + 1.35 (CLIs) ≈ **26 MB gz — roughly half the 50 MB target.**
| **total embed** | **~140 MB+** | **≤ 50 MB, aiming ~35** | — |

---

## Milestones

### S0 — Scaffolding, corpus, and measurement (the harness comes first)

- [ ] `git init ~/Projects/nebula-slim`; workspace skeleton with empty crates,
      musl target config, `scripts/build-musl.sh` (zig-cc or musl-cross, match
      vessel-init's build approach), CI-able `scripts/size-report.sh`
      (stripped sizes + gz, prints the ledger row).
- [ ] **Compatibility corpus v1** (`corpus/`): executable list of real
      invocations with expected outcomes. Mine, in order:
      `scripts/test-phase2.sh` (docker basics, build, compose), the four
      Apps catalog entries (`apps/catalog.json`: uptime-kuma, gitea,
      vaultwarden, n8n — run + volumes + ports + env), and 30–50 commands
      from top docker tutorials (run/exec/logs/cp/inspect/build patterns).
      Format: one file per case (cmd, stdin, expected exit/stdout shape),
      runner produces `PASS/FAIL/SKIP(501)` and a single % score.
- [ ] Run corpus against **real dockerd** on the existing engine to validate
      the harness itself (target: 100% there, by construction).
- [ ] Diff-proxy tool (`corpus/diffproxy`): unix-socket MITM that logs
      request/response pairs to both slimd and real dockerd and diffs them.
      Cheap version is fine (record/replay, not live tee).
- [ ] Decision spike, timeboxed: **youki libcontainer static-musl size**.
      Build a hello-world consumer, strip, record. Go/no-go threshold:
      if it adds > ~3 MB to slimd, write the minimal runtime ourselves (S1).
- [ ] Decision spike, timeboxed: async runtime + HTTP. Candidates:
      (a) tokio(featured-down)+hyper, (b) tokio+hand-rolled httparse server,
      (c) threads+httparse. Build all three as hello-socket servers, strip,
      record sizes, pick one. Bias: simplest thing that handles hijack
      streams cleanly; concurrency needs are modest (one user, tens of
      containers).

**Gate:** corpus runner scoring real dockerd; size ledger automated; both
spikes decided and logged in `tasks/issues.md`. Sync skeleton to `slim/`.

### S1 — Runtime core: a process in a box

The smallest thing that runs: given an unpacked rootfs dir + config, start a
contained process. No images, no API yet — exercised by a test binary.

- [ ] Namespaces (mnt/pid/uts/ipc/net), cgroup2 (memory/cpu/pids limits),
      pivot_root, standard mounts (/proc, /sys, /dev minimal set, devpts),
      hostname, env, cwd, user (root default; USER support = setuid/gid).
- [ ] Lifecycle: create → start → signal/kill → wait → exit code; reaping.
- [ ] stdio: pipes mode + pty mode (for `-t`); log capture to file
      (json-lines like docker's json-file driver — keeps `logs` simple).
- [ ] exec-in-container (enter namespaces, optional pty).
- [ ] OOM/exit event plumbing (the events-lite source of truth).
- [ ] Tier-2 tests run inside a privileged container on the existing engine.

**Gate:** test harness runs a busybox rootfs, gets stdout, exit codes, exec,
TTY resize. Size check on the test binary.

### S2 — Image store: pull, layers, overlayfs

- [ ] Registry client: pull by tag/digest, manifest list → arch select,
      layer fetch with resume, `oci-client`/`oci-distribution` if the size
      budget allows, else minimal HTTP against registry v2.
      Auth: anonymous, basic, token (Docker Hub + ghcr are the corpus).
- [ ] Layer store on the data disk (`/var/lib/nebula/slim/`): content-addressed
      blobs, tar application with whiteout handling, overlayfs lowerdir
      chains, config/manifest JSON store.
- [ ] Image ops: list, inspect, rm (with refcount), tag; prepare rootfs
      (overlay mount) + teardown for the runtime.
- [ ] Push (manifest + blob upload) — needed for build story completeness;
      keep minimal.
- [ ] Corpus cases: pull alpine/busybox/the four catalog images; digest
      pinning; `docker images` shapes.

**Gate:** pull → run busybox/alpine end-to-end via test harness (S1+S2 glued).
Wall-clock pull/run measured vs dockerd.

### S3 — slimd: the Engine API daemon

- [ ] slim-http server on `/var/run/docker.sock`: routing, API version prefix
      handling (`/v1.43/...` and unversioned), JSON error envelopes,
      **clean 501 + message for everything unimplemented — never hang,
      never panic** (catch-all route, top-level panic guard per connection).
- [ ] System: `_ping`, `/version`, `/info` (enough fields for the docker CLI
      not to trip), `/events` (lite: container lifecycle + image pull).
- [ ] Containers: create (translate HostConfig subset: binds, ports, env,
      entrypoint/cmd, restart policy, resources), start, stop, restart, kill,
      rm, wait, list (filters: name/label/status), inspect (the big one —
      the CLI reads many fields; fill what corpus needs, null the rest),
      rename, stats-lite (one-shot + stream).
- [ ] Streams: logs (+follow, stdout/stderr multiplexing frames), attach,
      exec create/start/resize/inspect with connection hijack (interactive
      `-it` is corpus-critical).
- [ ] `docker cp` (archive endpoints GET/PUT `/containers/{id}/archive`).
- [ ] Restart policies: `no`, `on-failure[:max]`, `always`,
      `unless-stopped` — supervisor loop inside slimd.
- [ ] State persistence: containers survive slimd restart (state dir +
      rescan; vessel-init restarts slimd on crash with 250 ms backoff,
      same as dockerd today).

**Gate:** real docker CLI against slimd (tier 2: slimd running inside a
privileged container) passes run/ps/logs/exec/rm/inspect corpus slice.
First official corpus % reported.

### S4 — Networking & volumes

- [ ] Default bridge (`nebula0`, but presented as `bridge` in the API) +
      veth pairs + IPAM (one /24, simple allocator persisted with state).
      Use `rtnetlink`; fall back to shelling busybox `ip` only if a wall is
      hit (log it in issues.md).
- [ ] Port publish via nftables (dnat + hairpin); `-p host:container[/udp]`.
      Must compose with nebulad's existing host-side dynamic port forward.
- [ ] Container DNS: point resolv.conf at the bridge gateway IP, slimd
      answers container-name + `<name>.<network>` lookups itself and relays
      the rest to the agent's dns_proxy path (gateway:53 → host resolver),
      matching the docker0/172.17.0.1 pattern documented in CLAUDE.md.
- [ ] Networks API: create/ls/rm/connect/disconnect, inspect-lite;
      user-defined networks get isolated bridges + DNS scoping.
- [ ] Volumes: CRUD + anonymous volumes + `-v name:/path` binds under
      `/var/lib/nebula/slim/volumes`; bind mounts of guest paths (and the
      virtiofs share paths so `-v $HOME/...` keeps working from the host
      CLI's perspective).
- [ ] Corpus: catalog apps (they need ports+volumes+env), two-container
      network with name resolution (the classic app+db tutorial).

**Gate:** all four Apps catalog entries install and serve traffic from the
host browser against slimd (tier 2). Corpus % update.

### S5 — docker build (the Suyog-call feature)

- [ ] Dockerfile parser: FROM (multi-stage `AS`), RUN, COPY/ADD (wildcards,
      `--chown`, URL ADD, tar auto-extract), ENV, ARG (+ build-args),
      WORKDIR, USER, LABEL, EXPOSE, CMD, ENTRYPOINT, VOLUME, SHELL,
      HEALTHCHECK (parse+store, enforcement optional), `COPY --from`,
      `.dockerignore`.
- [ ] Executor over the S1/S2 stack: each RUN = container exec on the
      staged rootfs, snapshot the upper dir as a new layer; metadata ops are
      config-only layers.
- [ ] Build context: accept the tar the docker CLI POSTs to `/build`;
      stream-unpack with ignore rules.
- [ ] **Instruction cache**: key = instruction text + parent layer +
      content hash of copied inputs; invalidation matches docker's mental
      model (the corpus has explicit cache-hit/miss cases).
- [ ] Skip-with-warning (NOT fail): `--mount=type=cache|secret|ssh`,
      heredocs, buildx-isms, `--platform` mismatches.
- [ ] Build progress output in the classic (non-buildkit) stream format the
      CLI renders.
- [ ] Corpus: the `scripts/test-phase2.sh` build case, a multi-stage Go-style
      build, a node app with .dockerignore + cache assertions.

**Gate:** `docker build` corpus slice green; build wall-clock vs dockerd
recorded. Sync S1–S5 state into `slim/`.

### S6 — docker-slim (standalone host client binary)

`nebula docker` today execs the real docker binary — slim cannot assume one
is installed. Build **`docker-slim`**: a standalone host binary (thin main
over the `slim-client` lib) that is drop-in argv-compatible with the docker
CLI for the corpus surface. Nebula's only change (deferred to S7): the
`nebula docker` wrapper execs the packaged `docker-slim` when the engine is
slim, real docker otherwise.

- [ ] Engine API client over the unix socket (honors `DOCKER_HOST`,
      defaults to `$NEBULA_HOME/run/docker.sock`), types shared with
      `slim-api` — the client and the daemon can never drift on
      serialization.
- [ ] Builds for host triples (aarch64/x86_64 darwin + linux; windows
      later with the rest of the windows story) — unlike slimd, this is NOT
      a guest musl binary.
- [ ] Command surface, corpus-driven: run (the big flag matrix: -d, -it,
      --rm, -p, -v, -e/--env-file, --name, --network, --restart,
      --entrypoint, -w, -u, --label, --memory, --cpus, --add-host,
      --hostname), create/start/stop/restart/kill/rm, ps, images,
      pull/push/tag, build, exec, logs (-f, --tail, -t), inspect, cp, wait,
      stats, volume/network/image/container subcommand trees, system
      df-lite/prune-lite, login/logout (config.json-style cred file in
      `$NEBULA_HOME`).
- [ ] `--format` / `inspect -f`: Go-template-lite via the shared `slim-tmpl`
      engine — common `{{.Field.Path}}`, `{{json .}}`, `{{range}}`; clear
      error (not silent garbage) on unsupported constructs.
- [ ] Output fidelity: table layouts, `-q`, exit codes, stdout/stderr split
      matching real docker — scripts and the corpus depend on these.
- [ ] Interactive `-it`: raw terminal + resize over the hijacked connection
      (reuse nebula-cli's existing pty handling from the shell path).
- [ ] Corpus runs in BOTH client modes — real docker CLI vs slim CLI against
      slimd, diffing output; slim CLI also runs against real dockerd as a
      cross-check (catches client bugs vs daemon bugs).
- [ ] Ledger: docker-slim stripped size per host triple.

**Gate:** corpus % under docker-slim within ~2 points of the real-CLI score;
`test-slim` runs pass with NO real docker binary on PATH.

### S7 — Integration into nebula proper (main-repo changes start here)

- [ ] `vessel/rootfs/Dockerfile`: add `FLAVOR=slim` — busybox/alpine-minimal
      base + slimd + vessel-init + vessel-agent + iptables/nft userspace;
      no dockerd/containerd/k3s/apk-docker packages. `ROOTFS_SIZE_MB`
      sized down accordingly.
- [ ] `crates/vessel-init`: add `slimd` to the services table (wait_for
      `/var/lib/nebula`, restart-on-crash) — enabled when present on the
      rootfs / flavor env, mutually exclusive with dockerd+containerd.
- [ ] Engine selection plumbed where needed (`nebula install-image` flavor
      choice, doctor awareness). `nebula docker`/`kubectl`/`helm` wrappers
      each grow ONE check: engine is slim → exec the packaged `*-slim`
      binary; otherwise existing behavior. No other nebula changes.
- [ ] Packaging: `docker-slim`/`kubectl-slim`/`helm-slim` ship next to
      `nebula`/`nebulad` in the .app/dist bundle and the sign/notarize
      pipeline picks them up (`scripts/sign-dev.sh` + release pipeline).
- [ ] `scripts/test-slim.sh` in house style (capture-then-grep, sign-dev
      first): boots a slim vessel, runs the full corpus through the real
      host-side proxy path, prints corpus % + size ledger.
- [ ] Run phase-2/3-equivalent checks against the slim engine; log gaps in
      `tasks/issues.md`.
- [ ] Measure: slim rootfs gz size, cold boot → first container wall-clock
      vs full flavor.

**Gate:** `scripts/test-slim.sh` green end-to-end on a real vessel; ledger
row "total embed" filled with a real number.

### S8 — k8s facade + kubectl-slim (standalone host client binary)

- [ ] slim-kube lib: parse Deployment / Job / Service / ConfigMap / Secret /
      Pod YAML (multi-doc, `---`); map to slim engine ops:
      Deployment → N restart-supervised containers (labels carry identity),
      Job → run-to-completion + backoff, Service → published ports + DNS
      name `<svc>.<zone>`, ConfigMap/Secret → env + mounted files,
      Pod → multi-container with shared netns.
- [ ] **`kubectl-slim`** binary (thin main over slim-kube + slim-client):
      verbs apply/delete/get/logs/scale/exec + describe-lite, translated
      onto Engine API calls against the host docker socket. Output shapes
      close enough to kubectl for eyeballs and basic scripts; no k8s API
      wire protocol. Nebula's `kube.rs` change (at S7) is just the
      exec-`kubectl-slim` routing check — no facade logic in nebula-cli.
- [ ] `kubectl get` table output for the mapped kinds; `-o yaml/json`
      reconstructed from stored manifests + live status.
- [ ] In slim mode `nebula kubectl` never execs real kubectl and never
      starts k3s — no kubectl binary on the host, no k3s in the rootfs.
      Ledger: kubectl-slim stripped size.
- [ ] Corpus extension: the `scripts/test-phase5.sh`-derived basics +
      a guestbook-style Deployment+Service tutorial.

**Gate:** the facade verbs pass their corpus slice on a slim vessel; clear
`unsupported in slim — use full flavor` message for everything else
(CRDs, helm, operators...).

### S9 — helm-slim (standalone host client binary)

Third standalone binary: thin main over the slim-helm + slim-kube libs,
routed from the `nebula helm` wrapper by the same S7 engine check. Helm's
surface decomposes as: chart fetch + values merge + Go-template render +
apply. Render fidelity is the hard part (Go templates + sprig functions) —
the rest reuses machinery we already have.

- [ ] `slim-tmpl` spike first: evaluate the `gtmpl` crate vs writing our own
      Go-template subset (size + fidelity budget, same method as the S0
      spikes). This engine is shared with S6's `--format`, so it likely
      already exists in reduced form by now — S9 extends it.
- [ ] Sprig subset, corpus-mined from real charts: default, quote/squote,
      toYaml/fromYaml, include/define, tpl, required, indent/nindent, trunc,
      trimSuffix, b64enc, dict/list/get/set, printf, if/range/with/end
      pipeline semantics. Unsupported function → render error naming it,
      never silently wrong output.
- [ ] Chart sources: HTTP repos (index.yaml), OCI registries (reuse
      slim-image's registry client), local dirs and .tgz; dependency
      (subchart) resolution + values scoping.
- [ ] Values semantics: chart defaults ← -f files ← --set/--set-string,
      subchart blocks, global.
- [ ] Verbs: install/upgrade/uninstall/list/status/template (+ get
      manifest); release state stored as metadata on slim-kube objects, not
      k8s Secrets.
- [ ] Rendered manifests apply through slim-kube (S8); kinds outside the
      facade fail with the standard `unsupported in slim` message naming the
      kind.
- [ ] Corpus: 3–5 popular charts that fit the facade (simple web-app-shaped
      ones), plus `helm template` golden-output diffs vs real helm on those
      charts.

**Gate:** corpus charts install and serve traffic; `helm template` diffs
clean on the corpus set; no real helm binary involved anywhere.
Ledger: helm-slim stripped size (and the ≤ 6 MB combined CLI row).

### S10 — Compose (stretch, end of phase 1 scope)

- [ ] Client-side translation in `nebula docker compose` path (native in
      slim-client): services → run with network/volumes/env/depends_on
      ordering; up/down/ps/logs. Only if S0–S9 are green and budget remains.

### S11 — Hardening & the research report

- [ ] Soak: 50-container scale case (mirror phase-6 bounded scale), restart
      matrix (slimd crash, vessel reboot, host nebulad restart).
- [ ] Diff-proxy sweep over the whole corpus for response-shape drift.
- [ ] Final numbers: corpus % (slim vs dockerd), embed MB, perf table
      (pull/run/build/boot). Write up in `tasks/spike-notes.md` style —
      this is the research result that decides the slim tier's future.
- [ ] Stretch: kernel config trim for a slim-specific kernel (no k8s
      netfilter extras, etc.) if rootfs alone can't hit ≤ 50 MB — only
      open this if needed; it forks the kernel build matrix.

## Risks / open questions (log resolutions in tasks/issues.md)

- **inspect-field long tail**: the docker CLI and ecosystem tools read odd
  corners of `inspect` output. Mitigation: corpus-driven, diff-proxy, and
  nulling unknown fields rather than omitting them.
- **Hijack/stream correctness** (`-it`, attach, logs -f) is where hand-rolled
  HTTP bites. The S0 spike must prove the chosen stack does upgrade +
  half-close cleanly.
- **overlayfs edge cases** (whiteouts, opaque dirs, hardlinks across layers)
  — use real-world images in the corpus early (gitea is a good torture test).
- **youki dependency weight** — settled by the S0 spike, not by taste.
- **Registry auth quirks** (Docker Hub rate limits, token refresh, ghcr
  anonymous) — corpus needs a non-Hub registry case.
- **Coexistence**: a user switching a vessel between full and slim flavors —
  separate state dirs (`/var/lib/nebula/slim/`) so images/containers don't
  cross-contaminate; document that they don't share caches.
- **Go-template fidelity** (helm render, `inspect -f`, `--format`) — Go's
  template language + sprig is a deep surface. Mitigation: one shared
  `slim-tmpl` engine grown corpus-first, hard errors on unsupported
  constructs (never silently wrong YAML), `helm template` golden diffs vs
  real helm.
- **CLI output drift** — scripts grep docker/kubectl output. Mitigation:
  corpus diffs slim CLI vs real CLI output byte-for-byte where feasible
  (tables, -q, exit codes), not just "did it work".
- **Main-repo merge conflicts with the other agent** — all S0–S6 work stays
  in `~/Projects/nebula-slim`; main-repo edits batched at S7+.
