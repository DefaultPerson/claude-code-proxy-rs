//! HTTP route handlers.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::adapter::{request, response};
use crate::error::AppError;
use crate::native;
use crate::server::{AppState, ProxyMode};
use crate::subprocess;
use crate::types::anthropic::MessagesRequest;

/// GET /health
pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    match state.mode {
        ProxyMode::Native => {
            let client = state.native_client.as_ref().unwrap();
            let oauth_ok = client.credentials.get_access_token().await.is_ok();
            let device_id = &client.config.identity.device_id;
            Json(json!({
                "status": if oauth_ok { "ok" } else { "degraded" },
                "version": env!("CARGO_PKG_VERSION"),
                "mode": "native",
                "oauth": if oauth_ok { "valid" } else { "expired/refreshing" },
                "upstream": client.config.upstream.url,
                "canonical_device": format!("{}...", &device_id[..8]),
            }))
        }
        ProxyMode::Subprocess => {
            Json(json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "mode": "subprocess",
            }))
        }
    }
}

/// GET /v1/models
pub async fn models() -> impl IntoResponse {
    Json(json!({
        "data": [
            { "id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6", "type": "model", "object": "model" },
            { "id": "claude-opus-4-6", "display_name": "Claude Opus 4.6", "type": "model", "object": "model" },
            { "id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5", "type": "model", "object": "model" },
            { "id": "claude-sonnet-4", "display_name": "Claude Sonnet 4.6", "type": "model", "object": "model" },
            { "id": "claude-opus-4", "display_name": "Claude Opus 4.6", "type": "model", "object": "model" },
            { "id": "claude-haiku-4", "display_name": "Claude Haiku 4.5", "type": "model", "object": "model" },
        ],
        "has_more": false,
        "object": "list",
    }))
}

/// POST /v1/messages — Anthropic Messages API
pub async fn messages(
    State(state): State<AppState>,
    Json(request): Json<MessagesRequest>,
) -> Result<Response, AppError> {
    if request.messages.is_empty() {
        return Err(AppError::BadRequest("messages must not be empty".into()));
    }

    let request_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let is_streaming = request.stream;

    info!(
        "[req={request_id}] POST /v1/messages model={} stream={is_streaming} messages={}",
        request.model,
        request.messages.len()
    );

    match state.mode {
        ProxyMode::Native => {
            // Native mode: forward to api.anthropic.com (SSE passthrough)
            let client = state.native_client.as_ref().unwrap();
            let body = serde_json::to_value(&request)
                .map_err(|e| AppError::BadRequest(format!("Failed to serialize request: {e}")))?;
            client.forward(body, "/v1/messages", &request_id).await
        }
        ProxyMode::Subprocess => {
            // Subprocess mode: existing behavior
            log_tools_warning(&request_id, &request);
            let config = make_config(&state);
            let (options, prompt) =
                request::prepare_subprocess(&request, request_id.clone(), &state.cwd, &config);

            if is_streaming {
                handle_anthropic_streaming(request_id, options, prompt, &request.model).await
            } else {
                handle_non_streaming(request_id, options, prompt, &request.model).await
            }
        }
    }
}

