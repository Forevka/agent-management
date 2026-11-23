#!/usr/bin/env bash
# Rolls the newest release out to this host: the orchestrator, then this host's own node.
#
# Run as root by fleet-update.timer every few minutes (see deploy/README.md §8). Each run either
# moves the host to the newest release or leaves it exactly as it was, and says which:
#
#   newest release, on main?  ->  check out its tag  ->  build its image
#     ->  DRY-RUN ITS MIGRATIONS against the live database  ->  back up the database
#     ->  swap the image in  ->  20 s of continuous health  ->  otherwise put the old one back
#
# The dry run is the gate the rest exists to protect. CI and the e2e always start from an empty
# database, so a migration that collides with real rows passes every check and then takes the
# orchestrator down — which happened on 2026-11-20. `fleet migrate --dry-run` applies the new
# migrations to the live database in one transaction and rolls it back.
#
# Only a tag on main's history is deployed. Anyone who can push a tag to the public repository
# could otherwise bypass the owner's merge, which is meant to be the one gate.
#
# A release that failed is remembered and not retried; the next one (the fix) is. To retry it
# anyway — say the failure was the network — rm /var/lib/fleet-update/failed-<tag>. A dry run
# that only ran out of time waiting for a lock the live orchestrator held is retried next tick.
#
# Rolling back works because the orchestrator's migrator ignores applied migrations it does not
# know (`ignore_missing`, from v0.2.2): migrations are add-only, and new columns are nullable or
# defaulted, so the previous release runs against the newer schema. After a rollback the old
# image is health-checked too, and the owner is told the truth either way.
#
# THIS SCRIPT RUNS AS ROOT, so it is the one part of a release that does not update itself.
# systemd runs the installed copy at /usr/local/sbin/fleet-update, not the repository's: a merged
# change here would otherwise run as root on the next tick with nobody having read it as root
# code. When the repository's copy differs, the owner is told once, with the command to install
# it. Sourcing the file only defines its functions, so they can be tried one at a time.

set -euo pipefail

REPO=/opt/fleet
DEPLOY=$REPO/deploy
ENV_FILE=$DEPLOY/.env
STATE=/var/lib/fleet-update
BACKUPS=/root/backups
KEEP_BACKUPS=10
CONTAINER=fleet-orchestrator-1
DB_CONTAINER=fleet-postgres-1
NETWORK=fleet_default
WEB=http://127.0.0.1:8090/
NODE_BIN=/usr/local/bin/fleet
# How long a busy node may hold off before it is updated anyway (its runs resume). Counted from
# when the node first fell behind, not per release, so frequent merges cannot postpone it forever.
NODE_DEADLINE_SECS=$((2 * 3600))

log() { echo "fleet-update: $*"; }

# Reads one value from the deploy .env without sourcing it (the file is data, not a script).
env_value() { grep -E "^$1=" "$ENV_FILE" | tail -n1 | cut -d= -f2-; }

# Tells the owner in the fleet's Telegram group. The token goes to curl on stdin, never on its
# command line, where any user on the host could read it from the process list. Bounded, so a
# hung call cannot hold the lock, and logged when Telegram refuses, so silence means delivered.
notify() {
    local text=$1 silent=${2:-false} token chat code
    token=$(env_value TELEGRAM_ORCHESTRATOR_TOKEN || true)
    chat=$(env_value TELEGRAM_CHAT_ID || true)
    [ -n "$token" ] && [ -n "$chat" ] || { log "cannot notify: no Telegram settings"; return 0; }
    code=$(printf 'url = "https://api.telegram.org/bot%s/sendMessage"\n' "$token" |
        curl -sS -K - -o /dev/null -w '%{http_code}' --max-time 20 \
            --data-urlencode "chat_id=$chat" \
            --data-urlencode "text=$text" \
            --data-urlencode "disable_notification=$silent" 2>/dev/null || echo "000")
    [ "$code" = "200" ] || log "notify failed (HTTP $code): $text"
}

