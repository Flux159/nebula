#!/bin/bash
# Phase 10 acceptance: REST API + TS/Python SDKs against a live engine.
set -uo pipefail
cd "$(dirname "$0")/.."

NEBULA="$PWD/target/debug/nebula"
cargo build -p nebula-cli -p nebulad >/dev/null 2>&1
scripts/sign-dev.sh target/debug/nebula target/debug/nebulad >/dev/null
PASS=0; FAIL=0
check() {
    if eval "$2" >/dev/null 2>&1; then
        echo "PASS: $1"; PASS=$((PASS+1))
    else
        echo "FAIL: $1  [$2]"; FAIL=$((FAIL+1))
    fi
}

$NEBULA down --force >/dev/null 2>&1; sleep 1
$NEBULA up >/dev/null || { echo "FATAL: up failed"; exit 1; }
API=http://127.0.0.1:7440
# dockerd takes a few seconds after boot; the containers endpoint needs it.
for _ in $(seq 1 30); do
    curl -fsS $API/v1alpha1/containers >/dev/null 2>&1 && break
    sleep 1
done

echo "--- REST API"
check "healthz"               "curl -fsS $API/healthz | grep -q true"
check "status shape"          "curl -fsS $API/v1alpha1/status | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d[\"apiVersion\"]==\"v1alpha1\" and d[\"vmState\"]==\"Running\" and d[\"agent\"]'"
check "stats shape"           "curl -fsS $API/v1alpha1/stats | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d[\"maxMib\"]>0 and d[\"hostFootprintMib\"]>0'"
check "exec via API"          "curl -fsS -X POST $API/v1alpha1/exec -d '{\"cmd\":\"uname\",\"args\":[\"-m\"]}' | grep -q aarch64"
check "exec bad json is 400"  "curl -s -o /dev/null -w '%{http_code}' -X POST $API/v1alpha1/exec -d 'nope' | grep -q 400"
check "containers endpoint"   "curl -fsS $API/v1alpha1/containers | python3 -c 'import json,sys; assert isinstance(json.load(sys.stdin), list)'"
check "unknown route is 404"  "curl -s -o /dev/null -w '%{http_code}' $API/v1alpha1/nope | grep -q 404"

echo "--- Python SDK"
check "python sdk end-to-end" "PYTHONPATH=sdk/python python3 -c '
from nebula_vm import NebulaClient
n = NebulaClient()
assert n.is_running()
s = n.status(); assert s[\"vmState\"] == \"Running\"
r = n.exec(\"uname\", [\"-m\"]); assert \"aarch64\" in r[\"stdout\"] and r[\"exit_code\"] == 0
assert isinstance(n.containers(), list)
st = n.stats(); assert st[\"maxMib\"] > 0
print(\"python sdk ok\")'"

echo "--- TypeScript SDK"
if command -v npx >/dev/null 2>&1; then
    (cd sdk/typescript && npm install --silent >/dev/null 2>&1 && npx tsc >/dev/null 2>&1)
    check "ts sdk compiles"        "[ -f sdk/typescript/dist/index.js ]"
    check "ts sdk end-to-end"      "node -e '
import(\"./sdk/typescript/dist/index.js\").then(async ({ NebulaClient }) => {
  const n = new NebulaClient();
  if (!(await n.isRunning())) throw new Error(\"not running\");
  const s = await n.status();
  if (s.vmState !== \"Running\") throw new Error(\"bad state\");
  const r = await n.exec(\"uname\", [\"-m\"]);
  if (!r.stdout.includes(\"aarch64\")) throw new Error(\"bad exec\");
  await n.containers();
  console.log(\"ts sdk ok\");
})'"
else
    echo "SKIP: node/npx not available"
fi

echo
echo "phase 10: $PASS passed, $FAIL failed"
exit $((FAIL > 0))
