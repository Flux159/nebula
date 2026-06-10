# 120-network-dns: user-defined bridge network with container-name DNS.
# Cleanups run newest-first, so the container is removed before the network.
ensure_image alpine:3.19
cleanup_add dk network rm slimtest-net
cleanup_add dk rm -f slimtest-srv

assert_ok "network create" dk network create slimtest-net
assert_ok "server container on network" \
    dk run -d --name slimtest-srv --network slimtest-net alpine:3.19 sleep 60

# ping resolves slimtest-srv via the network's embedded DNS and proves
# reachability in one shot. -W 3 bounds the wait.
assert_ok "peer resolves and pings slimtest-srv by name" \
    dk run --rm --network slimtest-net alpine:3.19 ping -c 1 -W 3 slimtest-srv
