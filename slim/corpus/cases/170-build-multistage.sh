# 170-build-multistage: COPY --from=0 carries an artifact across stages.
ensure_image alpine:3.19
cleanup_add dk rmi -f slimtest-multi:1

mkdir -p "$CASE_TMP/ctx"
cat >"$CASE_TMP/ctx/Dockerfile" <<'EOF'
FROM alpine:3.19
RUN echo stage-zero-artifact > /artifact

FROM alpine:3.19
COPY --from=0 /artifact /artifact
CMD ["cat", "/artifact"]
EOF

assert_ok "multi-stage build" dk build -t slimtest-multi:1 "$CASE_TMP/ctx"
assert_ok "run final stage" dk run --rm slimtest-multi:1
assert_out_eq "artifact copied from stage 0" "stage-zero-artifact"
