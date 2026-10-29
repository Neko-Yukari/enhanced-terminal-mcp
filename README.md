# Enhanced Terminal MCP Server

A standalone Model Context Protocol (MCP) server that provides terminal execution, binary detection, and shell detection capabilities.

## Features

### Tools

1. **enhanced_terminal** - Execute shell commands or native programs with smart async switching
   - **Streaming Output**: Real-time output notifications in sync mode
   - Automatically switches to background after 50 seconds (configurable)
   - PTY support with proper terminal emulation
   - Configurable working directory, shell, timeout, and token preview limits
   - Security denylist blocks dangerous commands
   - Returns job ID for tracking background tasks

2. **enhanced_terminal_job_status** - Get status and output of background jobs
   - Check progress of long-running commands
   - Retrieve full output when complete
   - View exit codes and duration

3. **enhanced_terminal_job_list** - List all jobs (running and completed)
   - See recent command history
   - Filter and limit results
   - Quick overview of job statuses

4. **enhanced_terminal_job_cancel** - Cancel running background jobs
   - Windows: forcefully terminate the task-owned Windows Job Object and wait for its processes to exit
   - Unix: send SIGTERM to the tracked process

5. **enhanced_terminal_job_stdin** - Send input to running background jobs
   - Write exact UTF-8 text to a job's PTY stdin
   - Include `\n` in `input` to submit a line
   - Useful for prompts after commands switch to background

6. **detect_binaries** - Detect developer tools with 16 concurrent checks
   - Scans PATH for 190+ common development tools across 26 categories
   - Fast parallel version detection
   - Supports filtering by category (rust_tools, python_tools, etc.)
   - Categories include: package managers, build systems, programming language tools, editors, containers, and more

**Note:** Shell information is automatically detected at server startup and included in the server instructions, so no separate tool call is needed to discover available shells.

### Key Features

- **Streaming Notifications**: Emits MCP logging notifications as command output arrives (client support varies)
- **Smart Async Switching**: Commands automatically move to background after 50 seconds (configurable)
- **Security Denylist**: Blocks dangerous commands like `rm -rf /`, `shutdown`, fork bombs, etc.
- **Job Management**: Track, monitor, feed stdin to, and cancel background jobs with rich metadata
- **Job Filtering**: Filter jobs by status, tags, or working directory
- **Output Pagination**: Seek into specific byte ranges of very long logs
- **Job Tags**: Categorize jobs with custom tags for easy filtering
- **Call Logging**: Appends every `enhanced_terminal` shell execution request to `enhanced_terminal_calls.jsonl`
- **16 Concurrent Checks**: Fast parallel binary detection
- **PTY Support**: Full terminal emulation for interactive commands

## Installation

### Prerequisites

- Rust with 2024 edition support (Rust 1.85+ recommended)
- Cargo

### Build from Source

```bash
git clone <repository-url>
cd enhanced-terminal-mcp
cargo build --release
```

The binary will be located at `target/release/enhanced-terminal-mcp`.

## Sudo Workflow (Recommended)

This server handles sudo commands automatically to avoid password prompts during tool execution:

1. **First sudo command**: Triggers an askpass dialog (via `sudo -A -v`) to authenticate once
2. **Subsequent sudo commands**: Rewritten to `sudo -n` (non-interactive) and use the cached sudo timestamp
3. **Keepalive**: Background task refreshes the timestamp every 5 minutes to keep it valid

All of this is **enabled by default**. The `sudo_wrapper_applied` field in results shows when the `-n` flag was added.

### Recommended Setup: Sudoers Timestamp Sharing

For the best experience, configure sudo to share timestamps across all your sessions (not just per-TTY):

1. Create `/etc/sudoers.d/enhanced-terminal-mcp` using `visudo`:

```bash
sudo visudo -f /etc/sudoers.d/enhanced-terminal-mcp
```

2. Add these lines:

```
Defaults !tty_tickets
Defaults timestamp_timeout=10
Defaults use_pty
```

3. **Benefits**:
   - Prime sudo **once** in any terminal: `sudo -v`
   - The MCP server will reuse that timestamp automatically
   - No askpass dialog needed (unless timestamp expires)
   - Works across all your terminal sessions and the MCP server

4. **Security note**: `!tty_tickets` means any process running as your user can reuse your sudo timestamp while it's valid. Keep `timestamp_timeout` reasonable (e.g., 10 minutes).

### Alternative: Askpass-based Workflow (Default Behavior)

If you prefer not to change sudoers, the server defaults will work:

