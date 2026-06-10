# Nebula — agent skill

Nebula runs containers, Kubernetes, and microVMs on macOS (Apple Silicon).
One always-on engine VM hosts docker + k3s; additional named microVMs
("vessels") boot in ~0.1–0.6s with snapshot/branch support. Print this guide
anytime: `nebula skill` (pipe it into your context or a SKILL.md).

## Engine lifecycle

```bash
nebula up           # boot the engine (~0.6s to healthy)
nebula status       # engine/agent health, contexts, ports
nebula down         # stop it (containers stop too; state persists)
nebula doctor       # diagnose common setup problems
nebula stats        # guest memory / balloon / host footprint (--watch)
```

Repair: `nebula vessels reset vessel` restores the engine OS to pristine
while KEEPING container/k8s data (~1s). `nebula down && nebula up` restarts.

## Containers (docker)

```bash
nebula setup docker          # point the docker CLI here (revert: nebula revert docker)
docker run -d -p 8080:80 nginx   # then use docker normally; ports appear on localhost
nebula docker ps             # one-off command, NEVER touches your contexts
```

The docker socket is `~/.nebula/run/docker.sock` — any docker client library
works against `unix://` that path. Containers resolve each other at
`<name>.nebula.local`. amd64 images run via Rosetta transparently.

## Kubernetes (k3s)

```bash
nebula kubectl get nodes     # one-off; first call starts k3s (~20s, once)
nebula setup kubectl         # merge a `nebula` context into your kubeconfig
nebula helm install …        # helm against the same cluster
```

Standalone kubeconfig: `~/.nebula/kubeconfig`. Images built/pulled through
the docker socket are schedulable in k3s WITHOUT a registry (shared
containerd store).

## Vessels (named microVMs)

```bash
nebula vessels new dev                          # libkrun: ~0.1s boot; --gpu for Vulkan
nebula vessels new agent --backend vz           # VZ: enables LIVE memory snapshots
nebula vessels new deb --from-image debian:bookworm-slim   # any arm64 docker image
                                                # (local `docker build` tags work too)
nebula vessels new ml --volume models:50 --volume scratch:10
                                                # extra persistent volumes: auto-
                                                # formatted ext4, mounted /mnt/<name>,
                                                # included in snapshots/branches
nebula vessels exec dev -- uname -a             # run a command (vsock, no ssh)
nebula vessels shell dev                        # interactive shell
nebula vessels ls / info / stop / start / rm / reset
```

Each vessel has a persistent rootfs + data disk; both survive stop/start.
Ship a prebuilt vessel OS: `nebula vessels convert-image <ref> --out f.img`
then `nebula vessels new x --rootfs-img f.img` (offline, ~0.1s).

## Snapshots & tree search (MCTS pattern)

```bash
nebula vessels snapshot agent step1     # vz vessels: disks + LIVE memory by default
                                        #   (~0.4s, the vessel never stops)
                                        # krun/stopped vessels: disk-only (~10ms)
nebula vessels snapshot agent step1 --no-memory    # force disk-only
nebula vessels restore agent step1      # memory snapshots RESUME mid-execution:
                                        #   running processes/RAM/sockets come back
nebula vessels branch agent try --snapshot step1 --count 5   # 5 independent clones
```

Tree-search loop over an agent's actions:

```bash
nebula vessels snapshot agent step1                 # checkpoint before the action
nebula vessels branch agent cand --snapshot step1 --count 4
for i in 1 2 3 4; do
  nebula vessels exec cand-$i -- run-attempt --temperature 0.8   # different seeds
done
# score the candidates however you like, keep the winner:
nebula vessels rm cand-2 --force … ; nebula vessels snapshot cand-1 step2 ; …
```

Memory-snapshot branches wake mid-execution at the exact saved instant —
no assumption that the agent flushed state to disk. Caveat: they share the
source's network identity; `exec`/`shell` (vsock) are unaffected.

## Ephemeral sandboxes

```bash
nebula sandbox run -- python3 -c 'print(1)'   # boot+run+teardown ~250ms
nebula sandbox run --share-cwd -- make test   # cwd mounted at /workdir
nebula sandbox run --gpu -- vulkaninfo        # virtio-gpu Venus (Vulkan->Metal)
```

## Isolation / embedding

`NEBULA_HOME=<dir>` gives a completely separate instance (own engine, disks,
sockets, ports). Per-instance `config.toml`: `max_ram_mib`, `cpus`,
`api_port` (REST at `127.0.0.1:<api_port>/v1alpha1/{status,stats,containers}`),
`dns_zone` (e.g. `galaxy.local`), `dns_port`, `k8s_port`. Full guide:
docs/embedding.md in the repo (github.com/Flux159/nebula).

## Gotchas worth knowing

- Snapshot/branch/restore stop a vessel briefly UNLESS it is a running vz
  vessel (live memory path). GPU requires krun; memory snapshots require vz.
- `nebula setup …` always has an exact undo: `nebula revert <tool>` (a revert
  stack restores your previous contexts; prod-looking contexts warn loudly).
- Engine memory is elastic: a 32 GiB ceiling idles at ~1–2 GiB host-visible.
