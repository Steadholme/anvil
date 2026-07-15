# Anvil — CI build runner

Anvil turns source into builds on owned metal. An operator defines a **pipeline** (a git repo URL,
a branch, and a list of shell steps); triggering it enqueues a **run**: Anvil shallow-clones the
repo, executes each step in order inside a per-run workspace, captures the combined stdout/stderr,
exit status, and duration, then cleans the workspace.

Anvil is part of the Steadholme estate. It lives at `ci.w33d.xyz`, sits behind the Sluice gateway, and
is **internal-only**.

## Security posture (read this)

**v1 is NOT sandboxed.** A pipeline's steps run as ordinary subprocesses **inside the Anvil
container**, isolated only by:

- a **per-step wall-clock timeout** (`ANVIL_STEP_TIMEOUT`, default 600s) after which the process is
  killed,
- a **minimal, scrubbed environment** (`env_clear` + a fixed `PATH`; no inherited host secrets),
- the **non-root container user** (uid 10001), and
- a **bounded concurrency limit** (`ANVIL_MAX_CONCURRENT`, default 2).

A malicious or buggy step can still do anything that container user can do (network, filesystem
within the container, CPU/memory). Real per-run isolation — a microVM / sandbox boundary — is the
deliberately deferred **Crucible** hard wave. Until then the trust boundary is **identity**: only an
SSO-authenticated operator can define or trigger a pipeline. Do not point Anvil at untrusted repos
or grant console access to untrusted users.

## Identity

Anvil does no login of its own. The Sluice gateway runs the OIDC browser login against Keystone,
**strips** any inbound `X-Auth-*`, and **injects** the verified `X-Auth-Subject` / `X-Auth-Email`,
which Anvil trusts. State-changing POSTs additionally carry a double-submit CSRF token
(`__Host-csrf` cookie + hidden form field).

## Endpoints

| Method + path                    | Auth        | Purpose                                            |
|----------------------------------|-------------|----------------------------------------------------|
| `GET  /healthz`                  | public      | Liveness probe (container HEALTHCHECK).            |
| `GET  /`                         | sso         | Console: pipelines + recent runs + create form.   |
| `POST /api/pipelines`            | sso + CSRF  | Create a pipeline `{name, repo_url, branch, steps}`. |
| `POST /api/pipelines/{id}/run`   | sso + CSRF  | Enqueue a run; the scheduler executes it.          |
| `GET  /run/{id}`                 | sso         | Run page: live-ish log, status, duration.          |

## Storage

`ANVIL_STORE=memory` (default) keeps everything in-process — the service boots zero-config, no
database. `ANVIL_STORE=postgres` (db `anvil`) persists to PostgreSQL via `sqlx` runtime queries
(portable standard SQL, rustls, no macros, no vendor types); the schema is created idempotently on
startup. Build workspaces + artifacts are written under `ANVIL_DATA` (`/data`, the `anvil_data`
volume) and removed when each run finishes.

```sql
pipelines(id TEXT PRIMARY KEY, name TEXT NOT NULL, repo_url TEXT NOT NULL,
          branch TEXT NOT NULL DEFAULT 'main', steps TEXT NOT NULL DEFAULT '', created_at BIGINT);
runs(id TEXT PRIMARY KEY, pipeline_id TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'queued',
     started_at BIGINT NOT NULL DEFAULT 0, finished_at BIGINT NOT NULL DEFAULT 0,
     exit_code BIGINT NOT NULL DEFAULT 0, log TEXT NOT NULL DEFAULT '');
-- INDEX(pipeline_id, started_at)
```

## Audit

Notable events are emitted to Watchtower through a non-blocking, bounded-queue emitter
(`source=anvil`). A failed run emits `anvil.run.fail`. A slow or down Watchtower never blocks,
slows, or fails a request — events are dropped (counted + warned) when the queue saturates.

## Configuration

| Env var               | Default              | Meaning                                             |
|-----------------------|----------------------|-----------------------------------------------------|
| `BIND_ADDR`           | `0.0.0.0:9240`       | Listen address.                                     |
| `ANVIL_STORE`         | `memory`             | `memory` or `postgres`.                             |
| `DATABASE_URL`        | —                    | Required when `ANVIL_STORE=postgres` (db `anvil`).  |
| `ANVIL_DATA`          | `/data`             | Root for per-run build workspaces.                  |
| `ANVIL_STEP_TIMEOUT`  | `600`                | Per-step timeout, seconds.                          |
| `ANVIL_MAX_CONCURRENT`| `2`                  | Max concurrently-executing runs.                    |
| `GIT_BIN`             | `git`                | `git` binary (resolved on PATH).                    |
| `AUDIT_ENABLED`       | `false`              | Enable the Watchtower audit emitter.                |
| `WATCHTOWER_URL`      | `http://watchtower:8500` | Watchtower base URL (plain HTTP).              |
| `AUDIT_INGEST_TOKEN`  | —                    | Bearer token for Watchtower ingest.                 |

## Build & test

```sh
cargo check --all-targets
cargo test
```

The service boots zero-config (`cargo run`) on the in-memory store with audit disabled.
