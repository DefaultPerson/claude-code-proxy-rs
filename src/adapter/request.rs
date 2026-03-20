//! Convert Anthropic Messages API request → CLI subprocess args + stdin prompt.

use crate::subprocess::SubprocessOptions;
use crate::types::anthropic::MessagesRequest;

/// Prepare subprocess options and the stdin prompt from an Anthropic request.
pub struct ProxyConfig {
    pub max_turns: u32,
    pub replace_system_prompt: bool,
    pub effort: Option<String>,
    pub embed_system_prompt: bool,
}

pub fn prepare_subprocess(
    request: &MessagesRequest,
    request_id: String,
    cwd: &str,
    config: &ProxyConfig,
) -> (SubprocessOptions, String) {
    let system_text = request.system.as_ref().map(|s| s.to_text());

    if config.embed_system_prompt {
        // Embed system prompt in prompt text with <system> tags.
        // Keeps Claude Code's default 43K system prompt intact.
        let mut prompt_parts = Vec::new();
        if let Some(ref sys) = system_text {
            if !sys.is_empty() {
                prompt_parts.push(format!("<system>\n{sys}\n</system>"));
            }
        }
        prompt_parts.push(messages_to_prompt(&request.messages));
        let prompt = prompt_parts.join("\n\n");

        let options = SubprocessOptions {
            request_id,
            model: request.model.clone(),
            system_prompt: None,
            cwd: cwd.to_string(),
            max_turns: None,
            replace_system_prompt: false,
            effort: config.effort.clone(),
            disable_tools: false,
        };
        (options, prompt)
    } else {
        let prompt = messages_to_prompt(&request.messages);
        let options = SubprocessOptions {
            request_id,
            model: request.model.clone(),
            system_prompt: system_text,
            cwd: cwd.to_string(),
            max_turns: None,
            replace_system_prompt: config.replace_system_prompt,
            effort: config.effort.clone(),
            disable_tools: false,
        };
        (options, prompt)
    }
}

/// Convert the messages array into a text prompt for CLI stdin.
///
/// For a single user message, just pass the text directly.
/// For multi-turn conversations, format as a readable conversation.
fn messages_to_prompt(messages: &[crate::types::anthropic::Message]) -> String {
    if messages.is_empty() {
        return String::new();
    }

    // Single user message — pass text directly (most common case)
    if messages.len() == 1 && messages[0].role == "user" {
        return messages[0].content.to_text();
    }

    // Multi-turn: format as conversation with role markers
    let mut parts = Vec::new();

    for msg in messages {
        let text = msg.content.to_text();
        if text.is_empty() {
            continue;
        }

        match msg.role.as_str() {
            "user" => parts.push(text),
            "assistant" => {
                parts.push(format!(
                    "<assistant_response>\n{text}\n</assistant_response>"
                ));
            }
            _ => parts.push(text),
        }
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::anthropic::{Content, Message};

    #[test]
    fn single_user_message() {
        let messages = vec![Message {
            role: "user".to_string(),
            content: Content::Text("Hello world".to_string()),
        }];
        let prompt = messages_to_prompt(&messages);
        assert_eq!(prompt, "Hello world");
    }

    #[test]
    fn multi_turn_conversation() {
        let messages = vec![
            Message {
                role: "user".to_string(),
                content: Content::Text("Hi".to_string()),
            },
            Message {
                role: "assistant".to_string(),
                content: Content::Text("Hello!".to_string()),
            },
            Message {
                role: "user".to_string(),
                content: Content::Text("How are you?".to_string()),
            },
        ];
        let prompt = messages_to_prompt(&messages);
        assert!(prompt.contains("Hi"));
        assert!(prompt.contains("<assistant_response>\nHello!\n</assistant_response>"));
        assert!(prompt.contains("How are you?"));
    }

    #[test]
    fn empty_messages() {
        let prompt = messages_to_prompt(&[]);
        assert_eq!(prompt, "");
    }
}
