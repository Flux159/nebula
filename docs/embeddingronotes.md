# Field notes: embedding nebula-slim in a shipped app

Written August 2026, from building **Ragnarok Offline** — a cross-platform
desktop app (Electron) that runs a Ragnarok Online server and client entirely
offline. It ships nebula-slim as its container engine: two containers
(rAthena + MariaDB) on a private network, started on launch and torn down on
quit, on machines that have never had Docker installed.

This is the first embedder that shipped to real machines. Galaxy came first and
worked — nebula-slim under Tauri, on the author's Mac — but it was never
released and never run on Linux or Windows. So the surface it proved is the one
where the developer's machine is the only machine: no cold install, no second
Mac, no other platform, no user's Downloads folder. Most of what follows is
what that leaves untested.

`embedding.md` is the how-to. This is what actually cost time, and why. Where
something has since been fixed, it says so — the point is the shape of the
failure, which the next embedder will meet in a new form.

## Bugs this shipped app found

**Layer ownership was dropped on image import.** MariaDB's entrypoint creates
`/run/mysqld` and chowns it to `mysql`, and under slim it came back
root-owned, so the server could not create its socket. The image was correct;
the uid/gid did not survive `docker load`. Fixed in `slim: keep uid/gid through
image import and COPY --chown` and `slim: apply layer ownership by hand,
tolerating blank uid/gid`. The app still chowns at runtime in its entrypoint,
because it has to run on kits built before the fix.

**The Windows client could not find its own engine, or accept a Windows path.**
Two separate defects, each of which made a healthy engine look broken in a
different way. Written up in `tasks/windowsclientpaths.md`.

Given only `NEBULA_HOME`, `docker-slim` on Windows falls back to Docker's
default `tcp://127.0.0.1:2375` and reports a connection refused naming the
engine -- while the engine is up, with `agent healthy` and a live REST API in
its log. The port it should use sits in `run/docker.sock`, which on Windows is
not a socket but a text file holding the loopback port. `docs/slim-config.md`
says the CLIs resolve this themselves; they do not. An embedder must read that
file and pass `DOCKER_HOST` explicitly -- and must read it per call, because
the port is reassigned on every engine boot.

And `docker cp` refuses every absolute Windows path: it tells a local path from
a container path by looking for a colon, so `C:\Users\...` parses as container
`C` and the command sees two container paths. That one compounds badly, because
Windows has no virtiofs under nebula -- an embedder *cannot* bind mount and must
use `create` → `cp` → `start` to get config or seed data into a container. The
only supported route for the thing every embedder needs does not accept the only
kind of path the platform produces. Working around it means running from the
parent directory and passing a relative name.

The pattern in both: the error names something that is fine. "Cannot connect to
the engine" when the engine is healthy; "one of the paths must be a container
path" when one of them is. An embedder debugging either goes to the wrong place
first, and on a platform where nothing else has been proven yet, that is
expensive.

The general shape: **an image built against dockerd and loaded into slim is not
guaranteed to behave the same.** Anything the image relies on the runtime
preserving — ownership, modes, symlinks, xattrs — is worth an explicit
assertion in the entrypoint rather than an assumption.

**The embed kits disagreed on their own contents.** `scripts/embed-kit.sh`
(macOS) shipped `docker-slim`/`kubectl-slim`/`helm-slim` in `bin/`;
`embed-kit-linux.yml` and `embed-kit-windows.yml` hand-assembled and copied
only `nebula` + `nebulad`. So a kit shipping an engine that speaks the Docker
API shipped nothing to speak it with, and Linux/Windows embedders needed a
second download from `nebula-slim-clis-*` that macOS embedders did not. Fixed
by routing all three assemblers through `scripts/stage-slim-clis.sh`.

Worth keeping in mind for the next kit change: this held for months because
nothing tested it. The Linux workflow's own header claimed it mirrored
`embed-kit.sh`, which is a comment asserting the property that had drifted. The
fix added a real assertion — boot the assembled kit and drive it with the
kit's own `docker-slim` — which is the check that would have caught it.

## Correct behaviour that shapes the embedder's design

**`docker save` is not implemented, and cannot be.** Slim stores layers
unpacked, so the original layer tars no longer exist. This is documented, but
its consequence is larger than it looks: an app that wants to ship prebuilt
images has to **build them against a real docker daemon**, save the bundle at
build time, and `load` it on first launch. That splits the app's CI in two —
one pipeline for images, another for the app — and they cannot be merged.

**Saved image bundles are architecture-specific, and must match the guest.**
The guest arch follows the host, so an arm64 machine needs arm64 images. For a
cross-platform app this means the image pipeline runs on native runners per
arch, and the app build downloads the one matching its target rather than
building anything. Getting this wrong produces an app that installs cleanly and
then fails to start a container, on the user's machine, with an exec format
error.

