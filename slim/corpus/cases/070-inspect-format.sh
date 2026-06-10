# 070-inspect-format: Go-template -f formatting on container inspect.
ensure_image alpine:3.19
cleanup_add dk rm -f slimtest-i1

assert_ok "run -d sleeper" dk run -d --name slimtest-i1 alpine:3.19 sleep 60

assert_ok "inspect -f .State.Running" \
    dk inspect -f '{{.State.Running}}' slimtest-i1
assert_out_eq "State.Running is true" "true"

assert_ok "inspect -f .Config.Image" \
    dk inspect -f '{{.Config.Image}}' slimtest-i1
assert_out_contains "Config.Image is alpine" "alpine"
