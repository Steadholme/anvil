//! Server configuration, env-driven with working dev defaults.
//!
//! Every value keeps its dev default when the corresponding env var is unset/empty, so the
//! in-memory dev path boots with NO configuration and NO database — exactly like
//! keystone/inkwell/loom. Production overrides each via the environment.

/// Default listen address (all interfaces, internal-only port 9240).
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:9240";

/// Default on-disk root for per-run build workspaces + artifacts (`ANVIL_DATA`). Each run clones
/// into `<ANVIL_DATA>/<run_id>` and the workspace is removed once the run finishes.
pub const DEFAULT_DATA_DIR: &str = "/data";

/// Default `git` binary (resolved on PATH; the runtime image installs the `git` package).
pub const DEFAULT_GIT_BIN: &str = "git";

/// Default per-step wall-clock timeout, seconds (`ANVIL_STEP_TIMEOUT`). A step exceeding this is
/// killed and the run is marked failed.
pub const DEFAULT_STEP_TIMEOUT_SECS: u64 = 600;

/// Default ceiling on concurrently-executing runs (`ANVIL_MAX_CONCURRENT`). Extra runs stay
/// `queued` until a slot frees — Anvil shares one box with the rest of the estate.
pub const DEFAULT_MAX_CONCURRENT: usize = 2;

/// How many pipelines the console lists.
pub const PIPELINE_LIST_LIMIT: usize = 200;

/// How many recent runs the console lists.
pub const RUN_LIST_LIMIT: usize = 50;

/// Hard cap on a pipeline name, in characters.
pub const MAX_NAME_CHARS: usize = 200;

/// Hard cap on the combined run log we retain, in bytes. Beyond this a truncation notice is
/// appended and further output is dropped (keeps one runaway build from unbounded log growth).
pub const MAX_LOG_BYTES: usize = 1024 * 1024;

/// Runtime configuration. Cheap to clone; shared read-only behind `Arc`.
#[derive(Clone, Debug)]
pub struct Config {
    /// Listen address (`BIND_ADDR`).
    pub bind_addr: String,
    /// On-disk root for per-run workspaces (`ANVIL_DATA`).
    pub data_dir: String,
    /// `git` binary (`GIT_BIN`).
    pub git_bin: String,
    /// Per-step timeout in seconds (`ANVIL_STEP_TIMEOUT`).
    pub step_timeout_secs: u64,
    /// Max concurrent runs (`ANVIL_MAX_CONCURRENT`).
    pub max_concurrent: usize,
}

impl Config {
    /// Default development configuration (in-memory store, workspaces under `/data`).
    pub fn dev() -> Self {
        Config {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            data_dir: DEFAULT_DATA_DIR.to_string(),
            git_bin: DEFAULT_GIT_BIN.to_string(),
            step_timeout_secs: DEFAULT_STEP_TIMEOUT_SECS,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
        }
    }

    /// Configuration with the dev defaults overridden by environment variables.
    pub fn from_env() -> Self {
        let mut config = Config::dev();
        if let Some(v) = env_nonempty("BIND_ADDR") {
            config.bind_addr = v;
        }
        if let Some(v) = env_nonempty("ANVIL_DATA") {
            config.data_dir = v;
        }
        if let Some(v) = env_nonempty("GIT_BIN") {
            config.git_bin = v;
        }
        if let Some(v) = env_nonempty("ANVIL_STEP_TIMEOUT").and_then(|v| v.parse::<u64>().ok()) {
            if v > 0 {
                config.step_timeout_secs = v;
            }
        }
        if let Some(v) = env_nonempty("ANVIL_MAX_CONCURRENT").and_then(|v| v.parse::<usize>().ok()) {
            if v > 0 {
                config.max_concurrent = v;
            }
        }
        config
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::dev()
    }
}

/// Read an env var, returning `None` when unset OR empty (empty never clobbers a default).
pub fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Interpret a boolean-ish env var (`on` / `true` / `1` / `yes`, case-insensitive).
pub fn env_truthy(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "on" | "true" | "1" | "yes"
    )
}
