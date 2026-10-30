use crate::detection::{detect_binaries, detect_shells};
use crate::tools::{ReadDirection, ReadSpec, select_page, is_credential_hint, JobSearchInput, search_job, JobManager, TerminalExecutionInput, default_shell, execute_command};
use chrono::{SecondsFormat, Utc};
use rmcp::{
    ErrorData as McpError, handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters, model::*, service::{RequestContext, RoleServer}, tool, tool_handler,
    tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

#[cfg(unix)]
use nix::fcntl::FlockArg;
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DetectBinariesInput {
    /// Optional filter for specific categories
    #[serde(default)]
    filter_categories: Option<Vec<String>>,
    /// Maximum concurrent checks (default: 16)
    #[serde(default = "default_concurrency")]
    max_concurrency: usize,
    /// Version detection timeout in milliseconds (default: 1500)
    #[serde(default = "default_version_timeout")]
    version_timeout_ms: u64,
    /// Include missing binaries in output (default: false)
    #[serde(default)]
    include_missing: bool,
}

fn default_concurrency() -> usize {
    16
}

fn default_version_timeout() -> u64 {
    1500
}

const ENHANCED_TERMINAL_CALL_LOG_FILE: &str = "enhanced_terminal_calls.jsonl";
static ENHANCED_TERMINAL_CALL_LOG_MUTEX: Mutex<()> = Mutex::new(());

#[derive(Debug, Serialize)]
struct EnhancedTerminalCallLogEntry<'a> {
    datetime: String,
    tool: &'static str,
    parameters: &'a TerminalExecutionInput,
}

fn enhanced_terminal_call_log_path() -> PathBuf {
    std::env::var_os("ENHANCED_TERMINAL_CALL_LOG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(ENHANCED_TERMINAL_CALL_LOG_FILE)
        })
}

/// The call log records every command's parameters and is append-only, so it is
/// capped: past the limit the current file becomes one `.1` generation.
const DEFAULT_CALL_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;

