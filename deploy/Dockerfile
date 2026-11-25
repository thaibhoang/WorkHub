# Production image for WorkHub: `wh serve` (API + SSE + UI, optionally the foreman)
# with Claude Code, git and gh for the agent sessions it runs.
#
#   deploy/deploy.sh          # builds this, ships it to the server, restarts
#
# Built on bookworm so the binary runs on older hosts too. Agents run in this
# container with the plain worktree sandbox; `sandbox: { kind: docker }` is not
# available here (no docker socket inside).

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY migrations migrations
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin wh && cp target/release/wh /usr/local/bin/wh

FROM node:22-bookworm-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web ./
RUN npm run build

FROM node:22-bookworm-slim
ARG CLAUDE_CODE_VERSION=latest
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git jq openssh-client procps ripgrep \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
       > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*
RUN npm install -g @anthropic-ai/claude-code@${CLAUDE_CODE_VERSION} && npm cache clean --force

WORKDIR /app
COPY --from=build /usr/local/bin/wh /usr/local/bin/wh
COPY --from=web /web/dist web/dist
COPY factory factory
COPY benchmarks benchmarks
COPY deploy/entrypoint.sh /usr/local/bin/wh-entrypoint
# Volumes mounted here start out owned by `node` (uid 1000), the user everything runs as.
RUN mkdir -p /data /repos && chown node:node /data /repos /home/node

USER node
ENV HOME=/home/node \
    WH_DATA_DIR=/data \
    DISABLE_AUTOUPDATER=1 \
    DISABLE_TELEMETRY=1 \
    RUST_LOG=info
EXPOSE 7070
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s CMD curl -fsS http://127.0.0.1:7070/api/health || exit 1
LABEL org.opencontainers.image.title="workhub"
ENTRYPOINT ["wh-entrypoint"]
