# 230-events-lite: a `start` event is observable on the events stream.
#
# Hang-proofing, in layers:
#   1. The window uses ABSOLUTE epoch timestamps. (The spec's literal
#      `--since 0s --until 5s` would not work: docker parses relative
#      durations as "that long AGO", so `--until 5s` ends the window in the
#      past and the collector exits before our container starts.)
#   2. --until makes `docker events` terminate itself when the window ends.
#   3. The collector runs in the background; we poll its pid with a bounded
#      loop and kill -9 as a last resort. No foreground read can block.
ensure_image alpine:3.19

EVFILE="$CASE_TMP/events.log"
NOW=$(date +%s)
dk events --since "$NOW" --until "$((NOW + 5))" >"$EVFILE" 2>"$CASE_TMP/events.err" &
EVPID=$!
cleanup_add "kill -9 $EVPID"

sleep 1
assert_ok "run a container inside the event window" \
    dk run --rm alpine:3.19 true

# Wait (bounded, max 10s) for the collector to end on its own via --until.
i=0
while kill -0 "$EVPID" 2>/dev/null && [ "$i" -lt 10 ]; do
    sleep 1
    i=$((i + 1))
done
kill -9 "$EVPID" 2>/dev/null
wait "$EVPID" 2>/dev/null

assert_ok "events stream recorded a start event" grep -e " start" "$EVFILE"
