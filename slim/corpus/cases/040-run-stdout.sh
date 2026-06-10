# 040-run-stdout: container stdout streams back to the client.
ensure_image alpine:3.19

assert_ok "run --rm echo hello" dk run --rm alpine:3.19 echo hello
assert_out_contains "stdout contains hello" "hello"
