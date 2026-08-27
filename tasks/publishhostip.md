# Published ports always bind 127.0.0.1, ignoring HostIp

> **RESOLVED (2026-08-27).** `ContainerInfo.tcp_ports` now carries a
> `PublishedPort { port, guest_loopback, host_ip }` — the two jobs the bool was
> doing are separate fields, so target selection is untouched — and
> `spawn_port_forward` binds the host address. Reconciliation compares it, so a
> container republished from `127.0.0.1` to `0.0.0.0` gets a new listener.
>
> On the "worth deciding deliberately" question: **honour it, gated on
> config**, the middle option. `allow_public_publish` (also
> `NEBULA_ALLOW_PUBLIC_PUBLISH=1`) defaults to false, which keeps today's
> behaviour exactly. Reason for not simply honouring the publish: docker
> reports `0.0.0.0` for a bare `-p 8080:80`, so honouring unconditionally would
> silently put every existing user's published ports on the network on upgrade.
> That is the same posture `api_host` already takes.
>
> Verified against a second isolated instance, live engine untouched: with the
> flag on, nebulad listens `*:8499` and the LAN address answers 200; with it
> off, the same container and publish spec give `127.0.0.1:8499` and the LAN
> address is refused.

`docker run -p 0.0.0.0:6900:6900` publishes on loopback anyway, so a container
in a nebula microVM cannot be reached from another machine. There is no way to
host anything on a LAN.

Found while building LAN play for Ragnarok Offline: the game's login, char and
map servers are published with an explicit `0.0.0.0` host IP, and `lsof` shows
nebulad listening on `127.0.0.1` for all three.

## Where

`crates/nebulad/src/net.rs:184`

```rust
fn spawn_port_forward(port: u16, target: ForwardTarget, stop: Arc<AtomicBool>) -> bool {
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
```

The bind address is a literal. Nothing above it can influence the choice,
whatever the container asked for.

## The information is already there and is being discarded

`containers()` (same file, ~line 383) parses the host IP out of the Docker API
response and immediately reduces it to a boolean:

```rust
let host_ip = p["IP"].as_str().unwrap_or_default();
let loopback = matches!(host_ip, "::1") || host_ip.starts_with("127.");
```

`ContainerInfo.tcp_ports` is `Vec<(u16, bool)>`, so the address string is gone
by the time anything could act on it. docker-slim is not at fault — it parses
and forwards `HostIp` correctly, and has a test (`host_ip_is_preserved`) that
says so.

## Careful: that bool is doing two unrelated jobs

`loopback` currently decides the forward *target* — a port that dockerd bound
on the guest's own 127.0.0.1 must be reached through the vsock proxy rather
than dialled at the guest's NAT address, per the comment at net.rs:~128. That
is a statement about the **guest** side.

The host bind address is a statement about the **host** side. They happen to
coincide today because the same publish spec produces both. Replacing the bool
outright would break target selection; the fix is to carry the host IP *as
well*, not instead.

## Suggested shape

- `tcp_ports: Vec<(u16, bool)>` becomes `Vec<(u16, bool, IpAddr)>`, or a small
  struct — the tuple is already at the limit of what reads clearly.
- `spawn_port_forward` takes the host IP and binds it.
- The forwarder reconciliation at net.rs:~147 already compares `was_loopback`
  to detect a changed publish; it needs to compare the address too, or a
  container republished from `127.0.0.1` to `0.0.0.0` will keep its old
  loopback-bound forwarder.

## Worth deciding deliberately

Binding a guest's port to `0.0.0.0` exposes it to the network, which is a real
security boundary and not one an embedder should be able to cross by accident.
Right now it cannot be crossed at all, which is the safe end of the wrong
trade. Options, in increasing strictness: honour whatever the publish says;
honour it only when nebula's own config opts in (`allow_public_publish`);
require both. An embedder shipping a game to players wants the first or second
— the current behaviour means a LAN feature cannot be built on nebula at all.

## Reproducing

```sh
nebula docker run -d -p 0.0.0.0:8080:80 --name t nginx
lsof -nP -iTCP -sTCP:LISTEN | grep 8080     # 127.0.0.1:8080, expected 0.0.0.0
```
