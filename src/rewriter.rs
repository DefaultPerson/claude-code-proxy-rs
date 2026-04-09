//! Identity normalization for native mode.
//!
//! Port of cc-gateway/src/rewriter.ts (353 lines).
//! Rewrites request bodies and headers to present a canonical device identity.

use axum::http::HeaderMap;
use rand::Rng;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::config::NativeConfig;

// CCH hash algorithm constants (reverse-engineered from CLI)
const CCH_SALT: &str = "59cf53e54c78";
const CCH_POSITIONS: [usize; 3] = [4, 7, 20];

/// Headers to strip when forwarding to upstream.
#[allow(dead_code)]
const STRIP_HEADERS: &[&str] = &[
    "host",
    "connection",
    "proxy-authorization",
    "proxy-connection",
    "transfer-encoding",
    "authorization",
    "x-api-key",
    "x-anthropic-billing-header",
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Rewrite identity fields in the request body based on the URL path.
pub fn rewrite_body(body: &mut serde_json::Value, path: &str, config: &NativeConfig) {
    if path.starts_with("/v1/messages") {
        rewrite_messages_body(body, config);
    } else if path.contains("/event_logging/batch") {
        rewrite_event_batch(body, config);
    } else if path.contains("/policy_limits") || path.contains("/settings") {
        rewrite_generic_identity(body, config);
    }
}

/// Rewrite HTTP headers: strip auth/billing, set canonical User-Agent.
#[allow(dead_code)]
pub fn rewrite_headers(headers: &HeaderMap, config: &NativeConfig) -> HeaderMap {
    let mut out = HeaderMap::new();
    let version = config
        .env
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("2.1.81");

    for (key, value) in headers.iter() {
        let lower = key.as_str().to_lowercase();

        if STRIP_HEADERS.contains(&lower.as_str()) {
            continue;
        }

        if lower == "user-agent" {
            out.insert(
                key.clone(),
                format!("claude-code/{version} (external, cli)")
                    .parse()
                    .unwrap(),
            );
        } else {
            out.insert(key.clone(), value.clone());
        }
    }

    out
}

// ---------------------------------------------------------------------------
// /v1/messages rewriting
// ---------------------------------------------------------------------------

fn rewrite_messages_body(body: &mut serde_json::Value, config: &NativeConfig) {
    // 1. Rewrite metadata.user_id (JSON string containing {device_id, ...})
    if let Some(user_id_str) = body
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
    {
        if let Ok(mut user_id) = serde_json::from_str::<serde_json::Value>(&user_id_str) {
            if user_id.get("device_id").is_some() {
                user_id["device_id"] = serde_json::Value::String(config.identity.device_id.clone());
                body["metadata"]["user_id"] =
                    serde_json::Value::String(serde_json::to_string(&user_id).unwrap());
                debug!("Rewrote metadata.user_id device_id");
            }
        } else {
            warn!("Failed to parse metadata.user_id");
        }
    }

    // 2. Rewrite <system-reminder> blocks in messages
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            if let Some(content) = msg.get_mut("content") {
                if let Some(s) = content.as_str().map(|s| s.to_string()) {
                    *content = serde_json::Value::String(rewrite_system_reminders(&s, config));
                } else if let Some(blocks) = content.as_array_mut() {
                    for block in blocks.iter_mut() {
                        if let Some(text) = block
                            .get_mut("text")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string())
                        {
                            block["text"] =
                                serde_json::Value::String(rewrite_system_reminders(&text, config));
                        }
                    }
                }
            }
        }
    }

    // 3. Extract first user message for CCH hash computation (after rewrite)
    let first_user_text = extract_first_user_message(body);
    let version = config
        .env
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("2.1.81");
    let hash = if first_user_text.is_empty() {
        fallback_hash()
    } else {
        compute_cch(&first_user_text, version)
    };
    debug!(
        "Computed CCH: {hash} (from {} char message)",
        first_user_text.len()
    );

    // 4. Strip billing header and rewrite system prompt
    if let Some(system) = body.get_mut("system") {
        if let Some(blocks) = system.as_array_mut() {
            // Remove billing header blocks
            blocks.retain(|item| {
                let text = item
                    .as_str()
                    .or_else(|| item.get("text").and_then(|t| t.as_str()));
                if let Some(text) = text
                    && text.trim_start().starts_with("x-anthropic-billing-header:")
                {
                    debug!("Stripped billing header block from system prompt");
                    return false;
                }
                true
            });

            // Rewrite remaining blocks
            for item in blocks.iter_mut() {
                if let Some(s) = item.as_str().map(|s| s.to_string()) {
                    *item = serde_json::Value::String(rewrite_prompt_text(&s, config, Some(&hash)));
                } else if let Some(text) = item
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
                {
                    item["text"] =
                        serde_json::Value::String(rewrite_prompt_text(&text, config, Some(&hash)));
                }
            }
        } else if let Some(s) = system.as_str().map(|s| s.to_string()) {
            // Single string system prompt
            let cleaned = regex_replace_all(&s, r"x-anthropic-billing-header:[^\n]+\n?", "");
            *system = serde_json::Value::String(rewrite_prompt_text(&cleaned, config, Some(&hash)));
        }
    }
}

