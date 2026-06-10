# 140-restart-policy: --restart=always relaunches a container that exits.
# After ~5s the container (which lives 1s per run) has restarted at least
# once. We accept EITHER RestartCount > 0 OR State.Running == true: the
# daemon may report a restart-count, or we may catch it mid-flight while
# running again — both prove the policy engaged, and timing decides which
# snapshot we observe.
ensure_image alpine:3.19
cleanup_add dk rm -f slimtest-r1

assert_ok "run -d --restart=always" \
    dk run -d --restart=always --name slimtest-r1 alpine:3.19 sh -c 'sleep 1'

sleep 5

assert_ok "inspect restart state" \
    dk inspect -f '{{.RestartCount}} {{.State.Running}}' slimtest-r1
assert_out_contains "RestartCount>0 or Running=true" "[1-9]\|true"

assert_ok "rm -f stops the restart loop" dk rm -f slimtest-r1
