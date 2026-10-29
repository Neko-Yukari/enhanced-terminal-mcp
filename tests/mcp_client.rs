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

    let res = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "command": "printf log-test",
                    "cwd": ".",
                    "shell": "bash",
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

    let text = text_from_calltool(res);
    assert!(text.contains("log-test"), "unexpected output: {text}");

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
    assert_eq!(entry["parameters"]["command"], "printf log-test");
    assert_eq!(entry["parameters"]["shell"], "bash");
    assert_eq!(entry["parameters"]["force_sync"], true);
    assert_eq!(entry["parameters"]["env_vars"]["LOG_TEST"], "1");
    assert_eq!(entry["parameters"]["tags"][0], "audit-test");

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

    let mut tasks = Vec::new();
    for index in 0..8 {
        let log_path_string = log_path_string.clone();
        tasks.push(tokio::spawn(async move {
            let client = connect_child_client_with_env(&[(
                "ENHANCED_TERMINAL_CALL_LOG_PATH",
                log_path_string.as_str(),
            )])
            .await;
            let marker = format!("concurrent-log-marker-{index}");
            let long_comment = "x".repeat(4096);
            let command = format!("printf {marker} # {long_comment}");

            let res = client
                .peer()
                .call_tool(CallToolRequestParam {
                    name: Cow::Borrowed("enhanced_terminal"),
                    arguments: Some(
                        serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                            "command": command,
                            "cwd": ".",
                            "shell": "bash",
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

            let text = text_from_calltool(res);
            assert!(text.contains(&marker), "unexpected output: {text}");
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
    let res = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "command": "pwd",
                    "shell": "bash",
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

    let text = text_from_calltool(res);
    assert!(
        text.contains(&format!("Working Directory: {}", expected_cwd.display())),
        "omitted cwd should use server process cwd; output was: {text}"
    );
    assert!(
        text.contains(expected_cwd.to_string_lossy().as_ref()),
        "pwd output should include server process cwd; output was: {text}"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enhanced_terminal_echo() {
    let client = connect_child_client().await;

    let res = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "command": "echo hello",
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
        .expect("tools/call enhanced_terminal failed");

    let text = text_from_calltool(res);
    assert!(text.contains("hello"), "unexpected output: {text}");
    assert!(
        text.contains("Exit Code: 0"),
        "exit code should not use Option debug formatting: {text}"
    );
    assert!(
        !text.contains("Exit Code: Some("),
        "exit code leaked Option debug formatting: {text}"
    );

    let job_id = text
        .lines()
        .find_map(|line| line.strip_prefix("Job ID: "))
        .expect("missing job id");
    let parts: Vec<&str> = job_id.split('-').collect();
    assert!(
        parts.len() == 3 && parts[2].parse::<u64>().is_ok() && !job_id.starts_with("job-"),
        "expected readable adjective-noun-number job id, got: {job_id}"
    );
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
    let text = text_from_calltool(result);
    assert!(text.contains("+after"), "diff did not reach stdout: {text}");
    assert!(!text.contains("Press RETURN") && !text.contains("TIMED OUT"), "pager blocked git diff: {text}");
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
    let text = text_from_calltool(run);
    assert!(text.contains("SWITCHED TO BACKGROUND"), "request did not release its caller: {text}");
    let job_id = text.lines().find_map(|line| line.strip_prefix("Job ID: ")).unwrap();
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
        let status = text_from_calltool(status);
        if status.contains("Status: Completed") {
            assert!(status.contains("received:still-running"), "command lost its input: {status}");
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
        let jobs = text_from_calltool(jobs);
        if let Some(id) = jobs.lines().find_map(|line| line.strip_prefix("Job ID: ")) {
            job_id = Some(id.to_string());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let job_id = job_id.expect("terminal did not register its job");
    handle.cancel(Some("regression-test".to_string())).await.expect("request cancellation failed");
    let mut last_status = String::new();
    for _ in 0..50 {
        let status = client.peer().call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal_job_status"),
            arguments: Some(serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                "job_id": job_id,
                "incremental": false,
            })).unwrap()),
        }).await.expect("job status failed");
        last_status = text_from_calltool(status);
        if last_status.contains("Status: Canceled") {
            let pid = last_status.lines().find_map(|line| line.strip_prefix("PID: "))
                .expect("job PID missing");
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

    let run = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "command": "printf 'waiting for input\n'; IFS= read -r line; printf 'got:%s\n' \"$line\"",
                    "cwd": ".",
                    "shell": "bash",
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
    assert!(
        run_text.contains("SWITCHED TO BACKGROUND"),
        "expected async job: {run_text}"
    );
    let job_id = run_text
        .lines()
        .find_map(|line| line.strip_prefix("Job ID: "))
        .expect("missing job id")
        .to_string();

    let stdin = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal_job_stdin"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "job_id": job_id,
                    "input": "hello from stdin\n"
                }))
                .expect("tool arguments must be a JSON object")
                .into_iter()
                .collect(),
            ),
        })
        .await
        .expect("tools/call enhanced_terminal_job_stdin failed");

    let stdin_text = text_from_calltool(stdin);
    assert!(
        stdin_text.contains("Wrote 17 bytes"),
        "unexpected stdin confirmation: {stdin_text}"
    );

    let mut last_status = String::new();
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

        last_status = text_from_calltool(status);
        if last_status.contains("got:hello from stdin") && last_status.contains("Status: Completed")
        {
            return;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    panic!("job never consumed stdin; last status: {last_status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enhanced_terminal_preview_tokens_truncates_output() {
    let client = connect_child_client().await;

    let res = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "command": "printf 'alpha beta gamma delta'",
                    "cwd": ".",
                    "shell": "bash",
                    "force_sync": true,
                    "preview_tokens": 2
                }))
                .expect("tool arguments must be a JSON object")
                .into_iter()
                .collect(),
            ),
        })
        .await
        .expect("tools/call enhanced_terminal failed");

    let text = text_from_calltool(res);
    assert!(text.contains("alpha beta"), "unexpected output: {text}");
    let output_section = text.split("Output:\n").nth(1).unwrap_or("");
    assert!(
        !output_section.contains("gamma"),
        "preview was not token-truncated: {text}"
    );
    assert!(
        text.contains("Output truncated"),
        "missing truncation notice: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_status_hides_full_command_by_default() {
    let client = connect_child_client().await;
    let long_marker = "unique-long-command-marker-for-full-command-test";
    let long_prefix = "x".repeat(140);
    let command = format!("printf ok # {long_prefix} {long_marker}");

    let run = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "command": command,
                    "cwd": ".",
                    "shell": "bash",
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

    let run_text = text_from_calltool(run);
    let job_id = run_text
        .lines()
        .find_map(|line| line.strip_prefix("Job ID: "))
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
    let compact_text = text_from_calltool(compact);
    assert!(
        compact_text.contains("Summary:"),
        "missing summary: {compact_text}"
    );
    assert!(
        !compact_text.contains("Command:"),
        "full command should be hidden by default: {compact_text}"
    );
    assert!(
        !compact_text.contains(long_marker),
        "long command marker leaked without full_command=true: {compact_text}"
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
    let verbose_text = text_from_calltool(verbose);
    assert!(
        verbose_text.contains("Command:"),
        "missing full command: {verbose_text}"
    );
    assert!(
        verbose_text.contains(long_marker),
        "missing long marker: {verbose_text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_status_uses_byte_explicit_pagination() {
    let client = connect_child_client().await;

    let run = client
        .peer()
        .call_tool(CallToolRequestParam {
            name: Cow::Borrowed("enhanced_terminal"),
            arguments: Some(
                serde_json::from_value::<serde_json::Map<String, Value>>(json!({
                    "command": "printf 'abcdefghij'",
                    "cwd": ".",
                    "shell": "bash",
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

    let run_text = text_from_calltool(run);
    let job_id = run_text
        .lines()
        .find_map(|line| line.strip_prefix("Job ID: "))
        .expect("missing job id")
        .to_string();

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

    let text = text_from_calltool(status);
    assert!(
        text.contains("Output Mode: Paginated (offset_bytes: 0, limit_bytes: 4)"),
        "missing byte-explicit mode: {text}"
    );
    assert!(
        text.contains("Returned Byte Range: 0..4 (requested end: 4)"),
        "missing byte range metadata: {text}"
    );
    assert!(
        text.contains("Next Offset Bytes: 4"),
        "missing next byte offset: {text}"
    );
    assert!(text.contains("Output:\nabcd"), "unexpected output: {text}");
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
