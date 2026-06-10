# 130-env-workdir-user: -e and -w are honored. ($A expands inside the
# container shell; single quotes keep this script from expanding it.)
ensure_image alpine:3.19

assert_ok "run with -e A=B -w /tmp" \
    dk run --rm -e A=B -w /tmp alpine:3.19 sh -c 'echo "$A"; pwd'
assert_out_contains "env var A=B visible" "^B\$"
assert_out_contains "workdir is /tmp" "^/tmp\$"
