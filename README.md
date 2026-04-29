# Enhanced Terminal MCP Server

A standalone Model Context Protocol (MCP) server that provides terminal execution, binary detection, and shell detection capabilities. This server extracts and reimplements key tools from the Zed editor project.

## Features

### Tools

1. **enhanced_terminal** - Execute shell commands with smart async switching
   - **Streaming Output**: Real-time output notifications in sync mode
   - Automatically switches to background after 50 seconds (configurable)
   - PTY support with proper terminal emulation
   - Configurable working directory, shell, timeout, and output limits
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

5. **detect_binaries** - Detect developer tools with 16 concurrent checks
   - Scans PATH for 100+ common development tools
   - Fast parallel version detection
   - Supports filtering by category (rust_tools, python_tools, etc.)
   - Categories include: package managers, build systems, programming language tools, editors, containers, and more

**Note:** Shell information is automatically detected at server startup and included in the server instructions, so no separate tool call is needed to discover available shells.

### Key Features

- **Streaming Output**: Real-time output notifications as commands execute (sync mode)
- **Smart Async Switching**: Commands automatically move to background after 50 seconds (configurable)
- **Security Denylist**: Blocks dangerous commands like `rm -rf /`, `shutdown`, fork bombs, etc.
- **Job Management**: Track, monitor, and cancel background jobs with rich metadata
- **Job Filtering**: Filter jobs by status, tags, or working directory
- **Output Pagination**: Seek into specific byte ranges of very long logs
- **Job Tags**: Categorize jobs with custom tags for easy filtering
- **16 Concurrent Checks**: Fast parallel binary detection
- **PTY Support**: Full terminal emulation for interactive commands

## Installation

### Prerequisites

- Rust 1.70 or later
- Cargo

### Build from Source

```bash
git clone <repository-url>
# enhanced-terminal-mcp

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
cargo build --release
```

The binary will be located at `target/release/enhanced-terminal-mcp`.

## Usage

### Running the Server

The server uses stdio transport for MCP communication:

```bash
./enhanced-terminal-mcp
```

### Configuration

Add to your MCP client configuration (e.g., Claude Desktop, Zed):

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

### Tool Examples

#### enhanced_terminal

Basic synchronous execution (completes quickly):
```json
{
  "command": "ls -la",
  "cwd": ".",
  "shell": "bash"
}
```

Long-running command (auto-switches to background after 50 seconds):
```json
{
  "command": "npm install",
  "cwd": "./my-project",
  "shell": "bash",
  "async_threshold_secs": 50
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

#### enhanced_terminal_job_status

Get full output:
```json
{
  "job_id": "job-123",
  "incremental": false
}
```

Get incremental output (only new since last check):
```json
{
  "job_id": "job-123",
  "incremental": true
}
```

Get paginated output (first 1000 bytes):
```json
{
  "job_id": "job-123",
  "offset": 0,
  "limit": 1000
}
```

Get paginated output (next 1000 bytes):
```json
{
  "job_id": "job-123",
  "offset": 1000,
  "limit": 1000
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
  "job_id": "job-123"
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

- `package_managers` - npm, pip, cargo, dnf, apt, etc.
- `rust_tools` - cargo, rustc, rustfmt, clippy
- `python_tools` - python, pip, pytest, black, ruff
- `build_systems` - make, cmake, ninja
- `c_cpp_tools` - gcc, g++, clang, gdb
- `java_jvm_tools` - java, javac
- `node_js_tools` - node, deno, bun
- `go_tools` - go, gofmt
- `editors_dev` - vim, nvim, emacs, code
- `search_productivity` - rg, fd, fzf, jq, bat, tree
- `system_perf` - htop, ps, top, df, du
- `containers` - docker, podman, kubectl
- `networking` - curl, wget, dig
- `security` - openssl, gpg, ssh-keygen
- `databases` - sqlite3, psql, mysql
- `vcs` - git, gh

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
cargo test --lib denylist
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

Commands that exceed `async_threshold_secs` (default: 50 seconds) automatically switch to background execution. This prevents:
- Long-running commands from blocking the MCP server
- Timeout issues with package installations
- Slow build processes hanging the interface

Set `force_sync: true` to disable this behavior for specific commands.

### Incremental Output

Use `enhanced_terminal_job_status` with `incremental: true` for efficient polling of long-running jobs:
- First call returns all output accumulated so far
- Subsequent calls return only new output since last check
- Read position tracked per job_id
- Reset by calling with `incremental: false`

This enables streaming-like behavior without actual streaming infrastructure.

### Output Pagination

For very long outputs, use pagination mode in `enhanced_terminal_job_status`:
- Set `offset` to starting byte position
- Set `limit` to number of bytes to return (0 = all remaining)
- Returns `has_more` flag and `total_length`
- Allows seeking into specific segments without retrieving full output

Example workflow:
```json
// Get first 1000 bytes
{"job_id": "job-123", "offset": 0, "limit": 1000}
// Get next 1000 bytes
{"job_id": "job-123", "offset": 1000, "limit": 1000}
// Get all remaining
{"job_id": "job-123", "offset": 2000, "limit": 0}
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

### Performance

- **16 concurrent binary checks** - Fast parallel tool detection (configurable)
- **Smart async switching** - Auto-background after 50s (configurable)
- **Thread-based job execution** - Efficient background task management
- **Incremental output capture** - Memory-efficient streaming with read position tracking
- **No timeout by default** - Set ENHANCED_TERMINAL_TIMEOUT_SECS environment variable to enable

## Configuration

### Default Values

- **Shell**: `bash` (was: `sh`)
- **Async Threshold**: `50` seconds (was: `5`)
- **Timeout**: `None` (no timeout by default, set via ENHANCED_TERMINAL_TIMEOUT_SECS environment variable)
- **Output Limit**: `16384` bytes (16KB)
- **Max Concurrency**: `16` (was: `12`)
- **Version Timeout**: `1500` ms

## License

MIT License - see [LICENSE](LICENSE) file for details

## Credits

Tools extracted and adapted from the [Zed editor](https://github.com/zed-industries/zed) project.