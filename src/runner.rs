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

/// One runnable step from a pipeline definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelineStep {
    pub name: String,
    pub run: String,
}

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
        for (i, step) in parse_steps(&pipeline.steps).into_iter().enumerate() {
            self.append(
                run_id,
                &format!("\nanvil: step {} - {}\n$ {}\n", i + 1, step.name, step.run),
            )
            .await;
            let res = self
                .run_command("sh", &["-c", &step.run], Some(workspace))
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
        cmd.env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
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

/// Split a pipeline's definition into runnable steps, preserving compatibility with the original
/// one-command-per-line format while supporting a small YAML steps subset:
///
/// ```text
/// steps:
///   - name: Build
///     run: cargo build
///   - name: Test
///     run: |
///       cargo test
/// ```
pub fn parse_steps(steps: &str) -> Vec<PipelineStep> {
    if looks_like_yaml_steps(steps) {
        return parse_yaml_steps(steps);
    }
    parse_legacy_steps(steps)
}

/// Return only the shell commands from [`parse_steps`].
pub fn steps_of(steps: &str) -> Vec<String> {
    parse_steps(steps).into_iter().map(|s| s.run).collect()
}

fn parse_legacy_steps(steps: &str) -> Vec<PipelineStep> {
    let mut out = Vec::new();
    for line in steps.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        out.push(PipelineStep {
            name: format!("Step {}", out.len() + 1),
            run: line.to_string(),
        });
    }
    out
}

fn looks_like_yaml_steps(steps: &str) -> bool {
    steps.lines().any(|line| {
        let t = line.trim();
        t == "steps:" || t.starts_with("- ")
    })
}

fn parse_yaml_steps(steps: &str) -> Vec<PipelineStep> {
    let lines: Vec<&str> = steps.lines().collect();
    let mut out = Vec::new();
    let mut name = String::new();
    let mut run = String::new();
    let mut open = false;
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "steps:" {
            i += 1;
            continue;
        }

        let indent = indent_width(line);
        if let Some(rest) = trimmed.strip_prefix("- ") {
            flush_step(&mut out, &mut name, &mut run);
            open = true;
            if let Some(next) =
                apply_yaml_step_field(rest, &lines, i, indent + 2, &mut name, &mut run)
            {
                i = next;
            } else if !rest.trim().is_empty() {
                run = trim_yaml_scalar(rest).to_string();
                i += 1;
            } else {
                i += 1;
            }
            continue;
        }

        if open {
            if let Some(next) =
                apply_yaml_step_field(trimmed, &lines, i, indent, &mut name, &mut run)
            {
                i = next;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    flush_step(&mut out, &mut name, &mut run);
    out
}

fn apply_yaml_step_field(
    text: &str,
    lines: &[&str],
    line_index: usize,
    parent_indent: usize,
    name: &mut String,
    run: &mut String,
) -> Option<usize> {
    let (key, value) = split_yaml_key(text)?;
    match key {
        "name" => {
            *name = trim_yaml_scalar(value).to_string();
            Some(line_index + 1)
        }
        "run" => {
            let value = value.trim();
            if value == "|" || value == ">" {
                let (block, next) = collect_block(lines, line_index + 1, parent_indent);
                *run = block;
                Some(next)
            } else {
                *run = trim_yaml_scalar(value).to_string();
                Some(line_index + 1)
            }
        }
        _ => None,
    }
}

fn split_yaml_key(text: &str) -> Option<(&str, &str)> {
    let (key, value) = text.split_once(':')?;
    let key = key.trim();
    if key == "name" || key == "run" {
        Some((key, value.trim()))
    } else {
        None
    }
}

fn trim_yaml_scalar(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn collect_block(lines: &[&str], mut i: usize, parent_indent: usize) -> (String, usize) {
    let start = i;
    while i < lines.len() {
        let line = lines[i].trim_end_matches('\r');
        if !line.trim().is_empty() && indent_width(line) <= parent_indent {
            break;
        }
        i += 1;
    }

    let block = &lines[start..i];
    let base_indent = block
        .iter()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .map(indent_width)
        .min()
        .unwrap_or(parent_indent + 2);

    let mut out = String::new();
    for line in block {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            out.push('\n');
        } else {
            out.push_str(strip_indent(line, base_indent));
            out.push('\n');
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
    (out, i)
}

fn indent_width(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

fn strip_indent(line: &str, width: usize) -> &str {
    let mut byte_idx = 0;
    let mut seen = 0;
    for (idx, ch) in line.char_indices() {
        if seen >= width || ch != ' ' {
            break;
        }
        seen += 1;
        byte_idx = idx + ch.len_utf8();
    }
    &line[byte_idx..]
}

fn flush_step(out: &mut Vec<PipelineStep>, name: &mut String, run: &mut String) {
    let command = run.trim().to_string();
    if !command.is_empty() {
        let label = if name.trim().is_empty() {
            format!("Step {}", out.len() + 1)
        } else {
            name.trim().to_string()
        };
        out.push(PipelineStep {
            name: label,
            run: command,
        });
    }
    name.clear();
    run.clear();
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
        assert_eq!(
            steps_of(blob),
            vec!["echo a".to_string(), "echo b".to_string()]
        );
        assert!(steps_of("").is_empty());
        assert!(steps_of("\n\n# only comments\n").is_empty());
    }

    #[test]
    fn yaml_steps_parse_named_and_block_runs() {
        let blob = "\
steps:
  - name: Build
    run: cargo build --release
  - name: Test
    run: |
      cargo test
      cargo clippy
";
        let steps = parse_steps(blob);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].name, "Build");
        assert_eq!(steps[0].run, "cargo build --release");
        assert_eq!(steps[1].name, "Test");
        assert_eq!(steps[1].run, "cargo test\ncargo clippy");
    }

    #[test]
    fn yaml_steps_parse_dash_run_shorthand() {
        let blob = "steps:\n  - run: echo quick\n  - cargo test\n";
        let steps = parse_steps(blob);
        assert_eq!(steps[0].name, "Step 1");
        assert_eq!(steps[0].run, "echo quick");
        assert_eq!(steps[1].name, "Step 2");
        assert_eq!(steps[1].run, "cargo test");
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
