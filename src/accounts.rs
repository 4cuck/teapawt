//! PostgreSQL storage for the account inventory and the per-account cookie
//! jars.

use std::{
   collections::{
      BTreeMap,
      HashMap,
      HashSet,
   },
   env,
   fmt,
   io,
   sync::{
      Arc,
      Mutex as StdMutex,
      Weak,
   },
   time::{
      Duration,
      Instant,
   },
};

use serde::Serialize;
use serde_json::Value;
use tokio::{
   fs,
   sync::Mutex,
   time::{
      sleep,
      timeout,
   },
};
use tokio_postgres::{
   Client,
   Connection,
   NoTls,
   Socket,
   Transaction,
   tls::NoTlsStream,
   types::{
      IsNull,
      ToSql,
      Type,
      to_sql_checked,
   },
};

use crate::{
   error::{
      Error,
      Result,
   },
   types::{
      AccountPool,
      Session,
      SessionKind,
   },
};

const DEFAULT_URL: &str = "host=/var/run/postgresql user=teapot dbname=teapot";
const LOCK_KEY: i64 = 0x7465_6170_6163_6374;
const MIGRATION: &str = "legacy-v1";
const SEARCH: &str = "search";
const SEARCH_PASS: &str = "search-pass";
const AUTH: &str = "auth_token";
const CSRF: &str = "ct0";
const STATS_CACHE_TTL: Duration = Duration::from_secs(5);
/// How long one attempt to reopen the connection may take.
const REOPEN_PATIENCE: Duration = Duration::from_secs(5);
/// The wait before each attempt. It doubles after a failure, up to
/// [`REOPEN_WAIT_MAX`]: recovery from a crashed backend takes a few seconds,
/// and a server down for longer is then asked, with a line logged, that often.
const REOPEN_WAIT: Duration = Duration::from_secs(1);
const REOPEN_WAIT_MAX: Duration = Duration::from_secs(5);

const SCHEMA: &str = "\
   CREATE TABLE IF NOT EXISTS account_sessions (
      id bigint PRIMARY KEY,
      ordinal bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
      body jsonb NOT NULL
   );
   CREATE INDEX IF NOT EXISTS account_sessions_auth_token_idx
      ON account_sessions ((body->>'auth_token')) WHERE body->>'auth_token' <> '';
   CREATE TABLE IF NOT EXISTS account_cookies (
      account_id bigint NOT NULL REFERENCES account_sessions(id) ON DELETE CASCADE,
      name text NOT NULL,
      value text NOT NULL,
      PRIMARY KEY (account_id, name)
   );
   CREATE TABLE IF NOT EXISTS account_migrations (name text PRIMARY KEY);
   CREATE TABLE IF NOT EXISTS account_import_batches (
      id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
      imported_at bigint NOT NULL DEFAULT extract(epoch FROM now())::bigint,
      imported bigint NOT NULL,
      duplicates bigint NOT NULL,
      invalid bigint NOT NULL
   );";
const INSERT_ACCOUNT: &str = "INSERT INTO account_sessions (id, body) VALUES ($1, $2)";
const INSERT_COOKIE: &str =
   "INSERT INTO account_cookies (account_id, name, value) VALUES ($1, $2, $3)";

/// A shared store handle. One connection is serialized across clones so a
/// transaction's statements cannot interleave with another operation.
#[derive(Clone)]
pub struct AccountStore {
   client:      Arc<Mutex<Client>>,
   stats_cache: Arc<StdMutex<CachedStats>>,
}

type CachedStats = Option<(Instant, Stats)>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
   pub total:       i64,
   pub search:      i64,
   pub search_pass: i64,
   pub view:        i64,
   pub ct0_dead:    i64,
   pub rejected:    i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountIdentity {
   pub id:       i64,
   pub username: String,
   pub kind:     SessionKind,
   pub pool:     String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ImportSummary {
   pub imported:   usize,
   pub duplicates: usize,
   pub invalid:    usize,
}

type CookieJar = BTreeMap<String, String>;
type Cookies = HashMap<i64, CookieJar>;

fn store_error(message: &str) -> Error {
   Error::Internal(message.to_owned())
}

fn count_to_db(count: usize) -> Result<i64> {
   i64::try_from(count).map_err(|_| store_error("import batch count out of range"))
}

fn db_error(_: tokio_postgres::Error) -> Error {
   store_error("account database operation failed")
}

// tokio-postgres prints bound parameters with Debug when debug logging is on.
// Never pass a credential, its container, or a cookie value unwrapped.
struct Secret<T>(T);

impl<T> fmt::Debug for Secret<T> {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      f.write_str("[redacted]")
   }
}

impl<T: ToSql> ToSql for Secret<T> {
   fn to_sql(
      &self,
      ty: &Type,
      out: &mut bytes::BytesMut,
   ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
      self.0.to_sql(ty, out)
   }

   fn accepts(ty: &Type) -> bool {
      T::accepts(ty)
   }

   to_sql_checked!();
}

async fn lock(tx: &Transaction<'_>) -> Result<()> {
   tx.query_one("SELECT pg_advisory_xact_lock($1)", &[&LOCK_KEY])
      .await
      .map_err(db_error)?;
   Ok(())
}

async fn migrated(client: &impl tokio_postgres::GenericClient) -> Result<bool> {
   client
      .query_opt("SELECT 1 FROM account_migrations WHERE name = $1", &[
         &MIGRATION,
      ])
      .await
      .map(|row| row.is_some())
      .map_err(db_error)
}

async fn require_migration(client: &impl tokio_postgres::GenericClient) -> Result<()> {
   if !migrated(client).await? {
      return Err(store_error(
         "account database legacy migration is not complete",
      ));
   }
   Ok(())
}

