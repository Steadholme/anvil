//! The build runner: clone a repo and execute a pipeline's declared shell steps on owned metal.
//!
//! A triggered run is enqueued (a `queued` row) and then handed to a spawned background task. The
//! task first acquires a permit from a bounded [`Semaphore`] (default 2) — extra runs stay `queued`
//! until a slot frees, so Anvil never floods the shared box. With a slot, it marks the run
//! `running`, shallow-clones `repo_url@branch` into a per-run workspace under `ANVIL_DATA/<run>`,
//! runs each step with [`tokio::process::Command`] (cwd = workspace, a minimal environment, and a
//! per-step timeout), appends the combined stdout/stderr to `runs.log`, records the exit
//! code/status, cleans the workspace, and (on failure) emits an `anvil.run.fail` audit event.
//!
//! SECURITY (v1): steps run as ordinary subprocesses INSIDE the Anvil container, isolated only by a
//! per-step timeout and the non-root container user — NOT a sandbox. Real microVM/sandbox isolation
//! is the deferred 'Crucible' hard wave. Only SSO-authenticated operators can define/trigger
//! pipelines, which is the v1 trust boundary.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::audit::{AuditEvent, AuditSink};
use crate::config::{Config, MAX_LOG_BYTES};
use crate::now_secs;
use crate::store::{Pipeline, Store};

/// Status strings persisted on a run.
pub const STATUS_QUEUED: &str = "queued";
pub const STATUS_RUNNING: &str = "running";
pub const STATUS_SUCCESS: &str = "success";
pub const STATUS_FAILED: &str = "failed";

/// Exit code stamped on a step that exceeded its timeout (matches the shell `timeout(1)` code).
const TIMEOUT_EXIT_CODE: i64 = 124;
/// Exit code stamped when a subprocess could not be spawned at all.
const SPAWN_EXIT_CODE: i64 = 127;

/// Clones the [`Store`], audit sink, and config knobs the scheduler needs; cheap to clone (all
/// behind `Arc` / cloneable handles).
#[derive(Clone)]
pub struct Runner {
    store: Arc<dyn Store>,
    audit: AuditSink,
    data_dir: String,
    git_bin: String,
    step_timeout: Duration,
    /// Bounds how many runs execute at once; extra runs wait here while `queued`.
    sem: Arc<Semaphore>,
}

impl Runner {
    pub fn new(store: Arc<dyn Store>, audit: AuditSink, config: &Config) -> Self {
        Runner {
            store,
            audit,
            data_dir: config.data_dir.clone(),
            git_bin: config.git_bin.clone(),
            step_timeout: Duration::from_secs(config.step_timeout_secs),
            sem: Arc::new(Semaphore::new(config.max_concurrent.max(1))),
        }
    }

    /// Hand a freshly-created `queued` run to a background task. Returns immediately — the request
    /// path never waits on a build. `actor` is the operator email (for the failure audit event).
    pub fn enqueue(&self, run_id: String, pipeline: Pipeline, actor: String) {
        let runner = self.clone();
        tokio::spawn(async move {
            runner.execute(run_id, pipeline, actor).await;
        });
    }

