# Base image for WorkHub agent sessions run with `sandbox: { kind: docker }`.
#
#   wh sandbox build            # tags workhub-agent:latest with the host's Claude Code version
#
# Sessions run as the host user's uid:gid with HOME=/home/wh (a throwaway
# per-session directory mounted by the runner), in the task's git worktree
# mounted at the same path as on the host. Factories that need a toolchain
# extend this image, e.g.:
#
#   FROM workhub-agent:latest
#   RUN apt-get update && apt-get install -y --no-install-recommends python3 && rm -rf /var/lib/apt/lists/*
FROM node:22-bookworm-slim

ARG CLAUDE_CODE_VERSION=latest

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git jq procps ripgrep \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g @anthropic-ai/claude-code@${CLAUDE_CODE_VERSION} \
    && npm cache clean --force

RUN mkdir -p /home/wh && chmod 1777 /home/wh
ENV HOME=/home/wh \
    DISABLE_AUTOUPDATER=1 \
    DISABLE_TELEMETRY=1
WORKDIR /home/wh
LABEL org.opencontainers.image.title="workhub-agent" \
      org.opencontainers.image.description="Claude Code runtime for WorkHub agent sessions"
