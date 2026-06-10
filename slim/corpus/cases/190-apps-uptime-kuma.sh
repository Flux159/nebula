# 190-apps-uptime-kuma: real app from the nebula catalog (louislam/
# uptime-kuma:1) — detached run with published port + named volume, HTTP
# readiness within 60s. Heavy (image pull + node boot): CORPUS_QUICK=1 skips.
skip_if_quick
cleanup_add dk volume rm -f slimtest-kuma-data
cleanup_add dk rm -f slimtest-kuma

assert_ok "run uptime-kuma" \
    dk run -d --name slimtest-kuma -p 13201:3001 \
    -v slimtest-kuma-data:/app/data louislam/uptime-kuma:1

# curl prints only the HTTP status code; 200 or 302 (redirect to /setup)
# both mean the app is up and the port + volume plumbing works.
assert_retry_contains 60 "HTTP 200/302 on :13201 within 60s" "^200\$\|^302\$" \
    curl -s -o /dev/null -w '%{http_code}' --max-time 3 http://127.0.0.1:13201/
