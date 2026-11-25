#!/usr/bin/env bash
# Auto-deploys omni when a new image is published. Normally invoked by
# omni-deploy.timer (units in bin/assets/), but safe to run manually. No-ops
# without notifying when nothing changed.
#
# Source of truth: deploy/boris/deploy_omni.sh in the omni repository; install
# with deploy/boris/install.sh. Do not edit the copy in ~/compose/bin.
#
# A deploy must pass the container health check and then bin/omni_smoke.py
# (health, deep health, a real MCP tools/list and read-only calls). If either
# fails, the previous image (kept as :previous) is started again, the failed
# image id is remembered in $BAD_STATE and skipped until a newer image appears,
# and a priority-1 push says so.
#
# Resilience: GHCR pulls flake occasionally (network blips, transient 5xx /
# rate limits) even when there is no new image. To avoid spammy false alarms
# this script (a) retries the pull within a run, and (b) only sends a Pushover
# alert once a failure has PERSISTED across several consecutive timer ticks
# (see *_FAIL_THRESHOLD). A single flaky tick that recovers on the next tick is
# logged but never notified. A genuinely broken deploy/health state alerts
# quickly, and a one-shot recovery note is sent when things go green again.
set -euo pipefail

COMPOSE_DIR="/home/michael/compose"
DOCKER="/usr/bin/docker"
# Image healthcheck: 10s start period + 30s interval x 3 retries
HEALTH_TIMEOUT=180
LOCK_FILE="/tmp/omni-deploy.lock"

# Within-run pull resilience: ride out a momentary blip before counting a tick
# as failed. Total added latency per failing tick <= PULL_RETRIES*PULL_RETRY_DELAY.
PULL_RETRIES=3
PULL_RETRY_DELAY=10

# Consecutive-failure tracking. State line: "<key> <count> <alerted>".
# We alert only when a given failure key reaches its threshold (so transient
# flakes that recover never notify), and only once per streak (alerted flag).
# A flaky pull is usually transient GHCR/network noise -> tolerate a few ticks.
# ~5 consecutive failing ticks (~5 min, timer runs every 60s) = a real outage.
PULL_FAIL_THRESHOLD=5
# A changed image that won't come up / stay healthy is a real problem -> alert
# fast (2 ticks, giving one automatic retry before paging).
HEALTH_FAIL_THRESHOLD=2

exec 9>"$LOCK_FILE"
if ! flock -n 9; then
  echo "another deploy run is in progress; exiting"
  exit 0
fi

# At boot, docker-compose-startup.service runs `compose up -d` for the whole
# fleet. Recreating omni underneath it removes a container that compose is
# still tracking and fails the startup unit, so wait for the next tick.
if [[ "$(systemctl is-active docker-compose-startup.service 2>/dev/null)" == "activating" ]]; then
  echo "docker-compose-startup.service is still starting the fleet; exiting"
  exit 0
fi

# Pushover creds come from the systemd unit's EnvironmentFile; fall back to
# sourcing .env for manual runs.
if [[ -z "${PUSHOVER_USER:-}" || -z "${PUSHOVER_BORIS_TOKEN:-}" ]]; then
  # Source only these keys: other .env values are not valid shell.
  set -a
  # shellcheck source=/dev/null
  source <(grep -E "^(PUSHOVER_USER|PUSHOVER_BORIS_TOKEN)=" "$COMPOSE_DIR/.env")
  set +a
fi
if [[ -z "${PUSHOVER_USER:-}" || -z "${PUSHOVER_BORIS_TOKEN:-}" ]]; then
  echo "ERROR: PUSHOVER_USER / PUSHOVER_BORIS_TOKEN not set (expected via EnvironmentFile or $COMPOSE_DIR/.env)"
  exit 1
fi

notify() { # <priority> <title> <message>
  curl -fsS --max-time 15 https://api.pushover.net/1/messages.json \
    --form-string "token=${PUSHOVER_BORIS_TOKEN}" \
    --form-string "user=${PUSHOVER_USER}" \
    --form-string "priority=$1" \
    --form-string "title=$2" \
    --form-string "message=$3" >/dev/null \
    || echo "WARNING: failed to send Pushover notification"
}

