//! Trying the pending migrations against the real database before deploying them.
//!
//! The test suite cannot see a migration's effect on rows that already exist: CI and the e2e
//! always start from an empty database, so a backfill that collides on production data passes
//! every check and then takes the orchestrator down. That happened on 2026-11-20 (`0018`): its
//! column default numbered existing rows, the backfill skipped three of them, and the unique
//! index refused to build. The orchestrator exits on a failed migration, so it crash-looped
//! behind a 502 until someone noticed.
//!
//! [`dry_run`] runs every migration this binary would apply, in order, inside **one**
//! transaction on the live database, and rolls it back. Postgres DDL is transactional, so this
//! exercises exactly the data the real run will meet and leaves nothing behind. The updater
//! runs it with the *new* image before swapping it in, and refuses to deploy if it fails.

use anyhow::{Context, Result};
use sqlx::PgPool;

/// What a dry run found.
#[derive(Debug)]
pub struct DryRun {
    /// The migrations this binary would apply, oldest first: `(version, description)`.
    pub pending: Vec<(i64, String)>,
    /// The first one that failed, and why. `None` means every pending migration applied.
    pub failed: Option<(i64, String)>,
}

impl DryRun {
    pub fn ok(&self) -> bool {
        self.failed.is_none()
    }
}

/// Applies every pending migration in one transaction, then rolls it back.
///
/// Pending means "embedded in this binary and not recorded as applied". A database that has a
/// migration this binary does not know (it is *older* than the database) is not this function's
/// business: that is a downgrade, and the updater never deploys one.
pub async fn dry_run(db: &PgPool) -> Result<DryRun> {
    // A brand-new database has no table yet: everything is pending.
    let has_table: bool =
        sqlx::query_scalar("select to_regclass('public._sqlx_migrations') is not null")
            .fetch_one(db)
            .await?;
    let applied: Vec<i64> = if has_table {
        sqlx::query_scalar("select version from _sqlx_migrations where success")
            .fetch_all(db)
            .await?
    } else {
        Vec::new()
    };
    let pending: Vec<&sqlx::migrate::Migration> = crate::MIGRATOR
        .iter()
        .filter(|m| !applied.contains(&m.version))
        .collect();

    let mut tx = db
        .begin()
        .await
        .context("opening the dry-run transaction")?;
    let mut failed = None;
    for m in &pending {
        // A migration that opted out of transactions (`-- no-transaction`, e.g. `create index
        // concurrently`) cannot be tried and rolled back. Fail safe: the updater stops, and a
        // person deploys that one by hand.
        if m.no_tx {
            failed = Some((
                m.version,
                "runs outside a transaction, so it cannot be dry-run: deploy it by hand".into(),
            ));
            break;
        }
        // `raw_sql`, not `query`: a migration file is several statements.
        if let Err(e) = sqlx::raw_sql(m.sql.as_ref()).execute(&mut *tx).await {
            failed = Some((m.version, format!("{e}")));
            break;
        }
    }
    // Whatever happened, nothing is kept. An error above has already aborted the transaction,
    // and Postgres rolls an aborted one back either way.
    tx.rollback().await.context("rolling the dry run back")?;

    Ok(DryRun {
        pending: pending
            .iter()
            .map(|m| (m.version, m.description.to_string()))
            .collect(),
        failed,
    })
}

#[cfg(test)]
mod tests {
    use super::dry_run;

    /// A fresh, empty database (no migrations), or `None` when no test Postgres is configured.
    async fn empty_db() -> Option<sqlx::PgPool> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let admin = sqlx::PgPool::connect(&url)
            .await
            .expect("TEST_DATABASE_URL unreachable");
        let name = format!(
            "fleet_preflight_{}",
            crate::web::random_token()
                .to_lowercase()
                .replace(['-', '_'], "")
        );
        let name = &name[..name.len().min(40)];
        sqlx::query(&format!("create database {name}"))
            .execute(&admin)
            .await
            .unwrap();
        let base = url.rsplit_once('/').map(|(b, _)| b.to_owned()).unwrap();
        Some(
            sqlx::PgPool::connect(&format!("{base}/{name}"))
                .await
                .unwrap(),
        )
    }

    async fn tables(db: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar(
            "select count(*) from information_schema.tables where table_schema = 'public'",
        )
        .fetch_one(db)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_dry_run_applies_everything_and_keeps_nothing() {
        let Some(db) = empty_db().await else {
            eprintln!("skipped: TEST_DATABASE_URL not set");
            return;
        };
        let run = dry_run(&db).await.unwrap();
        assert!(
            run.ok(),
            "every migration applies to an empty database: {run:?}"
        );
        assert_eq!(run.pending.len(), crate::MIGRATOR.iter().count());
        assert_eq!(tables(&db).await, 0, "a dry run leaves nothing behind");

        // Once migrated for real, there is nothing left to try.
        crate::MIGRATOR.run(&db).await.unwrap();
        let run = dry_run(&db).await.unwrap();
        assert!(run.ok() && run.pending.is_empty(), "{run:?}");
    }

    /// The case the dry run exists for: a migration that collides with what the database
    /// already holds, which no empty test database can show. It must fail, say where, and change
    /// nothing — not the colliding state, not the ledger.
    #[tokio::test]
    async fn a_migration_that_collides_with_existing_state_is_caught_and_nothing_changes() {
        let Some(db) = empty_db().await else {
            eprintln!("skipped: TEST_DATABASE_URL not set");
            return;
        };
        crate::MIGRATOR.run(&db).await.unwrap();
        // Make the newest migration pending again while its table is still there, so applying
        // it collides — the shape of a backfill meeting rows it did not expect.
        let last = crate::MIGRATOR.iter().map(|m| m.version).max().unwrap();
        sqlx::query("delete from _sqlx_migrations where version = $1")
            .bind(last)
            .execute(&db)
            .await
            .unwrap();
        let before = tables(&db).await;

        let run = dry_run(&db).await.unwrap();
        let (at, why) = run.failed.clone().expect("the collision is caught");
        assert_eq!(at, last, "it names the migration that failed: {run:?}");
        assert!(
            why.contains("already exists"),
            "it says why, in the database's words: {why}"
        );

        assert_eq!(
            tables(&db).await,
            before,
            "the colliding state is left exactly as it was"
        );
        let ledger: Option<i64> = sqlx::query_scalar("select max(version) from _sqlx_migrations")
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(ledger < Some(last), "nothing was recorded as applied");
    }
}
