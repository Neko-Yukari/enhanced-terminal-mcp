use super::output_view::ReadDirection;
use anyhow::Result;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use rmcp::{
    model::{LoggingLevel, LoggingMessageNotificationParam},
    service::{RequestContext, RoleServer},
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
    /// Shell command. Supply either command or program, never both.
    #[serde(default)]
    pub command: String,
    /// Executable path/name launched directly, without shell parsing.
    #[serde(default)]
    pub program: Option<String>,
    /// Exact arguments for program, including empty strings and embedded quotes.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory (default: "."). When omitted, "." is resolved from the
    /// MCP server process working directory supplied by the caller/client.
    #[serde(default = "default_cwd")]
    pub cwd: String,
    /// Shell for command mode (Windows: "pwsh", other platforms: "bash")
    #[serde(default = "default_shell")]
    pub shell: String,
    /// Maximum number of GPT-5/o200k_base tokens to return in command output previews.
    /// Set to 0 to disable token truncation for the bounded in-memory preview buffer.
    #[serde(default = "default_preview_tokens")]
    pub preview_tokens: usize,
    /// Initial preview direction, head by default; tail is displayed in normal order.
    #[serde(default)]
    pub direction: ReadDirection,
    /// Environment variables to set for the command
    #[serde(default)]
    pub env_vars: std::collections::HashMap<String, String>,
    /// Wait synchronously for up to 60 seconds, then return a background job.
    #[serde(default)]
    pub force_sync: bool,
    /// Force immediate background execution and return a job_id without waiting for the async threshold.
    #[serde(default)]
    pub force_async: bool,
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

pub fn default_shell() -> String {
    #[cfg(windows)]
    return "pwsh".to_string();
    #[cfg(not(windows))]
    return "bash".to_string();
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
        .entry("GIT_PAGER".to_string())
        .or_insert_with(|| "cat".to_string());
    env_vars
        .entry("PAGER".to_string())
        .or_insert_with(|| "cat".to_string());
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
            };
        }

        // A token boundary can be inside a Unicode code point. Back off tokens,
        // rather than replacing characters or exceeding the requested budget.
        let mut end = preview_tokens;
        let preview = loop {
            if let Ok(text) = bpe.decode(&tokens[..end]) { break text; }
            end -= 1;
        };

        return PreviewedOutput {
            text: preview,
            truncated: true,
            tokens: Some(token_count),
        };
    }

    PreviewedOutput {
        text: text.to_string(),
        truncated: false,
        tokens: None,
    }
}

pub fn preview_tail_output(text: &str, budget: usize) -> PreviewedOutput {
    if budget == 0 { return preview_output(text, 0); }
    let bpe = o200k_base_singleton();
    let tokens = bpe.encode_ordinary(text);
    if tokens.len() <= budget { return preview_output(text, budget); }
    let mut start = tokens.len() - budget;
    let tail = loop {
        if let Ok(text) = bpe.decode(&tokens[start..]) { break text; }
        start += 1;
    };
    PreviewedOutput { text: tail, truncated: true, tokens: Some(tokens.len()) }
}

fn preview_for_direction(text: &str, budget: usize, direction: ReadDirection) -> PreviewedOutput {
    match direction { ReadDirection::Head => preview_output(text, budget), ReadDirection::Tail => preview_tail_output(text, budget) }
}

fn append_to_capture(buffer: &mut Vec<u8>, data: &[u8], limit: usize, direction: ReadDirection) -> bool {
    if direction == ReadDirection::Head { return append_to_preview_buffer(buffer, data, limit); }
    if data.len() >= limit {
        let mut start = data.len() - limit;
        while start < data.len() && data[start] & 0xc0 == 0x80 { start += 1; }
        let dropped = !buffer.is_empty() || start > 0;
        buffer.clear(); buffer.extend_from_slice(&data[start..]);
        return dropped;
    }
    let mut remove = (buffer.len() + data.len()).saturating_sub(limit);
    while remove < buffer.len() && buffer[remove] & 0xc0 == 0x80 { remove += 1; }
    if remove > 0 { buffer.drain(..remove); }
    buffer.extend_from_slice(data);
    remove > 0
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
        let mut end = remaining;
        while end > 0 && std::str::from_utf8(&data[..end]).is_err() { end -= 1; }
        buffer.extend_from_slice(&data[..end]);
        true
    }
}