/// Drive `connection`, and open another each time the server closes the one
/// in use. Ends once the store is dropped.
///
/// A restart closes every session, and so does the recovery that follows one
/// crashed backend. The client left behind fails each operation from then on:
/// after one such recovery the fresh ct0s and the cookie jars were retried
/// against it, and none was stored, until this process was restarted as well.
async fn keep_open(
   url: String,
   client: Weak<Mutex<Client>>,
   mut connection: Connection<Socket, NoTlsStream>,
) {
   loop {
      if connection.await.is_err() {
         tracing::warn!("account database connection closed");
      }
      let Some((reopened, next)) = reopen(&url, &client).await else {
         return;
      };
      let Some(store) = client.upgrade() else {
         return;
      };
      *store.lock().await = reopened;
      tracing::info!("account database connection reopened");
      connection = next;
   }
}

/// Ask for a connection until the server gives one. `None` once the store is
/// dropped: that closes its connection too, and leaves nothing to use another.
///
/// Every attempt waits first. A server that has just closed its sessions is
/// still restarting, and one that closes each new session at once would
/// otherwise be reconnected to without pause.
async fn reopen(
   url: &str,
   client: &Weak<Mutex<Client>>,
) -> Option<(Client, Connection<Socket, NoTlsStream>)> {
   let mut wait = REOPEN_WAIT;
   while client.strong_count() > 0 {
      sleep(wait).await;
      if let Ok(Ok(opened)) = timeout(REOPEN_PATIENCE, tokio_postgres::connect(url, NoTls)).await {
         return Some(opened);
      }
      tracing::warn!("account database connection not reopened");
      wait = (wait * 2).min(REOPEN_WAIT_MAX);
   }
   None
}

