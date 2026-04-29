use anyhow::Result;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use rmcp::{
    Peer,
    model::{LoggingLevel, LoggingMessageNotificationParam},
    service::RoleServer,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tiktoken_rs::o200k_base_singleton;
use tokio::sync::{Mutex as TokioMutex, mpsc};

use super::denylist::{find_matched_pattern, is_denied};
use super::job_manager::{JobManager, JobStatus};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalExecutionInput {
    /// Command to execute
    pub command: String,
    /// Working directory (default: ".")
    #[serde(default = "default_cwd")]
    pub cwd: String,
    /// Shell to use (default: "sh")
    #[serde(default = "default_shell")]
    pub shell: String,
    /// Maximum number of GPT-5/o200k_base tokens to return in command output previews.
    /// Set to 0 to disable token truncation for the bounded in-memory preview buffer.
    #[serde(default = "default_preview_tokens")]
    pub preview_tokens: usize,
    /// Environment variables to set for the command
    #[serde(default)]
    pub env_vars: std::collections::HashMap<String, String>,
    /// Force synchronous execution (wait for completion)
    #[serde(default)]
    pub force_sync: bool,
    /// Custom denylist patterns (in addition to defaults)
    #[serde(default)]
    pub custom_denylist: Vec<String>,
    /// Optional tags for categorizing jobs (e.g., ["build", "ci"])
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_cwd() -> String {
    ".".to_string()
}

fn default_shell() -> String {
    "bash".to_string()
}

fn default_preview_tokens() -> usize {
    4096
}

const MIN_PREVIEW_CAPTURE_BYTES: usize = 64 * 1024;

fn apply_default_env(mut env_vars: HashMap<String, String>) -> HashMap<String, String> {
    env_vars
        .entry("TERM".to_string())
        .or_insert_with(|| "dumb".to_string());
    env_vars
        .entry("NO_COLOR".to_string())
        .or_insert_with(|| "1".to_string());
    env_vars
        .entry("CLICOLOR".to_string())
        .or_insert_with(|| "0".to_string());
    env_vars
        .entry("CLICOLOR_FORCE".to_string())
        .or_insert_with(|| "0".to_string());
    env_vars
        .entry("FORCE_COLOR".to_string())
        .or_insert_with(|| "0".to_string());
    env_vars
        .entry("COLORTERM".to_string())
        .or_insert_with(|| "0".to_string());
    env_vars
}

fn get_async_threshold_secs() -> u64 {
    std::env::var("ENHANCED_TERMINAL_ASYNC_THRESHOLD_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
}

fn get_timeout_secs() -> Option<u64> {
    std::env::var("ENHANCED_TERMINAL_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
}

#[derive(Debug, Clone)]
pub struct PreviewedOutput {
    pub text: String,
    pub truncated: bool,
    pub tokens: Option<usize>,
    pub token_limit: Option<usize>,
    pub tokenizer: Option<&'static str>,
}

pub fn preview_output(text: &str, preview_tokens: usize) -> PreviewedOutput {
    if preview_tokens > 0 {
        let bpe = o200k_base_singleton();
        let tokens = bpe.encode_ordinary(text);
        let token_count = tokens.len();

        if token_count <= preview_tokens {
            return PreviewedOutput {
                text: text.to_string(),
                truncated: false,
                tokens: Some(token_count),
                token_limit: Some(preview_tokens),
                tokenizer: Some("o200k_base"),
            };
        }

        let preview = bpe
            .decode(&tokens[..preview_tokens])
            .unwrap_or_else(|_| text.chars().take(preview_tokens).collect());

        return PreviewedOutput {
            text: preview,
            truncated: true,
            tokens: Some(token_count),
            token_limit: Some(preview_tokens),
            tokenizer: Some("o200k_base"),
        };
    }

    PreviewedOutput {
        text: text.to_string(),
        truncated: false,
        tokens: None,
        token_limit: None,
        tokenizer: None,
    }
}

fn preview_buffer_limit(input: &TerminalExecutionInput) -> usize {
    if input.preview_tokens > 0 {
        MIN_PREVIEW_CAPTURE_BYTES.max(
            input
                .preview_tokens
                .saturating_mul(16)
                .saturating_add(8 * 1024),
        )
    } else {
        MIN_PREVIEW_CAPTURE_BYTES
    }
}

fn append_to_preview_buffer(buffer: &mut Vec<u8>, data: &[u8], limit: usize) -> bool {
    if buffer.len() >= limit {
        return !data.is_empty();
    }

    let remaining = limit.saturating_sub(buffer.len());
    if data.len() <= remaining {
        buffer.extend_from_slice(data);
        false
    } else {
        buffer.extend_from_slice(&data[..remaining]);
        true
    }
}

#[derive(Debug, Serialize)]
pub struct ExecutionResult {
    pub job_id: String,
    pub command: String,
    pub working_directory: String,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub output: String,
    pub truncated: bool,
    pub timed_out: bool,
    pub switched_to_async: bool,
    pub denied: bool,
    pub denial_reason: Option<String>,
    pub duration_secs: Option<f64>,
    /// What was added/prepended to sudo commands (e.g., "-n" for non-interactive)
    pub sudo_wrapper_applied: Option<String>,
}

pub async fn execute_command(
    input: &TerminalExecutionInput,
    job_manager: &JobManager,
    peer: Option<Peer<RoleServer>>,
) -> Result<ExecutionResult> {
    let command = input.command.trim();

    tracing::debug!(
        "execute_command called: command={}, force_sync={}",
        command,
        input.force_sync
    );

    // Optional: server-side sudo wrapping (opt-in).
    // When enabled, any command containing `sudo` is executed such that:
    // - sudo authentication is primed in the server process context (askpass)
    // - the actual privileged command runs via `sudo -n` so it never prompts
    //
    // This allows sudo timestamp reuse across multiple tool calls.
    if env_bool("ENHANCED_TERMINAL_SUDO_WRAP") && sudo_looks_used(command) {
        let report = ensure_sudo_primed_for_wrap(command, &input.env_vars).await;
        let wrapped = wrap_sudo_command_for_server(command);
        return execute_command_inner(&wrapped, input, job_manager, report, peer).await;
    }

    let sudo_prime_report = maybe_start_sudo_keepalive(command, &input.env_vars).await;

    if command.is_empty() {
        return Err(anyhow::anyhow!("Command cannot be empty"));
    }

    // Check denylist
    if is_denied(command, &input.custom_denylist) {
        let matched_pattern = find_matched_pattern(command, &input.custom_denylist);
        return Ok(ExecutionResult {
            job_id: String::new(),
            command: command.to_string(),
            working_directory: String::new(),
            exit_code: None,
            success: false,
            output: String::new(),
            truncated: false,
            timed_out: false,
            switched_to_async: false,
            denied: true,
            denial_reason: Some(format!(
                "Command denied by security policy. Matched pattern: {}",
                matched_pattern.unwrap_or_else(|| "unknown".to_string())
            )),
            duration_secs: None,
            sudo_wrapper_applied: None,
        });
    }

    // Resolve working directory and canonicalize (resolve symlinks)
    let cwd = if input.cwd == "." || input.cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(&input.cwd)
    };

    // Canonicalize to resolve symlinks
    let cwd = cwd.canonicalize().unwrap_or(cwd);

    // Create PTY system
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow::anyhow!("Failed to open PTY: {}", e))?;

    // Build command
    let mut cmd = CommandBuilder::new(&input.shell);
    cmd.arg("-c");
    cmd.arg(command);
    cmd.cwd(&cwd);

    // Set environment variables
    let env_vars = apply_default_env(input.env_vars.clone());
    for (key, value) in env_vars {
        cmd.env(key, value);
    }

    // Start the process
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| anyhow::anyhow!("Failed to spawn: {}", e))?;

    let pid = child.process_id();
    drop(pair.slave);

    // Register job
    let job_id = job_manager.new_job_id();
    job_manager.register_job_with_tags(
        job_id.clone(),
        command.to_string(),
        input.shell.clone(),
        cwd.display().to_string(),
        pid,
        input.tags.clone(),
    );

    // Read output with smart async switching
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("Failed to clone reader: {}", e))?;

    let preview_byte_limit = preview_buffer_limit(input);
    let timeout = get_timeout_secs().map(Duration::from_secs);
    let async_threshold = Duration::from_secs(get_async_threshold_secs());
    let start_time = Instant::now();

    let mut output = Vec::new();
    let mut truncated = false;
    let mut timed_out = false;
    let mut switched_to_async = false;

    // Channel for receiving output from reader task
    #[derive(Debug)]
    enum ReadMsg {
        Data(Vec<u8>),
        Eof,
        Error,
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<ReadMsg>();

    // Spawn reader task using tokio::task::spawn_blocking (PTY read is blocking I/O)
    let reader_job_id = job_id.clone();
    tokio::task::spawn_blocking(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    tracing::debug!("Reader task: EOF, job_id={}", reader_job_id);
                    let _ = tx.send(ReadMsg::Eof);
                    break;
                }
                Ok(n) => {
                    let data = buffer[..n].to_vec();
                    if tx.send(ReadMsg::Data(data)).is_err() {
                        break; // Main task dropped receiver
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Reader task error: {:?}, job_id={}", e, reader_job_id);
                    let _ = tx.send(ReadMsg::Error);
                    break;
                }
            }
        }
    });

    // Main loop: check elapsed time independently and receive output
    let check_interval = Duration::from_millis(100);
    loop {
        let elapsed = start_time.elapsed();

        // Check if we should switch to async (independent of I/O)
        if !input.force_sync && elapsed > async_threshold {
            tracing::debug!(
                "Main task: async threshold reached at {:.2}s, job_id={}",
                elapsed.as_secs_f64(),
                job_id
            );
            switched_to_async = true;
            break;
        }

        // Check for overall timeout (if set)
        if let Some(timeout_duration) = timeout {
            if elapsed > timeout_duration {
                tracing::debug!("Main task: timeout reached, job_id={}", job_id);
                let _ = child.kill();
                timed_out = true;
                break;
            }
        }

        // Try to receive output from reader task with timeout
        match tokio::time::timeout(check_interval, rx.recv()).await {
            Ok(Some(ReadMsg::Data(data))) => {
                truncated |= append_to_preview_buffer(&mut output, &data, preview_byte_limit);

                // Update job with incremental output
                let output_str = String::from_utf8_lossy(&data).to_string();
                job_manager.append_output(&job_id, &output_str, preview_byte_limit);

                // Send streaming notification if peer is available
                if let Some(ref peer) = peer {
                    let _ = peer
                        .notify_logging_message(LoggingMessageNotificationParam {
                            level: LoggingLevel::Info,
                            logger: Some("enhanced_terminal".to_string()),
                            data: serde_json::json!({
                                "job_id": &job_id,
                                "output": &output_str,
                                "type": "stream"
                            }),
                        })
                        .await;
                }
            }
            Ok(Some(ReadMsg::Eof)) => {
                tracing::debug!("Main task: EOF received, job_id={}", job_id);
                break;
            }
            Ok(Some(ReadMsg::Error)) => {
                tracing::warn!("Main task: read error received, job_id={}", job_id);
                break;
            }
            Ok(None) => {
                tracing::warn!("Main task: reader task disconnected, job_id={}", job_id);
                break;
            }
            Err(_) => {
                // Timeout waiting for data - this is normal, continue loop to check elapsed time
                continue;
            }
        }
    }

    if switched_to_async {
        tracing::info!(
            "Command switched to async mode: job_id={}, elapsed={:.2}s, output_so_far={} bytes",
            job_id,
            start_time.elapsed().as_secs_f64(),
            output.len()
        );

        // Spawn background task to continue monitoring
        let job_manager_clone = job_manager.clone();
        let job_id_clone = job_id.clone();
        let timeout_remaining = timeout.map(|t| t.saturating_sub(start_time.elapsed()));
        let child_arc = Arc::new(tokio::sync::Mutex::new(child));

        tokio::spawn(async move {
            tracing::debug!("Background task started for job_id={}", job_id_clone);
            let start_bg = Instant::now();

            // Continue receiving from the reader task channel
            loop {
                // Check for timeout (if set)
                if let Some(timeout_dur) = timeout_remaining {
                    if start_bg.elapsed() > timeout_dur {
                        let mut child_guard = child_arc.lock().await;
                        let _ = child_guard.kill();
                        drop(child_guard);
                        job_manager_clone.complete_job(&job_id_clone, None, JobStatus::TimedOut);
                        break;
                    }
                }

                match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                    Ok(Some(ReadMsg::Data(data))) => {
                        let output_str = String::from_utf8_lossy(&data).to_string();
                        job_manager_clone.append_output(
                            &job_id_clone,
                            &output_str,
                            preview_byte_limit,
                        );
                    }
                    Ok(Some(ReadMsg::Eof)) => {
                        // Process finished
                        let mut child_guard = child_arc.lock().await;
                        let exit_status = child_guard.wait().ok();
                        let exit_code = exit_status.map(|s| s.exit_code() as i32);
                        let status = if exit_code == Some(0) {
                            JobStatus::Completed
                        } else {
                            JobStatus::Failed
                        };
                        tracing::debug!(
                            "Background job completed: job_id={}, exit_code={:?}, status={:?}",
                            job_id_clone,
                            exit_code,
                            status
                        );
                        job_manager_clone.complete_job(&job_id_clone, exit_code, status);
                        break;
                    }
                    Ok(Some(ReadMsg::Error)) => {
                        job_manager_clone.complete_job(&job_id_clone, None, JobStatus::Failed);
                        break;
                    }
                    Ok(None) => {
                        // Reader task died unexpectedly
                        job_manager_clone.complete_job(&job_id_clone, None, JobStatus::Failed);
                        break;
                    }
                    Err(_) => {
                        // Timeout waiting for data, continue loop to check timeout
                        continue;
                    }
                }
            }
        });

        // Return immediately with a bounded preview and duration so far.
        let raw_output_str = String::from_utf8_lossy(&output).to_string();
        let preview = preview_output(&raw_output_str, input.preview_tokens);
        let output_str = preview.text;
        truncated |= preview.truncated;
        let duration_secs = start_time.elapsed().as_secs_f64();
        tracing::info!(
            "Returning async result: job_id={}, duration={:.2}s",
            job_id,
            duration_secs
        );
        return Ok(ExecutionResult {
            job_id,
            command: command.to_string(),
            working_directory: cwd.display().to_string(),
            exit_code: None,
            success: false,
            output: output_str,
            truncated,
            timed_out: false,
            switched_to_async: true,
            denied: false,
            denial_reason: None,
            duration_secs: Some(duration_secs),
            sudo_wrapper_applied: if sudo_looks_used(command) {
                Some("-n".to_string())
            } else {
                None
            },
        });
    }

    // Synchronous completion
    let exit_status = child.wait().ok();
    let exit_code = exit_status.map(|s| s.exit_code() as i32);
    let success = exit_code.map(|c| c == 0).unwrap_or(false);

    let raw_output_str = String::from_utf8_lossy(&output).to_string();
    let preview = preview_output(&raw_output_str, input.preview_tokens);
    let mut output_str = preview.text;
    truncated |= preview.truncated;

    // Complete job
    let status = if timed_out {
        JobStatus::TimedOut
    } else if success {
        JobStatus::Completed
    } else {
        JobStatus::Failed
    };
    job_manager.complete_job(&job_id, exit_code, status);

    let duration_secs = start_time.elapsed().as_secs_f64();

    tracing::debug!(
        "Synchronous command completed: job_id={}, exit_code={:?}, duration={:.2}s",
        job_id,
        exit_code,
        duration_secs
    );

    // If this command used sudo, surface any priming attempt diagnostics in the output.
    // This makes failures visible even when MCP server logs aren't easily accessible.
    //
    // Note: we only surface the report produced by the keepalive/priming attempt for *this*
    // command invocation to avoid introducing extra async plumbing or dependencies.
    if sudo_looks_used(command) {
        if let Some(report) = sudo_prime_report {
            let mut diag = String::new();
            diag.push_str("\n\n[SUDO_PRIME]\n");
            diag.push_str(&format!("success: {}\n", report.success));
            if let Some(code) = report.exit_code {
                diag.push_str(&format!("exit_code: {}\n", code));
            }
            if let Some(askpass) = report.askpass {
                diag.push_str(&format!("askpass: {}\n", askpass));
            }
            if let Some(v) = report.display {
                diag.push_str(&format!("DISPLAY: {}\n", v));
            }
            if let Some(v) = report.wayland_display {
                diag.push_str(&format!("WAYLAND_DISPLAY: {}\n", v));
            }
            if let Some(v) = report.xdg_runtime_dir {
                diag.push_str(&format!("XDG_RUNTIME_DIR: {}\n", v));
            }
            if let Some(v) = report.dbus_session_bus_address {
                diag.push_str(&format!("DBUS_SESSION_BUS_ADDRESS: {}\n", v));
            }
            if !report.stderr.trim().is_empty() {
                diag.push_str("stderr:\n");
                diag.push_str(report.stderr.trim());
                diag.push('\n');
            }
            output_str.push_str(&diag);
        }
    }

    Ok(ExecutionResult {
        job_id,
        command: command.to_string(),
        working_directory: cwd.display().to_string(),
        exit_code,
        success,
        output: output_str,
        truncated,
        timed_out,
        switched_to_async: false,
        denied: false,
        denial_reason: None,
        duration_secs: Some(duration_secs),
        sudo_wrapper_applied: if sudo_looks_used(command) {
            Some("-n".to_string())
        } else {
            None
        },
    })
}

