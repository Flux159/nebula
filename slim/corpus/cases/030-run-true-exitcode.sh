# 030-run-true-exitcode: container exit codes propagate to the client.
ensure_image alpine:3.19

assert_ok "run --rm true exits 0" dk run --rm alpine:3.19 true
assert_exit 7 "run --rm 'exit 7' propagates exit 7" \
    dk run --rm alpine:3.19 sh -c 'exit 7'
