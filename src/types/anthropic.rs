//! Anthropic Messages API types (request from OpenClaw, response to OpenClaw).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request types (what OpenClaw sends us)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u64,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub stream: bool,
    pub system: Option<SystemPrompt>,
    pub metadata: Option<Metadata>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub stop_sequences: Option<Vec<String>>,
    pub tools: Option<Vec<serde_json::Value>>,
    pub tool_choice: Option<serde_json::Value>,
}

/// System prompt: either a plain string or array of content blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

#[derive(Debug, Deserialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Metadata {
    pub user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Content,
}

/// Message content: either a plain string or array of content blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Content blocks in messages. We use untagged deserialization with a typed
/// struct first, falling back to raw Value for unknown block types (thinking,
/// redacted_thinking, etc.) that OpenClaw may include in conversation history.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ContentBlock {
    Typed(TypedContentBlock),
    /// Catch-all for unknown block types (thinking, redacted_thinking, etc.)
    Unknown(serde_json::Value),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum TypedContentBlock {
    #[serde(rename = "text")]
    Text { text: String },

    #[serde(rename = "image")]
    Image {
        source: Option<serde_json::Value>,
    },

    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: Option<serde_json::Value>,
    },
}

// ---------------------------------------------------------------------------
// Response types (what we send to OpenClaw)
// ---------------------------------------------------------------------------

/// Non-streaming response.
#[derive(Debug, Serialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub response_type: String,
    pub role: String,
    pub content: Vec<ResponseContentBlock>,
    pub model: String,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: ResponseUsage,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ResponseContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct ResponseUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub cache_creation_input_tokens: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub cache_read_input_tokens: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// Error response matching Anthropic error format.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    #[serde(rename = "type")]
    pub response_type: String,
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Helpers for extracting text from request content
// ---------------------------------------------------------------------------

impl SystemPrompt {
    pub fn to_text(&self) -> String {
        match self {
            SystemPrompt::Text(s) => s.clone(),
            SystemPrompt::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| b.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

impl Content {
    pub fn to_text(&self) -> String {
        match self {
            Content::Text(s) => s.clone(),
            Content::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Typed(typed) => match typed {
                        TypedContentBlock::Text { text } => Some(text.clone()),
                        TypedContentBlock::ToolUse { id, name, input } => {
                            Some(format!(
                                "[Tool call: {name} (id: {id})]\n{}\n[End tool call]",
                                serde_json::to_string_pretty(input).unwrap_or_default()
                            ))
                        }
                        TypedContentBlock::ToolResult {
                            tool_use_id,
                            content,
                        } => {
                            let text = content
                                .as_ref()
                                .map(|v| {
                                    v.as_str().map(|s| s.to_string()).unwrap_or_else(|| {
                                        serde_json::to_string(v).unwrap_or_default()
                                    })
                                })
                                .unwrap_or_default();
                            Some(format!(
                                "[Tool result for {tool_use_id}]\n{text}\n[End tool result]"
                            ))
                        }
                        TypedContentBlock::Image { .. } => None,
                    },
                    // Unknown block types (thinking, redacted_thinking, etc.)
                    // Skip them — they're internal reasoning, not user-visible content
                    ContentBlock::Unknown(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

impl MessagesResponse {
    pub fn new(
        id: String,
        model: String,
        text: String,
        stop_reason: Option<String>,
        usage: ResponseUsage,
    ) -> Self {
        Self {
            id,
            response_type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![ResponseContentBlock::Text { text }],
            model,
            stop_reason,
            stop_sequence: None,
            usage,
        }
    }
}

impl ErrorResponse {
    pub fn new(error_type: &str, message: String) -> Self {
        Self {
            response_type: "error".to_string(),
            error: ErrorDetail {
                error_type: error_type.to_string(),
                message,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_request() {
        let json = r#"{"model":"claude-sonnet-4-6","max_tokens":100,"messages":[{"role":"user","content":"Hello"}],"stream":true}"#;
        let req: MessagesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "claude-sonnet-4-6");
        assert_eq!(req.max_tokens, 100);
        assert!(req.stream);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].content.to_text(), "Hello");
    }

    #[test]
    fn parse_request_with_system_string() {
        let json = r#"{"model":"claude-sonnet-4-6","max_tokens":100,"messages":[{"role":"user","content":"Hi"}],"system":"You are helpful"}"#;
        let req: MessagesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.system.unwrap().to_text(), "You are helpful");
    }

    #[test]
    fn parse_request_with_system_blocks() {
        let json = r#"{"model":"claude-sonnet-4-6","max_tokens":100,"messages":[{"role":"user","content":"Hi"}],"system":[{"type":"text","text":"Block 1"},{"type":"text","text":"Block 2"}]}"#;
        let req: MessagesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.system.unwrap().to_text(), "Block 1\nBlock 2");
    }

    #[test]
    fn parse_request_with_content_blocks() {
        let json = r#"{"model":"claude-sonnet-4-6","max_tokens":100,"messages":[{"role":"user","content":[{"type":"text","text":"What is this?"}]}]}"#;
        let req: MessagesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.messages[0].content.to_text(), "What is this?");
    }

    #[test]
    fn parse_request_with_tool_result() {
        let json = r#"{"model":"claude-sonnet-4-6","max_tokens":100,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"result text"}]}]}"#;
        let req: MessagesRequest = serde_json::from_str(json).unwrap();
        let text = req.messages[0].content.to_text();
        assert!(text.contains("result text"));
        assert!(text.contains("[Tool result for toolu_01]"));
    }

    #[test]
    fn serialize_response() {
        let resp = MessagesResponse::new(
            "msg_123".to_string(),
            "claude-sonnet-4-6".to_string(),
            "Hello!".to_string(),
            Some("end_turn".to_string()),
            ResponseUsage::default(),
        );
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"message\""));
        assert!(json.contains("\"text\":\"Hello!\""));
    }

    #[test]
    fn serialize_error_response() {
        let err = ErrorResponse::new("invalid_request_error", "Missing model".to_string());
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("\"type\":\"error\""));
        assert!(json.contains("invalid_request_error"));
    }

    #[test]
    fn content_to_text_includes_tool_use() {
        let json = r#"{"model":"claude-sonnet-4-6","max_tokens":100,"messages":[{"role":"assistant","content":[{"type":"text","text":"Let me check."},{"type":"tool_use","id":"toolu_01","name":"get_weather","input":{"city":"SF"}}]}]}"#;
        let req: MessagesRequest = serde_json::from_str(json).unwrap();
        let text = req.messages[0].content.to_text();
        assert!(text.contains("Let me check."));
        assert!(text.contains("[Tool call: get_weather"));
        assert!(text.contains("SF"));
    }

    #[test]
    fn content_to_text_includes_tool_result() {
        let json = r#"{"model":"claude-sonnet-4-6","max_tokens":100,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"72°F sunny"}]}]}"#;
        let req: MessagesRequest = serde_json::from_str(json).unwrap();
        let text = req.messages[0].content.to_text();
        assert!(text.contains("[Tool result for toolu_01]"));
        assert!(text.contains("72°F sunny"));
    }
}
