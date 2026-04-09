//! Direct API client for native mode.
//!
//! Port of cc-gateway/src/proxy.ts forward logic.
//! Forwards requests to api.anthropic.com with OAuth token injection
//! and identity normalization.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures::StreamExt;
use tracing::{error, info};

use crate::config::NativeConfig;
use crate::error::AppError;
use crate::oauth::CredentialStore;
use crate::rewriter;

pub struct NativeClient {
    http: reqwest::Client,
    pub credentials: Arc<CredentialStore>,
    pub config: NativeConfig,
}

impl NativeClient {
    pub fn new(credentials: Arc<CredentialStore>, config: NativeConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            credentials,
            config,
        }
    }

    /// Forward a request to upstream with body/header rewriting and OAuth token injection.
    /// Returns the upstream response as an axum Response (streaming or not).
    pub async fn forward(
        &self,
        mut body: serde_json::Value,
        path: &str,
        request_id: &str,
    ) -> Result<Response, AppError> {
        // 1. Rewrite body (identity normalization)
        rewriter::rewrite_body(&mut body, path, &self.config);

        // 2. Get OAuth token
        let token = self
            .credentials
            .get_access_token()
            .await
            .map_err(AppError::TokenError)?;

        // 3. Build serialized body
        let body_bytes = serde_json::to_vec(&body).unwrap();

        // 4. Build headers
        let mut req_headers = reqwest::header::HeaderMap::new();
        req_headers.insert("x-api-key", token.parse().unwrap());
        req_headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        req_headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        req_headers.insert(
            reqwest::header::USER_AGENT,
            format!(
                "claude-code/{} (external, cli)",
                self.config
                    .env
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("2.1.81")
            )
            .parse()
            .unwrap(),
        );

        // 5. Set host header (matches cc-gateway proxy.ts line 143)
        if let Ok(parsed) = reqwest::Url::parse(&self.config.upstream.url)
            && let Some(host) = parsed.host_str()
        {
            let host_val = if let Some(port) = parsed.port() {
                format!("{host}:{port}")
            } else {
                host.to_string()
            };
            req_headers.insert(reqwest::header::HOST, host_val.parse().unwrap());
        }

        // 6. Forward to upstream
        let upstream_url = format!("{}{}", self.config.upstream.url, path);
        info!("[req={request_id}] Native → {upstream_url}");

        let resp = self
            .http
            .post(&upstream_url)
            .headers(req_headers)
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| {
                error!("[req={request_id}] Upstream error: {e}");
                AppError::Upstream(502, format!("Upstream connection error: {e}"))
            })?;

        let status = resp.status();
        info!("[req={request_id}] Upstream responded: {status}");

        // 6. If upstream error, return error response
        if !status.is_success() {
            let status_u16 = status.as_u16();
            let body_text = resp.text().await.unwrap_or_default();
            error!("[req={request_id}] Upstream error ({status_u16}): {body_text}");
            return Err(AppError::Upstream(status_u16, body_text));
        }

        // 7. Build axum response, streaming body through
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();

        let is_sse = content_type.contains("text/event-stream");

        // Collect response headers (skip hop-by-hop)
        let mut response_headers = axum::http::HeaderMap::new();
        for (key, value) in resp.headers().iter() {
            let lower = key.as_str().to_lowercase();
            if lower == "transfer-encoding" || lower == "connection" {
                continue;
            }
            if let Ok(name) = axum::http::HeaderName::from_bytes(key.as_str().as_bytes())
                && let Ok(val) = axum::http::HeaderValue::from_bytes(value.as_bytes())
            {
                response_headers.insert(name, val);
            }
        }

        if is_sse {
            // SSE passthrough: stream bytes from reqwest → axum body
            let byte_stream = resp
                .bytes_stream()
                .map(|result| result.map_err(std::io::Error::other));
            let body = Body::from_stream(byte_stream);

            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::CONNECTION, "keep-alive")
                .header("x-request-id", request_id);

            // Forward specific upstream headers
            if let Some(req_id) = response_headers.get("request-id") {
                builder = builder.header("x-upstream-request-id", req_id);
            }

            Ok(builder.body(body).unwrap())
        } else {
            // Non-streaming: return full body
            let body_bytes = resp.bytes().await.map_err(|e| {
                AppError::Upstream(502, format!("Failed to read upstream body: {e}"))
            })?;

            let mut builder = Response::builder().status(StatusCode::OK);
            for (key, value) in response_headers.iter() {
                builder = builder.header(key, value);
            }

            Ok(builder.body(Body::from(body_bytes)).unwrap())
        }
    }

    /// Send a request to upstream and return the raw reqwest Response.
    /// Used for OpenAI streaming where we need to parse the SSE events ourselves.
    pub async fn send_raw(
        &self,
        mut body: serde_json::Value,
        path: &str,
        request_id: &str,
    ) -> Result<reqwest::Response, AppError> {
        rewriter::rewrite_body(&mut body, path, &self.config);

        let token = self
            .credentials
            .get_access_token()
            .await
            .map_err(AppError::TokenError)?;

        let body_bytes = serde_json::to_vec(&body).unwrap();
        let version = self
            .config
            .env
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("2.1.81");

        let mut req_headers = reqwest::header::HeaderMap::new();
        req_headers.insert("x-api-key", token.parse().unwrap());
        req_headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        req_headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        req_headers.insert(
            reqwest::header::USER_AGENT,
            format!("claude-code/{version} (external, cli)")
                .parse()
                .unwrap(),
        );

        if let Ok(parsed) = reqwest::Url::parse(&self.config.upstream.url)
            && let Some(host) = parsed.host_str()
        {
            let host_val = if let Some(port) = parsed.port() {
                format!("{host}:{port}")
            } else {
                host.to_string()
            };
            req_headers.insert(reqwest::header::HOST, host_val.parse().unwrap());
        }

        let upstream_url = format!("{}{}", self.config.upstream.url, path);
        info!("[req={request_id}] Native raw → {upstream_url}");

        let resp = self
            .http
            .post(&upstream_url)
            .headers(req_headers)
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| {
                error!("[req={request_id}] Upstream error: {e}");
                AppError::Upstream(502, format!("Upstream connection error: {e}"))
            })?;

        let status = resp.status();
        if !status.is_success() {
            let status_u16 = status.as_u16();
            let body_text = resp.text().await.unwrap_or_default();
            error!("[req={request_id}] Upstream error ({status_u16}): {body_text}");
            return Err(AppError::Upstream(status_u16, body_text));
        }

        Ok(resp)
    }
}

