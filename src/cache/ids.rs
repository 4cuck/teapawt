//! Username and numeric id, in Postgres.
//!
//! `UserTweets` needs that id. Without it, the profile waits for
//! `UserByScreenName` to finish and only then asks for the posts. The id does
//! not change when the account posts, so the row is not a copy of the page.
//! The maps are the working copy of the table. There is no size cap: Postgres
//! is what remembers them across a restart.
//!
//! The table runs to tens of millions of rows. It is read after the server is
//! already answering, and a name asked for before its row arrives just takes
//! the slower path. Each map is split into shards behind their own locks: one
//! map that size stops every reader for seconds whenever it doubles.

use std::{
   collections::HashMap,
   env,
   future::poll_fn,
   hash::{BuildHasher as _, Hasher as _, RandomState},
   iter,
   pin::pin,
   sync::{OnceLock, RwLock},
   time::Instant,
};

use futures_core::Stream as _;
use tokio::sync::mpsc;
use tokio_postgres::NoTls;

const DEFAULT_URL: &str = "host=/var/run/postgresql user=teapot dbname=teapot";

const UPSERT: &str = "\
INSERT INTO user_ids (username, id) VALUES ($1, $2) \
ON CONFLICT (username) DO UPDATE SET id = EXCLUDED.id";

/// A shard resizes alone, holding up only the names that hash to it.
const SHARDS: usize = 64;
const SHARDS_U64: u64 = SHARDS as u64;

type Names = HashMap<Box<str>, u64>;
type Ids = HashMap<u64, Box<str>>;

struct Directory {
   by_name: Vec<RwLock<Names>>,
   by_id: Vec<RwLock<Ids>>,
}

fn directory() -> &'static Directory {
   static IDS: OnceLock<Directory> = OnceLock::new();
   IDS.get_or_init(|| Directory {
      by_name: iter::repeat_with(RwLock::default).take(SHARDS).collect(),
      by_id: iter::repeat_with(RwLock::default).take(SHARDS).collect(),
   })
}

fn sender() -> &'static OnceLock<mpsc::UnboundedSender<(String, String)>> {
   static TX: OnceLock<mpsc::UnboundedSender<(String, String)>> = OnceLock::new();
   &TX
}

fn name_shard(name: &str) -> usize {
   static SEED: OnceLock<RandomState> = OnceLock::new();
   let mut hasher = SEED.get_or_init(RandomState::new).build_hasher();
   hasher.write(name.as_bytes());
   usize::try_from(hasher.finish() % SHARDS_U64).unwrap_or_default()
}

/// Snowflake ids end in a per-millisecond sequence that is mostly zero, so
/// the low bits alone would pile ids into a few shards.
fn id_shard(id: u64) -> usize {
   usize::try_from((id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) % SHARDS_U64).unwrap_or_default()
}

pub fn get(username: &str) -> Option<String> {
   let key = username.to_lowercase();
   let names = directory().by_name[name_shard(&key)].read().ok()?;
   names.get(key.as_str()).map(u64::to_string)
}

pub fn username_for(id: &str) -> Option<String> {
   let id = id.parse::<u64>().ok()?;
   let ids = directory().by_id[id_shard(id)].read().ok()?;
   ids.get(&id).map(ToString::to_string)
}

pub fn remember(username: &str, id: &str) {
   let Ok(numeric) = id.parse::<u64>() else {
      return;
   };
   if username.is_empty() {
      return;
   }
   let key = username.to_lowercase();
   let shard = &directory().by_name[name_shard(&key)];
   if shard
      .read()
      .ok()
      .is_some_and(|names| names.get(key.as_str()) == Some(&numeric))
   {
      return;
   }
   let previous = {
      let Ok(mut names) = shard.write() else {
         return;
      };
      match names.insert(key.as_str().into(), numeric) {
         Some(stored) if stored == numeric => return,
         previous => previous,
      }
   };
   // The name moved to another account; its old id no longer leads here.
   if let Some(old) = previous
      && let Ok(mut ids) = directory().by_id[id_shard(old)].write()
      && ids.get(&old).is_some_and(|name| **name == *key)
   {
      ids.remove(&old);
   }
   if let Ok(mut ids) = directory().by_id[id_shard(numeric)].write() {
      ids.insert(numeric, key.as_str().into());
   }
   if let Some(tx) = sender().get() {
      let _ = tx.send((key, id.to_owned()));
   }
}

/// Read every id already stored, then keep new ones as they show up. Runs
/// beside the server; names remembered meanwhile are kept over the rows.
pub async fn load() {
   let Some(client) = connect().await else {
      return;
   };

   // Queued from here on, so a name learned while the table is read is
   // written once the writer starts.
   let (tx, rx) = mpsc::unbounded_channel();
   let _ = sender().set(tx);

   let started = Instant::now();
   let (names, ids, count) = read_table(&client).await;
   adopt(names, ids);
   tracing::info!(
      count,
      secs = started.elapsed().as_secs_f32(),
      "loaded user ids from postgres"
   );
   tokio::spawn(write_new_ids(client, rx));
}