fn call_log_max_bytes() -> u64 {
    std::env::var("ENHANCED_TERMINAL_CALL_LOG_MAX_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CALL_LOG_MAX_BYTES)
}

fn rotate_call_log_if_needed(path: &Path) {
    let max = call_log_max_bytes();
    if max == 0 {
        return;
    }
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() < max {
        return;
    }
    let backup = path.with_extension("jsonl.1");
    let _ = std::fs::remove_file(&backup);
    let _ = std::fs::rename(path, &backup);
}

#[cfg(unix)]
struct CallLogFileLock(RawFd);

#[cfg(unix)]
impl Drop for CallLogFileLock {
    fn drop(&mut self) {
        #[allow(deprecated)]
        let _ = nix::fcntl::flock(self.0, FlockArg::Unlock);
    }
}

#[cfg(unix)]
fn lock_call_log_file(file: &File) -> io::Result<CallLogFileLock> {
    let fd = file.as_raw_fd();
    #[allow(deprecated)]
    nix::fcntl::flock(fd, FlockArg::LockExclusive).map_err(io::Error::other)?;
    Ok(CallLogFileLock(fd))
}

fn write_call_log_line(file: &mut File, line: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    let _file_lock = lock_call_log_file(file)?;

    file.write_all(line)?;
    file.flush()
}

fn log_enhanced_terminal_call(input: &TerminalExecutionInput) -> io::Result<()> {
    let entry = EnhancedTerminalCallLogEntry {
        datetime: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        tool: "enhanced_terminal",
        parameters: input,
    };

    let mut line = serde_json::to_vec(&entry).map_err(io::Error::other)?;
    line.push(0x0A);

    let _process_guard = ENHANCED_TERMINAL_CALL_LOG_MUTEX
        .lock()
        .map_err(|e| io::Error::other(format!("call log mutex poisoned: {e}")))?;

    let log_path = enhanced_terminal_call_log_path();
    rotate_call_log_if_needed(&log_path);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    write_call_log_line(&mut file, &line)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobStatusInput {
    /// Job ID to query
    pub job_id: String,
    /// If true, return only new output since last check (default: true)
    #[serde(default = "default_incremental")]
    pub incremental: bool,
    /// Explicit 0-based stored UTF-8 byte start; even 0 means positioned reading, not incremental.
    #[serde(default)]
    pub offset_bytes: Option<usize>,
    /// Explicit byte count (0 = all remaining); omit for the normal incremental mode.
    #[serde(default)]
    pub limit_bytes: Option<usize>,
    /// Maximum number of GPT-5/o200k_base tokens to return from the selected output chunk.
    /// Defaults to 4096. Set to 0 to disable token truncation.
    #[serde(default = "default_preview_tokens")]
    pub preview_tokens: usize,
    /// If true, include the full command. Defaults to false to keep repeated polling compact.
    #[serde(default)]
    pub full_command: bool,
    /// Explicit head/tail read, without moving the shared incremental cursor.
    #[serde(default)]
    pub direction: Option<ReadDirection>,
    /// 1-based stored LF-delimited line to begin reading; do not mix with byte selectors.
    #[serde(default)]
    pub start_line: Option<usize>,
    /// Positive number of lines, from start_line/head or the end in tail mode.
    #[serde(default)]
    pub line_count: Option<usize>,
    /// Exclusive 0-based byte end for tail continuation; defaults to snapshot end.
    #[serde(default)]
    pub before_byte: Option<usize>,
}

fn default_incremental() -> bool {
    true
}

fn default_preview_tokens() -> usize {
    4096
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobListInput {
    /// Maximum number of jobs to return (default: 50)
    #[serde(default = "default_max_jobs")]
    pub max_jobs: usize,
    /// Filter by job status (e.g., ["Running", "Completed"])
    #[serde(default)]
    pub status_filter: Option<Vec<String>>,
    /// Filter by tag (e.g., "build")
    #[serde(default)]
    pub tag_filter: Option<String>,
    /// Filter by working directory
    #[serde(default)]
    pub cwd_filter: Option<String>,
    /// Sort order: "newest" (default) or "oldest"
    #[serde(default = "default_sort_order")]
    pub sort_order: String,
}

fn default_max_jobs() -> usize {
    50
}

fn default_sort_order() -> String {
    "newest".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobCancelInput {
    /// Job ID to cancel
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobStdinInput {
    /// Job ID whose PTY stdin should receive input
    pub job_id: String,
    /// Exact UTF-8 input to write. Include a trailing newline (\n) to submit a line.
    pub input: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobWaitInput {
    /// Job IDs to wait for
    pub job_ids: Vec<String>,
    /// Literal substring that ends the wait early once it appears in a job's output
    #[serde(default)]
    pub pattern: Option<String>,
    /// Maximum total wait in milliseconds (default 60000; 0 returns an immediate snapshot)
    #[serde(default = "default_wait_timeout_ms")]
    pub timeout_ms: u64,
    /// Poll interval in milliseconds (default 250)
    #[serde(default = "default_wait_poll_ms")]
    pub poll_interval_ms: u64,
    /// Trailing lines returned per job (default 20)
    #[serde(default = "default_wait_tail_lines")]
    pub tail_lines: usize,
    /// Token budget for each job's trailing output (default 1000; 0 disables)
    #[serde(default = "default_wait_preview_tokens")]
    pub preview_tokens: usize,
}

fn default_wait_timeout_ms() -> u64 {
    60_000
}

fn default_wait_poll_ms() -> u64 {
    250
}

fn default_wait_tail_lines() -> usize {
    20
}

fn default_wait_preview_tokens() -> usize {
    1000
}

/// Every tool answers with one JSON document; there is no text format to parse.
fn json_result(value: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(value.to_string())])
}

/// One job's snapshot as reported by `job_wait`: last lines of transcript plus
/// the facts a caller needs to decide whether to keep waiting or react.
fn job_wait_entry(
    job: &crate::tools::job_manager::JobRecord,
    waited: Duration,
    settled: bool,
    matched: bool,
    input: &JobWaitInput,
) -> serde_json::Value {
    let page = select_page(
        &job.full_output,
        &ReadSpec {
            direction: Some(ReadDirection::Tail),
            offset_bytes: None,
            limit_bytes: None,
            before_byte: None,
            start_line: None,
            line_count: Some(input.tail_lines.max(1)),
            cursor: 0,
            preview_tokens: input.preview_tokens,
        },
    )
    .ok();
    serde_json::json!({
        "job_id": job.job_id,
        "status": format!("{:?}", job.status),
        "settled": settled,
        "pattern_matched": matched,
        "exit_code": job.exit_code,
        "duration": job.duration_string(),
        "waited_ms": waited.as_millis() as u64,
        "truncated": page.as_ref().map(|page| page.selection_has_more).unwrap_or(false),
        "output_tail": page.map(|page| page.text).unwrap_or_default(),
    })
}

#[derive(Clone)]
pub struct EnhancedTerminalServer {
    tool_router: ToolRouter<Self>,
    shell_info: String,
    job_manager: JobManager,
    detected_shells: Vec<String>,
}

#[tool_router]
impl EnhancedTerminalServer {
    pub fn new() -> Self {
        // Detect shells at startup
        let shells = detect_shells();
        let detected_shells: Vec<String> = shells.iter().map(|s| s.name.clone()).collect();

        let shell_info = if shells.is_empty() {
            "No shells detected.".to_string()
        } else {
            let mut info = String::from("Available shells:\n");
            for shell in &shells {
                info.push_str(&format!("  - {} ({})", shell.name, shell.path));
                if let Some(ref version) = shell.version {
                    info.push_str(&format!(" - {}", version));
                }
                info.push('\n');
            }
            if let Ok(current_shell) = std::env::var("SHELL") {
                info.push_str(&format!("\nCurrent shell: {}", current_shell));
            }
            info
        };

        Self {
            tool_router: Self::tool_router(),
            shell_info,
            job_manager: JobManager::new(),
            detected_shells,
        }
    }

    #[tool(
        name = "enhanced_terminal",
        description = "Execute a shell command or native program in a PTY and return one JSON object.

PARAMETERS:
- command (string): Shell command; supply either command or program, not both
- program (string): Native executable path/name, launched directly without a shell
- args (string array): Exact program arguments; preserves empty strings and quotes
- cwd (string, default: '.'): Working directory; '.' resolves from the MCP server process working directory set by the caller/client
- shell (string, default: Windows 'pwsh', other platforms 'bash'): Shell to use from available shells (see below)
- preview_tokens (number, default: 4096): o200k_base token budget for the initial output preview; 0 disables truncation
- direction (head or tail, default: head): Which end of the captured preview to return; tail is in normal order
- env_vars (object, default: {}): Environment variables to set; GIT_PAGER and PAGER default to cat unless explicitly supplied
- force_sync (boolean, default: false): Wait up to 60 seconds before returning a background job; the command keeps running
- force_async (boolean, default: false): Start in the background immediately and return a job_id
- custom_denylist (array, default: []): Additional dangerous patterns to block
- tags (array, default: []): Tags for later filtering, e.g. [\"build\", \"ci\"]

AVAILABLE SHELLS:
{shell_list}

BEHAVIOR:
- Commands still running after 50 seconds move to the background and keep running
  (ENHANCED_TERMINAL_ASYNC_THRESHOLD_SECS, capped at 60 seconds including force_sync)
- No timeout by default (ENHANCED_TERMINAL_TIMEOUT_SECS)
- A denied command returns denied=true with denial_reason instead of executing

RETURNS (JSON):
- job_id, command, working_directory: identity of this execution
- switched_to_async: true when the result is a snapshot of a still-running job
- exit_code (null while running), success, timed_out, duration_secs
- output: transcript preview, bounded by preview_tokens
- truncated: true when output continues beyond the preview; read the rest with
  enhanced_terminal_job_status (offset_bytes/limit_bytes) or the tail via enhanced_terminal_job_wait
- preview_direction: head or tail
- status, input_hint: set when the call ended early, e.g. WaitingForInput with the observed prompt
- denied, denial_reason: set when the denylist blocked the command

Calling conventions:
- Poll only through enhanced_terminal_job_wait; do not loop on enhanced_terminal_job_status
- A truncated result does not mean the command failed"
    )]
    async fn enhanced_terminal(
        &self,
        Parameters(input): Parameters<TerminalExecutionInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        log_enhanced_terminal_call(&input).map_err(|e| {
            McpError::internal_error(format!("Failed to log enhanced_terminal call: {}", e), None)
        })?;

        if input.force_sync && input.force_async {
            return Err(McpError::invalid_params(
                "force_sync and force_async cannot both be true",
                None,
            ));
        }

        // Validate shell against detected shells
        if input.program.is_none() && !self.detected_shells.is_empty() && !self.detected_shells.contains(&input.shell) {
            return Err(McpError::invalid_params(
                format!(
                    "Shell '{}' not found. Available shells: {}",
                    input.shell,
                    self.detected_shells.join(", ")
                ),
                None,
            ));
        }

        let result = execute_command(&input, &self.job_manager, context)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Command execution failed: {}", e), None)
            })?;

        let mut payload = serde_json::to_value(&result).map_err(|error| {
            McpError::internal_error(format!("Failed to serialize result: {error}"), None)
        })?;
        payload["preview_direction"] = serde_json::json!(input.direction.name());
        Ok(json_result(payload))
    }

    #[tool(
        name = "enhanced_terminal_job_status",
        description = "Read one job's stored raw UTF-8 PTY transcript and return one JSON object.

PARAMETERS:
- job_id (string, required): job identifier
- incremental (boolean, default true): forward read from the shared cursor; explicit selectors below never move it
- offset_bytes / limit_bytes (0-based bytes, exclusive end): positioned byte read; limit 0 means to the end
- direction (head or tail) with start_line / line_count (1-based LF lines): positioned line read
- before_byte: exclusive 0-based end for tail continuation
- preview_tokens (number, default 4096): o200k_base token budget for the returned slice; 0 disables
- full_command (boolean, default false): include the full command text in the reply

BEHAVIOR:
- Do not mix line and byte positioning
- A large line is split by the token budget; read.partial_line and read.continue locate the next segment
- CRLF counts as one line break; terminal cursor movement is not interpreted
- The transcript is raw PTY data including ANSI/repaint sequences, not clean stdout

RETURNS (JSON):
- job_id, summary, status, duration, shell, cwd, exit_code, pid, tags
- command: only when full_command is true
- input_hint and credential_prompt: set when the job is blocked on stdin
- read: mode (incremental|explicit|full), direction, total_bytes, total_lines,
  byte_start/byte_end, line_start/line_end, partial_line, partial_start/partial_end,
  selection_start_byte/selection_end_byte, has_more_before, has_more_after,
  truncated, tokens, token_budget, next_offset_bytes, continue
- output: the selected transcript slice

To follow a running job, prefer enhanced_terminal_job_wait over repeated status calls."
    )]
    async fn job_status(
        &self,
        Parameters(input): Parameters<JobStatusInput>,
    ) -> Result<CallToolResult, McpError> {
        let job = self.job_manager.get_job(&input.job_id)
            .ok_or_else(|| McpError::invalid_params("Job not found", None))?;
        let page = select_page(&job.full_output, &ReadSpec {
            direction: input.direction, offset_bytes: input.offset_bytes, limit_bytes: input.limit_bytes,
            before_byte: input.before_byte, start_line: input.start_line, line_count: input.line_count,
            cursor: if input.incremental { job.last_read_position } else { 0 }, preview_tokens: input.preview_tokens,
        }).map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        if input.incremental && !page.explicit { self.job_manager.set_read_position(&input.job_id, page.end); }
        let tail = page.direction == ReadDirection::Tail;
        let continuation = if page.selection_has_more {
            Some(if tail { serde_json::json!({ "job_id": job.job_id, "direction": "tail", "before_byte": page.start,
                "limit_bytes": page.start - page.selection_start, "preview_tokens": input.preview_tokens }) }
            else { serde_json::json!({ "job_id": job.job_id, "offset_bytes": page.end,
                "limit_bytes": page.selection_end - page.end, "preview_tokens": input.preview_tokens }) })
        } else { None };

        let mode = if page.explicit {
            "explicit"
        } else if input.incremental {
            "incremental"
        } else {
            "full"
        };
        let credential_prompt = job
            .input_hint
            .as_deref()
            .is_some_and(is_credential_hint);
        let payload = serde_json::json!({
            "job_id": job.job_id,
            "summary": job.summary,
            "status": format!("{:?}", job.status),
            "duration": job.duration_string(),
            "shell": job.shell,
            "cwd": job.cwd,
            "command": if input.full_command { Some(job.command.clone()) } else { None },
            "exit_code": job.exit_code,
            "pid": job.pid,
            "tags": job.tags.clone(),
            "input_hint": job.input_hint.clone(),
            "credential_prompt": credential_prompt,
            "read": {
                "mode": mode,
                "direction": page.direction.name(),
                "total_bytes": job.full_output.len(),
                "total_lines": page.total_lines,
                "byte_start": page.start,
                "byte_end": page.end,
                "line_start": page.first_line,
                "line_end": page.last_line,
                "partial_line": page.partial_start || page.partial_end,
                "partial_start": page.partial_start,
                "partial_end": page.partial_end,
                "selection_start_byte": page.selection_start,
                "selection_end_byte": page.selection_end,
                "has_more_before": tail && page.start > 0,
                "has_more_after": !tail && page.end < job.full_output.len(),
                "truncated": page.selection_has_more,
                "tokens": page.tokens,
                "token_budget": input.preview_tokens,
                "next_offset_bytes": if page.end < job.full_output.len() { Some(page.end) } else { None },
                "continue": continuation,
            },
            "output": page.text,
        });
        Ok(json_result(payload))
    }

    #[tool(
        name = "enhanced_terminal_job_wait",
        description = "Wait for jobs instead of polling them, and return one JSON object.

