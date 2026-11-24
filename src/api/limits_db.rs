//! One row per account in Postgres.
//!
//! The JSON file rewrote every account on each flush. A session change only
//! touches one account, and that update is the write that has to be fast.

use std::{
   collections::{
      HashMap,
      HashSet,
   },
   env,
   result::Result as StdResult,
   time::Duration,
};

use tokio::{
   sync::mpsc,
   time::{
      Instant,
      sleep_until,
      timeout,
   },
};
use tokio_postgres::{
   Client,
   Error as PgError,
   NoTls,
   Statement,
};

use crate::{
   error::{
      Error,
      Result,
   },
   types::SessionLimits,
};

const DEFAULT_URL: &str = "host=/var/run/postgresql user=teapot dbname=teapot";
/// How long dirty account IDs are coalesced before flushing one batch.
const FLUSH_EVERY: Duration = Duration::from_secs(2);
/// Flush before [`FLUSH_EVERY`] elapses once this many distinct accounts are
/// dirty.
const MAX_BATCH_SIZE: usize = 512;
/// How long one attempt to reopen the connection may take.
const REOPEN_PATIENCE: Duration = Duration::from_secs(5);

const UPSERT: &str = "\
INSERT INTO session_limits (id, body) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET body = \
                      EXCLUDED.body";

const BATCH_UPSERT: &str = "\
INSERT INTO session_limits (id, body) SELECT * FROM UNNEST($1::bigint[], $2::jsonb[]) ON CONFLICT \
                            (id) DO UPDATE SET body = EXCLUDED.body";

/// Handle for saving one account without rewriting the rest.
#[derive(Clone)]
pub struct LimitsDb {
   tx: mpsc::UnboundedSender<i64>,
}

pub struct Connected {
   client: Client,
}

