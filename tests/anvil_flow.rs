//! End-to-end HTTP flow over the in-memory store (NO database).
//!
//! Drives the real `app` router via `tower::oneshot`, exactly like the rest of the estate. Covers:
//! health, empty console, the SSO/CSRF guards on authoring, create-pipeline + listing, and a full
//! triggered-run lifecycle (queued -> running -> terminal) where the clone targets a refused port,
//! so the run deterministically FAILS without any network — exercising the scheduler, the store
//! state transitions, and the log capture end-to-end.

use std::sync::Arc;
use std::time::Duration;

use anvil::audit::AuditSink;
use anvil::config::Config;
use anvil::runner::Runner;
use anvil::store::{InMemoryStore, Pipeline, Store};
use anvil::{app, build_dev_state, AppState};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

const CSRF: &str = "tok_csrf_for_tests";

#[tokio::test]
async fn stylesheet_is_public_and_immutable() {
    let response = app(build_dev_state())
        .oneshot(get("/assets/anvil-20260908.css"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        response
            .headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .unwrap(),
        "nosniff"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body.len() > 10_000);
}

#[tokio::test]
async fn full_ci_flow_in_memory() {
    let state = build_dev_state();

    // --- health ------------------------------------------------------------
    let (status, _) = call(&state, get("/healthz")).await;
    assert_eq!(status, StatusCode::OK);

    // --- empty console -----------------------------------------------------
    let (status, body) = call(&state, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("/assets/anvil-20260908.css"));
    assert!(!body.contains("<style>"));
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
    // Terminal state is detected via the rendered pill element (`class="pill pill--failed"`),
    // not a bare substring.
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

#[tokio::test]
async fn machine_api_matches_one_repository_and_enqueues_runs() {
    let state = machine_state();
    for (id, repo_url) in [
        ("pl_match", "http://127.0.0.1:1/acme.git"),
        ("pl_other", "http://127.0.0.1:1/other.git"),
    ] {
        state
            .store
            .create_pipeline(&Pipeline {
                id: id.to_string(),
                name: id.to_string(),
                repo_url: repo_url.to_string(),
                branch: "main".to_string(),
                steps: "echo ok".to_string(),
                created_at: 1,
            })
            .await
            .unwrap();
    }

    let unauthorized = app(state.clone())
        .oneshot(get("/api/integrations/pipelines?repo_url=http%3A%2F%2F127.0.0.1%3A1%2Facme.git&branch=main"))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let listing = app(state.clone())
        .oneshot(machine_get("/api/integrations/pipelines?repo_url=http%3A%2F%2F127.0.0.1%3A1%2Facme.git&branch=main"))
        .await
        .unwrap();
    assert_eq!(listing.status(), StatusCode::OK);
    let listing_body = axum::body::to_bytes(listing.into_body(), usize::MAX)
        .await
        .unwrap();
    let listing_json: serde_json::Value = serde_json::from_slice(&listing_body).unwrap();
    assert_eq!(listing_json["pipelines"].as_array().unwrap().len(), 1);
    assert_eq!(listing_json["pipelines"][0]["id"], "pl_match");

    let trigger = app(state.clone())
        .oneshot(machine_post(
            "/api/integrations/repositories/runs",
            r#"{"repo_url":"http://127.0.0.1:1/acme.git","branch":"main","commit_sha":"deadbeef","actor":"loom"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(trigger.status(), StatusCode::ACCEPTED);
    let trigger_body = axum::body::to_bytes(trigger.into_body(), usize::MAX)
        .await
        .unwrap();
    let trigger_json: serde_json::Value = serde_json::from_slice(&trigger_body).unwrap();
    assert_eq!(trigger_json["runs"].as_array().unwrap().len(), 1);
    assert_eq!(trigger_json["runs"][0]["pipelineId"], "pl_match");
    assert_eq!(trigger_json["runs"][0]["commitSha"], "deadbeef");
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

fn machine_state() -> AppState {
    let mut config = Config::dev();
    config.api_token = "machine-token".to_string();
    config.data_dir = std::env::temp_dir()
        .join("anvil-machine-api-test")
        .to_string_lossy()
        .into_owned();
    let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
    let audit = AuditSink::disabled();
    let runner = Runner::new(store.clone(), audit.clone(), &config);
    AppState {
        config: Arc::new(config),
        store,
        audit,
        runner,
    }
}

fn machine_get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer machine-token")
        .body(Body::empty())
        .unwrap()
}

fn machine_post(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer machine-token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
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
