//! Subprocess management: spawn `claude` CLI and parse NDJSON output.

use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::types::cli::{CliMessage, ResultMessage, Usage};

/// Events emitted by the subprocess to the HTTP handler.
#[derive(Debug)]
pub enum SubprocessEvent {
    /// CLI init: session ID and model from `system/init`.
    Init { session_id: String, model: String },
    /// Raw Anthropic streaming event from `stream_event.event`.
    /// `event_type` is the event name (e.g. "content_block_delta").
    /// `payload` is the raw JSON to emit as SSE data.
    StreamEvent {
        event_type: String,
        payload: serde_json::Value,
    },
    /// Final result with usage data.
    Result(ResultData),
    /// CLI-level error (result.subtype starts with "error").
    CliError { errors: Vec<String> },
    /// Subprocess spawn/IO error.
    ProcessError(String),
    /// Process exited with given code.
    Close(i32),
}

#[derive(Debug)]
pub struct ResultData {
    pub result: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Usage,
    pub session_id: Option<String>,
    pub num_turns: Option<u64>,
    pub duration_ms: Option<u64>,
    pub total_cost_usd: Option<f64>,
}

/// Options for spawning the CLI subprocess.
pub struct SubprocessOptions {
    pub request_id: String,
    pub model: String,
    pub system_prompt: Option<String>,
    pub cwd: String,
    /// Max agentic turns. None = CLI default.
    pub max_turns: Option<u32>,
    /// If true, use --system-prompt (replace). If false, use --append-system-prompt.
    pub replace_system_prompt: bool,
    /// Effort level (low/medium/high/max).
    pub effort: Option<String>,
    /// If true, pass `--tools ""` to disable all built-in tools.
    pub disable_tools: bool,
}

/// Spawn `claude` CLI as a subprocess, sending events to `tx`.
///
/// The subprocess is killed if the receiver drops (client disconnect)
/// or after 30 minutes of inactivity.
pub async fn spawn_subprocess(
    prompt: String,
    options: SubprocessOptions,
    tx: mpsc::Sender<SubprocessEvent>,
) {
    let rid = &options.request_id;
    let start = Instant::now();

    let mut args: Vec<String> = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--include-partial-messages".to_string(),
        "--model".to_string(),
        normalize_model(&options.model),
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
        "--no-session-persistence".to_string(),
    ];

    if let Some(turns) = options.max_turns {
        args.push("--max-turns".to_string());
        args.push(turns.to_string());
    }

    if let Some(ref sys) = options.system_prompt {
        let flag = if options.replace_system_prompt {
            "--system-prompt"
        } else {
            "--append-system-prompt"
        };
        args.push(flag.to_string());
        args.push(sys.clone());
    }

    if let Some(ref effort) = options.effort {
        args.push("--effort".to_string());
        args.push(effort.clone());
    }

    if options.disable_tools {
        args.push("--tools".to_string());
        args.push(String::new());
    }

    info!("[req={rid}] Spawning claude -p --model {}", options.model);

    let mut child = match Command::new("claude")
        .args(&args)
        .current_dir(&options.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                "claude CLI not found. Install: npm install -g @anthropic-ai/claude-code"
                    .to_string()
            } else {
                format!("Failed to spawn claude: {e}")
            };
            error!("[req={rid}] {msg}");
            let _ = tx.send(SubprocessEvent::ProcessError(msg)).await;
            return;
        }
    };

    let pid = child.id().unwrap_or(0);
    info!("[req={rid}][pid={pid}] Process started");

    // Write prompt to stdin and close it.
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(prompt.as_bytes()).await {
            error!("[req={rid}][pid={pid}] Failed to write stdin: {e}");
            let _ = tx
                .send(SubprocessEvent::ProcessError(format!("stdin write: {e}")))
                .await;
            let _ = child.kill().await;
            return;
        }
        drop(stdin);
    }

    let stdout = child.stdout.take().expect("stdout not captured");
    let stderr = child.stderr.take().expect("stderr not captured");

    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut first_token = true;
    let inactivity_timeout = tokio::time::sleep(Duration::from_secs(30 * 60));
    tokio::pin!(inactivity_timeout);

    let progress_interval = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(progress_interval);

    loop {
        tokio::select! {
            line = stdout_reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        // Reset inactivity timer
                        inactivity_timeout.as_mut().reset(
                            tokio::time::Instant::now() + Duration::from_secs(30 * 60)
                        );
                        progress_interval.as_mut().reset(
                            tokio::time::Instant::now() + Duration::from_secs(30)
                        );

                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        let events = process_line(trimmed, rid);
                        for event in events {
                            if first_token && matches!(&event, SubprocessEvent::StreamEvent { event_type, .. } if event_type == "content_block_delta") {
                                let ttft = start.elapsed().as_secs_f64();
                                info!("[req={rid}][pid={pid}] TTFT: {ttft:.2}s");
                                first_token = false;
                            }
                            if tx.send(event).await.is_err() {
                                warn!("[req={rid}][pid={pid}] Client disconnected");
                                let _ = child.kill().await;
                                return;
                            }
                        }
                    }
                    Ok(None) => break, // stdout closed
                    Err(e) => {
                        error!("[req={rid}][pid={pid}] stdout read error: {e}");
                        break;
                    }
                }
            }

            line = stderr_reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            debug!("[req={rid}][pid={pid}] stderr: {trimmed}");
                        }
                    }
                    Ok(None) => {} // stderr closed
                    Err(_) => {}
                }
            }

            () = &mut inactivity_timeout => {
                warn!("[req={rid}][pid={pid}] Inactivity timeout (30min)");
                let _ = tx.send(SubprocessEvent::ProcessError(
                    "Inactivity timeout (30 minutes)".to_string()
                )).await;
                let _ = child.kill().await;
                return;
            }

            () = &mut progress_interval => {
                let elapsed = start.elapsed().as_secs();
                debug!("[req={rid}][pid={pid}] Running for {elapsed}s...");
                progress_interval.as_mut().reset(
                    tokio::time::Instant::now() + Duration::from_secs(30)
                );
            }
        }
    }

    let exit_code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);

    let elapsed = start.elapsed().as_secs_f64();
    info!("[req={rid}][pid={pid}] Exited code={exit_code} total={elapsed:.2}s");

    let _ = tx.send(SubprocessEvent::Close(exit_code)).await;
}

