//! Which nodes may connect: known nodes (rows in `nodes`) plus single-use invites.
//!
//! Checked against the database on every connection, so invites created by another process
//! (`fleet invite` through `docker compose exec`) work at once, and a revoked node (its row
//! deleted in the web UI) cannot reconnect.

use fleet_net::{Authorizer, BoxFuture, EndpointId};
use fleet_proto::Hello;
use sqlx::PgPool;

/// Invites older than this are refused even if unused.
const INVITE_VALID_DAYS: i32 = 7;

#[derive(Clone)]
pub struct DbAuth {
    db: PgPool,
}

impl DbAuth {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }

    async fn check(&self, node: EndpointId, hello: &Hello) -> Result<(), String> {
        let id = node.to_string();
        let known: bool = sqlx::query_scalar("select exists(select 1 from nodes where id = $1)")
            .bind(&id)
            .fetch_one(&self.db)
            .await
            .map_err(|e| format!("orchestrator database error: {e}"))?;
        if known {
            return Ok(());
        }
        let Some(secret) = &hello.join_secret else {
            return Err("this node is not part of the fleet (revoked or never joined); join with a fresh invite".into());
        };
        // Claiming the invite and creating the node row happen in one transaction, so an invite
        // admits exactly one node even when two hosts race with the same ticket.
        let mut tx = self.db.begin().await.map_err(|e| e.to_string())?;
        let claimed = sqlx::query(
            "update invites set used_by = $2 where secret = $1 and used_by is null
             and created_at > now() - make_interval(days => $3)",
        )
        .bind(secret)
        .bind(&id)
        .bind(INVITE_VALID_DAYS)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .rows_affected();
        if claimed == 0 {
            return Err(
                "this invite is invalid, expired or already used; create a new one with /invite"
                    .into(),
            );
        }
        sqlx::query("insert into nodes (id, name, os, app_version) values ($1, $2, $3, $4) on conflict (id) do nothing")
            .bind(&id)
            .bind(&hello.node_name)
            .bind(&hello.os)
            .bind(&hello.app_version)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl Authorizer for DbAuth {
    fn authorize<'a>(
        &'a self,
        node: EndpointId,
        hello: &'a Hello,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.check(node, hello))
    }
}
