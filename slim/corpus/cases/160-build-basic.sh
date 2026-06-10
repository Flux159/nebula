# 160-build-basic: single-stage build, run the result, remove the image.
ensure_image alpine:3.19
cleanup_add dk rmi -f slimtest-build:1

mkdir -p "$CASE_TMP/ctx"
cat >"$CASE_TMP/ctx/Dockerfile" <<'EOF'
FROM alpine:3.19
RUN echo built > /built.txt
CMD ["cat", "/built.txt"]
EOF

assert_ok "docker build" dk build -t slimtest-build:1 "$CASE_TMP/ctx"
assert_ok "run built image" dk run --rm slimtest-build:1
assert_out_eq "RUN layer baked the file" "built"

assert_ok "rmi built image" dk rmi slimtest-build:1
