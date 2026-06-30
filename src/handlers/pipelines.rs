//! The operator console + the SSO-gated create-pipeline / trigger-run / view-run flow.
//!
//! The console (`GET /`) lists pipelines and recent runs (status pills) and carries the
//! create-pipeline form. Authoring (`POST /api/pipelines`, `POST /api/pipelines/{id}/run`) is
//! mounted behind the gateway `auth=sso` route: the operator identity is ALWAYS taken from the
//! injected `X-Auth-Subject` / `X-Auth-Email` (never a client field), and every state-changing POST
//! is double-submit CSRF protected. Every producer-supplied string is HTML-escaped on render.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Form;
use serde::Deserialize;

use crate::auth;
use crate::config::{MAX_NAME_CHARS, RUN_LIST_LIMIT};
use crate::error::AppError;
use crate::handlers::{
    esc, fmt_duration, fmt_ts, html_with_cookie, redirect, status_pill, topbar, APP_CSS,
};
use crate::runner::{steps_of, STATUS_QUEUED};
use crate::store::{Pipeline, Run};
use crate::{new_id, now_secs, AppState};

const CONSOLE_HTML: &str = include_str!("../../templates/console.html");
const RUN_HTML: &str = include_str!("../../templates/run.html");

/// Create-pipeline form body. Identity is NEVER taken from the form — only the gateway headers.
#[derive(Debug, Deserialize)]
pub struct PipelineForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub repo_url: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub steps: String,
    #[serde(default)]
    pub csrf_token: String,
}

/// Trigger-run form body: just the CSRF token (the pipeline id is in the path).
#[derive(Debug, Deserialize)]
pub struct RunForm {
    #[serde(default)]
    pub csrf_token: String,
}

// ---------------------------------------------------------------------------
// GET / — console
// ---------------------------------------------------------------------------

/// `GET /` — the operator console: pipelines, recent runs (status pills), create-pipeline form.
pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let email = auth::display_email(&headers);
    let (csrf, set_cookie) = auth::ensure_csrf(&headers);

    let pipelines = state.store.list_pipelines().await;
    let runs = state.store.list_recent_runs(RUN_LIST_LIMIT).await;

    let pipelines_html = render_pipelines(&pipelines, &csrf);
    let runs_html = render_runs(&runs, &pipelines);

    let page = CONSOLE_HTML
        .replace("{{CSS}}", APP_CSS)
        .replace("{{TOPBAR}}", &topbar("Console", &email))
        .replace("{{CSRF}}", &esc(&csrf))
        .replace("{{PIPELINES}}", &pipelines_html)
        .replace("{{RUNS}}", &runs_html);
    html_with_cookie(page, set_cookie)
}

// ---------------------------------------------------------------------------
// POST /api/pipelines — create a pipeline
// ---------------------------------------------------------------------------

/// `POST /api/pipelines` — define a pipeline (operator from the injected `X-Auth-*`), then 303 `/`.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<PipelineForm>,
) -> Result<Response, AppError> {
    let (sub, email) = auth::require_operator(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let name = form.name.trim();
    if name.is_empty() {
        return Err(AppError::InvalidRequest("pipeline name is required".to_string()));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(AppError::InvalidRequest("pipeline name is too long".to_string()));
    }
    let repo_url = form.repo_url.trim();
    validate_repo_url(repo_url)?;
    let branch = normalize_branch(&form.branch);
    validate_branch(&branch)?;

    let pipeline = Pipeline {
        id: new_id("pl"),
        name: name.to_string(),
        repo_url: repo_url.to_string(),
        branch,
        // Stored verbatim (newline-separated); the runner splits + trims at execution time.
        steps: form.steps.replace('\r', ""),
        created_at: now_secs(),
    };
    state.store.create_pipeline(&pipeline).await?;
    tracing::info!(pipeline = %pipeline.id, actor = %sub, email = %email, "pipeline created");

    Ok(redirect("/"))
}

// ---------------------------------------------------------------------------
// POST /api/pipelines/{id}/run — enqueue a run
// ---------------------------------------------------------------------------

/// `POST /api/pipelines/{id}/run` — enqueue a run, hand it to the scheduler, 303 to the run page.
pub async fn run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RunForm>,
) -> Result<Response, AppError> {
    let (_sub, email) = auth::require_operator(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let pipeline = state
        .store
        .get_pipeline(&id)
        .await
        .ok_or_else(|| AppError::NotFound("no such pipeline".to_string()))?;

    let run = Run {
        id: new_id("run"),
        pipeline_id: pipeline.id.clone(),
        status: STATUS_QUEUED.to_string(),
        started_at: 0,
        finished_at: 0,
        exit_code: 0,
        log: String::new(),
    };
    state.store.create_run(&run).await?;
    // Hand off to the background scheduler — the request never waits on the build.
    state.runner.enqueue(run.id.clone(), pipeline, email);
    tracing::info!(run = %run.id, pipeline = %id, "run enqueued");

    Ok(redirect(&format!("/run/{}", run.id)))
}

// ---------------------------------------------------------------------------
// GET /run/{id} — the run page
// ---------------------------------------------------------------------------

/// `GET /run/{id}` — the run page: live-ish log, status, duration. While the run is not terminal the
/// page sets a short meta-refresh so the log tails without client JS.
pub async fn run_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let email = auth::display_email(&headers);
    let run = state
        .store
        .get_run(&id)
        .await
        .ok_or_else(|| AppError::NotFound("no such run".to_string()))?;
    let pipeline = state.store.get_pipeline(&run.pipeline_id).await;

    let pipeline_name = pipeline
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| run.pipeline_id.clone());

    let is_terminal = matches!(run.status.as_str(), "success" | "failed");
    // Auto-refresh while a run is queued/running so the log appears to tail.
    let refresh = if is_terminal {
        String::new()
    } else {
        r#"<meta http-equiv="refresh" content="3">"#.to_string()
    };

    let meta = format!(
        "Pipeline: {name} · Started {started} · Duration {dur} · Exit {exit}",
        name = esc(&pipeline_name),
        started = esc(&fmt_ts(run.started_at)),
        dur = esc(&fmt_duration(run.started_at, run.finished_at)),
        exit = run.exit_code,
    );

    let log = if run.log.is_empty() {
        "anvil: waiting for a runner slot…".to_string()
    } else {
        run.log.clone()
    };

    let page = RUN_HTML
        .replace("{{CSS}}", APP_CSS)
        .replace("{{REFRESH}}", &refresh)
        .replace("{{TOPBAR}}", &topbar("Run", &email))
        .replace("{{TITLE}}", &esc(&pipeline_name))
        .replace("{{RUN_ID}}", &esc(&run.id))
        .replace("{{STATUS}}", &status_pill(&run.status))
        .replace("{{META}}", &meta)
        .replace("{{LOG}}", &esc(&log));
    Ok(html_with_cookie(page, None))
}

