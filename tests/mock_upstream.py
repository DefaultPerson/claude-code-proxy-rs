"""Mock Anthropic API server for integration testing."""
import json
import time
from http.server import HTTPServer, BaseHTTPRequestHandler


class MockAnthropicHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        content_length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(content_length)) if content_length else {}

        # Verify headers
        api_key = self.headers.get("x-api-key", "")
        version = self.headers.get("anthropic-version", "")
        user_agent = self.headers.get("User-Agent", "")

        print(f"[MOCK] {self.command} {self.path}")
        print(f"  x-api-key: {api_key[:20]}...")
        print(f"  anthropic-version: {version}")
        print(f"  user-agent: {user_agent}")
        print(f"  model: {body.get('model', 'N/A')}")
        print(f"  stream: {body.get('stream', False)}")

        # Check metadata.user_id rewriting
        metadata = body.get("metadata") or {}
        user_id = metadata.get("user_id", "") if isinstance(metadata, dict) else ""
        if user_id:
            print(f"  metadata.user_id: {user_id[:60]}...")

        # Check system prompt (billing header should be stripped)
        system = body.get("system", [])
        if isinstance(system, list):
            print(f"  system blocks: {len(system)}")
            for i, block in enumerate(system):
                text = block.get("text", "") if isinstance(block, dict) else str(block)
                if "billing" in text.lower():
                    print(f"  WARNING: billing header NOT stripped in block {i}")
                else:
                    print(f"  system[{i}]: {text[:50]}...")
        elif isinstance(system, str):
            if "billing" in system.lower():
                print(f"  WARNING: billing header NOT stripped")
            print(f"  system: {system[:50]}...")

        if self.path == "/v1/messages":
            if body.get("stream"):
                self._handle_streaming(body)
            else:
                self._handle_non_streaming(body)
        else:
            self.send_response(404)
            self.end_headers()
            self.wfile.write(b'{"error": "not found"}')

    def _handle_streaming(self, body):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("request-id", "mock-req-123")
        self.end_headers()

        model = body.get("model", "claude-sonnet-4-6")

        # message_start
        msg_start = {
            "type": "message_start",
            "message": {
                "id": "msg_mock123",
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": None,
                "usage": {"input_tokens": 10, "output_tokens": 1},
            },
        }
        self._send_sse("message_start", msg_start)

        # ping
        self._send_sse("ping", {"type": "ping"})

        # content_block_start
        self._send_sse(
            "content_block_start",
            {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
        )

        # content_block_delta (send word by word)
        words = ["Hello", " from", " mock", " server!", " Native", " mode", " works!"]
        for word in words:
            delta = {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": word},
            }
            self._send_sse("content_block_delta", delta)
            time.sleep(0.05)

        # content_block_stop
        self._send_sse("content_block_stop", {"type": "content_block_stop", "index": 0})

        # message_delta
        self._send_sse(
            "message_delta",
            {
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                "usage": {"output_tokens": 7},
            },
        )

        # message_stop
        self._send_sse("message_stop", {"type": "message_stop"})

    def _handle_non_streaming(self, body):
        model = body.get("model", "claude-sonnet-4-6")
        resp = {
            "id": "msg_mock456",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "Hello from mock server! Non-streaming works!"}],
            "model": model,
            "stop_reason": "end_turn",
            "stop_sequence": None,
            "usage": {"input_tokens": 10, "output_tokens": 8},
        }
        resp_bytes = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp_bytes)))
        self.end_headers()
        self.wfile.write(resp_bytes)

    def _send_sse(self, event_name, data):
        line = f"event: {event_name}\ndata: {json.dumps(data)}\n\n"
        self.wfile.write(line.encode())
        self.wfile.flush()

    def log_message(self, format, *args):
        pass  # suppress default logging


if __name__ == "__main__":
    port = 19876
    server = HTTPServer(("127.0.0.1", port), MockAnthropicHandler)
    print(f"[MOCK] Anthropic API mock on http://127.0.0.1:{port}")
    server.serve_forever()
