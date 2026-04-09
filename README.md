# claude-code-proxy-rs

[![Rust](https://img.shields.io/badge/Rust-1.85+-f74c00?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Axum](https://img.shields.io/badge/Axum-0.8-blue)](https://github.com/tokio-rs/axum)
[![License](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Built for OpenClaw](https://img.shields.io/badge/Built_for-OpenClaw-ff6b35)](https://github.com/nicepkg/openclaw)

Lightweight Rust proxy for **OpenClaw** — use your Claude subscription (Max, Team, Enterprise) as an API provider. Two modes: **subprocess** (agentic, via CLI) and **native** (direct API calls with prompt caching & extended thinking).

## Two modes

### Subprocess mode (default)

Spawns `claude -p` CLI as a subprocess per request. Claude works as a full agent with built-in tools (Read, Edit, Bash, etc.) and its 43K system prompt.

```
Client → Proxy :3456 ←── NDJSON ──► claude CLI subprocess → Anthropic API
```

**Use when:** you need agentic coding — file operations, bash execution, multi-turn tool use.

### Native mode

Direct HTTP calls to `api.anthropic.com` using your subscription's OAuth token. No CLI, no subprocess — pure API proxy with identity normalization.

```
Client → Proxy :3456 → rewrite identity → api.anthropic.com/v1/messages
```

**Use when:** you need fast chatbot/completion, prompt caching, extended thinking (`budget_tokens`), or lower latency.

### Comparison

| | Subprocess | Native |
|---|---|---|
| Built-in tools | Read, Edit, Bash, Grep, Glob | None |
| System prompt | 43K Claude Code + yours | Only yours |
| Agentic loops | Yes (multi-turn tool use) | No |
| Prompt caching | No | Yes |
| Extended thinking | No budget control | Yes (`budget_tokens`) |
| Latency | ~2-5s (subprocess startup) | ~0.3s (HTTP) |
| Identity normalization | N/A (CLI handles) | Yes (device_id, billing header, env) |

## Features

- **Dual API** — `/v1/messages` (Anthropic) + `/v1/chat/completions` (OpenAI)
- **Streaming & non-streaming** for both APIs
- **System prompt modes** — embed in `<system>` tags or replace entirely (subprocess mode)
- **OAuth token management** — auto-refresh, credential store (native mode)
- **Identity normalization** — device_id, billing header, env fingerprint (native mode)
- **Rate limit handling** — warnings and rejection forwarding
- **Graceful shutdown** — SIGTERM / Ctrl+C

## Quick start

### Build

```bash
cargo build --release
cp target/release/claude-code-proxy ~/.local/bin/
```

### Subprocess mode

```bash
# Prerequisites: Claude Code CLI installed and authenticated
npm i -g @anthropic-ai/claude-code && claude auth login

claude-code-proxy --port 3456 --cwd ~ --embed-system-prompt
```

### Native mode

```bash
# Prerequisites: Claude Code CLI authenticated
npm i -g @anthropic-ai/claude-code && claude auth login

# Start with your Claude account email (required)
claude-code-proxy --mode native --email you@gmail.com --port 3456
```

The proxy auto-detects everything from the real system:
- OAuth tokens from `~/.claude/.credentials.json`
- Claude Code version from `claude --version`
- Platform, shell, OS version, architecture, Node.js version
- Total RAM from `/proc/meminfo`
- Persistent random device ID (stored in `~/.claude/.proxy_device_id`)

Then point Claude Code (or OpenClaw) at the proxy:
```bash
export ANTHROPIC_BASE_URL=http://localhost:3456
export ANTHROPIC_API_KEY=any-value
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
export CLAUDE_CODE_ATTRIBUTION_HEADER=false
claude  # works through proxy
```

**What the proxy does automatically:**
- Reads OAuth tokens from `~/.claude/.credentials.json`
- Generates a stable device ID from hostname (deterministic SHA-256)
- Uses safe defaults for identity/env fields (macOS arm64 profile)
- Strips billing headers, normalizes all fingerprints
- Auto-refreshes tokens 5 min before expiry
- Forwards telemetry endpoints with identity rewriting

**Advanced:** for custom identity/env, use a config file:
```bash
claude-code-proxy --mode native --native-config config.yaml --port 3456
```

**With a direct API key** (skips OAuth, no identity normalization):
```bash
claude-code-proxy --mode native --native-config config.yaml --api-key sk-ant-api03-... --port 3456
```

> [!IMPORTANT]
> ### 🤖 Instructions for LLM: Configuring proxy with OpenClaw
> Full setup guide for connecting this proxy as an **OpenClaw** LLM provider — `openclaw.json` config, systemd service, model IDs, and troubleshooting:
>
> **➜ [docs/SETUP.md](docs/SETUP.md)**

## CLI flags

### Common

| Flag | Default | Description |
|------|---------|-------------|
| `--port` | `3456` | Listen port |
| `--mode` | `subprocess` | Proxy mode: `subprocess` or `native` |

### Subprocess mode

| Flag | Default | Description |
|------|---------|-------------|
| `--cwd` | `.` | Working directory for CLI subprocess |
| `--embed-system-prompt` | `false` | Embed system prompt in `<system>` tags, keep CLI default prompt |
| `--replace-system-prompt` | `false` | Replace CLI system prompt entirely via `--system-prompt` |
| `--effort` | — | Thinking effort: `low`, `medium`, `high`, `max` |
| `--max-turns` | `100` | Max agentic turns per request |

### Native mode

| Flag | Default | Description |
|------|---------|-------------|
| `--email` | — | Claude account email (required without config file) |
| `--native-config` | — | Path to YAML config file (overrides auto-detect) |
| `--api-key` | — | Direct API key (skips OAuth) |

## Config file (native mode)

See [`config.example.yaml`](config.example.yaml) for a full template. Key sections:

```yaml
upstream:
  url: https://api.anthropic.com    # API endpoint

oauth:
  access_token: "sk-ant-oat01-..."  # from ~/.claude/.credentials.json
  refresh_token: "sk-ant-ort01-..." # auto-refreshed by proxy
  expires_at: 1775341861870         # ms since epoch

identity:
  device_id: "a1b2c3d4..."         # 64-char hex, canonical device ID
  email: "user@example.com"

prompt_env:                         # rewrites <env> block in system prompt
  platform: darwin
  shell: zsh
  os_version: "Darwin 24.4.0"
  working_dir: /Users/jack/projects
```

The proxy auto-refreshes OAuth tokens 5 minutes before expiry. Refresh tokens are rotated automatically.

## Verify

```bash
# Subprocess mode
curl -sN http://localhost:3456/v1/messages \
  -H 'content-type: application/json' \
  -d '{"model":"claude-sonnet-4-6","max_tokens":50,"messages":[{"role":"user","content":"Say hi"}],"stream":true}'

# Native mode (same endpoint, same format)
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

## Identity normalization (native mode)

In native mode, the proxy rewrites request fields to present a canonical device identity:

- **`metadata.user_id`** — rewrites `device_id` to canonical value
- **`x-anthropic-billing-header`** — stripped (enables cross-session prompt cache)
- **System prompt `<env>` block** — Platform, Shell, OS Version, Working directory
- **Home paths** — `/home/user/` → canonical prefix
- **User-Agent** — `claude-code/{version} (external, cli)`
- **Event logging** — device_id, email, env fingerprint, process metrics
- **CCH hash** — recomputed for canonical identity

## Client setup (native mode)

Set these environment variables on machines running Claude Code to route traffic through the proxy:

```bash
export ANTHROPIC_BASE_URL=http://localhost:3456   # Route to proxy
export ANTHROPIC_API_KEY=any-value                # Required by CC, value doesn't matter
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 # Block side-channel telemetry
export CLAUDE_CODE_ATTRIBUTION_HEADER=false        # Disable billing header client-side
```

> [!IMPORTANT]
> **All machines — including the admin — should use the proxy.** Direct Claude Code usage creates a second device fingerprint visible to Anthropic.

## Security considerations (native mode)

> [!WARNING]
> Anthropic has [restricted](https://www.theregister.com/2026/02/20/anthropic_clarifies_ban_third_party_claude_access/) the use of OAuth tokens in third-party tools (February 2026). Native mode is **at-your-own-risk**. Subprocess mode remains the legitimate approach.

**Built-in protections (automatic, no config needed):**
- Billing header stripped from both HTTP headers and system prompt body
- `metadata.user_id` device_id rewritten to canonical value
- System prompt `<env>` block (Platform, Shell, OS Version) rewritten
- Home directory paths sanitized
- User-Agent normalized to `claude-code/{version}`
- Event logging identity/env/process metrics rewritten
- OAuth tokens auto-refreshed, never exposed to clients

**MCP servers bypass the proxy.** `mcp-proxy.anthropic.com` is hardcoded in Claude Code and does not follow `ANTHROPIC_BASE_URL`. MCP requests go directly to Anthropic. Mitigation: block at network level or use local MCP servers.

**Network-level defense (optional).** Block direct connections to Anthropic from client machines:
```
# Clash / proxy rules
DOMAIN-SUFFIX,anthropic.com,REJECT
DOMAIN-SUFFIX,claude.com,REJECT  
DOMAIN-SUFFIX,claude.ai,REJECT
DOMAIN-SUFFIX,datadoghq.com,REJECT
```

**TLS for remote deployment.** For non-localhost, put a reverse proxy (nginx, caddy) in front, or use Tailscale for zero-config encryption.

## Troubleshooting

**Token expired:**
```
ERROR OAuth token expired, waiting for refresh
```
Proxy auto-refreshes, but if refresh token is invalid: re-run `claude auth login` and restart proxy.

**No credentials found:**
```
ERROR No credentials found at /home/user/.claude/.credentials.json
```
Run `claude auth login` first, then restart proxy.

**Claude Code connecting directly:**
Check: (1) `ANTHROPIC_BASE_URL` has no trailing slash, (2) proxy is running (`curl http://localhost:3456/health`), (3) `ANTHROPIC_API_KEY` is set to any non-empty value.

## License

MIT