- **Default askpass path**: `~/scripts/askpass-zenity.sh`
- **First sudo command** → askpass dialog
- **Server keeps timestamp alive** → no more prompts

The server will automatically pass through these env vars for GUI askpass:
- `DISPLAY` (defaults to `:0`)
- `WAYLAND_DISPLAY` (defaults to `wayland-0`)
- `XDG_RUNTIME_DIR`
- `DBUS_SESSION_BUS_ADDRESS`

### Configuration (Optional)

These environment variables control sudo behavior (all default to **ON**):

```bash
# Enable/disable sudo wrapping and keepalive (default: 1)
ENHANCED_TERMINAL_SUDO_WRAP=1
ENHANCED_TERMINAL_SUDO_KEEPALIVE=1
ENHANCED_TERMINAL_SUDO_KEEPALIVE_PRIME=1

# Custom askpass path (default: ~/scripts/askpass-zenity.sh)
ENHANCED_TERMINAL_SUDO_ASKPASS=/path/to/your/askpass.sh

# Keepalive refresh interval in seconds (default: 300, min: 30)
ENHANCED_TERMINAL_SUDO_KEEPALIVE_REFRESH_SECS=300
```

### Debugging

Enable detailed logging to see sudo priming/wrapping behavior:

```bash
RUST_LOG=debug enhanced-terminal-mcp
```

Look for log lines about `sudo -A -v` (priming) and `sudo -n` (wrapping).

## Usage

### Running the Server

The server uses stdio transport for MCP communication:

```bash
./enhanced-terminal-mcp
```

### Configuration

Add to your MCP client configuration:

```json
{
  "mcpServers": {
    "enhanced-terminal": {
      "command": "/path/to/enhanced-terminal-mcp",
      "args": []
    }
  }
}
```

### Call Logging

Every `enhanced_terminal` tool call is appended as one JSON object per line to `enhanced_terminal_calls.jsonl` in the repository root. Each entry includes an RFC3339 UTC `datetime`, the tool name, and the full submitted parameters. Writes use an in-process mutex and, on Unix, an exclusive file lock so concurrent tool calls and test server processes do not interleave JSON records.

Override the log path with `ENHANCED_TERMINAL_CALL_LOG_PATH` if needed.

### Choosing a tool in OpenCode V2

Use native `shell` for ordinary commands and background builds; background execution alone does not require this MCP. Use `enhanced_terminal` when you need direct `program` + `args`, interactive PTY stdin, task tags, or Windows task-tree cancellation.

Supply **either** `command` (interpreted by the selected shell) **or** `program` with an `args` array. Program mode launches an executable directly; it preserves empty arguments, embedded quotes, Unicode, spaces and trailing backslashes using Windows executable argument quoting. No shell expansion, pipes, substitutions or Bash-to-PowerShell translation occurs. `.cmd`/`.bat` need explicit shell command mode; applications with custom command-line parsers can still have their own rules.

```json
{"program":"C:/Python314/python.exe","args":["show_args.py","a\"b","","中文"],"cwd":"C:/work/project","force_sync":true}
```

Windows jobs are associated with an unnamed Job Object immediately after PTY spawn, using the child handle rather than a PID lookup. Descendants created after association belong to that job. Cancellation is forceful and reports success only after its active process count reaches zero. External service/WMI launches and the short spawn-to-assignment interval are not a guarantee of descendant containment. If association fails, execution fails instead of reporting a cancellable job. The task group is closed on completion/server exit; deliberately detached services should use a separate service manager.

Execution replies retain the full input command; the existing `job_status` option `full_command: true` remains available. Binary-scan JSON is compact but retains every field. Server instructions avoid repeating tool schemas. Output optimization does not lower the original 4096-token default or change a caller-selected budget.

