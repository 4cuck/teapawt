//! One-time corrections to stored account state, applied at startup before
//! the pool reads the store.
//!
//! The pool writes down what it concludes about each account, and it never
//! asks X the same question twice. That makes a wrong conclusion permanent:
//! an account benched on a misread answer stays benched across every restart.
//! When such a misreading is found and fixed in the code, the verdicts it
//! already wrote are undone here, once, from the evidence recorded with them.
//!
//! Each repair is recorded in `account_migrations` under its own name, in the
//! same transaction as its changes, so it never runs twice.

use std::env;

use tokio_postgres::{
   NoTls,
   Transaction,
};

use crate::error::{
   Error,
   Result,
};

/// The same database the account store uses.
const DEFAULT_URL: &str = "host=/var/run/postgresql user=teapot dbname=teapot";

/// Shared with `AccountStore` so startup repairs serialize with store writes.
const LOCK_KEY: i64 = 0x7465_6170_6163_6374;

const REPAIR_FALSE_SUSPENSIONS: &str = "repair-false-suspensions-v1";
const REPAIR_PERSIST_REJECTED: &str = "persist-rejected-credentials-v1";
const MAX_REASONABLE_RETRY_UNIX: i64 = 4_000_000_000;

fn db_error(err: tokio_postgres::Error) -> Error {
   Error::Internal(format!("account repair database operation failed: {err}"))
}

/// What one start's repairs changed. All zero when every repair had already
/// been applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
   /// Accounts returned to the search pool.
   pub restored_to_search: usize,
   /// Accounts taken back out of retirement.
   pub unretired:          usize,
   /// Accounts whose suspension flag rested on evidence that proved nothing.
   pub cleared_suspended:  usize,
   /// Accounts whose prior credentials rejection was persisted so restarts and
   /// the 24-hour filter rescan do not keep retrying locked/dead accounts.
   pub persisted_rejected: usize,
}

impl Summary {
   #[must_use]
   pub const fn changed(&self) -> bool {
      self.restored_to_search != 0
         || self.unretired != 0
         || self.cleared_suspended != 0
         || self.persisted_rejected != 0
   }
}