SERVICE="omni"
IMAGE="ghcr.io/micthiesen/omni:latest"
# The image running before a deploy. After a pull nothing else names it, so
# without this tag disk_guard would prune the only rollback target.
PREVIOUS="ghcr.io/micthiesen/omni:previous"
FAIL_STATE="/tmp/omni-deploy.failstate"
# Image id of a deploy that failed and was rolled back. Survives reboots so a
# boot-time `compose up` cannot bring the bad image back unnoticed.
BAD_STATE="$COMPOSE_DIR/.omni-deploy-bad"
SMOKE="$COMPOSE_DIR/bin/omni_smoke.py"
COMPOSE=(/usr/bin/docker compose)

# Populates FS_KEY / FS_COUNT / FS_ALERTED from the state file (defaults if absent).
load_fail_state() {
  FS_KEY="" FS_COUNT=0 FS_ALERTED=0
  if [[ -f "$FAIL_STATE" ]]; then
    read -r FS_KEY FS_COUNT FS_ALERTED <"$FAIL_STATE" 2>/dev/null || true
    [[ "$FS_COUNT" =~ ^[0-9]+$ ]] || FS_COUNT=0
    [[ "$FS_ALERTED" =~ ^[0-9]+$ ]] || FS_ALERTED=0
  fi
}

get_health() {
  $DOCKER inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}no-healthcheck{{end}}' \
    "$SERVICE" 2>/dev/null || echo "missing"
}

# Record a failure and exit 1 (from the deploy subshell). Alerts (priority 1)
# only when this failure key has failed >= <threshold> consecutive ticks, and
# only once per streak.
fail() { # <step> <key> <threshold> <message>
  local step="$1" key="$2" threshold="$3" msg="$4"
  echo "ERROR: $SERVICE $step failed: $msg"

  load_fail_state
  local count=1 alerted=0
  if [[ "$FS_KEY" == "$key" ]]; then
    count=$((FS_COUNT + 1))
    alerted="$FS_ALERTED"
  fi

  if ((count >= threshold)) && ((alerted == 0)); then
    notify 1 "${SERVICE} deploy FAILED" \
      "${step} failing for ${count} consecutive check(s): ${msg}"
    alerted=1
    echo "alerted after ${count}/${threshold} consecutive '${key}' failures"
  else
    echo "'${key}' failure ${count}/${threshold} (alerted=${alerted}); suppressing Pushover"
  fi

  printf '%s %s %s\n' "$key" "$count" "$alerted" >"$FAIL_STATE"
  exit 1
}

# Clear failure state on success; send a one-shot recovery note if we had alerted.
succeed() {
  load_fail_state
  rm -f "$FAIL_STATE"
  if ((FS_ALERTED == 1)); then
    notify 0 "${SERVICE} recovered" \
      "Deploy checks are green again after ${FS_COUNT} failed check(s)."
  fi
}

pull_with_retries() {
  local attempt=1
  while true; do
    if "${COMPOSE[@]}" pull --quiet "$SERVICE"; then
      return 0
    fi
    if ((attempt >= PULL_RETRIES)); then
      return 1
    fi
    echo "pull attempt ${attempt}/${PULL_RETRIES} failed; retrying in ${PULL_RETRY_DELAY}s..."
    sleep "$PULL_RETRY_DELAY"
    attempt=$((attempt + 1))
  done
}

# Waits for the container health check; prints the final status.
await_health() {
  local deadline=$((SECONDS + HEALTH_TIMEOUT)) status="unknown"
  while ((SECONDS < deadline)); do
    status="$(get_health)"
    [[ "$status" == "healthy" || "$status" == "unhealthy" ]] && break
    sleep 5
  done
  echo "$status"
}

# Health check, then the smoke test. Prints the failure reason; 0 when both pass.
verify_deploy() { # <revision>
  echo "waiting up to ${HEALTH_TIMEOUT}s for ${SERVICE} health check..." >&2
  local status
  status="$(await_health)"
  if [[ "$status" != "healthy" ]]; then
    echo "container is '${status}' after ${HEALTH_TIMEOUT}s"
    return 1
  fi
  local out
  if ! out="$(/usr/bin/python3 "$SMOKE" "$1" 2>&1)"; then
    echo "$out" >&2
    echo "smoke test failed: $(printf '%s\n' "$out" | grep -m1 '^FAIL' || printf '%s' "$out" | tail -1)"
    return 1
  fi
  echo "$out" >&2
  return 0
}