/// Bytes of recent output kept for prompt detection.
const INPUT_TAIL_BYTES: usize = 256;
/// Maximum characters kept from a detected prompt line.
const INPUT_HINT_MAX_CHARS: usize = 200;
/// A trailing prompt is only trusted after the command stays silent this long, so a
/// read that merely ends mid-line cannot be mistaken for a blocked command.
const INPUT_QUIET: Duration = Duration::from_millis(400);

fn append_to_input_tail(tail: &mut Vec<u8>, data: &[u8]) {
    tail.extend_from_slice(data);
    if tail.len() > INPUT_TAIL_BYTES {
        let excess = tail.len() - INPUT_TAIL_BYTES;
        tail.drain(..excess);
    }
}

/// Remove ANSI escape sequences (CSI, OSC, and short escapes) from raw terminal output.
fn strip_ansi_escapes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] != 0x1b {
            out.push(input[i]);
            i += 1;
            continue;
        }

        i += 1;
        match input.get(i) {
            Some(b'[') => {
                i += 1;
                while i < input.len() {
                    let byte = input[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            Some(b']') => {
                i += 1;
                while i < input.len() {
                    if input[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if input[i] == 0x1b && input.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            Some(_) => i += 1,
            None => {}
        }
    }
    out
}

/// Detect a trailing interactive prompt in recent output.
///
/// Returns the (ANSI-stripped, trimmed, truncated) prompt line when the output ends
/// on an unterminated line that looks like a question, a choice, or a credential
/// request. Output that ends with a newline is considered settled, not waiting.
pub fn detect_input_prompt(tail: &[u8]) -> Option<String> {
    if tail.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        return None;
    }

    let cleaned = strip_ansi_escapes(tail);
    let line_start = cleaned
        .iter()
        .rposition(|b| *b == b'\n' || *b == b'\r')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    let line = String::from_utf8_lossy(&cleaned[line_start..]);
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    let lower = line.to_ascii_lowercase();
    let looks_like_prompt = lower.ends_with('?')
        || lower.ends_with(':')
        || lower.ends_with('>')
        || lower.contains("password:")
        || lower.contains("passphrase:")
        || lower.contains("[y/n]")
        || lower.contains("(y/n)")
        || lower.contains("[y/n/a]")
        || lower.contains("press any key");
    if !looks_like_prompt {
        return None;
    }

    // Keep the tail of the line, not the head: the 256-byte window can cut the
    // beginning, and the character that triggered the match sits at the end.
    let chars: Vec<char> = line.chars().collect();
    let start = chars.len().saturating_sub(INPUT_HINT_MAX_CHARS);
    Some(chars[start..].iter().collect())
}

/// True when a detected prompt is asking for a secret.
pub fn is_credential_hint(hint: &str) -> bool {
    let lower = hint.to_ascii_lowercase();
    lower.contains("password") || lower.contains("passphrase")
}

/// Caller-facing guidance for a job that stopped on an interactive prompt.
fn waiting_input_note(hint: &str) -> String {
    let mut note = String::from(
        "The command appears to be waiting for terminal input. Answer it with enhanced_terminal_job_stdin \
         (append \\n to submit the line), or terminate it with enhanced_terminal_job_cancel.",
    );
    if is_credential_hint(hint) {
        note.push_str(
            " This looks like a credential prompt: do not send secrets through job_stdin — ask the user \
             to confirm or cancel the job instead.",
        );
    }
    note
}

/// Tracks recent output to decide whether a command is blocked on an interactive prompt.
#[derive(Default)]
struct InputWatcher {
    tail: Vec<u8>,
    pending: Option<(String, Instant)>,
}

impl InputWatcher {
    /// Feed newly received output; any new chunk cancels an unconfirmed prompt.
    fn observe(&mut self, data: &[u8]) {
        append_to_input_tail(&mut self.tail, data);
        self.pending = detect_input_prompt(&self.tail)
            .map(|hint| (hint, Instant::now() + INPUT_QUIET));
    }

    /// The prompt observed earlier, once the command has been quiet long enough.
    fn confirmed(&mut self) -> Option<String> {
        let now = Instant::now();
        match &self.pending {
            Some((_, deadline)) if now >= *deadline => self.pending.take().map(|(hint, _)| hint),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Utf8StreamDecoder { pending: Vec<u8> }

impl Utf8StreamDecoder {
    fn push(&mut self, data: &[u8]) -> String {
        self.pending.extend_from_slice(data);
        let mut output = String::new();
        let mut consumed = 0;
        loop {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(valid) => { output.push_str(valid); consumed = self.pending.len(); break; }
                Err(error) => {
                    let valid_end = consumed + error.valid_up_to();
                    output.push_str(std::str::from_utf8(&self.pending[consumed..valid_end]).unwrap());
                    consumed = valid_end;
                    match error.error_len() {
                        Some(n) => { output.push('\u{fffd}'); consumed += n; }
                        None => break, // Keep an incomplete code point for the next read.
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        output
    }

    fn finish(&mut self) -> String {
        let tail = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        tail
    }
}

fn build_command(command: &str, input: &TerminalExecutionInput) -> CommandBuilder {
    if let Some(program) = &input.program {
        let mut cmd = CommandBuilder::new(program);
        cmd.args(&input.args);
        return cmd;
    }
    let mut cmd = CommandBuilder::new(&input.shell);
    #[cfg(windows)]
    if input.shell == "cmd" { cmd.arg("/C"); }
    else if input.shell == "pwsh" || input.shell == "powershell" {
        cmd.args(["-NoLogo", "-NoProfile", "-Command"]);
    } else { cmd.arg("-c"); }
    #[cfg(not(windows))]
    cmd.arg("-c");
    cmd.arg(command);
    cmd
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
    /// Set when the call ended without completing; e.g. "WaitingForInput".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Prompt line observed when the command is blocked on stdin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_hint: Option<String>,
    /// Guidance for the caller about the reported status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub async fn execute_command(
    input: &TerminalExecutionInput,
    job_manager: &JobManager,
    context: RequestContext<RoleServer>,
) -> Result<ExecutionResult> {
    if context.ct.is_cancelled() {
        anyhow::bail!("Terminal request cancelled before execution");
    }
    let peer = Some(context.peer.clone());
    if input.program.as_deref().is_some_and(|p| p.trim().is_empty()) {
        anyhow::bail!("program cannot be empty");
    }
    if input.program.is_some() && !input.command.trim().is_empty() {
        anyhow::bail!("Supply either command or program, not both");
    }
    if input.program.is_none() && !input.args.is_empty() {
        anyhow::bail!("args requires program");
    }
    #[cfg(windows)]
    if input.program.as_deref().is_some_and(|p| {
        let p = p.to_ascii_lowercase(); p.ends_with(".cmd") || p.ends_with(".bat")
    }) {
        anyhow::bail!("program must be a native executable; use command for .cmd/.bat scripts");
    }
    let display = input.program.as_ref().map(|program| {
        format!("{} {}", program, serde_json::to_string(&input.args).unwrap())
    });
    let command = display.as_deref().unwrap_or(input.command.trim());

    tracing::debug!(
        "execute_command called: command={}, force_sync={}, force_async={}",
        command,
        input.force_sync,
        input.force_async
    );

    // Optional: server-side sudo wrapping (opt-in).
    // When enabled, any command containing `sudo` is executed such that:
    // - sudo authentication is primed in the server process context (askpass)
    // - the actual privileged command runs via `sudo -n` so it never prompts
    //
    // This allows sudo timestamp reuse across multiple tool calls.
    if input.program.is_none() && env_bool("ENHANCED_TERMINAL_SUDO_WRAP") && sudo_looks_used(command) {
        let report = ensure_sudo_primed_for_wrap(command, &input.env_vars).await;
        let wrapped = wrap_sudo_command_for_server(command);
        return execute_command_inner(&wrapped, input, job_manager, report, context).await;
    }

    let sudo_prime_report = if input.program.is_none() {
        maybe_start_sudo_keepalive(command, &input.env_vars).await
    } else { None };

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
            status: None,
            input_hint: None,
            note: None,
        });
    }

    // Resolve working directory
    let cwd = if input.cwd == "." || input.cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(&input.cwd)
    };

    // Canonicalize to resolve symlinks, but on Windows avoid UNC paths
    // which can cause CMD.EXE to fail with STATUS_DLL_INIT_FAILED
    let cwd = cwd.canonicalize().unwrap_or(cwd.clone());
    #[cfg(windows)]
    let cwd = {
        let path_str = cwd.to_string_lossy();
        if path_str.starts_with(r"\\?\") {
            PathBuf::from(&path_str[4..])
        } else {
            cwd
        }
    };

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

    let mut cmd = build_command(command, input);
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
    #[cfg(windows)]
    if let Err(error) = job_manager.attach_windows_job(&job_id, child.as_ref()) {
        let _ = child.kill();
        return Err(error.context("Failed to own PTY process tree"));
    }
    job_manager.register_job_with_tags(
        job_id.clone(),
        command.to_string(),
        if input.program.is_some() { "direct".to_string() } else { input.shell.clone() },
        cwd.display().to_string(),
        pid,
        input.tags.clone(),
    );

    // Read output with smart async switching and keep a writer for future stdin calls.
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("Failed to clone reader: {}", e))?;
    let stdin_writer = pair
        .master
        .take_writer()
        .map_err(|e| anyhow::anyhow!("Failed to open PTY stdin writer: {}", e))?;
    job_manager.attach_stdin_writer(&job_id, stdin_writer);

    let preview_byte_limit = preview_buffer_limit(input);
    let timeout = get_timeout_secs().map(Duration::from_secs);
    let async_threshold = Duration::from_secs(if input.force_sync {
        60
    } else {
        get_async_threshold_secs().min(60)
    });
    let start_time = Instant::now();

    let mut output = Vec::new();
    let mut truncated = false;
    let mut timed_out = false;
    let mut switched_to_async = false;
    // Recent output used to detect prompts that block on stdin.
    let mut input_watcher = InputWatcher::default();
    let mut input_hint: Option<String> = None;

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
        let mut decoder = Utf8StreamDecoder::default();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    tracing::debug!("Reader task: EOF, job_id={}", reader_job_id);
                    let tail = decoder.finish();
                    if !tail.is_empty() { let _ = tx.send(ReadMsg::Data(tail.into_bytes())); }
                    let _ = tx.send(ReadMsg::Eof);
                    break;
                }
                Ok(n) => {
                    let data = decoder.push(&buffer[..n]).into_bytes();
                    if !data.is_empty() && tx.send(ReadMsg::Data(data)).is_err() {
                        break; // Main task dropped receiver
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Reader task error: {:?}, job_id={}", e, reader_job_id);
                    let tail = decoder.finish();
                    if !tail.is_empty() { let _ = tx.send(ReadMsg::Data(tail.into_bytes())); }
                    let _ = tx.send(ReadMsg::Error);
                    break;
                }
            }
        }
    });

    // Main loop: check elapsed time independently and receive output
    let check_interval = Duration::from_millis(100);
    let mut no_output_ticks = 0;
    loop {
        let elapsed = start_time.elapsed();

        if context.ct.is_cancelled() {
            if job_manager.get_job(&job_id).is_some_and(|job| job.status == JobStatus::Running) {
                job_manager.cancel_job(&job_id)?;
            }
            job_manager.complete_job(&job_id, None, JobStatus::Canceled);
            anyhow::bail!("Terminal request cancelled: {job_id}");
        }

        // Check if we should switch to async (independent of I/O)
        if input.force_async || elapsed > async_threshold {
            if input.force_async {
                tracing::debug!("Main task: force_async requested, job_id={}", job_id);
            } else {
                tracing::debug!(
                    "Main task: async threshold reached at {:.2}s, job_id={}",
                    elapsed.as_secs_f64(),
                    job_id
                );
            }
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
                truncated |= append_to_capture(&mut output, &data, preview_byte_limit, input.direction);

                // Update job with incremental output
                let output_str = String::from_utf8_lossy(&data).to_string();
                job_manager.append_output(&job_id, &output_str, preview_byte_limit);

                // A trailing prompt means the command is likely blocked on stdin: surface
                // that explicitly instead of leaving the caller with a silent wait.
                input_watcher.observe(&data);

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
                // Timeout waiting for data
                no_output_ticks += 1;
                // WINDOWS FIX: PTY EOF is unreliable; poll child process directly
                if let Ok(Some(_status)) = child.try_wait() {
                    tracing::debug!(
                        "Main task: child exited without PTY EOF, breaking job_id={}",
                        job_id
                    );
                    break;
                }
                // If no output for 3 seconds (30 ticks) and output buffer is empty,
                // the process likely crashed immediately (e.g., DLL init failure on Windows)
                if output.is_empty() && no_output_ticks > 30 {
                    tracing::warn!(
                        "Main task: no output for 3s, process likely crashed, killing job_id={}",
                        job_id
                    );
                    let _ = child.kill();
                    break;
                }
                // Quiet long enough after a trailing prompt: the command is blocked on stdin.
                // Hand it to the background monitor instead of waiting for the async threshold.
                if let Some(hint) = input_watcher.confirmed() {
                    tracing::info!(
                        "Command waiting for input: job_id={}, elapsed={:.2}s, hint={:?}",
                        job_id,
                        elapsed.as_secs_f64(),
                        hint
                    );
                    job_manager.mark_waiting_input(&job_id, &hint);
                    input_hint = Some(hint);
                    switched_to_async = true;
                    break;
                }
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
        // Reader/writer handles do not own the ConPTY. Keep its master alive
        // until the background monitor completes, including stdin interaction.
        let pty_master = pair.master;

        tokio::spawn(async move {
            let _pty_master = pty_master;
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
                        // New output means the previous prompt was answered (or output
                        // continued); re-evaluate the tail without stopping the job.
                        job_manager_clone.clear_waiting_input(&job_id_clone);
                        input_watcher.observe(&data);
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
                        // WINDOWS FIX: PTY EOF is unreliable; poll child process directly
                        {
                            let mut child_guard = child_arc.lock().await;
                            if let Ok(Some(status)) = child_guard.try_wait() {
                                let exit_code = status.exit_code() as i32;
                                let job_status = if exit_code == 0 {
                                    JobStatus::Completed
                                } else {
                                    JobStatus::Failed
                                };
                                drop(child_guard);
                                tracing::debug!(
                                    "Background task: child exited without PTY EOF, job_id={}, exit_code={}",
                                    job_id_clone,
                                    exit_code
                                );
                                job_manager_clone.complete_job(
                                    &job_id_clone,
                                    Some(exit_code),
                                    job_status,
                                );
                                break;
                            }
                        }
                        if let Some(hint) = input_watcher.confirmed() {
                            job_manager_clone.mark_waiting_input(&job_id_clone, &hint);
                        }
                        // Timeout waiting for data, continue loop to check timeout
                        continue;
                    }
                }
            }
        });

        // Return immediately with a bounded preview and duration so far.
        let raw_output_str = String::from_utf8_lossy(&output).to_string();
        let preview = preview_for_direction(&raw_output_str, input.preview_tokens, input.direction);
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
            status: input_hint.as_ref().map(|_| "WaitingForInput".to_string()),
            note: input_hint.as_deref().map(waiting_input_note),
            input_hint,
        });
    }

    // Synchronous completion
    let exit_status = child.wait().ok();
    let exit_code = exit_status.map(|s| s.exit_code() as i32);
    let success = exit_code.map(|c| c == 0).unwrap_or(false);

    let raw_output_str = String::from_utf8_lossy(&output).to_string();
    let preview = preview_for_direction(&raw_output_str, input.preview_tokens, input.direction);
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
        status: None,
        input_hint: None,
        note: None,
    })
}

