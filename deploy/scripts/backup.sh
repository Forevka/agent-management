#!/usr/bin/env bash
# Nightly backup of the fleet stack, run on the VPS host from cron:
#
#   # crontab -e (as a user in the docker group, or root)
#   17 3 * * * /opt/fleet/deploy/scripts/backup.sh >> /var/log/fleet-backup.log 2>&1
#
# Writes to $BACKUP_DIR (default: deploy/backups/) one folder per run:
#   globals.sql          roles (pg_dumpall --globals-only)
#   fleet.dump           database `fleet`      (pg_dump custom format)
#   hindsight.dump       database `hindsight`  (pg_dump custom format)
#   fleet-data.tar.gz    the orchestrator's data dir, including node.key (its
#                        iroh identity: lose it and every node must rejoin)
# and deletes runs older than $KEEP_DAYS (default 14).
#
# Not included, on purpose: deploy/.env, which holds FLEET_MASTER_KEY. Keep an
# offline copy of it yourself. Without that key, the secrets stored in the
# database dumps cannot be decrypted.
#
# Copy the backup folder off the VPS (rclone, restic, rsync...) as well; a
# backup on the same disk does not survive losing the VPS.
#
# Restore (stack stopped except postgres; see README.md, "Restore"):
#   docker compose exec -T postgres pg_restore -U postgres -d fleet --clean --if-exists < fleet.dump
set -euo pipefail

DEPLOY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BACKUP_DIR="${BACKUP_DIR:-$DEPLOY_DIR/backups}"
KEEP_DAYS="${KEEP_DAYS:-14}"

compose() { docker compose --project-directory "$DEPLOY_DIR" -f "$DEPLOY_DIR/docker-compose.yml" "$@"; }

stamp="$(date -u +%Y%m%dT%H%M%SZ)"
out="$BACKUP_DIR/$stamp"
mkdir -p "$out"
chmod 700 "$BACKUP_DIR" "$out"
umask 077

echo "[$(date -Is)] backup -> $out"

compose exec -T postgres pg_dumpall -U postgres --globals-only > "$out/globals.sql"
for db in fleet hindsight; do
	compose exec -T postgres pg_dump -U postgres --format=custom --compress=6 "$db" > "$out/$db.dump"
done

# Read through a throwaway container so this works whether or not the
# orchestrator is running. The image is the orchestrator's own (it has tar).
compose run --rm --no-deps -T --entrypoint tar orchestrator \
	-C /var/lib/fleet -czf - . > "$out/fleet-data.tar.gz"

# A dump that is empty means pg_dump failed after the shell opened the file.
for f in "$out"/*; do
	if [ ! -s "$f" ]; then
		echo "backup FAILED: $f is empty" >&2
		exit 1
	fi
done

find "$BACKUP_DIR" -mindepth 1 -maxdepth 1 -type d -mtime +"$KEEP_DAYS" -exec rm -rf {} +

echo "[$(date -Is)] backup done: $(du -sh "$out" | cut -f1)"