async fn execute_command_inner(
    command: &str,
    input: &TerminalExecutionInput,
    job_manager: &JobManager,
    sudo_prime_report: Option<SudoPrimeReport>,
    peer: Option<Peer<RoleServer>>,
) -> Result<ExecutionResult> {
    let command = command.trim();

    if command.is_empty() {
        return Err(anyhow::anyhow!("Command cannot be empty"));
    }

    // Check denylist
    if is_denied(command, &input.custom_denylist) {
        let matched_pattern = find_matched_pattern(command, &input.custom_denylist);
        return Ok(ExecutionResult {
            job_id: String::new(),
            command: command.to_string(),
            working_directory: String::new(),
            exit_code: None,
            success: false,
            output: String::new(),
            truncated: false,
            timed_out: false,
            switched_to_async: false,
            denied: true,
            denial_reason: Some(format!(
                "Command denied by security policy. Matched pattern: {}",
                matched_pattern.unwrap_or_else(|| "unknown".to_string())
            )),
            duration_secs: None,
            sudo_wrapper_applied: None,
        });
    }

    // Resolve working directory and canonicalize (resolve symlinks)
    let cwd = if input.cwd == "." || input.cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(&input.cwd)
    };

    // Canonicalize to resolve symlinks
    let cwd = cwd.canonicalize().unwrap_or(cwd);

    // Create PTY system
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow::anyhow!("Failed to open PTY: {}", e))?;

    // Build command
    let mut cmd = CommandBuilder::new(&input.shell);
    cmd.arg("-c");
    cmd.arg(command);
    cmd.cwd(&cwd);

    // Set environment variables
    let env_vars = apply_default_env(input.env_vars.clone());
    for (key, value) in env_vars {
        cmd.env(key, value);
    }

    // Start the process
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| anyhow::anyhow!("Failed to spawn: {}", e))?;

    let pid = child.process_id();
    drop(pair.slave);

    // Register job
    let job_id = job_manager.new_job_id();
    job_manager.register_job_with_tags(
        job_id.clone(),
        command.to_string(),
        input.shell.clone(),
        cwd.display().to_string(),
        pid,
        input.tags.clone(),
    );

    // Read output with smart async switching
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("Failed to clone reader: {}", e))?;

    let preview_byte_limit = preview_buffer_limit(input);
    let timeout = get_timeout_secs().map(Duration::from_secs);
    let async_threshold = Duration::from_secs(get_async_threshold_secs());
    let start_time = Instant::now();

    let mut output = Vec::new();
    let mut truncated = false;
    let mut timed_out = false;
    let mut switched_to_async = false;

    // Channel for receiving output from reader task
    #[derive(Debug)]
    enum ReadMsg {
        Data(Vec<u8>),
        Eof,
        Error,
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<ReadMsg>();

    // Spawn reader task using tokio::task::spawn_blocking (PTY read is blocking I/O)
    let reader_job_id = job_id.clone();
    tokio::task::spawn_blocking(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    tracing::debug!("Reader task: EOF, job_id={}", reader_job_id);
                    let _ = tx.send(ReadMsg::Eof);
                    break;
                }
                Ok(n) => {
                    let data = buffer[..n].to_vec();
                    if tx.send(ReadMsg::Data(data)).is_err() {
                        break; // Main task dropped receiver
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Reader task error: {:?}, job_id={}", e, reader_job_id);
                    let _ = tx.send(ReadMsg::Error);
                    break;
                }
            }
        }
    });

    // Main loop: check elapsed time independently and receive output
    let check_interval = Duration::from_millis(100);
    loop {
        let elapsed = start_time.elapsed();

        // Check if we should switch to async (independent of I/O)
        if !input.force_sync && elapsed > async_threshold {
            tracing::debug!(
                "Main task: async threshold reached at {:.2}s, job_id={}",
                elapsed.as_secs_f64(),
                job_id
            );
            switched_to_async = true;
            break;
        }

        // Check for overall timeout (if set)
        if let Some(timeout_duration) = timeout {
            if elapsed > timeout_duration {
                tracing::debug!("Main task: timeout reached, job_id={}", job_id);
                let _ = child.kill();
                timed_out = true;
                break;
            }
        }

        // Try to receive output from reader task with timeout
        match tokio::time::timeout(check_interval, rx.recv()).await {
            Ok(Some(ReadMsg::Data(data))) => {
                truncated |= append_to_preview_buffer(&mut output, &data, preview_byte_limit);

                // Update job with incremental output
                let output_str = String::from_utf8_lossy(&data).to_string();
                job_manager.append_output(&job_id, &output_str, preview_byte_limit);

                // Send streaming notification if peer is available
                if let Some(ref peer) = peer {
                    let _ = peer
                        .notify_logging_message(LoggingMessageNotificationParam {
                            level: LoggingLevel::Info,
                            logger: Some("enhanced_terminal".to_string()),
                            data: serde_json::json!({
                                "job_id": &job_id,
                                "output": &output_str,
                                "type": "stream"
                            }),
                        })
                        .await;
                }
            }
            Ok(Some(ReadMsg::Eof)) => {
                tracing::debug!("Main task: EOF received, job_id={}", job_id);
                break;
            }
            Ok(Some(ReadMsg::Error)) => {
                tracing::warn!("Main task: read error received, job_id={}", job_id);
                break;
            }
            Ok(None) => {
                tracing::warn!("Main task: reader task disconnected, job_id={}", job_id);
                break;
            }
            Err(_) => {
                // Timeout waiting for data - this is normal, continue loop to check elapsed time
                continue;
            }
        }
    }

    if switched_to_async {
        tracing::info!(
            "Command switched to async mode: job_id={}, elapsed={:.2}s, output_so_far={} bytes",
            job_id,
            start_time.elapsed().as_secs_f64(),
            output.len()
        );

        // Spawn background task to continue monitoring
        let job_manager_clone = job_manager.clone();
        let job_id_clone = job_id.clone();
        let timeout_remaining = timeout.map(|t| t.saturating_sub(start_time.elapsed()));
        let child_arc = Arc::new(tokio::sync::Mutex::new(child));

        tokio::spawn(async move {
            tracing::debug!("Background task started for job_id={}", job_id_clone);
            let start_bg = Instant::now();

            // Continue receiving from the reader task channel
            loop {
                // Check for timeout (if set)
                if let Some(timeout_dur) = timeout_remaining {
                    if start_bg.elapsed() > timeout_dur {
                        let mut child_guard = child_arc.lock().await;
                        let _ = child_guard.kill();
                        drop(child_guard);
                        job_manager_clone.complete_job(&job_id_clone, None, JobStatus::TimedOut);
                        break;
                    }
                }

                match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                    Ok(Some(ReadMsg::Data(data))) => {
                        let output_str = String::from_utf8_lossy(&data).to_string();
                        job_manager_clone.append_output(
                            &job_id_clone,
                            &output_str,
                            preview_byte_limit,
                        );
                    }
                    Ok(Some(ReadMsg::Eof)) => {
                        // Process finished
                        let mut child_guard = child_arc.lock().await;
                        let exit_status = child_guard.wait().ok();
                        let exit_code = exit_status.map(|s| s.exit_code() as i32);
                        let status = if exit_code == Some(0) {
                            JobStatus::Completed
                        } else {
                            JobStatus::Failed
                        };
                        tracing::debug!(
                            "Background job completed: job_id={}, exit_code={:?}, status={:?}",
                            job_id_clone,
                            exit_code,
                            status
                        );
                        job_manager_clone.complete_job(&job_id_clone, exit_code, status);
                        break;
                    }
                    Ok(Some(ReadMsg::Error)) => {
                        job_manager_clone.complete_job(&job_id_clone, None, JobStatus::Failed);
                        break;
                    }
                    Ok(None) => {
                        // Reader task died unexpectedly
                        job_manager_clone.complete_job(&job_id_clone, None, JobStatus::Failed);
                        break;
                    }
                    Err(_) => {
                        // Timeout waiting for data, continue loop to check timeout
                        continue;
                    }
                }
            }
        });

        // Return immediately with a bounded preview and duration so far.
        let raw_output_str = String::from_utf8_lossy(&output).to_string();
        let preview = preview_output(&raw_output_str, input.preview_tokens);
        let mut output_str = preview.text;
        truncated |= preview.truncated;
        let duration_secs = start_time.elapsed().as_secs_f64();

        if sudo_looks_used(command) {
            if let Some(report) = sudo_prime_report {
                let mut diag = String::new();
                diag.push_str("\n\n[SUDO_PRIME]\n");
                diag.push_str(&format!("success: {}\n", report.success));
                if let Some(code) = report.exit_code {
                    diag.push_str(&format!("exit_code: {}\n", code));
                }
                if let Some(askpass) = report.askpass {
                    diag.push_str(&format!("askpass: {}\n", askpass));
                }
                if let Some(v) = report.display {
                    diag.push_str(&format!("DISPLAY: {}\n", v));
                }
                if let Some(v) = report.wayland_display {
                    diag.push_str(&format!("WAYLAND_DISPLAY: {}\n", v));
                }
                if let Some(v) = report.xdg_runtime_dir {
                    diag.push_str(&format!("XDG_RUNTIME_DIR: {}\n", v));
                }
                if let Some(v) = report.dbus_session_bus_address {
                    diag.push_str(&format!("DBUS_SESSION_BUS_ADDRESS: {}\n", v));
                }
                if !report.stderr.trim().is_empty() {
                    diag.push_str("stderr:\n");
                    diag.push_str(report.stderr.trim());
                    diag.push('\n');
                }
                output_str.push_str(&diag);
            }
        }

        tracing::info!(
            "Returning async result: job_id={}, duration={:.2}s",
            job_id,
            duration_secs
        );
        return Ok(ExecutionResult {
            job_id,
            command: command.to_string(),
            working_directory: cwd.display().to_string(),
            exit_code: None,
            success: false,
            output: output_str,
            truncated,
            timed_out: false,
            switched_to_async: true,
            denied: false,
            denial_reason: None,
            duration_secs: Some(duration_secs),
            sudo_wrapper_applied: if sudo_looks_used(command) {
                Some("-n".to_string())
            } else {
                None
            },
        });
    }

    // Synchronous completion
    let exit_status = child.wait().ok();
    let exit_code = exit_status.map(|s| s.exit_code() as i32);
    let success = exit_code.map(|c| c == 0).unwrap_or(false);

    let raw_output_str = String::from_utf8_lossy(&output).to_string();
    let preview = preview_output(&raw_output_str, input.preview_tokens);
    let mut output_str = preview.text;
    truncated |= preview.truncated;

    // Complete job
    let status = if timed_out {
        JobStatus::TimedOut
    } else if success {
        JobStatus::Completed
    } else {
        JobStatus::Failed
    };
    job_manager.complete_job(&job_id, exit_code, status);

    let duration_secs = start_time.elapsed().as_secs_f64();

    tracing::debug!(
        "Synchronous command completed: job_id={}, exit_code={:?}, duration={:.2}s",
        job_id,
        exit_code,
        duration_secs
    );

    if sudo_looks_used(command) {
        if let Some(report) = sudo_prime_report {
            let mut diag = String::new();
            diag.push_str("\n\n[SUDO_PRIME]\n");
            diag.push_str(&format!("success: {}\n", report.success));
            if let Some(code) = report.exit_code {
                diag.push_str(&format!("exit_code: {}\n", code));
            }
            if let Some(askpass) = report.askpass {
                diag.push_str(&format!("askpass: {}\n", askpass));
            }
            if let Some(v) = report.display {
                diag.push_str(&format!("DISPLAY: {}\n", v));
            }
            if let Some(v) = report.wayland_display {
                diag.push_str(&format!("WAYLAND_DISPLAY: {}\n", v));
            }
            if let Some(v) = report.xdg_runtime_dir {
                diag.push_str(&format!("XDG_RUNTIME_DIR: {}\n", v));
            }
            if let Some(v) = report.dbus_session_bus_address {
                diag.push_str(&format!("DBUS_SESSION_BUS_ADDRESS: {}\n", v));
            }
            if !report.stderr.trim().is_empty() {
                diag.push_str("stderr:\n");
                diag.push_str(report.stderr.trim());
                diag.push('\n');
            }
            output_str.push_str(&diag);
        }
    }

    Ok(ExecutionResult {
        job_id,
        command: command.to_string(),
        working_directory: cwd.display().to_string(),
        exit_code,
        success,
        output: output_str,
        truncated,
        timed_out,
        switched_to_async: false,
        denied: false,
        denial_reason: None,
        duration_secs: Some(duration_secs),
        sudo_wrapper_applied: if sudo_looks_used(command) {
            Some("-n".to_string())
        } else {
            None
        },
    })
}

