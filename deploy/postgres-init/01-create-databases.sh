#!/bin/bash
# Runs once, when the postgres_data volume is empty (first `docker compose up`).
# To re-run it you must delete the volume, which deletes all data.
#
# Creates one role and one database per application, so neither can read the
# other's data and each can run its own migrations as the database owner:
#   fleet     / fleet      (orchestrator)
#   hindsight / hindsight  (memory server)
#
# Extensions are created here as the superuser because the application roles
# are not superusers. Hindsight checks for them and skips CREATE EXTENSION
# when they already exist.
set -euo pipefail

: "${FLEET_DB_PASSWORD:?FLEET_DB_PASSWORD is not set}"
: "${HINDSIGHT_DB_PASSWORD:?HINDSIGHT_DB_PASSWORD is not set}"

# Passwords go in as psql variables (:'name' quotes them), never spliced into
# the SQL text.
psql -v ON_ERROR_STOP=1 \
	--username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
	-v fleet_pw="$FLEET_DB_PASSWORD" \
	-v hindsight_pw="$HINDSIGHT_DB_PASSWORD" <<'EOSQL'
CREATE ROLE fleet LOGIN PASSWORD :'fleet_pw';
CREATE DATABASE fleet OWNER fleet;

CREATE ROLE hindsight LOGIN PASSWORD :'hindsight_pw';
CREATE DATABASE hindsight OWNER hindsight;

\connect hindsight
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS btree_gin;

\connect fleet
CREATE EXTENSION IF NOT EXISTS vector;
EOSQL

echo "postgres-init: created databases fleet and hindsight"