/// Apply every repair that has not been applied yet.
///
/// Must be called before `SessionPool::from_store`: the pool copies the stored
/// state into memory once and writes its own copy back from then on.
pub async fn run() -> Result<Summary> {
   let url = env::var("TEAPOT_LIMITS_DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
   let (mut client, connection) = tokio_postgres::connect(&url, NoTls)
      .await
      .map_err(|_| Error::InvalidConfig("could not connect to the account database".to_owned()))?;
   tokio::spawn(async move {
      // The connection ends when `client` is dropped at the end of `run`.
      let _ = connection.await;
   });
   apply(&mut client).await
}

/// The repairs themselves, on a connection the caller owns. Tests run this
/// against temporary copies of the tables.
pub(crate) async fn apply(client: &mut tokio_postgres::Client) -> Result<Summary> {
   let tx = client.transaction().await.map_err(db_error)?;
   tx.query_one("SELECT pg_advisory_xact_lock($1)", &[&LOCK_KEY])
      .await
      .map_err(db_error)?;

   let has_core_tables: bool = tx
      .query_one(
         "SELECT to_regclass('account_migrations') IS NOT NULL AND \
          to_regclass('account_sessions') IS NOT NULL",
         &[],
      )
      .await
      .map_err(db_error)?
      .try_get(0)
      .map_err(db_error)?;
   if !has_core_tables {
      tx.commit().await.map_err(db_error)?;
      return Ok(Summary::default());
   }

   let false_suspensions_applied = tx
      .query_opt("SELECT 1 FROM account_migrations WHERE name = $1", &[
         &REPAIR_FALSE_SUSPENSIONS,
      ])
      .await
      .map_err(db_error)?
      .is_some();
   let persist_rejected_applied = tx
      .query_opt("SELECT 1 FROM account_migrations WHERE name = $1", &[
         &REPAIR_PERSIST_REJECTED,
      ])
      .await
      .map_err(db_error)?
      .is_some();
   if false_suspensions_applied && persist_rejected_applied {
      tx.commit().await.map_err(db_error)?;
      return Ok(Summary::default());
   }

   let optional_tables = tx
      .query_one(
         "SELECT to_regclass('session_limits') IS NOT NULL, to_regclass('stats_account_events') \
          IS NOT NULL",
         &[],
      )
      .await
      .map_err(db_error)?;
   let has_limits: bool = optional_tables.try_get(0).map_err(db_error)?;
   let has_events: bool = optional_tables.try_get(1).map_err(db_error)?;

   let mut summary = Summary::default();

   if !false_suspensions_applied {
      if has_events {
         restore_search_accounts(&tx, has_limits, &mut summary).await?;
         tag_demoted_search_accounts(&tx, has_limits).await?;
      }

      if has_limits {
         clear_false_suspensions(&tx, has_events, &mut summary).await?;
         tx.execute(
            "DELETE FROM session_limits l WHERE NOT EXISTS (SELECT 1 FROM account_sessions s \
             WHERE s.id = l.id)",
            &[],
         )
         .await
         .map_err(db_error)?;
      }

      tx.execute("INSERT INTO account_migrations (name) VALUES ($1)", &[
         &REPAIR_FALSE_SUSPENSIONS,
      ])
      .await
      .map_err(db_error)?;
   }

   if !persist_rejected_applied {
      if has_limits && has_events {
         persist_rejected_credentials(&tx, &mut summary).await?;
      }
      tx.execute("INSERT INTO account_migrations (name) VALUES ($1)", &[
         &REPAIR_PERSIST_REJECTED,
      ])
      .await
      .map_err(db_error)?;
   }
   tx.commit().await.map_err(db_error)?;

   Ok(summary)
}

async fn persist_rejected_credentials(
   tx: &Transaction<'_>,
   summary: &mut Summary,
) -> Result<()> {
   let updated = tx
      .execute(
         "WITH latest AS (
             SELECT DISTINCT ON (e.session_id) e.session_id, e.kind, e.evidence
             FROM stats_account_events e
             ORDER BY e.session_id, e.observed_unix DESC, e.id DESC
          ),
          rejected AS (
             SELECT s.id
             FROM account_sessions s
             JOIN latest l ON l.session_id = s.id
             WHERE l.kind = 3
                OR (l.kind = 9 AND l.evidence LIKE 'X would not accept this account''s \
                   credentials:%')
          )
          INSERT INTO session_limits (id, body)
          SELECT
             r.id,
             jsonb_build_object(
                'rejected', true,
                'filters_failed', true,
                'filters_retry_at', $1::bigint
             )
          FROM rejected r
          ON CONFLICT (id) DO UPDATE SET
             body = session_limits.body || jsonb_build_object(
                'rejected', true,
                'filters_failed', true,
                'filters_retry_at', $1::bigint
             )
          WHERE COALESCE((session_limits.body->>'rejected')::boolean, false) = false
             OR COALESCE((session_limits.body->>'filters_failed')::boolean, false) = false
             OR (
                CASE
                   WHEN jsonb_typeof(session_limits.body->'filters_retry_at') = 'number'
                      THEN (session_limits.body->>'filters_retry_at')::bigint
                   ELSE 0
                END
             ) <> $1::bigint",
         &[&i64::MAX],
      )
      .await
      .map_err(db_error)?;
   summary.persisted_rejected = usize::try_from(updated).unwrap_or(usize::MAX);
   Ok(())
}

