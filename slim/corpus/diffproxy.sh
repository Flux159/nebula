#!/bin/sh
# diffproxy.sh — STUB, not load-bearing. Record proxy for docker API traffic.
#
# Intended use: MITM the unix socket between client and daemon, teeing both
# directions to files, so the same corpus run can be recorded against real
# dockerd and against slimd and the raw HTTP exchanges diffed:
#
#   ./diffproxy.sh /tmp/rec.sock "$HOME/.nebula/run/docker.sock" rec-real/
#   DOCKER_HOST=unix:///tmp/rec.sock ./run.sh
#   ./diffproxy.sh /tmp/rec.sock /tmp/slimd.sock rec-slim/
#   DOCKER_HOST=unix:///tmp/rec.sock ./run.sh
#   diff -u rec-real/c2s.raw rec-slim/c2s.raw   # (after normalizing ids)
#
# Caveats: one shared capture file across forked connections (interleaving),
# no per-request framing, breaks on connection hijack (attach). Good enough
# to eyeball; a real recorder should frame per-connection per-request.
LISTEN=${1:?usage: diffproxy.sh <listen.sock> <upstream.sock> [recdir]}
UPSTREAM=${2:?usage: diffproxy.sh <listen.sock> <upstream.sock> [recdir]}
RECDIR=${3:-rec}
mkdir -p "$RECDIR"
exec socat UNIX-LISTEN:"$LISTEN",fork,unlink-early \
    SYSTEM:"tee -a '$RECDIR/c2s.raw' | socat - UNIX-CONNECT:'$UPSTREAM' | tee -a '$RECDIR/s2c.raw'"
