use std::{
   collections::{BTreeMap, HashMap},
   env,
   fmt::Write as _,
   ops::Deref,
   path::Path,
   result::Result as StdResult,
   sync::{
      Arc,
      atomic::{AtomicBool, AtomicUsize, Ordering},
   },
   time::{Duration, SystemTime, UNIX_EPOCH},
};

use data_encoding::BASE64;
use ring::hmac;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use tokio::{
   fs,
   sync::{Mutex, Notify, OwnedSemaphorePermit, RwLock, Semaphore},
   time::sleep,
};

use crate::{
   error::{Error, Result},
   types::{Headroom, RateLimit, Session, SessionCredentials, SessionKind, SessionLimits, Spend},
};

use super::limits_db;

#[derive(Serialize)]
pub struct HealthResponse {
   pub sessions: SessionStats,
   pub requests: RequestStats,
   pub timestamp: String,
}

#[derive(Serialize)]
pub struct SessionStats {
   pub total: usize,
   pub limited: usize,
   pub rejected: usize,
   pub available: usize,
}

#[derive(Serialize)]
pub struct RequestStats {
   pub total: i32,
   pub by_api: HashMap<String, i32>,
}

#[derive(Serialize)]
pub struct DebugResponse {
   pub sessions: Vec<SessionDetail>,
   pub count: usize,
   pub timestamp: String,
}

#[derive(Serialize)]
pub struct SessionDetail {
   pub id: i64,
   pub username: String,
   pub kind: SessionKind,
   pub limited: bool,
   pub rejected: bool,
   pub limited_at: i64,
   pub pending: i32,
   pub apis: HashMap<String, RateLimit>,
}

/// Pool of authentication sessions for Twitter API.
#[derive(Clone)]
pub struct SessionPool {
   sessions: Vec<Arc<SessionSlot>>,
   limits: Arc<RwLock<HashMap<i64, SessionLimits>>>,
   cursor: Arc<AtomicUsize>,
   state: Option<Arc<LimitState>>,
   /// `sessions.jsonl` this pool was loaded from.
   source_path: String,
   /// Signaled after a lease returns its permit, so a caller blocked on a full
   /// pool can take whichever account freed rather than one chosen up front.
   freed: Arc<Notify>,
}

/// Writes one account at a time. The JSON path remains only for a deployment
/// that has no Postgres, and as the file imported on the first boot.
struct LimitState {
   path: String,
   dirty: Notify,
   writing: Mutex<()>,
   db: Option<limits_db::LimitsDb>,
}

const STATE_FLUSH_INTERVAL: Duration = Duration::from_secs(15);

/// Why one pass over the pool found no account.
enum Miss {
   /// None can serve this call: rejected, refused, or not acceptable.
   Nobody,
   /// Every candidate is out of quota.
   Spent,
   /// A candidate has quota but is at its concurrency cap.
   Busy,
}

/// A candidate with known budget. Higher remaining is chosen. At a tie, an
/// account that has returned results for this call wins.
#[derive(Clone, Copy)]
struct Candidate {
   index: usize,
   remaining: i32,
   verified: bool,
}

struct SessionSlot {
   credentials: Arc<SessionCredentials>,
   permits: Arc<Semaphore>,
   max_concurrency: usize,
}

/// A session and the concurrency permit held for its request lifetime.
pub struct SessionLease {
   credentials: Arc<SessionCredentials>,
   permit: Option<OwnedSemaphorePermit>,
   /// This lease decremented the endpoint's local remaining count.
   metered: AtomicBool,
   freed: Arc<Notify>,
}

impl Drop for SessionLease {
   fn drop(&mut self) {
      // Return the slot before waking anyone. A scan that runs while the
      // permit is still held goes back to sleep and misses this signal.
      drop(self.permit.take());
      self.freed.notify_one();
   }
}

impl Deref for SessionLease {
   type Target = SessionCredentials;

   fn deref(&self) -> &Self::Target {
      &self.credentials
   }
}