impl AccountStore {
   pub async fn connect() -> Result<Self> {
      let url = env::var("TEAPOT_LIMITS_DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
      let (client, connection) = tokio_postgres::connect(&url, NoTls).await.map_err(|_| {
         Error::InvalidConfig("could not connect to the account database".to_owned())
      })?;
      let store = Self {
         client:      Arc::new(Mutex::new(client)),
         stats_cache: Arc::default(),
      };
      tokio::spawn(keep_open(url, Arc::downgrade(&store.client), connection));
      store.create_schema().await?;
      Ok(store)
   }

   async fn create_schema(&self) -> Result<()> {
      let mut client = self.client.lock().await;
      let tx = client.transaction().await.map_err(db_error)?;
      lock(&tx).await?;
      tx.batch_execute(SCHEMA).await.map_err(db_error)?;
      tx.commit().await.map_err(db_error)
   }

   fn invalidate_stats_cache(&self) {
      *self
         .stats_cache
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
   }

   pub async fn load(&self) -> Result<Vec<Session>> {
      let client = self.client.lock().await;
      require_migration(&*client).await?;
      let rows = client
         .query("SELECT id, body FROM account_sessions ORDER BY ordinal", &[
         ])
         .await
         .map_err(db_error)?;
      rows
         .into_iter()
         .map(|row| {
            let mut session = serde_json::from_value::<Session>(row.try_get(1).map_err(db_error)?)
               .map_err(|_| store_error("account database contains an unreadable session"))?;
            session.id = row.try_get(0).map_err(db_error)?;
            Ok(session)
         })
         .collect()
   }

   pub async fn load_cookies(&self) -> Result<HashMap<i64, BTreeMap<String, String>>> {
      let client = self.client.lock().await;
      require_migration(&*client).await?;
      let rows = client
         .query("SELECT account_id, name, value FROM account_cookies", &[])
         .await
         .map_err(db_error)?;
      let mut cookies = Cookies::new();
      for row in rows {
         let name: String = row.try_get(1).map_err(db_error)?;
         if !session_cookie(&name) {
            cookies
               .entry(row.try_get(0).map_err(db_error)?)
               .or_default()
               .insert(name, row.try_get(2).map_err(db_error)?);
         }
      }
      Ok(cookies)
   }

   /// Replace the jar for each supplied account. An empty jar clears its
   /// cookies; omitted accounts (including newly imported ones) are untouched.
   pub async fn save_cookies(
      &self,
      cookies: &HashMap<i64, BTreeMap<String, String>>,
   ) -> Result<()> {
      let mut client = self.client.lock().await;
      let tx = client.transaction().await.map_err(db_error)?;
      lock(&tx).await?;
      require_migration(&tx).await?;
      let delete = tx
         .prepare("DELETE FROM account_cookies WHERE account_id = $1")
         .await
         .map_err(db_error)?;
      let insert = tx.prepare(INSERT_COOKIE).await.map_err(db_error)?;
      for (id, jar) in cookies {
         tx.execute(&delete, &[id]).await.map_err(db_error)?;
         for (name, value) in jar.iter().filter(|(name, _)| !session_cookie(name)) {
            tx.execute(&insert, &[id, name, &Secret(value)])
               .await
               .map_err(db_error)?;
         }
      }
      tx.commit().await.map_err(db_error)
   }

   pub async fn demote(&self, id: i64) -> Result<()> {
      let client = self.client.lock().await;
      require_migration(&*client).await?;
      client
         .execute(
            "UPDATE account_sessions
          SET body = body || jsonb_build_object('pool', '', 'demoted_from', lower(body->>'pool'))
          WHERE id = $1 AND lower(body->>'pool') IN ('search', 'search-pass')",
            &[&id],
         )
         .await
         .map_err(db_error)?;
      self.invalidate_stats_cache();
      Ok(())
   }

   /// The active pool this account was imported for (or demoted from).
   ///
   /// Read from the store rather than the live slot, because the slot's limits
   /// are what a probe is trying to correct.
   pub async fn origin_pool(&self, id: i64) -> AccountPool {
      let client = self.client.lock().await;
      let Ok(Some(row)) = client
         .query_opt(
            "SELECT lower(COALESCE(body->>'pool', '')), lower(COALESCE(body->>'demoted_from', ''))
          FROM account_sessions
          WHERE id = $1",
            &[&id],
         )
         .await
      else {
         return AccountPool::View;
      };
      let pool: String = row.try_get(0).unwrap_or_default();
      let demoted_from: String = row.try_get(1).unwrap_or_default();
      if pool == SEARCH_PASS {
         AccountPool::SearchPass
      } else if pool == SEARCH {
         AccountPool::Search
      } else if demoted_from == SEARCH_PASS {
         AccountPool::SearchPass
      } else if demoted_from == SEARCH {
         AccountPool::Search
      } else {
         AccountPool::View
      }
   }

   /// Explicitly assign an account to `pool`, clearing any demotion marker.
   pub async fn set_pool(&self, id: i64, pool: AccountPool) -> Result<()> {
      let client = self.client.lock().await;
      require_migration(&*client).await?;
      let stored = pool.stored();
      client
         .execute(
            "UPDATE account_sessions
          SET body = (body - 'demoted_from') || jsonb_build_object('pool', $2::text)
          WHERE id = $1",
            &[&id, &stored],
         )
         .await
         .map_err(db_error)?;
      self.invalidate_stats_cache();
      Ok(())
   }

   pub async fn save_ct0(&self, updates: &[(i64, String)]) -> Result<()> {
      let mut client = self.client.lock().await;
      let tx = client.transaction().await.map_err(db_error)?;
      lock(&tx).await?;
      require_migration(&tx).await?;
      let update = tx
         .prepare(
            "UPDATE account_sessions SET body = jsonb_set(body, '{ct0}', to_jsonb($2::text)) \
             WHERE id = $1",
         )
         .await
         .map_err(db_error)?;
      for (id, ct0) in updates {
         tx.execute(&update, &[id, &Secret(ct0)])
            .await
            .map_err(db_error)?;
      }
      tx.commit().await.map_err(db_error)
   }

   pub async fn import_text(&self, text: &str, pool: AccountPool) -> Result<ImportSummary> {
      let (accounts, mut summary) = parse_import(text);
      let mut client = self.client.lock().await;
      let tx = client.transaction().await.map_err(db_error)?;
      lock(&tx).await?;
      require_migration(&tx).await?;
      if !accounts.is_empty() {
         let mut max_id = max_existing_id(&tx).await?;
         let exists = tx
            .prepare(
               "SELECT 1 FROM account_sessions WHERE body->>'auth_token' <> '' AND \
                body->>'auth_token' = $1 LIMIT 1",
            )
            .await
            .map_err(db_error)?;
         let insert_account = tx.prepare(INSERT_ACCOUNT).await.map_err(db_error)?;
         let insert_cookie = tx.prepare(INSERT_COOKIE).await.map_err(db_error)?;
         let pool_name = pool.stored();
         for (mut session, jar) in accounts {
            if tx
               .query_opt(&exists, &[&Secret(&session.auth_token)])
               .await
               .map_err(db_error)?
               .is_some()
            {
               summary.duplicates += 1;
               continue;
            }
            max_id = max_id
               .checked_add(1)
               .ok_or_else(|| store_error("account IDs are exhausted"))?;
            session.id = max_id;
            if !pool_name.is_empty() {
               pool_name.clone_into(&mut session.pool);
            }
            let body = session_body(&session)?;
            tx.execute(&insert_account, &[&session.id, &Secret(body)])
               .await
               .map_err(db_error)?;
            for (name, value) in jar {
               tx.execute(&insert_cookie, &[&session.id, &name, &Secret(value)])
                  .await
                  .map_err(db_error)?;
            }
            summary.imported += 1;
         }
      }
      let imported = count_to_db(summary.imported)?;
      let duplicates = count_to_db(summary.duplicates)?;
      let invalid = count_to_db(summary.invalid)?;
      tx.execute(
         "INSERT INTO account_import_batches (imported, duplicates, invalid) VALUES ($1, $2, $3)",
         &[&imported, &duplicates, &invalid],
      )
      .await
      .map_err(db_error)?;
      tx.commit().await.map_err(db_error)?;
      self.invalidate_stats_cache();
      Ok(summary)
   }

   #[allow(
      dead_code,
      reason = "the standalone import CLI does not display recent batches"
   )]
   pub async fn recent_import_batches(&self) -> Result<Vec<(i64, ImportSummary)>> {
      let client = self.client.lock().await;
      client
         .query(
            "SELECT imported_at, imported, duplicates, invalid FROM account_import_batches ORDER \
             BY id DESC LIMIT 10",
            &[],
         )
         .await
         .map_err(db_error)?
         .into_iter()
         .map(|row| {
            let count = |index| -> Result<usize> {
               let value: i64 = row.try_get(index).map_err(db_error)?;
               usize::try_from(value).map_err(|_| store_error("invalid import batch count"))
            };
            Ok((row.try_get(0).map_err(db_error)?, ImportSummary {
               imported:   count(1)?,
               duplicates: count(2)?,
               invalid:    count(3)?,
            }))
         })
         .collect()
   }

   #[allow(dead_code, reason = "explicit offline CLI operation")]
   pub async fn restore_batch_to_search(&self, text: &str) -> Result<usize> {
      let (accounts, summary) = parse_import(text);
      if accounts.is_empty() || summary.invalid != 0 || summary.duplicates != 0 {
         return Err(store_error(
            "restore requires a complete, unique account batch",
         ));
      }
      let mut client = self.client.lock().await;
      let tx = client.transaction().await.map_err(db_error)?;
      lock(&tx).await?;
      require_migration(&tx).await?;
      let mut restored = 0;
      for (account, _) in accounts {
         let matched = tx
            .query(
               "SELECT id FROM account_sessions WHERE body->>'auth_token' = $1",
               &[&Secret(&account.auth_token)],
            )
            .await
            .map_err(db_error)?;
         let [row] = matched.as_slice() else {
            return Err(store_error(
               "restore batch must match exactly one stored row per account",
            ));
         };
         let id: i64 = row.try_get(0).map_err(db_error)?;
         let moved = tx
            .execute(
               "UPDATE account_sessions SET body = (body - 'demoted_from') || \
                '{\"pool\":\"search\"}'::jsonb WHERE id = $1 AND (coalesce(lower(body->>'pool'), \
                '') <> 'search' OR body ? 'demoted_from')",
               &[&id],
            )
            .await
            .map_err(db_error)?;
         if moved != 0 {
            tx.execute(
               "UPDATE session_limits SET body = ((body #- \
                '{refused_ops,SearchTimeline}'::text[]) - 'search_404_at'::text) || \
                '{\"search_banned\":false,\"search_only\":true,\"suspended\":false,\"retired\":\
                false,\"demoted_from_search\":false}'::jsonb WHERE id = $1",
               &[&id],
            )
            .await
            .map_err(db_error)?;
            restored += 1;
         }
      }
      tx.commit().await.map_err(db_error)?;
      self.invalidate_stats_cache();
      Ok(restored)
   }

   /// Account names are public operational metadata, not credential material.
   pub async fn identities(&self, ids: &[i64]) -> Result<HashMap<i64, AccountIdentity>> {
      let client = self.client.lock().await;
      require_migration(&*client).await?;
      let rows = client
         .query(
            "SELECT id, coalesce(body->>'username', ''), body->>'kind', coalesce(body->>'pool', \
             '')
          FROM account_sessions WHERE id = ANY($1)",
            &[&ids],
         )
         .await
         .map_err(db_error)?;
      rows
         .into_iter()
         .map(|row| {
            let id = row.try_get(0).map_err(db_error)?;
            Ok((id, AccountIdentity {
               id,
               username: row.try_get(1).map_err(db_error)?,
               kind: row
                  .try_get::<_, Option<String>>(2)
                  .map_err(db_error)?
                  .and_then(|kind| serde_json::from_value(Value::String(kind)).ok())
                  .unwrap_or(SessionKind::Cookie),
               pool: row.try_get(3).map_err(db_error)?,
            }))
         })
         .collect()
   }

   pub async fn find_identities(&self, query: &str, limit: i64) -> Result<Vec<i64>> {
      let client = self.client.lock().await;
      require_migration(&*client).await?;
      if let Ok(id) = query.parse::<i64>() {
         let row = client
            .query_opt("SELECT id FROM account_sessions WHERE id = $1", &[&id])
            .await
            .map_err(db_error)?;
         return Ok(row.map(|row| vec![row.get(0)]).unwrap_or_default());
      }
      let pattern = format!("%{}%", query.replace(['\\', '%', '_'], ""));
      Ok(client
         .query(
            "SELECT id FROM account_sessions WHERE body->>'username' ILIKE $1
          ORDER BY ordinal DESC LIMIT $2",
            &[&pattern, &limit.clamp(1, 500)],
         )
         .await
         .map_err(db_error)?
         .into_iter()
         .map(|row| row.get(0))
         .collect())
   }

   pub async fn stats(&self) -> Result<Stats> {
      {
         let cached = self
            .stats_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
         if let Some((fetched_at, stats)) = *cached
            && fetched_at.elapsed() < STATS_CACHE_TTL
         {
            return Ok(stats);
         }
      }
      let client = self.client.lock().await;
      require_migration(&*client).await?;
      let has_limits: bool = client
         .query_one("SELECT to_regclass('session_limits') IS NOT NULL", &[])
         .await
         .map_err(db_error)?
         .try_get(0)
         .map_err(db_error)?;
      let stats = if has_limits {
         let row = client
            .query_one(
               "SELECT
                   count(*),
                   count(*) FILTER (
                      WHERE COALESCE((l.body->>'rejected')::boolean, false)
                         OR COALESCE((l.body->>'retired')::boolean, false)
                   ),
                   count(*) FILTER (
                      WHERE NOT COALESCE((l.body->>'rejected')::boolean, false)
                         AND NOT COALESCE((l.body->>'retired')::boolean, false)
                         AND COALESCE((l.body->>'ct0_dead')::boolean, false)
                   ),
                   count(*) FILTER (
                      WHERE NOT COALESCE((l.body->>'rejected')::boolean, false)
                         AND NOT COALESCE((l.body->>'retired')::boolean, false)
                         AND NOT COALESCE((l.body->>'ct0_dead')::boolean, false)
                         AND lower(COALESCE(s.body->>'pool', '')) = 'search'
                   ),
                   count(*) FILTER (
                      WHERE NOT COALESCE((l.body->>'rejected')::boolean, false)
                         AND NOT COALESCE((l.body->>'retired')::boolean, false)
                         AND NOT COALESCE((l.body->>'ct0_dead')::boolean, false)
                         AND lower(COALESCE(s.body->>'pool', '')) = 'search-pass'
                   )
                FROM account_sessions s
                LEFT JOIN session_limits l ON l.id = s.id",
               &[],
            )
            .await
            .map_err(db_error)?;
         let total: i64 = row.try_get(0).map_err(db_error)?;
         let rejected: i64 = row.try_get(1).map_err(db_error)?;
         let ct0_dead: i64 = row.try_get(2).map_err(db_error)?;
         let search: i64 = row.try_get(3).map_err(db_error)?;
         let search_pass: i64 = row.try_get(4).map_err(db_error)?;
         Stats {
            total,
            search,
            search_pass,
            view: total - rejected - ct0_dead - search - search_pass,
            ct0_dead,
            rejected,
         }
      } else {
         let row = client
            .query_one(
               "SELECT
                   count(*),
                   count(*) FILTER (WHERE lower(body->>'pool') = 'search'),
                   count(*) FILTER (WHERE lower(body->>'pool') = 'search-pass')
                FROM account_sessions",
               &[],
            )
            .await
            .map_err(db_error)?;
         let total: i64 = row.try_get(0).map_err(db_error)?;
         let search: i64 = row.try_get(1).map_err(db_error)?;
         let search_pass: i64 = row.try_get(2).map_err(db_error)?;
         Stats {
            total,
            search,
            search_pass,
            view: total - search - search_pass,
            ct0_dead: 0,
            rejected: 0,
         }
      };
      *self
         .stats_cache
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((Instant::now(), stats));
      Ok(stats)
   }

   /// First boot only: import sessions in JSONL order, then attach their jars.
   /// The marker commits with the rows; a subsequent boot never reads the
   /// files.
   pub async fn migrate_legacy(&self, sessions_path: &str, cookies_path: &str) -> Result<()> {
      let mut client = self.client.lock().await;
      let tx = client.transaction().await.map_err(db_error)?;
      lock(&tx).await?;
      if migrated(&tx).await? {
         return tx.commit().await.map_err(db_error);
      }
      let text = fs::read_to_string(sessions_path)
         .await
         .map_err(|_| store_error("could not read the legacy sessions file"))?;
      let (sessions, ids) = audit_legacy(&text)?;
      let cookies = read_legacy_cookies(cookies_path).await?;
      let insert_account = tx.prepare(INSERT_ACCOUNT).await.map_err(db_error)?;
      let insert_cookie = tx.prepare(INSERT_COOKIE).await.map_err(db_error)?;
      for session in sessions {
         let body = session_body(&session)?;
         tx.execute(&insert_account, &[&session.id, &Secret(body)])
            .await
            .map_err(db_error)?;
      }
      let mut orphan_jars = 0_usize;
      for (id, jar) in cookies {
         if !ids.contains(&id) {
            orphan_jars += 1;
            continue;
         }
         for (name, value) in jar.into_iter().filter(|(name, _)| !session_cookie(name)) {
            tx.execute(&insert_cookie, &[&id, &name, &Secret(value)])
               .await
               .map_err(db_error)?;
         }
      }
      tx.execute("INSERT INTO account_migrations (name) VALUES ($1)", &[
         &MIGRATION,
      ])
      .await
      .map_err(db_error)?;
      tx.commit().await.map_err(db_error)?;
      self.invalidate_stats_cache();
      if orphan_jars != 0 {
         tracing::warn!(
            orphan_jars,
            "ignored legacy cookie jars without matching sessions"
         );
      }
      Ok(())
   }
}