# Starts the previous image again after a failed deploy and pages once.
rollback() { # <revision> <failed image id> <reason>
  local revision="$1" bad="$2" reason="$3"
  echo "ERROR: revision ${revision} failed (${reason}); rolling back. Recent container logs:"
  $DOCKER logs --tail 60 "$SERVICE" 2>&1 || true
  printf '%s\n' "$bad" >"$BAD_STATE"
  if ! $DOCKER image inspect "$PREVIOUS" >/dev/null 2>&1; then
    notify 1 "${SERVICE} deploy FAILED, no rollback" \
      "Revision ${revision} failed (${reason}) and no previous image is kept. omni is running the failed image."
    exit 1
  fi
  local previous_rev
  previous_rev="$($DOCKER image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$PREVIOUS")"
  previous_rev="${previous_rev:0:7}"
  # Compose starts whatever the service's image name points at.
  $DOCKER tag "$PREVIOUS" "$IMAGE"
  local status="not restarted"
  if "${COMPOSE[@]}" up -d --no-deps "$SERVICE"; then
    status="$(await_health)"
  fi
  notify 1 "${SERVICE} deploy rolled back" \
    "Revision ${revision} failed: ${reason}. Rolled back to ${previous_rev:-the previous image} (${status}). The failed image is skipped until a newer one is published."
  rm -f "$FAIL_STATE"
  exit 1
}

latest_image_id() {
  $DOCKER image inspect --format '{{.Id}}' "$IMAGE"
}

deploy_service() {
  cd "$COMPOSE_DIR"

  local running_image_id latest
  running_image_id="$($DOCKER inspect --format '{{.Image}}' "$SERVICE" 2>/dev/null || echo "none")"

  if ! pull_with_retries; then
    fail "image pull" "pull" "$PULL_FAIL_THRESHOLD" \
      "could not pull $IMAGE from GHCR after ${PULL_RETRIES} attempts"
  fi

  latest="$(latest_image_id)"

  # A rolled-back image stays rolled back until a newer one is published.
  # Point the tag back at the running image so a fleet-wide `compose up`
  # (boot) does not start the bad one.
  if [[ -f "$BAD_STATE" && "$(cat "$BAD_STATE")" == "$latest" && "$running_image_id" != "$latest" ]]; then
    [[ "$running_image_id" != "none" ]] && $DOCKER tag "$running_image_id" "$IMAGE"
    echo "${SERVICE}: newest image ${latest} failed its deploy earlier; keeping ${running_image_id}"
    return 0
  fi

  # omni's configuration is managed by hand in the main compose file, so only
  # a new image triggers a deploy.
  if [[ "$running_image_id" == "$latest" ]]; then
    # Image is already current. Confirm the running container is actually healthy
    # before declaring a no-op — a deployed-but-unhealthy container also lands
    # here (its image is the latest) and must not be mistaken for success.
    local status
    status="$(get_health)"
    if [[ "$status" == "healthy" || "$status" == "no-healthcheck" ]]; then
      echo "${SERVICE}: image unchanged and container ${status}; nothing to do"
      succeed
      return 0
    fi
    echo "${SERVICE}: image unchanged but container is '${status}'; recent logs:"
    $DOCKER logs --tail 60 "$SERVICE" 2>&1 || true
    fail "health verification" "health:$latest" "$HEALTH_FAIL_THRESHOLD" \
      "container is '${status}' on the current image"
  fi

  local revision short_rev
  revision="$($DOCKER image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$IMAGE")"
  short_rev="${revision:0:7}"
  [[ -n "$short_rev" ]] || short_rev="unknown"
  echo "${SERVICE}: new image detected (revision ${short_rev}): $running_image_id -> $latest"

  if [[ "$running_image_id" != "none" ]]; then
    $DOCKER tag "$running_image_id" "$PREVIOUS"
  fi
  if ! "${COMPOSE[@]}" up -d --no-deps "$SERVICE"; then
    rollback "$short_rev" "$latest" "docker compose up failed"
  fi
  local reason
  if ! reason="$(verify_deploy "$short_rev")"; then
    rollback "$short_rev" "$latest" "$reason"
  fi

  rm -f "$BAD_STATE"
  succeed
  echo "${SERVICE}: deploy successful: revision ${short_rev} is running and healthy"
  # Priority -2: recorded in the Pushover app without a banner or sound. Pushes
  # to main deploy several times a day; failures and recoveries still alert.
  notify -2 "${SERVICE} deployed" "Revision ${short_rev} is running, healthy and passed the smoke test."
}

# The deploy runs in an errexit subshell so fail() and unexpected errors end
# only the check. Calling it from a condition would disable errexit.
set +e
(
  set -e
  deploy_service
)
status=$?
set -e
((status == 0)) || exit 1
exit 0