static SUDO_KEEPALIVE_TASK: OnceLock<TokioMutex<bool>> = OnceLock::new();
static SUDO_PRIME_LAST: OnceLock<TokioMutex<Option<SudoPrimeReport>>> = OnceLock::new();

#[derive(Debug, Clone)]
struct SudoPrimeReport {
    success: bool,
    askpass: Option<String>,
    display: Option<String>,
    wayland_display: Option<String>,
    xdg_runtime_dir: Option<String>,
    dbus_session_bus_address: Option<String>,
    exit_code: Option<i32>,
    stderr: String,
}

fn sudo_looks_used(command: &str) -> bool {
    command.contains("sudo ") || command.starts_with("sudo")
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn default_gnome_session_env(
    env_vars: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<&'static str, String> {
    // Provide sane defaults for GNOME/Wayland GUI askpass helpers.
    // These are only used when priming via `sudo -A -v` in the server context.
    let mut out = std::collections::HashMap::new();

    let display = env_string("DISPLAY")
        .or_else(|| env_vars.get("DISPLAY").cloned())
        .unwrap_or_else(|| ":0".to_string());
    out.insert("DISPLAY", display);

    let wayland = env_string("WAYLAND_DISPLAY")
        .or_else(|| env_vars.get("WAYLAND_DISPLAY").cloned())
        .unwrap_or_else(|| "wayland-0".to_string());
    out.insert("WAYLAND_DISPLAY", wayland);

    // These are often required for GUI dialogs under Wayland
    if let Some(xdg) =
        env_string("XDG_RUNTIME_DIR").or_else(|| env_vars.get("XDG_RUNTIME_DIR").cloned())
    {
        out.insert("XDG_RUNTIME_DIR", xdg);
    }
    if let Some(dbus) = env_string("DBUS_SESSION_BUS_ADDRESS")
        .or_else(|| env_vars.get("DBUS_SESSION_BUS_ADDRESS").cloned())
    {
        out.insert("DBUS_SESSION_BUS_ADDRESS", dbus);
    }

    out
}

async fn record_sudo_prime_report(report: SudoPrimeReport) {
    let cell = SUDO_PRIME_LAST.get_or_init(|| TokioMutex::new(None));
    let mut guard = cell.lock().await;
    *guard = Some(report);
}

async fn take_sudo_prime_report() -> Option<SudoPrimeReport> {
    let cell = SUDO_PRIME_LAST.get_or_init(|| TokioMutex::new(None));
    let mut guard = cell.lock().await;
    guard.take()
}

async fn sudo_prime_with_askpass(
    askpass: &str,
    env_vars: &std::collections::HashMap<String, String>,
) -> bool {
    // Prime sudo credentials once using askpass (may prompt).
    // This runs in the server process context (not the PTY), so the sudo timestamp
    // is shared across subsequent tool invocations.
    let session_env = default_gnome_session_env(env_vars);

    let display = session_env.get("DISPLAY").cloned();
    let wayland_display = session_env.get("WAYLAND_DISPLAY").cloned();
    let xdg_runtime_dir = session_env.get("XDG_RUNTIME_DIR").cloned();
    let dbus_session_bus_address = session_env.get("DBUS_SESSION_BUS_ADDRESS").cloned();

    let mut cmd = tokio::process::Command::new("sudo");
    cmd.arg("-A").arg("-v");
    cmd.env("SUDO_ASKPASS", askpass);

    for (k, v) in &session_env {
        cmd.env(k, v);
    }

    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());

    let output = cmd.output().await;

    match output {
        Ok(out) => {
            let success = out.status.success();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let exit_code = out.status.code();

            if success {
                tracing::info!("sudo keepalive: sudo -A -v succeeded");
            } else {
                tracing::warn!(
                    "sudo keepalive: sudo -A -v failed (status={:?}) stderr={}",
                    exit_code,
                    stderr.trim()
                );
            }

            record_sudo_prime_report(SudoPrimeReport {
                success,
                askpass: Some(askpass.to_string()),
                display,
                wayland_display,
                xdg_runtime_dir,
                dbus_session_bus_address,
                exit_code,
                stderr,
            })
            .await;

            success
        }
        Err(e) => {
            tracing::warn!("sudo keepalive: failed to run sudo -A -v: {}", e);

            record_sudo_prime_report(SudoPrimeReport {
                success: false,
                askpass: Some(askpass.to_string()),
                display: env_string("DISPLAY"),
                wayland_display: env_string("WAYLAND_DISPLAY"),
                xdg_runtime_dir: env_string("XDG_RUNTIME_DIR"),
                dbus_session_bus_address: env_string("DBUS_SESSION_BUS_ADDRESS"),
                exit_code: None,
                stderr: e.to_string(),
            })
            .await;

            false
        }
    }
}