async fn restore_search_accounts(
   tx: &Transaction<'_>,
   has_limits: bool,
   summary: &mut Summary,
) -> Result<()> {
   let query = if has_limits {
      "SELECT s.id
       FROM account_sessions s
       LEFT JOIN session_limits l ON l.id = s.id
       WHERE COALESCE((l.body->>'ct0_dead')::boolean, false) = false
         AND COALESCE(
            (
               SELECT e.kind
               FROM stats_account_events e
               WHERE e.session_id = s.id AND e.kind IN (3, 11)
               ORDER BY e.observed_unix DESC, e.id DESC
               LIMIT 1
            ),
            0
         ) <> 3
         AND (
            EXISTS (
               SELECT 1
               FROM stats_account_events e
               WHERE e.session_id = s.id
                 AND e.evidence = 'Search refused for this account: suspended'
            )
            OR (
               (
                  lower(COALESCE(s.body->>'pool', '')) = 'search'
                  OR lower(COALESCE(s.body->>'demoted_from', '')) = 'search'
               )
               AND (
                  lower(COALESCE(s.body->>'pool', '')) <> 'search'
                  OR s.body ? 'demoted_from'
                  OR COALESCE((l.body->>'search_only')::boolean, false) = false
                  OR (l.body->>'suspended')::boolean IS TRUE
                  OR (l.body->>'retired')::boolean IS TRUE
                  OR (l.body->>'search_banned')::boolean IS TRUE
               )
            )
         )"
   } else {
      "SELECT s.id
       FROM account_sessions s
       WHERE COALESCE(
            (
               SELECT e.kind
               FROM stats_account_events e
               WHERE e.session_id = s.id AND e.kind IN (3, 11)
               ORDER BY e.observed_unix DESC, e.id DESC
               LIMIT 1
            ),
            0
         ) <> 3
         AND (
            EXISTS (
               SELECT 1
               FROM stats_account_events e
               WHERE e.session_id = s.id
                 AND e.evidence = 'Search refused for this account: suspended'
            )
            OR lower(COALESCE(s.body->>'pool', '')) = 'search'
            OR lower(COALESCE(s.body->>'demoted_from', '')) = 'search'
         )"
   };

   let rows = tx.query(query, &[]).await.map_err(db_error)?;
   let candidates: Vec<i64> = rows
      .into_iter()
      .map(|row| row.try_get(0))
      .collect::<std::result::Result<_, _>>()
      .map_err(db_error)?;
   if candidates.is_empty() {
      return Ok(());
   }

   if has_limits {
      let retired_count: i64 = tx
         .query_one(
            "SELECT count(*) FROM session_limits WHERE id = ANY($1) AND \
             (body->>'retired')::boolean IS TRUE",
            &[&candidates],
         )
         .await
         .map_err(db_error)?
         .try_get(0)
         .map_err(db_error)?;
      summary.unretired += usize::try_from(retired_count).unwrap_or(0);
   }

   let moved = tx
      .execute(
         "UPDATE account_sessions SET body = (body - 'demoted_from') || \
          '{\"pool\":\"search\"}'::jsonb WHERE id = ANY($1) AND lower(COALESCE(body->>'pool', \
          '')) <> 'search'",
         &[&candidates],
      )
      .await
      .map_err(db_error)?;
   summary.restored_to_search += usize::try_from(moved).unwrap_or(usize::MAX);

   tx.execute(
      "UPDATE account_sessions SET body = body - 'demoted_from' WHERE id = ANY($1) AND body ? \
       'demoted_from'",
      &[&candidates],
   )
   .await
   .map_err(db_error)?;

   if has_limits {
      tx.execute(
         "UPDATE session_limits SET body = ((body #- '{refused_ops,SearchTimeline}'::text[]) - \
          'search_404_at'::text) || \
          '{\"suspended\":false,\"retired\":false,\"search_banned\":false,\"search_only\":true,\"\
          demoted_from_search\":false,\"filters_failed\":false,\"filters_retry_at\":0}'::jsonb \
          WHERE id = ANY($1)",
         &[&candidates],
      )
      .await
      .map_err(db_error)?;
   }

   Ok(())
}

