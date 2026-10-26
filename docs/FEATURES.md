# Enhanced Terminal MCP Server - Features

## Overview

A production-ready Model Context Protocol (MCP) server providing terminal execution, job management, and developer tool detection with enterprise-grade security and performance.

## Core Features

### 1. Smart Async Command Execution

**What it does:** Automatically switches long-running commands to background execution.

**Key Points:**
- Default threshold: 5 seconds (configurable via `async_threshold_secs`)
- Returns immediately with job ID when threshold exceeded
- Commands continue running in background
- Use `job_status` to check progress and retrieve output
- Set `force_sync: true` to disable auto-switching

**Use Cases:**
- Package installations (`npm install`, `cargo build`)
- Large file operations
- Database migrations
- Docker image builds
- Test suites

**Example:**
```json
{
  "command": "npm install",
  "async_threshold_secs": 5
}
```
Returns immediately with job ID after 5 seconds if still running.

### 2. Job Management

**What it does:** Track, monitor, and control background jobs.

**Available Tools:**

#### job_status
Get detailed status and output of any job:
- Current status (Running, WaitingForInput, Completed, Failed, TimedOut, Canceled)
- Exit code (if completed)
- Duration
- Full output (respects output limits)
- PID information

#### job_list
List all jobs with quick overview:
- Shows recent jobs first
- Configurable limit (default: 50)
- Output previews (first 100 chars)
- Status and duration for each job

#### job_cancel
Cancel running jobs (Unix only):
- Sends SIGTERM to process
- Graceful termination
- Updates job status to Canceled
- Works with process trees

**Use Cases:**
- Monitor long-running builds
- Cancel stuck operations
- Review command history
- Debug failed commands
- Track parallel operations

### 3. Security Denylist

**What it does:** Blocks dangerous commands before execution.

**Protected Against:**

**Destructive Operations:**
- `rm -rf /` and variants
- `mkfs` (filesystem formatting)
- `dd if=/dev/zero` (disk wiping)
- Direct device writes (`> /dev/sda`)

**System Manipulation:**
- `shutdown`, `reboot`, `halt`, `poweroff`
- System init commands
- Systemctl power operations

**Resource Exhaustion:**
- Fork bombs (`:(){:|:&};:`)
- Infinite loops
- Memory bombs

**Dangerous Permissions:**
- `chmod 777 /`
- `chown -R root`
- Recursive permission changes on system directories

**Package Management:**
- Force removal commands
- System package uninstallation

**Other Risks:**
- Kernel module manipulation (`rmmod`, `insmod`)
- Cron deletion (`crontab -r`)
- Moving system directories (`mv /etc`, `mv /usr`)

**Custom Patterns:**
Add your own patterns via `custom_denylist`:
```json
{
  "command": "docker run myimage",
  "custom_denylist": ["docker rm -f", "kubectl delete --all"]
}
```

**Case Insensitive:** All patterns matched case-insensitively.

### 4. High-Performance Binary Detection

**What it does:** Scans for developer tools with parallel version detection.

**Performance:**
- 16 concurrent checks (configurable)
- ~1.5 second timeout per binary
- Optimized thread pool
- Skips missing binaries quickly

**Categories (100+ tools):**
- package_managers (npm, pip, cargo, dnf, apt, snap, flatpak, brew)
- rust_tools (cargo, rustc, rustfmt, clippy)
- python_tools (python, pip, pytest, black, ruff, mypy)
- build_systems (make, cmake, ninja, gradle, maven)
- c_cpp_tools (gcc, g++, clang, gdb, lldb)
- java_jvm_tools (java, javac, kotlin)
- node_js_tools (node, deno, bun, npm, yarn)
- go_tools (go, gofmt)
- editors_dev (vim, nvim, emacs, code, zed)
- search_productivity (rg, fd, fzf, jq, bat, tree, exa)
- system_perf (htop, ps, top, df, du)
- containers (docker, podman, kubectl, helm)
- networking (curl, wget, dig, traceroute)
- security (openssl, gpg, ssh-keygen)
- databases (sqlite3, psql, mysql, redis-cli)
- vcs (git, gh)

**Filtering:**
```json
{
  "filter_categories": ["rust_tools", "python_tools"],
  "max_concurrency": 16,
  "include_missing": false
}
```

### 5. Shell Detection (Built-in)

**What it does:** Automatically detects shells at server startup.

**Benefits:**
- No separate tool call needed
- Reduces latency and token usage
- Information always available in server instructions
- Includes version detection

**Detected Shells:**
- bash, zsh, fish
- sh, dash, ksh
- tcsh, csh
- Custom shells from $SHELL

**Integration:**
Shell information embedded in `ServerInfo` instructions visible to LLM.

### 6. PTY Support

**What it does:** Full terminal emulation for commands.

**Features:**
- Proper terminal sizing (24x80 default)
- ANSI color code support
- Interactive program support
- Signal handling
- Process group management

**Use Cases:**
- Running programs that expect TTY
- Color output preservation
- Progress bars and spinners
- Interactive CLIs

