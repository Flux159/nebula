# 115-ports-hostip: `-p 127.0.0.1:PORT:PORT` must publish on loopback ONLY,
# and must say so. Reporting 0.0.0.0 for a loopback-scoped publish is the
# difference between "listening on loopback" and "listening on every
# interface this machine has".
ensure_image alpine:3.19
cleanup_add dk rm -f corpus-lo

assert_ok "run with -p 127.0.0.1:18091:80" \
    dk run -d --name corpus-lo -p 127.0.0.1:18091:80 alpine:3.19 \
    sh -c 'while true; do echo hi-loopback | nc -l -p 80; done'

assert_ok "docker port reports the bound address" dk port corpus-lo
assert_out_contains "port mapping is scoped to 127.0.0.1" "127\.0\.0\.1:18091"

assert_ok "ps reports the bound address" dk ps
assert_out_contains "PORTS column shows 127.0.0.1" "127\.0\.0\.1:18091"

assert_ok "inspect reports the bound address" \
    dk inspect -f '{{(index .NetworkSettings.Ports "80/tcp" 0).HostIp}}' corpus-lo
assert_out_eq "HostIp is preserved" "127.0.0.1"

assert_retry_contains 10 "loopback publish is reachable on 127.0.0.1" "hi-loopback" \
    nc -w 3 127.0.0.1 18091