Output previews default to 4096 o200k tokens (a display budget, not the model's exact tokenizer). The captured stream is a raw PTY transcript, so terminal wrapping/repaints may repeat text; it is not a reconstructed stdout file. Complete decoded output stays in this server process's job history and is available in byte pages; it is not a persistent disk log and is lost when this MCP process exits. Every truncated response includes a concrete follow-up call. Do not fetch the entire log merely because a preview was truncated.

### Working Directory Defaults

If `cwd` is omitted, it defaults to `.`. That `.` is resolved relative to the MCP server process working directory supplied by the caller/client. In practice, when Codex starts this MCP server from a project, omitted `cwd` uses that project/server launch directory. Pass `cwd` explicitly when you need a specific repository or subdirectory.

### Tool Examples

#### enhanced_terminal

Basic synchronous execution (completes quickly). `cwd` is optional; omitting it uses the MCP server process working directory supplied by the caller/client:
```json
{
  "command": "ls -la",
  "cwd": ".",
  "shell": "bash"
}
```

Long-running command (auto-switches to background after 50 seconds by default):
```json
{
  "command": "npm install",
  "cwd": "./my-project",
  "shell": "bash"
}
```

Force immediate async execution (useful for interactive commands that need stdin):
```json
{
  "command": "read -p 'stdin> ' value; echo received=$value",
  "force_async": true
}
```

With environment variables:
```json
{
  "command": "npm run build",
  "env_vars": {
    "NODE_ENV": "production",
    "API_KEY": "secret123"
  }
}
```

Force synchronous execution (wait for completion):
```json
{
  "command": "cargo build --release",
  "force_sync": true
}
```

With custom denylist:
```json
{
  "command": "docker run myimage",
  "custom_denylist": ["docker rm", "docker system prune"]
}
```

With tags for job categorization:
```json
{
  "command": "cargo build --release",
  "tags": ["build", "release"]
}
```

Token-bounded preview (GPT-5/o200k_base tokenizer):
```json
{
  "command": "cargo test",
  "preview_tokens": 4000
}
```

`preview_tokens` defaults to 4096. Set it to 0 to disable token truncation for the bounded in-memory preview buffer.

Job IDs are readable adjective-noun-number handles such as `brave-river-1`, making them easier to copy and discuss than numeric IDs.

#### enhanced_terminal_job_status

Read from the beginning (still preview-bounded; use `preview_tokens: 0` for unbounded output). `job_status` returns the command summary by default; pass `full_command: true` only when you need the full command text:
```json
{
  "job_id": "brave-river-1",
  "incremental": false,
  "full_command": true
}
```

Get incremental output (only new since last check):
```json
{
  "job_id": "brave-river-1",
  "incremental": true
}
```

Get paginated output (first 1000 bytes):
```json
{
  "job_id": "brave-river-1",
  "offset_bytes": 0,
  "limit_bytes": 1000
}
```

Get paginated output (next 1000 bytes):
```json
{
  "job_id": "brave-river-1",
  "offset_bytes": 1000,
  "limit_bytes": 1000
}
```

#### enhanced_terminal_job_list

List all jobs:
```json
{
  "max_jobs": 50
}
```

Filter by status:
```json
{
  "max_jobs": 50,
  "status_filter": ["Running", "Completed"]
}
```

Filter by tag:
```json
{
  "max_jobs": 50,
  "tag_filter": "build"
}
```

Filter by working directory:
```json
{
  "max_jobs": 50,
  "cwd_filter": "/home/user/project"
}
```

Combined filters with sort order:
```json
{
  "max_jobs": 50,
  "status_filter": ["Completed"],
  "tag_filter": "test",
  "sort_order": "oldest"
}
```

#### enhanced_terminal_job_cancel

```json
{
  "job_id": "brave-river-1"
}
```

#### enhanced_terminal_job_stdin

Write input to a running async job. Newlines are not appended automatically, so include `\n` when you want to submit a line:

```json
{
  "job_id": "brave-river-1",
  "input": "yes\n"
}
```

#### detect_binaries

```json
{
  "filter_categories": ["rust_tools", "python_tools"],
  "max_concurrency": 16,
  "version_timeout_ms": 1500,
  "include_missing": false
}
```

## Binary Categories

The `detect_binaries` tool supports filtering by these categories:

- `package_managers` - npm, pip, cargo, dnf, apt, snap, flatpak, brew, pnpm, uv, poetry, pipx
- `rust_tools` - cargo, rustc, rustfmt, clippy-driver
- `python_tools` - python, python3, pip, pytest, black, ruff, mypy, uv, poetry, pipenv, pipx, pyright, pylint, flake8, isort, ipython
- `build_systems` - make, cmake, ninja, gradle, maven, mvn
- `c_cpp_tools` - gcc, g++, clang, gdb, lldb
- `java_jvm_tools` - java, javac, javadoc, jar, jarsigner, jconsole, jdeps, jlink, jshell, kotlin, kotlinc, scala, scalac, groovy, groovyc
- `maven_tools` - mvn, mvnw, mvnd
- `node_js_tools` - node, deno, bun, npm, yarn, pnpm, tsx, tsc, biome, prettier, eslint
- `go_tools` - go, gofmt
- `editors_dev` - vim, nvim, emacs, code, hx, nano, micro
- `search_productivity` - rg, fd, fzf, jq, bat, tree, exa, sd, zoxide, lsd, dust, btm, broot, choose
- `system_perf` - htop, ps, top, df, du
- `containers` - docker, podman, kubectl, helm, docker-compose, kind, minikube, skopeo, buildah, nerdctl, k9s
- `networking` - curl, wget, dig, traceroute, http, nc, nmap, ss, ping, mtr, socat
- `security` - openssl, gpg, ssh-keygen, age, sops, vault, pass
- `auth_helpers` - zenity, ssh-askpass, sshaskpass, ksshaskpass, lxqt-openssh-askpass, gnome-ssh-askpass, x11-ssh-askpass, pinentry variants
- `databases` - sqlite3, psql, mysql, redis-cli, mongosh, duckdb, clickhouse-client, redis-server
- `vcs` - git, gh, lazygit, tig, gitui, hg, svn
- `cloud_cli` - aws, gcloud, az, doctl, fly, vercel, wrangler
- `iac_tools` - terraform, tofu, pulumi, ansible, ansible-playbook, vagrant, packer
- `media_tools` - ffmpeg, ffprobe, convert, magick, exiftool, yt-dlp, sox
- `ai_ml_tools` - ollama, huggingface-cli, nvidia-smi, nvcc, rocm-smi, dvc, mlflow
- `docs_tools` - pandoc, sphinx-build, mkdocs, doxygen, asciidoctor, mdbook
- `ruby_tools` - ruby, gem, bundle, rake, irb, rails
- `dotnet_tools` - dotnet, nuget, msbuild
- `cad_utils` - ODAFileConverter, dwg2svg, dwg2SVG, dwg2bmp, dwg2pdf, qcad, librecad, freecad, freecadcmd, openscad, dxf2gcode

## Development

### Building

```bash
cargo build
```

### Testing

```bash
cargo test
```

### Running Tests for Denylist

```bash
cargo test denylist
```

### Running Locally

```bash
cargo run
```

## Security

### Command Denylist

The server includes a comprehensive denylist that blocks dangerous commands:

**Destructive Operations:**
- `rm -rf /`, `rm -rf /*`, `rm --no-preserve-root`
- `mkfs`, `dd if=/dev/zero`, filesystem formatting
- Writes to `/dev/sda`, `/dev/hda`

**System Manipulation:**
- `shutdown`, `reboot`, `halt`, `poweroff`
- `init 0`, `init 6`, systemctl power commands

**Fork Bombs:**
- `:(){:|:&};:` and variants

**Dangerous Permission Changes:**
- `chmod 777 /`, `chmod -R 777 /`
- `chown -R root`, `chown root /`

**Package Manager Risks:**
- Force uninstall commands across apt, yum, dnf, pacman

**Other Risks:**
- Kernel module manipulation
- Cron deletion (`crontab -r`)
- Moving system directories

### Custom Denylist

You can add custom patterns via the `custom_denylist` parameter:

```json
{
  "command": "docker run myimage",
  "custom_denylist": ["docker rm -f", "kubectl delete"]
}
```

### Async Threshold

Commands that exceed the server async threshold (default: 50 seconds, configurable with `ENHANCED_TERMINAL_ASYNC_THRESHOLD_SECS`) automatically switch to background execution. This prevents:
- Long-running commands from blocking the MCP server
- Timeout issues with package installations
- Slow build processes hanging the interface

Set `force_sync: true` to wait for up to 60 seconds. Foreground waits are capped at 60 seconds even when the configured async threshold is higher; after that the call returns a job ID and the command keeps running in the background. Set `force_async: true` to return a job ID immediately, which is the recommended flow before using `enhanced_terminal_job_stdin`.

Commands default to `GIT_PAGER=cat` and `PAGER=cat` so Git output does not wait for a paging prompt. Explicit `env_vars` can override these values for an interactive job.

A command that stops on an interactive prompt does not sit silently in the foreground: the call returns early with `Status: WaitingForInput`, the observed prompt line as `Input Hint`, and a `Note` explaining how to answer or cancel. Background jobs get the same status while they wait and fall back to `Running` when output resumes. Prompts that look like credential requests are never answerable through `enhanced_terminal_job_stdin`; ask the user instead.

Cancelling an active MCP terminal request cancels its foreground job through the same process termination path as `enhanced_terminal_job_cancel`. Once a call has returned a background job ID, the job runs independently of that completed request; cancel it with `enhanced_terminal_job_cancel` when needed.

### Incremental Output

Use `enhanced_terminal_job_status` with `incremental: true` for efficient polling of long-running jobs:
- First call returns all output accumulated so far
- Subsequent calls return only new output since last check
- Read position tracked per job_id
- Only bytes actually returned advance the incremental cursor; truncated remainder stays unread
- Use `incremental: false` to read again from the beginning without resetting the cursor

This enables streaming-like behavior without actual streaming infrastructure.

### Interactive Job Input

Use `enhanced_terminal_job_stdin` to write to a running job's PTY stdin after it has switched to background. For commands that wait for input, start them with `force_async: true` so the first call returns a job ID immediately; a command that blocks on stdin is reported as `Status: WaitingForInput` either way. The stdin tool writes exactly the provided `input` string and does not append a newline automatically.

### Output Pagination

For very long outputs, use pagination mode in `enhanced_terminal_job_status`:
- Set `offset_bytes` to starting byte position
- Set `limit_bytes` to number of bytes to select (0 = all remaining)
- Returns total byte length, returned byte range, `Has More`, and `Next Offset Bytes`
- Use the returned next offset rather than adding the requested byte limit; UTF-8 boundaries can adjust a page
- Set `preview_tokens: 0` for exact byte-page readback without a second token cutoff
- Allows seeking into specific segments without retrieving full output

Example workflow:
```json
// Get first 1000 bytes
{"job_id": "brave-river-1", "offset_bytes": 0, "limit_bytes": 1000}
// Get next 1000 bytes
{"job_id": "brave-river-1", "offset_bytes": 1000, "limit_bytes": 1000}
// Get all remaining
{"job_id": "brave-river-1", "offset_bytes": 2000, "limit_bytes": 0}
```

### Job Tags and Filtering

Tag jobs when creating them for easier organization:
```json
{
  "command": "cargo test",
  "tags": ["test", "ci"]
}
```

Filter jobs by various criteria in `enhanced_terminal_job_list`:
- **status_filter**: Match specific statuses (e.g., ["Running", "Completed"])
- **tag_filter**: Show only jobs with a specific tag
- **cwd_filter**: Show only jobs from a specific directory
- **sort_order**: "newest" (default) or "oldest"

All filters can be combined for powerful queries.

## Architecture

This server uses a modular structure with Rust 2024 edition:

- `src/main.rs` - Entry point and server initialization
- `src/server.rs` - MCP server implementation with tool handlers
- `src/detection/` - Binary and shell detection logic
- `src/tools/` - Terminal execution, job management, security denylist

### Dependencies

- **rmcp** 0.8 - Official Rust SDK for Model Context Protocol
- **tokio** 1.x - Async runtime
- **portable-pty** 0.8 - Cross-platform PTY support for terminal emulation
- **serde/serde_json** 1.x - Serialization
- **schemars** 1.0 - JSON Schema generation for tool inputs
- **anyhow** 1.x - Error handling
- **nix** 0.29 - Unix signal handling (Unix only)
- **tiktoken-rs** 0.11 - GPT-5/o200k_base-compatible token counting for previews
- **chrono** 0.4 - UTC timestamps for call logging
- **tracing/tracing-subscriber** 0.1/0.3 - structured server logging

### Performance

- **16 concurrent binary checks** - Fast parallel tool detection (configurable)
- **Smart async switching** - Auto-background after 50s (configurable)
- **Tokio background monitoring** - Jobs continue running after smart async switching
- **Incremental output capture** - Poll new output with read position tracking; byte pagination is available for long logs
- **No timeout by default** - Set ENHANCED_TERMINAL_TIMEOUT_SECS environment variable to enable

## Configuration

### Default Values

- **Shell**: `pwsh` on Windows, `bash` on other platforms
- **Working Directory**: `.` resolved from the MCP server process working directory supplied by the caller/client
- **Preview Tokens**: `4096` GPT-5/o200k_base tokens (`0` disables token truncation)
- **Async Threshold**: `50` seconds (`ENHANCED_TERMINAL_ASYNC_THRESHOLD_SECS`)
- **Timeout**: `None` by default (`ENHANCED_TERMINAL_TIMEOUT_SECS` enables a timeout)
- **Job IDs**: readable `adjective-noun-number` handles
- **Call Log**: concurrent-safe JSONL at `enhanced_terminal_calls.jsonl` in the repo root (`ENHANCED_TERMINAL_CALL_LOG_PATH` overrides)
- **Max Binary Detection Concurrency**: `16`
- **Version Probe Timeout**: `1500` ms

## License

MIT License - see [LICENSE](LICENSE) file for details

### Searching complete job output

`enhanced_terminal_job_search` searches an immutable snapshot of one `job_id` from this MCP server. `mode: "literal"` (default) uses rg fixed strings; `"regex"` uses rg patterns; `"semantic"` uses the installed local sgrep text model offline with no persistent index. Tool discovery reuses OpenCode's cached rg and the existing sgrep installation; optional host environment overrides are `ENHANCED_TERMINAL_RG_BIN` / `ENHANCED_TERMINAL_SGREP_BIN`. PATH changes affect only the search child.

```json
{"job_id":"brave-river-1","query":"ELOCK","mode":"literal","limit":10,"context_lines":2,"context_bytes":4096}
{"job_id":"brave-river-1","query":"recover after a temporary failure","mode":"semantic","limit":3}
```

The view is **stored UTF-8 raw PTY transcript**, with ANSI and cursor-repaint text preserved, not reconstructed stdout. A result identifies snapshot size, line range, byte positions and context source range. Semantic results locate relevant line blocks rather than exact query tokens. Context is an explicitly selected excerpt (`context_bytes`, 0 disables that excerpt limit); omitted surrounding bytes remain available using each result's exact `job_status` readback arguments. Search never scans the working directory or other jobs, changes the incremental cursor, or discards the original transcript. Temporary snapshots are removed when the search finishes. A service/index restart does not preserve in-memory job history.

### Preview direction and cursors

Initial previews take the **head**, using the original default 4096 o200k tokens; they do not accumulate lines backward from the tail. Token boundaries are adjusted only to keep UTF-8 valid. Initial capture also has the existing bounded byte buffer; full job history is separate. Synchronous and background modes have the same direction; an immediate background reply may have no output yet.

`job_status` explicit `offset_bytes`/`limit_bytes` refer to the stored transcript's UTF-8 bytes, then `preview_tokens` applies. Follow returned offsets (UTF-8 page boundaries may adjust). Explicit pages never move the job's shared incremental cursor. Default incremental reads start at that cursor and advance only by bytes actually returned; `incremental: false` reads from the beginning without resetting it. Head remains the default. Use explicit `direction: "tail"` to read the suffix in normal order; `before_byte` is the exclusive backward continuation anchor. Preview limits and excerpt pagination are retrieval controls, not lossless deduplication.

### Head/tail and line-addressed reads

```json
{"job_id":"brave-river-1","direction":"head","line_count":20}
{"job_id":"brave-river-1","direction":"tail","line_count":20}
{"job_id":"brave-river-1","start_line":40,"line_count":10}
{"job_id":"brave-river-1","offset_bytes":500,"limit_bytes":4096,"preview_tokens":0}
{"job_id":"brave-river-1","direction":"tail","before_byte":500,"limit_bytes":200}
```

These are `enhanced_terminal_job_status` arguments. Initial `enhanced_terminal` previews also accept `direction: "head" | "tail"`; the default is head. Tail always displays the selected suffix forward, not reversed. A line request and a byte request cannot be combined. Tail uses `line_count` or `limit_bytes`; use `before_byte`, not `offset_bytes`/`start_line`, to continue backward. Setting even `offset_bytes: 0` explicitly positions the read and does not take the shared incremental cursor.

The body after `Output:` remains the exact selected raw transcript, without inserted line-number prefixes. Headers return its **1-based first/last line**, total line count and **0-based byte start/exclusive end**. `Read Metadata` gives machine-readable indices, `partial_line`, `partial_start`/`partial_end`, the selected envelope, and concrete continuation arguments. Line numbers count stored LF breaks (CRLF is one break), not screen rows or reconstructed stdout lines; ANSI cursor movement is not interpreted.

One enormous line still obeys this call's token budget. Follow `Continue Selection` rather than repeating `start_line` when it is partial; use the returned end byte for forward reads, or returned start as `before_byte` for backward reads. Each tail page is forward-ordered; prepend older pages to reconstruct a transcript. Explicit reads, tail and search never change the shared incremental cursor. Byte boundaries are adjusted to whole UTF-8 code points (a one-byte page may therefore return one complete multibyte character). A token budget too small for even one code point is rejected rather than returning a non-advancing page. `preview_tokens: 0` explicitly disables the token limit; use `limit_bytes` to keep such reads bounded. The default remains 4096.
