[![Rust](https://img.shields.io/badge/Rust-1.85+-f74c00?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Axum](https://img.shields.io/badge/Axum-0.8-blue)](https://github.com/tokio-rs/axum)
[![License](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Claude Code](https://img.shields.io/badge/Claude_Code-CLI_Proxy-blueviolet?logo=anthropic)](https://docs.anthropic.com/en/docs/claude-code)
[![Built for OpenClaw](https://img.shields.io/badge/Built_for-OpenClaw-ff6b35)](https://github.com/nicepkg/openclaw)

# claude-code-proxy-rs

Lightweight Rust proxy built for **[OpenClaw](https://github.com/nicepkg/openclaw)** — wraps **Claude Code CLI** as a subprocess and exposes it via **Anthropic Messages API** and **OpenAI Chat Completions API**.

Use your Claude subscription (Max, Team, Enterprise) as an OpenClaw provider without a direct API key.

## How it works

```
  Client (OpenClaw, curl, SDK)
          │
          ▼
  ┌───────────────┐   stdin/stdout   ┌──────────────┐
  │  Proxy :3456  │ ◄─── NDJSON ───► │  claude CLI  │
  │    (axum)     │                  │  subprocess  │
  └───────────────┘                  └──────────────┘
          │
      SSE / JSON
          │
          ▼
        Client
```

Each request spawns a `claude -p --output-format stream-json` subprocess. The proxy filters SSE events — only **text content blocks** are forwarded (thinking, tool_use, and signature blocks are stripped for SDK compatibility). Stateless: no session persistence.

## Features

- **Dual API** — `/v1/messages` (Anthropic) + `/v1/chat/completions` (OpenAI)
- **Streaming & non-streaming** for both APIs
- **System prompt modes** — embed in `<system>` tags (preserves CLI's 43K default prompt) or replace entirely
- **Rate limit handling** — warnings and rejection forwarding
- **Graceful shutdown** — SIGTERM / Ctrl+C
- **Inactivity timeout** — kills stuck subprocesses after 30 min

## Quick start

```bash
# Prerequisites: Claude Code CLI installed and authenticated
npm i -g @anthropic-ai/claude-code && claude auth login

# Build
cargo build --release

# Run
./target/release/claude-code-proxy --port 3456 --cwd ~ --embed-system-prompt
```

> [!IMPORTANT]
> ### 🤖 Instructions for LLM: Configuring proxy with OpenClaw
> Full setup guide for connecting this proxy as an **OpenClaw** LLM provider — `openclaw.json` config, systemd service, model IDs, and troubleshooting:
>
> **➜ [docs/SETUP.md](docs/SETUP.md)**

### CLI flags

| Flag | Default | Description |
|------|---------|-------------|
| `--port` | `3456` | Listen port |
| `--cwd` | `.` | Working directory for CLI subprocess |
| `--embed-system-prompt` | `false` | Embed system prompt in `<system>` tags, keep CLI default prompt |
| `--replace-system-prompt` | `false` | Replace CLI system prompt entirely via `--system-prompt` |
| `--effort` | — | Thinking effort: `low`, `medium`, `high`, `max` |
| `--max-turns` | `100` | Max agentic turns per request |

## Verify

```bash
curl -sN http://localhost:3456/v1/messages \
  -H 'content-type: application/json' \
  -d '{"model":"claude-sonnet-4-6","max_tokens":50,"messages":[{"role":"user","content":"Say hi"}],"stream":true}'
```

Expected: SSE stream with `message_start` → `content_block_delta` → `message_stop`.

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Health check |
| `GET` | `/v1/models` | Available models list |
| `POST` | `/v1/messages` | Anthropic Messages API |
| `POST` | `/v1/chat/completions` | OpenAI Chat Completions API |

## License

MIT
