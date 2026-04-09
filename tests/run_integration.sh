#!/bin/bash
# Integration test: mock upstream + proxy in native mode
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
MOCK_PORT=19876
PROXY_PORT=19877
PASS=0
FAIL=0

cleanup() {
    echo ""
    echo "=== Cleanup ==="
    [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null && echo "Killed mock (pid=$MOCK_PID)"
    [ -n "$PROXY_PID" ] && kill "$PROXY_PID" 2>/dev/null && echo "Killed proxy (pid=$PROXY_PID)"
    wait "$MOCK_PID" 2>/dev/null
    wait "$PROXY_PID" 2>/dev/null
    echo ""
    echo "================================"
    echo "  PASSED: $PASS  FAILED: $FAIL"
    echo "================================"
    [ "$FAIL" -gt 0 ] && exit 1
    exit 0
}
trap cleanup EXIT

assert_contains() {
    local label="$1" output="$2" expected="$3"
    if echo "$output" | grep -qF "$expected"; then
        echo "  ✓ $label"
        PASS=$((PASS + 1))
    else
        echo "  ✗ $label — expected '$expected'"
        echo "    got: $(echo "$output" | head -3)"
        FAIL=$((FAIL + 1))
    fi
}

assert_not_contains() {
    local label="$1" output="$2" unexpected="$3"
    if echo "$output" | grep -qF "$unexpected"; then
        echo "  ✗ $label — found unexpected '$unexpected'"
        FAIL=$((FAIL + 1))
    else
        echo "  ✓ $label"
        PASS=$((PASS + 1))
    fi
}

# 1. Start mock upstream
echo "=== Starting mock upstream on :$MOCK_PORT ==="
python3 "$SCRIPT_DIR/mock_upstream.py" &
MOCK_PID=$!
sleep 0.5

# 2. Build & start proxy in native mode
echo "=== Building proxy ==="
cargo build --quiet --manifest-path "$PROJECT_DIR/Cargo.toml"

echo "=== Starting proxy in NATIVE mode on :$PROXY_PORT ==="
"$PROJECT_DIR/target/debug/claude-code-proxy" \
    --mode native \
    --native-config "$SCRIPT_DIR/test_config.yaml" \
    --api-key "sk-ant-oat01-fake-test-token" \
    --port "$PROXY_PORT" &
PROXY_PID=$!
sleep 1

# -----------------------------------------------------------------------
echo ""
echo "=== Test 1: Health check ==="
HEALTH=$(curl -s "http://127.0.0.1:$PROXY_PORT/health")
assert_contains "status ok" "$HEALTH" '"status":"ok"'

# -----------------------------------------------------------------------
echo ""
echo "=== Test 2: Models list ==="
MODELS=$(curl -s "http://127.0.0.1:$PROXY_PORT/v1/models")
assert_contains "has claude-sonnet-4-6" "$MODELS" 'claude-sonnet-4-6'
assert_contains "has claude-opus-4-6" "$MODELS" 'claude-opus-4-6'

# -----------------------------------------------------------------------
echo ""
echo "=== Test 3: /v1/messages non-streaming (Anthropic format) ==="
RESP=$(curl -s "http://127.0.0.1:$PROXY_PORT/v1/messages" \
    -H 'content-type: application/json' \
    -d '{
        "model": "claude-sonnet-4-6",
        "max_tokens": 100,
        "messages": [{"role":"user","content":"Say hi"}]
    }')
assert_contains "has content" "$RESP" "Hello from mock server"
assert_contains "has stop_reason" "$RESP" "end_turn"
assert_contains "has usage" "$RESP" "input_tokens"

# -----------------------------------------------------------------------
echo ""
echo "=== Test 4: /v1/messages streaming (Anthropic format, SSE passthrough) ==="
SSE=$(curl -sN --max-time 5 "http://127.0.0.1:$PROXY_PORT/v1/messages" \
    -H 'content-type: application/json' \
    -d '{
        "model": "claude-sonnet-4-6",
        "max_tokens": 100,
        "messages": [{"role":"user","content":"Say hi"}],
        "stream": true
    }' 2>/dev/null || true)
assert_contains "has message_start" "$SSE" "message_start"
assert_contains "has content_block_delta" "$SSE" "content_block_delta"
assert_contains "has text_delta" "$SSE" "text_delta"
assert_contains "has message_stop" "$SSE" "message_stop"
assert_contains "has Hello" "$SSE" "Hello"

# -----------------------------------------------------------------------
echo ""
echo "=== Test 5: /v1/chat/completions non-streaming (OpenAI format) ==="
OAI_RESP=$(curl -s "http://127.0.0.1:$PROXY_PORT/v1/chat/completions" \
    -H 'content-type: application/json' \
    -d '{
        "model": "claude-sonnet-4-6",
        "max_tokens": 100,
        "messages": [
            {"role":"system","content":"You are helpful"},
            {"role":"user","content":"Say hi"}
        ]
    }')
assert_contains "has chat.completion" "$OAI_RESP" "chat.completion"
assert_contains "has content" "$OAI_RESP" "Hello from mock server"
assert_contains "has finish_reason" "$OAI_RESP" "finish_reason"
assert_contains "has usage" "$OAI_RESP" "prompt_tokens"

# -----------------------------------------------------------------------
echo ""
echo "=== Test 6: /v1/chat/completions streaming (OpenAI format) ==="
OAI_SSE=$(curl -sN --max-time 5 "http://127.0.0.1:$PROXY_PORT/v1/chat/completions" \
    -H 'content-type: application/json' \
    -d '{
        "model": "claude-sonnet-4-6",
        "max_tokens": 100,
        "messages": [{"role":"user","content":"Say hi"}],
        "stream": true
    }' 2>/dev/null || true)
assert_contains "has chat.completion.chunk" "$OAI_SSE" "chat.completion.chunk"
assert_contains "has Hello" "$OAI_SSE" "Hello"
assert_contains "has [DONE]" "$OAI_SSE" "[DONE]"
assert_contains "has finish_reason stop" "$OAI_SSE" '"finish_reason":"stop"'

# -----------------------------------------------------------------------
echo ""
echo "=== Test 7: Identity normalization (metadata.user_id rewrite) ==="
REWRITE_RESP=$(curl -s "http://127.0.0.1:$PROXY_PORT/v1/messages" \
    -H 'content-type: application/json' \
    -d '{
        "model": "claude-sonnet-4-6",
        "max_tokens": 100,
        "messages": [{"role":"user","content":"test identity"}],
        "metadata": {"user_id": "{\"device_id\":\"old_device_id_here\",\"account_uuid\":\"acc-123\"}"},
        "system": [
            {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.81.abc; cc_entrypoint=cli;"},
            {"type":"text","text":"Real system prompt here. Platform: linux\nShell: bash\nOS Version: Linux 6.5.0"}
        ]
    }')
# The mock server prints what it receives — check proxy logs for rewriting
# Response should still work
assert_contains "identity test response ok" "$REWRITE_RESP" "Hello from mock server"

# -----------------------------------------------------------------------
echo ""
echo "=== Test 8: Error handling — empty messages ==="
ERR_RESP=$(curl -s -w "\n%{http_code}" "http://127.0.0.1:$PROXY_PORT/v1/messages" \
    -H 'content-type: application/json' \
    -d '{"model":"claude-sonnet-4-6","max_tokens":100,"messages":[]}')
assert_contains "returns 400" "$ERR_RESP" "400"
assert_contains "error message" "$ERR_RESP" "must not be empty"

# -----------------------------------------------------------------------
echo ""
echo "=== Test 9: Unknown path forwarding (native mode) ==="
NOT_FOUND=$(curl -s "http://127.0.0.1:$PROXY_PORT/nonexistent")
assert_contains "upstream returns error" "$NOT_FOUND" "error"