async fn max_existing_id(tx: &Transaction<'_>) -> Result<i64> {
   let row = tx
      .query_one("SELECT GREATEST(0, max(id)) FROM account_sessions", &[])
      .await
      .map_err(db_error)?;
   let mut id: i64 = row.try_get(0).map_err(db_error)?;
   let row = tx
      .query_one("SELECT to_regclass('session_limits') IS NOT NULL", &[])
      .await
      .map_err(db_error)?;
   if row.try_get::<_, bool>(0).map_err(db_error)? {
      // Exclude concurrent limits writes while choosing IDs, including orphan
      // limits belonging to accounts no longer present in the sessions file.
      tx.batch_execute("LOCK TABLE session_limits IN SHARE MODE")
         .await
         .map_err(db_error)?;
      let row = tx
         .query_one("SELECT GREATEST(0, max(id)) FROM session_limits", &[])
         .await
         .map_err(db_error)?;
      id = id.max(row.try_get(0).map_err(db_error)?);
   }
   Ok(id)
}

fn session_body(session: &Session) -> Result<Value> {
   serde_json::to_value(session).map_err(|_| store_error("could not encode an account session"))
}

fn session_cookie(name: &str) -> bool {
   name == AUTH || name == CSRF
}

async fn read_legacy_cookies(path: &str) -> Result<Cookies> {
   if path.is_empty() {
      return Ok(Cookies::new());
   }
   match fs::read_to_string(path).await {
      Ok(text) => {
         serde_json::from_str(&text).map_err(|_| store_error("legacy cookie data is invalid"))
      },
      Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Cookies::new()),
      Err(_) => Err(store_error("could not read the legacy cookie file")),
   }
}

