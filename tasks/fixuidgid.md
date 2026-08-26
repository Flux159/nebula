# nebula-slim loses file ownership

Found while moving **RagnarokMac** onto nebula-slim (the app whose requirements
are in [hostbindmounts.md](hostbindmounts.md)). Everything that document asked
for works; this is the one thing that stopped the stack from booting.

Three separate defects, all about uid/gid. The first is the serious one: it
silently rewrites every image the engine imports.

---

## 1. `docker load` and `docker pull` discard uid/gid — the blocker

Every file in an imported image arrives owned by `root:root`, whatever the
image says. Permission bits, mtimes and xattrs all survive; only ownership is
dropped.

**How it showed up.** RagnarokMac's MariaDB container would not start:

```
[ERROR] Can't start server : Bind on unix socket: Permission denied
[ERROR] Do you already have another server running on socket: /run/mysqld/init.sock ?
[ERROR] Aborting
```

The image does `mkdir -p /run/mysqld && chown -R mysql:mysql /run/mysqld`, and
`mariadbd` runs as `mysql`. Under slim that directory is `root:root`, so the
server cannot create its socket. Nothing in the error points at ownership,
which is what made it expensive to find — it reads like a stale socket or a
second server, and the same image on dockerd is fine.

**Minimal reproduction.**

```dockerfile
FROM alpine:3.23
RUN adduser -D -u 4242 appuser \
    && mkdir -p /chowned-dir && chown appuser:appuser /chowned-dir \
    && touch /chowned-file && chown 4242:4242 /chowned-file \
    && mkdir -p /setuid && cp /bin/busybox /setuid/bb && chmod 4755 /setuid/bb
COPY --chown=appuser:appuser payload.txt /copied-file
```

Build it with real docker, `docker save | docker-slim load`, then
`stat -c "%U:%G %a %n"` each path:

| path | dockerd | nebula-slim |
|---|---|---|
| `/chowned-dir` | `appuser:appuser 755` | **`root:root`** 755 |
| `/chowned-file` | `appuser:appuser 644` | **`root:root`** 644 |
| `/copied-file` | `appuser:appuser 644` | **`root:root`** 644 |
| `/setuid/bb` | `root:root 4755` | `root:root 4755` |

Modes are right in every row, including the setuid bit. Only the owner column
is wrong. And the failure is a real one, not cosmetic:

```
$ docker-slim run --rm -u 4242 uidgid-probe:1 sh -c 'touch /chowned-dir/x'
touch: /chowned-dir/x: Permission denied
```

A container running as a non-root user cannot write to the directory the image
gave it.

**Cause.** `slim/crates/slim-image/src/unpack.rs`, in `apply_layer`:

```rust
let mut ar = tar::Archive::new(reader);
ar.set_preserve_permissions(true);
ar.set_preserve_mtime(true);
ar.set_unpack_xattrs(cfg!(target_os = "linux"));
ar.set_overwrite(true);
```

The tar crate defaults `preserve_ownerships` to false and it is never turned
on, so `entry.unpack_in()` writes every entry as the calling user — slimd, i.e.
root. Modes and mtimes are preserved because those two lines ask for them.

**Scope.** Both importers, since both go through `apply_layer`:

- `docker load` — `slim-image/src/load.rs:275`
- `docker pull` — `slim-image/src/lib.rs:297`

`docker build` *inside* slim is not affected here: `commit_layer`
(`slim-build/src/lib.rs:451`) renames the overlay upper dir into the layer
store, so on-disk ownership is never round-tripped through a tar. That is why
`RUN … && chown` survives a slim-native build but not an import — and why this
did not show up in the appstack tests, which build in the engine.

**Fix.** Ask for ownership, guarded the way xattrs already are — slimd is root
in the guest, so the `chown` will succeed there, while a non-root host unpack
would fail on an unprivileged `chown`:

```rust
ar.set_preserve_ownerships(cfg!(target_os = "linux"));
```

Worth confirming the tar crate applies numeric uid/gid rather than resolving
the names in the tar's `uname`/`gname` fields against the *host's* passwd
database — layer tars carry both, and only the numeric ids are meaningful.

---

## 2. `COPY --chown=name:name` silently resolves to root

slim's classic builder accepts the flag and parses it, but only understands
numeric ids. `slim-build/src/lib.rs:977`:

```rust
let (uid, gid) = match spec.split_once(':') {
    Some((u, g)) => (u.parse().unwrap_or(0), g.parse().unwrap_or(0)),
    None => { let u = spec.parse().unwrap_or(0); (u, u) }
};
```

`"appuser".parse::<u32>()` fails, `unwrap_or(0)` turns that into root, and the
build succeeds with the wrong owner. Docker resolves names against the image's
own `/etc/passwd` and `/etc/group`.

Measured, building the same Dockerfile both ways:

| | dockerd | nebula-slim |
|---|---|---|
| `COPY --chown=4242:4242` | `appuser:appuser` | `appuser:appuser` ✓ |
| `COPY --chown=appuser:appuser` | `appuser:appuser` | **`root:root`** |

The silence is the problem: a parse failure that means "I could not do what you
asked" is being spelled "root". At minimum fail the build; better, resolve
names against the stage's `/etc/passwd` and `/etc/group`, which is what the
flag is for.

## 3. `COPY --chown` of a directory is not recursive

`apply_chown` calls `libc::chown` once on the destination path. For a directory
copy that sets the directory and leaves everything inside it root-owned:

```
appuser:appuser /numeric-tree
root:root       /numeric-tree/top.txt
root:root       /numeric-tree/sub/deep.txt
```

Docker applies the ownership to every copied entry. Walk the copied tree
instead of chowning its root.

---

## Suggested order

1. **`set_preserve_ownerships` in `apply_layer`** — one line, and it is the one
   that makes third-party images behave. Nothing else here matters as much.
2. **Fail or resolve on a non-numeric `--chown`** — small, and it removes a
   silent wrong answer.
3. **Recursive `--chown`** — least urgent; a Dockerfile can `RUN chown -R`.

## How to verify

The probe Dockerfile above is the whole test. As a regression case:

1. Build it with real docker, `save`, `load` into slim, and assert
   `/chowned-dir` is `appuser:appuser` — plus a `run -u 4242` that writes into
   it, so the check fails the way a user would experience it.
2. Build it with `docker-slim build` and assert the same for the `COPY --chown`
   paths, numeric and named, file and directory.

Case 1 belongs with the `docker load` coverage in `slim/test/appstack.sh`,
since that is the path RagnarokMac takes: images are baked on a developer
machine, shipped in the .app, and loaded on first launch. Nothing in that flow
ever builds inside the engine, so a build-only test would have kept missing it.

## Workaround in the meantime

Assert ownership at runtime in the entrypoint, where the process is still root
— which is what RagnarokMac's MariaDB image now does, and what the official
image has always done:

```sh
mkdir -p /run/mysqld
chown mysql:mysql /run/mysqld
```

That only works for images whose entrypoint starts as root. An image with a
`USER` line and no root-owned entrypoint has no way to repair itself.