PARAMETERS:
- job_ids (array, required): jobs to wait for
- pattern (string, optional): literal substring that ends that job's wait once it appears in its transcript
- timeout_ms (number, default 60000): maximum total wait; 0 returns a snapshot immediately
- poll_interval_ms (number, default 250): internal poll interval
- tail_lines (number, default 20): trailing transcript lines reported per job
- preview_tokens (number, default 1000): o200k_base token budget per job's tail; 0 disables it

BEHAVIOR:
- A job settles when it leaves Running/WaitingForInput (Completed, Failed, TimedOut, Canceled)
- Returns as soon as every job settled or matched the pattern, or the timeout expires
- A matching pattern ends that job's wait even though the process may still be running; settled reports which happened
- Reading here never moves the cursor used by enhanced_terminal_job_status
- Unknown job ids are reported per entry instead of failing the whole call

RETURNS (JSON): { waited_ms, timed_out, jobs: [{ job_id, status, settled, pattern_matched, exit_code, duration, waited_ms, truncated, output_tail }] }"
    )]
    async fn job_wait(
        &self,
        Parameters(input): Parameters<JobWaitInput>,
    ) -> Result<CallToolResult, McpError> {
        use crate::tools::JobStatus;

        if input.job_ids.is_empty() {
            return Err(McpError::invalid_params(
                "job_ids must contain at least one job id",
                None,
            ));
        }

        let pattern = input
            .pattern
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let started = Instant::now();
        let deadline = started.checked_add(Duration::from_millis(input.timeout_ms));
        let poll = Duration::from_millis(input.poll_interval_ms.max(25));

        let mut pending = input.job_ids.clone();
        let mut entries: Vec<serde_json::Value> = Vec::new();

        loop {
            let mut still_running = Vec::new();
            for job_id in pending.drain(..) {
                let Some(job) = self.job_manager.get_job(&job_id) else {
                    entries.push(serde_json::json!({ "job_id": job_id, "error": "job not found" }));
                    continue;
                };
                let matched = pattern.is_some_and(|value| job.full_output.contains(value));
                let settled = !matches!(job.status, JobStatus::Running | JobStatus::WaitingForInput);
                if settled || matched {
                    entries.push(job_wait_entry(&job, started.elapsed(), settled, matched, &input));
                } else {
                    still_running.push(job_id);
                }
            }
            pending = still_running;
            if pending.is_empty() {
                break;
            }
            let Some(deadline) = deadline else {
                break;
            };
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            tokio::time::sleep(poll.min(deadline - now)).await;
        }

        let timed_out = !pending.is_empty();
        for job_id in pending {
            match self.job_manager.get_job(&job_id) {
                Some(job) => entries.push(job_wait_entry(
                    &job,
                    started.elapsed(),
                    false,
                    false,
                    &input,
                )),
                None => entries.push(serde_json::json!({ "job_id": job_id, "error": "job not found" })),
            }
        }

        Ok(json_result(serde_json::json!({
            "waited_ms": started.elapsed().as_millis() as u64,
            "timed_out": timed_out,
            "jobs": entries,
        })))
    }

    #[tool(
        name = "enhanced_terminal_job_search",
        description = "Search only one job's complete stored UTF-8 PTY transcript. mode=literal uses rg fixed strings (default); regex uses rg; semantic uses the existing local sgrep text model offline, without a persistent index. No unrelated files are searched. Results include raw transcript byte offsets, line ranges, explicitly bounded context and concrete job_status readback arguments. ANSI and repaint sequences are preserved: this is a raw transcript view, not clean stdout. Search does not move the incremental cursor or change the original 4096-token preview default."
    )]
    async fn job_search(
        &self,
        Parameters(input): Parameters<JobSearchInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = search_job(&self.job_manager, &input).await
            .map_err(|error| McpError::internal_error(format!("Job search failed: {error:#}"), None))?;
        let text = serde_json::to_string(&result)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        name = "enhanced_terminal_job_list",
        description = "List tracked jobs as a JSON array, newest first by default.

PARAMETERS:
- max_jobs (number, default 50): maximum entries returned
- status_filter (array): any of Running, WaitingForInput, Completed, Failed, TimedOut, Canceled
- tag_filter (string): only jobs carrying this tag
- cwd_filter (string): only jobs from this working directory
- sort_order (string, default \"newest\"): \"newest\" or \"oldest\"

BEHAVIOR:
- Filters combine with AND; an empty result is the empty array
- output_preview is the first 100 characters of the transcript, not the full output

RETURNS (JSON array): [{ job_id, summary, status, exit_code, duration, cwd, shell, tags, output_preview }]"
    )]
    async fn job_list(
        &self,
        Parameters(input): Parameters<JobListInput>,
    ) -> Result<CallToolResult, McpError> {
        use crate::tools::JobStatus;

        // Parse status filter if provided
        let status_filter: Option<Vec<JobStatus>> = input.status_filter.as_ref().map(|filters| {
            filters
                .iter()
                .filter_map(|s| match s.as_str() {
                    "Running" => Some(JobStatus::Running),
                    "WaitingForInput" => Some(JobStatus::WaitingForInput),
                    "Completed" => Some(JobStatus::Completed),
                    "Failed" => Some(JobStatus::Failed),
                    "TimedOut" => Some(JobStatus::TimedOut),
                    "Canceled" => Some(JobStatus::Canceled),
                    _ => None,
                })
                .collect()
        });

        // Get filtered jobs
        let mut jobs = if input.status_filter.is_some()
            || input.tag_filter.is_some()
            || input.cwd_filter.is_some()
        {
            self.job_manager.list_jobs_filtered(
                status_filter.as_deref(),
                input.tag_filter.as_deref(),
                input.cwd_filter.as_deref(),
            )
        } else {
            self.job_manager.list_jobs()
        };

        // Apply sort order
        if input.sort_order == "oldest" {
            jobs.reverse();
        }

        let jobs_to_show = jobs.into_iter().take(input.max_jobs).collect::<Vec<_>>();

        let listed: Vec<serde_json::Value> = jobs_to_show
            .iter()
            .map(|job| {
                serde_json::json!({
                    "job_id": job.job_id,
                    "summary": job.summary,
                    "status": format!("{:?}", job.status),
                    "exit_code": job.exit_code,
                    "duration": job.duration_string(),
                    "cwd": job.cwd,
                    "shell": job.shell,
                    "tags": job.tags,
                    "output_preview": job.output.chars().take(100).collect::<String>(),
                })
            })
            .collect();

        Ok(json_result(serde_json::Value::Array(listed)))
    }

    #[tool(
        name = "enhanced_terminal_job_cancel",
        description = "Cancel a running job: terminate its owned process tree on Windows; send SIGTERM on Unix.