fn audit_legacy(text: &str) -> Result<(Vec<Session>, HashSet<i64>)> {
   let mut sessions = Vec::new();
   let mut malformed = 0_usize;
   for line in text.lines().filter(|line| !line.trim().is_empty()) {
      match serde_json::from_str::<Session>(line) {
         Ok(session) => sessions.push(session),
         Err(_) => malformed += 1,
      }
   }
   let mut ids = HashSet::new();
   let mut auth_tokens = HashSet::new();
   let mut oauth_tokens = HashSet::new();
   let mut duplicate_ids = 0_usize;
   let mut duplicate_auth_tokens = 0_usize;
   let mut duplicate_oauth_tokens = 0_usize;
   for session in &sessions {
      duplicate_ids += usize::from(!ids.insert(session.id));
      duplicate_auth_tokens +=
         usize::from(!session.auth_token.is_empty() && !auth_tokens.insert(&session.auth_token));
      duplicate_oauth_tokens +=
         usize::from(!session.oauth_token.is_empty() && !oauth_tokens.insert(&session.oauth_token));
   }
   if malformed != 0
      || duplicate_ids != 0
      || duplicate_auth_tokens != 0
      || duplicate_oauth_tokens != 0
   {
      tracing::warn!(
         malformed,
         duplicate_ids,
         duplicate_auth_tokens,
         duplicate_oauth_tokens,
         "legacy account audit"
      );
   }
   if malformed != 0 {
      return Err(store_error("legacy sessions contain invalid rows"));
   }
   if duplicate_ids != 0 {
      return Err(store_error("legacy sessions contain duplicate IDs"));
   }
   // Reused credentials are reported but existing IDs/pools remain intact.
   Ok((sessions, ids))
}

