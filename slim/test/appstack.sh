#!/bin/sh
# Runs INSIDE a privileged alpine container in a nebula engine microVM (same
# harness as smoke.sh; see scripts/test-slim.sh).
#
# Covers what a real packaged app stack asks the engine for, from
# tasks/hostbindmounts.md: host directory bind mounts (read-only, read-write,
# paths WITH SPACES), named volumes that outlive their container, image
# VOLUMEs, container-to-container DNS, host-IP-scoped port publishing, tty
# containers, `docker load` of a saved image archive, and the uid/gid an
# imported or built image carries (tasks/fixuidgid.md).
#
# House style: capture-then-grep, never `cmd | grep -q`.
set -u
mkdir -p /var/lib/nebula
mount -t tmpfs tmpfs /var/lib/nebula 2>/dev/null
export SLIM_DATA=/var/lib/nebula/slim
export SLIM_RUN_DIR=/var/lib/nebula/run
export SLIM_SOCKET=/var/run/docker.sock
export DOCKER_HOST=unix:///var/run/docker.sock
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "PASS: $1"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $1"; }
DS=/slim/docker-slim
IMG=${APPSTACK_IMAGE:-alpine:3.19}

# Poll instead of sleeping: on a loaded CI runner a fixed sleep is either
# flaky or slow, and every server below needs a moment to bind its socket.
wait_running() {
    for _ in $(seq 1 60); do
        [ "$($DS inspect -f '{{.State.Running}}' "$1" 2>/dev/null)" = "true" ] && return 0
        sleep 0.25
    done
    return 1
}
# retry_out <tries> <pattern> <cmd...> — run cmd until its output matches.
retry_out() {
    _n=$1; _pat=$2; shift 2
    while [ "$_n" -gt 0 ]; do
        "$@" >/tmp/o 2>&1
        grep -q "$_pat" /tmp/o && return 0
        _n=$((_n - 1))
        sleep 0.5
    done
    return 1
}

echo "== booting slimd =="
/slim/slimd > /tmp/slimd.log 2>&1 &
SLIMD_PID=$!
for i in $(seq 1 50); do [ -S "$SLIM_SOCKET" ] && break; sleep 0.1; done
[ -S "$SLIM_SOCKET" ] || { bad "slimd socket"; cat /tmp/slimd.log; exit 1; }

$DS pull "$IMG" >/tmp/o 2>&1 || { bad "pull $IMG"; cat /tmp/o; exit 1; }

# ---------------------------------------------------------------- bind mounts
# The macOS location an app actually uses has spaces in it.
STATE="/tmp/Application Support/nebula.appstack"
mkdir -p "$STATE/conf" "$STATE/sql"
printf 'motd: hello\n' > "$STATE/conf/inter_conf.txt"
printf 'CREATE TABLE t(i INT);\n' > "$STATE/sql/schema.sql"

echo "== bind: read-only directory whose path contains spaces =="
$DS run --rm -v "$STATE/conf:/conf:ro" "$IMG" cat /conf/inter_conf.txt >/tmp/o 2>&1
grep -q "motd: hello" /tmp/o && ok "ro bind readable (spaces in path)" || { bad "ro bind readable"; cat /tmp/o; }

$DS run --rm -v "$STATE/conf:/conf:ro" "$IMG" sh -c 'echo x > /conf/nope' >/tmp/o 2>&1
[ $? -ne 0 ] && ok "ro bind rejects writes" || bad "ro bind rejects writes"
[ -f "$STATE/conf/nope" ] && bad "ro bind leaked a write to the host" || ok "host dir untouched by ro bind"

echo "== bind: read-write directory =="
$DS run --rm -v "$STATE/sql:/initdb" "$IMG" sh -c 'echo written-by-container > /initdb/out.txt' >/tmp/o 2>&1
grep -q "written-by-container" "$STATE/sql/out.txt" 2>/dev/null && ok "rw bind writes reach the host" || { bad "rw bind writes reach the host"; cat /tmp/o; }

