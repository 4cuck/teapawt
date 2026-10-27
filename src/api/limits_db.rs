//! One row per account in Postgres.
//!
//! The JSON file rewrote every account on each flush. A session change only
//! touches one account, and that update is the write that has to be fast.

use std::{collections::HashMap, env};

use tokio::sync::{mpsc, oneshot};
use tokio_postgres::NoTls;

use crate::{
   error::{Error, Result},
   types::SessionLimits,
};

const DEFAULT_URL: &str = "host=/var/run/postgresql user=teapot dbname=teapot";

const UPSERT: &str = "\
INSERT INTO session_limits (id, body) VALUES ($1, $2) \
ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body";

struct Job {
   id: i64,
   done: Option<oneshot::Sender<()>>,
}

/// Handle for saving one account without rewriting the rest.
#[derive(Clone)]
pub struct LimitsDb {
   tx: mpsc::UnboundedSender<Job>,
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
      limits: std::sync::Arc<tokio::sync::RwLock<HashMap<i64, SessionLimits>>>,
   ) -> LimitsDb {
      let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
      tokio::spawn(async move {
         let client = self.client;
         let statement = match client.prepare(UPSERT).await {
            Ok(statement) => statement,
            Err(err) => {
               tracing::warn!("postgres limits writer stopped: {err}");
               return;
            },
         };
         while let Some(job) = rx.recv().await {
            let mut jobs = vec![job];
            while let Ok(extra) = rx.try_recv() {
               jobs.push(extra);
            }
            for job in jobs {
               let saved = limits.read().await.get(&job.id).cloned();
               if let Some(saved) = saved {
                  match serde_json::to_value(&saved) {
                     Ok(body) => {
                        if let Err(err) = client.execute(&statement, &[&job.id, &body]).await {
                           tracing::warn!(id = job.id, "postgres session update failed: {err}");
                        }
                     },
                     Err(err) => {
                        tracing::warn!(id = job.id, "session state was not encoded: {err}")
                     },
                  }
               }
               if let Some(done) = job.done {
                  let _ = done.send(());
               }
            }
         }
      });
      LimitsDb { tx }
   }
}

impl LimitsDb {
   pub fn save(&self, id: i64) {
      let _ = self.tx.send(Job { id, done: None });
   }

   pub async fn save_now(&self, id: i64) {
      let (done, received) = oneshot::channel();
      if self.tx.send(Job { id, done: Some(done) }).is_err() {
         return;
      }
      let _ = received.await;
   }
}