impl SessionPool {
   /// Load sessions from a JSONL file.
   #[expect(
      clippy::cognitive_complexity,
      reason = "session loading has inherent branching"
   )]
   pub async fn load(path: &str, max_concurrent_requests: u32) -> Result<Self> {
      let parsed = if Path::new(path).exists() {
         let content = fs::read_to_string(path).await?;
         let mut parsed = Vec::new();

         for line in content.lines() {
            if line.trim().is_empty() {
               continue;
            }
            match serde_json::from_str::<Session>(line) {
               Ok(session) => parsed.push(session),
               Err(err) => {
                  tracing::warn!("Failed to parse session: {err}");
               },
            }
         }

         tracing::info!("Loaded {} sessions", parsed.len());
         parsed
      } else {
         tracing::warn!("Sessions file not found: {path}");
         Vec::new()
      };

      let max_concurrency = usize::try_from(max_concurrent_requests.max(1)).unwrap_or(usize::MAX);
      let mut sessions = Vec::with_capacity(parsed.len());
      let mut limits = HashMap::with_capacity(parsed.len());

      for session in parsed {
         let (creds, lims) = session.into_credentials_and_limits();
         let id = creds.id;
         sessions.push(Arc::new(SessionSlot {
            credentials: Arc::new(creds),
            permits: Arc::new(Semaphore::new(max_concurrency)),
            max_concurrency,
         }));
         limits.insert(id, lims);
      }

      let state_path =
         env::var("TEAPOT_SESSION_STATE_FILE").unwrap_or_else(|_| "session-limits.json".to_owned());
      let state = if state_path.is_empty() {
         None
      } else {
         match limits_db::Connected::connect().await {
            Ok(mut db) => match db.load().await {
               Ok(saved) if !saved.is_empty() => {
                  let restored = Self::overlay(&mut limits, saved);
                  tracing::info!("Restored rate-limit state for {restored} sessions from postgres");
                  Some((state_path, Some(db)))
               },
               Ok(_) => {
                  Self::restore(&state_path, &mut limits).await;
                  if let Err(err) = db.import(&limits).await {
                     tracing::warn!("could not import session limits into postgres: {err}");
                  } else {
                     tracing::info!(
                        sessions = limits.len(),
                        "imported session limits into postgres"
                     );
                  }
                  Some((state_path, Some(db)))
               },
               Err(err) => {
                  tracing::warn!("postgres limits unreadable, using the JSON file: {err}");
                  Self::restore(&state_path, &mut limits).await;
                  Some((state_path, None))
               },
            },
            Err(err) => {
               tracing::warn!("postgres limits store unavailable, keeping the JSON file: {err}");
               Self::restore(&state_path, &mut limits).await;
               Some((state_path, None))
            },
         }
      };

      let limits = Arc::new(RwLock::new(limits));
      let state = state.map(|(path, ready)| {
         let db = ready.map(|ready| ready.spawn(Arc::clone(&limits)));
         Arc::new(LimitState {
            path,
            dirty: Notify::new(),
            writing: Mutex::new(()),
            db,
         })
      });

      let pool = Self {
         sessions,
         limits,
         cursor: Arc::new(AtomicUsize::new(0)),
         state,
         source_path: path.to_owned(),
         freed: Arc::new(Notify::new()),
      };
      pool.spawn_flusher();

      Ok(pool)
   }

   async fn restore(path: &str, limits: &mut HashMap<i64, SessionLimits>) {
      let Ok(content) = fs::read_to_string(path).await else {
         return;
      };
      match serde_json::from_str::<BTreeMap<i64, SessionLimits>>(&content) {
         Ok(saved) => {
            let mut restored = 0_usize;
            for (id, saved_limits) in saved {
               if let Some(slot) = limits.get_mut(&id) {
                  *slot = saved_limits;
                  restored += 1;
               }
            }
            tracing::info!("Restored rate-limit state for {restored} sessions");
         },
         Err(err) => tracing::warn!("Ignoring unreadable session state: {err}"),
      }
   }

   fn overlay(
      limits: &mut HashMap<i64, SessionLimits>,
      saved: HashMap<i64, SessionLimits>,
   ) -> usize {
      let mut restored = 0_usize;
      for (id, saved_limits) in saved {
         if let Some(slot) = limits.get_mut(&id) {
            *slot = saved_limits;
            restored += 1;
         }
      }
      restored
   }

   fn spawn_flusher(&self) {
      let Some(state) = self.state.clone() else {
         return;
      };
      // Postgres writes the one account that changed. The JSON rewrite is
      // only the fallback when that database is not there.
      if state.db.is_some() {
         return;
      }
      let limits = Arc::clone(&self.limits);
      tokio::spawn(async move {
         loop {
            state.dirty.notified().await;
            // Coalesce, since update_session_limit fires on every response.
            sleep(STATE_FLUSH_INTERVAL).await;

            if let Err(err) = Self::store(&state, &limits).await {
               tracing::warn!("Failed to store session state: {err}");
               state.dirty.notify_one();
            }
         }
      });
   }

   async fn store(state: &LimitState, limits: &RwLock<HashMap<i64, SessionLimits>>) -> Result<()> {
      let _writing = state.writing.lock().await;
      // Copy under the lock, then encode after releasing it. Encoding the
      // whole pool is the slow part, and every call to X needs this same lock.
      let snapshot = limits.read().await.clone();
      let body = serde_json::to_string(&snapshot)?;
      let temporary = format!("{}.tmp", state.path);
      fs::write(&temporary, body).await?;
      fs::rename(&temporary, &state.path).await?;
      Ok(())
   }

   fn mark_dirty(&self, session_id: i64) {
      let Some(state) = self.state.as_ref() else {
         return;
      };
      if let Some(db) = &state.db {
         db.save(session_id);
      } else {
         state.dirty.notify_one();
      }
   }

   /// Write the one account before returning, for the transitions worth a restart.
   async fn persist_now(&self, session_id: i64) {
      let Some(state) = self.state.as_ref() else {
         return;
      };
      if let Some(db) = &state.db {
         db.save_now(session_id).await;
         return;
      }
      if let Err(err) = Self::store(state, &self.limits).await {
         tracing::warn!("Failed to store session state: {err}");
         state.dirty.notify_one();
      }
   }

   /// Acquire an available session for an API request.
   pub(crate) async fn acquire(
      &self,
      api: &str,
      required_kind: Option<SessionKind>,
   ) -> Result<SessionLease> {
      self.acquire_excluding(api, required_kind, None).await
   }

   /// Acquire a session while avoiding a token that was just rejected.
   pub(crate) async fn acquire_excluding(
      &self,
      api: &str,
      required_kind: Option<SessionKind>,
      excluded_id: Option<i64>,
   ) -> Result<SessionLease> {
      self.acquire_with(|_| api, required_kind, excluded_id).await
   }

   /// Acquire when the endpoint depends on which kind of session is chosen.
   ///
   /// The OAuth and cookie flows call different endpoints for the same feature,
   /// and each keeps its own budget, so picking a session against one key and
   /// then spending the other hides an exhausted bucket from selection.
   pub(crate) async fn acquire_by_kind(
      &self,
      cookie_api: &str,
      oauth_api: &str,
   ) -> Result<SessionLease> {
      self
         .acquire_with(
            |kind| match kind {
               SessionKind::OAuth => oauth_api,
               SessionKind::Cookie => cookie_api,
            },
            None,
            None,
         )
         .await
   }

   /// Whether some other session still has budget for `api`.
   ///
   /// Filtered the same way acquisition is, `required_kind` included, so a
   /// caller pinned to one kind is not told a replacement exists that
   /// acquisition will then refuse.
   pub(crate) async fn has_unlimited(
      &self,
      api: &str,
      required_kind: Option<SessionKind>,
      excluded_id: i64,
   ) -> bool {
      let limits = self.limits.read().await;
      self.sessions.iter().any(|slot| {
         slot.credentials.id != excluded_id
            && required_kind.is_none_or(|kind| slot.credentials.kind == kind)
            && limits
               .get(&slot.credentials.id)
               .is_none_or(|session_limits| {
                  !session_limits.rejected
                     && !session_limits.is_limited(api)
                     && !session_limits.refuses(api)
               })
      })
   }

   async fn acquire_with<'a>(
      &self,
      api_for: impl Fn(SessionKind) -> &'a str + Send,
      required_kind: Option<SessionKind>,
      excluded_id: Option<i64>,
   ) -> Result<SessionLease> {
      if self.sessions.is_empty() {
         return Err(Error::NoSessions);
      }

      loop {
         let mut limits = self.limits.write().await;
         match self.scan(&mut limits, &api_for, required_kind, &|id| {
            excluded_id != Some(id)
         }) {
            Ok(lease) => return Ok(lease),
            Err(Miss::Nobody) => return Err(Error::NoSessions),
            Err(Miss::Spent) => return Err(Error::RateLimited),
            Err(Miss::Busy) => {},
         }
         let waiter = self.freed.notified();
         tokio::pin!(waiter);
         // Register before leaving the lock so a release during the handoff
         // is still observed.
         waiter.as_mut().enable();
         drop(limits);
         waiter.await;
      }
   }

   /// One pass for a spare account that `accept` allows, taken only if it
   /// has a free slot now. Used for the second copy of a slow call, which is
   /// not worth waiting for.
   ///
   /// Never waits for the limits lock. The caller holds the first copy
   /// unpolled meanwhile, and that copy may be queued for this lock to
   /// record its account's quota: the queue hands it the lock, it never runs
   /// to release it, and every call to X waits behind both.
   pub(crate) fn try_acquire_where(
      &self,
      api: &str,
      required_kind: Option<SessionKind>,
      accept: impl Fn(i64) -> bool,
   ) -> Option<SessionLease> {
      if self.sessions.is_empty() {
         return None;
      }
      let mut limits = self.limits.try_write().ok()?;
      self
         .scan(&mut limits, &|_| api, required_kind, &accept)
         .ok()
   }

   /// The account with the most calls left, starting from the rotation point.
   /// A known window always beats an account we have never measured: that
   /// unknown account is how a 429 still gets through.
   fn scan<'a>(
      &self,
      limits: &mut HashMap<i64, SessionLimits>,
      api_for: &impl Fn(SessionKind) -> &'a str,
      required_kind: Option<SessionKind>,
      accept: &impl Fn(i64) -> bool,
   ) -> StdResult<SessionLease, Miss> {
      let start = self.cursor.fetch_add(1, Ordering::Relaxed);
      let count = self.sessions.len();
      let mut best: Option<Candidate> = None;
      let mut known_account: Option<usize> = None;
      let mut unknown: Option<usize> = None;
      let mut any = false;
      let mut busy = false;
      for offset in 0..count {
         let index = start.wrapping_add(offset) % count;
         let slot = &self.sessions[index];
         if !accept(slot.credentials.id)
            || required_kind.is_some_and(|kind| slot.credentials.kind != kind)
         {
            continue;
         }
         any = true;
         if slot.permits.available_permits() == 0 {
            busy = true;
            continue;
         }
         let api = api_for(slot.credentials.kind);
         let session_limits = limits.get(&slot.credentials.id);
         let rejected = session_limits.is_some_and(|limits| limits.rejected);
         if rejected {
            continue;
         }
         let verified = session_limits.is_some_and(|limits| limits.verified(api));
         match session_limits.map(|limits| limits.headroom(api)) {
            Some(Headroom::Blocked) => {},
            Some(Headroom::Safe(remaining)) => {
               let better = best.is_none_or(|chosen: Candidate| {
                  remaining > chosen.remaining
                     || (remaining == chosen.remaining && verified && !chosen.verified)
               });
               if better {
                  best = Some(Candidate {
                     index,
                     remaining,
                     verified,
                  });
               }
            },
            Some(Headroom::Unknown) | None => {
               if verified && known_account.is_none() {
                  known_account = Some(index);
               } else if unknown.is_none() {
                  unknown = Some(index);
               }
            },
         }
      }

      let index = best
         .map(|chosen| chosen.index)
         .or(known_account)
         .or(unknown)
         .ok_or(if !any {
            Miss::Nobody
         } else if busy {
            Miss::Busy
         } else {
            Miss::Spent
         })?;
      let slot = &self.sessions[index];
      let api = api_for(slot.credentials.kind);
      let spent = match limits.get_mut(&slot.credentials.id) {
         Some(session_limits) => session_limits.try_spend(api),
         None => Spend::Unmetered,
      };
      if spent == Spend::Denied {
         return Err(if busy { Miss::Busy } else { Miss::Spent });
      }
      let Ok(permit) = Arc::clone(&slot.permits).try_acquire_owned() else {
         return Err(Miss::Busy);
      };
      Ok(self.lease(
         Arc::clone(&slot.credentials),
         permit,
         spent == Spend::Metered,
      ))
   }

   fn lease(
      &self,
      credentials: Arc<SessionCredentials>,
      permit: OwnedSemaphorePermit,
      metered: bool,
   ) -> SessionLease {
      SessionLease {
         credentials,
         permit: Some(permit),
         metered: AtomicBool::new(metered),
         freed: Arc::clone(&self.freed),
      }
   }

   /// How many sessions can still call `api`.
   pub(crate) async fn available_for(&self, api: &str) -> usize {
      let limits = self.limits.read().await;
      self
         .sessions
         .iter()
         .filter(|slot| {
            limits
               .get(&slot.credentials.id)
               .is_none_or(|session_limits| {
                  !session_limits.rejected
                     && !session_limits.is_limited(api)
                     && !session_limits.refuses(api)
               })
         })
         .count()
   }

   /// Return a local spend when the call failed before X counted it.
   pub(crate) async fn refund_if_metered(&self, session: &SessionLease, api: &str) {
      if !session.metered.swap(false, Ordering::Relaxed) {
         return;
      }
      let mut limits = self.limits.write().await;
      if let Some(session_limits) = limits.get_mut(&session.id) {
         session_limits.refund_spend(api);
      }
   }

   /// The call reached X, so the local spend stands.
   pub(crate) fn disarm_meter(&self, session: &SessionLease) {
      session.metered.store(false, Ordering::Relaxed);
   }

   /// Update rate limit info for a session.
   ///
   /// A successful response with valid rate-limit headers proves the session
   /// is working, so clear the global `limited` flag if it has expired.
   pub async fn update_session_limit(
      &self,
      session_id: i64,
      api: &str,
      limit: i32,
      remaining: i32,
      reset: i64,
   ) {
      let mut limits = self.limits.write().await;

      if let Some(lim) = limits.get_mut(&session_id) {
         lim.rejected = false;
         // The session responded, so clear its expired global limit
         if lim.limited && !lim.is_limited(api) {
            lim.limited = false;
         }
         // A local spend already lowered `remaining`. A late header must not
         // raise it back over calls that are still in flight, and a 429 mark
         // of zero stays zero until its reset.
         let now = time::OffsetDateTime::now_utc().unix_timestamp();
         let remaining = if let Some(existing) = lim.apis.get(api) {
            if existing.reset > now && existing.remaining == 0 {
               0
            } else if existing.reset == reset && reset > now {
               remaining.min(existing.remaining)
            } else {
               remaining
            }
         } else {
            remaining
         };
         lim.update_limit(api, limit, remaining, reset);
      }
      drop(limits);

      if remaining <= 0 && reset > time::OffsetDateTime::now_utc().unix_timestamp() {
         self.persist_now(session_id).await;
      } else {
         self.mark_dirty(session_id);
      }
   }

   /// Mark one API as rate limited for a session, leaving its other endpoints
   /// usable. Each endpoint has its own budget, so a 429 on one says nothing
   /// about the rest.
   pub async fn mark_endpoint_limited(&self, session_id: i64, api: &str) {
      let mut limits = self.limits.write().await;
      let damage = limits
         .get_mut(&session_id)
         .map(|lim| lim.note_rate_limit(api));
      drop(limits);
      if let Some(damage) = damage {
         let username = self
            .sessions
            .iter()
            .find(|slot| slot.credentials.id == session_id)
            .map(|slot| slot.credentials.username.clone())
            .unwrap_or_default();
         match damage {
            crate::types::Damage::Search => tracing::info!(
               session_id,
               session_user = %username,
               "account hit a search limit; leaving it out of search"
            ),
            crate::types::Damage::Profile => tracing::info!(
               session_id,
               session_user = %username,
               "account hit a second profile limit; leaving it out of profile pages"
            ),
            crate::types::Damage::Window => {},
         }
      }
      self.persist_now(session_id).await;
   }

   /// Record that a session got results for `api`, which moves it ahead of
   /// unchecked accounts for that call.
   pub async fn mark_verified(&self, session_id: i64, api: &str) {
      if self
         .limits
         .read()
         .await
         .get(&session_id)
         .is_none_or(|session_limits| session_limits.verified(api))
      {
         return;
      }
      let mut limits = self.limits.write().await;
      if let Some(lim) = limits.get_mut(&session_id) {
         lim.verify(api);
      }
      drop(limits);
      self.mark_dirty(session_id);
   }

   /// Cookie sessions nobody has yet seen get results for `api`, or refused
   /// long enough ago to look at again.
   pub async fn unchecked_for(&self, api: &str) -> Vec<i64> {
      let limits = self.limits.read().await;
      self
         .sessions
         .iter()
         .filter(|slot| slot.credentials.kind == SessionKind::Cookie)
         .filter(|slot| {
            limits
               .get(&slot.credentials.id)
               .is_none_or(|session_limits| {
                  !session_limits.rejected && session_limits.unchecked(api)
               })
         })
         .map(|slot| slot.credentials.id)
         .collect()
   }

   /// Leave a session out of one operation X answers it nothing for, while
   /// other accounts get results. Search stays out for good. The rest of its
   /// endpoints stay in use, and a non-search refusal expires after a day.
   pub async fn mark_refused(&self, session_id: i64, api: &str) {
      let username = self
         .sessions
         .iter()
         .find(|slot| slot.credentials.id == session_id)
         .map(|slot| slot.credentials.username.clone())
         .unwrap_or_default();
      let mut limits = self.limits.write().await;
      if let Some(lim) = limits.get_mut(&session_id) {
         lim.refuse(api);
      }
      drop(limits);
      if api.ends_with("SearchTimeline") {
         tracing::info!(
            session_id,
            session_user = %username,
            "account cannot search; leaving it out permanently"
         );
      }
      self.persist_now(session_id).await;
   }

   /// Mark a session's credentials as refused, taking it out of rotation until
   /// the tokens are replaced.
   pub async fn mark_rejected(&self, session_id: i64) {
      let mut limits = self.limits.write().await;

      if let Some(lim) = limits.get_mut(&session_id) {
         lim.rejected = true;
      }
   }

   /// Mark a session as globally rate limited.
   pub async fn mark_limited(&self, session_id: i64) {
      let mut limits = self.limits.write().await;

      if let Some(lim) = limits.get_mut(&session_id) {
         lim.limited = true;
         lim.limited_at = time::OffsetDateTime::now_utc().unix_timestamp();
      }
      drop(limits);
      self.persist_now(session_id).await;
   }

   /// Get session count.
   pub const fn len(&self) -> usize {
      self.sessions.len()
   }

   /// Session ids in load order, used to pin SOCKS5 exits.
   #[must_use]
   pub fn session_ids(&self) -> Vec<i64> {
      self
         .sessions
         .iter()
         .map(|slot| slot.credentials.id)
         .collect()
   }

   /// Check if pool is empty.
   pub const fn is_empty(&self) -> bool {
      self.sessions.is_empty()
   }

   /// Cookie sessions that still have X's default sensitive-content filters on.
   pub async fn pending_filter_sync_ids(&self) -> Vec<i64> {
      let limits = self.limits.read().await;
      self
         .sessions
         .iter()
         .filter_map(|slot| {
            if slot.credentials.kind != SessionKind::Cookie {
               return None;
            }
            if limits
               .get(&slot.credentials.id)
               .is_some_and(|session_limits| {
                  session_limits.filters_cleared && session_limits.age_gate_cleared
               })
            {
               return None;
            }
            Some(slot.credentials.id)
         })
         .collect()
   }

   /// Take a specific session, for startup work that must hit that account.
   pub async fn acquire_id(&self, session_id: i64, api: &str) -> Result<SessionLease> {
      let slot = self
         .sessions
         .iter()
         .find(|slot| slot.credentials.id == session_id)
         .ok_or(Error::NoSessions)?;
      let limits = self.limits.read().await;
      if limits
         .get(&session_id)
         .is_some_and(|session_limits| session_limits.rejected)
      {
         return Err(Error::SessionRejected(format!(
            "session {session_id} rejected"
         )));
      }
      if limits
         .get(&session_id)
         .is_some_and(|session_limits| session_limits.is_limited(api))
      {
         return Err(Error::RateLimited);
      }
      let credentials = Arc::clone(&slot.credentials);
      let permits = Arc::clone(&slot.permits);
      drop(limits);
      let permit = permits
         .acquire_owned()
         .await
         .map_err(|_| Error::Internal("session request limiter closed".into()))?;
      Ok(self.lease(credentials, permit, false))
   }

   /// Remember that this session already had sensitive-content filters turned
   /// off and an adult birthdate written, so a restart does not spend the same
   /// writes again.
   pub async fn mark_filters_cleared(&self, session_id: i64) {
      let mut limits = self.limits.write().await;
      if let Some(lim) = limits.get_mut(&session_id) {
         lim.filters_cleared = true;
         lim.age_gate_cleared = true;
      }
      drop(limits);
      self.persist_now(session_id).await;
   }

   /// The account refused the settings write (`You may not change this
   /// setting`). Take it out of the live pool and keep the credentials in a
   /// side file so they can be fixed later.
   pub async fn retire_filter_blocked(&self, session_id: i64) {
      self.mark_rejected(session_id).await;
      let Some(credentials) = self
         .sessions
         .iter()
         .find(|slot| slot.credentials.id == session_id)
         .map(|slot| Arc::clone(&slot.credentials))
      else {
         return;
      };
      let limits = self
         .limits
         .read()
         .await
         .get(&session_id)
         .cloned()
         .unwrap_or_default();
      let session = Session {
         id: credentials.id,
         username: credentials.username.clone(),
         pending: 0,
         limited: limits.limited,
         limited_at: limits.limited_at,
         apis: limits.apis,
         kind: credentials.kind,
         oauth_token: credentials.oauth_token.clone(),
         oauth_secret: credentials.oauth_secret.clone(),
         auth_token: credentials.auth_token.clone(),
         ct0: credentials.ct0.clone(),
      };
      let Ok(line) = serde_json::to_string(&session) else {
         return;
      };
      let blocked = filter_blocked_path(&self.source_path);
      match append_session_line(&blocked, session_id, &line).await {
         Ok(true) => {
            if let Err(err) = rewrite_sessions_without(&self.source_path, session_id).await {
               tracing::warn!(
                  session_id,
                  "saved the blocked session but could not remove it from {}: {err}",
                  self.source_path
               );
            } else {
               tracing::warn!(
                  session_id,
                  username = %credentials.username,
                  path = %blocked,
                  "moved session that cannot change account settings"
               );
            }
         },
         Ok(false) => {
            tracing::warn!(
               session_id,
               path = %blocked,
               "session was already in the filter-blocked file"
            );
            let _ = rewrite_sessions_without(&self.source_path, session_id).await;
         },
         Err(err) => {
            tracing::warn!(session_id, "failed to save filter-blocked session: {err}");
         },
      }
   }

   /// A cookie session's id and cookie header, for requests that fetch x.com's
   /// web app rather than the API. Takes no concurrency permit, since those
   /// pages are outside the per-session API rate limits. The id lets the fetch
   /// go out as that account's browser, on its proxy.
   pub(crate) fn cookie_header(&self) -> Option<(i64, String)> {
      self
         .sessions
         .iter()
         .map(|slot| &slot.credentials)
         .find(|creds| {
            creds.kind == SessionKind::Cookie
               && !creds.auth_token.is_empty()
               && !creds.ct0.is_empty()
         })
         .map(|creds| {
            (
               creds.id,
               format!("auth_token={}; ct0={}", creds.auth_token, creds.ct0),
            )
         })
   }

   /// Get health statistics about the session pool.
   #[expect(
      clippy::iter_over_hash_type,
      reason = "iteration order irrelevant for aggregation"
   )]
   pub async fn get_health(&self) -> HealthResponse {
      let limits = self.limits.read().await;

      let mut limited_count = 0;
      let mut rejected_count = 0;
      let mut total_requests = 0;
      let mut by_api = HashMap::<String, i32>::new();

      for lim in limits.values() {
         // Counted once by effective state, since a session can carry both
         // flags and a global limit expires on its own.
         if lim.rejected {
            rejected_count += 1;
         } else if lim.is_globally_limited() {
            limited_count += 1;
         }

         for (api, limit_info) in &lim.apis {
            let used = limit_info.limit - limit_info.remaining;
            total_requests += used;
            *by_api.entry(api.clone()).or_default() += used;
         }
      }
      drop(limits);

      HealthResponse {
         sessions: SessionStats {
            total: self.sessions.len(),
            limited: limited_count,
            rejected: rejected_count,
            // Endpoint budgets are per API, so this counts only sessions that
            // are unusable for every endpoint.
            available: self
               .sessions
               .len()
               .saturating_sub(limited_count + rejected_count),
         },
         requests: RequestStats {
            total: total_requests,
            by_api,
         },
         timestamp: time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap(),
      }
   }

   /// Get detailed debug info about sessions.
   pub async fn get_debug(&self) -> DebugResponse {
      let limits = self.limits.read().await;

      let sessions = self
         .sessions
         .iter()
         .map(|slot| {
            let sess = &slot.credentials;
            let lim = limits.get(&sess.id);
            SessionDetail {
               id: sess.id,
               username: sess.username.clone(),
               kind: sess.kind,
               limited: lim.is_some_and(|sl| sl.limited),
               rejected: lim.is_some_and(|sl| sl.rejected),
               limited_at: lim.map_or(0, |sl| sl.limited_at),
               pending: i32::try_from(
                  slot
                     .max_concurrency
                     .saturating_sub(slot.permits.available_permits()),
               )
               .unwrap_or(i32::MAX),
               apis: lim.map(|sl| sl.apis.clone()).unwrap_or_default(),
            }
         })
         .collect();

      DebugResponse {
         sessions,
         count: self.sessions.len(),
         timestamp: time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap(),
      }
   }
}

