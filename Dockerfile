# syntax=docker/dockerfile:1.7
#
# Image for the `fleet` binary. In deploy/docker-compose.yml it runs the
# orchestrator; nodes run the same binary outside Docker (they need the user's
# Claude Code login and repositories), from the release assets instead.
#
# Build:  docker build -t fleet:local .
# Run:    docker run --rm fleet:local --help

ARG RUST_VERSION=1
ARG NODE_VERSION=22

# ---- web UI -----------------------------------------------------------------
# The single-page app is embedded into the binary, so it is built first and its
# output copied into the Rust build. Only apps/web is copied here, so a change
# anywhere else in the tree does not rebuild it.
FROM node:${NODE_VERSION}-bookworm-slim AS web
WORKDIR /web
COPY apps/web/package.json apps/web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY apps/web/ ./
# affectUI Pro is fetched here from its registry with the owner's access key, passed as the build
# secret `affectui` (`docker build --secret id=affectui,src=<file>`), so the key is in no layer. It
# is never committed: the repository is public (see apps/web/DESIGN.md). Its source is deleted in
# the same step, so no layer (nor the build cache CI exports) holds it either; only dist/ is used.
RUN --mount=type=secret,id=affectui npm run build && rm -rf /root/.cache/affectui node_modules/.affectui

# ---- build ------------------------------------------------------------------
# The builder and the runtime must be the same Debian release, so the binary
# links against the glibc it will run on.
FROM rust:${RUST_VERSION}-bookworm AS builder
WORKDIR /src

# The release this image is (`v0.2.7`), baked into the binary as `fleet_proto::BUILD_VERSION`.
# Unset or empty, the binary reports `<workspace version>-dev`, which is what a hand build is.
ARG FLEET_VERSION=""
ENV FLEET_VERSION=${FLEET_VERSION}

ENV CARGO_TERM_COLOR=always \
    CARGO_INCREMENTAL=0 \
    # sqlx's query! macros check against .sqlx/ instead of a live database
    # when this is set. Harmless if the workspace does not use sqlx macros.
    SQLX_OFFLINE=true

COPY . .
# The built UI, which crates/orchestrator embeds. It lands where a local
# `npm run build` would have put it, so the Rust side needs no special case.
COPY --from=web /web/dist ./apps/web/dist

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