fn parse_import(text: &str) -> (Vec<(Session, CookieJar)>, ImportSummary) {
   let text = text.trim_start_matches('\u{feff}');
   let lines = text.lines().map(str::trim);
   let mut summary = ImportSummary::default();
   let mut accounts = Vec::new();
   let netscape = lines.clone().any(|line| {
      line.eq_ignore_ascii_case("# Netscape HTTP Cookie File")
         || line.starts_with("#HttpOnly_")
         || (!line.starts_with('#') && line.bytes().filter(|&byte| byte == b'\t').count() == 6)
   });
   let mut record = |account| {
      match account {
         Some(account) => accounts.push(account),
         None => summary.invalid += 1,
      }
   };
   if let Ok(value) = serde_json::from_str::<Value>(text) {
      parse_json_import(&value, &mut record);
   } else if netscape {
      record(parse_netscape(text));
   } else {
      for line in lines.filter(|line| !line.is_empty() && !line.starts_with('#')) {
         if line.starts_with(['{', '[']) {
            match serde_json::from_str::<Value>(line) {
               Ok(value) => parse_json_import(&value, &mut record),
               Err(_) => record(None),
            }
         } else if let Some(header) = cookie_header(line) {
            record(parse_cookie_header(header));
         } else {
            record(parse_export(line).map(|session| (session, CookieJar::new())));
         }
      }
   }
   (accounts, summary)
}

fn cookie_header(line: &str) -> Option<&str> {
   if let Some((name, value)) = line.split_once(':') {
      if name.eq_ignore_ascii_case("cookie") {
         return Some(value);
      }
   }
   line
      .split(';')
      .filter_map(|pair| pair.split_once('='))
      .any(|(name, _)| matches!(name.trim(), AUTH | CSRF))
      .then_some(line)
}

fn parse_cookie_header(header: &str) -> Option<(Session, CookieJar)> {
   let mut jar = CookieJar::new();
   for pair in header
      .split(';')
      .map(str::trim)
      .filter(|pair| !pair.is_empty())
   {
      let (name, value) = pair.split_once('=')?;
      insert_import_cookie(&mut jar, name.trim(), value.trim())?;
   }
   finish_import_jar(jar)
}

fn parse_json_import(value: &Value, record: &mut impl FnMut(Option<(Session, CookieJar)>)) {
   match value {
      Value::Array(items) if !items.iter().any(|item| item.get("name").is_some()) => {
         for item in items {
            parse_json_import(item, record);
         }
      },
      _ => record(parse_json_jar(value)),
   }
}

fn parse_json_jar(value: &Value) -> Option<(Session, CookieJar)> {
   let mut jar = CookieJar::new();
   let data = value.get("cookies").unwrap_or(value);
   match data {
      Value::Array(entries) => {
         for entry in entries {
            let name = entry.get("name")?.as_str()?;
            let value = entry.get("value")?.as_str()?;
            if let Some(domain) = entry.get("domain") {
               if !import_domain(domain.as_str()?) {
                  continue;
               }
            }
            if let Some(path) = entry.get("path") {
               if path.as_str()? != "/" {
                  continue;
               }
            }
            insert_import_cookie(&mut jar, name, value)?;
         }
      },
      Value::Object(cookies) if value.get("cookies").is_some() => {
         for (name, value) in cookies {
            insert_import_cookie(&mut jar, name, value.as_str()?)?;
         }
      },
      Value::Object(cookies) => {
         // Bare objects can be session exports with metadata, not just cookie maps.
         for name in [AUTH, CSRF, "twid"] {
            if let Some(value) = cookies.get(name) {
               insert_import_cookie(&mut jar, name, value.as_str()?)?;
            }
         }
      },
      _ => return None,
   }
   let (mut session, jar) = finish_import_jar(jar)?;
   if let Some(username) = value.get("username").and_then(Value::as_str) {
      if !username.is_empty() && !username.chars().any(char::is_control) {
         username.clone_into(&mut session.username);
      }
   }
   Some((session, jar))
}

fn import_domain(domain: &str) -> bool {
   let domain = domain.strip_prefix('.').unwrap_or(domain);
   domain.eq_ignore_ascii_case("x.com") || domain.eq_ignore_ascii_case("twitter.com")
}