/// Parse a single NDJSON line into zero or more subprocess events.
fn process_line(line: &str, rid: &str) -> Vec<SubprocessEvent> {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            debug!(
                "[req={rid}] Non-JSON line: {}",
                &line[..line.len().min(100)]
            );
            return vec![];
        }
    };

    // Try typed deserialization
    let msg: CliMessage = match serde_json::from_value(value.clone()) {
        Ok(m) => m,
        Err(_) => {
            debug!("[req={rid}] Unrecognized JSON type");
            return vec![];
        }
    };

    match msg {
        CliMessage::System(sys) => {
            if sys.subtype.as_deref() == Some("init") {
                if let (Some(session_id), Some(model)) = (sys.session_id, sys.model) {
                    info!("[req={rid}] Init: model={model} session={session_id}");
                    return vec![SubprocessEvent::Init { session_id, model }];
                }
            }
            // Other system subtypes (api_retry, status, etc.) — skip
            debug!("[req={rid}] System subtype: {:?}", sys.subtype);
            vec![]
        }

        CliMessage::StreamEvent(se) => {
            // Skip subagent events
            if se.parent_tool_use_id.is_some() {
                return vec![];
            }

            let event_type = se
                .event
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            vec![SubprocessEvent::StreamEvent {
                event_type,
                payload: se.event,
            }]
        }

        CliMessage::Result(r) => {
            if r.is_error() {
                let errors = r.errors.unwrap_or_default();
                warn!("[req={rid}] CLI error: {errors:?}");
                return vec![SubprocessEvent::CliError { errors }];
            }

            vec![SubprocessEvent::Result(result_data_from(r))]
        }

        CliMessage::RateLimit(rl) => {
            let status = rl
                .rate_limit_info
                .as_ref()
                .and_then(|i| i.status.as_deref())
                .unwrap_or("unknown");
            match status {
                "rejected" => {
                    warn!("[req={rid}] Rate limited (rejected)");
                    vec![SubprocessEvent::ProcessError(
                        "Rate limit exceeded. Please try again later.".to_string(),
                    )]
                }
                "allowed_warning" => {
                    let util = rl
                        .rate_limit_info
                        .as_ref()
                        .and_then(|i| i.utilization)
                        .unwrap_or(0.0);
                    warn!(
                        "[req={rid}] Rate limit warning: {:.0}% utilized",
                        util * 100.0
                    );
                    vec![]
                }
                _ => vec![],
            }
        }

        CliMessage::Assistant(_) => {
            // Used internally by CLI for multi-turn tracking.
            // We get streaming data from stream_event instead.
            vec![]
        }

        CliMessage::Unknown => vec![],
    }
}

/// Normalize model names to what Claude CLI accepts.
/// CLI recognizes: "sonnet", "opus", "haiku", or full IDs like "claude-sonnet-4-6".
fn normalize_model(model: &str) -> String {
    let m = model.to_lowercase();
    if m.contains("opus") {
        "opus".to_string()
    } else if m.contains("haiku") {
        "haiku".to_string()
    } else if m.contains("sonnet") {
        "sonnet".to_string()
    } else {
        model.to_string()
    }
}

fn result_data_from(r: ResultMessage) -> ResultData {
    ResultData {
        result: r.result,
        stop_reason: r.stop_reason,
        usage: r.usage.unwrap_or_default(),
        session_id: r.session_id,
        num_turns: r.num_turns,
        duration_ms: r.duration_ms,
        total_cost_usd: r.total_cost_usd,
    }
}