PARAMETERS:
- job_id (string, required): job identifier to cancel

BEHAVIOR:
- Windows terminates the job-owned process group and confirms exit before reporting
- Unix sends SIGTERM to the tracked process
- The job status becomes Canceled

RETURNS (JSON): { job_id, canceled: true }; verify with enhanced_terminal_job_status or enhanced_terminal_job_wait"
    )]
    async fn job_cancel(
        &self,
        Parameters(input): Parameters<JobCancelInput>,
    ) -> Result<CallToolResult, McpError> {
        self.job_manager
            .cancel_job(&input.job_id)
            .map_err(|e| McpError::internal_error(format!("Failed to cancel job: {}", e), None))?;

        Ok(json_result(serde_json::json!({
            "job_id": input.job_id,
            "canceled": true,
        })))
    }

    #[tool(
        name = "enhanced_terminal_job_stdin",
        description = "Write text to a running job's PTY stdin.

PARAMETERS:
- job_id (string, required): job identifier returned by enhanced_terminal
- input (string, required): exact UTF-8 text; add \n yourself to submit a line

BEHAVIOR:
- Writes to the PTY stdin of a Running or WaitingForInput job
- Never appends a newline
- Refuses to answer a credential prompt (password/passphrase); ask the user to confirm or cancel instead