/// Extract text from OpenAI message content (string or array of content parts).
/// Matches old proxy's `extractText()` behavior.
fn extract_text(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter_map(|part| {
                if part["type"].as_str() == Some("text") {
                    part["text"].as_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

/// POST /v1/chat/completions — OpenAI Chat Completions API
pub async fn chat_completions(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, AppError> {
    // Extract fields from OpenAI format
    let model = body["model"].as_str().unwrap_or("claude-sonnet-4-6");
    let stream = body["stream"].as_bool().unwrap_or(false);
    let messages_val = body["messages"].as_array();

    if messages_val.is_none() || messages_val.unwrap().is_empty() {
        return Err(AppError::BadRequest("messages is required".into()));
    }

    let request_id = uuid::Uuid::new_v4().to_string()[..8].to_string();

    info!(
        "[req={request_id}] POST /v1/chat/completions model={model} stream={stream} messages={}",
        messages_val.unwrap().len()
    );

    match state.mode {
        ProxyMode::Native => {
            // Native mode: convert OpenAI → Anthropic, forward to API
            let client = state.native_client.as_ref().unwrap();
            let anthropic_body = native::openai_to_anthropic(&body);

            if stream {
                // For streaming: forward to API, then parse Anthropic SSE → OpenAI SSE
                handle_native_openai_streaming(client, anthropic_body, &request_id, model).await
            } else {
                // Non-streaming: forward, convert Anthropic JSON → OpenAI JSON
                handle_native_openai_non_streaming(client, anthropic_body, &request_id, model)
                    .await
            }
        }
        ProxyMode::Subprocess => {
            // Subprocess mode: existing behavior
            let msgs = messages_val.unwrap();
            let embed = state.embed_system_prompt;

            let system_parts: Vec<String> = msgs
                .iter()
                .filter(|m| matches!(m["role"].as_str(), Some("system") | Some("developer")))
                .map(|m| extract_text(&m["content"]))
                .filter(|t| !t.is_empty())
                .collect();

            let mut prompt_parts: Vec<String> = Vec::new();
            if embed && !system_parts.is_empty() {
                prompt_parts
                    .push(format!("<system>\n{}\n</system>", system_parts.join("\n")));
            }

            for m in msgs.iter() {
                let role = m["role"].as_str().unwrap_or("user");
                if matches!(role, "system" | "developer") {
                    continue;
                }
                let text = extract_text(&m["content"]);

                let part = match role {
                    "assistant" => {
                        let mut parts = Vec::new();
                        if !text.is_empty() {
                            parts.push(text);
                        }
                        if let Some(tool_calls) = m["tool_calls"].as_array() {
                            for tc in tool_calls {
                                let name =
                                    tc["function"]["name"].as_str().unwrap_or("unknown");
                                let args =
                                    tc["function"]["arguments"].as_str().unwrap_or("{}");
                                parts.push(format!("[Called tool: {name}({args})]"));
                            }
                        }
                        if parts.is_empty() {
                            continue;
                        }
                        format!(
                            "<previous_response>\n{}\n</previous_response>",
                            parts.join("\n")
                        )
                    }
                    "tool" => {
                        let tool_text = extract_text(&m["content"]);
                        if tool_text.is_empty() {
                            continue;
                        }
                        format!("<tool_result>\n{tool_text}\n</tool_result>")
                    }
                    _ => {
                        if text.is_empty() {
                            continue;
                        }
                        text
                    }
                };
                prompt_parts.push(part);
            }
            let prompt = prompt_parts.join("\n\n");

            let system_prompt = if embed || system_parts.is_empty() {
                None
            } else {
                Some(system_parts.join("\n"))
            };

            let config = make_config(&state);
            let options = subprocess::SubprocessOptions {
                request_id: request_id.clone(),
                model: model.to_string(),
                system_prompt,
                cwd: state.cwd.clone(),
                max_turns: None,
                replace_system_prompt: !embed,
                effort: config.effort,
                disable_tools: false,
            };

            if stream {
                handle_openai_streaming(request_id, options, prompt, model).await
            } else {
                handle_openai_non_streaming(request_id, options, prompt, model).await
            }
        }
    }
}

fn log_tools_warning(request_id: &str, request: &MessagesRequest) {
    if let Some(ref tools) = request.tools
        && !tools.is_empty()
    {
        warn!(
            "[req={request_id}] Request contains {} tool definitions — \
                 ignored (CLI uses built-in tools: Read, Edit, Bash, Grep, Glob, etc.)",
            tools.len()
        );
    }
}

fn make_config(state: &AppState) -> request::ProxyConfig {
    request::ProxyConfig {
        max_turns: state.max_turns,
        replace_system_prompt: state.replace_system_prompt,
        effort: state.effort.clone(),
        embed_system_prompt: state.embed_system_prompt,
    }
}

// ---------------------------------------------------------------------------
// Anthropic streaming (event: name\ndata: json\n\n)
// ---------------------------------------------------------------------------

fn format_sse_event(event_name: &str, data: &str) -> Vec<u8> {
    format!("event: {event_name}\ndata: {data}\n\n").into_bytes()
}

async fn handle_anthropic_streaming(
    request_id: String,
    options: subprocess::SubprocessOptions,
    prompt: String,
    model: &str,
) -> Result<Response, AppError> {
    let (sub_tx, mut sub_rx) = mpsc::channel::<subprocess::SubprocessEvent>(64);
    tokio::spawn(async move {
        subprocess::spawn_subprocess(prompt, options, sub_tx).await;
    });

    let (bytes_tx, bytes_rx) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>(64);
    let rid = request_id.clone();
    let model = model.to_string();

    tokio::spawn(async move {
        let mut state = response::StreamState::new(rid.clone(), model);
        while let Some(event) = sub_rx.recv().await {
            let sse_events = response::cli_event_to_sse(&event, &mut state);
            for (event_name, data) in sse_events {
                let bytes = format_sse_event(&event_name, &data);
                if bytes_tx.send(Ok(bytes)).await.is_err() {
                    return;
                }
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(bytes_rx);
    let body = Body::from_stream(stream);

    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header("x-request-id", &request_id)
        .body(body)
        .unwrap())
}

// ---------------------------------------------------------------------------
// OpenAI streaming (data: json\n\ndata: [DONE]\n\n)
// ---------------------------------------------------------------------------

fn format_openai_sse(data: &str) -> Vec<u8> {
    format!("data: {data}\n\n").into_bytes()
}

async fn handle_openai_streaming(
    request_id: String,
    options: subprocess::SubprocessOptions,
    prompt: String,
    model: &str,
) -> Result<Response, AppError> {
    let (sub_tx, mut sub_rx) = mpsc::channel::<subprocess::SubprocessEvent>(64);
    tokio::spawn(async move {
        subprocess::spawn_subprocess(prompt, options, sub_tx).await;
    });

    let (bytes_tx, bytes_rx) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>(64);
    let rid = request_id.clone();
    let model = model.to_string();
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    tokio::spawn(async move {
        // Send initial SSE comment to confirm connection (matches old proxy behavior)
        let _ = bytes_tx.send(Ok(b":ok\n\n".to_vec())).await;

        let mut sent_role = false;
        #[allow(unused_assignments)]
        let mut output_tokens: u64 = 0;
        #[allow(unused_assignments)]
        let mut input_tokens: u64 = 0;

        while let Some(event) = sub_rx.recv().await {
            match event {
                subprocess::SubprocessEvent::StreamEvent {
                    event_type,
                    payload,
                } => {
                    // Only forward text_delta content (skip thinking, tool_use, etc.)
                    if event_type == "content_block_delta"
                        && let Some(text) = payload
                            .get("delta")
                            .and_then(|d| d.get("text"))
                            .and_then(|t| t.as_str())
                    {
                        // Build delta without role: null (omit key like OpenAI spec)
                        let mut delta = serde_json::Map::new();
                        if !sent_role {
                            sent_role = true;
                            delta.insert("role".to_string(), json!("assistant"));
                        }
                        delta.insert("content".to_string(), json!(text));

                        let chunk = json!({
                            "id": format!("chatcmpl-{rid}"),
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": delta,
                                "finish_reason": serde_json::Value::Null,
                            }]
                        });
                        let bytes = format_openai_sse(&serde_json::to_string(&chunk).unwrap());
                        if bytes_tx.send(Ok(bytes)).await.is_err() {
                            return;
                        }
                    }
                }
                subprocess::SubprocessEvent::Result(data) => {
                    input_tokens = data.usage.input_tokens.unwrap_or(0);
                    output_tokens = data.usage.output_tokens.unwrap_or(0);

                    // If no text was streamed (e.g. multi-turn tool use),
                    // send the final result text as one chunk
                    if !sent_role
                        && let Some(ref text) = data.result
                        && !text.is_empty()
                    {
                        let chunk = json!({
                            "id": format!("chatcmpl-{rid}"),
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": { "role": "assistant", "content": text },
                                "finish_reason": serde_json::Value::Null,
                            }]
                        });
                        let bytes = format_openai_sse(&serde_json::to_string(&chunk).unwrap());
                        let _ = bytes_tx.send(Ok(bytes)).await;
                    }

                    let done_chunk = json!({
                        "id": format!("chatcmpl-{rid}"),
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": {},
                            "finish_reason": "stop",
                        }],
                        "usage": {
                            "prompt_tokens": input_tokens,
                            "completion_tokens": output_tokens,
                            "total_tokens": input_tokens + output_tokens,
                        }
                    });
                    let bytes = format_openai_sse(&serde_json::to_string(&done_chunk).unwrap());
                    let _ = bytes_tx.send(Ok(bytes)).await;
                    let _ = bytes_tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
                    return;
                }
                subprocess::SubprocessEvent::ProcessError(msg) => {
                    let err = json!({"error": {"message": msg, "type": "server_error"}});
                    let bytes = format_openai_sse(&serde_json::to_string(&err).unwrap());
                    let _ = bytes_tx.send(Ok(bytes)).await;
                    let _ = bytes_tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
                    return;
                }
                subprocess::SubprocessEvent::CliError { errors } => {
                    // If text was already streamed, finish gracefully instead
                    // of sending an error (e.g. max_turns reached after text output).
                    if sent_role {
                        let done_chunk = json!({
                            "id": format!("chatcmpl-{rid}"),
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
                        });
                        let bytes = format_openai_sse(&serde_json::to_string(&done_chunk).unwrap());
                        let _ = bytes_tx.send(Ok(bytes)).await;
                        let _ = bytes_tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
                        return;
                    }
                    let msg = errors.join("; ");
                    let err_msg = if msg.is_empty() {
                        "CLI error".to_string()
                    } else {
                        msg
                    };
                    let err = json!({"error": {"message": err_msg, "type": "server_error"}});
                    let bytes = format_openai_sse(&serde_json::to_string(&err).unwrap());
                    let _ = bytes_tx.send(Ok(bytes)).await;
                    let _ = bytes_tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
                    return;
                }
                _ => {} // Init, Close — skip
            }
        }

        // Stream ended without Result — send DONE anyway
        let _ = bytes_tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(bytes_rx);
    let body = Body::from_stream(stream);

    // Send :ok comment first, then SSE data (matches old proxy behavior)
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header("x-request-id", &request_id)
        .body(body)
        .unwrap())
}

async fn handle_openai_non_streaming(
    request_id: String,
    options: subprocess::SubprocessOptions,
    prompt: String,
    model: &str,
) -> Result<Response, AppError> {
    let (sub_tx, mut sub_rx) = mpsc::channel::<subprocess::SubprocessEvent>(64);
    tokio::spawn(async move {
        subprocess::spawn_subprocess(prompt, options, sub_tx).await;
    });

    let mut state = response::StreamState::new(request_id.clone(), model.to_string());
    let mut last_error: Option<String> = None;

    while let Some(event) = sub_rx.recv().await {
        let _ = response::cli_event_to_sse(&event, &mut state);
        match &event {
            subprocess::SubprocessEvent::ProcessError(msg) => last_error = Some(msg.clone()),
            subprocess::SubprocessEvent::CliError { errors } => {
                last_error = Some(errors.join("; "))
            }
            _ => {}
        }
    }

    if let Some(err) = last_error {
        error!("[req={request_id}] Error: {err}");
        return Err(AppError::Subprocess(err));
    }

    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let resp = json!({
        "id": format!("chatcmpl-{request_id}"),
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": state.collected_text,
            },
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": state.usage.input_tokens,
            "completion_tokens": state.usage.output_tokens,
            "total_tokens": state.usage.input_tokens + state.usage.output_tokens,
        }
    });

    Ok(Json(resp).into_response())
}

async fn handle_non_streaming(
    request_id: String,
    options: subprocess::SubprocessOptions,
    prompt: String,
    model: &str,
) -> Result<Response, AppError> {
    let (sub_tx, mut sub_rx) = mpsc::channel::<subprocess::SubprocessEvent>(64);
    tokio::spawn(async move {
        subprocess::spawn_subprocess(prompt, options, sub_tx).await;
    });

    let mut state = response::StreamState::new(request_id.clone(), model.to_string());
    let mut last_error: Option<String> = None;

    while let Some(event) = sub_rx.recv().await {
        let _ = response::cli_event_to_sse(&event, &mut state);
        match &event {
            subprocess::SubprocessEvent::ProcessError(msg) => last_error = Some(msg.clone()),
            subprocess::SubprocessEvent::CliError { errors } => {
                last_error = Some(errors.join("; "))
            }
            _ => {}
        }
    }

    if let Some(err) = last_error {
        error!("[req={request_id}] Error: {err}");
        return Err(AppError::Subprocess(err));
    }

    let resp = response::build_non_streaming_response(&state);
    Ok(Json(resp).into_response())
}

// ---------------------------------------------------------------------------
// Native mode: OpenAI format handlers
// ---------------------------------------------------------------------------

async fn handle_native_openai_streaming(
    client: &native::NativeClient,
    anthropic_body: serde_json::Value,
    request_id: &str,
    model: &str,
) -> Result<Response, AppError> {
    // Forward to Anthropic API as streaming, get raw reqwest response
    let resp = client
        .send_raw(anthropic_body, "/v1/messages", request_id)
        .await?;

    // The response is Anthropic SSE — we need to convert to OpenAI SSE format
    let (bytes_tx, bytes_rx) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>(64);
    let model = model.to_string();
    let rid = request_id.to_string();
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Read the Anthropic SSE body and convert to OpenAI format
    tokio::spawn(async move {
        use futures::StreamExt;

        let _ = bytes_tx.send(Ok(b":ok\n\n".to_vec())).await;

        let mut sent_role = false;
        let mut buffer = String::new();
        let mut current_block_is_text = false;

        let mut byte_stream = resp.bytes_stream();

        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(_) => break,
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE events from buffer
            while let Some(event_end) = buffer.find("\n\n") {
                let event_text = buffer[..event_end].to_string();
                buffer = buffer[event_end + 2..].to_string();

                // Parse SSE event
                let mut event_type = String::new();
                let mut data = String::new();
                for line in event_text.lines() {
                    if let Some(t) = line.strip_prefix("event: ") {
                        event_type = t.to_string();
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data = d.to_string();
                    }
                }

                if data.is_empty() {
                    continue;
                }

                let payload: serde_json::Value = match serde_json::from_str(&data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                match event_type.as_str() {
                    "content_block_start" => {
                        let block_type = payload
                            .get("content_block")
                            .and_then(|b| b.get("type"))
                            .and_then(|t| t.as_str());
                        current_block_is_text = block_type == Some("text");
                    }
                    "content_block_delta" if current_block_is_text => {
                        if let Some(text) = payload
                            .get("delta")
                            .and_then(|d| d.get("text"))
                            .and_then(|t| t.as_str())
                        {
                            let mut delta = serde_json::Map::new();
                            if !sent_role {
                                sent_role = true;
                                delta.insert("role".to_string(), json!("assistant"));
                            }
                            delta.insert("content".to_string(), json!(text));

                            let chunk = json!({
                                "id": format!("chatcmpl-{rid}"),
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [{
                                    "index": 0,
                                    "delta": delta,
                                    "finish_reason": serde_json::Value::Null,
                                }]
                            });
                            let bytes =
                                format_openai_sse(&serde_json::to_string(&chunk).unwrap());
                            if bytes_tx.send(Ok(bytes)).await.is_err() {
                                return;
                            }
                        }
                    }
                    "message_delta" => {
                        let output_tokens = payload
                            .get("usage")
                            .and_then(|u| u.get("output_tokens"))
                            .and_then(|t| t.as_u64())
                            .unwrap_or(0);

                        let done_chunk = json!({
                            "id": format!("chatcmpl-{rid}"),
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": {},
                                "finish_reason": "stop",
                            }],
                            "usage": {
                                "prompt_tokens": 0,
                                "completion_tokens": output_tokens,
                                "total_tokens": output_tokens,
                            }
                        });
                        let bytes =
                            format_openai_sse(&serde_json::to_string(&done_chunk).unwrap());
                        let _ = bytes_tx.send(Ok(bytes)).await;
                        let _ = bytes_tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
                        return;
                    }
                    _ => {} // Skip other events
                }
            }
        }

        // Stream ended without message_delta
        let _ = bytes_tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(bytes_rx);
    let body = Body::from_stream(stream);

    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header("x-request-id", request_id)
        .body(body)
        .unwrap())
}

