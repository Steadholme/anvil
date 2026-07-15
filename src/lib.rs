//! Anvil — CI build runner for the Steadholme stack.
//!
//! Library root: defines [`AppState`], wires the routes via [`app`], and provides
//! [`build_dev_state`] (in-memory store, disabled audit) and [`build_state_from_env`]
//! (env-selected store + Watchtower audit + run scheduler). Integration tests consume [`app`]
//! directly via `tower::oneshot`, exactly like the rest of the estate.
//!
//! Anvil sits behind a Sluice `auth=sso` route at the subdomain ROOT (`ci.w33d.xyz`); the gateway
//! authenticates the operator, STRIPS any inbound `X-Auth-*`, and injects the verified
//! `X-Auth-Subject` / `X-Auth-Email`. Anvil is INTERNAL-ONLY and trusts those headers.
//!
//! Source -> build on owned metal: a pipeline shallow-clones a git repo and runs declared shell
//! steps, capturing combined logs + exit status. A bounded scheduler caps concurrent runs.
//!
//! Endpoints (Sluice forwards the path unmodified):
//! - `GET  /healthz`                    liveness (container HEALTHCHECK, public)
//! - `GET  /`                           console: pipelines + recent runs + create-pipeline form
//! - `GET  /pipeline/{id}`               pipeline detail: definition + recent runs + badge link
//! - `GET  /pipeline/{id}/edit`          edit an existing pipeline definition
//! - `POST /api/pipelines`              create a pipeline (sso, CSRF)
//! - `POST /api/pipelines/{id}`          update a pipeline (sso, CSRF)
//! - `POST /api/pipelines/{id}/run`     enqueue a run (sso, CSRF)
//! - `GET  /badge/{id}/status.svg`       SVG status badge for a pipeline
//! - `GET  /run/{id}`                   the run page (live-ish log, status, duration)

pub mod audit;
pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod runner;
pub mod store;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::routing::{get, post};
use axum::Router;

use crate::audit::AuditSink;
use crate::config::{env_nonempty, env_truthy, Config};
use crate::runner::Runner;
use crate::store::{InMemoryStore, PgStore, Store};

/// Shared application state. Cheap to clone (everything behind `Arc` / cloneable handles).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Arc<dyn Store>,
    pub audit: AuditSink,
    pub runner: Runner,
}

/// Build the router wiring all endpoints onto `state`. Routes are explicit (no fallback): the
/// service owns its subdomain, so Sluice forwards these exact paths.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::health::healthz))
        .route("/", get(handlers::pipelines::index))
        .route("/pipeline/{id}", get(handlers::pipelines::pipeline_page))
        .route("/pipeline/{id}/edit", get(handlers::pipelines::edit_page))
        .route("/api/pipelines", post(handlers::pipelines::create))
        .route("/api/pipelines/{id}", post(handlers::pipelines::update))
        .route("/api/pipelines/{id}/run", post(handlers::pipelines::run))
        .route(
            "/badge/{id}/status.svg",
            get(handlers::pipelines::status_badge),
        )
        .route("/run/{id}", get(handlers::pipelines::run_page))
        .with_state(state)
}

/// Assemble an [`AppState`] from its parts (shared by dev + env construction so the [`Runner`]
/// always drives the SAME store the handlers read).
fn assemble(config: Config, store: Arc<dyn Store>, audit: AuditSink) -> AppState {
    let runner = Runner::new(store.clone(), audit.clone(), &config);
    AppState {
        config: Arc::new(config),
        store,
        audit,
        runner,
    }
}

/// Construct dev state: dev [`Config`], an empty [`InMemoryStore`], and a disabled audit sink (no
/// network). The data root is a per-process temp directory so concurrent dev runs / tests can
/// actually create run workspaces without colliding or needing `/data`.
pub fn build_dev_state() -> AppState {
    let mut config = Config::dev();
    config.data_dir = std::env::temp_dir()
        .join(format!("anvil-dev-{}", now_nanos()))
        .to_string_lossy()
        .into_owned();
    assemble(
        config,
        Arc::new(InMemoryStore::new()),
        AuditSink::disabled(),
    )
}

/// Build runtime state from the environment.
///
/// [`Config`] comes from [`Config::from_env`]. The store is selected by `ANVIL_STORE`:
/// - `memory` (default): empty [`InMemoryStore`] — no database required.
/// - `postgres`: connect `DATABASE_URL` (db `anvil`), run the idempotent migration, wire [`PgStore`].
///
/// The data root (`ANVIL_DATA`) is created on startup so the first run's clone succeeds. The audit
/// sink is enabled by `AUDIT_ENABLED` + `WATCHTOWER_URL` + `AUDIT_INGEST_TOKEN`. Returns an error
/// string on misconfiguration so `main` can fail loudly.
pub async fn build_state_from_env() -> Result<AppState, String> {
    let config = Config::from_env();

    tokio::fs::create_dir_all(&config.data_dir)
        .await
        .map_err(|e| format!("create data dir {}: {e}", config.data_dir))?;

    let store_kind = env_nonempty("ANVIL_STORE").unwrap_or_else(|| "memory".to_string());
    let store: Arc<dyn Store> = match store_kind.as_str() {
        "postgres" => {
            let database_url = env_nonempty("DATABASE_URL")
                .ok_or_else(|| "ANVIL_STORE=postgres requires DATABASE_URL".to_string())?;
            tracing::info!("ANVIL_STORE=postgres — connecting to database");
            let pg = PgStore::connect(&database_url)
                .await
                .map_err(|e| format!("connect postgres: {e}"))?;
            pg.migrate()
                .await
                .map_err(|e| format!("run migration: {e}"))?;
            tracing::info!("postgres store ready (migrated)");
            Arc::new(pg)
        }
        "memory" => Arc::new(InMemoryStore::new()),
        other => return Err(format!("unknown ANVIL_STORE={other} (use memory|postgres)")),
    };

    let audit = AuditSink::start(
        env_truthy("AUDIT_ENABLED"),
        &env_nonempty("WATCHTOWER_URL").unwrap_or_default(),
        env_nonempty("AUDIT_INGEST_TOKEN").as_deref(),
    );

    Ok(assemble(config, store, audit))
}

/// Current wall-clock time in epoch seconds (created/started/finished granularity).
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs() as i64
}

/// Monotonic-ish nanosecond counter, the high-entropy part of an id.
fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos()
}

/// Generate a filesystem-safe, collision-resistant id with the given prefix: a nanosecond stamp
/// plus 8 hex CSPRNG bytes (so two ids minted in the same nanosecond still differ). The result is
/// `[a-z0-9_]` only, safe to use directly as a workspace directory name.
pub fn new_id(prefix: &str) -> String {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable");
    format!("{prefix}_{}_{}", now_nanos(), hex::encode(bytes))
}