RETURNS (JSON): { job_id, bytes_written }"
    )]
    async fn job_stdin(
        &self,
        Parameters(input): Parameters<JobStdinInput>,
    ) -> Result<CallToolResult, McpError> {
        // Credential prompts must not be answered by the model itself.
        if let Some(job) = self.job_manager.get_job(&input.job_id) {
            if matches!(job.status, crate::tools::JobStatus::WaitingForInput) {
                if let Some(hint) = job.input_hint.as_deref() {
                    if is_credential_hint(hint) {
                        return Err(McpError::invalid_params(
                            format!(
                                "Job {} is waiting on a credential prompt ({:?}). Do not send secrets \
                                 through job_stdin — ask the user to confirm or cancel the job.",
                                input.job_id, hint
                            ),
                            None::<serde_json::Value>,
                        ));
                    }
                }
            }
        }

        let bytes_written = self
            .job_manager
            .write_stdin(&input.job_id, &input.input)
            .map_err(|e| {
                McpError::invalid_params(
                    format!("Failed to write to job stdin: {}", e),
                    None::<serde_json::Value>,
                )
            })?;

        Ok(json_result(serde_json::json!({
            "job_id": input.job_id,
            "bytes_written": bytes_written,
        })))
    }

    #[tool(
        name = "detect_binaries",
        description = "Detect developer tools and their versions with fast parallel scanning.

