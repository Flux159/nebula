# 060-exec: exec into a running container, plain and with -e env injection.
ensure_image alpine:3.19
cleanup_add dk rm -f slimtest-e1

assert_ok "run -d sleeper" dk run -d --name slimtest-e1 alpine:3.19 sleep 60

assert_ok "exec echo" dk exec slimtest-e1 echo from-exec
assert_out_contains "exec stdout streams back" "from-exec"

# $FOO is expanded by the shell inside the container, not by this script.
assert_ok "exec -e FOO=bar" dk exec -e FOO=bar slimtest-e1 sh -c 'echo "$FOO"'
assert_out_eq "exec env var visible in container" "bar"
