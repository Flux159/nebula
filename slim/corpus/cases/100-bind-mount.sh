# 100-bind-mount: host directory bind-mounted read into a container.
# CASE_TMP lives under ${TMPDIR:-/tmp}; host file sharing of that path is
# part of the compatibility contract being scored.
ensure_image alpine:3.19

mkdir -p "$CASE_TMP/host-dir"
printf 'bind-data\n' >"$CASE_TMP/host-dir/f"

assert_ok "run with bind mount" \
    dk run --rm -v "$CASE_TMP/host-dir:/hostdir" alpine:3.19 cat /hostdir/f
assert_out_eq "host file readable in container" "bind-data"
