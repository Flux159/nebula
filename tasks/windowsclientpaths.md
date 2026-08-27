# Two Windows bugs in the slim client, found shipping a real app

Both hit while bringing Ragnarok Offline up on Windows for the first time.
Each is small; together they made a perfectly healthy engine look broken in
two different, misleading ways.

---

## 1. `docker-slim` does not find the engine from `NEBULA_HOME`

`docs/slim-config.md` says:

> On Windows (no AF_UNIX in std) the CLIs default to loopback TCP — nebula's
> WHP host proxy maps the guest vsock ports to `127.0.0.1`.

It does not. Given only `NEBULA_HOME`, the client falls back to Docker's
default endpoint:

```
> $env:NEBULA_HOME = "$env:APPDATA\Ragnarok Offline\nebula"
> docker-slim.exe ps
Error: Cannot connect to the slim engine at tcp://127.0.0.1:2375:
No connection could be made because the target machine actively refused it.
(os error 10061)
```

The port it should have used is sitting in the instance directory:

```
> Get-Content "$env:NEBULA_HOME\run\docker.sock"
63692
> $env:DOCKER_HOST = "tcp://127.0.0.1:63692"
> docker-slim.exe ps
CONTAINER ID   IMAGE   COMMAND   CREATED   STATUS   PORTS   NAMES     # works
```

On Windows `run/docker.sock` is not a socket, it is a text file holding the
loopback port `nebulad`'s proxy listens on. The client should read it when
`DOCKER_HOST` is unset and `NEBULA_HOME` is set, exactly as the unix path
derives `unix://$NEBULA_HOME/run/docker.sock`.

**Why it matters more than it looks.** The failure names the engine, so it
reads as "the engine is down" — and the engine was fine: it booted in 2.5s
with `agent healthy`, `socket proxy ready` and a live REST API. An embedder
sees their app hang on a startup step and goes looking in the wrong place.
The port is also reassigned on each boot, so an embedder cannot work around
it with a fixed environment variable; they have to read the file themselves,
which is the thing the client was supposed to do.

## 2. `docker cp` rejects any absolute Windows path

```
> docker-slim.exe cp C:\Users\me\state\sql ragnarok-db:/docker-entrypoint-initdb.d
Error: one of the paths must be a container path (container:path)
```

`cp` distinguishes a local path from a container path by looking for a colon.
Every absolute Windows path begins with one, so `C:\Users\...` parses as
container `C`, the command sees two container paths, and refuses.

Real docker special-cases a single-letter prefix as a drive letter. `parse_cp`
(or wherever the split happens in `slim/crates/slim-client`) needs the same:
treat `X:` as a drive when `X` is one ASCII letter and the next character is a
separator.

**Why it matters more than it looks.** Windows has no virtiofs under nebula,
so an embedder cannot bind mount and *must* use `cp` to get config, schema or
seed data into a container -- `create` → `cp` → `start`. That makes `cp` the
only supported path for the thing every embedder needs, and it does not accept
the only kind of path Windows produces.

The workaround is to run from the parent directory and pass a relative name,
which is what Ragnarok Offline does now. It should not be necessary.

---

## Suggested tests

Both are cheap to cover and neither is currently exercised:

- With `NEBULA_HOME` set and `DOCKER_HOST` unset on Windows, `docker-slim ps`
  reaches the engine.
- `docker-slim cp` accepts `C:\path\to\dir` in both directions, and still
  rejects a genuinely malformed pair.
