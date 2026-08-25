# 155-image-load: `docker save | docker load` round trip. An app that ships
# its own images.tar.gz has no other way to install them offline.
ensure_image alpine:3.19

TAR="$CASE_TMP/loaded.tar"
if ! dk save alpine:3.19 -o "$TAR" >/dev/null 2>&1; then
    # slim has no `save` (layers are stored unpacked); use the real docker as
    # the archive producer when it is around, else skip.
    if command -v docker >/dev/null 2>&1 && docker save alpine:3.19 -o "$TAR" >/dev/null 2>&1; then
        :
    else
        skip "no way to produce a save archive on this host"
    fi
fi

cleanup_add dk rmi -f corpus-loaded:1
dk tag alpine:3.19 corpus-loaded:1 >/dev/null 2>&1

assert_ok "load a docker save archive" dk load -i "$TAR"
assert_out_contains "load reports what it loaded" "Loaded image"

gzip -9 -c "$TAR" >"$TAR.gz"
assert_ok "load a gzipped archive" dk load -i "$TAR.gz"
assert_out_contains "gzipped load reports what it loaded" "Loaded image"

assert_ok "the loaded image runs" dk run --rm alpine:3.19 echo loaded-ok
assert_out_eq "loaded image is usable" "loaded-ok"