    /// Drive one run start-to-finish. Acquires a concurrency permit (held for the whole run), then
    /// clones + runs steps, updating the store as it goes. All store errors are logged and swallowed
    /// — a run never panics the scheduler.
    async fn execute(&self, run_id: String, pipeline: Pipeline, actor: String) {
        // Hold a slot for the lifetime of the run; while waiting, the run stays `queued`.
        let _permit = match self.sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                tracing::error!(run = %run_id, "run semaphore closed — aborting run");
                return;
            }
        };

        let started = now_secs();
        self.store_running(&run_id, started).await;

        let workspace = PathBuf::from(&self.data_dir).join(&run_id);
        let outcome = self.run_pipeline(&run_id, &pipeline, &workspace).await;

        // Always clean the workspace — artifacts the operator wants must be emitted by a step to an
        // external sink; v1 does not retain the working tree.
        let _ = tokio::fs::remove_dir_all(&workspace).await;

        let (status, exit_code) = outcome;
        if let Err(e) = self
            .store
            .finish_run(&run_id, status, exit_code, now_secs())
            .await
        {
            tracing::error!(run = %run_id, error = %e, "finish_run failed");
        }

        if status == STATUS_FAILED {
            // Notable event -> Watchtower. Non-blocking; a down Watchtower never affects the run.
            self.audit.emit(AuditEvent::warning(
                "anvil.run.fail",
                &actor,
                &run_id,
                &format!("pipeline={} exit={}", pipeline.id, exit_code),
            ));
        }
        tracing::info!(run = %run_id, status, exit_code, "run finished");
    }

    /// Clone the repo and run the steps; returns `(status, exit_code)`. Stops at the first failing
    /// step (exit code preserved). An empty step list with a successful clone is a success.
    async fn run_pipeline(
        &self,
        run_id: &str,
        pipeline: &Pipeline,
        workspace: &Path,
    ) -> (&'static str, i64) {
        // Fresh workspace: remove any stale dir, then recreate the data root.
        let _ = tokio::fs::remove_dir_all(workspace).await;
        if let Err(e) = tokio::fs::create_dir_all(workspace).await {
            self.append(run_id, &format!("anvil: cannot create workspace: {e}\n"))
                .await;
            return (STATUS_FAILED, SPAWN_EXIT_CODE);
        }

        // --- shallow clone --------------------------------------------------
        self.append(
            run_id,
            &format!(
                "anvil: cloning {} @ {} (shallow)\n",
                pipeline.repo_url, pipeline.branch
            ),
        )
        .await;
        let ws = workspace.to_string_lossy().to_string();
        // `--` terminates options so a repo URL / path beginning with `-` can never be read as a
        // git option (defense in depth even though only operators define pipelines).
        let clone = self
            .run_command(
                &self.git_bin,
                &[
                    "clone",
                    "--depth",
                    "1",
                    "--single-branch",
                    "--branch",
                    &pipeline.branch,
                    "--",
                    &pipeline.repo_url,
                    &ws,
                ],
                None,
            )
            .await;
        self.append(run_id, &clone.output).await;
        if clone.code != 0 {
            self.append(
                run_id,
                &format!("anvil: clone failed (exit {})\n", clone.code),
            )
            .await;
            return (STATUS_FAILED, clone.code);
        }

        // --- steps ----------------------------------------------------------
        for (i, step) in steps_of(&pipeline.steps).into_iter().enumerate() {
            self.append(run_id, &format!("\n$ {step}\n")).await;
            let res = self
                .run_command("sh", &["-c", &step], Some(workspace))
                .await;
            self.append(run_id, &res.output).await;
            if res.code != 0 {
                self.append(
                    run_id,
                    &format!("anvil: step {} failed (exit {})\n", i + 1, res.code),
                )
                .await;
                return (STATUS_FAILED, res.code);
            }
        }

        self.append(run_id, "\nanvil: all steps succeeded\n").await;
        (STATUS_SUCCESS, 0)
    }

    /// Run one subprocess with a minimal environment + the per-step timeout. Combines stdout then
    /// stderr into one log chunk. On timeout the child is killed (`kill_on_drop`) and exit code
    /// [`TIMEOUT_EXIT_CODE`] is returned; a spawn failure returns [`SPAWN_EXIT_CODE`].
    async fn run_command(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> CmdResult {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.env_clear();
        // A deliberately minimal, predictable environment. No inherited secrets leak into a build.
        cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        cmd.env("CI", "true");
        cmd.env("ANVIL", "true");
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
            cmd.env("HOME", dir);
        }
        // Avoid any interactive credential / editor prompt blocking the run.
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.env("DEBIAN_FRONTEND", "noninteractive");
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // If the timeout future is dropped, the child is killed rather than leaked.
        cmd.kill_on_drop(true);

        match tokio::time::timeout(self.step_timeout, cmd.output()).await {
            Ok(Ok(output)) => {
                let mut buf = String::from_utf8_lossy(&output.stdout).into_owned();
                if !output.stderr.is_empty() {
                    buf.push_str(&String::from_utf8_lossy(&output.stderr));
                }
                CmdResult {
                    code: output.status.code().map(|c| c as i64).unwrap_or(-1),
                    output: buf,
                }
            }
            Ok(Err(e)) => CmdResult {
                code: SPAWN_EXIT_CODE,
                output: format!("anvil: failed to spawn `{program}`: {e}\n"),
            },
            Err(_) => CmdResult {
                code: TIMEOUT_EXIT_CODE,
                output: format!(
                    "anvil: command timed out after {}s — killed\n",
                    self.step_timeout.as_secs()
                ),
            },
        }
    }

    /// Mark the run `running` (logged-and-swallowed on store error).
    async fn store_running(&self, run_id: &str, started: i64) {
        if let Err(e) = self.store.mark_run_running(run_id, started).await {
            tracing::error!(run = %run_id, error = %e, "mark_run_running failed");
        }
    }

    /// Append a log chunk, capping it so one runaway build can't grow the log without bound.
    async fn append(&self, run_id: &str, chunk: &str) {
        let chunk = cap_chunk(chunk);
        if let Err(e) = self.store.append_run_log(run_id, &chunk).await {
            tracing::error!(run = %run_id, error = %e, "append_run_log failed");
        }
    }
}

/// Result of one subprocess: its exit code + the combined stdout/stderr text.
struct CmdResult {
    code: i64,
    output: String,
}

/// Split a pipeline's `steps` blob into individual, non-empty trimmed shell commands. Blank lines
/// and `#` comment lines are skipped.
pub fn steps_of(steps: &str) -> Vec<String> {
    steps
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

/// Truncate a single log chunk to [`MAX_LOG_BYTES`] at a char boundary, appending a notice.
fn cap_chunk(chunk: &str) -> String {
    if chunk.len() <= MAX_LOG_BYTES {
        return chunk.to_string();
    }
    let mut end = MAX_LOG_BYTES;
    while end > 0 && !chunk.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…anvil: output truncated…\n", &chunk[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_skip_blanks_and_comments() {
        let blob = "echo a\n\n  # a comment\n  echo b  \n";
        assert_eq!(steps_of(blob), vec!["echo a".to_string(), "echo b".to_string()]);
        assert!(steps_of("").is_empty());
        assert!(steps_of("\n\n# only comments\n").is_empty());
    }

    #[test]
    fn cap_chunk_truncates_oversized() {
        let small = "hello";
        assert_eq!(cap_chunk(small), "hello");
        let big = "x".repeat(MAX_LOG_BYTES + 10);
        let out = cap_chunk(&big);
        assert!(out.len() <= MAX_LOG_BYTES + 40);
        assert!(out.contains("output truncated"));
    }
}