/// A connection, driven until the server or this process closes it.
async fn open() -> StdResult<Client, PgError> {
   let url = env::var("TEAPOT_LIMITS_DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
   let (client, connection) = tokio_postgres::connect(&url, NoTls).await?;
   tokio::spawn(async move {
      if let Err(err) = connection.await {
         tracing::warn!("postgres limits connection closed: {err}");
      }
   });
   Ok(client)
}

/// The writer's connection and the upsert prepared on it. A statement belongs
/// to the connection that prepared it, so the two are replaced together.
///
/// PostgreSQL closes every session when it restarts, and when it recovers
/// from one crashed backend. The writer used to keep the closed client: after
/// one such recovery every flush failed, and no account's limits were saved,
/// until this process was restarted as well.
struct Writer {
   client: Client,
   upsert: Statement,
}

impl Writer {
   async fn prepare(client: Client) -> StdResult<Self, PgError> {
      let upsert = client.prepare(BATCH_UPSERT).await?;
      Ok(Self { client, upsert })
   }

   /// A new connection, if the server is giving any out.
   async fn reopen() -> Option<Self> {
      let opened = timeout(REOPEN_PATIENCE, async {
         Self::prepare(open().await?).await
      })
      .await;
      match opened {
         Ok(Ok(writer)) => {
            tracing::info!("postgres limits connection reopened");
            Some(writer)
         },
         Ok(Err(err)) => {
            tracing::warn!("postgres limits connection not reopened: {err}");
            None
         },
         Err(_) => {
            tracing::warn!("postgres limits connection not reopened: no answer");
            None
         },
      }
   }

   /// Send one batch. `false` when the connection closed under it, and the
   /// server may not have it. A batch the server answered with an error is
   /// not one to send again.
   async fn send(&self, ids: &[i64], bodies: &[serde_json::Value]) -> bool {
      if ids.is_empty() {
         return true;
      }
      let Err(err) = self.client.execute(&self.upsert, &[&ids, &bodies]).await else {
         return true;
      };
      tracing::warn!(rows = ids.len(), "postgres session update failed: {err}");
      !self.client.is_closed()
   }
}

/// Gather the accounts to write next into `pending`. `true` once every handle
/// is dropped and no more will arrive.
///
/// With nothing pending this waits for one account, then for the rest until
/// [`FLUSH_EVERY`] has passed or [`MAX_BATCH_SIZE`] are dirty. After a flush
/// that did not reach the server (`unsent`) its accounts are pending already:
/// the whole of `FLUSH_EVERY` then passes before the next attempt, however
/// many are waiting, and the queue keeps emptying into the set meanwhile.
async fn collect(
   rx: &mut mpsc::UnboundedReceiver<i64>,
   pending: &mut HashSet<i64>,
   unsent: bool,
) -> bool {
   let enough = if unsent { usize::MAX } else { MAX_BATCH_SIZE };
   if pending.is_empty() {
      let Some(first) = rx.recv().await else {
         return true;
      };
      pending.insert(first);
   }
   // Drain any IDs already buffered before arming the debounce timer.
   while pending.len() < enough
      && let Ok(extra) = rx.try_recv()
   {
      pending.insert(extra);
   }
   if pending.len() >= enough {
      return false;
   }
   let timer = sleep_until(Instant::now() + FLUSH_EVERY);
   tokio::pin!(timer);
   while pending.len() < enough {
      tokio::select! {
         biased;
         () = &mut timer => break,
         next = rx.recv() => match next {
            Some(id) => {
               pending.insert(id);
            },
            None => return true,
         },
      }
   }
   false
}

impl Connected {
   pub async fn connect() -> Result<Self> {
      let client = open()
         .await
         .map_err(|err| Error::InvalidConfig(format!("postgres: {err}")))?;
      client
         .batch_execute(
            "CREATE TABLE IF NOT EXISTS session_limits (
               id bigint PRIMARY KEY,
               body jsonb NOT NULL
            )",
         )
         .await
         .map_err(|err| Error::InvalidConfig(format!("postgres schema: {err}")))?;
      let _ = client
         .batch_execute(
            "ALTER TABLE session_limits SET (fillfactor = 70, autovacuum_vacuum_scale_factor = \
             0.02, autovacuum_vacuum_cost_limit = 1000)",
         )
         .await;
      Ok(Self { client })
   }

   pub async fn load(&self) -> Result<HashMap<i64, SessionLimits>> {
      let rows = self
         .client
         .query("SELECT id, body::text FROM session_limits", &[])
         .await
         .map_err(|err| Error::InvalidConfig(format!("postgres load: {err}")))?;
      let mut limits = HashMap::with_capacity(rows.len());
      for row in rows {
         let id: i64 = row.get(0);
         let body: String = row.get(1);
         match serde_json::from_str::<SessionLimits>(&body) {
            Ok(saved) => {
               limits.insert(id, saved);
            },
            Err(err) => tracing::warn!(id, "ignoring unreadable session row: {err}"),
         }
      }
      Ok(limits)
   }

   /// First boot: the JSON file is the copy we already have. One transaction
   /// moves it into the table, and later changes update a single row.
   #[allow(
      dead_code,
      reason = "retained for standalone JSON-to-Postgres migration"
   )]
   pub async fn import(&mut self, limits: &HashMap<i64, SessionLimits>) -> Result<()> {
      let transaction = self
         .client
         .transaction()
         .await
         .map_err(|err| Error::InvalidConfig(format!("postgres import: {err}")))?;
      let statement = transaction
         .prepare(UPSERT)
         .await
         .map_err(|err| Error::InvalidConfig(format!("postgres import: {err}")))?;
      for (id, limits) in limits {
         let body = serde_json::to_value(limits)?;
         transaction
            .execute(&statement, &[id, &body])
            .await
            .map_err(|err| Error::InvalidConfig(format!("postgres import: {err}")))?;
      }
      transaction
         .commit()
         .await
         .map_err(|err| Error::InvalidConfig(format!("postgres import: {err}")))
   }

   pub fn spawn(
      self,
      snapshot: impl Fn(i64) -> Option<SessionLimits> + Send + 'static,
   ) -> LimitsDb {
      let (tx, mut rx) = mpsc::unbounded_channel::<i64>();
      tokio::spawn(async move {
         let mut writer = match Writer::prepare(self.client).await {
            Ok(writer) => Some(writer),
            Err(err) => {
               tracing::warn!("postgres limits writer not prepared: {err}");
               None
            },
         };
         let mut pending = HashSet::with_capacity(MAX_BATCH_SIZE);
         // The last flush did not reach the server.
         let mut unsent = false;
         loop {
            let closed = collect(&mut rx, &mut pending, unsent).await;
            if !pending.is_empty() {
               if writer
                  .as_ref()
                  .is_none_or(|writer| writer.client.is_closed())
               {
                  writer = Writer::reopen().await;
               }
               unsent = if let Some(ref writer) = writer {
                  // Every response queues its account, so a busy one is
                  // queued many times over. Coalescing in a set writes its
                  // row once per flush window, as it is now.
                  let (batch_ids, batch_bodies) = encode_batch(&mut pending, &snapshot);
                  let sent = writer.send(&batch_ids, &batch_bodies).await;
                  if !sent {
                     // Its accounts wait for the next connection, and are
                     // written as they are by then.
                     pending.extend(batch_ids);
                  }
                  !sent
               } else {
                  true
               };
            }
            if closed {
               break;
            }
         }
      });
      LimitsDb { tx }
   }
}