echo "== bind: several directories at once, one of them missing =="
rm -rf "$STATE/npc"
$DS run --rm \
    -v "$STATE/conf:/conf:ro" \
    -v "$STATE/sql:/docker-entrypoint-initdb.d:ro" \
    -v "$STATE/npc:/npc" \
    "$IMG" sh -c 'cat /conf/inter_conf.txt; ls /docker-entrypoint-initdb.d; echo made > /npc/f' >/tmp/o 2>&1
grep -q "schema.sql" /tmp/o && ok "three binds in one container" || { bad "three binds in one container"; cat /tmp/o; }
[ -d "$STATE/npc" ] && grep -q made "$STATE/npc/f" 2>/dev/null && ok "missing bind source created as a directory" || bad "missing bind source created"

echo "== bind: --mount type=bind =="
$DS run --rm --mount "type=bind,source=$STATE/conf,target=/conf,readonly" "$IMG" cat /conf/inter_conf.txt >/tmp/o 2>&1
grep -q "motd: hello" /tmp/o && ok "--mount type=bind (spaces + readonly)" || { bad "--mount type=bind"; cat /tmp/o; }
$DS run --rm --mount "type=bind,source=$STATE/does-not-exist,target=/x" "$IMG" true >/tmp/o 2>&1
[ $? -ne 0 ] && ok "--mount type=bind refuses a missing source" || bad "--mount type=bind refuses a missing source"

echo "== inspect reports mounts =="
$DS run -d --name as-mounts -v "$STATE/conf:/conf:ro" "$IMG" sleep 30 >/tmp/o 2>&1
wait_running as-mounts || true
$DS inspect -f '{{range .Mounts}}{{.Type}} {{.Destination}} {{.RW}}{{end}}' as-mounts >/tmp/o 2>&1
grep -q "bind /conf false" /tmp/o && ok "inspect .Mounts shows the ro bind" || { bad "inspect .Mounts"; cat /tmp/o; }
$DS rm -f as-mounts >/dev/null 2>&1

# ------------------------------------------------------------- named volumes
echo "== named volume outlives its container =="
$DS volume create appstack-db >/tmp/o 2>&1 && ok "volume create" || { bad "volume create"; cat /tmp/o; }
$DS run -d --name as-db -v appstack-db:/var/lib/mysql "$IMG" sleep 30 >/tmp/o 2>&1
wait_running as-db || { bad "as-db never reached running"; tail -5 /tmp/slimd.log; }
$DS exec as-db sh -c 'echo player-row > /var/lib/mysql/data' >/tmp/o 2>&1 && ok "write into named volume" || { bad "write into named volume"; cat /tmp/o; }
$DS rm -f as-db >/dev/null 2>&1
$DS run --rm -v appstack-db:/var/lib/mysql "$IMG" cat /var/lib/mysql/data >/tmp/o 2>&1
grep -q "player-row" /tmp/o && ok "volume data survives container removal" || { bad "volume data survives"; cat /tmp/o; }
$DS volume inspect appstack-db >/tmp/o 2>&1
grep -q "appstack-db" /tmp/o && ok "volume inspect" || { bad "volume inspect"; cat /tmp/o; }
$DS volume ls >/tmp/o 2>&1
grep -q "appstack-db" /tmp/o && ok "volume ls" || { bad "volume ls"; cat /tmp/o; }

echo "== image VOLUME gets an anonymous volume, seeded from the image =="
mkdir -p /tmp/volctx
printf 'FROM %s\nRUN mkdir -p /seed && echo from-image > /seed/f\nVOLUME /seed\nCMD ["true"]\n' "$IMG" > /tmp/volctx/Dockerfile
$DS build -t appstack-vol:1 /tmp/volctx >/tmp/o 2>&1 || { bad "build VOLUME image"; tail -5 /tmp/o; }
$DS run --rm appstack-vol:1 cat /seed/f >/tmp/o 2>&1
grep -q "from-image" /tmp/o && ok "image VOLUME seeded from image content" || { bad "image VOLUME seeded"; cat /tmp/o; }

# ------------------------------------------------------------- container DNS
echo "== container-to-container DNS on a user network =="
$DS network create appstack-net >/tmp/o 2>&1 && ok "network create" || { bad "network create"; cat /tmp/o; }
$DS run -d --name as-login --network appstack-net "$IMG" \
    sh -c 'while true; do echo login-ok | nc -l -p 6900; done' >/tmp/o 2>&1
