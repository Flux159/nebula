# 050-detach-ps-logs: detached run, ps visibility, logs, stop, ps -a, rm.
ensure_image alpine:3.19
cleanup_add dk rm -f slimtest-d1

assert_ok "run -d slimtest-d1" \
    dk run -d --name slimtest-d1 alpine:3.19 sh -c 'echo started; sleep 30'

assert_ok "docker ps exits 0" dk ps
assert_out_contains "ps shows slimtest-d1" "slimtest-d1"

# logs may lag the run by a beat on slower daemons; retry briefly.
assert_retry_contains 5 "logs contain 'started'" "started" dk logs slimtest-d1

assert_ok "docker stop slimtest-d1" dk stop slimtest-d1

assert_ok "docker ps -a exits 0" dk ps -a
assert_out_contains "ps -a shows Exited" "Exited"

assert_ok "docker rm slimtest-d1" dk rm slimtest-d1