fn encode_batch(
   pending: &mut HashSet<i64>,
   snapshot: &impl Fn(i64) -> Option<SessionLimits>,
) -> (Vec<i64>, Vec<serde_json::Value>) {
   let rows = pending
      .drain()
      .filter_map(|id| Some((id, snapshot(id)?)))
      .collect::<Vec<_>>();
   let mut batch_ids = Vec::with_capacity(rows.len());
   let mut batch_bodies = Vec::with_capacity(rows.len());
   for (id, saved) in rows {
      match serde_json::to_value(&saved) {
         Ok(body) => {
            batch_ids.push(id);
            batch_bodies.push(body);
         },
         Err(err) => tracing::warn!(id, "session state was not encoded: {err}"),
      }
   }
   (batch_ids, batch_bodies)
}

impl LimitsDb {
   pub fn save(&self, id: i64) {
      let _ = self.tx.send(id);
   }
}

#[cfg(test)]
mod tests {
   use std::{
      env,
      sync::{
         Arc,
         Mutex,
         PoisonError,
      },
      time::Duration,
   };

   use tokio::time::{
      sleep,
      timeout,
   };
   use tokio_postgres::{
      Client,
      NoTls,
   };

   use super::*;

   #[tokio::test]
   #[ignore = "requires an isolated PostgreSQL cluster on a /tmp Unix socket"]
   async fn a_closed_connection_is_reopened_and_the_batch_is_written() {
      let url = env::var("TEAPOT_LIMITS_DATABASE_URL").unwrap();
      assert!(url.starts_with("host=/tmp/teapot-runtime-test-"));
      let (checker, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
      tokio::spawn(connection);
      let connected = Connected::connect().await.unwrap();
      let writer = other_backend(&checker).await;
      let current = Arc::new(Mutex::new(SessionLimits {
         suspended: true,
         ..SessionLimits::default()
      }));
      let snapshot = Arc::clone(&current);
      let db = connected.spawn(move |id| {
         (id == 7).then(|| {
            snapshot
               .lock()
               .unwrap_or_else(PoisonError::into_inner)
               .clone()
         })
      });

      db.save(7);
      stored(&checker, true).await;

      checker
         .batch_execute(&format!("SELECT pg_terminate_backend({writer})"))
         .await
         .unwrap();
      gone(&checker, writer).await;
      current
         .lock()
         .unwrap_or_else(PoisonError::into_inner)
         .suspended = false;
      db.save(7);
      stored(&checker, false).await;
   }

   async fn stored(checker: &Client, suspended: bool) {
      let found = timeout(Duration::from_secs(20), async {
         loop {
            let flag = checker
               .query_opt(
                  "SELECT (body->>'suspended')::boolean FROM session_limits WHERE id = 7",
                  &[],
               )
               .await
               .ok()
               .flatten()
               .and_then(|row| row.try_get::<_, Option<bool>>(0).ok().flatten());
            if flag == Some(suspended) {
               break;
            }
            sleep(Duration::from_millis(50)).await;
         }
      })
      .await;
      assert!(found.is_ok(), "suspended was not {suspended} within 20s");
   }

   async fn other_backend(checker: &Client) -> i32 {
      let rows = checker
         .query(
            "SELECT pid FROM pg_stat_activity WHERE backend_type = 'client backend' AND pid <> \
             pg_backend_pid()",
            &[],
         )
         .await
         .unwrap();
      assert_eq!(rows.len(), 1, "expected the writer connection alone");
      rows[0].get(0)
   }

   async fn gone(checker: &Client, pid: i32) {
      let found = timeout(Duration::from_secs(5), async {
         loop {
            if let Ok(gone) = checker
               .query_opt("SELECT 1 FROM pg_stat_activity WHERE pid = $1", &[&pid])
               .await
               && gone.is_none()
            {
               break;
            }
            sleep(Duration::from_millis(20)).await;
         }
      })
      .await;
      assert!(found.is_ok(), "terminated backend {pid} was still running");
   }

   #[test]
   fn encode_batch_drains_distinct_ids_and_skips_missing_snapshots() {
      let mut pending = HashSet::from([10_i64, 20, 30]);
      let (mut ids, bodies) =
         encode_batch(&mut pending, &|id| (id != 20).then(SessionLimits::default));
      assert!(pending.is_empty());
      ids.sort_unstable();
      assert_eq!(ids, [10, 30]);
      assert_eq!(bodies.len(), 2);
   }
}
