use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Job status for background command execution
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum JobStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Canceled,
}

/// Record of a background job
#[derive(Debug, Clone, Serialize)]
pub struct JobRecord {
    pub job_id: String,
    pub command: String,
    pub shell: String,
    pub cwd: String,
    pub started_at: SystemTime,
    pub finished_at: Option<SystemTime>,
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub output: String,
    pub full_output: String,
    pub truncated: bool,
    pub pid: Option<u32>,
    pub last_read_position: usize,
    /// Optional tags for categorizing jobs (e.g., ["build", "ci"])
    pub tags: Vec<String>,
    /// Command summary (first N characters)
    pub summary: String,
}

impl JobRecord {
    /// Get duration of the job (elapsed or total if finished)
    pub fn duration(&self) -> Option<Duration> {
        let end_time = self.finished_at.unwrap_or_else(SystemTime::now);
        end_time.duration_since(self.started_at).ok()
    }

    /// Get duration as formatted string
    pub fn duration_string(&self) -> String {
        self.duration()
            .map(|d| {
                if self.finished_at.is_some() {
                    format!("{:.2}s", d.as_secs_f64())
                } else {
                    format!("{:.2}s (running)", d.as_secs_f64())
                }
            })
            .unwrap_or_else(|| "unknown".to_string())
    }
}