fn env_bool(name: &str) -> bool {
    // Default these features ON unless explicitly disabled.
    // Supported values for true: 1/true/yes/on
    // Supported values for false: 0/false/no/off
    let default_true = matches!(
        name,
        "ENHANCED_TERMINAL_SUDO_KEEPALIVE"
            | "ENHANCED_TERMINAL_SUDO_KEEPALIVE_PRIME"
            | "ENHANCED_TERMINAL_SUDO_WRAP"
    );

    let raw = std::env::var(name).ok().unwrap_or_default();
    let v = raw.trim().to_ascii_lowercase();

    if v.is_empty() {
        return default_true;
    }

    match v.as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default_true,
    }
}

async fn ensure_sudo_primed_for_wrap(
    _command: &str,
    env_vars: &std::collections::HashMap<String, String>,
) -> Option<SudoPrimeReport> {
    if !env_bool("ENHANCED_TERMINAL_SUDO_KEEPALIVE_PRIME") {
        return None;
    }

    // If already cached in server context, don't prompt/prime again.
    let need_prime = tokio::process::Command::new("sudo")
        .arg("-n")
        .arg("-v")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| !s.success())
        .unwrap_or(true);

    if !need_prime {
        return None;
    }

    // Prefer server env askpass, then per-command env, then a conventional default path.
    let askpass = env_string("ENHANCED_TERMINAL_SUDO_ASKPASS")
        .or_else(|| env_string("SUDO_ASKPASS"))
        .or_else(|| env_vars.get("SUDO_ASKPASS").cloned())
        .or_else(|| {
            Some(format!(
                "{}/scripts/askpass-zenity.sh",
                std::env::var("HOME").ok().unwrap_or_default()
            ))
        })
        .filter(|p| !p.trim().is_empty());

    if let Some(askpass) = askpass {
        tracing::info!("sudo wrap: priming sudo credentials via askpass (command contains sudo)");
        let _ok = sudo_prime_with_askpass(&askpass, env_vars).await;
        return take_sudo_prime_report().await;
    }

    record_sudo_prime_report(SudoPrimeReport {
        success: false,
        askpass: None,
        display: env_string("DISPLAY"),
        wayland_display: env_string("WAYLAND_DISPLAY"),
        xdg_runtime_dir: env_string("XDG_RUNTIME_DIR"),
        dbus_session_bus_address: env_string("DBUS_SESSION_BUS_ADDRESS"),
        exit_code: None,
        stderr: "sudo wrap: priming enabled but no askpass configured".to_string(),
    })
    .await;

    take_sudo_prime_report().await
}

