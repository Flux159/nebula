# 105-bind-mount-spaces: host directories whose path contains spaces — the
# macOS norm (`~/Library/Application Support/<bundle-id>/…`). Read-only and
# read-write, plus a source that doesn't exist yet.
ensure_image alpine:3.19

STATE="$CASE_TMP/Application Support/nebula.corpus"
mkdir -p "$STATE/conf"
printf 'motd: hello\n' >"$STATE/conf/inter_conf.txt"

assert_ok "read-only bind from a path with spaces" \
    dk run --rm -v "$STATE/conf:/conf:ro" alpine:3.19 cat /conf/inter_conf.txt
assert_out_eq "config file readable in the container" "motd: hello"

assert_fail "read-only bind rejects writes" \
    dk run --rm -v "$STATE/conf:/conf:ro" alpine:3.19 sh -c 'echo x >/conf/nope'

mkdir -p "$STATE/sql"
assert_ok "read-write bind from a path with spaces" \
    dk run --rm -v "$STATE/sql:/initdb" alpine:3.19 sh -c 'echo from-container >/initdb/out'
assert_ok "the write landed on the host" cat "$STATE/sql/out"
assert_out_eq "host file has the container's bytes" "from-container"

# docker creates a missing -v source rather than failing the run.
rm -rf "$STATE/npc"
assert_ok "missing bind source is created" \
    dk run --rm -v "$STATE/npc:/npc" alpine:3.19 sh -c 'echo made >/npc/f'
assert_ok "created source is a directory on the host" cat "$STATE/npc/f"
assert_out_eq "content written through the created source" "made"

assert_ok "--mount type=bind with a spaced source" \
    dk run --rm --mount "type=bind,source=$STATE/conf,target=/conf,readonly" \
    alpine:3.19 cat /conf/inter_conf.txt
assert_out_eq "--mount delivered the same file" "motd: hello"

# Containers write as root, so the runner (unprivileged) can't rm -rf the
# scratch dir afterwards. Hand that back to a container. Inline rather than
# cleanup_add: registered cleanups are re-split on whitespace, and this path
# has spaces in it — which is the whole point of the case.
dk run --rm -v "$STATE:/state" alpine:3.19 rm -rf /state/conf /state/sql /state/npc >/dev/null 2>&1