wait_running as-login || { bad "as-login never reached running"; tail -5 /tmp/slimd.log; }
retry_out 10 "1 packets received" \
    $DS run --rm --network appstack-net "$IMG" ping -c1 -W2 as-login \
    && ok "resolve peer by container name" || { bad "resolve peer by name"; cat /tmp/o; }
retry_out 10 "login-ok" \
    $DS run --rm --network appstack-net "$IMG" nc -w2 as-login 6900 \
    && ok "connect to peer by name (the config-file case)" || { bad "connect to peer by name"; cat /tmp/o; }
$DS run -d --name as-char --network appstack-net --network-alias ragnarok-char "$IMG" sleep 30 >/tmp/o 2>&1
wait_running as-char || true
retry_out 10 "1 packets received" \
    $DS run --rm --network appstack-net "$IMG" ping -c1 -W2 ragnarok-char \
    && ok "resolve peer by --network-alias" || { bad "resolve by alias"; cat /tmp/o; }

# ------------------------------------------------------------------- ports
echo "== published port bound to a host address =="
$DS run -d --name as-web -p 127.0.0.1:18081:80 "$IMG" \
    sh -c 'while true; do echo hi-loopback | nc -l -p 80; done' >/tmp/o 2>&1
wait_running as-web || { bad "as-web never reached running"; tail -5 /tmp/slimd.log; }
$DS ps >/tmp/o 2>&1
grep -q "127.0.0.1:18081" /tmp/o && ok "ps reports the host address, not 0.0.0.0" || { bad "ps host address"; cat /tmp/o; }
$DS port as-web >/tmp/o 2>&1
grep -q "127.0.0.1:18081" /tmp/o && ok "docker port reports the host address" || { bad "docker port"; cat /tmp/o; }
$DS inspect -f '{{(index .NetworkSettings.Ports "80/tcp" 0).HostIp}}' as-web >/tmp/o 2>&1
grep -qx "127.0.0.1" /tmp/o && ok "inspect reports the host address" || { bad "inspect host address"; cat /tmp/o; }
retry_out 10 "hi-loopback" nc -w5 127.0.0.1 18081 \
    && ok "loopback publish is reachable on 127.0.0.1" \
    || { bad "loopback publish reachable"; cat /tmp/o; tail -5 /tmp/slimd.log; }
# Scoped to loopback means NOT DNAT'd onto every address.
iptables -t nat -S SLIM >/tmp/o 2>&1
grep -q "dport 18081" /tmp/o && bad "loopback publish leaked a wildcard DNAT rule" || ok "no wildcard DNAT for a loopback publish"

echo "== wildcard publish still reaches every address =="
$DS run -d --name as-web2 -p 18082:80 "$IMG" \
    sh -c 'while true; do echo hi-any | nc -l -p 80; done' >/tmp/o 2>&1
wait_running as-web2 || { bad "as-web2 never reached running"; tail -5 /tmp/slimd.log; }
retry_out 10 "hi-any" nc -w5 127.0.0.1 18082 \
    && ok "wildcard publish reachable on loopback" || { bad "wildcard publish loopback"; cat /tmp/o; }
iptables -t nat -S SLIM >/tmp/o 2>&1
grep -q "dport 18082" /tmp/o && ok "wildcard publish installs a DNAT rule" || { bad "wildcard DNAT rule"; cat /tmp/o; }

echo "== a long-lived connection survives =="
# The game socket stays open for a whole session: hold one for 6s (three
# reconcile ticks on the host side) and check it is still carrying bytes.
( { sleep 6; echo ping; } | nc -w8 127.0.0.1 18082 > /tmp/longconn ) 2>/dev/null
grep -q "hi-any" /tmp/longconn && ok "connection held open for 6s still served" || { bad "long-lived connection"; cat /tmp/longconn; }

