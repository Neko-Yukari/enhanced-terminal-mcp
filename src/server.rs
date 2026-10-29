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
    path::PathBuf,
    sync::Mutex,
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
        description = "Execute shell commands in a PTY with smart async switching and security.

PARAMETERS:
- command (string): Shell command; supply either command or program, not both\n- program (string): Native executable path/name, launched directly without a shell\n- args (string array): Exact program arguments; preserves empty strings and quotes
- cwd (string, default: '.'): Working directory for command execution; when omitted, '.' is resolved from the MCP server process working directory set by the caller/client
- shell (string, default: Windows 'pwsh', other platforms 'bash'): Shell to use from available shells (see below)
- preview_tokens (number, default: 4096): Maximum GPT-5/o200k_base tokens in the initial preview; 0 disables token truncation\n- direction (head or tail, default: head): Which end of the captured preview to show; tail is in normal order
- env_vars (object, default: {}): Environment variables to set; GIT_PAGER and PAGER default to cat unless explicitly supplied
- force_sync (boolean, default: false): Wait up to 60 seconds before returning a background job; commands keep running
- force_async (boolean, default: false): Force immediate background execution and return a job_id without waiting for the async threshold
- custom_denylist (array, default: []): Additional dangerous patterns to block
- tags (array, default: []): Optional tags for categorizing jobs (e.g., [\"build\", \"ci\"])

AVAILABLE SHELLS:
{shell_list}

BEHAVIOR:
- Commands running longer than 50 seconds automatically switch to background (keeps running)
  (configurable via ENHANCED_TERMINAL_ASYNC_THRESHOLD_SECS, capped at 60 seconds including force_sync)
- Set force_async=true to immediately return a job_id for interactive commands that need stdin
- No timeout by default - commands run until completion
  (configurable via ENHANCED_TERMINAL_TIMEOUT_SECS environment variable)
- Returns a readable adjective-noun-number job_id for tracking via enhanced_terminal_job_status
- Security denylist blocks dangerous commands (rm -rf /, shutdown, fork bombs, etc.)
- PTY support preserves colors and terminal features
- Incremental output captured during background execution

SECURITY:
- 40+ dangerous patterns blocked by default
- Custom patterns via custom_denylist parameter
- No privilege escalation without explicit configuration
- Token previews and bounded preview buffers prevent oversized MCP responses
- Optional timeout protection via ENHANCED_TERMINAL_TIMEOUT_SECS environment variable

RETURNS:
- job_id: Unique readable adjective-noun-number identifier for this command execution
- command: The executed command
- working_directory: Resolved working directory path
- exit_code: Exit code (if completed, null if still running)
- success: Boolean indicating success (if completed)
- output: Command output preview (truncated to preview_tokens by default)
- truncated: Boolean indicating if output was truncated
- timed_out: Boolean indicating if command was killed by timeout
- switched_to_async: Boolean indicating if command moved to background
- status: Set when the call ended early without completing, e.g. WaitingForInput
- input_hint: Prompt line observed when the command is blocked on stdin
- note: Guidance for the reported status
- denied: Boolean indicating if command was blocked
- denial_reason: Reason for denial (if denied)"
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

        if result.denied {
            let mut result_text = format!("Command: {}\n", result.command);
            result_text.push_str(&format!(
                "Status: DENIED\n\nReason: {}\n",
                result
                    .denial_reason
                    .unwrap_or_else(|| "Security policy violation".to_string())
            ));
            return Ok(CallToolResult::success(vec![Content::text(result_text)]));
        }

        let mut result_text = format!("Job ID: {}\n", result.job_id);
        result_text.push_str(&format!("Command: {}\n", result.command));
        result_text.push_str(&format!("Truncated: {}\nPreview Direction: {}\n", result.truncated, input.direction.name()));
        result_text.push_str(&format!(
            "Working Directory: {}\n",
            result.working_directory
        ));

        if result.switched_to_async {
            if let Some(duration) = result.duration_secs {
                result_text.push_str(&format!(
                    "Duration: {:.2}s (switched to background)\n",
                    duration
                ));
            }
            match result.status.as_deref() {
                Some(status) => result_text.push_str(&format!("Status: {}\n", status)),
                None => result_text.push_str("Status: SWITCHED TO BACKGROUND\n"),
            }
            if let Some(hint) = result.input_hint.as_deref() {
                result_text.push_str(&format!("Input Hint: {}\n", hint));
            }
            if let Some(note) = result.note.as_deref() {
                result_text.push_str(&format!("Note: {}\n", note));
            }
            result_text
                .push_str("The command is still running. Use enhanced_terminal_job_status to check progress.\n");
            result_text.push_str("\nPartial Output:\n");
            result_text.push_str(&result.output);
            if result.truncated {
                result_text
                    .push_str("\n\n[Output preview truncated - use enhanced_terminal_job_status to get full output]");
            }
        } else {
            // Show duration first for completed/timed out commands
            if let Some(duration) = result.duration_secs {
                result_text.push_str(&format!("Duration: {:.2}s\n", duration));
            }

            match result.exit_code {
                Some(exit_code) => result_text.push_str(&format!("Exit Code: {}\n", exit_code)),
                None => result_text.push_str("Exit Code: null\n"),
            }
            result_text.push_str(&format!("Success: {}\n", result.success));

            if result.timed_out {
                result_text.push_str("Status: TIMED OUT ⏱️\n");
            } else if result.success {
                result_text.push_str("Status: COMPLETED ✅\n");
            } else {
                result_text.push_str("Status: FAILED ❌\n");
            }

            result_text.push_str("\nOutput:\n");
            result_text.push_str(&result.output);

            if result.truncated {
                result_text.push_str("\n\n[Output truncated due to preview token limit]");
            }
        }

        if result.truncated {
            result_text.push_str(&format!(
                "\nRead complete output in pages: enhanced_terminal_job_status({{\"job_id\":\"{}\",\"offset_bytes\":0,\"limit_bytes\":4096,\"preview_tokens\":0}})\n",
                result.job_id,
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(result_text)]))
    }

    #[tool(
        name = "enhanced_terminal_job_status",
        description = "Read the stored raw UTF-8 PTY transcript, preserving ANSI/repaint sequences. Default is incremental forward reading with the original 4096-token budget. Explicit direction=head/tail, start_line/line_count (1-based LF lines), or offset_bytes/limit_bytes (0-based bytes, exclusive end) never move that cursor. Tail displays text in normal forward order; before_byte anchors backward continuation. Do not mix line and byte positioning. Large lines are split by the requested preview budget: returned raw byte/line ranges, partial_line flags and Continue Selection arguments locate every segment. preview_tokens=0 disables the token limit; explicit byte limits still apply. CRLF counts as one line break; terminal cursor movement is not interpreted. A command blocked on stdin reports Status: WaitingForInput with the observed prompt as Input Hint."
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
        let has_more = if tail { page.start > 0 } else { page.end < job.full_output.len() };
        let continuation = if page.selection_has_more {
            Some(if tail { serde_json::json!({ "job_id": job.job_id, "direction": "tail", "before_byte": page.start,
                "limit_bytes": page.start - page.selection_start, "preview_tokens": input.preview_tokens }) }
            else { serde_json::json!({ "job_id": job.job_id, "offset_bytes": page.end,
                "limit_bytes": page.selection_end - page.end, "preview_tokens": input.preview_tokens }) })
        } else { None };

        let mut result_text = format!(
            "Job ID: {}\nSummary: {}\nShell: {}\nWorking Directory: {}\nStatus: {:?}\nDuration: {}\n",
            job.job_id, job.summary, job.shell, job.cwd, job.status, job.duration_string(),
        );
        if let Some(hint) = job.input_hint.as_deref() {
            result_text.push_str(&format!("Input Hint: {}\n", hint));
            if is_credential_hint(hint) {
                result_text.push_str(
                    "Note: This looks like a credential prompt. Do not send secrets through \
                     enhanced_terminal_job_stdin — ask the user to confirm or cancel the job.\n",
                );
            }
        }
        if input.full_command { result_text.push_str(&format!("Command: {}\n", job.command)); }
        if let Some(code) = job.exit_code { result_text.push_str(&format!("Exit Code: {}\n", code)); }
        if let Some(pid) = job.pid { result_text.push_str(&format!("PID: {}\n", pid)); }
        if !job.tags.is_empty() { result_text.push_str(&format!("Tags: {}\n", job.tags.join(", "))); }
        result_text.push_str(&format!(
            "Output Mode: {}\nDirection: {}\nTotal Output Length: {} bytes\nTotal Lines: {}\nReturned Byte Range: {}..{}\nHas More: {}\nTruncated: {}\n",
            if page.explicit { "Explicit" } else if input.incremental { "Incremental" } else { "Full" },
            page.direction.name(), job.full_output.len(), page.total_lines, page.start, page.end, has_more, page.selection_has_more,
        ));
        let line_range = page.first_line.zip(page.last_line).map(|(first,last)| format!("{first}..{last}")).unwrap_or_else(|| "none".to_string());
        result_text.push_str(&format!("Returned Line Range: {}\nPartial Line: {}\n", line_range, page.partial_start || page.partial_end));
        let metadata = serde_json::json!({ "view": "stored_utf8_raw_pty_transcript", "byte_base": 0, "byte_end_exclusive": true,
            "line_base": 1, "line_delimiter": "LF (CRLF is one break)", "line_start": page.first_line, "line_end": page.last_line,
            "partial_line": page.partial_start || page.partial_end, "partial_start": page.partial_start, "partial_end": page.partial_end,
            "selection_start_byte": page.selection_start, "selection_end_byte": page.selection_end,
            "selection_has_more": page.selection_has_more, "continue": continuation });
        result_text.push_str(&format!("Read Metadata: {}\n", metadata));
        if input.preview_tokens > 0 { result_text.push_str(&format!("Preview Tokens: {} / {} (o200k_base)\n", page.tokens.unwrap_or(0), input.preview_tokens)); }
        if page.end < job.full_output.len() { result_text.push_str(&format!("Next Offset Bytes: {}\n", page.end)); }
        if tail && page.start > 0 { result_text.push_str(&format!("Previous Before Byte: {}\n", page.start)); }
        if let Some(args) = continuation { result_text.push_str(&format!("Continue Selection: {}\n", args)); }
        result_text.push_str("\nOutput:\n");
        result_text.push_str(&page.text);
        // Coordinates and continuation live in the header, never inside raw program output.
        Ok(CallToolResult::success(vec![Content::text(result_text)]))
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
        description = "List all background jobs with status and output previews.

PARAMETERS:
- max_jobs (number, default: 50): Maximum number of jobs to return
- status_filter (array, optional): Filter by status (e.g., [\"Running\", \"Completed\"])
- tag_filter (string, optional): Filter by tag (e.g., \"build\")
- cwd_filter (string, optional): Filter by working directory
- sort_order (string, default: \"newest\"): Sort order (\"newest\" or \"oldest\")

BEHAVIOR:
- Jobs sorted by start time (newest first by default)
- Shows running and completed jobs
- Output preview limited to first 100 characters
- Includes duration, exit codes, tags, and summary

FILTERING:
- status_filter: Match any of the provided statuses
  - Valid values: \"Running\", \"WaitingForInput\", \"Completed\", \"Failed\", \"TimedOut\", \"Canceled\"
- tag_filter: Show only jobs with the specified tag
- cwd_filter: Show only jobs from a specific directory
- Filters are combined with AND logic

RETURNS: List of jobs with:
- job_id: Unique readable adjective-noun-number identifier
- command: Full executed command
- summary: First 100 characters of command
- status: Current status (Running, WaitingForInput, Completed, Failed, TimedOut, Canceled)
- exit_code: Exit code if completed
- duration: Time elapsed since start
- tags: Optional tags assigned to this job
- cwd: Working directory
- shell: Shell used
- output_preview: First 100 characters of output"
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

        if jobs_to_show.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No jobs found.".to_string(),
            )]));
        }

        let mut result_text = format!("Found {} job(s):\n\n", jobs_to_show.len());

        for job in jobs_to_show {
            result_text.push_str(&format!("Job ID: {}\n", job.job_id));
            result_text.push_str(&format!("  Summary: {}\n", job.summary));
            result_text.push_str(&format!("  Status: {:?}\n", job.status));
            result_text.push_str(&format!("  CWD: {}\n", job.cwd));
            result_text.push_str(&format!("  Shell: {}\n", job.shell));

            if !job.tags.is_empty() {
                result_text.push_str(&format!("  Tags: {}\n", job.tags.join(", ")));
            }

            if let Some(exit_code) = job.exit_code {
                result_text.push_str(&format!("  Exit Code: {}\n", exit_code));
            }

            // Use the duration helper method
            result_text.push_str(&format!("  Duration: {}\n", job.duration_string()));

            // Show output preview (first 100 chars)
            let preview = if job.output.len() > 100 {
                format!("{}...", &job.output[..100])
            } else {
                job.output.clone()
            };
            result_text.push_str(&format!("  Output Preview: {}\n", preview.trim()));
            result_text.push_str("\n");
        }

        Ok(CallToolResult::success(vec![Content::text(result_text)]))
    }

    #[tool(
        name = "enhanced_terminal_job_cancel",
        description = "Cancel a running job: terminate its owned process tree on Windows; send SIGTERM on Unix.