async fn handle_native_openai_non_streaming(
    client: &native::NativeClient,
    mut anthropic_body: serde_json::Value,
    request_id: &str,
    model: &str,
) -> Result<Response, AppError> {
    // Ensure non-streaming
    anthropic_body["stream"] = json!(false);

    let resp = client
        .forward(anthropic_body, "/v1/messages", request_id)
        .await?;

    // Read the Anthropic JSON response and convert to OpenAI format
    let body_bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|e| AppError::Upstream(502, format!("Failed to read response: {e}")))?;

    let anthropic_resp: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| AppError::Upstream(502, format!("Invalid JSON from upstream: {e}")))?;

    // Extract text from content blocks
    let text = anthropic_resp
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        b.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    let input_tokens = anthropic_resp
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let output_tokens = anthropic_resp
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);

    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let openai_resp = json!({
        "id": format!("chatcmpl-{request_id}"),
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": text,
            },
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": input_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        }
    });

    Ok(Json(openai_resp).into_response())
}

/// Fallback handler: 404 in subprocess mode, forward to upstream in native mode.
pub async fn fallback(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Result<Response, AppError> {
    match state.mode {
        ProxyMode::Native => {
            let client = state.native_client.as_ref().unwrap();
            let path = req.uri().path().to_string();
            let method = req.method().clone();
            let request_id = uuid::Uuid::new_v4().to_string()[..8].to_string();

            info!("[req={request_id}] {method} {path} (native forward)");

            let body_bytes = axum::body::to_bytes(req.into_body(), 10_485_760)
                .await
                .map_err(|e| AppError::BadRequest(format!("Failed to read body: {e}")))?;

            let body: serde_json::Value = if body_bytes.is_empty() {
                json!({})
            } else {
                serde_json::from_slice(&body_bytes).unwrap_or(json!({}))
            };

            client.forward(body, &path, &request_id).await
        }
        ProxyMode::Subprocess => Ok((
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({
                "type": "error",
                "error": {
                    "type": "not_found_error",
                    "message": "Not found"
                }
            })),
        )
            .into_response()),
    }
}
