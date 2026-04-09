//! Native mode configuration.
//!
//! Supports two modes:
//! - YAML config file (`--native-config config.yaml`)
//! - Zero-config auto-detection from `~/.claude/.credentials.json`

use std::collections::HashMap;
use std::path::Path;

use sha2::{Digest, Sha256};
use serde::Deserialize;
use tracing::{info, warn};

#[derive(Debug, Clone, Deserialize)]
pub struct NativeConfig {
    #[allow(dead_code)]
    pub server: Option<ServerConfig>,
    pub upstream: UpstreamConfig,
    pub oauth: OAuthConfig,
    pub identity: IdentityConfig,
    pub env: HashMap<String, serde_json::Value>,
    pub prompt_env: PromptEnvConfig,
    pub process: ProcessConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ServerConfig {
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpstreamConfig {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OAuthConfig {
    pub access_token: Option<String>,
    pub refresh_token: String,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IdentityConfig {
    pub device_id: String,
    pub email: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PromptEnvConfig {
    pub platform: String,
    pub shell: String,
    pub os_version: String,
    pub working_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProcessConfig {
    pub constrained_memory: u64,
    pub rss_range: [u64; 2],
    pub heap_total_range: [u64; 2],
    pub heap_used_range: [u64; 2],
}

/// Load config from a YAML file.
pub fn load_native_config(path: &str) -> Result<NativeConfig, String> {
    let path = Path::new(path);
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read config file {}: {e}", path.display()))?;
    let config: NativeConfig = serde_yaml::from_str(&content)
        .map_err(|e| format!("Failed to parse config YAML: {e}"))?;

    if config.identity.device_id.contains("0000000000") {
        return Err(
            "config: identity.device_id must be set to a real 64-char hex value".to_string(),
        );
    }
    if config.oauth.refresh_token.is_empty() {
        return Err("config: oauth.refresh_token is required".to_string());
    }

    Ok(config)
}

/// Auto-detect config from real system + `~/.claude/.credentials.json`.
///
/// Reads OAuth credentials from Claude Code's credential store,
/// detects the actual CC version, OS, shell, and generates a persistent
/// device_id. Designed to match a real Claude Code installation's fingerprint.
pub fn auto_detect_config() -> Result<NativeConfig, String> {
    let home = dirs::home_dir().ok_or("Cannot determine home directory")?;

    // --- 1. OAuth credentials ---
    let creds_path = home.join(".claude/.credentials.json");
    let creds_content = std::fs::read_to_string(&creds_path).map_err(|_| {
        format!(
            "No credentials found at {}\nRun: claude auth login",
            creds_path.display()
        )
    })?;

    let creds_json: serde_json::Value =
        serde_json::from_str(&creds_content).map_err(|e| format!("Invalid credentials JSON: {e}"))?;

    let oauth_obj = &creds_json["claudeAiOauth"];
    if oauth_obj.is_null() {
        return Err("No claudeAiOauth in credentials. Run: claude auth login".to_string());
    }

    let access_token = oauth_obj["accessToken"].as_str().map(String::from);
    let refresh_token = oauth_obj["refreshToken"]
        .as_str()
        .ok_or("Missing refreshToken. Run: claude auth login")?
        .to_string();
    let expires_at = oauth_obj["expiresAt"].as_u64();

    info!("Auto-detected credentials from {}", creds_path.display());

    // --- 2. Persistent device_id (random, stored in ~/.claude/.proxy_device_id) ---
    let device_id_path = home.join(".claude/.proxy_device_id");
    let device_id = if let Ok(existing) = std::fs::read_to_string(&device_id_path) {
        let id = existing.trim().to_string();
        if id.len() == 64 {
            info!("Loaded persisted device_id: {}...", &id[..8]);
            id
        } else {
            generate_and_persist_device_id(&device_id_path)
        }
    } else {
        generate_and_persist_device_id(&device_id_path)
    };

    // --- 3. Real CC version (from `claude --version`) ---
    let cc_version = detect_cc_version().unwrap_or_else(|| "2.1.81".to_string());

    // --- 4. Real system info ---
    let sys = detect_system_info();

    info!(
        "System: platform={}, shell={}, os={}, cc_version={}",
        sys.platform, sys.shell, sys.os_version, cc_version
    );

    // --- 5. Build config ---
    let home_dir = home.to_string_lossy().to_string();

    Ok(NativeConfig {
        server: None,
        upstream: UpstreamConfig {
            url: "https://api.anthropic.com".to_string(),
        },
        oauth: OAuthConfig {
            access_token,
            refresh_token,
            expires_at,
        },
        identity: IdentityConfig {
            device_id,
            email: String::new(), // Must be provided via --email or config file
        },
        env: build_env(&cc_version, &sys),
        prompt_env: PromptEnvConfig {
            platform: sys.platform.clone(),
            shell: sys.shell.clone(),
            os_version: sys.os_version.clone(),
            working_dir: home_dir,
        },
        process: ProcessConfig {
            constrained_memory: detect_total_memory(),
            rss_range: [300_000_000, 500_000_000],
            heap_total_range: [200_000_000, 400_000_000],
            heap_used_range: [100_000_000, 200_000_000],
        },
    })
}

// ---------------------------------------------------------------------------
// Detection helpers
// ---------------------------------------------------------------------------

struct SystemInfo {
    platform: String,
    platform_raw: String,
    arch: String,
    shell: String,
    os_version: String,
    terminal: String,
    deployment_env: String,
}

fn detect_system_info() -> SystemInfo {
    let platform = std::env::consts::OS; // "linux", "macos", "windows"
    let platform_name = match platform {
        "macos" => "darwin",
        other => other,
    };
    let arch = std::env::consts::ARCH; // "x86_64", "aarch64"
    let arch_name = match arch {
        "aarch64" => "arm64",
        other => other,
    };

    let shell = std::env::var("SHELL")
        .unwrap_or_else(|_| "/bin/bash".to_string());
    let shell_name = shell.rsplit('/').next().unwrap_or("bash").to_string();

    let os_version = detect_os_version(platform_name);

    let terminal = std::env::var("TERM_PROGRAM")
        .unwrap_or_else(|_| {
            if platform_name == "darwin" {
                "Apple_Terminal".to_string()
            } else {
                std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string())
            }
        });

    let deployment_env = format!("unknown-{platform_name}");

    SystemInfo {
        platform: platform_name.to_string(),
        platform_raw: platform_name.to_string(),
        arch: arch_name.to_string(),
        shell: shell_name,
        os_version,
        terminal,
        deployment_env,
    }
}

fn detect_os_version(platform: &str) -> String {
    match platform {
        "darwin" => {
            // Darwin kernel version from uname
            std::process::Command::new("uname")
                .arg("-sr")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "Darwin 24.4.0".to_string())
        }
        "linux" => {
            std::process::Command::new("uname")
                .arg("-sr")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "Linux 6.5.0".to_string())
        }
        _ => format!("{platform} unknown"),
    }
}

fn detect_cc_version() -> Option<String> {
    let output = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version_str = String::from_utf8(output.stdout).ok()?;
    // Output is "2.1.92 (Claude Code)" — take first token
    let version = version_str
        .trim()
        .split_whitespace()
        .next()
        .unwrap_or(version_str.trim());
    info!("Detected Claude Code version: {version}");
    Some(version.to_string())
}

fn detect_total_memory() -> u64 {
    // Try /proc/meminfo (Linux)
    if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
        for line in content.lines() {
            if line.starts_with("MemTotal:") {
                if let Some(kb_str) = line.split_whitespace().nth(1) {
                    if let Ok(kb) = kb_str.parse::<u64>() {
                        return kb * 1024; // kB → bytes
                    }
                }
            }
        }
    }
    // Default: 32GB
    34_359_738_368
}

fn generate_and_persist_device_id(path: &Path) -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.random::<u8>()).collect();
    let id = hex::encode(&bytes);
    if let Err(e) = std::fs::write(path, &id) {
        warn!("Could not persist device_id to {}: {e}", path.display());
    } else {
        info!("Generated new device_id: {}... (persisted to {})", &id[..8], path.display());
    }
    id
}

