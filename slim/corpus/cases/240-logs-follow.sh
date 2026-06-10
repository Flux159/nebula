# 240-logs-follow: `docker logs -f` streams lines as they appear and exits
# when the container exits. The follower runs in the background writing to a
# file; a bounded poll loop replaces any blocking wait, so this cannot hang.
ensure_image alpine:3.19
cleanup_add dk rm -f slimtest-lf

assert_ok "run -d 3-line printer (~1 line/s for 3s)" \
    dk run -d --name slimtest-lf alpine:3.19 \
    sh -c 'for i in 1 2 3; do echo "line $i"; sleep 1; done'

LOGF="$CASE_TMP/follow.log"
dk logs -f slimtest-lf >"$LOGF" 2>"$CASE_TMP/follow.err" &
LPID=$!
cleanup_add "kill -9 $LPID"

# Container exits after ~3s; give the follower up to 10s to notice and exit.
i=0
while kill -0 "$LPID" 2>/dev/null && [ "$i" -lt 10 ]; do
    sleep 1
    i=$((i + 1))
done
follow_exited=yes
if kill -0 "$LPID" 2>/dev/null; then
    follow_exited=no
    kill -9 "$LPID" 2>/dev/null
fi
wait "$LPID" 2>/dev/null

# Capture-then-count: grep -c on the file, no pipes.
nlines=$(grep -c -e '^line' "$LOGF" 2>/dev/null) || nlines=0
assert_ok "captured at least 2 lines (got $nlines)" [ "$nlines" -ge 2 ]
assert_ok "logs -f exited when the container exited" [ "$follow_exited" = yes ]
