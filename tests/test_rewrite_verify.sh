#!/bin/bash
# Verify rewriter actually rewrites: check what mock server receives
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
MOCK_PORT=19878
PROXY_PORT=19879
MOCK_LOG=$(mktemp)

cleanup() {
    [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null
    [ -n "$PROXY_PID" ] && kill "$PROXY_PID" 2>/dev/null
    wait "$MOCK_PID" 2>/dev/null
    wait "$PROXY_PID" 2>/dev/null
    rm -f "$MOCK_LOG"
}
trap cleanup EXIT

# Start mock (capture stdout to log)
python3 "$SCRIPT_DIR/mock_upstream.py" > "$MOCK_LOG" 2>&1 &
MOCK_PID=$!
# Override port in mock
kill "$MOCK_PID" 2>/dev/null; wait "$MOCK_PID" 2>/dev/null

# Use a modified mock with custom port
MOCK_PORT=19878
python3 -c "
import sys; sys.path.insert(0, '$SCRIPT_DIR')
from mock_upstream import MockAnthropicHandler
from http.server import HTTPServer
HTTPServer(('127.0.0.1', $MOCK_PORT), MockAnthropicHandler).serve_forever()
" > "$MOCK_LOG" 2>&1 &
MOCK_PID=$!
sleep 0.3

# Create test config pointing to custom port
TEST_CFG=$(mktemp --suffix=.yaml)
sed "s/19876/$MOCK_PORT/" "$SCRIPT_DIR/test_config.yaml" > "$TEST_CFG"

# Start proxy
"$PROJECT_DIR/target/debug/claude-code-proxy" \
    --mode native \
    --native-config "$TEST_CFG" \
    --api-key "sk-ant-oat01-fake-test-token" \
    --port "$PROXY_PORT" 2>/dev/null &
PROXY_PID=$!
sleep 0.5

echo "=== Sending request with identity data ==="
curl -s "http://127.0.0.1:$PROXY_PORT/v1/messages" \
    -H 'content-type: application/json' \
    -d '{
        "model": "claude-sonnet-4-6",
        "max_tokens": 100,
        "messages": [{"role":"user","content":"Hello world test message here!"}],
        "metadata": {"user_id": "{\"device_id\":\"ORIGINAL_DEVICE_ID_abc123\",\"account_uuid\":\"acc-999\"}"},
        "system": [
            {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.81.abc; cc_entrypoint=cli;"},
            {"type":"text","text":"You are helpful.\nPlatform: linux\nShell: bash\nOS Version: Linux 6.5.0\nWorking directory: /home/bob/myproject"}
        ]
    }' > /dev/null

sleep 0.3

echo ""
echo "=== What mock server received ==="
cat "$MOCK_LOG"

echo ""
echo "=== Verification ==="
PASS=0; FAIL=0

check() {
    local label="$1" pattern="$2"
    if grep -qF "$pattern" "$MOCK_LOG"; then
        echo "  ✓ $label"
        PASS=$((PASS + 1))
    else
        echo "  ✗ $label — pattern '$pattern' not found"
        FAIL=$((FAIL + 1))
    fi
}

check_not() {
    local label="$1" pattern="$2"
    if grep -qF "$pattern" "$MOCK_LOG"; then
        echo "  ✗ $label — found '$pattern' (should be stripped)"
        FAIL=$((FAIL + 1))
    else
        echo "  ✓ $label"
        PASS=$((PASS + 1))
    fi
}

check "x-api-key received" "sk-ant-oat01-fake-test"
check "user-agent rewritten" "claude-code/2.1.81"
check "device_id rewritten" "a1b2c3d4e5f6"
check_not "billing header stripped" "billing"
check "system blocks = 1 (billing stripped)" "system blocks: 1"
check "platform rewritten" "Platform: darwin"
check "shell rewritten" "Shell: zsh"
check "OS version rewritten" "Darwin 24.4.0"

echo ""
echo "  PASSED: $PASS  FAILED: $FAIL"
rm -f "$TEST_CFG"
[ "$FAIL" -gt 0 ] && exit 1