PARAMETERS:
- job_id (string, required): The readable adjective-noun-number job identifier to cancel

BEHAVIOR:
- Sends SIGTERM signal to the process (Unix only)
- Graceful termination attempt
- Updates job status to Canceled
- Works with process groups

PLATFORM SUPPORT:
- Unix/Linux/macOS: Full support with SIGTERM
- Windows: Terminates the job-owned process group and confirms exit before reporting Canceled

RETURNS:
- Confirmation message with job_id
- Instructions to use job_status to verify cancellation"
    )]
    async fn job_cancel(
        &self,
        Parameters(input): Parameters<JobCancelInput>,
    ) -> Result<CallToolResult, McpError> {
        self.job_manager
            .cancel_job(&input.job_id)
            .map_err(|e| McpError::internal_error(format!("Failed to cancel job: {}", e), None))?;

        let result_text = format!(
            "Job {} has been canceled. Use enhanced_terminal_job_status to verify.",
            input.job_id
        );

        Ok(CallToolResult::success(vec![Content::text(result_text)]))
    }

    #[tool(
        name = "enhanced_terminal_job_stdin",
        description = "Write input to a running background job's PTY stdin.

PARAMETERS:
- job_id (string, required): The readable adjective-noun-number job identifier returned by enhanced_terminal
- input (string, required): Exact UTF-8 input to write; include \n to submit a line

