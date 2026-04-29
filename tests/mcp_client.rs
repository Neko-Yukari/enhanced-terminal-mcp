use std::borrow::Cow;
use std::process::Stdio;
use std::time::Duration;

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
    let mut child = Command::new(env!("CARGO_BIN_EXE_enhanced-terminal-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
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
    assert!(names.iter().any(|n| n == "detect_binaries"));
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
