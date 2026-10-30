use std::borrow::Cow;
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParam;
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};
use tokio::process::Command;

/// Spawn the MCP server as a child process and return its piped stdin/stdout as an AsyncRead/AsyncWrite transport.
async fn spawn_child_stdio_transport() -> (
    impl tokio::io::AsyncRead + Send + Unpin + 'static,
    impl tokio::io::AsyncWrite + Send + Unpin + 'static,
) {
    spawn_child_stdio_transport_with_env(&[]).await
}

async fn spawn_child_stdio_transport_with_env(
    env_vars: &[(&str, &str)],
) -> (
    impl tokio::io::AsyncRead + Send + Unpin + 'static,
    impl tokio::io::AsyncWrite + Send + Unpin + 'static,
) {
    spawn_child_stdio_transport_with_env_and_cwd(env_vars, None).await
}

async fn spawn_child_stdio_transport_with_env_and_cwd(
    env_vars: &[(&str, &str)],
    cwd: Option<&Path>,
) -> (
    impl tokio::io::AsyncRead + Send + Unpin + 'static,
    impl tokio::io::AsyncWrite + Send + Unpin + 'static,
) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_enhanced-terminal-mcp"));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    for (key, value) in env_vars {
        command.env(key, value);
    }

    let mut child = command
        .spawn()
        .expect("failed to spawn enhanced-terminal-mcp test server");

    let stdin = child.stdin.take().expect("child stdin missing");
    let stdout = child.stdout.take().expect("child stdout missing");

    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    (stdout, stdin)
}

async fn connect_child_client() -> RunningService<RoleClient, ()> {
    let (r, w) = spawn_child_stdio_transport().await;

    // `()` implements `ClientHandler` in rmcp, so it can act as a no-op client service.
    // With `transport-async-rw`, (AsyncRead, AsyncWrite) can be used as a client transport.
    ().serve((r, w))
        .await
        .expect("failed to initialize rmcp client over child stdio")
}

async fn connect_child_client_with_env(
    env_vars: &[(&str, &str)],
) -> RunningService<RoleClient, ()> {
    let (r, w) = spawn_child_stdio_transport_with_env(env_vars).await;

    // `()` implements `ClientHandler` in rmcp, so it can act as a no-op client service.
    // With `transport-async-rw`, (AsyncRead, AsyncWrite) can be used as a client transport.
    ().serve((r, w))
        .await
        .expect("failed to initialize rmcp client over child stdio")
}

async fn connect_child_client_with_cwd(cwd: &Path) -> RunningService<RoleClient, ()> {
    let (r, w) = spawn_child_stdio_transport_with_env_and_cwd(&[], Some(cwd)).await;

    // `()` implements `ClientHandler` in rmcp, so it can act as a no-op client service.
    // With `transport-async-rw`, (AsyncRead, AsyncWrite) can be used as a client transport.
    ().serve((r, w))
        .await
        .expect("failed to initialize rmcp client over child stdio")
}

fn text_from_calltool(res: rmcp::model::CallToolResult) -> String {
    let mut out = String::new();
    for c in res.content {
        if let Some(t) = c.raw.as_text() {
            out.push_str(&t.text);
        }
    }
    out
}

fn json_of(res: rmcp::model::CallToolResult) -> Value {
    let text = text_from_calltool(res);
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("expected a JSON reply ({error}): {text}"))
}

/// Shells exercised by these tests. Each command below is expressed per shell
/// so the suite runs anywhere at least one of them exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellKind {
    Bash,
    Cmd,
    PowerShell,
    Pwsh,
}

