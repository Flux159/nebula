# 200-apps-vaultwarden: real app from the nebula catalog (vaultwarden/
# server:latest) — same shape as 190. CORPUS_QUICK=1 skips.
skip_if_quick
cleanup_add dk volume rm -f slimtest-vw-data
cleanup_add dk rm -f slimtest-vw

assert_ok "run vaultwarden" \
    dk run -d --name slimtest-vw -p 13203:80 \
    -v slimtest-vw-data:/data vaultwarden/server:latest

assert_retry_contains 60 "HTTP 200 on :13203 within 60s" "^200\$" \
    curl -s -o /dev/null -w '%{http_code}' --max-time 3 http://127.0.0.1:13203/