// ---------------------------------------------------------------------------
// /api/event_logging/batch rewriting
// ---------------------------------------------------------------------------

fn rewrite_event_batch(body: &mut serde_json::Value, config: &NativeConfig) {
    let events = match body.get_mut("events").and_then(|e| e.as_array_mut()) {
        Some(e) => e,
        None => return,
    };

    for event in events.iter_mut() {
        let data = match event.get_mut("event_data") {
            Some(d) => d,
            None => continue,
        };

        // Identity fields
        if data.get("device_id").is_some() {
            data["device_id"] = serde_json::Value::String(config.identity.device_id.clone());
        }
        if data.get("email").is_some() {
            data["email"] = serde_json::Value::String(config.identity.email.clone());
        }

        // Replace env fingerprint entirely
        if data.get("env").is_some() {
            data["env"] = build_canonical_env(config);
        }

        // Process metrics with randomized values
        if let Some(process) = data.get("process").cloned() {
            data["process"] = build_canonical_process(&process, config);
        }

        // Strip fields that leak gateway URL
        if let Some(obj) = data.as_object_mut() {
            obj.remove("baseUrl");
            obj.remove("base_url");
            obj.remove("gateway");
        }

        // Rewrite base64-encoded additional_metadata
        if let Some(meta) = data
            .get("additional_metadata")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
            && let Some(rewritten) = rewrite_additional_metadata(&meta)
        {
            data["additional_metadata"] = serde_json::Value::String(rewritten);
        }

        let event_name = data
            .get("event_name")
            .and_then(|n| n.as_str())
            .unwrap_or("unknown");
        debug!("Rewrote event: {event_name}");
    }
}

fn rewrite_generic_identity(body: &mut serde_json::Value, config: &NativeConfig) {
    if body.get("device_id").is_some() {
        body["device_id"] = serde_json::Value::String(config.identity.device_id.clone());
    }
    if body.get("email").is_some() {
        body["email"] = serde_json::Value::String(config.identity.email.clone());
    }
}

// ---------------------------------------------------------------------------
// Text rewriting helpers
// ---------------------------------------------------------------------------

/// Rewrite prompt text: env block, paths, billing header hash.
fn rewrite_prompt_text(text: &str, config: &NativeConfig, hash: Option<&str>) -> String {
    let pe = &config.prompt_env;
    let mut result = text.to_string();

    // 1. Billing header fingerprint
    if let Some(hash) = hash {
        let version = config
            .env
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("2.1.81");
        result = regex_replace_all(
            &result,
            r"cc_version=[\d.]+\.[a-f0-9]{3}",
            &format!("cc_version={version}.{hash}"),
        );
    }

    // 2. <env> block fields
    result = regex_replace_all(
        &result,
        r"Platform:\s*\S+",
        &format!("Platform: {}", pe.platform),
    );
    result = regex_replace_all(&result, r"Shell:\s*\S+", &format!("Shell: {}", pe.shell));
    result = regex_replace_all(
        &result,
        r"OS Version:\s*[^\n<]+",
        &format!("OS Version: {}", pe.os_version),
    );

    // 3. Working directory
    result = regex_replace_all(
        &result,
        r"((?:Primary )?[Ww]orking directory:\s*)/\S+",
        &format!("$1{}", pe.working_dir),
    );

    // 4. Home directory paths
    let canonical_home = extract_home_prefix(&pe.working_dir);
    result = regex_replace_all(&result, r"/(?:Users|home)/[^/\s]+/", &canonical_home);

    result
}