// ---------------------------------------------------------------------------
// Render helpers
// ---------------------------------------------------------------------------

/// Render the pipeline list: each card shows name, repo@branch, step count, and a Run button (a
/// CSRF-protected POST form).
fn render_pipelines(pipelines: &[Pipeline], csrf: &str) -> String {
    if pipelines.is_empty() {
        return r#"<div class="empty-state"><h2>No pipelines yet</h2><p>Define one below: a repo URL and the shell steps to run.</p></div>"#.to_string();
    }
    let mut out = String::new();
    for p in pipelines {
        let n_steps = steps_of(&p.steps).len();
        out.push_str(&format!(
            r#"<article class="card-pipeline">
  <div class="card-pipeline__main">
    <h3 class="card-pipeline__name">{name}</h3>
    <div class="card-pipeline__repo"><code>{repo}</code> <span class="branch">@ {branch}</span></div>
    <div class="card-pipeline__meta">{n} step{plural}</div>
  </div>
  <form class="inline-form" method="post" action="/api/pipelines/{id}/run">
    <input type="hidden" name="csrf_token" value="{csrf}">
    <button class="btn btn-primary btn-sm" type="submit">Run</button>
  </form>
</article>"#,
            name = esc(&p.name),
            repo = esc(&p.repo_url),
            branch = esc(&p.branch),
            n = n_steps,
            plural = if n_steps == 1 { "" } else { "s" },
            id = esc(&p.id),
            csrf = esc(csrf),
        ));
    }
    out
}

/// Render the recent-run rows: status pill, pipeline name (linked to the run page), start + duration.
fn render_runs(runs: &[Run], pipelines: &[Pipeline]) -> String {
    if runs.is_empty() {
        return r#"<tr><td colspan="4" class="runs__empty">No runs yet.</td></tr>"#.to_string();
    }
    let mut out = String::new();
    for r in runs {
        let name = pipelines
            .iter()
            .find(|p| p.id == r.pipeline_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| r.pipeline_id.clone());
        out.push_str(&format!(
            r#"<tr>
  <td>{pill}</td>
  <td><a href="/run/{id}">{name}</a></td>
  <td class="muted">{started}</td>
  <td class="muted">{dur}</td>
</tr>"#,
            pill = status_pill(&r.status),
            id = esc(&r.id),
            name = esc(&name),
            started = esc(&fmt_ts(r.started_at)),
            dur = esc(&fmt_duration(r.started_at, r.finished_at)),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Accept only an `http(s)://` clone URL. Rejects empty, `file://`, `ssh://`, scp-style, and other
/// schemes — Anvil clones internal repos (e.g. Loom) over HTTP and must never read a local path.
fn validate_repo_url(url: &str) -> Result<(), AppError> {
    if url.is_empty() {
        return Err(AppError::InvalidRequest("repo URL is required".to_string()));
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(
            "repo URL must start with http:// or https://".to_string(),
        ))
    }
}

/// Empty branch -> `main`; otherwise the trimmed value.
fn normalize_branch(branch: &str) -> String {
    let b = branch.trim();
    if b.is_empty() {
        "main".to_string()
    } else {
        b.to_string()
    }
}

/// A branch ref is a single token of `[A-Za-z0-9._/-]`, not starting with `-` (so it is never read
/// as a git option) — rejects whitespace, control chars, and shell-y characters.
fn validate_branch(branch: &str) -> Result<(), AppError> {
    if branch.starts_with('-')
        || !branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
    {
        return Err(AppError::InvalidRequest(
            "invalid branch name (allowed: letters, digits, . _ / -)".to_string(),
        ));
    }
    Ok(())
}