async fn tag_demoted_search_accounts(tx: &Transaction<'_>, has_limits: bool) -> Result<()> {
   let query = if has_limits {
      "SELECT s.id
       FROM account_sessions s
       LEFT JOIN session_limits l ON l.id = s.id
       WHERE lower(COALESCE(s.body->>'pool', '')) <> 'search'
         AND COALESCE((l.body->>'ct0_dead')::boolean, false) = false
         AND EXISTS (
            SELECT 1
            FROM stats_account_events e
            WHERE e.session_id = s.id
              AND e.evidence = 'Search refused; account moved to the view pool'
         )"
   } else {
      "SELECT s.id
       FROM account_sessions s
       WHERE lower(COALESCE(s.body->>'pool', '')) <> 'search'
         AND EXISTS (
            SELECT 1
            FROM stats_account_events e
            WHERE e.session_id = s.id
              AND e.evidence = 'Search refused; account moved to the view pool'
         )"
   };

   let rows = tx.query(query, &[]).await.map_err(db_error)?;
   let demoted_ids: Vec<i64> = rows
      .into_iter()
      .map(|row| row.try_get(0))
      .collect::<std::result::Result<_, _>>()
      .map_err(db_error)?;
   if demoted_ids.is_empty() {
      return Ok(());
   }

   tx.execute(
      "UPDATE account_sessions SET body = body || '{\"demoted_from\":\"search\"}'::jsonb WHERE id \
       = ANY($1)",
      &[&demoted_ids],
   )
   .await
   .map_err(db_error)?;

   if has_limits {
      tx.execute(
         "UPDATE session_limits SET body = body || '{\"demoted_from_search\":true}'::jsonb WHERE \
          id = ANY($1)",
         &[&demoted_ids],
      )
      .await
      .map_err(db_error)?;
   }

   Ok(())
}

async fn clear_false_suspensions(
   tx: &Transaction<'_>,
   has_events: bool,
   summary: &mut Summary,
) -> Result<()> {
   let query = if has_events {
      "SELECT
          l.id,
          (l.body->>'retired')::boolean IS TRUE,
          (l.body->>'suspended')::boolean IS TRUE
       FROM session_limits l
       JOIN account_sessions s ON s.id = l.id
       WHERE ((l.body->>'suspended')::boolean IS TRUE OR (l.body->>'retired')::boolean IS TRUE)
         AND NOT EXISTS (
            SELECT 1
            FROM stats_account_events e
            WHERE e.session_id = l.id
              AND e.kind = 2
              AND e.endpoint NOT IN ('account/settings', 'account', 'settings')
         )"
   } else {
      "SELECT
          l.id,
          (l.body->>'retired')::boolean IS TRUE,
          (l.body->>'suspended')::boolean IS TRUE
       FROM session_limits l
       JOIN account_sessions s ON s.id = l.id
       WHERE (l.body->>'suspended')::boolean IS TRUE OR (l.body->>'retired')::boolean IS TRUE"
   };

   let rows = tx.query(query, &[]).await.map_err(db_error)?;
   let mut clear_ids = Vec::with_capacity(rows.len());
   for row in rows {
      let id: i64 = row.try_get(0).map_err(db_error)?;
      let was_retired: bool = row.try_get(1).map_err(db_error)?;
      let was_suspended: bool = row.try_get(2).map_err(db_error)?;
      summary.unretired += usize::from(was_retired);
      summary.cleared_suspended += usize::from(was_suspended);
      clear_ids.push(id);
   }
   if clear_ids.is_empty() {
      return Ok(());
   }

   tx.execute(
      "UPDATE session_limits
       SET body = CASE
          WHEN (
             CASE
                WHEN jsonb_typeof(body->'filters_retry_at') = 'number'
                   THEN (body->>'filters_retry_at')::bigint
                ELSE 0
             END
          ) > $2 THEN
             body || \
       '{\"suspended\":false,\"retired\":false,\"search_banned\":false,\"filters_failed\":false,\"\
       filters_retry_at\":0}'::jsonb
          ELSE
             body || '{\"suspended\":false,\"retired\":false,\"search_banned\":false}'::jsonb
       END
       WHERE id = ANY($1)",
      &[&clear_ids, &MAX_REASONABLE_RETRY_UNIX],
   )
   .await
   .map_err(db_error)?;

   Ok(())
}

#[cfg(test)]
mod tests {
   use super::Summary;

   #[test]
   fn summary_changed_reflects_any_non_zero_counter() {
      assert!(!Summary::default().changed());
      assert!(
         Summary {
            restored_to_search: 1,
            ..Summary::default()
         }
         .changed()
      );
      assert!(
         Summary {
            unretired: 2,
            ..Summary::default()
         }
         .changed()
      );
      assert!(
         Summary {
            cleared_suspended: 3,
            ..Summary::default()
         }
         .changed()
      );
      assert!(
         Summary {
            persisted_rejected: 4,
            ..Summary::default()
         }
         .changed()
      );
   }
}
