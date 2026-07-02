//! End-to-end HTTP flow over the in-memory store (NO database).
//!
//! Drives the real `app` router via `tower::oneshot`, exactly like the rest of the estate. Covers:
//! health, empty console, the SSO/CSRF guards on authoring, create-pipeline + listing, and a full
//! triggered-run lifecycle (queued -> running -> terminal) where the clone targets a refused port,
//! so the run deterministically FAILS without any network — exercising the scheduler, the store
//! state transitions, and the log capture end-to-end.

use std::time::Duration;

use anvil::{app, build_dev_state, AppState};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

const CSRF: &str = "tok_csrf_for_tests";

#[tokio::test]
async fn full_ci_flow_in_memory() {
    let state = build_dev_state();

    // --- health ------------------------------------------------------------
    let (status, _) = call(&state, get("/healthz")).await;
    assert_eq!(status, StatusCode::OK);

    // --- empty console -----------------------------------------------------
    let (status, body) = call(&state, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("No pipelines yet"),
        "empty pipelines placeholder"
    );
    assert!(body.contains("No runs yet."), "empty runs placeholder");

    // --- GET / sets a CSRF cookie ------------------------------------------
    let resp = app(state.clone()).oneshot(get("/")).await.unwrap();
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        set_cookie.contains("__Host-csrf="),
        "GET / mints CSRF cookie"
    );

    // --- POST /api/pipelines without identity -> 401 -----------------------
    let body = form(&[
        ("name", "x"),
        ("repo_url", "https://h/r.git"),
        ("csrf_token", CSRF),
    ]);
    let (status, _) = call(&state, post_csrf("/api/pipelines", &body, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no X-Auth -> 401");

    // --- POST /api/pipelines with bad CSRF -> 401 --------------------------
    let body = form(&[
        ("name", "x"),
        ("repo_url", "https://h/r.git"),
        ("csrf_token", "WRONG"),
    ]);
    let (status, _) = call(
        &state,
        post_csrf("/api/pipelines", &body, Some(("u_op", "op@hf"))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "CSRF mismatch -> 401");

    // --- non-http repo URL rejected ----------------------------------------
    let body = form(&[
        ("name", "evil"),
        ("repo_url", "file:///etc/passwd"),
        ("csrf_token", CSRF),
    ]);
    let (status, _) = call(
        &state,
        post_csrf("/api/pipelines", &body, Some(("u_op", "op@hf"))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "file:// repo URL rejected");

    // --- create a real pipeline --------------------------------------------
    let body = form(&[
        ("name", "build-and-test"),
        ("repo_url", "http://127.0.0.1:1/nope.git"),
        ("branch", "main"),
        ("steps", "echo hello\ncargo test"),
        ("csrf_token", CSRF),
    ]);
    let resp = app(state.clone())
        .oneshot(post_csrf("/api/pipelines", &body, Some(("u_op", "op@hf"))))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp), "/", "create redirects to console");

    // --- console now lists the pipeline ------------------------------------
    let (_, body) = call(&state, get("/")).await;
    assert!(body.contains("build-and-test"), "pipeline listed");
    assert!(body.contains("2 steps"), "step count rendered");

    // Recover the pipeline id from its Run form action on the console.
    let pid = extract_pipeline_id(&body).expect("pipeline id in console");

    // --- pipeline detail + badge before any run -------------------------------
    let (status, body) = call(&state, get(&format!("/pipeline/{pid}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("Status badge"),
        "detail renders badge section"
    );
    assert!(body.contains("echo hello"), "detail renders legacy steps");

    let (status, body) = call(&state, get(&format!("/badge/{pid}/status.svg"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(">never<"),
        "badge shows never before first run"
    );

    // --- edit the pipeline to YAML-style steps --------------------------------
    let (status, body) = call(&state, get(&format!("/pipeline/{pid}/edit"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Edit pipeline"), "edit page rendered");
    assert!(body.contains(&format!(r#"action="/api/pipelines/{pid}""#)));

    let yaml_steps = "steps:\n  - name: Build\n    run: cargo build --release\n  - name: Test\n    run: |\n      cargo test\n      cargo clippy";
    let body = form(&[
        ("name", "build-and-test-updated"),
        ("repo_url", "http://127.0.0.1:1/nope.git"),
        ("branch", "main"),
        ("steps", yaml_steps),
        ("csrf_token", CSRF),
    ]);
    let resp = app(state.clone())
        .oneshot(post_csrf(
            &format!("/api/pipelines/{pid}"),
            &body,
            Some(("u_op", "op@hf")),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        location(&resp),
        format!("/pipeline/{pid}"),
        "update redirects to detail"
    );

    let (status, body) = call(&state, get(&format!("/pipeline/{pid}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("build-and-test-updated"),
        "updated name rendered"
    );
    assert!(body.contains("Build"), "YAML step name rendered");
    assert!(body.contains("cargo clippy"), "YAML block run rendered");

    // --- trigger a run -----------------------------------------------------
    let trigger = form(&[("csrf_token", CSRF)]);
    let resp = app(state.clone())
        .oneshot(post_csrf(
            &format!("/api/pipelines/{pid}/run"),
            &trigger,
            Some(("u_op", "op@hf")),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let run_loc = location(&resp);
    assert!(run_loc.starts_with("/run/run_"), "run redirect: {run_loc}");

    // --- the run reaches a terminal FAILED state (clone to a refused port) --
    // Note: the inlined CSS contains the `.pill--failed` selector on EVERY page, so terminal state
    // must be detected via the rendered pill element (`class="pill pill--failed"`), not a bare
    // substring.
    let mut terminal = false;
    for _ in 0..50 {
        let (status, body) = call(&state, get(&run_loc)).await;
        assert_eq!(status, StatusCode::OK);
        if body.contains(r#"class="pill pill--failed""#) {
            assert!(body.contains("cloning"), "log shows the clone attempt");
            assert!(body.contains("clone failed"), "log shows the clone failure");
            terminal = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        terminal,
        "run should fail fast when the clone target refuses"
    );

    let (status, body) = call(&state, get(&format!("/badge/{pid}/status.svg"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(">failed<"),
        "badge shows failed after terminal run"
    );

    // --- triggering a run for an unknown pipeline -> 404 -------------------
    let (status, _) = call(
        &state,
        post_csrf(
            "/api/pipelines/pl_missing/run",
            &trigger,
            Some(("u_op", "op@hf")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // --- unknown run id -> 404 ---------------------------------------------
    let (status, _) = call(&state, get("/run/run_missing")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // --- updating an unknown pipeline -> 404 --------------------------------
    let body = form(&[
        ("name", "missing"),
        ("repo_url", "https://h/r.git"),
        ("csrf_token", CSRF),
    ]);
    let (status, _) = call(
        &state,
        post_csrf("/api/pipelines/pl_missing", &body, Some(("u_op", "op@hf"))),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

async fn call(state: &AppState, req: Request<Body>) -> (StatusCode, String) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn location(resp: &axum::response::Response) -> String {
    resp.headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// Build a urlencoded POST carrying the test CSRF cookie + (optionally) gateway identity.
fn post_csrf(uri: &str, body: &str, ident: Option<(&str, &str)>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, format!("__Host-csrf={CSRF}"));
    if let Some((sub, email)) = ident {
        b = b
            .header("x-auth-subject", sub)
            .header("x-auth-email", email);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Minimal application/x-www-form-urlencoded value encoder.
fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                o.push(b as char)
            }
            b' ' => o.push('+'),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

/// Pull the first pipeline id out of a `/api/pipelines/{id}/run` form action in the console HTML.
fn extract_pipeline_id(html: &str) -> Option<String> {
    let marker = "/api/pipelines/";
    let start = html.find(marker)? + marker.len();
    let rest = &html[start..];
    let end = rest.find("/run")?;
    Some(rest[..end].to_string())
}
