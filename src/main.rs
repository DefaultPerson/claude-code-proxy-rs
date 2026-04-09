mod adapter;
mod config;
mod error;
mod native;
mod oauth;
mod rewriter;
mod routes;
mod server;
mod subprocess;
mod types;

use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "claude-code-proxy")]
#[command(about = "Anthropic Messages API proxy over Claude Code CLI")]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value = "3456")]
    port: u16,

    /// Working directory for the Claude CLI subprocess
    #[arg(long, default_value = ".")]
    cwd: String,

    /// Max agentic turns per request (prevents runaway loops)
    #[arg(long, default_value = "100")]
    max_turns: u32,

    /// Replace Claude Code's system prompt entirely instead of appending
    #[arg(long, default_value = "false")]
    replace_system_prompt: bool,

    /// Effort level for Claude (low, medium, high, max)
    #[arg(long)]
    effort: Option<String>,

    /// Embed system prompt in prompt text instead of using --system-prompt (replace).
    /// Keeps Claude Code's default 43K system prompt intact.
    #[arg(long, default_value = "false")]
    embed_system_prompt: bool,

    /// Proxy mode: "subprocess" (default) or "native" (direct API calls)
    #[arg(long, default_value = "subprocess")]
    mode: String,

    /// Path to native mode config YAML (required for --mode native)
    #[arg(long)]
    native_config: Option<String>,

    /// Direct API key (skip OAuth, use with --mode native)
    #[arg(long)]
    api_key: Option<String>,

    /// Email for identity normalization (required for --mode native without config file)
    #[arg(long)]
    email: Option<String>,
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "claude_code_proxy=info,tower_http=info".parse().unwrap()),
        )
        .compact()
        .with_target(false)
        .init();

    let args = Args::parse();

    let (mode, native_client) = match args.mode.as_str() {
        "native" => init_native_mode(&args).await,
        _ => init_subprocess_mode().await,
    };

    // Resolve cwd
    let cwd = std::fs::canonicalize(&args.cwd)
        .unwrap_or_else(|_| std::path::PathBuf::from(&args.cwd))
        .to_string_lossy()
        .to_string();

    let state = server::AppState {
        cwd: cwd.clone(),
        max_turns: args.max_turns,
        replace_system_prompt: args.replace_system_prompt,
        effort: args.effort,
        embed_system_prompt: args.embed_system_prompt,
        mode,
        native_client,
    };
    let app = server::create_router(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind {addr}: {e}");
            if e.kind() == std::io::ErrorKind::AddrInUse {
                error!("Port {} is already in use", args.port);
            }
            std::process::exit(1);
        }
    };

    info!("Listening on http://{addr} (mode: {})", args.mode);
    info!("CWD: {cwd}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap_or_else(|e| error!("Server error: {e}"));
}

async fn init_subprocess_mode() -> (server::ProxyMode, Option<Arc<native::NativeClient>>) {
    // Verify claude CLI is available
    match tokio::process::Command::new("claude")
        .arg("--version")
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            info!("Claude CLI: {}", version.trim());
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("claude --version failed: {stderr}");
            std::process::exit(1);
        }
        Err(e) => {
            error!("claude CLI not found: {e}");
            error!("Install: npm install -g @anthropic-ai/claude-code");
            std::process::exit(1);
        }
    }

    (server::ProxyMode::Subprocess, None)
}

async fn init_native_mode(args: &Args) -> (server::ProxyMode, Option<Arc<native::NativeClient>>) {
    let mut native_config = match &args.native_config {
        Some(path) => config::load_native_config(path).unwrap_or_else(|e| {
            error!("Failed to load native config: {e}");
            std::process::exit(1);
        }),
        None => {
            info!("No --native-config provided, auto-detecting from ~/.claude/.credentials.json");
            config::auto_detect_config().unwrap_or_else(|e| {
                error!("{e}");
                std::process::exit(1);
            })
        }
    };

    // Apply --email flag (overrides config file value too)
    if let Some(email) = &args.email {
        native_config.identity.email = email.clone();
    }

    // Validate email is set (not empty, not placeholder)
    if native_config.identity.email.is_empty() {
        error!("Email is required for native mode. Use --email you@example.com");
        error!("This should be the email associated with your Claude account.");
        std::process::exit(1);
    }
    if native_config.identity.email == "user@example.com"
        || native_config.identity.email.ends_with("@example.com")
    {
        error!(
            "Email '{}' looks like a placeholder — use your real Claude account email.",
            native_config.identity.email
        );
        error!("Usage: --email your-real-email@domain.com");
        std::process::exit(1);
    }

    info!("Native mode: upstream={}", native_config.upstream.url);
    info!(
        "Native mode: device_id={}...",
        &native_config.identity.device_id[..8]
    );
    info!("Native mode: email={}", native_config.identity.email);

    // If --api-key provided, use it instead of OAuth
    if let Some(api_key) = &args.api_key {
        warn!("Using direct API key (OAuth disabled)");
        native_config.oauth.access_token = Some(api_key.clone());
        // Set far-future expiry so it's never "expired"
        native_config.oauth.expires_at = Some(u64::MAX / 2);
    }

    let credentials = match oauth::CredentialStore::new(&native_config.oauth) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to init credentials: {e}");
            std::process::exit(1);
        }
    };

    // Initialize (validate token, refresh if needed)
    if args.api_key.is_none() {
        if let Err(e) = credentials.init().await {
            error!("OAuth init failed: {e}");
            std::process::exit(1);
        }
        // Start background refresh loop
        credentials.clone().start_refresh_loop();
    }

    let client = Arc::new(native::NativeClient::new(credentials, native_config));

    (server::ProxyMode::Native, Some(client))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("Received Ctrl+C, shutting down"),
        () = terminate => info!("Received SIGTERM, shutting down"),
    }
}