/// The table's connection, with the table created if it was not there.
async fn connect() -> Option<tokio_postgres::Client> {
   let url = env::var("TEAPOT_LIMITS_DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
   let (client, connection) = match tokio_postgres::connect(&url, NoTls).await {
      Ok(connected) => connected,
      Err(err) => {
         tracing::warn!("user ids staying in this process only: {err}");
         return None;
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
      return None;
   }
   Some(client)
}

/// Every stored row, sharded the way the directory is.
async fn read_table(client: &tokio_postgres::Client) -> (Vec<Names>, Vec<Ids>, usize) {
   // Sized up front so the shards do not double over and over while filling.
   let estimate = row_estimate(client).await;
   let per_shard = estimate / SHARDS + estimate / SHARDS / 8;
   let mut names = iter::repeat_with(|| Names::with_capacity(per_shard))
      .take(SHARDS)
      .collect::<Vec<_>>();
   let mut ids = iter::repeat_with(|| Ids::with_capacity(per_shard))
      .take(SHARDS)
      .collect::<Vec<_>>();
   let mut count = 0_usize;
   let rows = match client
      .query_raw("SELECT username, id FROM user_ids", iter::empty::<&str>())
      .await
   {
      Ok(rows) => rows,
      Err(err) => {
         tracing::warn!("user ids were not read: {err}");
         return (names, ids, count);
      },
   };
   let mut rows = pin!(rows);
   while let Some(row) = poll_fn(|cx| rows.as_mut().poll_next(cx)).await {
      let row = match row {
         Ok(row) => row,
         Err(err) => {
            tracing::warn!(count, "user ids were not all read: {err}");
            break;
         },
      };
      let (Ok(username), Ok(id)) = (row.try_get::<_, &str>(0), row.try_get::<_, &str>(1)) else {
         continue;
      };
      let Ok(id) = id.parse::<u64>() else {
         continue;
      };
      names[name_shard(username)].insert(username.into(), id);
      ids[id_shard(id)].insert(id, username.into());
      count += 1;
   }
   (names, ids, count)
}

/// Postgres's own count from its last analyze, which costs no table scan.
async fn row_estimate(client: &tokio_postgres::Client) -> usize {
   client
      .query_one(
         "SELECT reltuples::bigint FROM pg_class WHERE oid = 'user_ids'::regclass",
         &[],
      )
      .await
      .ok()
      .and_then(|row| usize::try_from(row.get::<_, i64>(0)).ok())
      .unwrap_or(0)
}

async fn write_new_ids(
   client: tokio_postgres::Client,
   mut rx: mpsc::UnboundedReceiver<(String, String)>,
) {
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
}

/// Swap the maps read from the table in, one shard at a time. Anything the
/// live shard learned while the table was read is newer and is kept.
fn adopt(names: Vec<Names>, ids: Vec<Ids>) {
   let directory = directory();
   for (live, mut loaded) in directory.by_name.iter().zip(names) {
      let Ok(mut live) = live.write() else {
         continue;
      };
      loaded.extend(live.drain());
      *live = loaded;
   }
   for (live, mut loaded) in directory.by_id.iter().zip(ids) {
      let Ok(mut live) = live.write() else {
         continue;
      };
      loaded.extend(live.drain());
      *live = loaded;
   }
}

#[cfg(test)]
mod tests {
   use std::iter;

   use super::{Ids, Names, SHARDS, adopt, get, id_shard, name_shard, remember, username_for};

   #[test]
   fn an_id_is_found_again_without_storing_posts() {
      remember("IdMapProbe", "99");
      assert_eq!(get("idmapprobe").as_deref(), Some("99"));
      assert_eq!(username_for("99").as_deref(), Some("idmapprobe"));
      remember("NotAnId", "abc");
      assert_eq!(get("notanid"), None);
   }

   #[test]
   fn a_handle_taken_by_another_account_leaves_its_old_id() {
      remember("PassedOn", "7001");
      remember("passedon", "7002");
      assert_eq!(get("PassedOn").as_deref(), Some("7002"));
      assert_eq!(username_for("7002").as_deref(), Some("passedon"));
      assert_eq!(username_for("7001"), None);
   }

   #[test]
   fn rows_read_from_the_table_do_not_undo_a_name_learned_meanwhile() {
      remember("MovedHandle", "5001");
      let mut names = iter::repeat_with(Names::new).take(SHARDS).collect::<Vec<_>>();
      let mut ids = iter::repeat_with(Ids::new).take(SHARDS).collect::<Vec<_>>();
      for (name, id) in [("movedhandle", 4000_u64), ("tableonly", 4001)] {
         names[name_shard(name)].insert(name.into(), id);
         ids[id_shard(id)].insert(id, name.into());
      }
      adopt(names, ids);
      assert_eq!(get("MovedHandle").as_deref(), Some("5001"));
      assert_eq!(get("tableonly").as_deref(), Some("4001"));
      assert_eq!(username_for("4001").as_deref(), Some("tableonly"));
   }

   #[test]
   fn snowflake_ids_spread_over_every_shard() {
      // Ids minted a millisecond apart, each with a zero sequence.
      let base = 441_000_000_000_u64 << 22;
      let mut used = [false; SHARDS];
      for step in 0..4096_u64 {
         used[id_shard(base + (step << 22))] = true;
      }
      assert!(used.iter().all(|&shard| shard), "{used:?}");
   }
}
