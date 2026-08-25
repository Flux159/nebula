# What nebula-slim needs to host a real app stack

Written from a concrete case: **RagnarokMac**, a self-contained macOS app that
runs a Ragnarok Online server and client. It embeds `nebula`, `nebulad` and
`docker-slim`, ships the guest kernel and rootfs, and needs no Docker install —
which is exactly the embedding story nebula-slim exists for.

It currently runs on **full nebula** (dockerd/containerd in the vessel). Slim is
the better fit on every axis except capability: the app is 486 MB installed and
334 MB compressed, of which **130 MB is the guest rootfs** carrying a Go
container stack the app barely uses. Slim's own README puts the engine at ~32 MB.
Switching would be the single biggest size win available.

What follows is everything the stack asks the engine for, from
`scripts/stack.sh` and `scripts/bootstrap.sh` in that project. Items are ordered
by how badly their absence hurts.

---

## 1. Host bind mounts of directories — the blocker

Every one of these is a host directory mounted into a container:

| Mount | Purpose |
|---|---|
| `<state>/conf` → `/rathena/conf/import:ro` | generated server config: rates, MOTD, DB host |
| `<state>/sql` → `/docker-entrypoint-initdb.d:ro` | schema, imported by MariaDB on first boot |
| `<state>/npc/kafras` → `/rathena/npc/kafras:ro` | rewritten NPC scripts (free teleport/storage) |

They are how the app configures the servers **without rebuilding images**. A
settings change rewrites a file on the host and restarts one container; without
bind mounts the alternative is rebuilding a 224 MB image every time someone
moves a slider, which is not a product.

Requirements:

- **Directory** binds, read-only and read-write.
- The source is an arbitrary host path, reachable through the existing
  `$HOME` virtiofs share.
- **Paths containing spaces must work.** The macOS standard location is
  `~/Library/Application Support/<bundle-id>/…`. This is not hypothetical: on
  full nebula today, a *single-file* bind from such a path fails —
  ```
  error mounting ".../state/conf/inter_conf.txt" to rootfs at
  "/rathena/conf/import/inter_conf.txt": not a directory
  ```
  and worse, the failed mount leaves a **directory** behind at the source path,
  which then shadows the file the app writes there. It reproduces with the real
  docker CLI, so it is not a docker-slim bug — but slim should not inherit it.
  Directory binds from the same path work, which is the workaround in use.
- **Single-file binds** would be welcome but are not required; the app moved to
  directory mounts deliberately.

## 2. Named volumes

`-v ragnarokmac-db:/var/lib/mysql` holds the player database — characters,
inventory, progress. It must survive container removal, engine restart and VM
reboot. It already does on full nebula, and it is the one piece of state whose
loss a user would actually feel.

Needs `volume create`/`ls`/`inspect` enough for the app to confirm it exists.

## 3. Container-to-container DNS

Four containers on one user-defined network address each other **by name**:

```
inter_conf.txt:  login_server_ip: ragnarok-db
char_conf.txt:   login_ip: ragnarok-login
map_conf.txt:    char_ip: ragnarok-char
```

Those names are written into config files, so they must resolve inside the
network. `network create` plus name resolution on a user-defined bridge.

## 4. Published ports bound to a host address

`-p 127.0.0.1:6900:6900` and friends. Two properties matter:

- The **host-IP part must be honoured**. `docker-slim` currently appears to
  ignore it — containers started with `-p 127.0.0.1:5121:5121` report
  `0.0.0.0:5121->5121/tcp`. For a single-player offline game the difference
  between "listening on loopback" and "listening on every interface the machine
  has" is a real one, and it is the sort of thing a user is entitled to assume.
- Long-lived connections must survive. The game socket stays open for the whole
  session. See `net.rs` — two bugs there were fixed while building this
  (forward teardown on a failed container listing, and loopback-scoped publishes
  dialling the wrong address); slim's path should not reintroduce them.

## 5. Lifecycle and inspection

Used on every start, stop and status poll:

- `run -d -t` — **`-t` is required**, not cosmetic. rAthena writes with
  `printf(3)`, which block-buffers when stdout is not a tty, so without a tty
  its errors never reach `logs` at all. This cost hours to diagnose.
- `rm -f`, `stop -t <n>` — graceful stop matters for the database.
- `inspect -f '{{.State.Status}}'` and `'{{.Id}}'` — the app polls these for its
  status panel and to wait for a container name to be released.
- `logs --tail N` — surfaced in the UI for diagnosis.
- `exec` — used as a readiness probe (`mariadb -e 'SELECT 1'`), because slim has
  no `--health-cmd` and polling for the thing you actually depend on is clearer
  anyway.
- `cp` (container → host) — extracts the SQL schema and the NPC scripts from the
  image so they can be edited and mounted back.
- `create` — a throwaway container purely as a `cp` source.

All of these work in docker-slim today except as noted.

## 6. Image load — currently missing

`docker save` / `docker load` are **not implemented**, and the app relies on
`load`: it ships `images.tar.gz` (135 MB: rAthena + MariaDB) and loads it on
first run so a fresh machine needs no registry. `save` only runs on a developer
machine and can stay a real-docker dependency; **`load` is on the user path**
and has no alternative short of pulling from a network the app is designed not
to need.

## 7. Not needed

Stated so scope does not creep: no Kubernetes, no Helm, no compose, no
multi-arch manifests, no registry auth, no BuildKit. `build` is used once on a
developer machine (classic builder is fine) and never by an end user.

---

## Suggested order

1. **Directory bind mounts**, spaces included. Nothing else matters until this
   works — it is what makes configuration possible without image rebuilds.
2. **`docker load`**. Without it a packaged app cannot install its own images.
3. **Host-IP-scoped port publishing.** Correctness and a reasonable security
   expectation.
4. Named volumes, container DNS, `exec`/`cp`/`logs`/`inspect` parity — mostly
   present; needs confirming against this list.

## How to verify

RagnarokMac is the test rig. `scripts/stack.sh` drives everything above and
takes `RAGNAROKMAC_DOCKER=<path to client>`, so pointing it at a slim build
exercises the whole surface in one command:

```
RAGNAROKMAC_DOCKER=/path/to/docker-slim scripts/stack.sh up
scripts/stack.sh status     # expect four containers Up
```

If that comes up and the game connects, slim can host the stack.