fn wrap_sudo_command_for_server(command: &str) -> String {
    // Replace any sudo invocation with a non-interactive sudo so the command never prompts.
    // This is a best-effort string rewrite; it intentionally stays conservative.
    //
    // Examples:
    // - "sudo apt update" -> "sudo -n apt update"
    // - "cmd && sudo id"  -> "cmd && sudo -n id"
    //
    // This does not attempt to perfectly parse shell syntax.
    // It is opt-in and intended for workflows where you control the command content.
    let mut out = command.to_string();

    // Start-of-command "sudo "
    if out.starts_with("sudo ") {
        out = out.replacen("sudo ", "sudo -n ", 1);
    }

    // Common separators that can precede sudo. Keep spaces to reduce false positives.
    for pat in [
        "&& sudo ",
        "|| sudo ",
        "; sudo ",
        "\n sudo ",
        "\n\tsudo ",
        "\tsudo ",
    ] {
        if out.contains(pat) {
            out = out.replace(pat, &pat.replace("sudo ", "sudo -n "));
        }
    }

    out
}

fn env_u64(name: &str, default_value: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default_value)
}

async fn maybe_start_sudo_keepalive(
    command: &str,
    env_vars: &std::collections::HashMap<String, String>,
) -> Option<SudoPrimeReport> {
    // Opt-in via env:
    // - ENHANCED_TERMINAL_SUDO_KEEPALIVE=1 enables keepalive behavior
    // - ENHANCED_TERMINAL_SUDO_KEEPALIVE_REFRESH_SECS controls refresh interval (default 300s)
    //
    // Priming:
    // - If enabled, command uses sudo, and `sudo -n -v` fails (no cached timestamp),
    //   optionally prime credentials once via `sudo -A -v` using an askpass helper.
    //
    // Keepalive:
    // - Background task periodically runs `sudo -n -v` to refresh the sudo timestamp (no prompt).
    if !env_bool("ENHANCED_TERMINAL_SUDO_KEEPALIVE") {
        return None;
    }
    if !sudo_looks_used(command) {
        return None;
    }

    // Prime once (optional) in the long-lived server context
    // Askpass resolution order:
    // 1) ENHANCED_TERMINAL_SUDO_ASKPASS (server env)
    // 2) SUDO_ASKPASS (server env)
    // 3) env_vars["SUDO_ASKPASS"] (per-tool env passed by client)
    if env_bool("ENHANCED_TERMINAL_SUDO_KEEPALIVE_PRIME") {
        let need_prime = tokio::process::Command::new("sudo")
            .arg("-n")
            .arg("-v")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| !s.success())
            .unwrap_or(true);

        if need_prime {
            let askpass = env_string("ENHANCED_TERMINAL_SUDO_ASKPASS")
                .or_else(|| env_string("SUDO_ASKPASS"))
                .or_else(|| env_vars.get("SUDO_ASKPASS").cloned())
                .or_else(|| {
                    Some(format!(
                        "{}/scripts/askpass-zenity.sh",
                        std::env::var("HOME").ok().unwrap_or_default()
                    ))
                })
                .filter(|p| !p.trim().is_empty());

            if let Some(askpass) = askpass {
                tracing::info!("sudo keepalive: priming sudo credentials via askpass");
                let _ok = sudo_prime_with_askpass(&askpass, env_vars).await;
                // Report is stored; return it so caller can surface it in output.
                return take_sudo_prime_report().await;
            } else {
                tracing::warn!(
                    "sudo keepalive: priming enabled but no askpass configured (set ENHANCED_TERMINAL_SUDO_ASKPASS or SUDO_ASKPASS)"
                );
                record_sudo_prime_report(SudoPrimeReport {
                    success: false,
                    askpass: None,
                    display: env_string("DISPLAY"),
                    wayland_display: env_string("WAYLAND_DISPLAY"),
                    xdg_runtime_dir: env_string("XDG_RUNTIME_DIR"),
                    dbus_session_bus_address: env_string("DBUS_SESSION_BUS_ADDRESS"),
                    exit_code: None,
                    stderr: "no askpass configured".to_string(),
                })
                .await;
                return take_sudo_prime_report().await;
            }
        }
    }

    // Start keepalive loop at most once
    let gate = SUDO_KEEPALIVE_TASK.get_or_init(|| TokioMutex::new(false));
    let mut started = gate.lock().await;
    if *started {
        return None;
    }
    *started = true;
    drop(started);

    let refresh_secs = env_u64("ENHANCED_TERMINAL_SUDO_KEEPALIVE_REFRESH_SECS", 300).max(30);
    tracing::info!(
        "Starting sudo keepalive task (refresh_secs={})",
        refresh_secs
    );

    tokio::spawn(async move {
        let interval = Duration::from_secs(refresh_secs);
        loop {
            tokio::time::sleep(interval).await;

            // `sudo -n -v` refreshes timestamp if already authenticated; fails if it would prompt.
            let status = tokio::process::Command::new("sudo")
                .arg("-n")
                .arg("-v")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;

            match status {
                Ok(s) if s.success() => {
                    tracing::debug!("sudo keepalive refreshed timestamp");
                }
                Ok(_) => {
                    tracing::debug!("sudo keepalive: no cached credentials (sudo -n -v failed)");
                }
                Err(e) => {
                    tracing::warn!("sudo keepalive: failed to run sudo -n -v: {}", e);
                }
            }
        }
    });
    None
}
