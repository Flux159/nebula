# slim engine configuration (environment variables)

`slimd` (the guest engine) and the host CLIs are configured entirely by
environment variables — no config file. This is the full set.

## slimd (engine, runs in the guest)

| Var | Default | Purpose |
|---|---|---|
| `SLIM_SOCKET` | `/var/run/docker.sock` | Docker Engine API unix socket path. |
| `SLIM_DATA` | `/var/lib/nebula/slim` | Engine data root: containers, volumes, images. Must be a real (non-overlay) filesystem. |
| `SLIM_RUN_DIR` | `/run/slim` | Transient overlay mounts (container rootfs). |
| `NEBULA_IMAGES_DIR` | `$SLIM_DATA/images` | **Image/layer store location** (engine-neutral, shared by Nebula and Nebula-slim — see ["Shared image cache across engines"](#shared-image-cache-across-engines)). Point several slim engines at one shared, persistent dir for a **shared image cache** — pull-once, reuse-many, offline after warm-up. Must be a real (non-overlay) fs. |
| `SLIM_REGISTRY_MIRROR` | (unset) | Pull-through **registry mirror** for `docker.io` (e.g. `mirror.gcr.io` or a local `registry:2`). Redirects only the *network host* — images still tag/resolve under their original name. Use for offline/corporate mirrors or to avoid Docker Hub anonymous pull rate limits in CI. |
| `SLIM_KUBE_API` | (on) | Set to `off` to disable the Kubernetes apiserver-lite + controller bridge. |
| `SLIM_KUBE_API_ADDR` | `0.0.0.0:6443` | TLS apiserver listen address (in-pod operators). |
| `SLIM_KUBE_SOCKET` | `<dir of SLIM_SOCKET>/slim-kube.sock` | Plain-HTTP apiserver unix socket for host clients (kubectl-slim/helm-slim via nebula's socket proxy). |
| `SLIM_PAUSE_BIN` | (auto) | Path to the static pod-sandbox `pause` binary. Auto-located next to `slimd` and at `/usr/local/share/slim/pause`; override if elsewhere. If absent, pod sandboxes fall back to the app image + `sleep`. |

## Host CLIs (docker-slim / kubectl-slim / helm-slim)

| Var | Purpose |
|---|---|
| `DOCKER_HOST` | `unix:///path` (unix) or `tcp://host:port` (any platform, incl. Windows). |
| `SLIM_SOCKET` | Docker API endpoint override (unix path or `tcp://...`). |
| `SLIM_KUBE_SOCKET` / `SLIM_KUBE_HOST` | Apiserver endpoint for kubectl-slim/helm-slim (unix path or `tcp://...`). |
| `NEBULA_HOME` | Instance root; CLIs discover `$NEBULA_HOME/run/{docker,slim-kube}.sock`. |

On Windows (no AF_UNIX in std) the CLIs default to loopback TCP — nebula's WHP
host proxy maps the guest vsock ports to `127.0.0.1`.

## Shared image cache across engines

The image store is content-addressed per layer (`layers/<diff_id>` shared across
images, `blobs/sha256/<digest>`, `.complete` markers) — like docker. By default
each engine has its own store under `$SLIM_DATA/images`, so separate engines
re-pull the same images.

Point multiple engines at **one** `NEBULA_IMAGES_DIR` to share that cache:

```sh
# engine A and engine B both:
export NEBULA_IMAGES_DIR=/srv/nebula-imgcache   # a shared, persistent, real fs
```

The first engine to pull an image populates the cache; every other engine then
**reuses the layers with no pull** (works fully offline once warm). When
`NEBULA_IMAGES_DIR` is set, `db.json` reads/writes are made cross-process safe
(an exclusive `flock` + reload-modify-write, and reads refresh from disk), so
concurrent engines sharing the cache don't clobber each other's image metadata.
Layers and blobs are content-addressed and idempotent, so they're always safe to
share. Constraint: the shared dir must be a real (non-overlay) filesystem.

Combine with `SLIM_REGISTRY_MIRROR` so even the cold first pull avoids Docker Hub
rate limits — that's exactly what the test harness does (`test/run-all.sh`).

### Works for both Nebula and Nebula-slim

`NEBULA_IMAGES_DIR` is deliberately engine-neutral (not `SLIM_*`): it names the
*one* image store both engines read, so the knob means the same thing whichever
engine you're driving. The honest caveat is that the two engines honor it
differently because their stores are built differently:

- **Nebula-slim** uses a content-addressed, per-layer store that is *designed*
  for concurrent sharing (flock + reload-modify-write on `db.json`, idempotent
  layers/blobs). Pointing N slim engines at one `NEBULA_IMAGES_DIR` gives a true
  pull-once / reuse-many cache, safe across processes — as described above.
- **Full Nebula** ships the real Docker engine, whose `data-root` is a single
  monolithic tree (images + containers + volumes together) — dockerd has no way
  to relocate *only* images, and is not designed to share its data-root between
  concurrent daemons. `NEBULA_IMAGES_DIR` would therefore have to move the whole
  store and still couldn't be shared, so it is intentionally a **no-op on full
  Nebula** rather than a knob that quietly means something different from its
  name. Cross-engine image reuse on full Nebula is instead the docker-native
  way: a pull-through registry mirror (a `registry:2` / `mirror.gcr.io`). The
  *config surface* differs per engine — slim reads `SLIM_REGISTRY_MIRROR`, full
  Nebula's dockerd reads `registry-mirrors` in its `daemon.json` — but the
  effect is the same registry-layer cache.

Net: `NEBULA_IMAGES_DIR` is the store-layer shared cache, and it only does
something on slim (the docs say so rather than pretending otherwise). A
pull-through registry mirror is the registry-layer cache available on both
engines, configured per-engine (`SLIM_REGISTRY_MIRROR` for slim,
`registry-mirrors` in `daemon.json` for full Nebula). The `NEBULA_IMAGES_DIR`
name stays engine-neutral so the knob can grow a full-Nebula meaning later if
dockerd ever gains a separable image store.