/// Convert OpenAI Chat Completions request → Anthropic Messages API request.
/// Used for /v1/chat/completions in native mode.
pub fn openai_to_anthropic(body: &serde_json::Value) -> serde_json::Value {
    let model = body["model"].as_str().unwrap_or("claude-sonnet-4-6");
    let max_tokens = body["max_tokens"].as_u64().unwrap_or(4096);
    let stream = body["stream"].as_bool().unwrap_or(false);
    let temperature = body.get("temperature").cloned();

    let messages_val = body["messages"].as_array();
    if messages_val.is_none() {
        return serde_json::json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": ""}],
            "stream": stream,
        });
    }

    let msgs = messages_val.unwrap();

    // Extract system prompts
    let mut system_parts: Vec<String> = Vec::new();
    let mut anthropic_messages: Vec<serde_json::Value> = Vec::new();

    for msg in msgs {
        let role = msg["role"].as_str().unwrap_or("user");
        match role {
            "system" | "developer" => {
                if let Some(text) = extract_text_content(&msg["content"])
                    && !text.is_empty()
                {
                    system_parts.push(text);
                }
            }
            "assistant" => {
                if let Some(text) = extract_text_content(&msg["content"])
                    && !text.is_empty()
                {
                    anthropic_messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": text,
                    }));
                }
            }
            "tool" => {
                // Skip tool messages in conversion (no tool_use_id mapping)
            }
            _ => {
                // user
                if let Some(text) = extract_text_content(&msg["content"])
                    && !text.is_empty()
                {
                    anthropic_messages.push(serde_json::json!({
                        "role": "user",
                        "content": text,
                    }));
                }
            }
        }
    }

    // Ensure messages alternate roles (Anthropic requirement)
    // If empty, add a placeholder
    if anthropic_messages.is_empty() {
        anthropic_messages.push(serde_json::json!({
            "role": "user",
            "content": "",
        }));
    }

    let mut result = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": anthropic_messages,
        "stream": stream,
    });

    if !system_parts.is_empty() {
        result["system"] = serde_json::Value::String(system_parts.join("\n"));
    }

    if let Some(temp) = temperature {
        result["temperature"] = temp;
    }

    result
}

fn extract_text_content(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let texts: Vec<String> = arr
            .iter()
            .filter_map(|part| {
                if part["type"].as_str() == Some("text") {
                    part["text"].as_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect();
        if !texts.is_empty() {
            return Some(texts.join("\n"));
        }
    }
    None
}