**Kits must be release assets, not workflow artifacts.** An artifact needs an
Actions-scoped token and a run id to point at, which is a different fetch path
in every consumer and is unavailable to a person debugging by hand.
`gh release download` works identically from CI, a script and a shell. Both
nebula and this app's dependency repos publish kits to releases for this
reason.

**Fetching a private kit in CI spends a shared, invisible budget.** The kits
live in a private repo, so the app's CI authenticates with a personal access
token. GitHub's 5,000 requests/hour is per *user account*, pooled across every
one of that account's tokens -- so an app build competes with everything else
the account does, and a second token buys nothing. Ragnarok Offline's release
build failed on all three platforms with `HTTP 403: API rate limit exceeded`,
several times, for reasons entirely outside the build: an unrelated production
service was polling 400+ pull requests a minute on the same account and
draining the hour's budget before CI asked for anything.

Two things follow. Fetch a repository's *own* assets with the Actions run token
(`github.token`), which has a separate per-repository budget -- borrowing a
personal token for a same-repo download makes the build fail for reasons that
have nothing to do with it. And where a personal token is genuinely needed, for
a cross-repo private fetch, know that its budget is shared: a GitHub App
installation token is the only credential with a bucket of its own.

**libkrun is per-platform, and macOS does not use it.** Linux and Windows load
`lib/libkrun.so.1` / `lib/krun.dll` from next to `bin/`; macOS drives the
microVM through Virtualization.framework. The macOS kit still contains ~14 MB
of dylibs, which a mac app can and should exclude — the shipped app has never
carried one. An embedder copying `lib/` unconditionally silently inflates every
macOS build.

**No virtiofs on Windows means no host bind mounts.** This is in `EMBED.md`,
but it is an app-architecture constraint rather than a footnote: any design
that passes a host directory into a container needs a second path built on
volumes plus the archive API before it can claim to be cross-platform.

## Ship a compiled binary, not shell scripts

The single largest piece of rework in this project. Ragnarok Offline drove the
engine from 507 lines of `stack.sh` plus 84 of `link-assets.sh`, called by the
app through `/bin/bash`. That worked on macOS and Linux and could not run on
Windows at all — and the Windows installer built cleanly the whole time, which
made the app look far further along than it was. Building is not running.

What actually broke, none of it obvious from a Mac:

- **No `/bin/bash`.** The supervisor could not be invoked at all; the app died
  before any of its logic ran.
- **Host bind mounts are unavailable** (no virtiofs on Windows), so the
  container came up with no config, no schema and no NPC scripts. The
  replacement — `create`, `docker cp`, `start` — is a branch that has to live
  *somewhere*.
- **Symlinks need Developer Mode or elevation.** Linking a user's 3.5 GB
  client into the asset root became directory junctions and hard links.
- **`shasum` does not exist there**, only `sha256sum`; on macOS it is the
  reverse. Picking one broke a third of the build matrix.
- **`.exe` suffixes**, `pkill` vs `taskkill`, and a data directory that has a
  different conventional location on all three platforms.

The tempting fix is a second implementation — a PowerShell copy of the shell
script. Resist it. That is two things that must agree forever and eventually
will not, which is exactly the failure this repo spent a release fixing across
`embed-kit.sh`, `embed-kit-linux.yml` and `embed-kit-windows.yml`: three
assemblers of the same kit that quietly disagreed about its contents for
months.

One compiled binary instead. The port came to a small dependency-free Rust
crate — 379 KB, no runtime to ship, under a minute to build on every runner —
and every platform difference became a branch inside one codebase that is read
and changed as a unit. An embedder is already shipping `nebula`, `nebulad` and
`docker-slim`, so this adds no new toolchain and no new class of artifact.

Two things worth doing while porting:

- **Keep the same command surface.** The app invoked `up`/`down`/`status`/
  `logs`/`backup`/`restore`, wrote a phase file the UI polled, and printed a
  couple of lines the UI parsed. Holding all of that byte-identical meant the
  port could be verified against the shell version's behaviour rather than
  against a reading of what it was supposed to do.
- **Verify on the platform that already worked, first.** The real risk in this
  kind of port is not the new platform, it is regressing the proven one. Run
  the whole sequence — engine install, image load, database, servers, backup,
  teardown — and diff the generated config against what the shell produced.

The general rule: anything an embedder's app runs on a user's machine should
be a binary it ships. A shell script is a dependency on an interpreter, a
coreutils flavour and a filesystem semantics that one of your three platforms
does not have.

## Things that were our bugs, but every embedder will hit them

These are all "the Docker API behaves as specified, and the specification is
surprising". Recorded because each one reached a user first.