# ------------------------------------------------------------ tty + lifecycle
echo "== run -d -t allocates a tty =="
$DS run -d -t --name as-tty "$IMG" sh -c 'printf "unflushed-no-newline"; sleep 30' >/tmp/o 2>&1
wait_running as-tty || { bad "as-tty never reached running"; tail -5 /tmp/slimd.log; }
$DS inspect -f '{{.Config.Tty}}' as-tty >/tmp/o 2>&1
grep -q "true" /tmp/o && ok "inspect .Config.Tty" || { bad "inspect Tty"; cat /tmp/o; }
retry_out 10 "unflushed-no-newline" $DS logs as-tty \
    && ok "tty makes unflushed output visible in logs" || { bad "tty logs"; cat /tmp/o; }
$DS inspect -f '{{.State.Status}}' as-tty >/tmp/o 2>&1
grep -q "running" /tmp/o && ok "inspect .State.Status" || { bad "inspect Status"; cat /tmp/o; }
$DS inspect -f '{{.Id}}' as-tty >/tmp/o 2>&1
[ -s /tmp/o ] && ok "inspect .Id" || bad "inspect .Id"
$DS logs --tail 1 as-tty >/tmp/o 2>&1 && ok "logs --tail" || { bad "logs --tail"; cat /tmp/o; }
$DS stop -t 2 as-tty >/tmp/o 2>&1 && ok "stop -t" || { bad "stop -t"; cat /tmp/o; }
$DS rm -f as-tty >/dev/null 2>&1

echo "== create + cp (container -> host) =="
$DS create --name as-src "$IMG" true >/tmp/o 2>&1 && ok "create" || { bad "create"; cat /tmp/o; }
$DS cp as-src:/etc/alpine-release /tmp/from-container >/tmp/o 2>&1
[ -s /tmp/from-container ] && ok "cp container -> host" || { bad "cp container->host"; cat /tmp/o; }
$DS rm -f as-src >/dev/null 2>&1

# --------------------------------------------------------------------- load
if [ -f /slim/load-image.tar ]; then
    echo "== docker load =="
    $DS load -i /slim/load-image.tar >/tmp/o 2>&1
    grep -q "Loaded image" /tmp/o && ok "load from a docker save archive" || { bad "load"; cat /tmp/o; }
    LOADED=$(sed -n 's/^Loaded image: //p' /tmp/o | head -1)
    if [ -n "$LOADED" ]; then
        $DS images >/tmp/o 2>&1
        grep -q "$(echo "$LOADED" | cut -d: -f1)" /tmp/o && ok "loaded image is listed" || { bad "loaded image listed"; cat /tmp/o; }
        $DS run --rm "$LOADED" true >/tmp/o 2>&1 && ok "run the loaded image" || { bad "run loaded image"; cat /tmp/o; }
    fi
    if [ -f /slim/load-image.tar.gz ]; then
        $DS load -i /slim/load-image.tar.gz >/tmp/o 2>&1
        grep -q "Loaded image" /tmp/o && ok "load from a gzipped archive" || { bad "load gzipped"; cat /tmp/o; }
    fi
    # stdin form, as `docker load < file` uses
    $DS load < /slim/load-image.tar >/tmp/o 2>&1
    grep -q "Loaded image" /tmp/o && ok "load from stdin" || { bad "load stdin"; cat /tmp/o; }
else
    echo "SKIP: no /slim/load-image.tar staged — docker load not exercised"
fi