/// Sign a request with `OAuth1`.
pub fn oauth1_sign(
   method: &str,
   url: &str,
   params: &[(&str, &str)],
   oauth_token: &str,
   oauth_secret: &str,
) -> String {
   // OAuth parameters
   let timestamp = time::OffsetDateTime::now_utc().unix_timestamp().to_string();
   let nonce = format!(
      "{:032x}",
      SystemTime::now()
         .duration_since(UNIX_EPOCH)
         .unwrap()
         .as_nanos()
   );

   let mut oauth_params = vec![
      ("oauth_consumer_key", super::endpoints::CONSUMER_KEY),
      ("oauth_nonce", &nonce),
      ("oauth_signature_method", "HMAC-SHA1"),
      ("oauth_timestamp", &timestamp),
      ("oauth_token", oauth_token),
      ("oauth_version", "1.0"),
   ];

   // RFC 5849 requires sorting by percent-encoded key and then encoded value.
   let mut all_params: Vec<(&str, &str)> = params.to_vec();
   all_params.extend(oauth_params.iter().copied());
   let param_string = normalized_parameter_string(&all_params);

   // Create signature base string
   let base_string = format!(
      "{}&{}&{}",
      method.to_uppercase(),
      percent_encode(url),
      percent_encode(&param_string)
   );

   // Create signing key
   let signing_key = format!(
      "{}&{}",
      percent_encode(super::endpoints::CONSUMER_SECRET),
      percent_encode(oauth_secret)
   );

   // Generate signature
   let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, signing_key.as_bytes());
   let tag = hmac::sign(&key, base_string.as_bytes());
   let signature = BASE64.encode(tag.as_ref());

   oauth_params.push(("oauth_signature", &signature));

   // Build Authorization header
   let auth_header = oauth_params
      .iter()
      .map(|&(param, val)| format!("{}=\"{}\"", param, percent_encode(val)))
      .collect::<Vec<_>>()
      .join(", ");

   format!("OAuth {auth_header}")
}

