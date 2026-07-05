# syntax=docker/dockerfile:1
#
# Multi-stage build for Anvil (CI build runner).
#   - builder: rust:1.96-slim (Debian trixie).
#   - runtime: debian:trixie-slim (matching glibc), non-root, ca-certificates + git.
#
# Anvil links NO OpenSSL: sqlx uses `rustls` and the Watchtower audit hop is hand-rolled plaintext
# HTTP, so the binary depends only on glibc. The runtime DOES install `git` — Anvil shells out to
# `git clone` to fetch a pipeline's source (e.g. from Loom over HTTP) before running the declared
# shell steps. The container HEALTHCHECK uses the built-in `anvil healthcheck` subcommand, so the
# image needs no curl.

FROM rust:1.96-slim AS builder
WORKDIR /build

# Cache the dependency graph first: build a throwaway lib/bin against the real manifest so
# `cargo build` only recompiles our crate when src/ changes, not the whole tree.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --bin anvil \
    && rm -rf src

# Now build the real binary. static/ + templates/ are include_str!'d into the binary, so they must
# be present at compile time.
COPY src ./src
COPY static ./static
COPY templates ./templates
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --bin anvil \
    && strip target/release/anvil

FROM debian:trixie-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user (no shell, no home writes needed).
RUN useradd --system --uid 10001 --user-group --no-create-home anvil
COPY --from=builder /build/target/release/anvil /usr/local/bin/anvil

# Per-run build workspaces + artifacts live here. Pre-creating it owned by uid 10001 means a FRESH
# named/anonymous volume mounted here inherits writable ownership (Docker seeds the volume from the
# image path).
RUN mkdir -p /data && chown 10001:10001 /data
VOLUME ["/data"]

USER anvil
# Default in-container config; overridable at runtime.
ENV BIND_ADDR=0.0.0.0:9240 \
    ANVIL_DATA=/data
EXPOSE 9240

# Dependency-free liveness probe -> GET /healthz on the loopback, exit 0/1.
HEALTHCHECK --interval=10s --timeout=5s --start-period=5s --retries=3 \
    CMD ["anvil", "healthcheck"]

ENTRYPOINT ["anvil"]
