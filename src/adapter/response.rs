//! Convert CLI subprocess events → Anthropic SSE events.

use serde_json::json;

use crate::subprocess::SubprocessEvent;
use crate::types::anthropic::{MessagesResponse, ResponseContentBlock, ResponseUsage};
use crate::types::cli::Usage;

/// Mutable state tracked during streaming conversion.
pub struct StreamState {
    pub sent_message_start: bool,
    pub model: String,
    pub request_id: String,
    pub collected_text: String,
    pub usage: ResponseUsage,
    pub stop_reason: Option<String>,
    pub sent_message_stop: bool,
    pub current_block_is_text: bool,
}

impl StreamState {
    pub fn new(request_id: String, model: String) -> Self {
        Self {
            sent_message_start: false,
            sent_message_stop: false,
            current_block_is_text: false,
            model,
            request_id,
            collected_text: String::new(),
            usage: ResponseUsage::default(),
            stop_reason: None,
        }
    }
}

/// An SSE event to send to the client: (event_name, json_data).
pub type SseEvent = (String, String);

/// Allowed event types to forward from CLI stream_event.
const FORWARD_EVENT_TYPES: &[&str] = &[
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "ping",
];

/// Convert a subprocess event into zero or more SSE events.
pub fn cli_event_to_sse(event: &SubprocessEvent, state: &mut StreamState) -> Vec<SseEvent> {
    match event {
        SubprocessEvent::Init { model, .. } => {
            state.model = model.clone();
            emit_message_start(state)
        }

        SubprocessEvent::StreamEvent {
            event_type,
            payload,
        } => {
            // Ensure message_start was sent
            let mut events = if !state.sent_message_start {
                emit_message_start(state)
            } else {
                vec![]
            };

            // Only forward TEXT content blocks and ping.
            // Skip thinking, tool_use, signature blocks — they crash
            // OpenClaw's Anthropic SDK (fine-grained-tool-streaming beta).
            match event_type.as_str() {
                "content_block_start" => {
                    let block_type = payload
                        .get("content_block")
                        .and_then(|b| b.get("type"))
                        .and_then(|t| t.as_str());
                    if block_type == Some("text") {
                        state.current_block_is_text = true;
                        let data = serde_json::to_string(payload).unwrap_or_default();
                        events.push((event_type.clone(), data));
                    } else {
                        state.current_block_is_text = false;
                    }
                }
                "content_block_delta" => {
                    if state.current_block_is_text {
                        if let Some(text) = payload
                            .get("delta")
                            .and_then(|d| d.get("text"))
                            .and_then(|t| t.as_str())
                        {
                            state.collected_text.push_str(text);
                        }
                        let data = serde_json::to_string(payload).unwrap_or_default();
                        events.push((event_type.clone(), data));
                    }
                }
                "content_block_stop" => {
                    if state.current_block_is_text {
                        let data = serde_json::to_string(payload).unwrap_or_default();
                        events.push((event_type.clone(), data));
                    }
                }
                "ping" => {
                    let data = serde_json::to_string(payload).unwrap_or_default();
                    events.push((event_type.clone(), data));
                }
                _ => {} // Skip CLI's message_start/delta/stop
            }

            events
        }

        SubprocessEvent::Result(data) => {
            state.usage = usage_from_cli(&data.usage);
            state.stop_reason = data.stop_reason.clone();

            // Guard: don't emit message_stop twice
            if state.sent_message_stop {
                return vec![];
            }

            let mut events = vec![];

            // If no streaming happened, emit message_start first
            if !state.sent_message_start {
                events.extend(emit_message_start(state));
            }

            // Emit message_delta with final stop_reason and usage
            let stop_reason = data.stop_reason.as_deref().unwrap_or("end_turn");
            let delta_payload = json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": null
                },
                "usage": {
                    "output_tokens": state.usage.output_tokens
                }
            });
            events.push((
                "message_delta".to_string(),
                serde_json::to_string(&delta_payload).unwrap(),
            ));

            // Emit message_stop
            let stop_payload = json!({"type": "message_stop"});
            events.push((
                "message_stop".to_string(),
                serde_json::to_string(&stop_payload).unwrap(),
            ));

            state.sent_message_stop = true;
            events
        }

        SubprocessEvent::CliError { errors } => {
            let msg = errors.join("; ");
            let payload = json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": msg
                }
            });
            vec![(
                "error".to_string(),
                serde_json::to_string(&payload).unwrap(),
            )]
        }

        SubprocessEvent::ProcessError(msg) => {
            let payload = json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": msg
                }
            });
            vec![(
                "error".to_string(),
                serde_json::to_string(&payload).unwrap(),
            )]
        }

        SubprocessEvent::Close(code) => {
            // If message_stop was already sent by Result handler, nothing to do.
            if state.sent_message_stop {
                return vec![];
            }

            let mut events = vec![];

            // Safety net: if CLI exited without sending a result event
            // (known issue: CLI sometimes omits result — GitHub #8126),
            // emit proper SSE termination so the client doesn't hang.
            if state.sent_message_start {
                let stop_reason = if *code == 0 { "end_turn" } else { "end_turn" };
                let delta = json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": stop_reason, "stop_sequence": null },
                    "usage": { "output_tokens": state.usage.output_tokens }
                });
                events.push((
                    "message_delta".to_string(),
                    serde_json::to_string(&delta).unwrap(),
                ));
                events.push((
                    "message_stop".to_string(),
                    serde_json::to_string(&json!({"type": "message_stop"})).unwrap(),
                ));
            }

            // If CLI exited with error before any streaming started
            if !state.sent_message_start && *code != 0 {
                let payload = json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": format!("CLI exited with code {code} without producing output")
                    }
                });
                events.push((
                    "error".to_string(),
                    serde_json::to_string(&payload).unwrap(),
                ));
            }

            events
        }
    }
}