fn filter_blocked_path(sessions_path: &str) -> String {
   if let Ok(path) = env::var("TEAPOT_FILTER_BLOCKED_FILE")
      && !path.is_empty()
   {
      return path;
   }
   let path = Path::new(sessions_path);
   let name = "sessions-filter-blocked.jsonl";
   path.parent()
      .filter(|dir| !dir.as_os_str().is_empty())
      .map(|dir| dir.join(name).to_string_lossy().into_owned())
      .unwrap_or_else(|| name.to_owned())
}

/// `true` when the line was written. `false` when this id is already there.
async fn append_session_line(path: &str, session_id: i64, line: &str) -> Result<bool> {
   if file_has_session(path, session_id).await? {
      return Ok(false);
   }
   let mut file = fs::OpenOptions::new()
      .create(true)
      .append(true)
      .open(path)
      .await?;
   use tokio::io::AsyncWriteExt as _;
   file.write_all(line.as_bytes()).await?;
   file.write_all(b"\n").await?;
   Ok(true)
}

async fn file_has_session(path: &str, session_id: i64) -> Result<bool> {
   let content = match fs::read_to_string(path).await {
      Ok(content) => content,
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
      Err(err) => return Err(err.into()),
   };
   Ok(content.lines().any(|line| session_line_id(line) == Some(session_id)))
}

