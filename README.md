# Enhanced Terminal MCP Server

A standalone Model Context Protocol (MCP) server that provides terminal execution, binary detection, and shell detection capabilities.

## Features

### Tools

1. **enhanced_terminal** - Execute shell commands with smart async switching
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

4. **enhanced_terminal_job_cancel** - Cancel running background jobs (Unix only)
   - Send SIGTERM to running processes
   - Graceful termination of long-running commands

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

Get full output. `job_status` returns the command summary by default; pass `full_command: true` only when you need the full command text:
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

Set `force_sync: true` to disable this behavior for specific commands. Set `force_async: true` to return a job ID immediately without waiting for the threshold, which is the recommended flow before using `enhanced_terminal_job_stdin`.

### Incremental Output

Use `enhanced_terminal_job_status` with `incremental: true` for efficient polling of long-running jobs:
- First call returns all output accumulated so far
- Subsequent calls return only new output since last check
- Read position tracked per job_id
- Reset by calling with `incremental: false`

This enables streaming-like behavior without actual streaming infrastructure.

### Interactive Job Input

Use `enhanced_terminal_job_stdin` to write to a running job's PTY stdin after it has switched to background. For commands that wait for input, start them with `force_async: true` so the first call returns a job ID immediately. The stdin tool writes exactly the provided `input` string and does not append a newline automatically.

### Output Pagination

For very long outputs, use pagination mode in `enhanced_terminal_job_status`:
- Set `offset_bytes` to starting byte position
- Set `limit_bytes` to number of bytes to select (0 = all remaining)
- Returns `has_more` flag and `total_length`
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

- **Shell**: `bash`
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