async fn execute_command_inner(
    command: &str,
    input: &TerminalExecutionInput,
    job_manager: &JobManager,
    sudo_prime_report: Option<SudoPrimeReport>,
    context: RequestContext<RoleServer>,
) -> Result<ExecutionResult> {
    let command = command.trim();
    let peer = Some(context.peer.clone());

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
            status: None,
            input_hint: None,
            note: None,
        });
    }

    // Resolve working directory
    let cwd = if input.cwd == "." || input.cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(&input.cwd)
    };

    // Canonicalize to resolve symlinks, but on Windows avoid UNC paths
    // which can cause CMD.EXE to fail with STATUS_DLL_INIT_FAILED
    let cwd = cwd.canonicalize().unwrap_or(cwd.clone());
    #[cfg(windows)]
    let cwd = {
        let path_str = cwd.to_string_lossy();
        if path_str.starts_with(r"\\?\") {
            PathBuf::from(&path_str[4..])
        } else {
            cwd
        }
    };

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

    let mut cmd = build_command(command, input);
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
    #[cfg(windows)]
    if let Err(error) = job_manager.attach_windows_job(&job_id, child.as_ref()) {
        let _ = child.kill();
        return Err(error.context("Failed to own PTY process tree"));
    }
    job_manager.register_job_with_tags(
        job_id.clone(),
        command.to_string(),
        if input.program.is_some() { "direct".to_string() } else { input.shell.clone() },
        cwd.display().to_string(),
        pid,
        input.tags.clone(),
    );

    // Read output with smart async switching and keep a writer for future stdin calls.
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("Failed to clone reader: {}", e))?;
    let stdin_writer = pair
        .master
        .take_writer()
        .map_err(|e| anyhow::anyhow!("Failed to open PTY stdin writer: {}", e))?;
    job_manager.attach_stdin_writer(&job_id, stdin_writer);

    let preview_byte_limit = preview_buffer_limit(input);
    let timeout = get_timeout_secs().map(Duration::from_secs);
    let async_threshold = Duration::from_secs(if input.force_sync {
        60
    } else {
        get_async_threshold_secs().min(60)
    });
    let start_time = Instant::now();

    let mut output = Vec::new();
    let mut truncated = false;
    let mut timed_out = false;
    let mut switched_to_async = false;
    // Recent output used to detect prompts that block on stdin.
    let mut input_watcher = InputWatcher::default();
    let mut input_hint: Option<String> = None;

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
        let mut decoder = Utf8StreamDecoder::default();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    tracing::debug!("Reader task: EOF, job_id={}", reader_job_id);
                    let tail = decoder.finish();
                    if !tail.is_empty() { let _ = tx.send(ReadMsg::Data(tail.into_bytes())); }
                    let _ = tx.send(ReadMsg::Eof);
                    break;
                }
                Ok(n) => {
                    let data = decoder.push(&buffer[..n]).into_bytes();
                    if !data.is_empty() && tx.send(ReadMsg::Data(data)).is_err() {
                        break; // Main task dropped receiver
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Reader task error: {:?}, job_id={}", e, reader_job_id);
                    let tail = decoder.finish();
                    if !tail.is_empty() { let _ = tx.send(ReadMsg::Data(tail.into_bytes())); }
                    let _ = tx.send(ReadMsg::Error);
                    break;
                }
            }
        }
    });

    // Main loop: check elapsed time independently and receive output
    let check_interval = Duration::from_millis(100);
    let mut no_output_ticks = 0;
    loop {
        let elapsed = start_time.elapsed();

        if context.ct.is_cancelled() {
            if job_manager.get_job(&job_id).is_some_and(|job| job.status == JobStatus::Running) {
                job_manager.cancel_job(&job_id)?;
            }
            job_manager.complete_job(&job_id, None, JobStatus::Canceled);
            anyhow::bail!("Terminal request cancelled: {job_id}");
        }

        // Check if we should switch to async (independent of I/O)
        if input.force_async || elapsed > async_threshold {
            if input.force_async {
                tracing::debug!("Main task: force_async requested, job_id={}", job_id);
            } else {
                tracing::debug!(
                    "Main task: async threshold reached at {:.2}s, job_id={}",
                    elapsed.as_secs_f64(),
                    job_id
                );
            }
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
                truncated |= append_to_capture(&mut output, &data, preview_byte_limit, input.direction);

                // Update job with incremental output
                let output_str = String::from_utf8_lossy(&data).to_string();
                job_manager.append_output(&job_id, &output_str, preview_byte_limit);

                // A trailing prompt means the command is likely blocked on stdin: surface
                // that explicitly instead of leaving the caller with a silent wait.
                input_watcher.observe(&data);

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
                // Timeout waiting for data
                no_output_ticks += 1;
                // WINDOWS FIX: PTY EOF is unreliable; poll child process directly
                if let Ok(Some(_status)) = child.try_wait() {
                    tracing::debug!(
                        "Main task: child exited without PTY EOF, breaking job_id={}",
                        job_id
                    );
                    break;
                }
                // If no output for 3 seconds (30 ticks) and output buffer is empty,
                // the process likely crashed immediately (e.g., DLL init failure on Windows)
                if output.is_empty() && no_output_ticks > 30 {
                    tracing::warn!(
                        "Main task: no output for 3s, process likely crashed, killing job_id={}",
                        job_id
                    );
                    let _ = child.kill();
                    break;
                }
                // Quiet long enough after a trailing prompt: the command is blocked on stdin.
                // Hand it to the background monitor instead of waiting for the async threshold.
                if let Some(hint) = input_watcher.confirmed() {
                    tracing::info!(
                        "Command waiting for input: job_id={}, elapsed={:.2}s, hint={:?}",
                        job_id,
                        elapsed.as_secs_f64(),
                        hint
                    );
                    job_manager.mark_waiting_input(&job_id, &hint);
                    input_hint = Some(hint);
                    switched_to_async = true;
                    break;
                }
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
        // Reader/writer handles do not own the ConPTY. Keep its master alive
        // until the background monitor completes, including stdin interaction.
        let pty_master = pair.master;

        tokio::spawn(async move {
            let _pty_master = pty_master;
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
                        // New output means the previous prompt was answered (or output
                        // continued); re-evaluate the tail without stopping the job.
                        job_manager_clone.clear_waiting_input(&job_id_clone);
                        input_watcher.observe(&data);
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
                        // WINDOWS FIX: PTY EOF is unreliable; poll child process directly
                        {
                            let mut child_guard = child_arc.lock().await;
                            if let Ok(Some(status)) = child_guard.try_wait() {
                                let exit_code = status.exit_code() as i32;
                                let job_status = if exit_code == 0 {
                                    JobStatus::Completed
                                } else {
                                    JobStatus::Failed
                                };
                                drop(child_guard);
                                tracing::debug!(
                                    "Background task: child exited without PTY EOF, job_id={}, exit_code={}",
                                    job_id_clone,
                                    exit_code
                                );
                                job_manager_clone.complete_job(
                                    &job_id_clone,
                                    Some(exit_code),
                                    job_status,
                                );
                                break;
                            }
                        }
                        if let Some(hint) = input_watcher.confirmed() {
                            job_manager_clone.mark_waiting_input(&job_id_clone, &hint);
                        }
                        // Timeout waiting for data, continue loop to check timeout
                        continue;
                    }
                }
            }
        });

        // Return immediately with a bounded preview and duration so far.
        let raw_output_str = String::from_utf8_lossy(&output).to_string();
        let preview = preview_for_direction(&raw_output_str, input.preview_tokens, input.direction);
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
            status: input_hint.as_ref().map(|_| "WaitingForInput".to_string()),
            note: input_hint.as_deref().map(waiting_input_note),
            input_hint,
        });
    }

    // Synchronous completion
    let exit_status = child.wait().ok();
    let exit_code = exit_status.map(|s| s.exit_code() as i32);
    let success = exit_code.map(|c| c == 0).unwrap_or(false);

    let raw_output_str = String::from_utf8_lossy(&output).to_string();
    let preview = preview_for_direction(&raw_output_str, input.preview_tokens, input.direction);
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
        status: None,
        input_hint: None,
        note: None,
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