/// Rewrite only <system-reminder> blocks within message text.
fn rewrite_system_reminders(text: &str, config: &NativeConfig) -> String {
    // Match <system-reminder>...</system-reminder> blocks
    let re = regex_lite::Regex::new(r"(?s)(<system-reminder>)(.*?)(</system-reminder>)").unwrap();
    re.replace_all(text, |caps: &regex_lite::Captures| {
        let open = &caps[1];
        let content = &caps[2];
        let close = &caps[3];
        format!(
            "{}{}{}",
            open,
            rewrite_prompt_text(content, config, None),
            close
        )
    })
    .to_string()
}

/// CCH hash: SHA-256(salt + chars_at_positions + version)[0..3]
fn compute_cch(first_user_message: &str, version: &str) -> String {
    let chars: String = CCH_POSITIONS
        .iter()
        .map(|&i| first_user_message.chars().nth(i).unwrap_or('0'))
        .collect();

    let mut hasher = Sha256::new();
    hasher.update(format!("{CCH_SALT}{chars}{version}"));
    let result = hasher.finalize();
    // Match TypeScript: .digest('hex').slice(0, 3)
    hex::encode(result)[..3].to_string()
}

/// Fallback hash: generated once per process (matches TypeScript behavior).
fn fallback_hash() -> String {
    use std::sync::LazyLock;
    static HASH: LazyLock<String> = LazyLock::new(|| {
        let mut rng = rand::rng();
        format!("{:03x}", rng.random_range(0..4096u32))
    });
    HASH.clone()
}

/// Extract first user message text from messages array.
fn extract_first_user_message(body: &serde_json::Value) -> String {
    let messages = match body.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return String::new(),
    };

    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
            return s.to_string();
        }
        if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("text")
                    && let Some(text) = block.get("text").and_then(|t| t.as_str())
                {
                    return text.to_string();
                }
            }
        }
        return String::new();
    }

    String::new()
}

// ---------------------------------------------------------------------------
// Canonical env/process builders
// ---------------------------------------------------------------------------

fn build_canonical_env(config: &NativeConfig) -> serde_json::Value {
    let env = &config.env;
    let get_str = |key: &str| -> serde_json::Value {
        env.get(key).cloned().unwrap_or(serde_json::Value::Null)
    };
    let get_bool = |key: &str, default: bool| -> bool {
        env.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
    };

    serde_json::json!({
        "platform": get_str("platform"),
        "platform_raw": env.get("platform_raw").or_else(|| env.get("platform")).cloned().unwrap_or(serde_json::Value::Null),
        "arch": get_str("arch"),
        "node_version": get_str("node_version"),
        "terminal": get_str("terminal"),
        "package_managers": get_str("package_managers"),
        "runtimes": get_str("runtimes"),
        "is_running_with_bun": get_bool("is_running_with_bun", false),
        "is_ci": false,
        "is_claubbit": false,
        "is_claude_code_remote": false,
        "is_local_agent_mode": false,
        "is_conductor": false,
        "is_github_action": false,
        "is_claude_code_action": false,
        "is_claude_ai_auth": get_bool("is_claude_ai_auth", true),
        "version": get_str("version"),
        "version_base": env.get("version_base").or_else(|| env.get("version")).cloned().unwrap_or(serde_json::Value::Null),
        "build_time": get_str("build_time"),
        "deployment_environment": get_str("deployment_environment"),
        "vcs": get_str("vcs"),
    })
}

