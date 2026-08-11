//! Forge Floor 的 SSR、无 JavaScript 与可访问 DOM 契约。

use anvil::handlers::{error_page, fmt_ts, status_pill};
use anvil::store::{Pipeline, Run};
use anvil::{app, build_dev_state, AppState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

const PIPELINE_ID: &str = "pl_forge_floor";

#[tokio::test]
async fn skip_link_is_first_body_child_and_targets_main() {
    let state = fixture().await;
    for path in [
        "/",
        "/pipeline/pl_forge_floor",
        "/pipeline/pl_forge_floor/edit",
        "/run/run_running",
    ] {
        let page = get(&state, path).await;
        assert_skip_and_main(&page, path);
    }

    let error = error_page(StatusCode::NOT_FOUND, "missing");
    assert_skip_and_main(&error, "error page");
}

#[test]
fn pills_have_word_glyph_sr_name_and_state_class() {
    for (state, glyph) in [
        ("queued", "…"),
        ("running", "»"),
        ("success", "✓"),
        ("failed", "✕"),
        ("never", "–"),
        ("unknown", "?"),
    ] {
        let pill = status_pill(state);
        assert_eq!(
            pill,
            format!(
                r#"<span class="pill pill--{state}"><span class="pill__ico" aria-hidden="true">{glyph}</span><span class="sr-only">Status: </span>{state}</span>"#
            )
        );
    }
}

#[test]
fn unknown_and_never_do_not_map_to_queued() {
    let never = status_pill("never");
    assert!(never.contains("pill--never"));
    assert!(!never.contains("pill--queued"));

    let unknown = status_pill("provider-invented-state");
    assert!(unknown.contains("pill--unknown"));
    assert!(unknown.ends_with("unknown</span>"));
    assert!(!unknown.contains("provider-invented-state"));
    assert!(!unknown.contains("pill--queued"));
}

#[tokio::test]
async fn non_terminal_run_has_refresh_now_and_no_auto_refresh_directive() {
    let state = fixture().await;
    let snapshot = "This page is a snapshot from its last load. It does not update automatically and is not a live stream. Use Refresh now to poll the latest stored state.";

    for (run_id, queued) in [("run_queued", true), ("run_running", false)] {
        let page = get(&state, &format!("/run/{run_id}")).await;
        let body = body_markup(&page);
        assert!(body.contains(snapshot));
        assert!(body.contains(&format!(
            r#"<a class="btn btn-secondary btn-sm" href="/run/{run_id}">Refresh now</a>"#
        )));
        assert!(!page.to_ascii_lowercase().contains("http-equiv=\"refresh\""));
        assert!(!page.contains("{{REFRESH"));
        assert_eq!(
            body.contains("Runs execute with limited concurrency. A queued run starts when a slot becomes available."),
            queued
        );
    }
}

#[tokio::test]
async fn terminal_run_has_no_refresh_control() {
    let state = fixture().await;
    for run_id in ["run_success", "run_failed"] {
        let page = get(&state, &format!("/run/{run_id}")).await;
        let body = body_markup(&page);
        assert!(!body.contains("class=\"refresh-control\""));
        assert!(!body.contains("Refresh now"));
        assert!(!body.contains("snapshot from its last load"));
        assert!(!page.to_ascii_lowercase().contains("http-equiv=\"refresh\""));
    }
}

#[tokio::test]
async fn exit_hidden_for_non_terminal_runs() {
    let state = fixture().await;
    for run_id in ["run_queued", "run_running"] {
        let page = get(&state, &format!("/run/{run_id}")).await;
        assert!(body_markup(&page).contains("Exit —"));
        assert!(!body_markup(&page).contains("Exit 91"));
        assert!(!body_markup(&page).contains("Exit 92"));
    }

    let success = get(&state, "/run/run_success").await;
    let failed = get(&state, "/run/run_failed").await;
    assert!(body_markup(&success).contains("Exit 0"));
    assert!(body_markup(&failed).contains("Exit 17"));

    let ledger = get(&state, "/pipeline/pl_forge_floor").await;
    let queued = row_containing(&ledger, "run_queued");
    let running = row_containing(&ledger, "run_running");
    let success = row_containing(&ledger, "run_success");
    let failed = row_containing(&ledger, "run_failed");
    assert!(queued.ends_with("<td class=\"muted\">—</td>\n</tr>"));
    assert!(!queued.contains(">91</td>"));
    assert!(running.ends_with("<td class=\"muted\">—</td>\n</tr>"));
    assert!(!running.contains(">92</td>"));
    assert!(success.ends_with("<td class=\"muted\">0</td>\n</tr>"));
    assert!(failed.ends_with("<td class=\"muted\">17</td>\n</tr>"));
}

#[tokio::test]
async fn empty_terminal_log_remains_an_empty_stored_log() {
    let state = fixture().await;
    let page = get(&state, "/run/run_empty_success").await;
    let body = body_markup(&page);
    assert!(body.contains(
        r#"<pre class="log" id="run-log" role="region" aria-label="Combined stored log for this run" tabindex="0"></pre>"#
    ));
    assert!(!body.contains("waiting for a runner slot"));
}

#[tokio::test]
async fn native_mutations_keep_forms_and_csrf() {
    let state = fixture().await;
    let console = get(&state, "/").await;
    assert!(console.contains(r#"<form class="editor" method="post" action="/api/pipelines">"#));
    assert!(console.contains(r#"type="hidden" name="csrf_token""#));

    let pipeline = get(&state, "/pipeline/pl_forge_floor").await;
    assert!(pipeline.contains(
        r#"<form class="inline-form" method="post" action="/api/pipelines/pl_forge_floor/run">"#
    ));
    assert!(pipeline.contains(r#"type="hidden" name="csrf_token""#));

    let edit = get(&state, "/pipeline/pl_forge_floor/edit").await;
    assert!(edit
        .contains(r#"<form class="editor" method="post" action="/api/pipelines/pl_forge_floor">"#));
    assert!(edit.contains(r#"type="hidden" name="csrf_token""#));
}

#[tokio::test]
async fn run_action_is_oxide_while_authoring_and_back_are_neutral() {
    let state = fixture().await;
    let console = get(&state, "/").await;
    let create = opening_tag_before(&console, "Create pipeline");
    assert!(!create.contains("btn-primary"));

    let pipeline = get(&state, "/pipeline/pl_forge_floor").await;
    let run = opening_tag_before(&pipeline, ">Run</button>");
    assert!(run.contains("btn-primary"));

    let edit = get(&state, "/pipeline/pl_forge_floor/edit").await;
    let save = opening_tag_before(&edit, "Save pipeline");
    assert!(!save.contains("btn-primary"));

    let error = error_page(StatusCode::BAD_REQUEST, "bad input");
    let back = opening_tag_before(&error, "Back to the console");
    assert!(back.contains("btn-secondary"));
    assert!(!back.contains("btn-primary"));
}

#[tokio::test]
async fn trough_is_labelled_focusable_and_contains_stored_log_without_js() {
    let state = fixture().await;
    let page = get(&state, "/run/run_running").await;
    assert!(page.contains(
        "<pre class=\"log\" id=\"run-log\" role=\"region\" aria-label=\"Combined stored log for this run\" tabindex=\"0\">line &lt;one&gt;\nline two</pre>"
    ));
    assert!(!page.to_ascii_lowercase().contains("<script"));
}

#[tokio::test]
async fn ledgers_have_captions_scopes_and_labelled_scrollers() {
    let state = fixture().await;
    for path in ["/", "/pipeline/pl_forge_floor"] {
        let page = get(&state, path).await;
        let body = body_markup(&page);
        assert!(body.contains("<caption>"), "caption missing on {path}");
        assert!(
            body.contains(r#"scope="col""#),
            "column scope missing on {path}"
        );
        assert!(
            body.contains(r#"class="table-scroll""#),
            "scroller missing on {path}"
        );
        assert!(
            body.contains(r#"role="region""#),
            "region missing on {path}"
        );
        assert!(
            body.contains(r#"tabindex="0""#),
            "focus seam missing on {path}"
        );
        assert!(
            body.contains("aria-label="),
            "scroller label missing on {path}"
        );
    }
}

#[tokio::test]
async fn rendered_pages_contain_no_script_or_inline_handler() {
    let state = fixture().await;
    for path in [
        "/",
        "/pipeline/pl_forge_floor",
        "/pipeline/pl_forge_floor/edit",
        "/run/run_queued",
    ] {
        let page = get(&state, path).await;
        let lower = page.to_ascii_lowercase();
        assert!(!lower.contains("<script"), "script element on {path}");
        for handler in ["onclick=", "onload=", "onchange=", "onsubmit="] {
            assert!(
                !lower.contains(handler),
                "inline handler {handler} on {path}"
            );
        }
    }
}

#[test]
fn parsed_timestamps_render_utc_suffix() {
    let parsed = fmt_ts(1_704_067_200);
    assert_eq!(parsed, "Jan 1, 2024 00:00 UTC");
    assert_eq!(fmt_ts(0), "—");
    assert!(fmt_ts(-1).ends_with(" UTC"));

    let out_of_range = fmt_ts(i64::MAX);
    assert_eq!(out_of_range, i64::MAX.to_string());
    assert!(!out_of_range.contains("UTC"));
}

async fn fixture() -> AppState {
    let state = build_dev_state();
    state
        .store
        .create_pipeline(&Pipeline {
            id: PIPELINE_ID.to_string(),
            name: "Forge floor".to_string(),
            repo_url: "https://git.example.invalid/forge.git".to_string(),
            branch: "main".to_string(),
            steps: "echo build".to_string(),
            created_at: 1_704_067_200,
        })
        .await
        .unwrap();

    for run in [
        Run {
            id: "run_queued".to_string(),
            pipeline_id: PIPELINE_ID.to_string(),
            status: "queued".to_string(),
            started_at: 0,
            finished_at: 0,
            exit_code: 91,
            log: String::new(),
        },
        Run {
            id: "run_running".to_string(),
            pipeline_id: PIPELINE_ID.to_string(),
            status: "running".to_string(),
            started_at: 1_704_067_300,
            finished_at: 0,
            exit_code: 92,
            log: "line <one>\nline two".to_string(),
        },
        Run {
            id: "run_success".to_string(),
            pipeline_id: PIPELINE_ID.to_string(),
            status: "success".to_string(),
            started_at: 1_704_067_100,
            finished_at: 1_704_067_110,
            exit_code: 0,
            log: "ok".to_string(),
        },
        Run {
            id: "run_failed".to_string(),
            pipeline_id: PIPELINE_ID.to_string(),
            status: "failed".to_string(),
            started_at: 1_704_067_000,
            finished_at: 1_704_067_020,
            exit_code: 17,
            log: "failed".to_string(),
        },
        Run {
            id: "run_empty_success".to_string(),
            pipeline_id: PIPELINE_ID.to_string(),
            status: "success".to_string(),
            started_at: 1_704_066_900,
            finished_at: 1_704_066_901,
            exit_code: 0,
            log: String::new(),
        },
    ] {
        state.store.create_run(&run).await.unwrap();
    }
    state
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

fn assert_skip_and_main(page: &str, label: &str) {
    let body = body_markup(page);
    let after_body = body
        .split_once('>')
        .map(|(_, rest)| rest.trim_start())
        .expect("body start tag");
    let first_end = after_body.find('>').expect("first body child end");
    let first = &after_body[..=first_end];
    assert!(
        first.starts_with("<a "),
        "first body child on {label}: {first}"
    );
    assert!(
        first.contains(r#"class="skip-link""#),
        "skip class on {label}"
    );
    assert!(
        first.contains(r##"href="#main-content""##),
        "skip target on {label}"
    );

    let main_start = page.find("<main").expect("main element");
    let main_end = page[main_start..].find('>').expect("main start tag") + main_start;
    let main = &page[main_start..=main_end];
    assert!(main.contains(r#"id="main-content""#), "main id on {label}");
    assert!(
        main.contains(r#"tabindex="-1""#),
        "main focus target on {label}"
    );
}

fn opening_tag_before<'a>(page: &'a str, text: &str) -> &'a str {
    let text_at = page.find(text).unwrap_or_else(|| panic!("missing {text}"));
    let start = page[..text_at].rfind('<').expect("opening tag start");
    let end = page[start..].find('>').expect("opening tag end") + start;
    &page[start..=end]
}

fn row_containing<'a>(page: &'a str, needle: &str) -> &'a str {
    let needle_at = page
        .find(needle)
        .unwrap_or_else(|| panic!("missing row value {needle}"));
    let start = page[..needle_at].rfind("<tr>").expect("row start");
    let end = page[needle_at..]
        .find("</tr>")
        .map(|offset| needle_at + offset + "</tr>".len())
        .expect("row end");
    &page[start..end]
}
