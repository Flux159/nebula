# 080-cp: docker cp out of and into a running container, byte-exact.
ensure_image alpine:3.19
cleanup_add dk rm -f slimtest-c1

assert_ok "run -d sleeper" dk run -d --name slimtest-c1 alpine:3.19 sleep 60

assert_ok "create file in container via exec" \
    dk exec slimtest-c1 sh -c 'echo cp-payload > /tmp/f'

assert_ok "cp container -> host" dk cp slimtest-c1:/tmp/f "$CASE_TMP/out"
assert_ok "read copied file" cat "$CASE_TMP/out"
assert_out_eq "contents survive cp out" "cp-payload"

printf 'host-payload\n' >"$CASE_TMP/in"
assert_ok "cp host -> container" dk cp "$CASE_TMP/in" slimtest-c1:/tmp/in
assert_ok "read file inside container" dk exec slimtest-c1 cat /tmp/in
assert_out_eq "contents survive cp in" "host-payload"