# The newest stable release tag on GitHub, from git itself (no API, no rate limit).
latest_release() {
    git -C "$REPO" ls-remote --tags --refs origin 'v*' |
        awk '{sub("refs/tags/", "", $2); print $2}' |
        grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' |
        sort -V | tail -n1
}

# What a binary says it is: "v0.2.4", or "0.1.0-dev" for a hand build.
version_of() { "$@" --version 2>/dev/null | awk '{print $2}'; }

running_version() { docker exec "$CONTAINER" fleet --version 2>/dev/null | awk '{print $2}'; }

container_state() {
    docker inspect -f '{{.State.Status}} {{.State.Restarting}}' "$CONTAINER" 2>/dev/null || echo "missing"
}

# Healthy means: running, not restarting, the expected version, and the web answering —
# continuously for 20 s, so a process that starts and then crashes on its first tick is caught.
healthy() {
    local want=$1 deadline=$((SECONDS + 120)) stable_since="" state version code
    while [ $SECONDS -lt $deadline ]; do
        state=$(container_state)
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
    log "unhealthy: state=[$state] version=[$version] web=[$code] (wanted $want)"
    return 1
}

compose_up() { (cd "$DEPLOY" && docker compose up -d orchestrator) >/dev/null 2>&1; }

# ------------------------------------------------------------------- the orchestrator

update_orchestrator() {
    local want=$1 was=$2 old_head
    if [ -e "$STATE/failed-$want" ]; then
        log "$want failed before; waiting for a newer release"
        return 0
    fi
    old_head=$(git -C "$REPO" rev-parse HEAD)

    # Puts the checkout back where the running image came from, so a later hand deploy
    # (`docker build /opt/fleet`) cannot build the release this run rejected, unchecked.
    # `--keep`, not `--hard`: a local edit to a tracked file on the server is someone's work,
    # and this refuses to move rather than discard it.
    restore_checkout() {
        [ "$(git -C "$REPO" rev-parse HEAD)" = "$old_head" ] && return 0
        git -C "$REPO" reset -q --keep "$old_head" ||
            log "could not put $REPO back on $old_head (local edits in the way?)"
    }

    # Before the swap: nothing is running the new release, so only the checkout needs undoing.
    refuse() {
        restore_checkout
        touch "$STATE/failed-$want"
        log "REFUSED $want: $1"
        notify "❌ Deploy of $want stopped: $1
Still running $was. A newer release will be tried when there is one."
        exit 1
    }

    # After the swap: put the old image back, then check it really came back.
    roll_back() {
        local why=$1
        log "rolling back to $was: $why"
        restore_checkout
        touch "$STATE/failed-$want"
        if ! docker image inspect fleet:previous >/dev/null 2>&1; then
            notify "🚨 $want failed ($why) and there is no previous image to roll back to. The orchestrator may be down: docker logs $CONTAINER"
            exit 1
        fi
        docker tag fleet:previous fleet:local
        if compose_up && healthy "$was"; then
            notify "❌ $want failed ($why) and was rolled back; $was is running and healthy.
Its migrations, if any, stayed applied (they are add-only)."
        else
            notify "🚨 $want failed ($why), and the rollback to $was did not come up healthy either. The orchestrator is probably DOWN: docker logs $CONTAINER"
        fi
        exit 1
    }

    # The web UI fetches affectUI Pro while the image builds, with the owner's access key, handed
    # to the build as a secret (so it is in no layer) from a root-only file (deploy/README.md §8).
    # Without it no release can build. That is a missing setting, not a bad release: so the
    # release is not marked failed (it deploys once the file is there), and the owner is told once.
    local aui=$DEPLOY/affectui.token
    if [ ! -s "$aui" ]; then
        log "waiting for $aui (the affectUI access key) to deploy $want"
        if [ ! -e "$STATE/no-token-$want" ]; then
            touch "$STATE/no-token-$want"
            notify "⏸ $want is waiting: $aui is missing, and the web UI cannot build without the affectUI access key (deploy/README.md §8). Still running $was; it deploys within five minutes of the file appearing."
        fi
        return 0
    fi

    log "deploying $want (running $was)"
    notify "🚀 Deploying $want (running $was)…" true

    git -C "$REPO" fetch -q --tags origin || refuse "could not fetch from GitHub"
    # Only what reached main through the owner's merge: a tag anywhere else is not a release,
    # however it is named, and pushing a tag is something an agent's git credentials can do.
    git -C "$REPO" merge-base --is-ancestor "$want" origin/main 2>/dev/null ||
        refuse "$want is not on main (a tag pushed outside a merge?)"
    git -C "$REPO" merge -q --ff-only "$want" || refuse "$want is not a fast-forward of what is checked out"
    # The image is built from the working tree, so the tree must *be* the release.
    [ "$(git -C "$REPO" rev-parse HEAD)" = "$(git -C "$REPO" rev-parse "$want^{commit}")" ] ||
        refuse "/opt/fleet is ahead of $want (moved by hand?); the next release will catch up"

    docker build -q --secret "id=affectui,src=$aui" --build-arg "FLEET_VERSION=$want" -t "fleet:$want" "$REPO" >/dev/null ||
        refuse "the image did not build"

    # The URL the running orchestrator uses, handed over through the environment, not argv.
    # `--init` so the dry run's process dies with the container, and `timeout` so a stuck one
    # cannot hold its transaction (and its locks) open behind the live orchestrator.
    local db_url report status=0
    db_url=$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$CONTAINER" |
        sed -n 's/^DATABASE_URL=//p' | head -n1)
    [ -n "$db_url" ] || refuse "could not read the orchestrator's DATABASE_URL"
    report=$(DATABASE_URL=$db_url timeout 300 docker run --rm --init --network "$NETWORK" \
        -e DATABASE_URL "fleet:$want" migrate --dry-run 2>&1) || status=$?
    if [ "$status" = 75 ]; then
        # A lock the live orchestrator held, not a broken migration: try again next tick.
        restore_checkout
        log "dry run could not finish (busy database); retrying next tick: $report"
        exit 0
    fi
    [ "$status" = 0 ] || refuse "its migrations failed a dry run against the live database:
