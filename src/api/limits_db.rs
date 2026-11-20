//! One row per account in Postgres.
//!
//! The JSON file rewrote every account on each flush. A session change only
//! touches one account, and that update is the write that has to be fast.

use std::{
   collections::HashMap,
   env,
};

use tokio::sync::mpsc;
use tokio_postgres::NoTls;

use crate::{
   error::{
      Error,
      Result,
   },
   types::SessionLimits,
};

const DEFAULT_URL: &str = "host=/var/run/postgresql user=teapot dbname=teapot";

const UPSERT: &str = "\
INSERT INTO session_limits (id, body) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET body = \
                      EXCLUDED.body";

const BATCH_UPSERT: &str = "\
INSERT INTO session_limits (id, body) SELECT * FROM UNNEST($1::bigint[], $2::jsonb[]) ON \
                            CONFLICT (id) DO UPDATE SET body = EXCLUDED.body";

/// Handle for saving one account without rewriting the rest.
#[derive(Clone)]
pub struct LimitsDb {
   tx: mpsc::UnboundedSender<i64>,
}

pub struct Connected {
   client: tokio_postgres::Client,
}

impl Connected {
   pub async fn connect() -> Result<Self> {
      let url = env::var("TEAPOT_LIMITS_DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
      let (client, connection) = tokio_postgres::connect(&url, NoTls)
         .await
         .map_err(|err| Error::InvalidConfig(format!("postgres: {err}")))?;
      tokio::spawn(async move {
         if let Err(err) = connection.await {
            tracing::warn!("postgres limits connection closed: {err}");
         }
      });
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
            "ALTER TABLE session_limits SET (fillfactor = 70, \
             autovacuum_vacuum_scale_factor = 0.02, autovacuum_vacuum_cost_limit = 1000)",
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
   #[allow(dead_code, reason = "retained for standalone JSON-to-Postgres migration")]
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
         let client = self.client;
         let statement = match client.prepare(BATCH_UPSERT).await {
            Ok(statement) => statement,
            Err(err) => {
               tracing::warn!("postgres limits writer stopped: {err}");
               return;
            },
         };
         while let Some(first) = rx.recv().await {
            let mut ids = vec![first];
            while let Ok(extra) = rx.try_recv() {
               ids.push(extra);
            }
            // Every response queues its account, so a busy one is queued
            // many times over. Its row is written once, as it is now.
            ids.sort_unstable();
            ids.dedup();
            let rows = ids
               .iter()
               .filter_map(|&id| Some((id, snapshot(id)?)))
               .collect::<Vec<_>>();
            let mut batch_ids: Vec<i64> = Vec::with_capacity(rows.len());
            let mut batch_bodies: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
            for (id, saved) in rows {
               match serde_json::to_value(&saved) {
                  Ok(body) => {
                     batch_ids.push(id);
                     batch_bodies.push(body);
                  },
                  Err(err) => tracing::warn!(id, "session state was not encoded: {err}"),
               }
            }
            if !batch_ids.is_empty()
               && let Err(err) = client.execute(&statement, &[&batch_ids, &batch_bodies]).await
            {
               tracing::warn!(
                  rows = batch_ids.len(),
                  "postgres session update failed: {err}"
               );
            }
         }
      });
      LimitsDb { tx }
   }
}

impl LimitsDb {
   pub fn save(&self, id: i64) {
      let _ = self.tx.send(id);
   }
}