async fn rewrite_sessions_without(path: &str, session_id: i64) -> Result<()> {
   let content = fs::read_to_string(path).await?;
   let mut kept = String::with_capacity(content.len());
   for line in content.lines() {
      if line.trim().is_empty() {
         continue;
      }
      if session_line_id(line) == Some(session_id) {
         continue;
      }
      kept.push_str(line);
      kept.push('\n');
   }
   let tmp = format!("{path}.tmp");
   fs::write(&tmp, kept).await?;
   fs::rename(&tmp, path).await?;
   Ok(())
}

fn session_line_id(line: &str) -> Option<i64> {
   let value: serde_json::Value = serde_json::from_str(line).ok()?;
   value.get("id")?.as_i64()
}

fn percent_encode(input: &str) -> String {
   let mut encoded = String::with_capacity(input.len());
   for byte in input.as_bytes() {
      if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
         encoded.push(char::from(*byte));
      } else {
         let _ = write!(encoded, "%{byte:02X}");
      }
   }
   encoded
}

fn normalized_parameter_string(params: &[(&str, &str)]) -> String {
   let mut encoded = params
      .iter()
      .map(|&(key, value)| (percent_encode(key), percent_encode(value)))
      .collect::<Vec<_>>();
   encoded.sort();
   encoded
      .into_iter()
      .map(|(key, value)| format!("{key}={value}"))
      .collect::<Vec<_>>()
      .join("&")
}

