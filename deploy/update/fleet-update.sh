#!/usr/bin/env bash
# Rolls the newest release out to this host: the orchestrator, then this host's own node.
#
# Run as root by fleet-update.timer every few minutes (see deploy/README.md). Each run either
# moves the host to the newest release or leaves it exactly as it was, and says which:
#
#   newer release?  ->  check out its tag  ->  build its image  ->  DRY-RUN ITS MIGRATIONS
#     ->  back up the database  ->  swap the image in  ->  health check  ->  roll back on failure
#
# The dry run is the gate the rest exists to protect. CI and the e2e always start from an empty
# database, so a migration that collides with real rows passes every check and then takes the
# orchestrator down — which happened on 2026-11-20. `fleet migrate --dry-run` applies the new
# migrations to the live database in one transaction and rolls it back; if that fails, nothing
# is deployed and the owner is told.
#
# A release that failed is not retried on every tick: it is remembered, and the next release
# (the fix) is what gets tried. Rolling the binary back after its migrations applied is safe
# because migrations are add-only, and older code ignores tables and columns it does not know.
# To retry one anyway (say the failure was the network): rm /var/lib/fleet-update/failed-<tag>.
#
# THIS SCRIPT RUNS AS ROOT, so it is the one part of a release that does not update itself.
# systemd runs the installed copy at /usr/local/sbin/fleet-update, not the one in the repo: a
# merged change here would otherwise execute as root on the next tick, with nobody having looked
# at it as root code. When the repo's copy differs, the owner is told once, with the one command
# that installs it.

set -euo pipefail

REPO=/opt/fleet
DEPLOY=$REPO/deploy
ENV_FILE=$DEPLOY/.env
STATE=/var/lib/fleet-update
BACKUPS=/root/backups
CONTAINER=fleet-orchestrator-1
DB_CONTAINER=fleet-postgres-1
NETWORK=fleet_default
WEB=http://127.0.0.1:8090/
NODE_BIN=/usr/local/bin/fleet
# How long a busy node may hold off its update before it is updated anyway (runs resume).
NODE_DEADLINE_SECS=$((2 * 3600))

log() { echo "fleet-update: $*"; }

# Reads one value from the deploy .env without sourcing it (the file is data, not a script).
env_value() { grep -E "^$1=" "$ENV_FILE" | tail -n1 | cut -d= -f2-; }

# Tells the owner in the fleet's Telegram group. The token goes to curl on stdin, never on its
# command line, where any user on the host could read it from the process list.
notify() {
    local text=$1 silent=${2:-false} token chat
    token=$(env_value TELEGRAM_ORCHESTRATOR_TOKEN || true)
    chat=$(env_value TELEGRAM_CHAT_ID || true)
    [ -n "$token" ] && [ -n "$chat" ] || { log "cannot notify: no Telegram settings"; return 0; }
    printf 'url = "https://api.telegram.org/bot%s/sendMessage"\n' "$token" |
        curl -sS -K - -o /dev/null \
            --data-urlencode "chat_id=$chat" \
            --data-urlencode "text=$text" \
            --data-urlencode "disable_notification=$silent" || log "notify failed"
}

# The newest stable release on GitHub, from git itself (no API, no rate limit).
latest_release() {
    git -C "$REPO" ls-remote --tags --refs origin 'v*' |
        awk '{sub("refs/tags/", "", $2); print $2}' |
        grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' |
        sort -V | tail -n1
}

# What a binary says it is: "v0.2.4", or "0.1.0-dev" for a hand build.
version_of() { "$@" --version 2>/dev/null | awk '{print $2}'; }

running_version() { docker exec "$CONTAINER" fleet --version 2>/dev/null | awk '{print $2}'; }

# Healthy means: running, not restarting, the version we just deployed, and the web answering —
# continuously for 20 s, so a process that starts and then crashes on its first tick is caught.
healthy() {
    local want=$1 deadline=$((SECONDS + 120)) stable_since=""
    while [ $SECONDS -lt $deadline ]; do
        local state version code
        state=$(docker inspect -f '{{.State.Status}} {{.State.Restarting}}' "$CONTAINER" 2>/dev/null || echo "missing")
        version=$(running_version || true)
        code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$WEB" || true)
        if [ "$state" = "running false" ] && [ "$version" = "$want" ] && [ "$code" = "200" ]; then
            stable_since=${stable_since:-$SECONDS}
            [ $((SECONDS - stable_since)) -ge 20 ] && return 0
        else
            stable_since=""
        fi
        sleep 3
    done
    log "unhealthy: state=[$state] version=[$version] web=[$code]"
    return 1
}

