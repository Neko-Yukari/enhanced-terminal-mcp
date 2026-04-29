use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

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
pub struct JobManager {
    jobs: Arc<Mutex<HashMap<String, JobRecord>>>,
    job_counter: Arc<Mutex<u64>>,
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

impl JobManager {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            job_counter: Arc::new(Mutex::new(1)),
        }
    }

    /// Generate a new unique job ID
    pub fn new_job_id(&self) -> String {
        let mut counter = self.job_counter.lock().unwrap();
        let id = *counter;
        *counter += 1;
        format!("job-{}", id)
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

    /// Complete a job
    pub fn complete_job(&self, job_id: &str, exit_code: Option<i32>, status: JobStatus) {
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

    /// Get output with pagination (offset and limit)
    pub fn get_output_range(
        &self,
        job_id: &str,
        offset: usize,
        limit: usize,
    ) -> Option<(String, bool, usize)> {
        let jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get(job_id) {
            let total_len = job.full_output.len();
            let requested_end = offset.saturating_add(limit).min(total_len);
            let start = floor_char_boundary(&job.full_output, offset);
            let end = floor_char_boundary(&job.full_output, requested_end);
            let output_slice = if start < total_len && start <= end {
                job.full_output[start..end].to_string()
            } else {
                String::new()
            };
            let has_more = requested_end < total_len;
            Some((output_slice, has_more, total_len))
        } else {
            None
        }
    }

    /// Cancel a running job (Unix only)
    #[cfg(unix)]
    pub fn cancel_job(&self, job_id: &str) -> Result<()> {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;

        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(job_id) {
            if matches!(job.status, JobStatus::Running) {
                if let Some(pid) = job.pid {
                    let pid = Pid::from_raw(pid as i32);
                    kill(pid, Signal::SIGTERM)?;
                    job.status = JobStatus::Canceled;
                    job.finished_at = Some(SystemTime::now());
                    return Ok(());
                }
            }
        }
        Err(anyhow::anyhow!("Job not found or not running"))
    }

    #[cfg(not(unix))]
    pub fn cancel_job(&self, job_id: &str) -> Result<()> {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(job_id) {
            if matches!(job.status, JobStatus::Running) {
                job.status = JobStatus::Canceled;
                job.finished_at = Some(SystemTime::now());
                return Ok(());
            }
        }
        Err(anyhow::anyhow!("Job not found or not running"))
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
        }
    }
}
