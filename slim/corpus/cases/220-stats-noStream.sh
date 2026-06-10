# 220-stats-noStream: lite check — one-shot stats on a running container
# exits 0 and mentions the container by name. (No value validation.)
ensure_image alpine:3.19
cleanup_add dk rm -f slimtest-s1

assert_ok "run -d sleeper" dk run -d --name slimtest-s1 alpine:3.19 sleep 30

assert_ok "docker stats --no-stream" dk stats --no-stream slimtest-s1
assert_out_contains "stats row names the container" "slimtest-s1"
