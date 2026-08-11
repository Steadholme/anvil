//! Forge Floor 的真实性、escaping 与权限边界契约。

use anvil::config::RUN_LIST_LIMIT;
use anvil::handlers::status_pill;
use anvil::store::{Pipeline, Run};
use anvil::{app, build_dev_state, AppState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn hostile_fixtures_are_escaped_at_every_sink() {
    let state = seeded(
        "pl_hostile",
        "Forge <script>alert(\"x\")</script> & ' one",
        "https://example.invalid/r?<x>&q=\"y\"",
        "main\"><img src=x onerror=alert(1)>",
        "echo '<script>steps()</script>'",
    )
    .await;
    seed_run(
        &state,
        "run_hostile",
        "pl_hostile",
        "running",
        "stored <script>log()</script> & \"quoted\"",
        9,
        1_704_067_200,
    )
    .await;

    for path in [
        "/",
        "/pipeline/pl_hostile",
        "/pipeline/pl_hostile/edit",
        "/run/run_hostile",
    ] {
        let page = get(&state, path).await;
        let body = body_markup(&page);
        assert!(!body.contains("<script>alert"), "raw name script on {path}");
        assert!(!body.contains("<script>steps"), "raw step script on {path}");
        assert!(!body.contains("<script>log"), "raw log script on {path}");
        assert!(
            !body.contains("<img src=x"),
            "raw attribute breakout on {path}"
        );
    }

    let detail = get(&state, "/pipeline/pl_hostile").await;
    assert!(
        detail.contains("Forge &lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; &#x27; one")
    );
    assert!(detail.contains("https://example.invalid/r?&lt;x&gt;&amp;q=&quot;y&quot;"));
    assert!(detail.contains("main&quot;&gt;&lt;img src=x onerror=alert(1)&gt;"));

    let run = get(&state, "/run/run_hostile").await;
    assert!(run.contains("stored &lt;script&gt;log()&lt;/script&gt; &amp; &quot;quoted&quot;"));

    let accessible =
        "Open run run_hostile for pipeline Forge <script>alert(\"x\")</script> & ' one";
    let escaped_accessible = "Open run run_hostile for pipeline Forge &lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; &#x27; one";
    assert!(accessible.contains("<script>"));
    for page in [
        get(&state, "/").await,
        get(&state, "/pipeline/pl_hostile").await,
    ] {
        assert!(page.contains(&format!(r#"aria-label="{escaped_accessible}""#)));
        assert!(!page.contains(&format!(r#"aria-label="{accessible}""#)));
    }
}

#[tokio::test]
async fn placeholder_shaped_values_are_never_reinterpreted() {
    let marker_name = "{{RUNS}}{{STATUS}}{{STEPS}}";
    let marker_steps = "echo {{RUNS}} {{DEFINITION}}";
    let state = seeded(
        "pl_markers",
        marker_name,
        "https://example.invalid/{{CSRF}}.git",
        "{{TOPBAR}}",
        marker_steps,
    )
    .await;
    seed_run(
        &state,
        "run_markers",
        "pl_markers",
        "running",
        "{{META}} {{LOG}}",
        88,
        1_704_067_200,
    )
    .await;

    let console = get(&state, "/").await;
    let card = article_containing(&console, marker_name);
    assert!(card.contains(&format!(">{marker_name}</a>")));
    assert!(!card.contains("<tr>"));

    let detail = get(&state, "/pipeline/pl_markers").await;
    let heading = element_containing(&detail, "<h1", "</h1>", marker_name);
    assert!(heading.contains(marker_name));
    assert!(!heading.contains("<tr>"));
    assert!(detail.contains("echo {{RUNS}} {{DEFINITION}}"));

    let edit = get(&state, "/pipeline/pl_markers/edit").await;
    assert!(edit.contains(&format!(r#"value="{marker_name}""#)));
    assert!(edit.contains("https://example.invalid/{{CSRF}}.git"));
    assert!(edit.contains(r#"value="{{TOPBAR}}""#));

    let run = get(&state, "/run/run_markers").await;
    let run_heading = element_containing(&run, "<h1", "</h1>", marker_name);
    assert!(run_heading.contains(marker_name));
    assert_eq!(run_heading.matches("pill pill--running").count(), 1);
    assert!(run.contains("{{META}} {{LOG}}"));
}

#[tokio::test]
async fn secret_shaped_value_renders_verbatim_unmasked() {
    let fake_secret = "sk-test-secret-shaped-value-DO-NOT-USE";
    let state = seeded(
        "pl_secret",
        "Secret-shaped fixture",
        "https://example.invalid/repo.git",
        "main",
        fake_secret,
    )
    .await;
    seed_run(
        &state,
        "run_secret",
        "pl_secret",
        "failed",
        fake_secret,
        1,
        1_704_067_200,
    )
    .await;

    let detail = get(&state, "/pipeline/pl_secret").await;
    let run = get(&state, "/run/run_secret").await;
    assert!(detail.contains(fake_secret));
    assert!(run.contains(fake_secret));
    for page in [&detail, &run] {
        assert!(!page.contains("[REDACTED]"));
        assert!(!page.contains("••••"));
    }
}

#[tokio::test]
async fn ansi_and_control_bytes_render_inert() {
    let state = seeded(
        "pl_ansi",
        "ANSI fixture",
        "https://example.invalid/repo.git",
        "main",
        "echo ansi",
    )
    .await;
    let log = "\u{1b}[31mFAIL\u{1b}[0m\ncontrol:\u{7}\n<b>not markup</b>";
    seed_run(
        &state,
        "run_ansi",
        "pl_ansi",
        "failed",
        log,
        1,
        1_704_067_200,
    )
    .await;

    let page = get(&state, "/run/run_ansi").await;
    assert!(page.contains("\u{1b}[31mFAIL\u{1b}[0m"));
    assert!(page.contains("control:\u{7}"));
    assert!(page.contains("&lt;b&gt;not markup&lt;/b&gt;"));
    assert!(!body_markup(&page).contains("<b>not markup</b>"));
}

#[tokio::test]
async fn console_recent_window_miss_is_not_never_or_queued() {
    let state = seeded(
        "pl_target",
        "Target pipeline",
        "https://example.invalid/target.git",
        "main",
        "echo target",
    )
    .await;
    state
        .store
        .create_pipeline(&Pipeline {
            id: "pl_busy".to_string(),
            name: "Busy pipeline".to_string(),
            repo_url: "https://example.invalid/busy.git".to_string(),
            branch: "main".to_string(),
            steps: "echo busy".to_string(),
            created_at: 2,
        })
        .await
        .unwrap();
    seed_run(
        &state,
        "run_target_old",
        "pl_target",
        "queued",
        "old",
        99,
        0,
    )
    .await;
    for index in 0..RUN_LIST_LIMIT {
        seed_run(
            &state,
            &format!("run_busy_{index:04}"),
            "pl_busy",
            "success",
            "ok",
            0,
            10_000 + index as i64,
        )
        .await;
    }

    let console = get(&state, "/").await;
    let target = article_containing(&console, "Target pipeline");
    assert!(target.contains("Latest: no run in the recent window"));
    assert!(!target.contains("pill--never"));
    assert!(!target.contains("pill--queued"));

    let detail = get(&state, "/pipeline/pl_target").await;
    assert!(body_markup(&detail).contains("pill--queued"));
}

#[test]
fn status_pill_catch_all_is_unknown_neutral() {
    let pill = status_pill("supergreen-from-provider");
    assert_eq!(
        pill,
        r#"<span class="pill pill--unknown"><span class="pill__ico" aria-hidden="true">?</span><span class="sr-only">Status: </span>unknown</span>"#
    );
    assert!(!pill.contains("supergreen"));
    assert!(!pill.contains("pill--queued"));
}

#[tokio::test]
async fn warning_copy_matches_three_sink_boundary_and_truncation_truth() {
    let state = seeded(
        "pl_warning",
        "Warning contract",
        "https://example.invalid/repo.git",
        "main",
        "echo warning",
    )
    .await;
    let warning = "Plaintext warning: the repository locator, branch, and step text are stored without secret masking. Create and update audit events record the repository locator and branch. A failed-run audit event records the pipeline identifier and exit code; an enqueue audit event records only the pipeline identifier. The durable run log records the repository locator and branch, command text, and combined command output; oversized chunks can be truncated. Every operator can read that log. Never put credentials or secrets in the repository locator or step text.";

    for path in ["/", "/pipeline/pl_warning/edit"] {
        let page = normalize_whitespace(&get(&state, path).await);
        assert!(page.contains(warning), "warning drift on {path}");
    }
}

#[tokio::test]
async fn status_link_copy_is_internal_only_and_snippet_is_plain_address() {
    let state = seeded(
        "pl_badge",
        "Badge contract",
        "https://example.invalid/repo.git",
        "main",
        "echo badge",
    )
    .await;
    seed_run(
        &state,
        "run_badge",
        "pl_badge",
        "success",
        "ok",
        0,
        1_704_067_200,
    )
    .await;
    let page = get(&state, "/pipeline/pl_badge").await;
    assert!(page.contains("Internal status link. This address is available only on the private operator surface and is not an anonymous public badge."));
    assert!(page.contains(r#"src="/badge/pl_badge/status.svg""#));
    assert!(page.contains("<code>/badge/pl_badge/status.svg</code>"));
    assert!(page.contains(r#"aria-label="Open run run_badge for pipeline Badge contract""#));
    assert!(!page.contains("![Anvil status]"));
}

#[tokio::test]
async fn status_badge_catch_all_is_literal_unknown() {
    let state = seeded(
        "pl_unknown_badge",
        "Unknown badge",
        "https://example.invalid/repo.git",
        "main",
        "echo badge",
    )
    .await;
    seed_run(
        &state,
        "run_unknown_badge",
        "pl_unknown_badge",
        "provider-supergreen",
        "",
        0,
        1_704_067_200,
    )
    .await;

    let badge = get(&state, "/badge/pl_unknown_badge/status.svg").await;
    assert!(badge.contains(r#"aria-label="anvil: unknown""#));
    assert!(badge.contains(">unknown</text>"));
    assert!(!badge.contains("provider-supergreen"));
}

#[tokio::test]
async fn long_unbroken_values_keep_required_wrapping_hooks() {
    let long_name = "N".repeat(200);
    let long_locator = format!("https://example.invalid/{}", "r".repeat(4_096));
    let long_step = "x".repeat(4_096);
    let state = seeded(
        "pl_long",
        &long_name,
        &long_locator,
        "branch-with-a-very-long-unbroken-name",
        &long_step,
    )
    .await;
    seed_run(
        &state,
        "run_long",
        "pl_long",
        "failed",
        &"L".repeat(4_096),
        1,
        1_704_067_200,
    )
    .await;

    let detail = get(&state, "/pipeline/pl_long").await;
    let run = get(&state, "/run/run_long").await;
    assert!(detail.contains(&long_name));
    assert!(detail.contains(&long_locator));
    assert!(detail.contains(&long_step));
    let compact = detail.split_whitespace().collect::<String>();
    assert!(compact.contains("overflow-wrap:anywhere"));
    assert!(run.contains(&"L".repeat(4_096)));
    assert!(run.contains(r#"class="log" id="run-log""#));
}

#[tokio::test]
async fn copy_contains_no_fabricated_capability_claim() {
    let state = seeded(
        "pl_claims",
        "Claims contract",
        "https://example.invalid/repo.git",
        "main",
        "echo claims",
    )
    .await;
    seed_run(
        &state,
        "run_claims",
        "pl_claims",
        "running",
        "stored log",
        0,
        1_704_067_200,
    )
    .await;

    let pages = [
        get(&state, "/").await,
        get(&state, "/pipeline/pl_claims").await,
        get(&state, "/pipeline/pl_claims/edit").await,
        get(&state, "/run/run_claims").await,
    ];
    for page in pages {
        let copy = normalize_whitespace(body_markup(&page)).to_ascii_lowercase();
        for fabricated in [
            "runs in a sandbox",
            "secrets are masked",
            "artifacts are retained",
            "live streaming is enabled",
            "queue position",
            "per-step progress",
            "output is validated",
            "provenance verified",
            "approval required",
            "attestation verified",
        ] {
            assert!(!copy.contains(fabricated), "fabricated claim: {fabricated}");
        }
    }
}

async fn seeded(id: &str, name: &str, repo: &str, branch: &str, steps: &str) -> AppState {
    let state = build_dev_state();
    state
        .store
        .create_pipeline(&Pipeline {
            id: id.to_string(),
            name: name.to_string(),
            repo_url: repo.to_string(),
            branch: branch.to_string(),
            steps: steps.to_string(),
            created_at: 1_704_067_000,
        })
        .await
        .unwrap();
    state
}

async fn seed_run(
    state: &AppState,
    id: &str,
    pipeline_id: &str,
    status: &str,
    log: &str,
    exit_code: i64,
    started_at: i64,
) {
    state
        .store
        .create_run(&Run {
            id: id.to_string(),
            pipeline_id: pipeline_id.to_string(),
            status: status.to_string(),
            started_at,
            finished_at: if matches!(status, "success" | "failed") {
                started_at + 5
            } else {
                0
            },
            exit_code,
            log: log.to_string(),
        })
        .await
        .unwrap();
}

async fn get(state: &AppState, path: &str) -> String {
    let response = app(state.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "GET {path}");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn body_markup(page: &str) -> &str {
    page.split_once("<body")
        .map(|(_, body)| body)
        .unwrap_or(page)
}

fn article_containing<'a>(page: &'a str, needle: &str) -> &'a str {
    let needle_at = page
        .find(needle)
        .unwrap_or_else(|| panic!("missing {needle}"));
    let start = page[..needle_at].rfind("<article").expect("article start");
    let end = page[needle_at..]
        .find("</article>")
        .map(|offset| needle_at + offset + "</article>".len())
        .expect("article end");
    &page[start..end]
}

fn element_containing<'a>(page: &'a str, open: &str, close: &str, needle: &str) -> &'a str {
    let mut offset = 0;
    while let Some(relative_start) = page[offset..].find(open) {
        let start = offset + relative_start;
        let end = page[start..]
            .find(close)
            .map(|relative_end| start + relative_end + close.len())
            .expect("element end");
        let element = &page[start..end];
        if element.contains(needle) {
            return element;
        }
        offset = end;
    }
    panic!("missing {needle} in {open} element")
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