fn build_canonical_process(
    original: &serde_json::Value,
    config: &NativeConfig,
) -> serde_json::Value {
    let pc = &config.process;
    let mut rng = rand::rng();

    // If original is a base64-encoded JSON string, decode → rewrite → re-encode
    if let Some(s) = original.as_str() {
        if let Ok(decoded) = base64_decode_json(s) {
            let mut rewritten = decoded;
            rewrite_process_fields(&mut rewritten, pc, &mut rng);
            return serde_json::Value::String(base64_encode_json(&rewritten));
        }
        return original.clone();
    }

    // Object: rewrite in-place
    if original.is_object() {
        let mut rewritten = original.clone();
        rewrite_process_fields(&mut rewritten, pc, &mut rng);
        return rewritten;
    }

    original.clone()
}

fn rewrite_process_fields(
    proc: &mut serde_json::Value,
    pc: &crate::config::ProcessConfig,
    rng: &mut impl Rng,
) {
    // Match TypeScript: Math.floor(min + Math.random() * (max - min)) — exclusive of max
    proc["constrainedMemory"] = serde_json::json!(pc.constrained_memory);
    proc["rss"] = serde_json::json!(rng.random_range(pc.rss_range[0]..pc.rss_range[1]));
    proc["heapTotal"] =
        serde_json::json!(rng.random_range(pc.heap_total_range[0]..pc.heap_total_range[1]));
    proc["heapUsed"] =
        serde_json::json!(rng.random_range(pc.heap_used_range[0]..pc.heap_used_range[1]));
}

fn rewrite_additional_metadata(original: &str) -> Option<String> {
    let mut decoded = base64_decode_json(original).ok()?;
    if let Some(obj) = decoded.as_object_mut() {
        obj.remove("baseUrl");
        obj.remove("base_url");
        obj.remove("gateway");
    }
    Some(base64_encode_json(&decoded))
}

// ---------------------------------------------------------------------------
// Utility helpers
// ---------------------------------------------------------------------------

fn regex_replace_all(text: &str, pattern: &str, replacement: &str) -> String {
    let re = regex_lite::Regex::new(pattern).unwrap();
    re.replace_all(text, replacement).to_string()
}

fn extract_home_prefix(working_dir: &str) -> String {
    // Extract "/Users/jack/" or "/home/user/" from "/Users/jack/projects"
    let parts: Vec<&str> = working_dir.splitn(4, '/').collect();
    if parts.len() >= 3 {
        format!("/{}/{}/", parts[1], parts[2])
    } else {
        "/Users/user/".to_string()
    }
}

fn base64_decode_json(s: &str) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD.decode(s)?;
    let text = String::from_utf8(decoded)?;
    Ok(serde_json::from_str(&text)?)
}