fn insert_import_cookie(jar: &mut CookieJar, name: &str, value: &str) -> Option<()> {
   if !valid_cookie(name, value)
      || (matches!(name, AUTH | CSRF | "twid") && jar.get(name).is_some_and(|saved| saved != value))
   {
      return None;
   }
   jar.insert(name.to_owned(), value.to_owned());
   Some(())
}

fn is_hex(field: &str, length: usize) -> bool {
   field.len() == length && field.bytes().all(|b| b.is_ascii_hexdigit())
}

fn parse_export(line: &str) -> Option<Session> {
   for separator in ["----", "|", ":"] {
      if line.contains(separator)
         && let Some(session) = parse_export_with(line, separator)
      {
         return Some(session);
      }
   }
   None
}

fn parse_export_with(line: &str, separator: &str) -> Option<Session> {
   let fields: Vec<_> = line.split(separator).map(str::trim).collect();
   let username = *fields.first()?;
   let credentials = fields.get(1..)?;
   let auth_token = credentials
      .iter()
      .copied()
      .rev()
      .find(|field| is_hex(field, 40))?;
   let ct0 = credentials
      .iter()
      .copied()
      .find(|field| is_hex(field, 160))
      .unwrap_or("");
   if username.is_empty() || username.chars().any(char::is_control) {
      return None;
   }
   Some(cookie_session(
      username.to_owned(),
      auth_token.to_owned(),
      ct0.to_owned(),
   ))
}

fn cookie_session(username: String, auth_token: String, ct0: String) -> Session {
   Session {
      username,
      auth_token,
      ct0,
      kind: SessionKind::Cookie,
      ..Session::default()
   }
}

fn parse_netscape(text: &str) -> Option<(Session, CookieJar)> {
   let mut jar = CookieJar::new();
   for raw in text.lines() {
      let raw = raw.trim_start_matches(' ');
      let line = if let Some(line) = raw.strip_prefix("#HttpOnly_") {
         line
      } else if raw.starts_with('#') || raw.trim().is_empty() {
         continue;
      } else {
         raw
      };
      let [domain, subdomains, path, secure, expiration, name, value] =
         line.split('\t').collect::<Vec<_>>().try_into().ok()?;
      if !import_domain(domain) {
         continue;
      }
      if !matches!(subdomains, "TRUE" | "FALSE")
         || !matches!(secure, "TRUE" | "FALSE")
         || expiration.parse::<u64>().is_err()
         || !path.starts_with('/')
      {
         return None;
      }
      // The runtime jar is shared across X endpoints, not scoped by path.
      if path != "/" {
         continue;
      }
      insert_import_cookie(&mut jar, name, value)?;
   }
   finish_import_jar(jar)
}

fn finish_import_jar(mut jar: CookieJar) -> Option<(Session, CookieJar)> {
   let auth_token = jar.remove(AUTH).filter(|value| !value.is_empty())?;
   // `ct0` is optional. The settings write that turns search safety off sends
   // `auth_token` alone; X answers 353 and issues a csrf cookie on that
   // response, and the write continues with it.
   let ct0 = jar
      .remove(CSRF)
      .filter(|value| !value.is_empty())
      .unwrap_or_default();
   let username = jar
      .get("twid")
      .and_then(|twid| {
         let twid = twid.trim_matches('"');
         let id = twid
            .strip_prefix("u%3D")
            .or_else(|| twid.strip_prefix("u%3d"))
            .or_else(|| twid.strip_prefix("u="))?;
         (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())).then(|| format!("x-{id}"))
      })
      .unwrap_or_default();
   Some((cookie_session(username, auth_token, ct0), jar))
}

