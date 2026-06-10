# 010-version-info: client/daemon handshake. `docker version` and
# `docker info` must exit 0 and report a Server section.
assert_ok "docker version exits 0" dk version
assert_out_contains "version output has Server section" "Server"

assert_ok "docker info exits 0" dk info
assert_out_contains "info output has Server section" "Server"
