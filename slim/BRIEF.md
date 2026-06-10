# nebula-slim — agent brief

You are building **nebula-slim**: Rust reimplementations of the container
stack Nebula currently ships as Go binaries, targeting the ~95% of commands
people actually run — "mostly compatible", explicitly NOT strict OCI/API
conformance. Work happens in this `slim/` directory as its own cargo
workspace. The host product (CLI, daemon, VMM, guest init/agent, images) is
done and working on macOS and Linux — read `docs/SKILL.md`, `docs/embedding.md`,
and `tasks/features.md` before writing code.

## Why (the pitch you are validating)

Nebula's embed payload is ~140 MB because of dockerd (69 MB), containerd
(40 MB), k3s (67 MB), and host CLIs — all already-stripped Go. Claude
Desktop ships a 10 GB VM bundle to run bash safely
(github.com/anthropics/claude-code/issues/29045 + the HN thread); the
"embeddable VM layer for AI apps" needs to be TINY. Target: **≤ 50 MB
total embed** (kernel 16 MB gz + slim rootfs + everything). Math says
~35 MB is reachable: minimal rootfs is 6 MB gz today, and the CLI surface
can live inside the existing `nebula` binary at zero added bytes.

## Decisions already made (don't relitigate)

1. **`docker build` IS in phase 1** (Suyog's call). No buildkit: implement a
   Dockerfile executor over your own layer store — FROM (incl. multi-stage
   `AS` / `COPY --from`), RUN, COPY, ADD, ENV, ARG, WORKDIR, USER, LABEL,
   EXPOSE, CMD, ENTRYPOINT, VOLUME. Layer caching by instruction+input hash.
   Skip (warn, don't fail): `--mount=type=cache`, heredoc syntax, buildx
   multi-arch.
2. **No k8s control plane.** k3s stays the opt-in "full" tier. The k8s story
   in slim is a **facade that hits the container runtime directly**: parse
   real Deployment/Job/Service/ConfigMap/Secret/Pod YAML and map onto slim
   containers (Deployment → N restart-supervised containers; Job →
   run-to-completion + backoff; Service → published ports + DNS name
   `<svc>.<zone>`; ConfigMap/Secret → env/files). Entry point is the
   existing `nebula kubectl` wrapper (crates/nebula-cli/src/kube.rs): when
   the engine is slim, it translates verbs natively instead of exec'ing real
   kubectl — you do NOT need to speak the Kubernetes API wire protocol in
   phase 1. Verbs: apply/delete/get/logs/scale/exec, describe-lite.
3. **Reuse existing Rust crates when they're small for our use**; budget
   each: `oci-spec` (types, tiny), `oci-distribution`/`oci-client` (registry
   pull/push), `tar`/`flate2`/`zstd`, `rtnetlink` (bridge/veth — prefer this
   over vendoring netavark unless you hit a wall), `nix`/`libc`. For the
   runtime: vendor or depend on **youki**'s libcontainer crate if it stays
   small in a static musl build; if it bloats, a minimal runc-equivalent
   (namespaces+cgroup2+pivot_root+seccomp-default) is ~2k lines and we
   control the kernel config (`vessel/kernel/nebula.fragment`) so you can
   assume modern everything.
4. Compose is client-side translation once run/networks/volumes exist —
   stretch goal at the END of phase 1, not core.

## Architecture (fit into what exists — don't invent parallel plumbing)

- **One static musl binary, `slimd`**, runs in the guest, supervised by
  `vessel-init` like dockerd is today (see `crates/vessel-init/src/main.rs`
  services table and `vessel/rootfs/Dockerfile`). It serves the **Docker
  Engine REST API subset** (v1.43-ish) on the same socket path dockerd uses
  — the host side (socket proxy over vsock, `DOCKER_HOST=unix://…`,
  `nebula docker …`, the UI, the REST API, the Apps catalog) then works
  UNCHANGED. That's the compatibility contract: real `docker` CLI as the
  client, your daemon as the server.
- Storage: overlayfs layer store on the data disk (`/var/lib/nebula`).
  Networking: one bridge + veth pairs + nftables port-publish, DNS names via
  the existing agent relay (see `crates/vessel-agent` dns_proxy).
- New rootfs flavor `slim` in `vessel/rootfs/Dockerfile` (FLAVOR arg
  exists): busybox/alpine-minimal + slimd + init/agent. No dockerd, no
  containerd, no k3s.
- Host CLI: extend `nebula docker` routing only if the real docker CLI
  can't express something — the goal is the REAL docker CLI passing against
  slimd.

## API subset (phase 1 definition of done)

Engine API: containers create/start/stop/restart/kill/rm/wait/logs(+follow)/
exec(+interactive)/inspect/list/stats-lite, images pull/push/list/inspect/
rm/tag/build(+context tar), volumes CRUD, networks create/connect/ls/rm,
events-lite, ping/version/info. Registry auth: anonymous + basic + token
(Docker Hub, ghcr). Everything else returns a clean 501 with a message,
never a hang or a panic.

## Method (this is a research bet — instrument it)

1. **Write the compatibility corpus FIRST**: an executable suite of real
   invocations (mine `scripts/test-phase*.sh`, the Apps catalog installs in
   `apps/catalog.json`, and common docker tutorials). Score = % passing
   against slimd vs real dockerd. Report this number in every commit; it is
   the research result.
2. Run real dockerd behind a diff-proxy when debugging compatibility (the
   engine vessel gives you a working reference implementation one socket
   away).
3. Measure continuously: binary size (musl, stripped), rootfs flavor size,
   pull/run/build wall-clock vs dockerd. The pitch fails if slim isn't BOTH
   small and fast.
4. Log surprises in `tasks/issues.md` like the rest of the repo does;
   acceptance script `scripts/test-slim.sh` in the house style
   (capture-then-grep — see the SIGPIPE notes in CLAUDE.md).

## Non-goals (phase 1)

Swarm, plugins, buildx/multi-arch, image signing/attestation, checkpoint,
live migration, registry HOSTING, strict OCI runtime conformance, Windows
containers, rootless-inside-guest (the VM is the security boundary —
exploit that simplification everywhere it makes code smaller).
