//! Username and numeric id, in Postgres.
//!
//! `UserTweets` needs that id. Without it, the profile waits for
//! `UserByScreenName` to finish and only then asks for the posts. The id does
//! not change when the account posts, so the row is not a copy of the page.
//! The maps are the working copy of the table. There is no size cap: Postgres
//! is what remembers them across a restart.

use std::{
   collections::HashMap,
   env,
   sync::{OnceLock, RwLock},
};

use tokio::sync::mpsc;
use tokio_postgres::NoTls;

const DEFAULT_URL: &str = "host=/var/run/postgresql user=teapot dbname=teapot";

const UPSERT: &str = "\
INSERT INTO user_ids (username, id) VALUES ($1, $2) \
ON CONFLICT (username) DO UPDATE SET id = EXCLUDED.id";

struct Directory {
   by_name: HashMap<String, String>,
   by_id: HashMap<String, String>,
}

fn directory() -> &'static RwLock<Directory> {
   static IDS: OnceLock<RwLock<Directory>> = OnceLock::new();
   IDS.get_or_init(|| {
      RwLock::new(Directory {
         by_name: HashMap::new(),
         by_id: HashMap::new(),
      })
   })
}

fn sender() -> &'static OnceLock<mpsc::UnboundedSender<(String, String)>> {
   static TX: OnceLock<mpsc::UnboundedSender<(String, String)>> = OnceLock::new();
   &TX
}

pub fn get(username: &str) -> Option<String> {
   let key = username.to_lowercase();
   directory().read().ok()?.by_name.get(&key).cloned()
}

pub fn username_for(id: &str) -> Option<String> {
   directory().read().ok()?.by_id.get(id).cloned()
}

pub fn remember(username: &str, id: &str) {
   if username.is_empty() || id.is_empty() {
      return;
   }
   let key = username.to_lowercase();
   let changed = {
      let Ok(mut ids) = directory().write() else {
         return;
      };
      let same = ids.by_name.get(&key).is_some_and(|stored| stored == id);
      if !same {
         ids.by_name.insert(key.clone(), id.to_owned());
         ids.by_id.insert(id.to_owned(), key.clone());
      }
      !same
   };
   if changed && let Some(tx) = sender().get() {
      let _ = tx.send((key, id.to_owned()));
   }
}

/// Read every id already stored, then keep new ones as they show up.
pub async fn load() {
   let url = env::var("TEAPOT_LIMITS_DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
   let (client, connection) = match tokio_postgres::connect(&url, NoTls).await {
      Ok(connected) => connected,
      Err(err) => {
         tracing::warn!("user ids staying in this process only: {err}");
         return;
      },
   };
   tokio::spawn(async move {
      if let Err(err) = connection.await {
         tracing::warn!("postgres user-id connection closed: {err}");
      }
   });
   if let Err(err) = client
      .batch_execute(
         "CREATE TABLE IF NOT EXISTS user_ids (
            username text PRIMARY KEY,
            id text NOT NULL
         )",
      )
      .await
   {
      tracing::warn!("user id table was not created: {err}");
      return;
   }
   match client.query("SELECT username, id FROM user_ids", &[]).await {
      Ok(rows) => {
         let mut ids = match directory().write() {
            Ok(ids) => ids,
            Err(_) => return,
         };
         for row in &rows {
            let username: String = row.get(0);
            let id: String = row.get(1);
            ids.by_id.insert(id.clone(), username.clone());
            ids.by_name.insert(username, id);
         }
         tracing::info!(count = rows.len(), "loaded user ids from postgres");
      },
      Err(err) => {
         tracing::warn!("user ids were not read: {err}");
         return;
      },
   }

   let (tx, mut rx) = mpsc::unbounded_channel();
   let _ = sender().set(tx);
   tokio::spawn(async move {
      let statement = match client.prepare(UPSERT).await {
         Ok(statement) => statement,
         Err(err) => {
            tracing::warn!("user id writer stopped: {err}");
            return;
         },
      };
      while let Some((username, id)) = rx.recv().await {
         let mut batch = vec![(username, id)];
         while let Ok(extra) = rx.try_recv() {
            batch.push(extra);
         }
         let mut latest = HashMap::new();
         for (username, id) in batch {
            latest.insert(username, id);
         }
         for (username, id) in latest {
            if let Err(err) = client.execute(&statement, &[&username, &id]).await {
               tracing::warn!(username, "user id was not stored: {err}");
            }
         }
      }
   });
}

#[cfg(test)]
mod tests {
   use super::{get, remember};

   #[test]
   fn an_id_is_found_again_without_storing_posts() {
      remember("IdMapProbe", "99");
      assert_eq!(get("idmapprobe").as_deref(), Some("99"));
   }
}
