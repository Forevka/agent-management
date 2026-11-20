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
    /// The failure was a wait that ran out (a lock held by the live orchestrator, or a slow
    /// statement), not something wrong with the migration: worth trying again in a few minutes.
    /// The updater must not give up on a release for this.
    pub transient: bool,
}

impl DryRun {
    pub fn ok(&self) -> bool {
        self.failed.is_none()
    }
}

/// Applies every pending migration in one transaction, then rolls it back.
///
/// Pending means "embedded in this binary and not recorded as applied". A database that has a
/// migration this binary does not know is accepted, as a real start accepts it (`MIGRATOR`
/// ignores those, which is what lets a rollback start).
pub async fn dry_run(db: &PgPool) -> Result<DryRun> {
    // A brand-new database has no table yet: everything is pending.
    let has_table: bool =
        sqlx::query_scalar("select to_regclass('public._sqlx_migrations') is not null")
            .fetch_one(db)
            .await?;
    let applied: Vec<(i64, bool, Vec<u8>)> = if has_table {
        sqlx::query_as("select version, success, checksum from _sqlx_migrations order by version")
            .fetch_all(db)
            .await?
    } else {
        Vec::new()
    };

    // Refuse what a real start would refuse, before trying anything — otherwise the dry run
    // passes, the swap happens, and the new orchestrator exits at startup. These are the checks
    // `Migrator::run` makes before applying anything.
    let refuse = |v: i64, why: &str| DryRun {
        pending: Vec::new(),
        failed: Some((v, why.to_owned())),
        transient: false,
    };
    if let Some((v, _, _)) = applied.iter().find(|(_, ok, _)| !ok) {
        return Ok(refuse(
            *v,
            "an earlier attempt left it half-applied (marked unsuccessful); a person has to \
             repair it before anything else is deployed",
        ));
    }
    for m in crate::MIGRATOR.iter() {
        if let Some((v, _, sum)) = applied.iter().find(|(v, _, _)| *v == m.version)
            && sum.as_slice() != m.checksum.as_ref()
        {
            return Ok(refuse(
                *v,
                "the applied migration differs from the one in this release (an applied \
                 migration was edited; migrations are add-only)",
            ));
        }
    }

    let pending: Vec<&sqlx::migrate::Migration> = crate::MIGRATOR
        .iter()
        .filter(|m| !applied.iter().any(|(v, _, _)| *v == m.version))
        .collect();

    let mut tx = db
        .begin()
        .await
        .context("opening the dry-run transaction")?;
    // This runs against the live database while the live orchestrator is using it. An
    // `alter table` takes an exclusive lock and holds it until the rollback, and while it *waits*
    // for that lock every ordinary query on the table queues behind it. Bounded waits mean a busy
    // table fails the dry run quickly — the updater tries again on the next tick — instead of
    // stalling the fleet it is there to protect.
    sqlx::query("set local lock_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("set local statement_timeout = '120s'")
        .execute(&mut *tx)
        .await?;
    let mut failed = None;
    let mut transient = false;
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
            // 55P03 lock_not_available and 57014 query_canceled are the two waits bounded
            // above running out: the live orchestrator held a lock, not a broken migration.
            transient = e
                .as_database_error()
                .and_then(|d| d.code())
                .is_some_and(|c| c == "55P03" || c == "57014");
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
        transient,
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

    /// The rollback case. A release added a migration, failed its health check, and the previous
    /// release — which does not know that migration — has to start against the database. With
    /// sqlx's default it refuses (`VersionMissing`) and the "rollback" crash-loops; `MIGRATOR`
    /// ignores unknown applied migrations so it can take over. The dry run must agree.
    #[tokio::test]
    async fn an_older_release_starts_against_a_database_that_is_ahead_of_it() {
        let Some(db) = empty_db().await else {
            eprintln!("skipped: TEST_DATABASE_URL not set");
            return;
        };
        crate::MIGRATOR.run(&db).await.unwrap();
        // A migration from a release newer than this binary.
        sqlx::query(
            "insert into _sqlx_migrations (version, description, success, checksum, execution_time)
             values (99999, 'from a newer release', true, '\\x00', 0)",
        )
        .execute(&db)
        .await
        .unwrap();
        crate::MIGRATOR
            .run(&db)
            .await
            .expect("the previous release starts, which is what makes a rollback possible");
        let run = dry_run(&db).await.unwrap();
        assert!(
            run.ok(),
            "the dry run accepts what a real start accepts: {run:?}"
        );
    }

    /// What a real start refuses, the dry run refuses first — otherwise it says "passed", the
    /// image is swapped, and the new orchestrator exits at startup.
    #[tokio::test]
    async fn a_database_a_real_start_would_refuse_is_refused_before_anything_is_tried() {
        let Some(db) = empty_db().await else {
            eprintln!("skipped: TEST_DATABASE_URL not set");
            return;
        };
        crate::MIGRATOR.run(&db).await.unwrap();

        // An applied migration that was edited afterwards (sqlx: VersionMismatch).
        sqlx::query("update _sqlx_migrations set checksum = '\\x00' where version = 1")
            .execute(&db)
            .await
            .unwrap();
        let run = dry_run(&db).await.unwrap();
        let (v, why) = run.failed.clone().expect("an edited migration is refused");
        assert_eq!(v, 1);
        assert!(why.contains("edited"), "{why}");
        assert!(!run.transient, "an edited migration is not worth retrying");
        crate::MIGRATOR
            .run(&db)
            .await
            .expect_err("and a real start refuses it too");

        // One an earlier attempt left half-applied (sqlx: Dirty).
        let checksum: Vec<u8> = crate::MIGRATOR
            .iter()
            .find(|m| m.version == 1)
            .unwrap()
            .checksum
            .to_vec();
        sqlx::query("update _sqlx_migrations set checksum = $1 where version = 1")
            .bind(&checksum)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("update _sqlx_migrations set success = false where version = 2")
            .execute(&db)
            .await
            .unwrap();
        let run = dry_run(&db).await.unwrap();
        let (v, why) = run
            .failed
            .clone()
            .expect("a half-applied migration is refused");
        assert_eq!(v, 2);
        assert!(why.contains("half-applied"), "{why}");
        assert!(!run.transient);
    }

    /// The dry run runs against the live database while the orchestrator uses it. When a table
    /// it needs is locked, it must give up quickly and say "try again" — not wait indefinitely
    /// while every other query on that table queues behind it, and not report the release as
    /// broken.
    #[tokio::test]
    async fn a_dry_run_blocked_by_the_live_orchestrator_gives_up_quickly_and_says_try_again() {
        let Some(db) = empty_db().await else {
            eprintln!("skipped: TEST_DATABASE_URL not set");
            return;
        };
        crate::MIGRATOR.run(&db).await.unwrap();
        // Make 0019 (task_links, which references `tasks`) pending again, cleanly.
        sqlx::query("drop table task_links")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("delete from _sqlx_migrations where version = 19")
            .execute(&db)
            .await
            .unwrap();

        // Someone else holds `tasks` exclusively, as a long transaction of the live orchestrator
        // might.
        let mut holder = db.begin().await.unwrap();
        sqlx::query("lock table tasks in access exclusive mode")
            .execute(&mut *holder)
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let run = dry_run(&db).await.unwrap();
        let took = started.elapsed();
        holder.rollback().await.unwrap();

        assert_eq!(run.failed.as_ref().map(|(v, _)| *v), Some(19), "{run:?}");
        assert!(
            run.transient,
            "a lock wait is a reason to retry, not a broken migration: {run:?}"
        );
        assert!(
            took < std::time::Duration::from_secs(30),
            "it gave up on the lock instead of waiting it out: {took:?}"
        );

        // With the lock gone, the same migration passes.
        let run = dry_run(&db).await.unwrap();
        assert!(run.ok(), "{run:?}");
    }
}