fn build_env(version: &str, sys: &SystemInfo) -> HashMap<String, serde_json::Value> {
    use serde_json::json;

    // Detect node version
    let node_version = std::process::Command::new("node")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "v22.0.0".to_string());

    let mut m = HashMap::new();
    m.insert("platform".into(), json!(sys.platform));
    m.insert("platform_raw".into(), json!(sys.platform_raw));
    m.insert("arch".into(), json!(sys.arch));
    m.insert("node_version".into(), json!(node_version));
    m.insert("terminal".into(), json!(sys.terminal));
    m.insert("package_managers".into(), json!("npm,pnpm"));
    m.insert("runtimes".into(), json!("node"));
    m.insert("is_running_with_bun".into(), json!(false));
    m.insert("is_ci".into(), json!(false));
    m.insert("is_claubbit".into(), json!(false));
    m.insert("is_claude_code_remote".into(), json!(false));
    m.insert("is_local_agent_mode".into(), json!(false));
    m.insert("is_conductor".into(), json!(false));
    m.insert("is_github_action".into(), json!(false));
    m.insert("is_claude_code_action".into(), json!(false));
    m.insert("is_claude_ai_auth".into(), json!(true));
    m.insert("version".into(), json!(version));
    m.insert("version_base".into(), json!(version));
    m.insert("build_time".into(), json!("2026-03-20T21:26:18Z")); // TODO: extract from CC binary
    m.insert("deployment_environment".into(), json!(sys.deployment_env));
    m.insert("vcs".into(), json!("git"));
    m
}

#[allow(dead_code)]
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}