fn base64_encode_json(value: &serde_json::Value) -> String {
    use base64::Engine;
    let json = serde_json::to_string(value).unwrap();
    base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> NativeConfig {
        NativeConfig {
            server: None,
            upstream: crate::config::UpstreamConfig {
                url: "https://api.anthropic.com".to_string(),
            },
            oauth: crate::config::OAuthConfig {
                access_token: None,
                refresh_token: "test".to_string(),
                expires_at: None,
            },
            identity: crate::config::IdentityConfig {
                device_id: "a".repeat(64),
                email: "test@example.com".to_string(),
            },
            env: {
                let mut m = std::collections::HashMap::new();
                m.insert("version".to_string(), serde_json::json!("2.1.81"));
                m.insert("platform".to_string(), serde_json::json!("darwin"));
                m
            },
            prompt_env: crate::config::PromptEnvConfig {
                platform: "darwin".to_string(),
                shell: "zsh".to_string(),
                os_version: "Darwin 24.4.0".to_string(),
                working_dir: "/Users/jack/projects".to_string(),
            },
            process: crate::config::ProcessConfig {
                constrained_memory: 34359738368,
                rss_range: [300000000, 500000000],
                heap_total_range: [40000000, 80000000],
                heap_used_range: [100000000, 200000000],
            },
        }
    }

    #[test]
    fn test_compute_cch() {
        let hash = compute_cch("Hello, world! This is a test message.", "2.1.81");
        assert_eq!(hash.len(), 3);
        // Hash should be deterministic
        let hash2 = compute_cch("Hello, world! This is a test message.", "2.1.81");
        assert_eq!(hash, hash2);
        // Different input → different hash
        let hash3 = compute_cch("Different message text here!!!", "2.1.81");
        assert_ne!(hash, hash3);
    }

    #[test]
    fn test_extract_first_user_message_string() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": "Hello world"}
            ]
        });
        assert_eq!(extract_first_user_message(&body), "Hello world");
    }

    #[test]
    fn test_extract_first_user_message_blocks() {
        let body = serde_json::json!({
            "messages": [
                {"role": "system", "content": "System prompt"},
                {"role": "user", "content": [
                    {"type": "text", "text": "Block text"}
                ]}
            ]
        });
        assert_eq!(extract_first_user_message(&body), "Block text");
    }

    #[test]
    fn test_rewrite_metadata_user_id() {
        let config = test_config();
        let mut body = serde_json::json!({
            "metadata": {
                "user_id": "{\"device_id\":\"old_id\",\"account_uuid\":\"acc\"}"
            },
            "messages": [{"role": "user", "content": "hi"}]
        });

        rewrite_messages_body(&mut body, &config);

        let user_id_str = body["metadata"]["user_id"].as_str().unwrap();
        let user_id: serde_json::Value = serde_json::from_str(user_id_str).unwrap();
        assert_eq!(user_id["device_id"].as_str().unwrap(), "a".repeat(64));
        assert_eq!(user_id["account_uuid"].as_str().unwrap(), "acc");
    }

    #[test]
    fn test_strip_billing_header() {
        let config = test_config();
        let mut body = serde_json::json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.81.abc"},
                {"type": "text", "text": "Real system prompt here"}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });

        rewrite_messages_body(&mut body, &config);

        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert!(
            system[0]["text"]
                .as_str()
                .unwrap()
                .contains("Real system prompt")
        );
    }

    #[test]
    fn test_rewrite_env_block() {
        let config = test_config();
        let text = "Platform: linux\nShell: bash\nOS Version: Linux 6.5.0\nWorking directory: /home/bob/project";
        let result = rewrite_prompt_text(text, &config, None);
        assert!(result.contains("Platform: darwin"));
        assert!(result.contains("Shell: zsh"));
        assert!(result.contains("OS Version: Darwin 24.4.0"));
        assert!(result.contains("/Users/jack/projects"));
    }

    #[test]
    fn test_rewrite_home_paths() {
        let config = test_config();
        let text = "File at /home/bob/src/main.rs and /Users/alice/code/test.py";
        let result = rewrite_prompt_text(text, &config, None);
        assert!(result.contains("/Users/jack/"));
        assert!(!result.contains("/home/bob/"));
        assert!(!result.contains("/Users/alice/"));
    }

    #[test]
    fn test_rewrite_headers() {
        let config = test_config();
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "sk-ant-oat01-old".parse().unwrap());
        headers.insert("user-agent", "old-agent".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert(
            "x-anthropic-billing-header",
            "cc_version=2.1.81.abc".parse().unwrap(),
        );

        let rewritten = rewrite_headers(&headers, &config);

        assert!(rewritten.get("x-api-key").is_none());
        assert!(rewritten.get("x-anthropic-billing-header").is_none());
        assert_eq!(
            rewritten.get("user-agent").unwrap().to_str().unwrap(),
            "claude-code/2.1.81 (external, cli)"
        );
        assert_eq!(
            rewritten.get("content-type").unwrap().to_str().unwrap(),
            "application/json"
        );
    }

    #[test]
    fn test_extract_home_prefix() {
        assert_eq!(extract_home_prefix("/Users/jack/projects"), "/Users/jack/");
        assert_eq!(extract_home_prefix("/home/user/code"), "/home/user/");
    }

    #[test]
    fn test_rewrite_generic_identity() {
        let config = test_config();
        let mut body = serde_json::json!({
            "device_id": "old_device",
            "email": "old@example.com"
        });
        rewrite_generic_identity(&mut body, &config);
        assert_eq!(body["device_id"].as_str().unwrap(), "a".repeat(64));
        assert_eq!(body["email"].as_str().unwrap(), "test@example.com");
    }
}