#[cfg(test)]
mod tests {
   use std::{env, process, time::Duration};

   use tokio::{fs, time::timeout};

   use super::{SessionKind, SessionPool, normalized_parameter_string, percent_encode};
   use crate::error::Error;

   #[test]
   fn oauth_percent_encoding_follows_rfc_5849() {
      assert_eq!(
         percent_encode("Ladies + Gentlemen"),
         "Ladies%20%2B%20Gentlemen"
      );
      assert_eq!(percent_encode("-._~"), "-._~");
      assert_eq!(percent_encode("☃"), "%E2%98%83");
   }

   #[test]
   fn oauth_parameters_sort_by_encoded_key_and_value() {
      let params = [
         ("b5", "="),
         ("a3", "a"),
         ("c@", ""),
         ("a2", "r b"),
         ("c2", ""),
         ("a3", "2 q"),
      ];
      assert_eq!(
         normalized_parameter_string(&params),
         "a2=r%20b&a3=2%20q&a3=a&b5=%3D&c%40=&c2="
      );
   }

   #[tokio::test]
   async fn a_full_account_hands_the_next_call_over_immediately() {
      unsafe { env::set_var("TEAPOT_SESSION_STATE_FILE", "") };
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-spread-{}.jsonl",
         process::id()
      ));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"one","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n",
            r#"{"id":2,"username":"two","kind":"cookie","auth_token":"b","ct0":"d"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 2).await.unwrap();

      let first = pool
         .acquire("TweetDetail", Some(SessionKind::Cookie))
         .await
         .unwrap();
      let other = if first.id == 1 { 2 } else { 1 };
      let second = pool
         .acquire_excluding("TweetDetail", Some(SessionKind::Cookie), Some(other))
         .await
         .unwrap();
      assert_eq!(first.id, second.id);

      let next = timeout(
         Duration::from_millis(50),
         pool.acquire("TweetDetail", Some(SessionKind::Cookie)),
      )
      .await
      .expect("a full account should be skipped, not waited on")
      .unwrap();
      assert_eq!(next.id, other);

      drop(first);
      drop(second);
      drop(next);
      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn the_account_with_the_most_remaining_goes_next() {
      unsafe { env::set_var("TEAPOT_SESSION_STATE_FILE", "") };
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-richest-{}.jsonl",
         process::id()
      ));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"one","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n",
            r#"{"id":2,"username":"two","kind":"cookie","auth_token":"b","ct0":"d"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 900;
      pool.update_session_limit(1, "TweetDetail", 50, 3, reset)
         .await;
      pool.update_session_limit(2, "TweetDetail", 50, 40, reset)
         .await;
      let got = pool
         .acquire("TweetDetail", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(got.id, 2, "the fuller account is used before the thin one");

      pool.update_session_limit(2, "TweetDetail", 50, 1, reset)
         .await;
      let next = pool
         .acquire("TweetDetail", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(next.id, 1, "an account on its last call is not spent");

      drop(got);
      drop(next);
      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_full_pool_takes_whichever_account_frees() {
      unsafe { env::set_var("TEAPOT_SESSION_STATE_FILE", "") };
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-frees-{}.jsonl",
         process::id()
      ));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"one","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n",
            r#"{"id":2,"username":"two","kind":"cookie","auth_token":"b","ct0":"d"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 1).await.unwrap();
      let first = pool
         .acquire("TweetDetail", Some(SessionKind::Cookie))
         .await
         .unwrap();
      let second = pool
         .acquire("TweetDetail", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_ne!(first.id, second.id);

      let waiting_pool = pool.clone();
      let waiting = tokio::spawn(async move {
         waiting_pool
            .acquire("TweetDetail", Some(SessionKind::Cookie))
            .await
      });
      tokio::time::sleep(Duration::from_millis(30)).await;
      let freed = second.id;
      drop(second);
      let woke = timeout(Duration::from_millis(200), waiting)
         .await
         .expect("a free account should be taken without waiting out another call")
         .unwrap()
         .unwrap();
      assert_eq!(woke.id, freed);

      drop(first);
      drop(woke);
      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn pool_filters_kinds_rotates_and_enforces_concurrency() {
      let path = env::temp_dir().join(format!("teapot-session-pool-{}.jsonl", process::id()));
      let sessions = concat!(
         r#"{"id":1,"username":"cookie","kind":"cookie","auth_token":"a","ct0":"c"}"#,
         "\n",
         r#"{"id":2,"username":"oauth-one","kind":"oauth","oauth_token":"a","oauth_secret":"s"}"#,
         "\n",
         r#"{"id":3,"username":"oauth-two","kind":"oauth","oauth_token":"b","oauth_secret":"t"}"#,
         "\n"
      );
      fs::write(&path, sessions).await.unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 1).await.unwrap();

      let cookie = pool
         .acquire("cookie-api", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(cookie.id, 1);
      assert!(
         timeout(
            Duration::from_millis(10),
            pool.acquire("cookie-api", Some(SessionKind::Cookie)),
         )
         .await
         .is_err()
      );
      drop(cookie);

      let first = pool
         .acquire("oauth-api", Some(SessionKind::OAuth))
         .await
         .unwrap();
      let first_id = first.id;
      drop(first);
      let second = pool
         .acquire("oauth-api", Some(SessionKind::OAuth))
         .await
         .unwrap();
      assert_ne!(first_id, second.id);

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_spent_window_is_not_handed_out_again() {
      // Keep the test from flushing into the live session-limits file.
      // The setter is process-global; this binary is the test run, not teapot.
      unsafe { env::set_var("TEAPOT_SESSION_STATE_FILE", "") };
      let path = env::temp_dir().join(format!("teapot-session-pool-spend-{}.jsonl", process::id()));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"cookie","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 1).await.unwrap();
      let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 3600;
      pool
         .update_session_limit(1, "UserTweets", 50, 1, reset)
         .await;

      let next = pool.acquire("UserTweets", Some(SessionKind::Cookie)).await;
      assert!(
         matches!(next, Err(Error::RateLimited)),
         "the last reserved call is not sent"
      );

      pool.update_session_limit(1, "Other", 50, 5, reset).await;
      let held = pool
         .acquire("Other", Some(SessionKind::Cookie))
         .await
         .unwrap();
      let waiting_pool = pool.clone();
      let waiting = tokio::spawn(async move {
         waiting_pool
            .acquire("Other", Some(SessionKind::Cookie))
            .await
      });
      tokio::time::sleep(Duration::from_millis(30)).await;
      pool.mark_endpoint_limited(1, "Other").await;
      drop(held);
      let woke = timeout(Duration::from_millis(500), waiting)
         .await
         .unwrap()
         .unwrap();
      assert!(matches!(woke, Err(Error::RateLimited)));

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_refused_operation_skips_that_account_only() {
      unsafe { env::set_var("TEAPOT_SESSION_STATE_FILE", "") };
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-refused-{}.jsonl",
         process::id()
      ));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"blocked","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n",
            r#"{"id":2,"username":"fine","kind":"cookie","auth_token":"b","ct0":"d"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      pool.mark_refused(1, "old-id/SearchTimeline").await;

      for _ in 0..4 {
         let lease = pool
            .acquire("new-id/SearchTimeline", Some(SessionKind::Cookie))
            .await
            .unwrap();
         assert_eq!(lease.id, 2);
      }
      assert_eq!(pool.available_for("new-id/SearchTimeline").await, 1);
      assert_eq!(pool.available_for("x/UserTweets").await, 2);

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn accounts_known_to_search_go_first() {
      unsafe { env::set_var("TEAPOT_SESSION_STATE_FILE", "") };
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-verified-{}.jsonl",
         process::id()
      ));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"new","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n",
            r#"{"id":2,"username":"known","kind":"cookie","auth_token":"b","ct0":"d"}"#,
            "\n",
            r#"{"id":3,"username":"new-too","kind":"cookie","auth_token":"e","ct0":"f"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      pool.mark_verified(2, "a/SearchTimeline").await;
      assert_eq!(pool.unchecked_for("b/SearchTimeline").await, vec![1, 3]);

      for _ in 0..5 {
         let lease = pool
            .acquire("b/SearchTimeline", Some(SessionKind::Cookie))
            .await
            .unwrap();
         assert_eq!(lease.id, 2);
      }
      let other = pool
         .acquire_excluding("b/SearchTimeline", Some(SessionKind::Cookie), Some(2))
         .await
         .unwrap();
      assert_ne!(other.id, 2);

      pool.mark_refused(2, "b/SearchTimeline").await;
      assert_eq!(pool.unchecked_for("b/SearchTimeline").await, vec![1, 3]);

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_locked_filter_session_leaves_the_pool_file() {
      unsafe { env::set_var("TEAPOT_SESSION_STATE_FILE", "") };
      unsafe { env::set_var("TEAPOT_FILTER_BLOCKED_FILE", "") };
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-blocked-{}.jsonl",
         process::id()
      ));
      fs::write(
         &path,
         concat!(
            r#"{"id":790,"username":"christina69nh5","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n",
            r#"{"id":2,"username":"fine","kind":"cookie","auth_token":"b","ct0":"d"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      pool.retire_filter_blocked(790).await;

      let kept = fs::read_to_string(&path).await.unwrap();
      assert!(!kept.contains("christina69nh5"));
      assert!(kept.contains("\"id\":2"));
      let blocked = path
         .parent()
         .unwrap()
         .join("sessions-filter-blocked.jsonl");
      let side = fs::read_to_string(&blocked).await.unwrap();
      assert!(side.contains("christina69nh5"));
      assert!(side.contains("\"auth_token\":\"a\""));

      let lease = pool
         .acquire("x/UserTweets", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(lease.id, 2);

      fs::remove_file(&path).await.unwrap();
      fs::remove_file(&blocked).await.unwrap();
   }

   #[tokio::test]
   async fn a_spare_account_is_not_waited_for_behind_a_queued_first_copy() {
      unsafe { env::set_var("TEAPOT_SESSION_STATE_FILE", "") };
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-spare-{}.jsonl",
         process::id()
      ));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"one","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n",
            r#"{"id":2,"username":"two","kind":"cookie","auth_token":"b","ct0":"d"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();

      // X has answered the first copy while another call holds the lock, so
      // it queues to record its account's quota.
      let busy = pool.limits.write().await;
      let mut first = Box::pin(pool.update_session_limit(1, "TweetDetail", 150, 100, 0));
      assert!(
         timeout(Duration::from_millis(20), &mut first)
            .await
            .is_err()
      );
      drop(busy);

      // The hedge timer fires with that copy queued and unpolled.
      assert!(
         pool
            .try_acquire_where("TweetDetail", Some(SessionKind::Cookie), |id| id != 1)
            .is_none()
      );
      timeout(Duration::from_secs(1), &mut first)
         .await
         .expect("the first copy finishes once it is polled again");
      let spare = pool
         .try_acquire_where("TweetDetail", Some(SessionKind::Cookie), |id| id != 1)
         .expect("a free spare once the lock is free");
      assert_eq!(spare.id, 2);

      drop(spare);
      fs::remove_file(path).await.unwrap();
   }
}