#[cfg(test)]
mod stream_tests {
    use super::*;
    #[test]
    fn split_utf8_and_preview_boundary_preserve_chinese() {
        let expected = "中文🙂末尾";
        for size in 1..=7 {
            let mut decoder = Utf8StreamDecoder::default();
            let mut output = String::new();
            for chunk in expected.as_bytes().chunks(size) { output.push_str(&decoder.push(chunk)); }
            output.push_str(&decoder.finish());
            assert_eq!(output, expected);
        }
        let mut buffer = Vec::new();
        assert!(append_to_preview_buffer(&mut buffer, "中文".as_bytes(), 4));
        assert_eq!(String::from_utf8(buffer).unwrap(), "中");
        let source = "中🙂文".repeat(8);
        for budget in 1..=10 {
            let preview = preview_output(&source, budget);
            assert!(source.starts_with(&preview.text));
            assert!(!preview.text.contains('\u{fffd}'));
            assert!(o200k_base_singleton().encode_ordinary(&preview.text).len() <= budget);
        }
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    #[test]
    fn newline_terminated_output_is_not_a_prompt() {
        assert_eq!(detect_input_prompt(b"Done.\n"), None);
        assert_eq!(detect_input_prompt(b"Continue? \r\n"), None);
    }

    #[test]
    fn detects_trailing_prompt_markers() {
        assert_eq!(
            detect_input_prompt(b"Overwrite file? "),
            Some("Overwrite file?".to_string())
        );
        assert_eq!(
            detect_input_prompt(b"Username: "),
            Some("Username:".to_string())
        );
        assert_eq!(
            detect_input_prompt(b"PS C:\\Users\\me> "),
            Some("PS C:\\Users\\me>".to_string())
        );
    }

    #[test]
    fn detects_credential_and_choice_prompts() {
        assert_eq!(
            detect_input_prompt(b"\x1b[1mPassword:\x1b[0m "),
            Some("Password:".to_string())
        );
        assert_eq!(
            detect_input_prompt(b"Enter passphrase: "),
            Some("Enter passphrase:".to_string())
        );
        assert_eq!(
            detect_input_prompt(b"Proceed? [y/n] "),
            Some("Proceed? [y/n]".to_string())
        );
        assert_eq!(
            detect_input_prompt(b"Press any key to continue "),
            Some("Press any key to continue".to_string())
        );
        assert!(is_credential_hint("Password:"));
        assert!(!is_credential_hint("Proceed? [y/n]"));
    }

    #[test]
    fn plain_output_is_not_a_prompt() {
        assert_eq!(detect_input_prompt(b"compiling 12 crates"), None);
        assert_eq!(detect_input_prompt(b""), None);
    }

    #[test]
    fn tail_keeps_only_the_last_bytes() {
        let mut tail = Vec::new();
        append_to_input_tail(&mut tail, &vec![b'a'; 300]);
        append_to_input_tail(&mut tail, b"\nDone? ");
        assert_eq!(tail.len(), INPUT_TAIL_BYTES);
        assert_eq!(detect_input_prompt(&tail), Some("Done?".to_string()));
    }

    #[test]
    fn a_prompt_needs_a_quiet_moment_before_it_counts() {
        let mut watcher = InputWatcher::default();
        watcher.observe(b"Overwrite file? ");
        // A read that merely ends mid-line must not be reported as blocked.
        assert_eq!(watcher.confirmed(), None);
        watcher.observe(b"yes\n");
        assert_eq!(watcher.confirmed(), None);
        watcher.observe(b"Continue? ");
        std::thread::sleep(INPUT_QUIET + Duration::from_millis(50));
        assert_eq!(watcher.confirmed(), Some("Continue?".to_string()));
        assert_eq!(watcher.confirmed(), None);
    }
}