fn valid_cookie(name: &str, value: &str) -> bool {
   !name.is_empty()
      && name
         .bytes()
         .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
      && value.bytes().all(|b| b.is_ascii_graphic() && b != b';')
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn parses_export_lines_and_keeps_auth_token_without_ct0() {
      let auth = "a".repeat(40);
      let ct0 = "b".repeat(160);
      let text = format!(
         "# comment\n \
          first:pass:mail:extra:{ct0}:{auth}\nsecond----pass----mail----{auth}----{ct0}\nthird|p:\
          a*s|mail@example.com|{auth}|{ct0}|3PUPQYSOGDNPMODH\nfourth:p|ass:mail@example.com:\
          {auth}:{ct0}\nmissing:pass:{auth}\n"
      );
      let (accounts, summary) = parse_import(&text);
      assert_eq!(summary.invalid, 0);
      assert_eq!(accounts.len(), 5);
      for ((session, jar), name) in accounts
         .iter()
         .take(4)
         .zip(["first", "second", "third", "fourth"])
      {
         assert_eq!(session.username, name);
         assert_eq!(session.auth_token, auth);
         assert_eq!(session.ct0, ct0);
         assert_eq!(session.kind, SessionKind::Cookie);
         assert!(jar.is_empty());
      }
      assert_eq!(accounts[4].0.username, "missing");
      assert_eq!(accounts[4].0.auth_token, auth);
      assert!(accounts[4].0.ct0.is_empty());
   }

   #[test]
   fn parses_headers_and_json_without_pairing_incomplete_accounts() {
      let text = concat!(
         "Cookie: auth_token=fake-a; ct0=fake-b; lang=en\n",
         "auth_token=missing-csrf\n",
         "ct0=missing-auth\n",
         "{\"username\":\"demo\",\"auth_token\":\"fake-c\",\"ct0\":\"fake-d\"}\n",
         "[{\"name\":\"auth_token\",\"value\":\"fake-e\",\"domain\":\".x.com\"},",
         "{\"name\":\"ct0\",\"value\":\"fake-f\",\"path\":\"/\"}]\n",
         "{\"cookies\":{\"auth_token\":\"fake-g\",\"ct0\":\"\"}}\n",
      );
      let (accounts, summary) = parse_import(text);
      assert_eq!(summary.invalid, 1);
      assert_eq!(accounts.len(), 5);
      assert_eq!(accounts[0].1["lang"], "en");
      assert!(accounts[1].0.ct0.is_empty());
      assert_eq!(accounts[1].0.auth_token, "missing-csrf");
      assert_eq!(accounts[2].0.username, "demo");
      assert_eq!(accounts[3].0.ct0, "fake-f");
      assert_eq!(accounts[4].0.auth_token, "fake-g");
      assert!(accounts[4].0.ct0.is_empty());
   }

   #[test]
   fn parses_one_netscape_jar_with_http_only_and_twid() {
      let text = concat!(
         "\u{feff}# Netscape HTTP Cookie File\r\n",
         "#HttpOnly_.x.com\tTRUE\t/\tTRUE\t0\tauth_token\ttest-auth\r\n",
         ".x.com\tTRUE\t/\tTRUE\t0\tct0\ttest-ct0\r\n",
         ".x.com\tTRUE\t/\tTRUE\t0\ttwid\tu%3D123456\r\n",
         ".x.com\tTRUE\t/\tTRUE\t0\tlang\ten\r\n",
         ".example.org\tTRUE\t/\tTRUE\t0\tauth_token\twrong\r\n",
      );
      let (accounts, summary) = parse_import(text);
      assert_eq!(summary.invalid, 0);
      let [(session, jar)] = accounts.as_slice() else {
         panic!("expected one account")
      };
      assert_eq!(session.username, "x-123456");
      assert_eq!(session.auth_token, "test-auth");
      assert_eq!(session.ct0, "test-ct0");
      assert_eq!(jar.len(), 2);
      assert_eq!(jar["lang"], "en");
   }

   #[test]
   fn a_netscape_jar_needs_auth_token_and_rejects_conflicts() {
      let auth = ".twitter.com\tTRUE\t/\tTRUE\t0\tauth_token\ta";
      let (accounts, summary) = parse_import(auth);
      assert_eq!(summary.invalid, 0);
      assert_eq!(accounts.len(), 1);
      assert_eq!(accounts[0].0.auth_token, "a");
      assert!(accounts[0].0.ct0.is_empty());

      let csrf = ".twitter.com\tTRUE\t/\tTRUE\t0\tct0\tc";
      let conflict = ".x.com\tTRUE\t/\tTRUE\t0\tauth_token\tb";
      let (accounts, summary) = parse_import(&format!("{auth}\n{csrf}\n{conflict}"));
      assert!(accounts.is_empty());
      assert_eq!(summary.invalid, 1);

      let (accounts, summary) = parse_import(csrf);
      assert!(accounts.is_empty());
      assert_eq!(summary.invalid, 1);
   }

   #[test]
   fn legacy_audit_keeps_order_and_pools_but_rejects_id_collisions() {
      let source = "{\"id\":42,\"pool\":\"Search\",\"auth_token\":\"fake\"}\n{\"id\":5,\"\
                    auth_token\":\"fake\"}";
      let (sessions, ids) = audit_legacy(source).unwrap();
      assert_eq!(
         sessions
            .iter()
            .map(|session| session.id)
            .collect::<Vec<_>>(),
         [42, 5]
      );
      assert_eq!(sessions[0].pool, "Search");
      assert_eq!(sessions[1].pool, "");
      assert_eq!(ids.len(), 2);
      assert!(audit_legacy(&format!("{source}\n{{\"id\":42}}")).is_err());
      assert!(audit_legacy(&format!("{source}\ninvalid")).is_err());
   }

   #[test]
   fn demoted_session_body_keeps_empty_pool_and_records_search_origin() {
      let mut saved: Session =
         serde_json::from_str(r#"{"id":45,"pool":"","demoted_from":"search","auth_token":"a"}"#)
            .unwrap();
      assert!(saved.pool.is_empty());
      assert_eq!(saved.demoted_from, "search");
      let (_, limits) = saved.clone().into_credentials_and_limits();
      assert!(!limits.search_only);
      assert!(limits.demoted_from_search);

      SEARCH.clone_into(&mut saved.pool);
      saved.demoted_from.clear();
      let (_, restored_limits) = saved.into_credentials_and_limits();
      assert!(restored_limits.search_only);
      assert!(!restored_limits.demoted_from_search);
   }

   /// The server process answering the store, if one is.
   async fn backend(store: &AccountStore) -> Option<i32> {
      let row = store
         .client
         .lock()
         .await
         .query_one("SELECT pg_backend_pid()", &[])
         .await
         .ok()?;
      Some(row.get(0))
   }

   #[tokio::test]
   #[ignore = "requires an isolated PostgreSQL cluster on a /tmp Unix socket"]
   async fn a_connection_the_server_closed_is_reopened() {
      let url = env::var("TEAPOT_LIMITS_DATABASE_URL").unwrap();
      assert!(url.starts_with("host=/tmp/teapot-runtime-test-"));
      let store = AccountStore::connect().await.unwrap();
      let closed = backend(&store).await.unwrap();
      // What a restart does to every session, and recovery from a crashed
      // backend with it.
      let _ = store
         .client
         .lock()
         .await
         .batch_execute("SELECT pg_terminate_backend(pg_backend_pid())")
         .await;
      timeout(Duration::from_secs(10), async {
         while backend(&store).await.is_none_or(|pid| pid == closed) {
            sleep(Duration::from_millis(10)).await;
         }
      })
      .await
      .unwrap();
      store.recent_import_batches().await.unwrap();
   }
}