$report"
    log "dry run passed: $(echo "$report" | head -n1)"

    local backup=$BACKUPS/fleet-before-$want.sql.gz size tables
    docker exec "$DB_CONTAINER" pg_dump -U fleet -d fleet | gzip >"$backup" ||
        refuse "the database backup failed"
    # A failed pg_dump still writes a small file that looks like a backup; check it is real.
    size=$(stat -c %s "$backup")
    tables=$(zcat "$backup" | grep -c 'CREATE TABLE' || true)
    [ "$size" -gt 100000 ] && [ "$tables" -gt 0 ] ||
        refuse "the database backup looks empty ($size bytes, $tables tables)"
    log "backup $backup ($size bytes, $tables tables)"
    # A full dump per release; keep the newest few.
    ls -1t "$BACKUPS"/fleet-before-v*.sql.gz 2>/dev/null | tail -n +$((KEEP_BACKUPS + 1)) |
        xargs -r rm -f

    # The rollback target is whatever is running now — unless that already *is* this release
    # (a retry after a blip), where retagging would make the rollback target the new image.
    if [ "$(docker image inspect -f '{{.Id}}' fleet:local 2>/dev/null)" != \
         "$(docker image inspect -f '{{.Id}}' "fleet:$want")" ]; then
        docker tag fleet:local fleet:previous 2>/dev/null || true
    fi
    docker tag "fleet:$want" fleet:local
    compose_up || roll_back "compose would not start it"
    healthy "$want" || roll_back "it did not come up healthy"

    log "deployed $want"
    notify "✅ Deployed $want (was $was). Backup: $backup" true
    # Keep this release and the one before it; drop older builds so the disk does not fill.
    docker images --format '{{.Repository}}:{{.Tag}}' fleet |
        grep -vE '^fleet:(local|previous)$' | grep -vx "fleet:$want" |
        xargs -r docker rmi -f >/dev/null 2>&1 || true
    docker image prune -f >/dev/null 2>&1 || true
}

# ------------------------------------------------------------------- this host's node