/// Global job registry
type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub struct JobManager {
    jobs: Arc<Mutex<HashMap<String, JobRecord>>>,
    job_counter: Arc<Mutex<u64>>,
    stdin_writers: Arc<Mutex<HashMap<String, PtyWriter>>>,
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[derive(Debug, Clone)]
pub struct OutputRange {
    pub output: String,
    pub has_more: bool,
    pub total_len_bytes: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub requested_end_byte: usize,
    pub next_offset_bytes: Option<usize>,
}

const JOB_ID_ADJECTIVES: &[&str] = &[
    "amber", "ancient", "autumn", "bold", "brave", "bright", "calm", "clever", "cosmic", "crimson",
    "curious", "daring", "dusky", "eager", "frosty", "gentle", "golden", "hidden", "honest",
    "jolly", "kind", "lively", "lucky", "lunar", "merry", "misty", "nimble", "noble", "proud",
    "quiet", "rapid", "restless", "royal", "silent", "silver", "solar", "steady", "swift", "tidy",
    "vivid", "warm", "wild", "wise", "zesty",
];

const JOB_ID_NOUNS: &[&str] = &[
    "badger", "beacon", "bison", "brook", "cedar", "comet", "copper", "dawn", "dolphin", "ember",
    "falcon", "fern", "forest", "glacier", "harbor", "heron", "island", "jaguar", "lantern",
    "meadow", "meteor", "nebula", "otter", "panda", "pioneer", "quartz", "raven", "river",
    "saffron", "sparrow", "summit", "thunder", "tiger", "violet", "voyager", "willow", "zephyr",
];

fn readable_job_id(sequence: u64) -> String {
    let seed = mix_job_id_seed(sequence);
    let adjective = JOB_ID_ADJECTIVES[seed as usize % JOB_ID_ADJECTIVES.len()];
    let noun = JOB_ID_NOUNS[(seed as usize / JOB_ID_ADJECTIVES.len()) % JOB_ID_NOUNS.len()];
    format!("{adjective}-{noun}-{sequence}")
}

fn mix_job_id_seed(sequence: u64) -> u64 {
    let time_seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default();
    mix64(sequence ^ time_seed.rotate_left(17))
}

fn mix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

#[cfg(test)]
fn is_readable_job_id(id: &str) -> bool {
    let mut parts = id.split('-');
    let Some(adjective) = parts.next() else {
        return false;
    };
    let Some(noun) = parts.next() else {
        return false;
    };

    let Some(number) = parts.next() else {
        return false;
    };

    JOB_ID_ADJECTIVES.contains(&adjective)
        && JOB_ID_NOUNS.contains(&noun)
        && number.parse::<u64>().is_ok_and(|value| value > 0)
        && parts.next().is_none()
}

impl JobManager {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            job_counter: Arc::new(Mutex::new(1)),
            stdin_writers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Generate a new unique, readable job ID (adjective-noun-number).
    pub fn new_job_id(&self) -> String {
        let mut counter = self.job_counter.lock().unwrap();
        let jobs = self.jobs.lock().unwrap();

        loop {
            let sequence = *counter;
            *counter += 1;
            let candidate = readable_job_id(sequence);
            if !jobs.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    /// Register a new job
    #[allow(dead_code)]
    pub fn register_job(
        &self,
        job_id: String,
        command: String,
        shell: String,
        cwd: String,
        pid: Option<u32>,
    ) {
        self.register_job_with_tags(job_id, command, shell, cwd, pid, Vec::new());
    }

    /// Register a new job with tags
    pub fn register_job_with_tags(
        &self,
        job_id: String,
        command: String,
        shell: String,
        cwd: String,
        pid: Option<u32>,
        tags: Vec<String>,
    ) {
        let summary = if command.chars().count() > 100 {
            format!("{}...", command.chars().take(97).collect::<String>())
        } else {
            command.clone()
        };

        let mut jobs = self.jobs.lock().unwrap();
        jobs.insert(
            job_id.clone(),
            JobRecord {
                job_id,
                command,
                shell,
                cwd,
                started_at: SystemTime::now(),
                finished_at: None,
                status: JobStatus::Running,
                exit_code: None,
                output: String::new(),
                full_output: String::new(),
                truncated: false,
                pid,
                last_read_position: 0,
                tags,
                summary,
            },
        );
    }

    /// Update job with output (incremental)
    pub fn append_output(&self, job_id: &str, output: &str, output_limit: usize) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(job_id) {
            job.full_output.push_str(output);

            if job.output.len() + output.len() <= output_limit {
                job.output.push_str(output);
            } else {
                let remaining = output_limit.saturating_sub(job.output.len());
                if remaining > 0 {
                    let end = floor_char_boundary(output, remaining.min(output.len()));
                    job.output.push_str(&output[..end]);
                }
                job.truncated = true;
            }
        }
    }

    /// Attach a PTY stdin writer to a running job.
    pub fn attach_stdin_writer(&self, job_id: &str, writer: Box<dyn Write + Send>) {
        let mut writers = self.stdin_writers.lock().unwrap();
        writers.insert(job_id.to_string(), Arc::new(Mutex::new(writer)));
    }

    /// Write bytes to a running job's PTY stdin.
    pub fn write_stdin(&self, job_id: &str, input: &str) -> Result<usize> {
        {
            let jobs = self.jobs.lock().unwrap();
            let job = jobs
                .get(job_id)
                .ok_or_else(|| anyhow::anyhow!("Job not found"))?;
            if !matches!(job.status, JobStatus::Running) {
                return Err(anyhow::anyhow!("Job is not running"));
            }
        }

        let writer = {
            let writers = self.stdin_writers.lock().unwrap();
            writers
                .get(job_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Job stdin is not available"))?
        };

        let bytes = {
            // On Windows ConPTY (WIN32_INPUT_MODE) a lone \n does not submit a
            // line in Read-Host/ReadConsole — a \r is required. Normalize so callers
            // can send plain \n like on POSIX terminals.
            #[cfg(windows)]
            let input = input.replace("\r\n", "\n").replace('\n', "\r\n");
            #[cfg(not(windows))]
            let input = input.to_string();
            input.into_bytes()
        };
        let mut writer = writer.lock().unwrap();
        writer.write_all(&bytes)?;
        writer.flush()?;
        Ok(bytes.len())
    }

    /// Complete a job
    pub fn complete_job(&self, job_id: &str, exit_code: Option<i32>, status: JobStatus) {
        self.stdin_writers.lock().unwrap().remove(job_id);

        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(job_id) {
            job.finished_at = Some(SystemTime::now());
            job.exit_code = exit_code;
            job.status = status;
        }
    }

    /// Get a job by ID
    pub fn get_job(&self, job_id: &str) -> Option<JobRecord> {
        let jobs = self.jobs.lock().unwrap();
        jobs.get(job_id).cloned()
    }

    /// Get incremental output (only new since last read)
    pub fn get_incremental_output(&self, job_id: &str) -> Option<(String, bool)> {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(job_id) {
            let new_output = job.full_output[job.last_read_position..].to_string();
            job.last_read_position = job.full_output.len();
            Some((new_output, matches!(job.status, JobStatus::Running)))
        } else {
            None
        }
    }

    /// Reset read position to get all output again
    #[allow(dead_code)]
    pub fn reset_read_position(&self, job_id: &str) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(job_id) {
            job.last_read_position = 0;
        }
    }

    /// List all jobs
    pub fn list_jobs(&self) -> Vec<JobRecord> {
        let jobs = self.jobs.lock().unwrap();
        let mut job_list: Vec<JobRecord> = jobs.values().cloned().collect();
        job_list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        job_list
    }

    /// List jobs with filtering options
    pub fn list_jobs_filtered(
        &self,
        status_filter: Option<&[JobStatus]>,
        tag_filter: Option<&str>,
        cwd_filter: Option<&str>,
    ) -> Vec<JobRecord> {
        let jobs = self.jobs.lock().unwrap();
        let mut job_list: Vec<JobRecord> =
            jobs.values()
                .filter(|job| {
                    // Filter by status
                    if let Some(statuses) = status_filter {
                        if !statuses.iter().any(|s| {
                            std::mem::discriminant(s) == std::mem::discriminant(&job.status)
                        }) {
                            return false;
                        }
                    }

                    // Filter by tag
                    if let Some(tag) = tag_filter {
                        if !job.tags.iter().any(|t| t == tag) {
                            return false;
                        }
                    }

                    // Filter by cwd
                    if let Some(cwd) = cwd_filter {
                        if job.cwd != cwd {
                            return false;
                        }
                    }

                    true
                })
                .cloned()
                .collect();

        job_list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        job_list
    }

    /// Add tags to an existing job
    #[allow(dead_code)]
    pub fn add_tags(&self, job_id: &str, tags: Vec<String>) -> Result<()> {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(job_id) {
            for tag in tags {
                if !job.tags.contains(&tag) {
                    job.tags.push(tag);
                }
            }
            Ok(())
        } else {
            Err(anyhow::anyhow!("Job not found"))
        }
    }

    /// Get output with byte-explicit pagination.
    pub fn get_output_range(
        &self,
        job_id: &str,
        offset_bytes: usize,
        limit_bytes: usize,
    ) -> Option<OutputRange> {
        let jobs = self.jobs.lock().unwrap();
        let job = jobs.get(job_id)?;

        let total_len_bytes = job.full_output.len();
        let requested_end_byte = if limit_bytes == usize::MAX {
            total_len_bytes
        } else {
            offset_bytes
                .saturating_add(limit_bytes)
                .min(total_len_bytes)
        };

        let start_byte = floor_char_boundary(&job.full_output, offset_bytes);
        let end_byte = floor_char_boundary(&job.full_output, requested_end_byte);
        let output = if start_byte < total_len_bytes && start_byte <= end_byte {
            job.full_output[start_byte..end_byte].to_string()
        } else {
            String::new()
        };
        let has_more = requested_end_byte < total_len_bytes;
        let next_offset_bytes = has_more.then_some(requested_end_byte);

        Some(OutputRange {
            output,
            has_more,
            total_len_bytes,
            start_byte,
            end_byte,
            requested_end_byte,
            next_offset_bytes,
        })
    }

    /// Cancel a running job (Unix only)
    #[cfg(unix)]
    pub fn cancel_job(&self, job_id: &str) -> Result<()> {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;

        let canceled = {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(job) = jobs.get_mut(job_id) {
                if matches!(job.status, JobStatus::Running) {
                    if let Some(pid) = job.pid {
                        let pid = Pid::from_raw(pid as i32);
                        kill(pid, Signal::SIGTERM)?;
                    }

                    job.status = JobStatus::Canceled;
                    job.finished_at = Some(SystemTime::now());
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        if canceled {
            self.stdin_writers.lock().unwrap().remove(job_id);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Job not found or not running"))
        }
    }

    #[cfg(not(unix))]
    pub fn cancel_job(&self, job_id: &str) -> Result<()> {
        let canceled = {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(job) = jobs.get_mut(job_id) {
                if matches!(job.status, JobStatus::Running) {
                    job.status = JobStatus::Canceled;
                    job.finished_at = Some(SystemTime::now());
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        if canceled {
            self.stdin_writers.lock().unwrap().remove(job_id);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Job not found or not running"))
        }
    }

    /// Delete a job from history
    #[allow(dead_code)]
    pub fn delete_job(&self, job_id: &str) -> Result<()> {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.remove(job_id)
            .ok_or_else(|| anyhow::anyhow!("Job not found"))?;
        Ok(())
    }

    /// Get clone of jobs Arc for sharing
    #[allow(dead_code)]
    pub fn jobs_arc(&self) -> Arc<Mutex<HashMap<String, JobRecord>>> {
        Arc::clone(&self.jobs)
    }
}

impl Default for JobManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for JobManager {
    fn clone(&self) -> Self {
        Self {
            jobs: Arc::clone(&self.jobs),
            job_counter: Arc::clone(&self.job_counter),
            stdin_writers: Arc::clone(&self.stdin_writers),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_job_ids_are_human_friendly() {
        let manager = JobManager::new();
        let id = manager.new_job_id();

        assert!(is_readable_job_id(&id), "unexpected job id: {id}");
        assert!(
            !id.starts_with("job-"),
            "job id should not use old numeric format: {id}"
        );
    }

    #[test]
    fn readable_job_ids_are_unique_in_registry() {
        let manager = JobManager::new();
        let mut ids = std::collections::HashSet::new();

        for _ in 0..128 {
            let id = manager.new_job_id();
            assert!(ids.insert(id.clone()), "duplicate id generated: {id}");
            manager.register_job(
                id,
                "true".to_string(),
                "bash".to_string(),
                "/tmp".to_string(),
                None,
            );
        }
    }
}