# ------------------------------------------------------------------- the orchestrator

update_orchestrator() {
    local want=$1 was=$2

    if [ -e "$STATE/failed-$want" ]; then
        log "$want failed before; waiting for a newer release"
        return 0
    fi
    fail() {
        touch "$STATE/failed-$want"
        log "FAILED: $1"
        notify "❌ Deploy of $want stopped: $1
Still running $was. A newer release will be tried when there is one."
        exit 1
    }

    log "deploying $want (running $was)"
    notify "🚀 Deploying $want (running $was)…" true

    git -C "$REPO" fetch -q --tags origin || fail "could not fetch from GitHub"
    # Tags sit on main's history, so this moves main forward exactly as a hand deploy does.
    git -C "$REPO" merge -q --ff-only "$want" || fail "$want is not a fast-forward of what is checked out"
    # The image is built from the working tree, so the tree must *be* the release: a checkout
    # someone moved past the tag by hand would otherwise be built and labelled as $want.
    [ "$(git -C "$REPO" rev-parse HEAD)" = "$(git -C "$REPO" rev-parse "$want^{commit}")" ] ||
        fail "/opt/fleet is ahead of $want (moved by hand?); the next release will catch up"

    docker build -q --build-arg "FLEET_VERSION=$want" -t "fleet:$want" "$REPO" >/dev/null ||
        fail "the image did not build"

    # The URL the running orchestrator uses, handed over through the environment, not argv.
    local db_url
    db_url=$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$CONTAINER" |
        sed -n 's/^DATABASE_URL=//p' | head -n1)
    [ -n "$db_url" ] || fail "could not read the orchestrator's DATABASE_URL"
    local report
    if ! report=$(DATABASE_URL=$db_url docker run --rm --network "$NETWORK" -e DATABASE_URL \
        "fleet:$want" migrate --dry-run 2>&1); then
        fail "its migrations failed a dry run against the live database:
$report"
    fi
    log "dry run passed: $(echo "$report" | head -n1)"

    local backup=$BACKUPS/fleet-before-$want.sql.gz
    docker exec "$DB_CONTAINER" pg_dump -U fleet -d fleet | gzip >"$backup" ||
        fail "the database backup failed"
    # A failed pg_dump still writes a small file that looks like a backup; check it is real.
    local size tables
    size=$(stat -c %s "$backup")
    tables=$(zcat "$backup" | grep -c 'CREATE TABLE' || true)
    [ "$size" -gt 100000 ] && [ "$tables" -gt 0 ] ||
        fail "the database backup looks empty ($size bytes, $tables tables)"
    log "backup $backup ($size bytes, $tables tables)"

    docker tag fleet:local fleet:previous 2>/dev/null || true
    docker tag "fleet:$want" fleet:local
    (cd "$DEPLOY" && docker compose up -d orchestrator) >/dev/null 2>&1 ||
        fail "compose would not start it"

    if healthy "$want"; then
        log "deployed $want"
        notify "✅ Deployed $want (was $was). Backup: $backup" true
        # Keep this release and the one before it; drop older builds so the disk does not fill.
        docker images --format '{{.Repository}}:{{.Tag}}' 'fleet' |
            grep -vE '^fleet:(local|previous)$' | grep -v "^fleet:$want$" |
            xargs -r docker rmi -f >/dev/null 2>&1 || true
        docker image prune -f >/dev/null 2>&1 || true
        return 0
    fi

    # Back to what was running. Its migrations stay applied, which older code tolerates.
    log "rolling back to $was"
    if docker image inspect fleet:previous >/dev/null 2>&1; then
        docker tag fleet:previous fleet:local
        (cd "$DEPLOY" && docker compose up -d orchestrator) >/dev/null 2>&1 || true
    fi
    touch "$STATE/failed-$want"
    notify "❌ $want did not come up healthy and was rolled back to $was.
Its migrations stayed applied (they are add-only). Logs: docker logs $CONTAINER"
    exit 1
}

