# 150-images-tag-rmi: tag an image, see it listed, untag it.
ensure_image alpine:3.19
cleanup_add dk rmi -f slimtest/tagged:v1

assert_ok "docker tag" dk tag alpine:3.19 slimtest/tagged:v1

assert_ok "docker images exits 0" dk images
assert_out_contains "tagged image listed" "slimtest/tagged"

assert_ok "docker rmi removes the tag" dk rmi slimtest/tagged:v1