PARAMETERS:
- filter_categories (array, optional): Category names to scan (e.g. [\"rust_tools\", \"python_tools\"])
- max_concurrency (number, default: 16): Number of concurrent version checks
- version_timeout_ms (number, default: 1500): Timeout per binary version check in milliseconds
- include_missing (boolean, default: false): Include binaries not found in PATH

CATEGORIES (190+ tools):
- package_managers: npm, pip, cargo, dnf, apt, snap, flatpak, brew, pnpm, uv, poetry, pipx
- rust_tools: cargo, rustc, rustfmt, clippy
- python_tools: python, python3, pip, pytest, black, ruff, mypy, uv, poetry, pipenv, pipx, pyright, pylint, flake8, isort, ipython
- build_systems: make, cmake, ninja, gradle, maven, mvn
- c_cpp_tools: gcc, g++, clang, gdb, lldb
- java_jvm_tools: java, javac, javadoc, jar, jarsigner, jconsole, jdeps, jlink, jshell, kotlin, kotlinc, scala, scalac, groovy, groovyc
- maven_tools: mvn, mvnw, mvnd
- node_js_tools: node, deno, bun, npm, yarn, pnpm, tsx, tsc, biome, prettier, eslint
- go_tools: go, gofmt
- editors_dev: vim, nvim, emacs, code, zed, hx, nano, micro
- search_productivity: rg, fd, fzf, jq, bat, tree, exa, sd, zoxide, lsd, dust, btm, broot, choose
- system_perf: htop, ps, top, df, du
- containers: docker, podman, kubectl, helm, docker-compose, kind, minikube, skopeo, buildah, nerdctl, k9s
- networking: curl, wget, dig, traceroute, http, nc, nmap, ss, ping, mtr, socat
- security: openssl, gpg, ssh-keygen, age, sops, vault, pass
- auth_helpers: zenity, ssh-askpass, sshaskpass, ksshaskpass, pinentry variants
- databases: sqlite3, psql, mysql, redis-cli, mongosh, duckdb, clickhouse-client, redis-server
- vcs: git, gh, lazygit, tig, gitui, hg, svn
- cloud_cli: aws, gcloud, az, doctl, fly, vercel, wrangler
- iac_tools: terraform, tofu, pulumi, ansible, ansible-playbook, vagrant, packer
- media_tools: ffmpeg, ffprobe, convert, magick, exiftool, yt-dlp, sox
- ai_ml_tools: ollama, huggingface-cli, nvidia-smi, nvcc, rocm-smi, dvc, mlflow
- docs_tools: pandoc, sphinx-build, mkdocs, doxygen, asciidoctor, mdbook
- ruby_tools: ruby, gem, bundle, rake, irb, rails
- dotnet_tools: dotnet, nuget, msbuild
- cad_utils: ODAFileConverter, dwg2svg, dwg2SVG, dwg2bmp, dwg2pdf, qcad, librecad, freecad, freecadcmd, openscad, dxf2gcode

PERFORMANCE:
- 16 concurrent checks by default
- ~7-10 seconds for all 26 categories (dominated by 1500ms version-probe timeout for uninstalled tools)
- ~300-1500ms per individual category
- Configurable timeout per binary
- Efficient PATH scanning

RETURNS (JSON): { binaries: [{ name, category, found, path, version, error }] }"
    )]
    async fn detect_binaries(
        &self,
        Parameters(input): Parameters<DetectBinariesInput>,
    ) -> Result<CallToolResult, McpError> {
        let reports = detect_binaries(
            input.filter_categories,
            input.max_concurrency,
            input.version_timeout_ms,
            input.include_missing,
        );

        let json_output = serde_json::json!({
            "binaries": reports
        });

        let json_text = serde_json::to_string(&json_output)
            .map_err(|e| McpError::internal_error(format!("Serialization error: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(json_text)]))
    }
}

#[tool_handler(router = self.tool_router)]
impl rmcp::ServerHandler for EnhancedTerminalServer {
    fn get_info(&self) -> ServerInfo {
        let instructions = format!(
            "Prefer enhanced_terminal for ordinary commands, builds, background jobs and interactive tasks. \
             Fall back to OpenCode native shell only when enhanced_terminal is unavailable, lacks a required capability, or has a confirmed compatibility issue; state the reason.\n\
             Supply either shell command or native program with an args array; no shell translation. \
             GIT_PAGER and PAGER default to cat; explicit env_vars override them. Foreground calls return a job ID within 60 seconds even with force_sync — except that a detected input prompt always returns early — and the command keeps running in the background.\n\
             Every tool answers with one JSON document: read fields instead of parsing text. \
             enhanced_terminal sets switched_to_async=true when the reply snapshots a still-running job, and truncated=true when output extends past preview_tokens.\n\
             Follow a running job with enhanced_terminal_job_wait; a literal pattern ends the wait early and settled reports whether the process actually exited. Do not poll job_status in a loop.\n\
             For stdin, start force_async=true then call enhanced_terminal_job_stdin; include a newline to submit. A job blocked on stdin reports status WaitingForInput with input_hint.\n\
             Default shell: {}. Previews default to 4096 o200k tokens.\n\
             job_status reads the stored raw transcript: incremental forward reads by default, or explicit head/tail with start_line/line_count (1-based LF lines), or offset_bytes/limit_bytes (0-based bytes, exclusive end) with preview_tokens=0 for exact pages. \
             The read object echoes the byte and line ranges, partial_line flags and a continue argument for the next segment. The full command appears only with job_status full_command=true.\n\
             job_search finds literal/regex (rg) or semantic (local sgrep) matches inside one transcript and returns byte readback references.\n\
             Windows cancellation forcefully terminates the task-owned job; Unix uses SIGTERM. \
             Output is a raw PTY transcript (ANSI/repaints possible), retained in memory until this server exits.\n\
             {}",
            default_shell(), self.shell_info,
        );

        ServerInfo {
            instructions: Some(instructions),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
