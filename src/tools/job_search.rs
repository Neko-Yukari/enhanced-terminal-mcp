//! Search one immutable snapshot of one job's stored UTF-8 PTY transcript.
use super::output_view::line_starts;
use super::job_manager::JobManager;
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Command;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode { #[default] Literal, Regex, Semantic }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobSearchInput {
    pub job_id: String,
    pub query: String,
    /// literal (rg fixed string), regex (rg), or semantic (local sgrep text model).
    #[serde(default)]
    pub mode: SearchMode,
    /// Applies to rg modes. Semantic search does not use casing rules.
    #[serde(default)]
    pub case_sensitive: bool,
    /// Maximum matches, 1..50. Results indicate whether more matches exist.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Surrounding transcript lines to include on each side.
    #[serde(default = "default_context")]
    pub context_lines: usize,
    /// Explicit maximum context bytes per result; 0 disables this excerpt limit.
    /// This never alters the stored transcript or the terminal preview budget.
    #[serde(default = "default_context_bytes")]
    pub context_bytes: usize,
}
fn default_limit() -> usize { 10 }
fn default_context() -> usize { 2 }
fn default_context_bytes() -> usize { 4096 }

struct SnapshotFile { directory: PathBuf, file: PathBuf }
impl SnapshotFile {
    fn create(text: &str) -> Result<Self> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory = std::env::temp_dir().join(format!("enhanced-terminal-search-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&directory)?;
        let snapshot = Self { file: directory.join("output.txt"), directory };
        std::fs::write(&snapshot.file, text)?;
        Ok(snapshot)
    }
}
impl Drop for SnapshotFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

fn binary(name: &str) -> PathBuf {
    let key = format!("ENHANCED_TERMINAL_{}_BIN", name.to_ascii_uppercase());
    if let Some(path) = std::env::var_os(key) { return path.into(); }
    let executable = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        for directory in [".cache/opencode/bin", ".local/share/opencode/bin", ".cargo/bin", ".local/bin"] {
            let path = Path::new(&home).join(directory).join(&executable);
            if path.is_file() { return path; }
        }
    }
    executable.into()
}

async fn run(mut command: Command) -> Result<Vec<u8>> {
    command.kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW; no shell/shim.
    let output = tokio::time::timeout(Duration::from_secs(60), command.output()).await
        .context("Search engine timed out")??;
    if output.status.code() == Some(1) { return Ok(Vec::new()); }
    if !output.status.success() {
        bail!("Search engine failed ({}): {}", output.status, String::from_utf8_lossy(&output.stderr));
    }
    Ok(output.stdout)
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) { at -= 1; }
    at
}

fn hit(text: &str, lines: &[usize], input: &JobSearchInput, start: usize, end: usize, first: usize, last: usize) -> Value {
    let lo = first.saturating_sub(1).saturating_sub(input.context_lines);
    let hi = last.saturating_add(input.context_lines).min(lines.len());
    let mut context_start = lines[lo];
    let full_end = lines.get(hi).copied().unwrap_or(text.len());
    let mut context_end = full_end;
    if input.context_bytes > 0 && context_end - context_start > input.context_bytes {
        context_start = floor_boundary(text, start.saturating_sub(input.context_bytes.saturating_sub(end - start) / 2).max(context_start));
        context_end = floor_boundary(text, context_start.saturating_add(input.context_bytes).min(full_end));
        if context_end == context_start && context_start < text.len() {
            context_end += text[context_start..].chars().next().unwrap().len_utf8();
        }
    }
    json!({
        "line_start": first, "line_end": last, "byte_base": 0, "byte_end_exclusive": true, "line_base": 1,
        "match_start_byte": start, "match_end_byte": end,
        "context": { "start_byte": context_start, "end_byte": context_end,
            "limited": context_start != lines[lo] || context_end != full_end,
            "text": &text[context_start..context_end] },
        "readback": { "tool": "enhanced_terminal_job_status", "arguments": {
            "job_id": input.job_id, "offset_bytes": context_start,
            "limit_bytes": context_end - context_start, "preview_tokens": 0 } }
    })
}