/// Build a non-streaming response from collected stream state.
pub fn build_non_streaming_response(state: &StreamState) -> MessagesResponse {
    MessagesResponse {
        id: format!("msg_{}", &state.request_id),
        response_type: "message".to_string(),
        role: "assistant".to_string(),
        content: vec![ResponseContentBlock::Text {
            text: state.collected_text.clone(),
        }],
        model: state.model.clone(),
        stop_reason: state.stop_reason.clone().or(Some("end_turn".to_string())),
        stop_sequence: None,
        usage: state.usage.clone(),
    }
}

fn emit_message_start(state: &mut StreamState) -> Vec<SseEvent> {
    state.sent_message_start = true;

    let msg_id = format!("msg_{}", &state.request_id);
    let payload = json!({
        "type": "message_start",
        "message": {
            "id": msg_id,
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": state.model,
            "stop_reason": null,
            "stop_sequence": null,
            "usage": {
                "input_tokens": 0,
                "output_tokens": 1
            }
        }
    });

    let ping = json!({"type": "ping"});

    vec![
        (
            "message_start".to_string(),
            serde_json::to_string(&payload).unwrap(),
        ),
        ("ping".to_string(), serde_json::to_string(&ping).unwrap()),
    ]
}

fn usage_from_cli(cli_usage: &Usage) -> ResponseUsage {
    ResponseUsage {
        input_tokens: cli_usage.input_tokens.unwrap_or(0),
        output_tokens: cli_usage.output_tokens.unwrap_or(0),
        cache_creation_input_tokens: cli_usage.cache_creation_input_tokens.unwrap_or(0),
        cache_read_input_tokens: cli_usage.cache_read_input_tokens.unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subprocess::ResultData;
    use crate::types::cli::Usage;

    #[test]
    fn init_emits_message_start_and_ping() {
        let mut state = StreamState::new("test-123".to_string(), "unknown".to_string());
        let event = SubprocessEvent::Init {
            session_id: "sess-1".to_string(),
            model: "claude-sonnet-4-6".to_string(),
        };

        let sse = cli_event_to_sse(&event, &mut state);
        assert_eq!(sse.len(), 2);
        assert_eq!(sse[0].0, "message_start");
        assert_eq!(sse[1].0, "ping");
        assert!(state.sent_message_start);
        assert_eq!(state.model, "claude-sonnet-4-6");
    }

    #[test]
    fn stream_event_forwards_content_block_delta() {
        let mut state = StreamState::new("test".to_string(), "model".to_string());
        state.sent_message_start = true;
        state.current_block_is_text = true;

        let payload = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "Hello"}
        });

        let event = SubprocessEvent::StreamEvent {
            event_type: "content_block_delta".to_string(),
            payload,
        };

        let sse = cli_event_to_sse(&event, &mut state);
        assert_eq!(sse.len(), 1);
        assert_eq!(sse[0].0, "content_block_delta");
        assert!(sse[0].1.contains("Hello"));
        assert_eq!(state.collected_text, "Hello");
    }

    #[test]
    fn stream_event_skips_message_start() {
        let mut state = StreamState::new("test".to_string(), "model".to_string());
        state.sent_message_start = true;

        let event = SubprocessEvent::StreamEvent {
            event_type: "message_start".to_string(),
            payload: json!({"type": "message_start"}),
        };

        let sse = cli_event_to_sse(&event, &mut state);
        assert!(sse.is_empty());
    }

    #[test]
    fn result_emits_delta_and_stop() {
        let mut state = StreamState::new("test".to_string(), "model".to_string());
        state.sent_message_start = true;

        let event = SubprocessEvent::Result(ResultData {
            result: Some("Done".to_string()),
            stop_reason: Some("end_turn".to_string()),
            usage: Usage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                ..Default::default()
            },
            session_id: Some("sess".to_string()),
            num_turns: Some(1),
            duration_ms: Some(5000),
            total_cost_usd: Some(0.01),
        });

        let sse = cli_event_to_sse(&event, &mut state);
        assert_eq!(sse.len(), 2);
        assert_eq!(sse[0].0, "message_delta");
        assert!(sse[0].1.contains("end_turn"));
        assert!(sse[0].1.contains("50")); // output_tokens
        assert_eq!(sse[1].0, "message_stop");
    }

    #[test]
    fn error_emits_error_event() {
        let mut state = StreamState::new("test".to_string(), "model".to_string());

        let event = SubprocessEvent::CliError {
            errors: vec!["Max turns reached".to_string()],
        };

        let sse = cli_event_to_sse(&event, &mut state);
        assert_eq!(sse.len(), 1);
        assert_eq!(sse[0].0, "error");
        assert!(sse[0].1.contains("Max turns reached"));
    }
}