**Container names are not unique, so `rm -f <name>` is not reliable.** A
container that exited without being cleaned up keeps its name; a later
`rm -f` then fails with "multiple containers match" — the one call that could
clear the mess refuses to run. Resolve to ids and remove each:
`docker ps -aq --filter name=<name>`.

**Removal is asynchronous and the name stays taken briefly after the call
returns.** Creating the replacement inside that window fails with `Conflict.
The container name "/x" is already in use`, which reads like a different
problem entirely. Poll until the name is free.

**A container being up is not the service being ready.** The app's first
symptom was "the map is not available" on roughly one launch in three — the
client connected before the map server had finished loading. Every embedder
needs its own readiness gate; container state cannot express this.

**All state lives under `NEBULA_HOME`, including the container volumes.**
Moving the engine home to give the app its own instance moved the database
with it — and losing it is silent, because a fresh MariaDB initialises happily
from the schema and simply has no characters in it. Any change to `NEBULA_HOME`
in a shipped app needs an explicit migration, and it needs to run before first
boot rather than after.

**Concurrent `up`/`down` from a UI needs a lock.** The app can start the stack
from two places and tears it down on quit, so invocations overlap; both remove
the containers, then both create them, and the loser gets a name conflict. An
atomic `mkdir` is the portable primitive — `flock(1)` is not reliable on macOS.

## macOS packaging

Two things that are specific to shipping a signed mac app around a microVM,
and are not obvious from either side alone.

**The VZ entitlements must be on the binary that calls the framework.**
`com.apple.security.virtualization` and `.hypervisor` belong on `nebula` and
`nebulad`, not only on the app's main executable. Electron and Tauri both sign
every binary they find with the *app's* entitlements, so the sidecars must be
re-signed afterwards — and re-signing a nested binary breaks the bundle seal,
so the app has to be re-sealed after that. The app does this in an `afterSign`
hook that then asserts `nebulad` still carries the entitlement, because the
failure mode without the assertion is a bundle that ships and cannot boot a VM.

**A non-ASCII filename in a signed bundle can break it on copy.** The English
translation shipped 21 files whose names are CP949 bytes read as Latin-1. A
`.dmg` is HFS+ and `/Applications` is APFS, and the two normalise such names
differently -- precomposed in the image, decomposed once copied out. A code
signature records exact names, so 20 sealed resources stopped resolving and
macOS refused the app as "damaged".

The trap is where it hides: the bundle verifies perfectly *inside* the image,
so a check that mounts the .dmg and runs `codesign --verify` passes on a build
nobody can open. The symptom also points at the wrong thing -- "damaged" reads
as a notarisation problem, and the notarisation was fine. Any embedder shipping
game assets, translations or fonts on macOS is a candidate.

Two things follow. Ship such a tree as an archive with a plain ASCII name and
unpack it at first run, so what is signed cannot move and what moves is not
signed. And verify the app *after copying it out* of the image, not only
inside: that is the step that fails, and it is the step a user performs.

**Notarisation is not the last thing a build does unless you make it so.**
electron-builder signs, notarises, and *then* calls the `afterSign` hook. A
hook that re-signs a sidecar -- which an embedder needs, because nebula's
binaries want their own entitlements -- invalidates the ticket that was just
issued. The build log says notarisation succeeded and the shipped app reports
`Unnotarized Developer ID`. Sign nested binaries in `afterPack`, before the
app's own signature seals them, and tell the packager to leave them alone
(`mac.signIgnore`) so it cannot overwrite their entitlements.

**TCC prompts must be raised by the app process.** File-access permission
prompts are keyed to the bundle id and only appear for the process the user
launched. Anything the app shells out to inherits the denial without ever
producing a prompt, so paths that need user-granted access have to be read in
the app before a helper is spawned.

**Local-network access is a permission, and it is asked for at the worst
moment.** The first connection to a LAN address raises the macOS prompt -- and
that same connection is the one it blocks. For an app whose LAN feature is the
point, that means the first attempt fails, the second races the dialog being
answered, and the third works. Provoke it deliberately when the user enables
networking, next to the switch that needs it, rather than letting the game's
own first packet do it.

## What would have saved the most time

1. **A conformance test an embedder can run against a kit.** Boot it, load an
   image built by real docker, assert ownership and modes survived, start two
   containers on a network, restart, tear down. Most of the day-long debugging
   above was discovering that one narrow assumption did not hold.
2. **One assembler for kits, exercised on every platform.** Now true, via
   `stage-slim-clis.sh`.
3. **Saying explicitly, in `embedding.md`, that image bundles are per-arch and
   `save` needs real docker.** Both are discoverable; neither is stated where an
   embedder designing their CI would look.
