# 210-wait-rm-flow: docker wait blocks until exit and prints the code.
ensure_image alpine:3.19
cleanup_add dk rm -f slimtest-w1

assert_ok "run -d short-lived exit-3 container" \
    dk run -d --name slimtest-w1 alpine:3.19 sh -c 'sleep 2; exit 3'

assert_ok "docker wait returns" dk wait slimtest-w1
assert_out_eq "wait prints the exit code 3" "3"

assert_ok "docker rm after exit" dk rm slimtest-w1