# The node on this host runs the same binary as the orchestrator image, so it follows it — but
# only once no agent is running on it, or after NODE_DEADLINE_SECS. The fleet's own standing
# sessions (brain, secretary, retrospective) do not count as busy: they wait most of the time,
# and they resume from their sessions by design.
update_node() {
    local want=$1 have node_ids busy since now
    have=$(version_of "$NODE_BIN" || true)
    if [ "$have" = "$want" ]; then
        rm -f "$STATE/node-waiting-since"
        return 0
    fi
    [ -e "$STATE/node-failed-$want" ] && return 0
    # Only from an image that is really that release and really running.
    [ "$(running_version || true)" = "$want" ] || return 0

    # `nodes.name` is not unique (a re-join leaves the old row), so take the one heard from
    # last; and anything but a clean zero counts as busy, so an error never restarts a node
    # that is working.
    node_ids=$(docker exec "$DB_CONTAINER" psql -U fleet -d fleet -At -c \
        "select id from nodes where name = 'vps' order by last_seen desc nulls last limit 1" 2>/dev/null || true)
    # A node id is a hex public key. Checked as exactly that before it goes into SQL, so it is
    # safe to splice whatever the query returned.
    if ! [[ "$node_ids" =~ ^[0-9a-f]{16,128}$ ]]; then
        log "cannot tell which node is this host's ([$node_ids]); leaving it alone"
        return 0
    fi
    busy=$(docker exec "$DB_CONTAINER" psql -U fleet -d fleet -At -c \
        "select count(*) from tasks where running and node_id = '$node_ids'
           and id not in ('brain', 'secretary', 'retro')" 2>/dev/null || echo error)

    now=$(date +%s)
    [ -e "$STATE/node-waiting-since" ] || echo "$now" >"$STATE/node-waiting-since"
    since=$(cat "$STATE/node-waiting-since")
    if [ "$busy" != "0" ] && [ $((now - since)) -lt $NODE_DEADLINE_SECS ]; then
        log "node busy ($busy); waiting to update it to $want"
        return 0
    fi

    local cid copied=0
    cid=$(docker create fleet:local)
    docker cp "$cid:/usr/local/bin/fleet" "$NODE_BIN.new" && copied=1
    docker rm -v "$cid" >/dev/null 2>&1 || true
    [ "$copied" = 1 ] || { log "could not copy the node binary out of the image"; return 0; }
    chmod 755 "$NODE_BIN.new"
    if [ "$(version_of "$NODE_BIN.new")" != "$want" ]; then
        rm -f "$NODE_BIN.new"
        log "the node binary in the image is not $want; leaving the node alone"
        return 0
    fi
    cp "$NODE_BIN" "$NODE_BIN.previous" 2>/dev/null || true
    mv "$NODE_BIN.new" "$NODE_BIN"
    # Not under `set -e`: a failed restart must still reach the restore below.
    if systemctl restart fleet-node && sleep 5 && systemctl is-active -q fleet-node; then
        rm -f "$STATE/node-waiting-since"
        log "node updated to $want (was $have)"
        if [ "$busy" != "0" ]; then
            notify "🔄 vps node updated to $want after waiting $(((now - since) / 60)) min; its runs resume." true
        fi
        return 0
    fi
    # Remembered, so a binary that will not start is not reinstalled — and the node not
    # restarted twice, and the group not pinged — every five minutes until the next release.
    touch "$STATE/node-failed-$want"
    log "node did not start on $want; restoring $have"
    if [ -e "$NODE_BIN.previous" ]; then
        mv "$NODE_BIN.previous" "$NODE_BIN"
        systemctl restart fleet-node || true
    fi
    notify "❌ vps node would not start on $want and was put back on $have. It stays there until the next release."
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

    # An empty answer from a container that is running is a hiccup (a busy `docker exec`), not a
    # missing orchestrator: deploying now would rebuild what is already there. Wait a tick.
    if [ -z "$have" ] && [ "$(container_state)" = "running false" ]; then
        log "the orchestrator is running but did not say its version; trying next tick"
        exit 0
    fi

    if [ "$have" != "$want" ]; then
        update_orchestrator "$want" "${have:-unknown}"
    fi
    update_node "$want"
    check_self
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
