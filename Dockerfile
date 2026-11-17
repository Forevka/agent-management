# syntax=docker/dockerfile:1.7
#
# Image for the `fleet` binary. In deploy/docker-compose.yml it runs the
# orchestrator; nodes run the same binary outside Docker (they need the user's
# Claude Code login and repositories), from the release assets instead.
#
# Build:  docker build -t fleet:local .
# Run:    docker run --rm fleet:local --help

ARG RUST_VERSION=1

# ---- build ------------------------------------------------------------------
# The builder and the runtime must be the same Debian release, so the binary
# links against the glibc it will run on.
FROM rust:${RUST_VERSION}-bookworm AS builder
WORKDIR /src

ENV CARGO_TERM_COLOR=always \
    CARGO_INCREMENTAL=0 \
    # sqlx's query! macros check against .sqlx/ instead of a live database
    # when this is set. Harmless if the workspace does not use sqlx macros.
    SQLX_OFFLINE=true

COPY . .

# Cache mounts keep the registry and target/ between builds without cargo-chef.
# target/ lives only in the cache mount, so the binary is copied out in the
# same RUN step.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked --package fleet --bin fleet \
 && install -D -m 0755 target/release/fleet /out/fleet

# ---- runtime ----------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# ca-certificates: TLS to Telegram, the iroh relay and Hindsight's upstreams.
# If a dependency ever switches to native-tls, add libssl3 here.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# A fixed, non-root UID so volume ownership is predictable across rebuilds.
RUN groupadd --system --gid 10001 fleet \
 && useradd --system --uid 10001 --gid fleet \
      --home-dir /var/lib/fleet --shell /usr/sbin/nologin fleet \
 && install -d -o fleet -g fleet -m 0700 /var/lib/fleet

COPY --from=builder /out/fleet /usr/local/bin/fleet

# Holds node.key (the orchestrator's iroh identity) and local state. A fresh
# named volume mounted here inherits the fleet:fleet ownership above.
ENV FLEET_DATA_DIR=/var/lib/fleet
VOLUME ["/var/lib/fleet"]
WORKDIR /var/lib/fleet

USER fleet:fleet

# 7700/udp: iroh QUIC (nodes connect here). 8080/tcp: web UI behind Caddy.
EXPOSE 7700/udp 8080/tcp

ENTRYPOINT ["fleet"]
CMD ["orchestrator"]
