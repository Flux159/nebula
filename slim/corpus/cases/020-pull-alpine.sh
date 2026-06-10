# 020-pull-alpine: pull the workhorse image used by the rest of the corpus,
# verify it lists and inspects. Capture-then-grep throughout.
assert_ok "docker pull alpine:3.19" dk pull alpine:3.19

assert_ok "docker images exits 0" dk images
assert_out_contains "alpine appears in images output" "alpine"

assert_ok "docker image inspect alpine:3.19" dk image inspect alpine:3.19
assert_out_contains "inspect reports Architecture" "Architecture"