impl ShellKind {
    fn name(self) -> &'static str {
        match self {
            ShellKind::Bash => "bash",
            ShellKind::Cmd => "cmd",
            ShellKind::PowerShell => "powershell",
            ShellKind::Pwsh => "pwsh",
        }
    }

    fn available(self) -> bool {
        // Windows ships a WSL `bash.exe` stub that this server does not list as
        // an available shell, so bash is only exercised on Unix.
        match self {
            ShellKind::Bash => cfg!(not(windows)) && command_exists("bash"),
            ShellKind::Cmd | ShellKind::PowerShell | ShellKind::Pwsh => {
                cfg!(windows) && command_exists(self.name())
            }
        }
    }

    /// `echo` is a builtin in all four shells and appends a newline.
    fn echo(self, text: &str) -> String {
        format!("echo {text}")
    }

    /// Print the working directory of the running job.
    fn print_cwd(self) -> String {
        match self {
            ShellKind::Bash => "pwd".to_string(),
            ShellKind::Cmd => "cd".to_string(),
            ShellKind::PowerShell | ShellKind::Pwsh => "(Get-Location).Path".to_string(),
        }
    }

    /// Read one stdin line and echo it back as `{prefix}<line>`.
    ///
    /// cmd needs delayed expansion for the variable it just read; the nested
    /// `cmd /v:on` inherits `line` through the environment, so `!line!` works
    /// without escaping quotes into the command string.
    fn read_stdin_then_echo(self, prefix: &str) -> String {
        match self {
            ShellKind::Bash => format!("IFS= read -r line; echo \"{prefix}$line\""),
            ShellKind::Cmd => format!("set /p line= & cmd /v:on /c echo {prefix}!line!"),
            ShellKind::PowerShell | ShellKind::Pwsh => {
                format!("$line = Read-Host; Write-Output \"{prefix}$line\"")
            }
        }
    }

    /// A command with a long trailing comment, so the same text works whether
    /// the shell treats `#` as a comment (bash, PowerShell) or as arguments (cmd).
    fn echo_with_comment(self, text: &str, comment: &str) -> String {
        format!("echo {text} # {comment}")
    }
}

