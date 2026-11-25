#!/usr/bin/env bash
# Build the image locally, ship it over SSH (no registry), restart the stack.
#
#   deploy/deploy.sh                    # host from WH_DEPLOY_HOST (default: hetzner_sport_match)
#
# First time: create ~/workhub/.env on the server from deploy/env.example.
set -euo pipefail
HOST=${WH_DEPLOY_HOST:-hetzner_sport_match}
DIR=${WH_DEPLOY_DIR:-workhub}
cd "$(dirname "$0")/.."

TAG=$(git rev-parse --short HEAD)
git diff --quiet HEAD -- . ':!deploy/README.md' || TAG="$TAG-dirty"
echo "==> building workhub:$TAG"
docker build -f deploy/Dockerfile -t "workhub:$TAG" .

echo "==> shipping image to $HOST"
docker save "workhub:$TAG" | gzip -1 | ssh "$HOST" 'gunzip | docker load'

echo "==> restarting"
ssh "$HOST" "mkdir -p $DIR"
scp -q deploy/docker-compose.yml "$HOST:$DIR/docker-compose.yml"
ssh "$HOST" bash -s -- "$DIR" "$TAG" <<'REMOTE'
set -euo pipefail
cd "$1"
test -f .env || { echo "missing ~/$1/.env (copy deploy/env.example)"; exit 1; }
sed -i '/^WH_IMAGE=/d' .env && echo "WH_IMAGE=workhub:$2" >> .env
docker compose up -d --wait
docker compose ps
# Keep the current image and the one before it (rollback).
docker images workhub --format '{{.CreatedAt}}\t{{.Tag}}' | sort -r | tail -n +3 | cut -f2 \
    | xargs -r -I{} docker rmi -f workhub:{} >/dev/null
REMOTE
echo "==> deployed workhub:$TAG"