pub async fn search_job(manager: &JobManager, input: &JobSearchInput) -> Result<Value> {
    if input.query.is_empty() { bail!("query cannot be empty"); }
    if !(1..=50).contains(&input.limit) { bail!("limit must be 1..50"); }
    let job = manager.get_job(&input.job_id).context("Job not found in this MCP server")?;
    let text = &job.full_output;
    let started = Instant::now();
    let lines = line_starts(text);
    let mut hits = Vec::new();
    let engine = if matches!(input.mode, SearchMode::Semantic) { "sgrep" } else { "rg" };
    let program = binary(engine);
    if !text.is_empty() {
        let snapshot = SnapshotFile::create(text)?;
        let mut command = Command::new(&program);
        command.current_dir(&snapshot.directory);
        if matches!(input.mode, SearchMode::Semantic) {
            command.args(["--json", "-n", &(input.limit + 1).to_string(), "--model", "text", "--no-cache", "--", &input.query, "."]);
            command.env("HF_HUB_OFFLINE", "1");
            let rg = binary("rg");
            if let Some(directory) = rg.parent().filter(|p| !p.as_os_str().is_empty()) {
                let mut paths = vec![directory.to_path_buf()];
                if let Some(path) = std::env::var_os("PATH") { paths.extend(std::env::split_paths(&path)); }
                command.env("PATH", std::env::join_paths(paths)?);
            }
            let output = run(command).await.context("Local semantic search failed; no literal fallback used")?;
            let found: Vec<Value> = if output.is_empty() { vec![] } else { serde_json::from_slice(&output)? };
            let expected = snapshot.file.canonicalize()?;
            for item in found.into_iter().take(input.limit + 1) {
                let path = item["path"].as_str().context("Invalid sgrep result path")?;
                if snapshot.directory.join(path).canonicalize()? != expected { bail!("sgrep returned another source"); }
                let first = item["start"].as_u64().context("Invalid sgrep start line")? as usize;
                let last = item["end"].as_u64().context("Invalid sgrep end line")? as usize;
                if first == 0 || last < first || last > lines.len() { bail!("sgrep returned an invalid line range"); }
                let start = lines[first - 1];
                let end = lines.get(last).copied().unwrap_or(text.len());
                hits.push(hit(text, &lines, input, start, end, first, last));
            }
        } else {
            command.args(["--json", "--text", "--line-number", "--color", "never", "--max-count", &(input.limit + 1).to_string()]);
            if matches!(input.mode, SearchMode::Literal) { command.arg("--fixed-strings"); }
            if !input.case_sensitive { command.arg("--ignore-case"); }
            command.args(["--", &input.query, "output.txt"]);
            let output = run(command).await.context("Exact search failed")?;
            for record in output.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
                let event: Value = serde_json::from_slice(record)?;
                if event["type"] != "match" { continue; }
                let data = &event["data"];
                let line = data["line_number"].as_u64().context("Invalid rg line")? as usize;
                let offset = data["absolute_offset"].as_u64().context("Invalid rg byte offset")? as usize;
                for found in data["submatches"].as_array().context("Invalid rg matches")? {
                    let start = offset + found["start"].as_u64().context("Invalid match start")? as usize;
                    let end = offset + found["end"].as_u64().context("Invalid match end")? as usize;
                    if line == 0 || line > lines.len() || end > text.len() || start > end || !text.is_char_boundary(start) || !text.is_char_boundary(end) { bail!("rg returned an invalid source range"); }
                    hits.push(hit(text, &lines, input, start, end, line, line));
                    if hits.len() > input.limit { break; }
                }
                if hits.len() > input.limit { break; }
            }
        }
    }
    let has_more = hits.len() > input.limit;
    hits.truncate(input.limit);
    Ok(json!({
        "job_id": input.job_id, "mode": input.mode, "engine": engine,
        "view": "stored_utf8_raw_pty_transcript",
        "view_note": "No ANSI stripping or screen reconstruction. Repaints may repeat text; this is not clean stdout. Byte offsets refer to the stored UTF-8 transcript before JSON escaping.",
        "snapshot_bytes": text.len(), "job_status": job.status, "line_origin": "1-based LF-delimited transcript lines; CRLF is one break, cursor motion is not interpreted",
        "limit": input.limit, "has_more": has_more, "hits": hits,
        "elapsed_ms": started.elapsed().as_millis(),
        "readback_note": "Use the explicit job_status byte range with preview_tokens=0. Explicit pages do not advance the incremental cursor. Search uses only this job snapshot; semantic mode uses the existing local model offline without a persistent index."
    }))
}