fn command_exists(name: &str) -> bool {
    #[cfg(windows)]
    let mut probe = {
        let mut probe = std::process::Command::new("where.exe");
        probe.arg(name);
        probe
    };
    #[cfg(not(windows))]
    let mut probe = {
        let mut probe = std::process::Command::new("sh");
        probe.args(["-c", &format!("command -v {name}")]);
        probe
    };
    probe
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn test_shells() -> Vec<ShellKind> {
    let shells: Vec<ShellKind> = [
        ShellKind::Bash,
        ShellKind::Cmd,
        ShellKind::PowerShell,
        ShellKind::Pwsh,
    ]
    .into_iter()
    .filter(|shell| shell.available())
    .collect();
    assert!(
        !shells.is_empty(),
        "integration tests need bash, cmd, powershell or pwsh on PATH"
    );
    shells
}

/// Compare shell-reported paths without the Windows canonical `\\?\` prefix.
fn normalize_path(value: &str) -> String {
    value
        .trim()
        .trim_start_matches(r"\\?\")
        .replace('/', r"\")
        .to_ascii_lowercase()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_tools_smoke() {
    let client = connect_child_client().await;

    let tools = client
        .peer()
        .list_tools(Default::default())
        .await
        .expect("tools/list failed");

    let mut names: Vec<String> = tools
        .tools
        .into_iter()
        .map(|t| t.name.into_owned())
        .collect();
    names.sort();

    assert!(names.iter().any(|n| n == "enhanced_terminal"));
    assert!(names.iter().any(|n| n == "enhanced_terminal_job_status"));
    assert!(names.iter().any(|n| n == "enhanced_terminal_job_list"));
    assert!(names.iter().any(|n| n == "enhanced_terminal_job_cancel"));
    assert!(names.iter().any(|n| n == "enhanced_terminal_job_stdin"));
    assert!(names.iter().any(|n| n == "detect_binaries"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enhanced_terminal_logs_calls_to_jsonl() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after epoch")
        .as_nanos();
    let log_path = std::env::temp_dir().join(format!(
        "enhanced_terminal_calls_{}_{}.jsonl",
        std::process::id(),
        unique
    ));
    let _ = fs::remove_file(&log_path);
    let log_path_string = log_path.display().to_string();
    let client = connect_child_client_with_env(&[(
        "ENHANCED_TERMINAL_CALL_LOG_PATH",
        log_path_string.as_str(),
    )])
    .await;

    for shell in test_shells() {
        let command = shell.echo("log-test");
        let res = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "command": command,
                        "cwd": ".",
                        "shell": shell.name(),
                        "force_sync": true,
                        "preview_tokens": 0,
                        "env_vars": {"LOG_TEST": "1"},
                        "tags": ["audit-test"]
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call enhanced_terminal failed");

        let result = json_of(res);
        assert!(
            result["output"]
                .as_str()
                .unwrap_or_default()
                .contains("log-test"),
            "unexpected output for {}: {result}",
            shell.name()
        );

        let log_text = fs::read_to_string(&log_path).expect("call log should be written");
        let log_line = log_text
            .lines()
            .last()
            .expect("call log should not be empty");
        let entry: Value = serde_json::from_str(log_line).expect("call log line should be JSON");

        assert_eq!(entry["tool"], "enhanced_terminal");
        assert!(
            entry["datetime"]
                .as_str()
                .is_some_and(|value| value.contains('T')),
            "missing RFC3339-like datetime: {entry:?}"
        );
        assert_eq!(entry["parameters"]["command"], json!(command));
        assert_eq!(entry["parameters"]["shell"], json!(shell.name()));
        assert_eq!(entry["parameters"]["force_sync"], true);
        assert_eq!(entry["parameters"]["env_vars"]["LOG_TEST"], "1");
        assert_eq!(entry["parameters"]["tags"][0], "audit-test");
    }

    let _ = fs::remove_file(&log_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn enhanced_terminal_concurrent_processes_log_valid_jsonl() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after epoch")
        .as_nanos();
    let log_path = std::env::temp_dir().join(format!(
        "enhanced_terminal_concurrent_calls_{}_{}.jsonl",
        std::process::id(),
        unique
    ));
    let _ = fs::remove_file(&log_path);
    let log_path_string = log_path.display().to_string();

    let shells = test_shells();
    let mut tasks = Vec::new();
    for index in 0..8 {
        let log_path_string = log_path_string.clone();
        let shell = shells[index % shells.len()];
        tasks.push(tokio::spawn(async move {
            let client = connect_child_client_with_env(&[(
                "ENHANCED_TERMINAL_CALL_LOG_PATH",
                log_path_string.as_str(),
            )])
            .await;
            let marker = format!("concurrent-log-marker-{index}");
            let long_comment = "x".repeat(4096);
            let command = shell.echo_with_comment(&marker, &long_comment);

            let res = client
                .peer()
                .call_tool(CallToolRequestParam {
                    name: Cow::Borrowed("enhanced_terminal"),
                    arguments: Some(
                        serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                            "command": command,
                            "cwd": ".",
                            "shell": shell.name(),
                            "force_sync": true,
                            "preview_tokens": 0,
                            "tags": ["concurrent-log-test", marker]
                        }))
                        .expect("tool arguments must be a JSON object")
                        .into_iter()
                        .collect(),
                    ),
                })
                .await
                .expect("tools/call enhanced_terminal failed");

            let result = json_of(res);
            assert!(
                result["output"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(&marker),
                "unexpected output for {}: {result}",
                shell.name()
            );
            marker
        }));
    }

    let mut expected_markers = Vec::new();
    for task in tasks {
        expected_markers.push(task.await.expect("concurrent log task panicked"));
    }
    expected_markers.sort();

    let log_text = fs::read_to_string(&log_path).expect("call log should be written");
    let lines: Vec<&str> = log_text.lines().collect();
    assert_eq!(
        lines.len(),
        expected_markers.len(),
        "expected exactly one JSONL record per call; log was: {log_text}"
    );

    let mut actual_markers = Vec::new();
    for (line_number, line) in lines.iter().enumerate() {
        let entry: Value = serde_json::from_str(line).unwrap_or_else(|error| {
            panic!(
                "call log line {} should be valid JSON: {error}; line was: {line}",
                line_number + 1
            )
        });
        assert_eq!(entry["tool"], "enhanced_terminal");
        let command = entry["parameters"]["command"]
            .as_str()
            .expect("logged command should be a string");
        let marker = command
            .split_whitespace()
            .nth(1)
            .expect("logged command should include marker")
            .to_string();
        actual_markers.push(marker);
    }
    actual_markers.sort();
    assert_eq!(actual_markers, expected_markers);

    let _ = fs::remove_file(&log_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enhanced_terminal_omitted_cwd_uses_server_process_cwd() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after epoch")
        .as_nanos();
    let temp_dir = std::env::temp_dir().join(format!(
        "enhanced_terminal_cwd_{}_{}",
        std::process::id(),
        unique
    ));
    fs::create_dir_all(&temp_dir).expect("failed to create temp server cwd");
    let expected_cwd = temp_dir
        .canonicalize()
        .expect("failed to canonicalize temp cwd");

    let client = connect_child_client_with_cwd(&expected_cwd).await;
    let expected = normalize_path(&expected_cwd.to_string_lossy());
    for shell in test_shells() {
        let res = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "command": shell.print_cwd(),
                        "shell": shell.name(),
                        "force_sync": true,
                        "preview_tokens": 0
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call enhanced_terminal failed");

        let result = json_of(res);
        assert_eq!(
            normalize_path(result["working_directory"].as_str().unwrap_or_default()),
            expected,
            "omitted cwd should use server process cwd for {}: {result}",
            shell.name()
        );
        assert!(
            normalize_path(result["output"].as_str().unwrap_or_default()).contains(&expected),
            "printed cwd should match the server process cwd for {}: {result}",
            shell.name()
        );
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enhanced_terminal_echo() {
    let client = connect_child_client().await;

    for shell in test_shells() {
        let res = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "command": shell.echo("hello"),
                        "cwd": ".",
                        "shell": shell.name(),
                        "force_sync": true
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call enhanced_terminal failed");

        let result = json_of(res);
        let output = result["output"].as_str().unwrap_or_default();
        assert!(
            output.contains("hello"),
            "unexpected output for {}: {result}",
            shell.name()
        );
        assert_eq!(
            result["exit_code"],
            json!(0),
            "exit code missing for {}: {result}",
            shell.name()
        );
        assert_eq!(
            result["success"],
            json!(true),
            "success flag missing for {}: {result}",
            shell.name()
        );

        let job_id = result["job_id"]
            .as_str()
            .expect("missing job id in JSON reply");
        let parts: Vec<&str> = job_id.split('-').collect();
        assert!(
            parts.len() == 3 && parts[2].parse::<u64>().is_ok() && !job_id.starts_with("job-"),
            "expected readable adjective-noun-number job id, got: {job_id}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_disables_default_pager() {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let directory = std::env::temp_dir().join(format!("terminal-pager-{}-{unique}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("before.txt"), "before\n").unwrap();
    fs::write(directory.join("after.txt"), "after\n").unwrap();
    let client = connect_child_client_with_env(&[("ENHANCED_TERMINAL_TIMEOUT_SECS", "3")]).await;
    let result = tokio::time::timeout(Duration::from_secs(10), client.peer().call_tool(CallToolRequestParam {
        name: Cow::Borrowed("enhanced_terminal"),
        arguments: Some(serde_json::from_value::<serde_json::Map<String, Value>>(json!({
            "program": "git",
            "args": ["--paginate", "-c", "core.pager=less", "diff", "--no-index", "--", "before.txt", "after.txt"],
            "cwd": directory,
            "force_sync": true,
        })).unwrap()),
    })).await.expect("git diff request never returned").expect("git diff call failed");
    let json = json_of(result);
    let output = json["output"].as_str().unwrap_or_default();
    assert!(output.contains("+after"), "diff did not reach stdout: {json}");
    assert_eq!(json["timed_out"], json!(false), "pager blocked git diff: {json}");
    assert!(!output.contains("Press RETURN"), "pager blocked git diff: {json}");
    fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forced_sync_returns_background_job_after_bounded_wait() {
    let client = connect_child_client().await;
    let command = if cfg!(windows) {
        "Write-Output 'waiting-for-input'; $line = Read-Host; Write-Output ('received:' + $line)"
    } else {
        "printf 'waiting-for-input\n'; IFS= read -r line; printf 'received:%s\n' \"$line\""
    };
    let run = tokio::time::timeout(Duration::from_secs(70), client.peer().call_tool(CallToolRequestParam {
        name: Cow::Borrowed("enhanced_terminal"),
        arguments: Some(serde_json::from_value::<serde_json::Map<String, Value>>(json!({
            "command": command,
            "shell": if cfg!(windows) { "pwsh" } else { "bash" },
            "force_sync": true,
        })).unwrap()),
    })).await.expect("force_sync blocked past the foreground limit").expect("terminal call failed");
    let run = json_of(run);
    assert_eq!(
        run["switched_to_async"],
        json!(true),
        "request did not release its caller: {run}"
    );
    let job_id = run["job_id"].as_str().unwrap().to_string();
    let input = client.peer().call_tool(CallToolRequestParam {
        name: Cow::Borrowed("enhanced_terminal_job_stdin"),
        arguments: Some(serde_json::from_value::<serde_json::Map<String, Value>>(json!({
            "job_id": job_id,
            "input": "still-running\n",
        })).unwrap()),
    }).await.expect("background command did not retain stdin");
    assert!(!input.is_error.unwrap_or(false));
    for _ in 0..50 {
        let status = client.peer().call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal_job_status"),
            arguments: Some(serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                "job_id": job_id,
                "incremental": false,
            })).unwrap()),
        }).await.expect("job status failed");
        let status = json_of(status);
        if status["status"] == json!("Completed") {
            assert!(
                status["output"].as_str().unwrap_or_default().contains("received:still-running"),
                "command lost its input: {status}"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("background command did not finish after stdin");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_request_terminates_foreground_job() {
    use rmcp::model::{ClientRequest, Request};
    use rmcp::service::PeerRequestOptions;

    let client = connect_child_client().await;
    let command = if cfg!(windows) {
        "Write-Output 'cancel-ready'; $line = Read-Host"
    } else {
        "printf 'cancel-ready\n'; IFS= read -r line"
    };
    let handle = client.peer().send_cancellable_request(
        ClientRequest::CallToolRequest(Request::new(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal"),
            arguments: Some(serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                "command": command,
                "shell": if cfg!(windows) { "pwsh" } else { "bash" },
                "force_sync": true,
                "tags": ["request-cancel-regression"],
            })).unwrap()),
        })),
        PeerRequestOptions::no_options(),
    ).await.expect("terminal request failed");
    let mut job_id = None;
    for _ in 0..50 {
        let jobs = client.peer().call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal_job_list"),
            arguments: Some(serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                "tag_filter": "request-cancel-regression",
            })).unwrap()),
        }).await.expect("job list failed");
        let jobs = json_of(jobs);
        if let Some(id) = jobs.as_array().and_then(|entries| entries.first())
            .and_then(|entry| entry["job_id"].as_str())
        {
            job_id = Some(id.to_string());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let job_id = job_id.expect("terminal did not register its job");
    handle.cancel(Some("regression-test".to_string())).await.expect("request cancellation failed");
    let mut last_status = Value::Null;
    for _ in 0..50 {
        let status = client.peer().call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal_job_status"),
            arguments: Some(serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                "job_id": job_id,
                "incremental": false,
            })).unwrap()),
        }).await.expect("job status failed");
        last_status = json_of(status);
        if last_status["status"] == json!("Canceled") {
            let pid = last_status["pid"].as_u64().expect("job PID missing");
            #[cfg(windows)]
            {
                let status = std::process::Command::new("pwsh")
                    .args(["-NoLogo", "-NoProfile", "-Command", &format!("if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 1 }}")])
                    .status().expect("process check failed");
                assert!(status.success(), "canceled job process {pid} is still running");
            }
            #[cfg(not(windows))]
            {
                let status = std::process::Command::new("kill").args(["-0", pid]).status().unwrap();
                assert!(!status.success(), "canceled job process {pid} is still running");
            }
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Do not leave a failed regression's process behind.
    let _ = client.peer().call_tool(CallToolRequestParam {
        name: Cow::Borrowed("enhanced_terminal_job_cancel"),
        arguments: Some(serde_json::from_value::<serde_json::Map<String, Value>>(json!({"job_id": job_id})).unwrap()),
    }).await;
    panic!("request cancellation left the job running: {last_status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_stdin_writes_to_running_async_job() {
    let client = connect_child_client().await;
    let input = "hello from stdin\n";
    // Windows ConPTY needs CRLF, so the reported byte count grows by one per newline.
    let expected_bytes = if cfg!(windows) {
        input.len() + input.matches('\n').count()
    } else {
        input.len()
    };

    for shell in test_shells() {
        let run = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "command": shell.read_stdin_then_echo("got:"),
                        "cwd": ".",
                        "shell": shell.name(),
                        "force_async": true,
                        "preview_tokens": 0
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call enhanced_terminal failed");

        let run_json = json_of(run);
        assert_eq!(
            run_json["switched_to_async"],
            json!(true),
            "expected async job for {}: {run_json}",
            shell.name()
        );
        let job_id = run_json["job_id"]
            .as_str()
            .expect("missing job id")
            .to_string();

        let stdin = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal_job_stdin"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "job_id": job_id,
                        "input": input
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call enhanced_terminal_job_stdin failed");

        let stdin_json = json_of(stdin);
        assert_eq!(
            stdin_json["bytes_written"],
            json!(expected_bytes),
            "unexpected stdin confirmation for {}: {stdin_json}",
            shell.name()
        );

        let mut last_status = Value::Null;
        let mut consumed = false;
        for _ in 0..50 {
            let status = client
                .peer()
                .call_tool(CallToolRequestParam {
                    name: Cow::Borrowed("enhanced_terminal_job_status"),
                    arguments: Some(
                        serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                            "job_id": job_id,
                            "incremental": false,
                            "preview_tokens": 0
                        }))
                        .expect("tool arguments must be a JSON object")
                        .into_iter()
                        .collect(),
                    ),
                })
                .await
                .expect("tools/call enhanced_terminal_job_status failed");

            last_status = json_of(status);
            if last_status["status"] == json!("Completed")
                && last_status["output"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("got:hello from stdin")
            {
                consumed = true;
                break;
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        assert!(
            consumed,
            "{} job never consumed stdin; last status: {last_status}",
            shell.name()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enhanced_terminal_preview_tokens_truncates_output() {
    let client = connect_child_client().await;
    // The PTY transcript starts with terminal setup escapes, so the budget has
    // to be large enough to reach the payload; the marker at the end still
    // falls outside it.
    let filler = (0..300)
        .map(|index| format!("w{index}"))
        .collect::<Vec<_>>()
        .join(" ");

    for shell in test_shells() {
        let command = shell.echo(&format!("preview-head {filler} preview-tail"));
        let res = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "command": command,
                        "cwd": ".",
                        "shell": shell.name(),
                        "force_sync": true,
                        "preview_tokens": 120
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call enhanced_terminal failed");

        let json = json_of(res);
        let output = json["output"].as_str().unwrap_or_default();
        assert_eq!(
            json["exit_code"],
            json!(0),
            "command should succeed for {}: {json}",
            shell.name()
        );
        assert!(
            output.contains("preview-head"),
            "preview should keep the head for {}: {json}",
            shell.name()
        );
        assert!(
            !output.contains("preview-tail"),
            "preview was not token-truncated for {}: {json}",
            shell.name()
        );
        assert_eq!(
            json["truncated"],
            json!(true),
            "missing truncation flag for {}: {json}",
            shell.name()
        );

        let full = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal_job_status"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "job_id": json["job_id"],
                        "incremental": false,
                        "preview_tokens": 0
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call enhanced_terminal_job_status failed");

        let full = json_of(full);
        assert!(
            full["output"]
                .as_str()
                .unwrap_or_default()
                .contains("preview-tail"),
            "full transcript should still hold the tail for {}: {full}",
            shell.name()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_status_hides_full_command_by_default() {
    let client = connect_child_client().await;
    let long_marker = "unique-long-command-marker-for-full-command-test";
    let long_prefix = "x".repeat(140);

    for shell in test_shells() {
        let command = shell.echo_with_comment("ok", &format!("{long_prefix} {long_marker}"));

        let run = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "command": command,
                        "cwd": ".",
                        "shell": shell.name(),
                        "force_sync": true,
                        "preview_tokens": 0
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call enhanced_terminal failed");

        let run_json = json_of(run);
        let job_id = run_json["job_id"]
            .as_str()
            .expect("missing job id")
            .to_string();

        let compact = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal_job_status"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "job_id": job_id,
                        "incremental": false,
                        "preview_tokens": 0
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call compact job_status failed");
        let compact_json = json_of(compact);
        assert!(
            compact_json["summary"].is_string(),
            "missing summary for {}: {compact_json}",
            shell.name()
        );
        assert!(
            compact_json["command"].is_null(),
            "full command should be hidden by default for {}: {compact_json}",
            shell.name()
        );
        assert!(
            !compact_json["summary"]
                .as_str()
                .unwrap_or_default()
                .contains(long_marker),
            "summary should stay truncated for {}: {compact_json}",
            shell.name()
        );

        let verbose = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal_job_status"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "job_id": job_id,
                        "incremental": false,
                        "preview_tokens": 0,
                        "full_command": true
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call verbose job_status failed");
        let verbose_json = json_of(verbose);
        assert!(
            verbose_json["command"]
                .as_str()
                .unwrap_or_default()
                .contains(long_marker),
            "missing full command for {}: {verbose_json}",
            shell.name()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_status_uses_byte_explicit_pagination() {
    let client = connect_child_client().await;

    for shell in test_shells() {
        let command = shell.echo("abcdefghij");
        let run = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "command": command,
                        "cwd": ".",
                        "shell": shell.name(),
                        "force_sync": true,
                        "preview_tokens": 0
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call enhanced_terminal failed");

        let run_json = json_of(run);
        let job_id = run_json["job_id"]
            .as_str()
            .expect("missing job id")
            .to_string();
        let total_bytes = run_json["output"]
            .as_str()
            .expect("output should be a string")
            .len();
        assert!(
            total_bytes > 10,
            "transcript should hold more than the marker for {}: {run_json}",
            shell.name()
        );

        let status = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal_job_status"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "job_id": job_id,
                        "offset_bytes": 0,
                        "limit_bytes": 4,
                        "preview_tokens": 0
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call enhanced_terminal_job_status failed");

        let status = json_of(status);
        assert!(
            status["read"]["mode"] == json!("explicit"),
            "missing byte-explicit mode for {}: {status}",
            shell.name()
        );
        assert_eq!(
            status["read"]["byte_start"],
            json!(0),
            "wrong byte start for {}: {status}",
            shell.name()
        );
        assert_eq!(
            status["read"]["byte_end"],
            json!(4),
            "wrong byte end for {}: {status}",
            shell.name()
        );
        assert_eq!(
            status["read"]["next_offset_bytes"],
            json!(4),
            "missing next byte offset for {}: {status}",
            shell.name()
        );
        assert_eq!(
            status["output"].as_str().unwrap_or_default().len(),
            4,
            "page should return exactly the requested bytes for {}: {status}",
            shell.name()
        );
        assert_eq!(
            status["read"]["total_bytes"],
            json!(total_bytes),
            "page should report the whole transcript size for {}: {status}",
            shell.name()
        );

        let whole = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: Cow::Borrowed("enhanced_terminal_job_status"),
                arguments: Some(
                    serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                        "job_id": job_id,
                        "offset_bytes": 0,
                        "limit_bytes": total_bytes,
                        "preview_tokens": 0
                    }))
                    .expect("tool arguments must be a JSON object")
                    .into_iter()
                    .collect(),
                ),
            })
            .await
            .expect("tools/call enhanced_terminal_job_status failed");

        let whole = json_of(whole);
        assert!(
            whole["output"]
                .as_str()
                .unwrap_or_default()
                .contains("abcdefghij"),
            "whole transcript should hold the command output for {}: {whole}",
            shell.name()
        );
    }
}

/// This test is opt-in because it may pop a GUI askpass prompt and requires a working desktop session.
///
/// Enable by setting:
/// - ENHANCED_TERMINAL_MCP_TEST_SUDO=1
/// Optionally override askpass used during priming:
/// - ENHANCED_TERMINAL_MCP_TEST_ASKPASS=/home/you/scripts/askpass-zenity.sh
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sudo_prime_then_cached_sudo_n_opt_in() {
    let enabled = std::env::var("ENHANCED_TERMINAL_MCP_TEST_SUDO")
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);

    if !enabled {
        return;
    }

    let askpass = std::env::var("ENHANCED_TERMINAL_MCP_TEST_ASKPASS").unwrap_or_else(|_| {
        format!(
            "{}/scripts/askpass-zenity.sh",
            std::env::var("HOME").unwrap_or_default()
        )
    });

    let client = connect_child_client().await;

    // Prime sudo once using askpass, then run a sudo -n command.
    let first = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "command": format!("SUDO_ASKPASS={} sudo -A -v && sudo -n ls /", askpass),
                    "cwd": ".",
                    "shell": "bash",
                    "force_sync": true
                }))
                .expect("tool arguments must be a JSON object")
                .into_iter()
                .collect(),
            ),
        })
        .await
        .expect("prime+sudo command failed");

    let first_text = text_from_calltool(first);
    assert!(
        first_text.contains("bin") || first_text.contains("usr") || first_text.contains("etc"),
        "expected sudo output after priming; got: {first_text}"
    );

    // Give the server a moment; timestamp should now be cached in the server context.
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Subsequent non-interactive sudo should succeed without prompting.
    let second = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "command": "sudo -n ls /",
                    "cwd": ".",
                    "shell": "bash",
                    "force_sync": true
                }))
                .expect("tool arguments must be a JSON object")
                .into_iter()
                .collect(),
            ),
        })
        .await
        .expect("cached sudo -n command failed");

    let second_text = text_from_calltool(second);
    assert!(
        second_text.contains("bin") || second_text.contains("usr") || second_text.contains("etc"),
        "expected cached sudo to work; got: {second_text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_wait_returns_settled_job_with_json_payload() {
    let client = connect_child_client().await;

    let run = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "command": "echo start; sleep 2; echo done",
                    "cwd": ".",
                    "force_async": true,
                    "preview_tokens": 0
                }))
                .expect("tool arguments must be a JSON object")
                .into_iter()
                .collect(),
            ),
        })
        .await
        .expect("tools/call enhanced_terminal failed");

    let run_text = text_from_calltool(run);
    let run_json: Value = serde_json::from_str(&run_text)
        .unwrap_or_else(|error| panic!("execution result was not JSON ({error}): {run_text}"));
    let job_id = run_json["job_id"]
        .as_str()
        .expect("missing job_id in JSON execution result")
        .to_string();
    assert_eq!(
        run_json["switched_to_async"],
        json!(true),
        "expected a background job: {run_json}"
    );

    let waited_started = std::time::Instant::now();
    let waited = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal_job_wait"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "job_ids": [job_id],
                    "pattern": "done",
                    "timeout_ms": 20000,
                    "tail_lines": 5
                }))
                .expect("tool arguments must be a JSON object")
                .into_iter()
                .collect(),
            ),
        })
        .await
        .expect("tools/call enhanced_terminal_job_wait failed");

    let waited_text = text_from_calltool(waited);
    let waited_json: Value = serde_json::from_str(&waited_text)
        .unwrap_or_else(|error| panic!("wait result was not JSON ({error}): {waited_text}"));
    assert_eq!(
        waited_json["timed_out"],
        json!(false),
        "wait timed out before the pattern appeared: {waited_json}"
    );
    let entry = &waited_json["jobs"][0];
    assert_eq!(
        entry["pattern_matched"],
        json!(true),
        "pattern was not reported as matched: {waited_json}"
    );
    assert!(
        entry["output_tail"]
            .as_str()
            .unwrap_or_default()
            .contains("done"),
        "missing trailing output: {waited_json}"
    );
    assert!(
        waited_started.elapsed() >= Duration::from_millis(500),
        "wait returned before the job produced output: {waited_json}"
    );

    // A matching pattern ends that job's wait early, so a second wait without
    // one must still observe the process exiting.
    let settled = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal_job_wait"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "job_ids": [job_id],
                    "timeout_ms": 20000
                }))
                .expect("tool arguments must be a JSON object")
                .into_iter()
                .collect(),
            ),
        })
        .await
        .expect("tools/call enhanced_terminal_job_wait failed");

    let settled_json: Value = serde_json::from_str(&text_from_calltool(settled))
        .unwrap_or_else(|error| panic!("second wait result was not JSON ({error})"));
    let settled_entry = &settled_json["jobs"][0];
    assert_eq!(
        settled_entry["settled"],
        json!(true),
        "job did not settle: {settled_json}"
    );
    assert_eq!(
        settled_entry["status"],
        json!("Completed"),
        "unexpected final status: {settled_json}"
    );
}