BEHAVIOR:
- Writes to the PTY stdin for a job that is Running or WaitingForInput
- Does not append a newline automatically
- Useful for answering prompts or interacting with commands after they switch to background
- Refuses to answer a detected credential prompt (password/passphrase); ask the user instead

RETURNS:
- Confirmation with the number of bytes written"
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

        let byte_label = if bytes_written == 1 { "byte" } else { "bytes" };
        let result_text = format!(
            "Wrote {} {} to stdin for job {}.",
            bytes_written, byte_label, input.job_id
        );

        Ok(CallToolResult::success(vec![Content::text(result_text)]))
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

RETURNS: JSON array of binaries with:
- name: Binary name
- category: Category identifier
- found: Boolean indicating if binary exists
- path: Full path (or paths separated by ';' if multiple)
- version: Version string (if detected)
- error: Error message (if version detection failed)"
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
             For stdin, start force_async=true then call enhanced_terminal_job_stdin; include a newline to submit. A command blocked on stdin reports Status: WaitingForInput with the observed prompt line.\n\
             Default shell: {}. Previews default to 4096 o200k tokens. \
             Read head/tail or 1-based start_line/line_count; raw byte positions are 0-based with exclusive ends. Truncated output includes a job ID and readback call; follow returned byte offsets with preview_tokens=0 for exact pages. \
             Full command is available via job_status full_command=true. \
             job_search finds literal/regex (rg) or semantic (local sgrep) matches in one full transcript and returns byte readback references.\n\
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