# ------------------------------------------------------------------- this host's node

# The node on this host runs the same binary as the orchestrator image, so it follows it — but
# only when no agent is running on it, or once it has waited NODE_DEADLINE_SECS. The fleet's own
# standing sessions (brain, secretary, retrospective) do not count as busy: they wait most of the
# time, and they resume from their sessions by design.
update_node() {
    local want=$1 have node_id busy since now
    have=$(version_of "$NODE_BIN" || true)
    [ "$have" = "$want" ] && return 0
    # Only from an image that is really that release and really running.
    [ "$(running_version || true)" = "$want" ] || return 0

    node_id=$(docker exec "$DB_CONTAINER" psql -U fleet -d fleet -At -c \
        "select id from nodes where name = 'vps'" 2>/dev/null || true)
    [ -n "$node_id" ] || { log "no node named vps; skipping the node"; return 0; }
    busy=$(docker exec "$DB_CONTAINER" psql -U fleet -d fleet -At -c \
        "select count(*) from tasks where running and node_id = '$node_id'
           and id not in ('brain', 'secretary', 'retro')" 2>/dev/null || echo 1)

    now=$(date +%s)
    [ -e "$STATE/node-waiting-$want" ] || echo "$now" >"$STATE/node-waiting-$want"
    since=$(cat "$STATE/node-waiting-$want")
    if [ "$busy" != "0" ] && [ $((now - since)) -lt $NODE_DEADLINE_SECS ]; then
        log "node busy ($busy running); waiting to update it to $want"
        return 0
    fi

    local cid copied=0
    cid=$(docker create "fleet:local")
    docker cp "$cid:/usr/local/bin/fleet" "$NODE_BIN.new" && copied=1
    docker rm -v "$cid" >/dev/null 2>&1 || true
    [ "$copied" = 1 ] || { log "could not copy the node binary out of the image"; return 0; }
    chmod 755 "$NODE_BIN.new"
    [ "$(version_of "$NODE_BIN.new")" = "$want" ] || {
        rm -f "$NODE_BIN.new"
        log "the node binary in the image is not $want; leaving the node alone"
        return 0
    }
    cp "$NODE_BIN" "$NODE_BIN.previous" 2>/dev/null || true
    mv "$NODE_BIN.new" "$NODE_BIN"
    systemctl restart fleet-node
    sleep 5
    if systemctl is-active -q fleet-node; then
        rm -f "$STATE/node-waiting-$want"
        log "node updated to $want (was $have)"
        [ "$busy" != "0" ] && notify "🔄 vps node updated to $want after waiting $(( (now - since) / 60 )) min; its runs resume." true
        return 0
    fi
    log "node did not start on $want; restoring $have"
    [ -e "$NODE_BIN.previous" ] && mv "$NODE_BIN.previous" "$NODE_BIN" && systemctl restart fleet-node
    notify "❌ vps node would not start on $want and was put back on $have."
}

# ------------------------------------------------------------------- the updater itself

# Never applies a new copy of itself (see the top): only says so, once per difference.
check_self() {
    local repo_copy=$DEPLOY/update/fleet-update.sh installed=/usr/local/sbin/fleet-update sum
    [ -f "$repo_copy" ] && [ -f "$installed" ] || return 0
    cmp -s "$repo_copy" "$installed" && return 0
    sum=$(sha256sum "$repo_copy" | cut -c1-12)
    [ -e "$STATE/told-updater-$sum" ] && return 0
    touch "$STATE/told-updater-$sum"
    notify "🛠 The updater changed in the newest release. It runs as root, so it is not applied by itself — read deploy/update/fleet-update.sh, then on the VPS:
install -m 0755 $repo_copy $installed" true
}

# ------------------------------------------------------------------- main

main() {
    mkdir -p "$STATE" "$BACKUPS"
    # One run at a time, even if a timer tick lands while a slow build is still going.
    exec 9>/run/fleet-update.lock
    flock -n 9 || exit 0

    local want have
    want=$(latest_release || true)
    [ -n "$want" ] || { log "no release yet"; exit 0; }
    have=$(running_version || true)

    if [ "$have" != "$want" ]; then
        update_orchestrator "$want" "${have:-unknown}"
    fi
    update_node "$want"
    check_self
}

# Sourced (by a test) it only defines the functions above; run, it does the work.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
