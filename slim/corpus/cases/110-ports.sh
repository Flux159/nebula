# 110-ports: published port reachable from the host, and `docker port`.
# busybox httpd ships in alpine — no extra image needed.
ensure_image alpine:3.19
cleanup_add dk rm -f slimtest-web

assert_ok "run httpd with -p 18080:80" \
    dk run -d --name slimtest-web -p 18080:80 alpine:3.19 \
    sh -c 'echo hi > /tmp/index.html && busybox httpd -f -p 80 -h /tmp'

# 127.0.0.1, not localhost: avoids an ::1-first resolver surprise.
assert_retry_contains 10 "curl through published port returns hi" "hi" \
    curl -s --max-time 3 http://127.0.0.1:18080/

assert_ok "docker port slimtest-web" dk port slimtest-web
assert_out_contains "port mapping lists 18080" "18080"
