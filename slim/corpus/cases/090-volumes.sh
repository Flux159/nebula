# 090-volumes: named volume lifecycle — create, write from one container,
# read from a second, remove.
ensure_image alpine:3.19
cleanup_add dk volume rm -f slimtest-v1

assert_ok "volume create" dk volume create slimtest-v1
assert_ok "write via first container" \
    dk run --rm -v slimtest-v1:/data alpine:3.19 sh -c 'echo vol-data > /data/f'
assert_ok "read via second container" \
    dk run --rm -v slimtest-v1:/data alpine:3.19 cat /data/f
assert_out_eq "data persisted across containers" "vol-data"

assert_ok "volume rm" dk volume rm slimtest-v1