### 7. Output Management

**What it does:** Intelligent output handling for large results.

**Features:**
- Configurable output limits (default: 16KB)
- Incremental capture during execution
- Truncation indicators
- Full output stored for job_status
- Memory-efficient streaming

**Behavior:**
- Preview output shown immediately
- Full output available via job_status
- Truncation notification when limit hit
- Separate storage for full vs preview output

## Configuration Options

### Terminal Execution

```json
{
  "command": "string",              // Required: command to execute
  "cwd": "string",                  // Default: "."
  "shell": "string",                // Default: "sh"
  "output_limit": number,           // Default: 16384 (16KB)
  "async_threshold_secs": number,   // Default: 5 seconds
  "force_sync": boolean,            // Default: false
  "custom_denylist": ["string"],    // Default: []
  "env_vars": object,               // Default: {}
  "tags": ["string"]                // Default: []
}
```

**Note:** Timeout is configured via the `ENHANCED_TERMINAL_TIMEOUT_SECS` environment variable (no timeout by default).

### Binary Detection

```json
{
  "filter_categories": ["string"],  // Optional: category filter
  "max_concurrency": number,        // Default: 16
  "version_timeout_ms": number,     // Default: 1500
  "include_missing": boolean        // Default: false
}
```

### Job Management

```json
// job_status
{
  "job_id": "string"               // Required: job identifier
}

// job_list
{
  "max_jobs": number               // Default: 50
}

// job_cancel
{
  "job_id": "string"               // Required: job to cancel
}
```

## Performance Characteristics

### Speed
- Binary detection: ~2-3 seconds for all 100+ tools (16 concurrent)
- Command startup: <100ms overhead
- Async switching: <1ms decision time
- Job status query: <5ms

### Memory
- Per job overhead: ~8KB (excluding output)
- Output storage: Configurable limit per job
- Job registry: In-memory HashMap
- Binary cache: None (stateless detection)

### Concurrency
- 16 parallel binary checks
- Unlimited concurrent jobs (system limited)
- Thread-per-job for background execution
- Lock-free job status reads

## Platform Support

### Unix/Linux/macOS
- Full feature support
- SIGTERM for job cancellation
- Unix permissions checking
- Process group management
- Fork detection

### Windows
- Basic support (not fully tested)
- No signal-based cancellation
- File permissions limited
- PTY may have limitations

## Security Model

### Defense in Depth
1. **Pre-execution validation:** Denylist check before spawn
2. **Resource limits:** Timeout and output size caps
3. **Permission isolation:** Runs as invoking user
4. **No privilege escalation:** No sudo by default
5. **Output sanitization:** Safe string handling

### Threat Protection
- ✅ Command injection (via denylist)
- ✅ Resource exhaustion (via timeouts)
- ✅ Disk filling (via output limits)
- ✅ Fork bombs (via denylist)
- ✅ System damage (via denylist)
- ⚠️ Network attacks (partial - custom denylist)
- ⚠️ Data exfiltration (user responsibility)

### Best Practices
1. Use custom denylist for sensitive environments
2. Set appropriate timeouts for your use case
3. Monitor job_list for unexpected commands
4. Review denial patterns regularly
5. Consider allowlist mode for strict security (future feature)

## Integration Examples

### Claude Desktop

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

### Zed Editor

```json
{
  "context_servers": {
    "enhanced-terminal": {
      "command": "/path/to/enhanced-terminal-mcp",
      "args": [],
      "enabled": true
    }
  }
}
```

### Custom MCP Client

```javascript
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';

const transport = new StdioClientTransport({
  command: '/path/to/enhanced-terminal-mcp',
  args: []
});

const client = new Client({
  name: 'my-client',
  version: '1.0.0'
}, {
  capabilities: {}
});

await client.connect(transport);
```

## Comparison with Alternatives

### vs. Shell MCP Server
- ✅ Job management
- ✅ Smart async switching
- ✅ Security denylist
- ✅ Binary detection
- ✅ PTY support

### vs. Direct Shell Access
- ✅ Safety (denylist)
- ✅ Monitoring (job tracking)
- ✅ Async handling
- ❌ Slightly slower (validation overhead)
- ❌ Not interactive (stdin limited)

### vs. SSH MCP Server
- ✅ Local execution (no network)
- ✅ Simpler setup
- ❌ No remote execution
- ✅ Better performance

## Roadmap

See SUMMARY.md for complete future enhancements list.

**Near-term:**
- [ ] Resource support (file reading)
- [ ] Prompt support (common tasks)
- [ ] Progress notifications
- [ ] Environment variable management

**Long-term:**
- [ ] Interactive stdin support
- [ ] Job output streaming via SSE
- [ ] Persistent job history
- [ ] Allowlist mode
- [ ] Windows signal support
- [ ] Command templates

## License

MIT License - See LICENSE file for details.

## Support

- GitHub Issues: https://github.com/tsoernes/enhanced-terminal-mcp/issues
- Documentation: README.md, SUMMARY.md
- Examples: See README.md "Usage" section