# ----------------------------------------------------------------- uid/gid
# An imported image must keep the ownership it was built with. Getting this
# wrong is silent: modes survive, only the owner column is rewritten to root,
# and the failure surfaces much later as a daemon that cannot write its own
# runtime dir (tasks/fixuidgid.md).
if [ -f /slim/uidgid-image.tar ]; then
    echo "== docker load preserves uid/gid =="
    $DS load -i /slim/uidgid-image.tar >/tmp/o 2>&1
    PROBE=$(sed -n 's/^Loaded image: //p' /tmp/o | head -1)
    if [ -z "$PROBE" ]; then
        bad "load uid/gid probe"; cat /tmp/o
    else
        $DS run --rm "$PROBE" stat -c "%U:%G %a %n" \
            /chowned-dir /chowned-file /copied-file /setuid/bb >/tmp/o 2>&1
        grep -q "appuser:appuser 755 /chowned-dir"  /tmp/o && ok "RUN chown survives load"        || { bad "RUN chown survives load"; cat /tmp/o; }
        grep -q "appuser:appuser 644 /chowned-file" /tmp/o && ok "numeric chown survives load"    || { bad "numeric chown survives load"; cat /tmp/o; }
        grep -q "appuser:appuser 644 /copied-file"  /tmp/o && ok "COPY --chown survives load"     || { bad "COPY --chown survives load"; cat /tmp/o; }
        grep -q "root:root 4755 /setuid/bb"         /tmp/o && ok "setuid bit survives load"       || { bad "setuid bit survives load"; cat /tmp/o; }
        # The check that fails the way a user experiences it.
        $DS run --rm -u 4242 "$PROBE" sh -c 'touch /chowned-dir/x' >/tmp/o 2>&1
        [ $? -eq 0 ] && ok "non-root user can write its own dir" || { bad "non-root user can write its own dir"; cat /tmp/o; }
    fi
else
    echo "SKIP: no /slim/uidgid-image.tar staged — load ownership not exercised"
fi

echo "== COPY --chown in the engine's own builder =="
CTX=/tmp/chown-ctx
mkdir -p "$CTX/tree/sub"
printf 'payload\n' > "$CTX/payload.txt"
printf 'top\n' > "$CTX/tree/top.txt"
printf 'deep\n' > "$CTX/tree/sub/deep.txt"
cat > "$CTX/Dockerfile" <<EOF
FROM $IMG
RUN adduser -D -u 4242 appuser
COPY --chown=4242:4242 payload.txt /numeric-file
COPY --chown=appuser:appuser payload.txt /named-file
COPY --chown=appuser:appuser tree /named-tree
EOF
$DS build -t chown-probe:1 "$CTX" >/tmp/o 2>&1
if grep -q "Successfully tagged" /tmp/o; then
    ok "build with --chown"
    $DS run --rm chown-probe:1 stat -c "%U:%G %n" \
        /numeric-file /named-file /named-tree /named-tree/top.txt /named-tree/sub/deep.txt >/tmp/o 2>&1
    grep -q "appuser:appuser /numeric-file"            /tmp/o && ok "--chown numeric"          || { bad "--chown numeric"; cat /tmp/o; }
    grep -q "appuser:appuser /named-file"              /tmp/o && ok "--chown by name"          || { bad "--chown by name"; cat /tmp/o; }
    grep -q "appuser:appuser /named-tree"              /tmp/o && ok "--chown dir root"         || { bad "--chown dir root"; cat /tmp/o; }
    grep -q "appuser:appuser /named-tree/top.txt"      /tmp/o && ok "--chown is recursive"     || { bad "--chown is recursive"; cat /tmp/o; }
    grep -q "appuser:appuser /named-tree/sub/deep.txt" /tmp/o && ok "--chown recurses deeply"  || { bad "--chown recurses deeply"; cat /tmp/o; }
else
    bad "build with --chown"; cat /tmp/o
fi
# An unresolvable name must fail the build, not quietly mean root.
cat > "$CTX/Dockerfile" <<EOF
FROM $IMG
COPY --chown=nosuchuser:nosuchgroup payload.txt /f
EOF
$DS build -t chown-bad:1 "$CTX" >/tmp/o 2>&1
if [ $? -ne 0 ]; then
    grep -q "no such user" /tmp/o && ok "unknown --chown name fails the build" || { bad "unknown --chown name: wrong error"; cat /tmp/o; }
else
    bad "unknown --chown name silently succeeded"; cat /tmp/o
fi
$DS rmi chown-probe:1 >/dev/null 2>&1
rm -rf "$CTX"

$DS rm -f as-login as-char as-web as-web2 >/dev/null 2>&1
$DS network rm appstack-net >/dev/null 2>&1
$DS volume rm appstack-db >/dev/null 2>&1

echo ""
echo "RESULT: $PASS passed, $FAIL failed"
kill $SLIMD_PID 2>/dev/null
[ $FAIL -eq 0 ]
