use std::{
   collections::{
      BTreeMap,
      HashMap,
      HashSet,
   },
   fmt::Write as _,
   ops::Deref,
   result::Result as StdResult,
   sync::{
      Arc,
      atomic::{
         AtomicBool,
         AtomicU32,
         AtomicU64,
         AtomicUsize,
         Ordering,
      },
   },
   time::{
      Duration,
      Instant,
      SystemTime,
      UNIX_EPOCH,
   },
};

use data_encoding::BASE64;
use ring::hmac;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
#[cfg(test)] use tokio::fs;
use tokio::{
   sync::{
      Notify,
      OwnedSemaphorePermit,
      Semaphore,
   },
   time::sleep,
};

use super::{
   limits_db,
   log_events::{
      self,
      Event,
   },
   queue::{
      AccountQueue,
      Outcome,
   },
};
use crate::{
   accounts::AccountStore,
   error::{
      Error,
      Result,
   },
   types::{
      AccountPool,
      Headroom,
      RateLimit,
      Session,
      SessionCredentials,
      SessionKind,
      SessionLimits,
      Spend,
      is_profile_op,
      is_self_service,
      operation_name,
   },
};

/// Consecutive empty `SearchTimeline` answers from a supposedly valid
/// (verified) search account before an operational alert is logged.
pub const SEARCH_EMPTY_STREAK_ALERT: u32 = 20;

const fn is_search_empty_streak_milestone(streak: u32) -> bool {
   matches!(streak, SEARCH_EMPTY_STREAK_ALERT | 50 | 100 | 150 | 187)
      || (streak > 187 && streak.is_multiple_of(100))
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountState {
   pub id:                        i64,
   pub username:                  String,
   pub kind:                      SessionKind,
   /// Effective runtime pool: `"search"`, `"search-pass"`, `"view"`,
   /// `"ct0_dead"`, or `"rejected"`.
   pub pool:                      &'static str,
   /// Underlying active pool (`"search"`, `"search-pass"`, or `"view"`) that
   /// the account returns to if a stale `ct0` is replaced or an operator
   /// reinstates it.
   pub home_pool:                 &'static str,
   /// Reserved for search once, in the view pool now. A health check that
   /// finds the account alive puts it back.
   pub demoted_from_search:       bool,
   pub available:                 bool,
   pub has_auth_token:            bool,
   pub ct0_present:               bool,
   pub ct0_len:                   usize,
   pub credentials_rejected:      bool,
   pub search_banned:             bool,
   pub profile_banned:            bool,
   pub ct0_dead:                  bool,
   /// X refused this account itself, so it cannot search. Kept for profile
   /// reads until one of those is refused too.
   pub suspended:                 bool,
   /// Both pools refused it: nothing is asked of this account again.
   pub retired:                   bool,
   /// The settings that hide NSFW media are off for this account.
   pub filters_cleared:           bool,
   /// Unix seconds before which a refused settings write is not retried.
   pub filters_retry_at:          i64,
   pub filters_failed:            bool,
   pub globally_limited:          bool,
   pub limited_at:                Option<i64>,
   pub profile_strikes:           u32,
   pub active_calls:              usize,
   pub last_active:               Option<i64>,
   pub search_empty_streak:       u32,
   pub search_empty_total:        u64,
   pub search_non_empty_total:    u64,
   pub search_rate_limited_total: u64,
   pub last_search_empty:         Option<i64>,
   pub rate_limits:               BTreeMap<String, RateLimit>,
   pub refused_operations:        BTreeMap<String, i64>,
   pub verified_operations:       BTreeMap<String, i64>,
}

/// Specific reasons an account is counted under "Out / unverified" for one or
/// more tracked operations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OutReasons {
   pub ct0_dead:             bool,
   pub credentials_rejected: bool,
   pub retired:              bool,
   pub unverified_search:    bool,
   pub search_banned:        bool,
   pub search_404_cooldown:  bool,
   pub profile_banned:       bool,
   pub globally_limited:     bool,
   pub refused_operations:   Vec<String>,
}

impl OutReasons {
   #[must_use]
   pub const fn any(&self) -> bool {
      self.ct0_dead
         || self.credentials_rejected
         || self.retired
         || self.unverified_search
         || self.search_banned
         || self.search_404_cooldown
         || self.profile_banned
         || self.globally_limited
         || !self.refused_operations.is_empty()
   }

   #[must_use]
   pub fn matches_filter(&self, reason: &str, suspended: bool) -> bool {
      match reason {
         "ct0_dead" => self.ct0_dead,
         "credentials_rejected" => self.credentials_rejected,
         "unverified" => self.unverified_search,
         "retired" => self.retired,
         "suspended" => suspended,
         "search_banned" => self.search_banned,
         "profile_banned" => self.profile_banned,
         "refused" => !self.refused_operations.is_empty() || self.search_404_cooldown,
         "search_404" => self.search_404_cooldown,
         "globally_limited" => self.globally_limited,
         _ => true,
      }
   }

   fn merge(&mut self, other: Self) {
      self.ct0_dead |= other.ct0_dead;
      self.credentials_rejected |= other.credentials_rejected;
      self.retired |= other.retired;
      self.unverified_search |= other.unverified_search;
      self.search_banned |= other.search_banned;
      self.search_404_cooldown |= other.search_404_cooldown;
      self.profile_banned |= other.profile_banned;
      self.globally_limited |= other.globally_limited;
      for op in other.refused_operations {
         if !self.refused_operations.contains(&op) {
            self.refused_operations.push(op);
         }
      }
   }

   const fn priority_rank(&self, suspended: bool) -> u8 {
      if self.ct0_dead {
         0
      } else if self.credentials_rejected {
         1
      } else if self.unverified_search {
         2
      } else if self.retired {
         3
      } else if self.search_banned || self.profile_banned || suspended {
         4
      } else if !self.refused_operations.is_empty()
         || self.search_404_cooldown
         || self.globally_limited
      {
         5
      } else {
         6
      }
   }
}

/// Live summary of one account listed on the Out / unverified investigation
/// page.
#[derive(Debug, Clone, Serialize)]
pub struct OutAccountEntry {
   pub id:                  i64,
   pub username:            String,
   pub kind:                SessionKind,
   pub pool:                &'static str,
   pub home_pool:           &'static str,
   pub demoted_from_search: bool,
   pub has_auth_token:      bool,
   pub ct0_present:         bool,
   pub ct0_len:             usize,
   pub suspended:           bool,
   pub filters_cleared:     bool,
   pub filters_failed:      bool,
   pub verified_search:     bool,
   pub last_active:         Option<i64>,
   pub search_empty_streak: u32,
   pub profile_strikes:     u32,
   pub reasons:             OutReasons,
}

/// Counts per reason across the currently scoped operation/pool/search filter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct OutBreakdown {
   pub total:                usize,
   pub ct0_dead:             usize,
   pub credentials_rejected: usize,
   pub unverified_search:    usize,
   pub retired:              usize,
   pub suspended:            usize,
   pub search_banned:        usize,
   pub profile_banned:       usize,
   pub refused:              usize,
   pub globally_limited:     usize,
}

/// Live out / unverified counts across every tracked operation and quarantine
/// pool tab.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct OutOperationCounts {
   pub all_out:             usize,
   pub search_timeline:     usize,
   pub search_pass:         usize,
   pub user_tweets:         usize,
   pub user_by_screen_name: usize,
   pub tweet_detail:        usize,
   pub demoted_from_search: usize,
   pub ct0_dead_pool:       usize,
   pub rejected_pool:       usize,
}

/// One paginated slice of Out / unverified accounts from the live pool.
#[derive(Debug, Clone, Serialize)]
pub struct OutAccountsSnapshot {
   pub counts:         OutOperationCounts,
   pub breakdown:      OutBreakdown,
   pub total_filtered: usize,
   pub page:           usize,
   pub total_pages:    usize,
   pub page_size:      usize,
   pub accounts:       Vec<OutAccountEntry>,
   pub now:            i64,
}

/// How many loaded accounts carry each mark, for the dashboard. One account
/// can carry several, so the marks do not add up to the total.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PoolCensus {
   pub total:                usize,
   /// Active accounts reserved for public search (excludes `ct0_dead` and
   /// `rejected` pools).
   pub search:               usize,
   /// Active accounts reserved for paid search-pass searches (excludes
   /// `ct0_dead` and `rejected` pools).
   pub search_pass:          usize,
   /// Active accounts for profile and post reads (excludes `ct0_dead` and
   /// `rejected` pools).
   pub view:                 usize,
   /// Accounts quarantined in the `ct0_dead` pool until a fresh CSRF token is
   /// supplied.
   pub ct0_dead_pool:        usize,
   /// Permanently dead accounts quarantined in the `rejected` pool (locked,
   /// credentials rejected, or retired).
   pub rejected_pool:        usize,
   /// In the active view pool, but reserved for search before a verdict moved
   /// them.
   pub demoted_from_search:  usize,
   /// Out of every call until an operator acts: credentials rejected, a
   /// stale `ct0`, or retired.
   pub out_of_rotation:      usize,
   pub credentials_rejected: usize,
   pub ct0_dead:             usize,
   pub retired:              usize,
   pub suspended:            usize,
   pub search_banned:        usize,
   pub profile_banned:       usize,
   /// X refused the settings write, so the account's filters may still be on.
   pub settings_refused:     usize,
   pub filters_cleared:      usize,
   pub globally_limited:     usize,
   /// Calls parked waiting for an account right now.
   pub parked_calls:         usize,
   pub operations:           Vec<OperationPool>,
}

/// Where the accounts of one pool stand for one call, counted the way the
/// minute report counts them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct OperationPool {
   pub operation: String,
   /// The pool that owns this call: `search`, `search-pass`, or `view`.
   pub pool:      &'static str,
   /// Accounts in that pool.
   pub accounts:  usize,
   /// Has quota and a free slot.
   pub free:      usize,
   /// Has quota, every slot mid-call.
   pub busy:      usize,
   /// Free, but kept for a call already parked on it.
   pub held:      usize,
   /// Sitting out a bare 429.
   pub resting:   usize,
   /// Window used up until X resets it.
   pub spent:     usize,
   /// Cannot take this call: rejected, refused, limited, or unverified.
   pub out:       usize,
}

#[derive(Serialize)]
pub struct HealthResponse {
   pub sessions:  SessionStats,
   pub requests:  RequestStats,
   pub timestamp: String,
}

#[derive(Serialize)]
pub struct SessionStats {
   pub total:     usize,
   pub limited:   usize,
   pub rejected:  usize,
   pub available: usize,
}

#[derive(Serialize)]
pub struct RequestStats {
   pub total:  i32,
   pub by_api: HashMap<String, i32>,
}

#[derive(Serialize)]
pub struct DebugResponse {
   pub sessions:  Vec<SessionDetail>,
   pub count:     usize,
   pub timestamp: String,
}

#[derive(Serialize)]
pub struct SessionDetail {
   pub id:         i64,
   pub username:   String,
   pub kind:       SessionKind,
   pub limited:    bool,
   pub rejected:   bool,
   pub limited_at: i64,
   pub pending:    i32,
   pub apis:       HashMap<String, RateLimit>,
}

/// Why X refused the account, which decides what the verdict costs.
///
/// A suspension is learned **once**. The settings write is the only call that
/// asks an account to prove it is not suspended, so that write is the probe: it
/// runs, records the answer, and never runs for that account again. A data call
/// is not a probe — it is real work — and a suspension reported there is X
/// answering a call the pool had already decided was worth making.
///
/// The distinction matters because it decides whether a refusal is a new fact
/// or a repeat of one already on record. Thousands of suspended accounts each
/// re-probed daily is 1,600 wasted settings writes a day to re-learn what the
/// pool already stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SuspensionProbe {
   /// The `account/settings` write: the one call that asks the question.
   Settings,
   /// A real call the pool handed out, search or profile read.
   Data,
}

/// Pool of authentication sessions for Twitter API.
#[derive(Clone)]
pub struct SessionPool {
   /// Requests clone an immutable snapshot of the roster (`slots` + O(1)
   /// `by_id` index), not all of its slots. Publishing an import swaps the
   /// snapshot without replacing existing slot identities.
   sessions:             Arc<std::sync::RwLock<Arc<SessionRoster>>>,
   max_concurrency:      usize,
   cursor:               Arc<AtomicUsize>,
   recovery_cursor:      Arc<AtomicUsize>,
   limit_writer:         Option<limits_db::LimitsDb>,
   account_store:        Option<AccountStore>,
   #[cfg(test)]
   source_path:          String,
   #[cfg(test)]
   source_write:         Arc<tokio::sync::Mutex<()>>,
   writes:               Arc<AccountWrites>,
   /// Calls parked until an account frees up, for the minute report.
   waiting:              Arc<AtomicUsize>,
   /// Time each call spent getting an account, reported every minute.
   queue:                Arc<AccountQueue>,
   telemetry:            Option<crate::stats::StatsStore>,
   /// Unix timestamp (seconds) of the latest successful non-empty tweet
   /// search across the pool, used to tell a broken account apart from a
   /// global X search outage.
   last_tweet_search_ok: Arc<AtomicU64>,
}

type SessionSlots = Arc<[Arc<SessionSlot>]>;

struct SessionRoster {
   slots: SessionSlots,
   by_id: HashMap<i64, usize>,
}

impl Default for SessionRoster {
   fn default() -> Self {
      Self {
         slots: Arc::from([]),
         by_id: HashMap::new(),
      }
   }
}

#[derive(Default)]
struct PendingAccountWrites {
   ct0s:      HashMap<i64, String>,
   demotions: HashSet<i64>,
}

#[derive(Default)]
struct AccountWrites {
   pending: std::sync::Mutex<PendingAccountWrites>,
   ready:   Notify,
}

impl AccountWrites {
   fn retry_demotion(&self, id: i64) {
      self
         .pending
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner)
         .demotions
         .insert(id);
      self.ready.notify_one();
   }

   fn cancel_demotion(&self, id: i64) {
      self
         .pending
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner)
         .demotions
         .remove(&id);
   }
}

/// Coalesce account writes without making an upstream response await the DB.
const ACCOUNT_WRITE_DELAY: Duration = Duration::from_secs(2);
/// How long an account's pooled HTTP/2 connection to X stays open while idle.
const WARM_CONNECTION_SECS: u64 = 900;
/// An account on an exit that trails the fleet ranks as if it had this
/// fraction of the calls it has left, and stays strictly behind quick exits
/// that still hold at least [`SLOW_EXIT_FAST_FLOOR`] calls.
const SLOW_EXIT_DISCOUNT: i32 = 4;
/// Minimum calls remaining on a quick exit for it to rank strictly ahead of
/// any slow exit on normal calls.
const SLOW_EXIT_FAST_FLOOR: i32 = 5;
/// Minimum calls remaining for warm HTTP/2 connection affinity to prefer an
/// already-warm quick exit over spinning up a cold connection.
const WARM_HEADROOM_FLOOR: i32 = 15;
/// Accounts a parked call listens on. A release on any of them wakes it.
const WAIT_ON_ACCOUNTS: usize = 32;
/// A parked call looks at the pool again after this even if none of its
/// accounts frees, for one whose rest or window ran out meanwhile.
const RESCAN_AFTER: Duration = Duration::from_secs(1);
/// The longest a call waits for an account before the visitor is told to try
/// again. When X has most accounts resting, the few left cannot drain the
/// line, and waiting it out only ended at the 45 s page timeout.
const MAX_PARK: Duration = Duration::from_secs(2);
/// Calls allowed in line per slot on the accounts a call would wait on. A
/// profile lookup holds a slot about 150 ms, so a line this deep clears in
/// about a second; a deeper one is turned away at once instead of joining.
const QUEUE_PER_SLOT: usize = 8;

/// Who is asking, which decides the accounts it may take.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Caller {
   /// A call that has not queued. It leaves alone an account a parked call
   /// is waiting on, so the slot that frees goes to the call already waiting.
   Fresh,
   /// The second copy of a slow call: a warm connection on a quick exit
   /// first, and never an account a parked call is waiting on.
   Hedge,
   /// A parked call that was woken. It may take the accounts it waited on.
   Queued,
}

/// Which search pool a `SearchTimeline` scan inspects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchScope {
   /// Public `search` pool only (`search_only && !search_pass_only`).
   PublicOnly,
   /// Dedicated paid `search-pass` pool only (`search_only && search_pass_only`).
   PassOnly,
}

impl SearchScope {
   const fn matches_flags(self, search_only: bool, search_pass_only: bool) -> bool {
      match self {
         Self::PublicOnly => search_only && !search_pass_only,
         Self::PassOnly => search_only && search_pass_only,
      }
   }
}

/// Why one pass over the pool found no account.
#[derive(Debug)]
enum Miss {
   /// None can serve this call: rejected, refused, or not acceptable.
   Nobody,
   /// Every candidate is out of quota.
   Spent,
   /// A candidate has quota but is at its concurrency cap.
   Busy,
}

impl Miss {
   /// What the queue report counts and what the caller is told, for a miss
   /// that is not waited out. Busy is not a rate limit: the account still has
   /// quota and another call holds its slot. Search has one such account, so
   /// reporting that as spent told visitors the account was out of calls.
   fn settle(self) -> (Outcome, Error) {
      match self {
         Self::Nobody => (Outcome::Nobody, Error::NoSessions),
         Self::Spent => (Outcome::Spent, Error::RateLimited),
         Self::Busy => (Outcome::Shed, Error::TransientUpstream),
      }
   }
}

/// A candidate with known budget. Quick exits with at least
/// [`SLOW_EXIT_FAST_FLOOR`] calls rank ahead of slow exits (which are also
/// discounted by [`SLOW_EXIT_DISCOUNT`]). Among quick exits, warm HTTP/2
/// connections with at least [`WARM_HEADROOM_FLOOR`] calls rank ahead of cold
/// connections on view endpoints; `SearchTimeline` instead spreads across the
/// fullest idle accounts so a small search pool is not hammered into
/// suspension.
#[derive(Clone, Copy)]
struct Candidate {
   index:     usize,
   remaining: i32,
   verified:  bool,
   warm:      bool,
   slow_exit: bool,
   idle:      bool,
}

/// Default budget assumed for a verified `SearchTimeline` account whose
/// 15-minute window has already expired (or has not been observed yet).
const SEARCH_DEFAULT_LIMIT: i32 = 187;

/// Short cooldown after an uncorroborated `SearchTimeline` `"Internal server
/// error"` (every tried account failed the query, or immediately on failure
/// before the retry loop finishes).
pub(crate) const SEARCH_UNCORROBORATED_FAIL_REST_SECS: i64 = 45;
/// Cooldown after a corroborated `SearchTimeline` `"Internal server error"`
/// (another account answered the exact same query). Keeps a newly suspended
/// account out of rotation immediately while giving a transient blip one
/// second chance on a different query before demoting.
const SEARCH_CORROBORATED_FAIL_REST_SECS: i64 = 120;
/// Minimum seconds between strike 1 and strike 2 on corroborated failures so
/// two concurrent requests hitting a brief connection blip cannot demote a
/// healthy account.
const SEARCH_CORROBORATED_MIN_GAP_SECS: u64 = 10;
/// Distinct corroborated failing queries in a row (separated by at least
/// [`SEARCH_CORROBORATED_MIN_GAP_SECS`]) before demoting to the view pool.
const SEARCH_CORROBORATED_DEMOTE_THRESHOLD: u32 = 2;
/// Distinct uncorroborated failing queries in a row while the pool is healthy
/// before running one background `"news"` canary check on the account.
const SEARCH_CANARY_TRIGGER_THRESHOLD: u32 = 3;
/// How recently some account must have answered a non-empty tweet search for
/// X search to be considered globally healthy (not in an outage).
const TWEET_SEARCH_HEALTHY_WINDOW_SECS: u64 = 300;

/// What [`SessionPool::record_search_timeline_failure`] decided to do with a
/// search account after `"Internal server error"` on
/// `search_timeline.timeline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchFailureAction {
   Rested,
   Demoted,
   NeedsCanary,
}

#[derive(Default)]
struct SearchFailureState {
   corroborated_streak:   u32,
   corroborated_query:    u64,
   corroborated_at:       u64,
   uncorroborated_streak: u32,
   uncorroborated_query:  u64,
   canary_in_flight:      bool,
}

/// Where the accounts for one call stood, for the minute report.
#[derive(Default)]
struct PoolCount {
   free:    usize,
   busy:    usize,
   held:    usize,
   resting: usize,
   spent:   usize,
   out:     usize,
}

/// Number of top-ranked candidates retained during a parallel read-only scan
/// so a concurrent caller claiming the first choice falls through to the next
/// best account without rescanning the pool.
const SCAN_TOP_K: usize = 16;
/// Eligible, non-blocked slots inspected before early-stopping a scan once
/// `best` already holds [`SCAN_TOP_K`] fast-exit `Safe` candidates.
const SCAN_PROBES: usize = 96;
/// Prime stride across large pools so concurrent callers inspect different
/// windows instead of contending on the same accounts.
const SCAN_STRIDE: usize = 19;
/// Bounded rescan attempts if every top-K candidate is claimed concurrently
/// between the read pass and per-slot claim.
const SCAN_CLAIM_RETRIES: usize = 3;

/// Pre-classified operation metadata reused across all slots in a scan pass.
#[derive(Clone, Copy)]
struct ApiSpec<'a> {
   api:          &'a str,
   op_name:      &'a str,
   is_search:    bool,
   self_service: bool,
   profile_op:   bool,
}

impl<'a> ApiSpec<'a> {
   fn new(api: &'a str) -> Self {
      let op_name = operation_name(api);
      Self {
         api,
         op_name,
         is_search: op_name == "SearchTimeline",
         self_service: is_self_service(op_name),
         profile_op: is_profile_op(op_name),
      }
   }
}

const fn candidate_rank(
   candidate: Candidate,
   prefer_warm: bool,
   is_search: bool,
) -> (bool, bool, i32, bool, bool) {
   if prefer_warm {
      (
         !candidate.slow_exit,
         candidate.warm,
         candidate.remaining,
         candidate.verified,
         false,
      )
   } else if is_search {
      let fast_healthy = !candidate.slow_exit && candidate.remaining >= SLOW_EXIT_FAST_FLOOR;
      (
         fast_healthy,
         false,
         candidate.remaining,
         candidate.idle,
         candidate.warm,
      )
   } else {
      let fast_healthy = !candidate.slow_exit && candidate.remaining >= SLOW_EXIT_FAST_FLOOR;
      let fast_warm_ample =
         fast_healthy && candidate.warm && candidate.remaining >= WARM_HEADROOM_FLOOR;
      (
         fast_healthy,
         fast_warm_ample,
         candidate.remaining,
         candidate.verified,
         candidate.warm,
      )
   }
}

fn better_candidate(
   candidate: Candidate,
   chosen: Candidate,
   prefer_warm: bool,
   is_search: bool,
) -> bool {
   candidate_rank(candidate, prefer_warm, is_search)
      > candidate_rank(chosen, prefer_warm, is_search)
}

fn can_stop_early(best: &[Candidate]) -> bool {
   best.len() >= SCAN_TOP_K && best.iter().all(|chosen| !chosen.slow_exit)
}

fn insert_best(
   best: &mut Vec<Candidate>,
   candidate: Candidate,
   prefer_warm: bool,
   is_search: bool,
) {
   let pos = best
      .iter()
      .position(|&existing| better_candidate(candidate, existing, prefer_warm, is_search))
      .unwrap_or(best.len());
   if pos < SCAN_TOP_K {
      best.insert(pos, candidate);
      if best.len() > SCAN_TOP_K {
         best.pop();
      }
   }
}

fn insert_fallback(fallback: &mut Vec<(u8, usize)>, rank: u8, index: usize) {
   let pos = fallback
      .iter()
      .position(|&(existing, _)| rank < existing)
      .unwrap_or(fallback.len());
   if pos < SCAN_TOP_K {
      fallback.insert(pos, (rank, index));
      if fallback.len() > SCAN_TOP_K {
         fallback.pop();
      }
   }
}

struct SessionSlot {
   credentials:               Arc<SessionCredentials>,
   /// A `ct0` x.com issued on a rejection, sent in place of the stored one.
   /// The credentials are shared and loaded once, so the fresh value lives
   /// here and is written back to the account store for the next start.
   fresh_ct0:                 std::sync::RwLock<Option<String>>,
   permits:                   Arc<Semaphore>,
   max_concurrency:           usize,
   /// Signaled when a lease on this account returns its permit, waking one
   /// call parked on this account and no other.
   freed:                     Arc<Notify>,
   /// Parked calls listening on this account.
   wanted:                    AtomicUsize,
   /// Unix timestamp (seconds) when this session last completed an upstream
   /// call.
   last_active:               AtomicU64,
   /// The exit this account is pinned to answers well behind the rest.
   slow_exit:                 AtomicBool,
   /// Lock-free mirrors of `limits` flags so scans skip wrong-pool and dead
   /// accounts without touching `limits`. Always kept in sync by
   /// `write_limits`.
   rejected:                  AtomicBool,
   search_only:               AtomicBool,
   search_pass_only:          AtomicBool,
   unavailable:               AtomicBool,
   /// Mutable rate-limit and verdict state for this account alone. Never held
   /// across `.await`.
   limits:                    std::sync::RwLock<SessionLimits>,
   /// Consecutive empty `SearchTimeline` answers without a non-empty page.
   search_empty_streak:       AtomicU32,
   /// Process-lifetime empty `SearchTimeline` answers.
   search_empty_total:        AtomicU64,
   /// Process-lifetime non-empty `SearchTimeline` answers.
   search_non_empty_total:    AtomicU64,
   /// Process-lifetime `SearchTimeline` rate-limit hits / switches.
   search_rate_limited_total: AtomicU64,
   /// Unix timestamp (seconds) of the latest empty `SearchTimeline` answer.
   last_search_empty:         AtomicU64,
   /// Fast-path flag indicating non-zero streaks inside `search_failures`.
   has_search_failures:       AtomicBool,
   /// Consecutive tweet-search `"Internal server error"` failure streaks.
   search_failures:           std::sync::Mutex<SearchFailureState>,
}

impl SessionSlot {
   fn new(credentials: SessionCredentials, limits: SessionLimits, max_concurrency: usize) -> Self {
      let rejected = limits.rejected;
      let search_only = limits.search_only;
      let search_pass_only = limits.search_pass_only;
      let unavailable = limits.ct0_dead || limits.retired;
      Self {
         credentials: Arc::new(credentials),
         fresh_ct0: std::sync::RwLock::new(None),
         permits: Arc::new(Semaphore::new(max_concurrency)),
         max_concurrency,
         freed: Arc::new(Notify::new()),
         wanted: AtomicUsize::new(0),
         last_active: AtomicU64::new(0),
         slow_exit: AtomicBool::new(false),
         rejected: AtomicBool::new(rejected),
         search_only: AtomicBool::new(search_only),
         search_pass_only: AtomicBool::new(search_pass_only),
         unavailable: AtomicBool::new(unavailable),
         limits: std::sync::RwLock::new(limits),
         search_empty_streak: AtomicU32::new(0),
         search_empty_total: AtomicU64::new(0),
         search_non_empty_total: AtomicU64::new(0),
         search_rate_limited_total: AtomicU64::new(0),
         last_search_empty: AtomicU64::new(0),
         has_search_failures: AtomicBool::new(false),
         search_failures: std::sync::Mutex::new(SearchFailureState::default()),
      }
   }

   fn ct0(&self) -> String {
      self
         .fresh_ct0
         .read()
         .unwrap_or_else(std::sync::PoisonError::into_inner)
         .clone()
         .unwrap_or_else(|| self.credentials.ct0.clone())
   }

   fn read_limits(&self) -> std::sync::RwLockReadGuard<'_, SessionLimits> {
      self
         .limits
         .read()
         .unwrap_or_else(std::sync::PoisonError::into_inner)
   }

   fn try_read_limits(&self) -> Option<std::sync::RwLockReadGuard<'_, SessionLimits>> {
      match self.limits.try_read() {
         Ok(guard) => Some(guard),
         Err(std::sync::TryLockError::Poisoned(err)) => Some(err.into_inner()),
         Err(std::sync::TryLockError::WouldBlock) => None,
      }
   }

   fn write_limits<R>(&self, update: impl FnOnce(&mut SessionLimits) -> R) -> R {
      let mut guard = self
         .limits
         .write()
         .unwrap_or_else(std::sync::PoisonError::into_inner);
      let result = update(&mut guard);
      self.sync_flags(&guard);
      result
   }

   fn try_write_limits<R>(&self, update: impl FnOnce(&mut SessionLimits) -> R) -> Option<R> {
      let mut guard = match self.limits.try_write() {
         Ok(guard) => guard,
         Err(std::sync::TryLockError::Poisoned(err)) => err.into_inner(),
         Err(std::sync::TryLockError::WouldBlock) => return None,
      };
      let result = update(&mut guard);
      self.sync_flags(&guard);
      Some(result)
   }

   fn sync_flags(&self, lim: &SessionLimits) {
      self.rejected.store(lim.rejected, Ordering::Relaxed);
      self.search_only.store(lim.search_only, Ordering::Relaxed);
      self
         .search_pass_only
         .store(lim.search_pass_only, Ordering::Relaxed);
      self
         .unavailable
         .store(lim.ct0_dead || lim.retired, Ordering::Relaxed);
   }
}

/// A parked call's claim on the accounts it listens on, given up on drop.
struct Wanted {
   sessions: SessionSlots,
   indices:  Vec<usize>,
}

impl Wanted {
   fn claim(sessions: SessionSlots, indices: Vec<usize>) -> Self {
      for &index in &indices {
         sessions[index].wanted.fetch_add(1, Ordering::Relaxed);
      }
      Self { sessions, indices }
   }
}

/// Letting go hands each account on: one that is free and still wanted wakes
/// the next call parked on it. Without this, a call that parked on an account
/// already free but held for others sleeps until its rescan, and a call
/// dropped right after its wake-up takes that wake-up with it.
impl Drop for Wanted {
   fn drop(&mut self) {
      for &index in &self.indices {
         let slot = &self.sessions[index];
         if slot.wanted.fetch_sub(1, Ordering::Relaxed) > 1 && slot.permits.available_permits() > 0
         {
            slot.freed.notify_one();
         }
      }
   }
}

/// Counts a call as parked for as long as it lives.
struct Parked<'a>(&'a AtomicUsize);

impl<'a> Parked<'a> {
   fn new(waiting: &'a AtomicUsize) -> Self {
      waiting.fetch_add(1, Ordering::Relaxed);
      Self(waiting)
   }
}

impl Drop for Parked<'_> {
   fn drop(&mut self) {
      self.0.fetch_sub(1, Ordering::Relaxed);
   }
}

/// A session and the concurrency permit held for its request lifetime.
pub struct SessionLease {
   credentials: Arc<SessionCredentials>,
   permit:      Option<OwnedSemaphorePermit>,
   /// This lease decremented the endpoint's local remaining count.
   metered:     AtomicBool,
   freed:       Arc<Notify>,
}

impl Drop for SessionLease {
   fn drop(&mut self) {
      // Return the slot before waking anyone. A scan that runs while the
      // permit is still held goes back to sleep and misses this signal.
      // Only a call parked on this very account is woken: one woken for an
      // account it cannot use went back to sleep and took the signal with it.
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
   /// The store refuses to load until the offline migration has completed.
   pub async fn from_store(store: AccountStore, max_concurrent_requests: u32) -> Result<Self> {
      let sessions = store.load().await?;
      let mut pool = Self::empty(max_concurrent_requests);
      pool.append_sessions(sessions).await;
      // Keep the existing PostgreSQL limit history, but never bootstrap it
      // from a legacy file in the web process.
      let db = limits_db::Connected::connect().await?;
      let saved = db.load().await?;
      let mut restored = 0;
      {
         let roster = pool.roster();
         for (id, mut previous) in saved {
            if let Some(&index) = roster.by_id.get(&id) {
               roster.slots[index].write_limits(|current| {
                  // The store says which pool an account is in and which one it
                  // came from; the saved limits only remember what X answered.
                  previous.search_only = current.search_only;
                  previous.search_pass_only = current.search_pass_only;
                  previous.demoted_from_search = current.demoted_from_search;
                  previous.demoted_from_search_pass = current.demoted_from_search_pass;
                  *current = previous;
               });
               restored += 1;
            }
         }
      }
      tracing::info!(
         sessions = pool.len(),
         restored,
         "loaded account pool from PostgreSQL"
      );
      let snapshot_sessions = Arc::clone(&pool.sessions);
      pool.limit_writer = Some(db.spawn(move |id| {
         let roster = Arc::clone(
            &snapshot_sessions
               .read()
               .unwrap_or_else(std::sync::PoisonError::into_inner),
         );
         let &index = roster.by_id.get(&id)?;
         Some(roster.slots.get(index)?.read_limits().clone())
      }));
      pool.account_store = Some(store.clone());
      pool.spawn_account_writer(store);
      Ok(pool)
   }

   #[cfg(test)]
   async fn load(path: &str, max_concurrent_requests: u32) -> Result<Self> {
      let content = fs::read_to_string(path).await?;
      let mut pool = Self::empty(max_concurrent_requests);
      pool.source_path = path.to_owned();
      pool
         .append_sessions(
            content
               .lines()
               .filter_map(|line| serde_json::from_str(line).ok())
               .collect(),
         )
         .await;
      Ok(pool)
   }

   fn empty(max_concurrent_requests: u32) -> Self {
      Self {
         sessions:                  Arc::new(std::sync::RwLock::new(Arc::new(
            SessionRoster::default(),
         ))),
         max_concurrency:           usize::try_from(max_concurrent_requests.max(1))
            .unwrap_or(usize::MAX),
         cursor:                    Arc::default(),
         recovery_cursor:           Arc::default(),
         limit_writer:              None,
         account_store:             None,
         #[cfg(test)]
         source_path:               String::new(),
         #[cfg(test)]
         source_write:              Arc::default(),
         writes:                    Arc::default(),
         waiting:                   Arc::default(),
         queue:                     Arc::default(),
         telemetry:                 None,
         last_tweet_search_ok:      Arc::new(AtomicU64::new(0)),
      }
   }

   fn roster(&self) -> Arc<SessionRoster> {
      Arc::clone(
         &self
            .sessions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
      )
   }

   fn slots(&self) -> SessionSlots {
      Arc::clone(&self.roster().slots)
   }

   fn slot(&self, session_id: i64) -> Option<Arc<SessionSlot>> {
      let roster = self.roster();
      let &index = roster.by_id.get(&session_id)?;
      roster.slots.get(index).map(Arc::clone)
   }

   /// Append accounts only. Existing slots, credentials, limits and leases
   /// retain their identity even if a caller holds the previous snapshot.
   pub(super) async fn append_sessions(&self, sessions: Vec<Session>) -> usize {
      if sessions.is_empty() {
         return 0;
      }
      let mut roster_guard = self
         .sessions
         .write()
         .unwrap_or_else(std::sync::PoisonError::into_inner);
      let mut by_id = roster_guard.by_id.clone();
      let base = roster_guard.slots.len();
      let mut new_slots = Vec::new();
      for session in sessions {
         if by_id.contains_key(&session.id) {
            continue;
         }
         let pool = AccountPool::from_stored(&session.pool);
         let (creds, mut lims) = session.into_credentials_and_limits();
         match pool {
            AccountPool::Search => lims.reserve_for_search(),
            AccountPool::SearchPass => lims.reserve_for_search_pass(),
            AccountPool::View => {},
         }
         by_id.insert(creds.id, base + new_slots.len());
         new_slots.push(Arc::new(SessionSlot::new(
            creds,
            lims,
            self.max_concurrency,
         )));
      }
      let count = new_slots.len();
      if count > 0 {
         let mut slots = roster_guard.slots.to_vec();
         slots.extend(new_slots);
         *roster_guard = Arc::new(SessionRoster {
            slots: slots.into(),
            by_id,
         });
      }
      count
   }

   fn mark_dirty(&self, session_id: i64) {
      if let Some(db) = &self.limit_writer {
         db.save(session_id);
      }
   }

   fn persist_soon(&self, session_id: i64) {
      self.mark_dirty(session_id);
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
      self
         .acquire_with_filter(|_| api, required_kind, |id| excluded_id != Some(id))
         .await
   }

   /// Acquire a session matching an arbitrary predicate (e.g. excluding tried
   /// sessions and stalled proxy ports).
   pub(crate) async fn acquire_where(
      &self,
      api: &str,
      required_kind: Option<SessionKind>,
      accept: impl Fn(i64) -> bool + Send,
   ) -> Result<SessionLease> {
      self
         .acquire_with_filter(|_| api, required_kind, accept)
         .await
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
         .acquire_with_filter(
            |kind| {
               match kind {
                  SessionKind::OAuth => oauth_api,
                  SessionKind::Cookie => cookie_api,
               }
            },
            None,
            |_| true,
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
      self
         .has_unlimited_excluding(api, required_kind, &[excluded_id])
         .await
   }

   /// Whether some untried session still has budget for `api`.
   pub(crate) async fn has_unlimited_excluding(
      &self,
      api: &str,
      required_kind: Option<SessionKind>,
      excluded_ids: &[i64],
   ) -> bool {
      let spec = ApiSpec::new(api);
      let allow_pass = super::budget::has_search_pass();
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      self.slots().iter().any(|slot| {
         !excluded_ids.contains(&slot.credentials.id)
            && required_kind.is_none_or(|kind| slot.credentials.kind == kind)
            && !slot.rejected.load(Ordering::Relaxed)
            && !slot.unavailable.load(Ordering::Relaxed)
            && (spec.self_service
               || (slot.search_only.load(Ordering::Relaxed) == spec.is_search
                  && (!spec.is_search
                     || allow_pass
                     || !slot.search_pass_only.load(Ordering::Relaxed))))
            && Self::serves_visitor_slot_for(&slot.read_limits(), spec, allow_pass, now)
      })
   }

   /// Whether this account may take `api`: not rejected, not refused, and in
   /// the pool that owns the call. Search never sees a view account. A view
   /// never sees a search account — except for the calls an account makes
   /// about itself, which are how a search account turns its filters off.
   fn serves_slot(lim: &SessionLimits, spec: ApiSpec<'_>, now: i64) -> bool {
      let right_pool = lim.search_only == spec.is_search || spec.self_service;
      !lim.rejected
         && !lim.is_globally_limited_at(now)
         && !lim.resting_at(spec.api, now)
         && !lim
            .apis
            .get(spec.api)
            .is_some_and(|rate| rate.remaining == 0 && rate.reset > now)
         && !lim.refuses_at_classified(
            spec.op_name,
            spec.is_search,
            spec.self_service,
            spec.profile_op,
            now,
         )
         && right_pool
   }

   fn serves_visitor_slot_for(
      lim: &SessionLimits,
      spec: ApiSpec<'_>,
      allow_pass: bool,
      now: i64,
   ) -> bool {
      Self::serves_slot(lim, spec, now)
         && (!spec.is_search
            || (lim.verified_op(spec.op_name) && (allow_pass || !lim.search_pass_only)))
   }

   fn serves_visitor_slot(lim: &SessionLimits, spec: ApiSpec<'_>, now: i64) -> bool {
      Self::serves_visitor_slot_for(lim, spec, true, now)
   }

   async fn acquire_with_filter<'a>(
      &self,
      api_for: impl Fn(SessionKind) -> &'a str + Send,
      required_kind: Option<SessionKind>,
      accept: impl Fn(i64) -> bool + Send,
   ) -> Result<SessionLease> {
      if self.is_empty() {
         return Err(Error::NoSessions);
      }

      let started = Instant::now();
      let label = api_for(required_kind.unwrap_or(SessionKind::Cookie));
      let prefer_search_pass =
         ApiSpec::new(label).is_search && super::budget::has_search_pass();
      let note = |outcome| self.queue.record(label, started.elapsed(), outcome);
      let mut parked = None;
      // This call's claim on the accounts it listened on, let go after the
      // next scan so the accounts it did not take go to the next in line.
      let mut claim: Option<Wanted> = None;
      let mut parked_scope: Option<SearchScope> = None;
      loop {
         let mut busy = Vec::new();
         let mut next_scope = SearchScope::PublicOnly;
         let sessions = self.slots();
         let pass = if prefer_search_pass {
            let pass_caller = if parked_scope == Some(SearchScope::PassOnly) {
               Caller::Queued
            } else {
               Caller::Fresh
            };
            let mut pass_busy = Vec::new();
            match self.scan(
               &sessions,
               &api_for,
               required_kind,
               &accept,
               pass_caller,
               SearchScope::PassOnly,
               Some(&mut pass_busy),
            ) {
               Ok(lease) => Ok(lease),
               Err(Miss::Busy) if !Self::should_shed(&sessions, started, &pass_busy) => {
                  busy = pass_busy;
                  next_scope = SearchScope::PassOnly;
                  Err(Miss::Busy)
               },
               Err(pass_miss) => {
                  // When every search-pass account is rate-limited, out of
                  // rotation, or congested past the shed threshold, fall back
                  // to the public search pool so paid users keep getting answers.
                  let pub_caller = if parked_scope == Some(SearchScope::PublicOnly) {
                     Caller::Queued
                  } else {
                     Caller::Fresh
                  };
                  let pub_res = self.scan(
                     &sessions,
                     &api_for,
                     required_kind,
                     &accept,
                     pub_caller,
                     SearchScope::PublicOnly,
                     Some(&mut busy),
                  );
                  match (pass_miss, pub_res) {
                     (_, Ok(lease)) => Ok(lease),
                     (Miss::Busy, Err(Miss::Busy | Miss::Spent | Miss::Nobody)) => {
                        if !busy.is_empty() && !Self::should_shed(&sessions, started, &busy) {
                           Err(Miss::Busy)
                        } else {
                           busy.clear();
                           Err(Miss::Busy)
                        }
                     },
                     (Miss::Spent, Err(Miss::Nobody)) => Err(Miss::Spent),
                     (_, other) => other,
                  }
               },
            }
         } else {
            let caller = if claim.is_some() {
               Caller::Queued
            } else {
               Caller::Fresh
            };
            self.scan(
               &sessions,
               &api_for,
               required_kind,
               &accept,
               caller,
               SearchScope::PublicOnly,
               Some(&mut busy),
            )
         };
         drop(claim.take());
         match pass {
            Ok(lease) => {
               note(Outcome::Leased);
               return Ok(lease);
            },
            Err(Miss::Busy)
               if !busy.is_empty() && !Self::should_shed(&sessions, started, &busy) => {},
            Err(Miss::Busy) => {
               note(Outcome::Shed);
               return Err(Error::TransientUpstream);
            },
            Err(miss) => {
               // Which call found the pool empty, and what the accounts that
               // serve it looked like at that instant. Without this the 503
               // says only "rate limited", which is equally consistent with a
               // burst, a wave of parked accounts, and an exhausted window.
               let now = time::OffsetDateTime::now_utc().unix_timestamp();
               let cookie_spec = ApiSpec::new(api_for(SessionKind::Cookie));
               let oauth_spec = ApiSpec::new(api_for(SessionKind::OAuth));
               let refused = sessions
                  .iter()
                  .filter(|slot| {
                     let spec = match slot.credentials.kind {
                        SessionKind::Cookie => cookie_spec,
                        SessionKind::OAuth => oauth_spec,
                     };
                     slot.read_limits().refuses_at_classified(
                        spec.op_name,
                        spec.is_search,
                        spec.self_service,
                        spec.profile_op,
                        now,
                     )
                  })
                  .count();
               let total = sessions.len();
               let reason = format!("{miss:?}");
               let (outcome, err) = miss.settle();
               note(outcome);
               if matches!(err, Error::RateLimited) {
                  tracing::warn!(
                     operation = label,
                     miss = %reason,
                     total,
                     refused,
                     "no account had budget for this call"
                  );
               }
               return Err(err);
            },
         }
         parked.get_or_insert_with(|| Parked::new(&self.waiting));
         let mut waiters = busy
            .iter()
            .map(|&index| Box::pin(sessions[index].freed.notified()))
            .collect::<Vec<_>>();
         // Register before claiming and re-checking permits so a release
         // during the handoff is never missed.
         for waiter in &mut waiters {
            waiter.as_mut().enable();
         }
         parked_scope = Some(next_scope);
         claim = Some(Wanted::claim(Arc::clone(&sessions), busy));
         if claim.as_ref().is_some_and(|claimed| {
            claimed.indices.iter().any(|&index| {
               sessions[index].permits.available_permits() > 0
                  && sessions[index].wanted.load(Ordering::Acquire) == 1
            })
         }) {
            continue;
         }
         let freed = std::future::poll_fn(|cx| {
            if waiters
               .iter_mut()
               .any(|waiter| waiter.as_mut().poll(cx).is_ready())
            {
               std::task::Poll::Ready(())
            } else {
               std::task::Poll::Pending
            }
         });
         let rescan = RESCAN_AFTER.min(MAX_PARK.saturating_sub(started.elapsed()));
         tokio::select! {
            () = freed => {},
            () = sleep(rescan) => {},
         }
      }
   }

   /// Whether a call that found every usable account mid-call should be turned
   /// away instead of parked: it has waited [`MAX_PARK`] already, or more
   /// calls wait on those accounts than their slots clear in about a second.
   /// The line is read from the busiest account, since a parked call listens
   /// on all of them.
   fn should_shed(sessions: &[Arc<SessionSlot>], started: Instant, busy: &[usize]) -> bool {
      if started.elapsed() >= MAX_PARK {
         return true;
      }
      let ahead = busy
         .iter()
         .map(|&index| sessions[index].wanted.load(Ordering::Relaxed))
         .max()
         .unwrap_or(0);
      let slots: usize = busy
         .iter()
         .map(|&index| sessions[index].max_concurrency)
         .sum();
      ahead >= slots.max(1) * QUEUE_PER_SLOT
   }

   /// An account with quota for `api` and a free slot right now, without
   /// queueing behind the calls already holding the rest. `Ok(None)` when
   /// every such account is mid-call: the caller has another route and is
   /// better off taking it than waiting seconds for this one.
   pub(crate) async fn acquire_free(
      &self,
      api: &str,
      required_kind: Option<SessionKind>,
   ) -> Result<Option<SessionLease>> {
      if self.is_empty() {
         return Err(Error::NoSessions);
      }
      let started = Instant::now();
      let sessions = self.slots();
      let prefer_search_pass = ApiSpec::new(api).is_search && super::budget::has_search_pass();
      let pass = if prefer_search_pass {
         match self.scan(
            &sessions,
            &|_| api,
            required_kind,
            &|_| true,
            Caller::Fresh,
            SearchScope::PassOnly,
            None,
         ) {
            Ok(lease) => Ok(lease),
            Err(pass_miss) => {
               let pub_res = self.scan(
                  &sessions,
                  &|_| api,
                  required_kind,
                  &|_| true,
                  Caller::Fresh,
                  SearchScope::PublicOnly,
                  None,
               );
               match (pass_miss, pub_res) {
                  (_, Ok(lease)) => Ok(lease),
                  (Miss::Busy, Err(_)) => Err(Miss::Busy),
                  (Miss::Spent, Err(Miss::Nobody)) => Err(Miss::Spent),
                  (_, other) => other,
               }
            },
         }
      } else {
         self.scan(
            &sessions,
            &|_| api,
            required_kind,
            &|_| true,
            Caller::Fresh,
            SearchScope::PublicOnly,
            None,
         )
      };
      match pass {
         Ok(lease) => {
            self.queue.record(api, started.elapsed(), Outcome::Leased);
            Ok(Some(lease))
         },
         Err(Miss::Busy) => Ok(None),
         Err(miss) => {
            let (outcome, err) = miss.settle();
            self.queue.record(api, started.elapsed(), outcome);
            Err(err)
         },
      }
   }

   /// One pass for a spare account that `accept` allows, taken only if it
   /// has a free slot now. Used for the second copy of a slow call, which is
   /// not worth waiting for.
   ///
   /// Never waits on any slot's limits lock: contended slots are skipped rather
   /// than blocking a hedge behind an unpolled first copy.
   pub(crate) fn try_acquire_where(
      &self,
      api: &str,
      required_kind: Option<SessionKind>,
      accept: impl Fn(i64) -> bool,
   ) -> Option<SessionLease> {
      if self.is_empty() {
         return None;
      }
      let sessions = self.slots();
      let prefer_search_pass = ApiSpec::new(api).is_search && super::budget::has_search_pass();
      if prefer_search_pass {
         if let Ok(lease) = self.scan(
            &sessions,
            &|_| api,
            required_kind,
            &accept,
            Caller::Hedge,
            SearchScope::PassOnly,
            None,
         ) {
            return Some(lease);
         }
      }
      self
         .scan(
            &sessions,
            &|_| api,
            required_kind,
            &accept,
            Caller::Hedge,
            SearchScope::PublicOnly,
            None,
         )
         .ok()
   }

   /// One line per operation that asked for an account since the last call:
   /// how many got one, how long that took, and how many were turned away.
   /// An operation that queued often also gets a line on where its accounts
   /// were: free, mid-call, held for parked calls, resting, spent or out.
   pub(crate) async fn queue_report(&self) -> Vec<String> {
      let report = self.queue.report();
      let mut lines = report.lines;
      if report.crowded.is_empty() {
         return lines;
      }
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      for api in &report.crowded {
         lines.push(self.pool_line(api, now));
      }
      lines
   }

   /// Count one account under where it stands for `api`.
   fn count_slot(
      count: &mut PoolCount,
      slot: &SessionSlot,
      lim: &SessionLimits,
      spec: ApiSpec<'_>,
      now: i64,
   ) {
      let state = if lim.rejected
         || lim.is_globally_limited_at(now)
         || lim.refuses_at_classified(
            spec.op_name,
            spec.is_search,
            spec.self_service,
            spec.profile_op,
            now,
         )
         || (spec.is_search && (!lim.search_only || !lim.verified_op(spec.op_name)))
         || (!spec.is_search && lim.search_only)
      {
         &mut count.out
      } else if lim.resting_at(spec.api, now) {
         &mut count.resting
      } else if lim.headroom_at_classified(
         spec.api,
         spec.op_name,
         spec.is_search,
         spec.self_service,
         spec.profile_op,
         now,
      ) == Headroom::Blocked
      {
         &mut count.spent
      } else if slot.permits.available_permits() == 0 {
         &mut count.busy
      } else if slot.wanted.load(Ordering::Relaxed) > 0 {
         &mut count.held
      } else {
         &mut count.free
      };
      *state += 1;
   }

   /// Effective pool membership for reporting and slot utilization.
   ///
   /// Permanently dead/locked accounts (`rejected` / `retired`) live in the
   /// `rejected` pool and stale-CSRF accounts (`ct0_dead`) live in the
   /// `ct0_dead` pool, so neither pollutes active `search`, `search-pass`, or
   /// `view` slot counts.
   const fn effective_pool(lim: &SessionLimits) -> &'static str {
      if lim.rejected || lim.retired {
         "rejected"
      } else if lim.ct0_dead {
         "ct0_dead"
      } else {
         Self::home_pool(lim)
      }
   }

   const fn home_pool(lim: &SessionLimits) -> &'static str {
      if lim.search_pass_only {
         "search-pass"
      } else if lim.search_only {
         "search"
      } else {
         "view"
      }
   }

   /// Every account's marks, and where each pool stands for the calls that
   /// carry most of its traffic, read in one pass for the dashboard.
   pub async fn census(&self) -> PoolCensus {
      let view_specs = [
         ApiSpec::new(super::endpoints::GRAPH_USER_TWEETS),
         ApiSpec::new(super::endpoints::GRAPH_USER),
         ApiSpec::new(super::endpoints::GRAPH_TWEET_DETAIL),
      ];
      let search_spec = ApiSpec::new(super::endpoints::GRAPH_SEARCH_TIMELINE);
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      let slots = self.slots();
      let mut census = PoolCensus {
         total: slots.len(),
         parked_calls: self.waiting.load(Ordering::Relaxed),
         ..PoolCensus::default()
      };
      let mut view_counts = [
         PoolCount::default(),
         PoolCount::default(),
         PoolCount::default(),
      ];
      let mut search_count = PoolCount::default();
      let mut search_pass_count = PoolCount::default();
      for slot in slots.iter() {
         let lim = slot.read_limits();
         let pool = Self::effective_pool(&lim);
         match pool {
            "search" => {
               census.search += 1;
               Self::count_slot(&mut search_count, slot, &lim, search_spec, now);
            },
            "search-pass" => {
               census.search_pass += 1;
               Self::count_slot(&mut search_pass_count, slot, &lim, search_spec, now);
            },
            "view" => {
               census.view += 1;
               for (index, &spec) in view_specs.iter().enumerate() {
                  Self::count_slot(&mut view_counts[index], slot, &lim, spec, now);
               }
            },
            "ct0_dead" => census.ct0_dead_pool += 1,
            _ => census.rejected_pool += 1,
         }
         census.demoted_from_search += usize::from(pool == "view" && lim.demoted_from_search);
         census.out_of_rotation += usize::from(pool == "rejected" || pool == "ct0_dead");
         census.credentials_rejected += usize::from(lim.rejected);
         census.ct0_dead += usize::from(lim.ct0_dead);
         census.retired += usize::from(lim.retired);
         census.suspended += usize::from(lim.suspended);
         census.search_banned += usize::from(lim.search_banned);
         census.profile_banned += usize::from(lim.profile_banned);
         census.settings_refused += usize::from(lim.filters_failed);
         census.filters_cleared += usize::from(lim.filters_cleared);
         census.globally_limited += usize::from(lim.is_globally_limited_at(now));
      }
      let make_row = |operation: &str, pool: &'static str, accounts: usize, count: PoolCount| {
         OperationPool {
            operation: operation.to_owned(),
            pool,
            accounts,
            free: count.free,
            busy: count.busy,
            held: count.held,
            resting: count.resting,
            spent: count.spent,
            out: count.out,
         }
      };
      let [user_tweets, user_by_screen_name, tweet_detail] = view_counts;
      census.operations = vec![
         make_row("UserTweets", "view", census.view, user_tweets),
         make_row(
            "UserByScreenName",
            "view",
            census.view,
            user_by_screen_name,
         ),
         make_row("TweetDetail", "view", census.view, tweet_detail),
         make_row("SearchTimeline", "search", census.search, search_count),
         make_row(
            "SearchTimeline",
            "search-pass",
            census.search_pass,
            search_pass_count,
         ),
      ];
      census
   }

   /// Explain why a slot in the operation's owning pool is counted as `out` by
   /// [`Self::count_slot`].
   fn out_reasons_for_spec(lim: &SessionLimits, spec: ApiSpec<'_>, now: i64) -> OutReasons {
      let mut reasons = OutReasons {
         ct0_dead:             lim.ct0_dead,
         credentials_rejected: lim.rejected,
         retired:              lim.retired,
         unverified_search:    spec.is_search && !lim.verified_op(spec.op_name),
         search_banned:        spec.is_search && lim.search_banned,
         search_404_cooldown:  spec.is_search && lim.search_404_cooling_at(now),
         profile_banned:       spec.profile_op && lim.profile_banned,
         globally_limited:     lim.is_globally_limited_at(now),
         refused_operations:   Vec::new(),
      };
      if lim.op_refused_at(spec.op_name, spec.is_search, now) {
         reasons.refused_operations.push(spec.op_name.to_owned());
      }
      reasons
   }

   /// Paginated live view of every account counted as "Out / unverified" in
   /// the pool census (or demoted from search into the view pool).
   #[expect(
      clippy::too_many_arguments,
      reason = "direct filter parameters from the admin query struct"
   )]
   pub fn out_accounts_snapshot(
      &self,
      op: Option<&str>,
      pool: Option<&str>,
      reason: Option<&str>,
      query: Option<&str>,
      sort: &str,
      page: usize,
      page_size: usize,
   ) -> OutAccountsSnapshot {
      let user_tweets = ApiSpec::new(super::endpoints::GRAPH_USER_TWEETS);
      let user_by_screen_name = ApiSpec::new(super::endpoints::GRAPH_USER);
      let tweet_detail = ApiSpec::new(super::endpoints::GRAPH_TWEET_DETAIL);
      let search_timeline = ApiSpec::new(super::endpoints::GRAPH_SEARCH_TIMELINE);

      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      let slots = self.slots();
      let mut counts = OutOperationCounts::default();
      let mut breakdown = OutBreakdown::default();
      let mut filtered: Vec<OutAccountEntry> = Vec::new();

      let trimmed_query = query.map(str::trim).filter(|val| !val.is_empty());
      let query_id = trimmed_query.and_then(|val| val.trim_start_matches('#').parse::<i64>().ok());
      let query_name = trimmed_query.map(|val| val.trim_start_matches('@').to_ascii_lowercase());

      for slot in slots.iter() {
         let lim = slot.read_limits();
         let slot_pool = Self::effective_pool(&lim);
         let home_pool = Self::home_pool(&lim);

         let scoped_reasons = match slot_pool {
            "rejected" => {
               counts.rejected_pool += 1;
               let reasons = OutReasons {
                  ct0_dead:             lim.ct0_dead,
                  credentials_rejected: lim.rejected,
                  retired:              lim.retired,
                  unverified_search:    lim.search_only && !lim.verified_op("SearchTimeline"),
                  search_banned:        lim.search_banned,
                  search_404_cooldown:  lim.search_404_cooling_at(now),
                  profile_banned:       lim.profile_banned,
                  globally_limited:     lim.is_globally_limited_at(now),
                  refused_operations:   Vec::new(),
               };
               let matches_scope = op.is_none()
                  && (pool == Some("rejected")
                     || (pool.is_none()
                        && matches!(reason, Some("credentials_rejected" | "retired"))));
               matches_scope.then_some(reasons)
            },
            "ct0_dead" => {
               counts.ct0_dead_pool += 1;
               let reasons = OutReasons {
                  ct0_dead:             true,
                  credentials_rejected: false,
                  retired:              false,
                  unverified_search:    lim.search_only && !lim.verified_op("SearchTimeline"),
                  search_banned:        lim.search_only && lim.search_banned,
                  search_404_cooldown:  lim.search_only && lim.search_404_cooling_at(now),
                  profile_banned:       !lim.search_only && lim.profile_banned,
                  globally_limited:     lim.is_globally_limited_at(now),
                  refused_operations:   Vec::new(),
               };
               let matches_scope = op.is_none()
                  && (pool == Some("ct0_dead")
                     || (pool.is_none() && reason == Some("ct0_dead")));
               matches_scope.then_some(reasons)
            },
            "search" => {
               let sr = Self::out_reasons_for_spec(&lim, search_timeline, now);
               if sr.any() {
                  counts.search_timeline += 1;
                  counts.all_out += 1;
               }
               match op {
                  Some("SearchTimeline") => {
                     matches!(pool, None | Some("search"))
                        .then_some(sr)
                        .filter(OutReasons::any)
                  },
                  Some(_) => None,
                  None => {
                     match pool {
                        Some("search") | None => sr.any().then_some(sr),
                        _ => None,
                     }
                  },
               }
            },
            "search-pass" => {
               let sr = Self::out_reasons_for_spec(&lim, search_timeline, now);
               if sr.any() {
                  counts.search_timeline += 1;
                  counts.search_pass += 1;
                  counts.all_out += 1;
               }
               match op {
                  Some("SearchTimeline") => {
                     matches!(pool, None | Some("search-pass"))
                        .then_some(sr)
                        .filter(OutReasons::any)
                  },
                  Some(_) => None,
                  None => {
                     match pool {
                        Some("search-pass") | None => sr.any().then_some(sr),
                        _ => None,
                     }
                  },
               }
            },
            _ => {
               if lim.demoted_from_search {
                  counts.demoted_from_search += 1;
               }
               let ut = Self::out_reasons_for_spec(&lim, user_tweets, now);
               let ub = Self::out_reasons_for_spec(&lim, user_by_screen_name, now);
               let td = Self::out_reasons_for_spec(&lim, tweet_detail, now);
               if ut.any() {
                  counts.user_tweets += 1;
               }
               if ub.any() {
                  counts.user_by_screen_name += 1;
               }
               if td.any() {
                  counts.tweet_detail += 1;
               }
               let mut combined = ut.clone();
               combined.merge(ub.clone());
               combined.merge(td.clone());
               if combined.any() {
                  counts.all_out += 1;
               }
               match op {
                  Some("UserTweets") => ut.any().then_some(ut),
                  Some("UserByScreenName") => ub.any().then_some(ub),
                  Some("TweetDetail") => td.any().then_some(td),
                  Some(_) => None,
                  None => {
                     match pool {
                        Some("demoted") => lim.demoted_from_search.then_some(combined),
                        Some("view") | None => combined.any().then_some(combined),
                        _ => None,
                     }
                  },
               }
            },
         };

         let Some(reasons) = scoped_reasons else {
            continue;
         };

         if let Some(ref needle) = query_name {
            let id_match = query_id == Some(slot.credentials.id);
            let user_match = slot
               .credentials
               .username
               .to_ascii_lowercase()
               .contains(needle);
            if !id_match && !user_match {
               continue;
            }
         }

         breakdown.total += 1;
         breakdown.ct0_dead += usize::from(reasons.ct0_dead);
         breakdown.credentials_rejected += usize::from(reasons.credentials_rejected);
         breakdown.unverified_search += usize::from(reasons.unverified_search);
         breakdown.retired += usize::from(reasons.retired);
         breakdown.suspended += usize::from(lim.suspended);
         breakdown.search_banned += usize::from(reasons.search_banned);
         breakdown.profile_banned += usize::from(reasons.profile_banned);
         breakdown.refused +=
            usize::from(!reasons.refused_operations.is_empty() || reasons.search_404_cooldown);
         breakdown.globally_limited += usize::from(reasons.globally_limited);

         if let Some(selected_reason) = reason
            && !reasons.matches_filter(selected_reason, lim.suspended)
         {
            continue;
         }

         let ct0 = slot.ct0();
         let last_active = slot.last_active.load(Ordering::Relaxed);
         filtered.push(OutAccountEntry {
            id: slot.credentials.id,
            username: slot.credentials.username.clone(),
            kind: slot.credentials.kind,
            pool: slot_pool,
            home_pool,
            demoted_from_search: lim.demoted_from_search,
            has_auth_token: !slot.credentials.auth_token.is_empty(),
            ct0_present: !ct0.is_empty(),
            ct0_len: ct0.len(),
            suspended: lim.suspended,
            filters_cleared: lim.filters_cleared,
            filters_failed: lim.filters_failed,
            verified_search: lim.verified_op("SearchTimeline"),
            last_active: (last_active > 0).then_some(last_active as i64),
            search_empty_streak: slot.search_empty_streak.load(Ordering::Relaxed),
            profile_strikes: lim.profile_strikes,
            reasons,
         });
      }

      match sort {
         "id_desc" => filtered.sort_unstable_by(|a, b| b.id.cmp(&a.id)),
         "id_asc" => filtered.sort_unstable_by(|a, b| a.id.cmp(&b.id)),
         "last_active" => {
            filtered.sort_unstable_by(|a, b| {
               b.last_active
                  .unwrap_or(0)
                  .cmp(&a.last_active.unwrap_or(0))
                  .then_with(|| a.id.cmp(&b.id))
            });
         },
         _ => {
            filtered.sort_unstable_by(|a, b| {
               a.reasons
                  .priority_rank(a.suspended)
                  .cmp(&b.reasons.priority_rank(b.suspended))
                  .then_with(|| a.id.cmp(&b.id))
            });
         },
      }

      let page_size = page_size.clamp(1, 200);
      let total_filtered = filtered.len();
      let total_pages = total_filtered.div_ceil(page_size).max(1);
      let page = page.clamp(1, total_pages);
      let start = (page - 1).saturating_mul(page_size);
      let accounts = filtered.into_iter().skip(start).take(page_size).collect();

      OutAccountsSnapshot {
         counts,
         breakdown,
         total_filtered,
         page,
         total_pages,
         page_size,
         accounts,
         now,
      }
   }

   fn pool_line(&self, api: &str, now: i64) -> String {
      let spec = ApiSpec::new(api);
      let mut count = PoolCount::default();
      for slot in self.slots().iter() {
         let lim = slot.read_limits();
         if lim.search_only == spec.is_search {
            Self::count_slot(&mut count, slot, &lim, spec, now);
         }
      }
      format!(
         "{} accounts: {} free, {} mid-call, {} held for parked calls, {} resting after a bare \
          429, {} spent, {} out; {} calls parked",
         spec.op_name,
         count.free,
         count.busy,
         count.held,
         count.resting,
         count.spent,
         count.out,
         self.waiting.load(Ordering::Relaxed),
      )
   }

   /// Flag the accounts pinned to an exit that trails the fleet, so they are
   /// picked after the others. Returns how many are flagged.
   pub(crate) fn mark_slow_exits(&self, slow: impl Fn(i64) -> bool) -> usize {
      self
         .slots()
         .iter()
         .map(|slot| {
            let flagged = slow(slot.credentials.id);
            slot.slow_exit.store(flagged, Ordering::Relaxed);
            usize::from(flagged)
         })
         .sum()
   }

   /// The account with the most calls left, starting from the rotation point.
   /// A known window always beats an account we have never measured: that
   /// unknown account is how a 429 still gets through.
   ///
   /// An account a parked call listens on counts as mid-call for anyone but
   /// a parked call. `busy` collects the accounts that could serve this call
   /// once they free up, for a caller that is about to park.
   ///
   /// Scans read-only across per-slot locks and keeps the top [`SCAN_TOP_K`]
   /// candidates so concurrent callers can claim different accounts in
   /// parallel without a global write lock.
   #[expect(
      clippy::too_many_arguments,
      reason = "internal pool scan passes pre-acquired slice, filter, caller mode, and search scope"
   )]
   fn scan<'a>(
      &self,
      sessions: &[Arc<SessionSlot>],
      api_for: &impl Fn(SessionKind) -> &'a str,
      required_kind: Option<SessionKind>,
      accept: &impl Fn(i64) -> bool,
      caller: Caller,
      search_scope: SearchScope,
      mut busy_out: Option<&mut Vec<usize>>,
   ) -> StdResult<SessionLease, Miss> {
      let prefer_warm = caller == Caller::Hedge;
      let count = sessions.len();
      if count == 0 {
         return Err(Miss::Nobody);
      }
      let cookie_spec = ApiSpec::new(api_for(SessionKind::Cookie));
      let oauth_spec = ApiSpec::new(api_for(SessionKind::OAuth));
      let mut best: Vec<Candidate> = Vec::with_capacity(SCAN_TOP_K);
      let mut fallback: Vec<(u8, usize)> = Vec::with_capacity(SCAN_TOP_K);
      let max_attempts = if caller == Caller::Hedge {
         1
      } else {
         SCAN_CLAIM_RETRIES
      };
      let stride = if count > SCAN_PROBES { SCAN_STRIDE } else { 1 };

      for attempt in 0..max_attempts {
         let start = self
            .cursor
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(stride)
            % count;
         let now = time::OffsetDateTime::now_utc().unix_timestamp();
         let now_u64 = u64::try_from(now).unwrap_or(0);
         let allow_early_stop = caller == Caller::Hedge || attempt + 1 < max_attempts;
         best.clear();
         fallback.clear();
         if let Some(out) = busy_out.as_deref_mut() {
            out.clear();
         }
         let mut any = false;
         let mut busy = false;
         let mut probed = 0_usize;

         for offset in 0..count {
            let index = start.wrapping_add(offset) % count;
            let slot = &sessions[index];
            if !accept(slot.credentials.id)
               || required_kind.is_some_and(|kind| slot.credentials.kind != kind)
            {
               continue;
            }
            if slot.rejected.load(Ordering::Relaxed) {
               continue;
            }
            let spec = match slot.credentials.kind {
               SessionKind::Cookie => cookie_spec,
               SessionKind::OAuth => oauth_spec,
            };
            let is_search_account = slot.search_only.load(Ordering::Relaxed);
            let is_pass_account = slot.search_pass_only.load(Ordering::Relaxed);
            if spec.is_search {
               if !search_scope.matches_flags(is_search_account, is_pass_account) {
                  continue;
               }
            } else if is_search_account {
               continue;
            }
            // Permanently dead/retired view accounts are always `Blocked`
            // after `any = true`, so skip them without locking `limits`.
            if !spec.is_search && slot.unavailable.load(Ordering::Relaxed) {
               any = true;
               continue;
            }
            let slot_busy = slot.permits.available_permits() == 0
               || (caller != Caller::Queued && slot.wanted.load(Ordering::Relaxed) > 0);
            if slot_busy && (!best.is_empty() || !fallback.is_empty()) {
               continue;
            }

            let lim = if caller == Caller::Hedge {
               let Some(guard) = slot.try_read_limits() else {
                  continue;
               };
               guard
            } else {
               slot.read_limits()
            };
            if lim.rejected {
               continue;
            }
            // Only the background audit may try unchecked search accounts, and
            // public searches never see search-pass accounts.
            if spec.is_search
               && (!search_scope.matches_flags(lim.search_only, lim.search_pass_only)
                  || !lim.verified_op(spec.op_name))
            {
               continue;
            }
            // A search account is not a candidate for anything else. `refuses`
            // already blocks the spend; skipping here keeps a profile lookup
            // from parking on an account it will never be given.
            if !spec.is_search && lim.search_only {
               continue;
            }
            any = true;
            let headroom = lim.headroom_at_classified(
               spec.api,
               spec.op_name,
               spec.is_search,
               spec.self_service,
               spec.profile_op,
               now,
            );
            if headroom == Headroom::Blocked {
               continue;
            }
            if slot_busy {
               busy = true;
               if let Some(out) = busy_out.as_deref_mut()
                  && out.len() < WAIT_ON_ACCOUNTS
               {
                  out.push(index);
               }
               continue;
            }

            probed += 1;
            let slow_exit = slot.slow_exit.load(Ordering::Relaxed);
            let slot_idle = slot.permits.available_permits() == slot.max_concurrency;
            match headroom {
               Headroom::Blocked => {},
               Headroom::Safe(remaining) => {
                  let remaining = if slow_exit {
                     remaining / SLOW_EXIT_DISCOUNT
                  } else {
                     remaining
                  };
                  let last_active = slot.last_active.load(Ordering::Relaxed);
                  let warm =
                     last_active > 0 && now_u64.saturating_sub(last_active) < WARM_CONNECTION_SECS;
                  // Skip `verified_op` (a HashMap probe) whenever even a
                  // verified copy of this slot cannot beat the worst entry
                  // already in `best`.
                  let optimistic = Candidate {
                     index,
                     remaining,
                     verified: true,
                     warm,
                     slow_exit,
                     idle: slot_idle,
                  };
                  if best.len() < SCAN_TOP_K
                     || better_candidate(
                        optimistic,
                        best[SCAN_TOP_K - 1],
                        prefer_warm,
                        spec.is_search,
                     )
                  {
                     let verified = spec.is_search || lim.verified_op(spec.op_name);
                     let candidate = Candidate {
                        index,
                        remaining,
                        verified,
                        warm,
                        slow_exit,
                        idle: slot_idle,
                     };
                     if best.len() < SCAN_TOP_K
                        || better_candidate(
                           candidate,
                           best[SCAN_TOP_K - 1],
                           prefer_warm,
                           spec.is_search,
                        )
                     {
                        insert_best(&mut best, candidate, prefer_warm, spec.is_search);
                     }
                  }
               },
               Headroom::Unknown if spec.is_search => {
                  // A verified search account whose 15-minute window has reset
                  // (or has not been observed yet) has a full window ready.
                  // Rank it in `best` instead of `fallback` so search load
                  // spreads across the whole search pool rather than hammering
                  // a handful of already-active accounts.
                  let raw_remaining = lim
                     .apis
                     .get(spec.api)
                     .filter(|rate| rate.limit > 0)
                     .map_or(SEARCH_DEFAULT_LIMIT, |rate| rate.limit);
                  let remaining = if slow_exit {
                     raw_remaining / SLOW_EXIT_DISCOUNT
                  } else {
                     raw_remaining
                  };
                  let last_active = slot.last_active.load(Ordering::Relaxed);
                  let warm =
                     last_active > 0 && now_u64.saturating_sub(last_active) < WARM_CONNECTION_SECS;
                  let candidate = Candidate {
                     index,
                     remaining,
                     verified: true,
                     warm,
                     slow_exit,
                     idle: slot_idle,
                  };
                  if best.len() < SCAN_TOP_K
                     || better_candidate(candidate, best[SCAN_TOP_K - 1], prefer_warm, true)
                  {
                     insert_best(&mut best, candidate, prefer_warm, true);
                  }
               },
               Headroom::Unknown => {
                  let verified = lim.verified_op(spec.op_name);
                  let last_active = slot.last_active.load(Ordering::Relaxed);
                  let warm =
                     last_active > 0 && now_u64.saturating_sub(last_active) < WARM_CONNECTION_SECS;
                  // Verified before unverified, then a quick exit, then a warm
                  // connection: a cold handshake on a quick exit still beats the
                  // round trip a slow one adds to every call.
                  let rank =
                     (u8::from(!verified) << 2) | (u8::from(slow_exit) << 1) | u8::from(!warm);
                  if fallback.len() < SCAN_TOP_K || rank < fallback[SCAN_TOP_K - 1].0 {
                     insert_fallback(&mut fallback, rank, index);
                  }
               },
            }

            if allow_early_stop && probed >= SCAN_PROBES && can_stop_early(&best) {
               break;
            }
         }

         if best.is_empty() && fallback.is_empty() {
            return Err(if !any {
               Miss::Nobody
            } else if busy {
               Miss::Busy
            } else {
               Miss::Spent
            });
         }

         for index in best
            .iter()
            .map(|chosen| chosen.index)
            .chain(fallback.iter().map(|&(_, index)| index))
         {
            let slot = &sessions[index];
            if caller != Caller::Queued && slot.wanted.load(Ordering::Acquire) > 0 {
               busy = true;
               if let Some(out) = busy_out.as_deref_mut()
                  && out.len() < WAIT_ON_ACCOUNTS
                  && !out.contains(&index)
               {
                  out.push(index);
               }
               continue;
            }
            let Ok(permit) = Arc::clone(&slot.permits).try_acquire_owned() else {
               busy = true;
               if let Some(out) = busy_out.as_deref_mut()
                  && out.len() < WAIT_ON_ACCOUNTS
                  && !out.contains(&index)
               {
                  out.push(index);
               }
               continue;
            };
            if caller != Caller::Queued && slot.wanted.load(Ordering::Acquire) > 0 {
               drop(permit);
               slot.freed.notify_one();
               busy = true;
               if let Some(out) = busy_out.as_deref_mut()
                  && out.len() < WAIT_ON_ACCOUNTS
                  && !out.contains(&index)
               {
                  out.push(index);
               }
               continue;
            }
            let spec = match slot.credentials.kind {
               SessionKind::Cookie => cookie_spec,
               SessionKind::OAuth => oauth_spec,
            };
            let claim = |lim: &mut SessionLimits| {
               if spec.is_search
                  && (!search_scope.matches_flags(lim.search_only, lim.search_pass_only)
                     || !lim.verified_op(spec.op_name))
               {
                  return Spend::Denied;
               }
               if !spec.is_search && lim.search_only {
                  return Spend::Denied;
               }
               lim.try_spend(spec.api)
            };
            let spent = if caller == Caller::Hedge {
               let Some(spent) = slot.try_write_limits(claim) else {
                  drop(permit);
                  slot.freed.notify_one();
                  continue;
               };
               spent
            } else {
               slot.write_limits(claim)
            };
            if spent == Spend::Denied {
               drop(permit);
               slot.freed.notify_one();
               continue;
            }
            return Ok(Self::lease(slot, permit, spent == Spend::Metered));
         }

         if attempt + 1 == max_attempts {
            return Err(if !any {
               Miss::Nobody
            } else if busy {
               Miss::Busy
            } else {
               Miss::Spent
            });
         }
      }

      Err(Miss::Busy)
   }

   fn lease(slot: &SessionSlot, permit: OwnedSemaphorePermit, metered: bool) -> SessionLease {
      SessionLease {
         credentials: Arc::clone(&slot.credentials),
         permit:      Some(permit),
         metered:     AtomicBool::new(metered),
         freed:       Arc::clone(&slot.freed),
      }
   }

   /// How many sessions can still call `api`.
   pub(crate) async fn available_for(&self, api: &str) -> usize {
      let spec = ApiSpec::new(api);
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      self
         .slots()
         .iter()
         .filter(|slot| {
            !slot.rejected.load(Ordering::Relaxed)
               && !slot.unavailable.load(Ordering::Relaxed)
               && (spec.self_service || slot.search_only.load(Ordering::Relaxed) == spec.is_search)
               && Self::serves_visitor_slot(&slot.read_limits(), spec, now)
         })
         .count()
   }

   /// Return a local spend when the call failed before X counted it.
   pub(crate) async fn refund_if_metered(&self, session: &SessionLease, api: &str) {
      if !session.metered.swap(false, Ordering::Relaxed) {
         return;
      }
      if let Some(slot) = self.slot(session.id) {
         slot.write_limits(|lim| lim.refund_spend(api));
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
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      let Some(slot) = self.slot(session_id) else {
         return;
      };
      slot
         .last_active
         .store(u64::try_from(now).unwrap_or(0), Ordering::Relaxed);
      slot.write_limits(|lim| {
         // Rate-limit headers also arrive on HTTP 200/403 code-326 responses,
         // so seeing a window header must never clear `lim.rejected`; only an
         // explicit operator `reinstate` lifts a credentials rejection.
         // The session responded, so clear its expired global limit
         if lim.limited && !lim.is_limited(api) {
            lim.limited = false;
         }
         // A local spend already lowered `remaining`. A late header must not
         // raise it back over calls that are still in flight, and a 429 mark
         // of zero stays zero until its reset.
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
      });

      if remaining <= 0 && reset > now {
         self.persist_soon(session_id);
      } else {
         self.mark_dirty(session_id);
      }
   }

   /// Mark one API as rate limited for a session, leaving its other endpoints
   /// usable. Each endpoint has its own budget, so a 429 on one says nothing
   /// about the rest.
   pub async fn mark_endpoint_limited(&self, session_id: i64, api: &str) {
      self.record_restriction(
         session_id,
         crate::stats::Restriction::RateLimited,
         api,
         "Upstream rate-limit budget reached; other endpoints remain independent",
      );
      let Some(slot) = self.slot(session_id) else {
         self.persist_soon(session_id);
         return;
      };
      let damage = slot.write_limits(|lim| lim.note_rate_limit(api));
      match damage {
         crate::types::Damage::Search => {
            tracing::info!(
               session_id,
               session_user = %slot.credentials.username,
               "account hit a search limit; leaving it out of search"
            )
         },
         crate::types::Damage::Profile => {
            tracing::info!(
               session_id,
               session_user = %slot.credentials.username,
               "account hit a second profile limit; leaving it out of profile pages"
            )
         },
         crate::types::Damage::Window => {},
      }
      self.persist_soon(session_id);
   }

   /// X refused this account `api` with a bare 429: no window, no body. Most
   /// clear within a couple of minutes, so it sits out only that long, longer
   /// each time X refuses it again straight after, with no strike and its
   /// real window untouched. Returns the rest in seconds.
   pub async fn rest_endpoint(&self, session_id: i64, api: &str) -> Option<i64> {
      self.queue.note_rest(api);
      self.record_restriction(
         session_id,
         crate::stats::Restriction::RateLimited,
         api,
         "Temporary cooldown after an upstream 429 without a rate-limit window",
      );
      let slot = self.slot(session_id)?;
      Some(slot.write_limits(|lim| lim.rest(api)))
   }

   /// Record that a session got results for `api`, which moves it ahead of
   /// unchecked accounts for that call.
   pub async fn mark_verified(&self, session_id: i64, api: &str) {
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      let Some(slot) = self.slot(session_id) else {
         return;
      };
      slot
         .last_active
         .store(u64::try_from(now).unwrap_or(0), Ordering::Relaxed);
      if slot.read_limits().verified(api) {
         return;
      }
      let verified = slot.write_limits(|lim| {
         if operation_name(api) == "SearchTimeline" && lim.refuses(api) {
            // A response from a lease issued before quarantine cannot undo it.
            return false;
         }
         lim.verify(api);
         if operation_name(api) == "SearchTimeline" {
            lim.reserve_for_search();
         }
         true
      });
      if verified {
         self.mark_dirty(session_id);
      }
   }

   /// Cookie accounts that still need a successful search for visitor
   /// admission.
   pub(super) async fn search_audit_ids(&self, api: &str) -> Vec<i64> {
      let spec = ApiSpec::new(api);
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      self
         .slots()
         .iter()
         .filter(|slot| {
            slot.credentials.kind == SessionKind::Cookie
               && !slot.rejected.load(Ordering::Relaxed)
               && !slot.unavailable.load(Ordering::Relaxed)
               && (spec.self_service || slot.search_only.load(Ordering::Relaxed) == spec.is_search)
         })
         .filter(|slot| {
            let lim = slot.read_limits();
            Self::serves_slot(&lim, spec, now) && !lim.verified_op(spec.op_name)
         })
         .map(|slot| slot.credentials.id)
         .collect()
   }

   /// Cookie sessions reserved for search that have not yet returned a result
   /// or been refused.
   #[cfg(test)]
   pub async fn unchecked_for(&self, api: &str) -> Vec<i64> {
      self
         .slots()
         .iter()
         .filter(|slot| slot.credentials.kind == SessionKind::Cookie)
         .filter(|slot| {
            let lim = slot.read_limits();
            !lim.rejected && lim.unchecked(api)
         })
         .map(|slot| slot.credentials.id)
         .collect()
   }

   /// Leave a session out of one operation X answers it nothing for. A search
   /// refusal rests the account for three minutes: an empty page is also what
   /// a query with no matches and an age-gated or filtered account produce,
   /// so it is not a ban. The account stays in the search pool so it is not
   /// mixed back into profile reads. Other refusals expire after a day.
   pub async fn mark_refused(&self, session_id: i64, api: &str) {
      let search = api.ends_with("SearchTimeline");
      self.record_restriction(
         session_id,
         if search {
            crate::stats::Restriction::SearchRested
         } else {
            crate::stats::Restriction::OperationRefused
         },
         api,
         if search {
            "Search returned no usable result; temporary cooldown"
         } else {
            "Upstream refused this operation; temporary cooldown"
         },
      );
      let slot = self.slot(session_id);
      if let Some(slot) = &slot {
         slot.write_limits(|lim| lim.refuse(api));
      }
      if api.ends_with("SearchTimeline") {
         let username = slot
            .as_ref()
            .map(|slot| slot.credentials.username.as_str())
            .unwrap_or_default();
         // Rested, not banned. An empty page is also a query with no matches,
         // and a count of those banned accounts that still search. Only the
         // settings probe, which a suspended account refuses, takes it out.
         tracing::debug!(
            session_id,
            session_user = %username,
            "account answered no search while another did; resting it"
         );
         log_events::note(Event::SearchRested);
      }
      self.persist_soon(session_id);
   }

   /// Record that X refused a call for this account, with what that refusal
   /// actually tells us.
   ///
   /// * **`SuspensionProbe::Data`** — a real call the pool handed out. X said
   ///   this account may not act, on the path the caller asked for. That is a
   ///   verdict about the account: it cannot search.
   /// * **`SuspensionProbe::Settings`** — the `account/settings` write. X
   ///   answers this one with 403 code 64 for accounts that search perfectly
   ///   well: the settings endpoint is gated separately, and 10 out of 10
   ///   accounts flagged this way answered a real `SearchTimeline` call with
   ///   200 and posts. So this probe proves only that the *settings write* is
   ///   refused. It is paced and the account keeps its pool.
   ///
   /// `restriction` is recorded in the census, or `None` when the caller
   /// already recorded it from the response it read.
   pub(super) async fn note_suspended(
      &self,
      session_id: i64,
      probe: SuspensionProbe,
      restriction: Option<crate::stats::Restriction>,
      evidence: &'static str,
   ) {
      if probe == SuspensionProbe::Settings {
         // The settings write was refused. Nothing about search is known, so
         // the account is paced — never benched, never moved.
         self
            .mark_filters_failed(session_id, evidence, crate::api::filters::REFUSAL_MEMORY)
            .await;
         return;
      }
      let Some(slot) = self.slot(session_id) else {
         return;
      };
      let Some((retire, in_search)) = slot.write_limits(|lim| {
         if lim.retired {
            return None;
         }
         let already_suspended = lim.suspended;
         let in_search = lim.search_only;
         let was_search_pass = lim.search_pass_only;
         // A view account refused on a real call has nothing left: it cannot
         // search, and the pool kept for it does not work either.
         let retire = already_suspended && !in_search;
         if retire {
            lim.retired = true;
         }
         if in_search {
            lim.demoted_from_search = true;
            if was_search_pass {
               lim.demoted_from_search_pass = true;
            }
         }
         lim.suspended = true;
         lim.search_banned = true;
         lim.search_only = false;
         lim.search_pass_only = false;
         // The settings write is never spent on this account again: the refusal
         // it would report is the one that was just recorded.
         lim.filters_failed = true;
         lim.filters_retry_at = i64::MAX;
         lim.refuse("SearchTimeline");
         Some((retire, in_search))
      }) else {
         return;
      };
      self.persist_soon(session_id);
      if let Some(restriction) = restriction {
         self.record_restriction(session_id, restriction, "account", evidence);
      }
      if retire {
         tracing::info!(
            session_id,
            "the view pool was refused too; retiring the account for good"
         );
         return;
      }
      if !in_search {
         // Already in the view pool: it is where it needs to be, and it stays
         // available for profile reads.
         return;
      }
      // A search account may still read profiles, so keep it for that.
      self.move_to_view_pool(session_id, evidence).await;
   }

   /// Clear a suspension verdict a probe has just contradicted.
   ///
   /// Only ever called from an operator's manual probe: the pool itself never
   /// re-asks, because re-asking across thousands of accounts is what starved
   /// search. When X answers for an account it had refused, the stored verdict
   /// was wrong and the account goes back to the pool its import named.
   ///
   /// Returns whether there was anything to clear. An account with no verdict
   /// against it, already in the pool it belongs to, is left alone and `false`
   /// comes back, so the caller can say "nothing to do" instead of "cleared".
   pub async fn reinstate(&self, session_id: i64) -> bool {
      let Some(slot) = self.slot(session_id) else {
         return false;
      };
      let store_pool = match &self.account_store {
         Some(store) => store.origin_pool(session_id).await,
         None => AccountPool::View,
      };
      let Some(target_pool) = slot.write_limits(|lim| {
         let target_pool = if store_pool == AccountPool::SearchPass
            || lim.search_pass_only
            || lim.demoted_from_search_pass
         {
            AccountPool::SearchPass
         } else if store_pool == AccountPool::Search || lim.demoted_from_search || lim.search_only {
            AccountPool::Search
         } else {
            AccountPool::View
         };
         let want_search = target_pool.is_search();
         let want_search_pass = target_pool == AccountPool::SearchPass;
         let anything_to_clear = lim.suspended
            || lim.retired
            || lim.rejected
            || lim.ct0_dead
            || lim.search_banned
            || lim.profile_banned
            || lim.profile_strikes > 0
            || lim.limited
            || lim.demoted_from_search
            || lim.demoted_from_search_pass
            || lim.search_only != want_search
            || lim.search_pass_only != want_search_pass
            || lim.filters_failed
            || lim.search_404_at.is_some()
            || !lim.refused_ops.is_empty();
         if !anything_to_clear {
            return None;
         }
         lim.suspended = false;
         lim.retired = false;
         lim.rejected = false;
         lim.ct0_dead = false;
         lim.search_banned = false;
         lim.profile_banned = false;
         lim.profile_strikes = 0;
         lim.limited = false;
         lim.limited_at = 0;
         lim.demoted_from_search = false;
         lim.demoted_from_search_pass = false;
         lim.search_only = want_search;
         lim.search_pass_only = want_search_pass;
         lim.filters_failed = false;
         lim.filters_retry_at = 0;
         lim.refused_ops.clear();
         lim.search_404_at = None;
         lim.resting.clear();
         Some(target_pool)
      }) else {
         return false;
      };
      slot.has_search_failures.store(false, Ordering::Release);
      *slot
         .search_failures
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner) = SearchFailureState::default();
      self.persist_soon(session_id);
      if target_pool.is_search() {
         self.writes.cancel_demotion(session_id);
      }
      // The store is what a restart reads, so it has to agree with the slot.
      if let Some(store) = &self.account_store
         && target_pool.is_search()
      {
         if let Err(err) = store.set_pool(session_id, target_pool).await {
            tracing::warn!(session_id, "could not restore the pool assignment: {err}");
         }
      }
      self.record_restriction(
         session_id,
         crate::stats::Restriction::Reinstated,
         "probe",
         "Manual probe found the account alive; verdict cleared",
      );
      tracing::info!(
         session_id,
         pool = target_pool.as_str(),
         "account reinstated by probe"
      );
      true
   }

   /// Leave search for the view pool without any suspension semantics.
   ///
   /// X served this account no search that another account could get, which is
   /// not a statement about the account itself: it may be a query with no
   /// matches, an age gate, or a filter. The account keeps working for profile
   /// reads and stops being tried for search.
   pub(super) async fn demote_to_view(
      &self,
      session_id: i64,
      restriction: Option<crate::stats::Restriction>,
      evidence: &'static str,
   ) {
      let Some(slot) = self.slot(session_id) else {
         return;
      };
      let in_search = slot.write_limits(|lim| {
         let in_search = lim.search_only;
         let was_search_pass = lim.search_pass_only;
         if in_search {
            lim.demoted_from_search = true;
            if was_search_pass {
               lim.demoted_from_search_pass = true;
            }
         }
         lim.search_banned = true;
         lim.search_only = false;
         lim.search_pass_only = false;
         lim.refuse("SearchTimeline");
         in_search
      });
      self.persist_soon(session_id);
      if let Some(restriction) = restriction {
         self.record_restriction(session_id, restriction, "SearchTimeline", evidence);
      }
      if in_search {
         self.move_to_view_pool(session_id, evidence).await;
      }
   }

   /// Operator override moving a loaded account into `search`, `search-pass`,
   /// or `view`.
   pub async fn set_account_pool(
      &self,
      session_id: i64,
      pool: AccountPool,
   ) -> crate::error::Result<bool> {
      let Some(slot) = self.slot(session_id) else {
         return Err(crate::error::Error::NotFound(format!(
            "Account {session_id} is not loaded"
         )));
      };
      self.writes.cancel_demotion(session_id);
      if let Some(store) = &self.account_store {
         store.set_pool(session_id, pool).await?;
      }
      let want_search = pool.is_search();
      let want_search_pass = pool == AccountPool::SearchPass;
      let changed = slot.write_limits(|lim| {
         if want_search {
            let changed = !lim.search_only
               || lim.search_pass_only != want_search_pass
               || lim.demoted_from_search
               || lim.demoted_from_search_pass
               || lim.search_banned
               || lim.suspended
               || lim.retired
               || lim.refused_ops.contains_key("SearchTimeline")
               || lim.search_404_at.is_some();
            lim.search_only = true;
            lim.search_pass_only = want_search_pass;
            lim.demoted_from_search = false;
            lim.demoted_from_search_pass = false;
            lim.search_banned = false;
            lim.suspended = false;
            lim.retired = false;
            lim.refused_ops.remove("SearchTimeline");
            lim.search_404_at = None;
            lim.resting.clear();
            changed
         } else {
            let changed = lim.search_only
               || lim.search_pass_only
               || lim.demoted_from_search
               || lim.demoted_from_search_pass;
            lim.search_only = false;
            lim.search_pass_only = false;
            lim.demoted_from_search = false;
            lim.demoted_from_search_pass = false;
            changed
         }
      });
      if want_search {
         slot.has_search_failures.store(false, Ordering::Release);
         *slot
            .search_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SearchFailureState::default();
      }
      if changed {
         self.persist_soon(session_id);
         self.record_restriction(
            session_id,
            if want_search {
               crate::stats::Restriction::Reinstated
            } else {
               crate::stats::Restriction::SearchOnly
            },
            "admin",
            match pool {
               AccountPool::SearchPass => "Operator moved account to the search-pass pool",
               AccountPool::Search => "Operator moved account to the search pool",
               AccountPool::View => "Operator moved account to the view pool",
            },
         );
      }
      Ok(changed)
   }

   /// Accounts moved out of search that are eligible for trickle recovery.
   pub(super) async fn demoted_search_candidates(&self, limit: usize) -> Vec<i64> {
      if limit == 0 {
         return Vec::new();
      }
      let slots = self.slots();
      let count = slots.len();
      if count == 0 {
         return Vec::new();
      }
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      let start = self.recovery_cursor.load(Ordering::Relaxed) % count;
      let mut candidates = Vec::with_capacity(limit.min(count));
      let mut next_cursor = start;
      for offset in 0..count {
         let index = (start + offset) % count;
         let slot = &slots[index];
         if slot.search_only.load(Ordering::Relaxed)
            || slot.rejected.load(Ordering::Relaxed)
            || slot.unavailable.load(Ordering::Relaxed)
         {
            continue;
         }
         let eligible = {
            let lim = slot.read_limits();
            lim.demoted_from_search
               && !lim.search_only
               && !lim.rejected
               && !lim.ct0_dead
               && !lim.retired
               && !lim.is_globally_limited_at(now)
         };
         if eligible {
            candidates.push(slot.credentials.id);
            next_cursor = (index + 1) % count;
            if candidates.len() == limit {
               break;
            }
         }
      }
      if !candidates.is_empty() {
         self.recovery_cursor.store(next_cursor, Ordering::Relaxed);
      }
      candidates
   }

   /// Clear the search-demotion marker and persist the permanent view
   /// assignment.
   pub(super) async fn clear_demoted_from_search(&self, session_id: i64) {
      self.writes.cancel_demotion(session_id);
      if let Some(slot) = self.slot(session_id) {
         slot.write_limits(|lim| {
            lim.demoted_from_search = false;
            lim.demoted_from_search_pass = false;
         });
      }
      self.persist_soon(session_id);
      if let Some(store) = &self.account_store {
         let _ = store.set_pool(session_id, AccountPool::View).await;
      }
   }

   /// Write the view-pool assignment to the store, retrying on failure.
   async fn move_to_view_pool(&self, session_id: i64, evidence: &str) {
      if let Some(store) = &self.account_store {
         match store.demote(session_id).await {
            Ok(()) => tracing::info!(session_id, reason = evidence, "moved to the view pool"),
            Err(err) => {
               tracing::warn!(
                  session_id,
                  "could not persist view assignment; will retry: {err}"
               );
               self.writes.retry_demotion(session_id);
            },
         }
         return;
      }
      #[cfg(test)]
      {
         let _write = self.source_write.lock().await;
         if let Err(err) = demote_session_line(&self.source_path, session_id).await {
            tracing::warn!(session_id, "could not move it to the view pool: {err}");
         }
      }
   }

   /// Rest an account after an ambiguous 404 without initiating a settings
   /// write.
   pub(super) async fn record_search_404(&self, session_id: i64) -> bool {
      let Some(slot) = self.slot(session_id) else {
         return false;
      };
      let recorded = slot.write_limits(|lim| {
         if lim.refuses("SearchTimeline") {
            return false;
         }
         lim.search_404_at = Some(time::OffsetDateTime::now_utc().unix_timestamp());
         lim.refuse("SearchTimeline");
         true
      });
      if !recorded {
         return false;
      }
      log_events::note(Event::SearchRested);
      self.record_restriction(
         session_id,
         crate::stats::Restriction::SearchRested,
         "SearchTimeline",
         "Search request returned HTTP 404; temporary cooldown",
      );
      self.persist_soon(session_id);
      true
   }

   /// Mark a session's credentials as refused, taking it permanently out of
   /// rotation (including across restarts and settings sync) until an operator
   /// explicitly reinstates it.
   pub async fn mark_rejected(&self, session_id: i64) {
      let Some(slot) = self.slot(session_id) else {
         return;
      };
      let (newly_rejected, changed) = slot.write_limits(|lim| {
         let newly_rejected = !lim.rejected;
         let changed = newly_rejected || !lim.filters_failed || lim.filters_retry_at != i64::MAX;
         lim.rejected = true;
         lim.filters_failed = true;
         lim.filters_retry_at = i64::MAX;
         (newly_rejected, changed)
      });
      if newly_rejected {
         self.record_restriction(
            session_id,
            crate::stats::Restriction::CredentialsRejected,
            "upstream",
            "Upstream rejected the account credentials",
         );
      }
      if changed {
         self.persist_soon(session_id);
      }
   }

   /// Mark a session as globally rate limited.
   pub async fn mark_limited(&self, session_id: i64) {
      if let Some(slot) = self.slot(session_id) {
         slot.write_limits(|lim| {
            lim.limited = true;
            lim.limited_at = time::OffsetDateTime::now_utc().unix_timestamp();
         });
      }
      self.persist_soon(session_id);
   }

   /// Get session count.
   pub fn len(&self) -> usize {
      self.slots().len()
   }

   /// Session ids in load order, used to detect new accounts.
   #[must_use]
   pub fn session_ids(&self) -> Vec<i64> {
      self
         .slots()
         .iter()
         .map(|slot| slot.credentials.id)
         .collect()
   }

   pub fn set_telemetry(&mut self, telemetry: Option<crate::stats::StatsStore>) {
      self.telemetry = telemetry;
   }

   pub async fn account_state(&self, session_id: i64) -> Option<AccountState> {
      let slot = self.slot(session_id)?;
      let lim = slot.read_limits();
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      Some(self.build_account_state(&slot, &lim, now))
   }

   pub async fn account_states(&self, ids: &[i64]) -> HashMap<i64, AccountState> {
      if ids.is_empty() {
         return HashMap::new();
      }
      let roster = self.roster();
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      let mut states = HashMap::with_capacity(ids.len());
      for &id in ids {
         if states.contains_key(&id) {
            continue;
         }
         if let Some(&index) = roster.by_id.get(&id)
            && let Some(slot) = roster.slots.get(index)
         {
            let lim = slot.read_limits();
            states.insert(id, self.build_account_state(slot, &lim, now));
         }
      }
      states
   }

   fn build_account_state(
      &self,
      slot: &SessionSlot,
      lim: &SessionLimits,
      now: i64,
   ) -> AccountState {
      let last_active = slot.last_active.load(Ordering::Relaxed);
      let last_search_empty = slot.last_search_empty.load(Ordering::Relaxed);
      let ct0 = slot.ct0();
      AccountState {
         id:                        slot.credentials.id,
         username:                  slot.credentials.username.clone(),
         kind:                      slot.credentials.kind,
         pool:                      Self::effective_pool(lim),
         home_pool:                 Self::home_pool(lim),
         demoted_from_search:       lim.demoted_from_search,
         available:                 !(lim.rejected || lim.ct0_dead || lim.retired),
         has_auth_token:            !slot.credentials.auth_token.is_empty(),
         ct0_present:               !ct0.is_empty(),
         ct0_len:                   ct0.len(),
         credentials_rejected:      lim.rejected,
         search_banned:             lim.search_banned,
         profile_banned:            lim.profile_banned,
         ct0_dead:                  lim.ct0_dead,
         suspended:                 lim.suspended,
         retired:                   lim.retired,
         filters_cleared:           lim.filters_cleared,
         filters_retry_at:          lim.filters_retry_at,
         filters_failed:            lim.filters_failed,
         globally_limited:          lim.is_globally_limited_at(now),
         limited_at:                lim.limited.then_some(lim.limited_at),
         profile_strikes:           lim.profile_strikes,
         active_calls:              slot
            .max_concurrency
            .saturating_sub(slot.permits.available_permits()),
         last_active:               (last_active > 0).then_some(last_active as i64),
         search_empty_streak:       slot.search_empty_streak.load(Ordering::Relaxed),
         search_empty_total:        slot.search_empty_total.load(Ordering::Relaxed),
         search_non_empty_total:    slot.search_non_empty_total.load(Ordering::Relaxed),
         search_rate_limited_total: slot.search_rate_limited_total.load(Ordering::Relaxed),
         last_search_empty:         (last_search_empty > 0).then_some(last_search_empty as i64),
         rate_limits:               lim.apis.clone().into_iter().collect(),
         refused_operations:        lim
            .refused_ops
            .iter()
            .filter(|(_, seen)| **seen > now - 86400)
            .map(|(api, seen)| (api.clone(), *seen))
            .collect(),
         verified_operations:       lim.verified_ops.clone().into_iter().collect(),
      }
   }

   /// Session IDs whose live consecutive empty-search streak is at least
   /// `min_streak`.
   pub fn search_empty_streak_ids(&self, min_streak: u32) -> Vec<i64> {
      self
         .slots()
         .iter()
         .filter(|slot| slot.search_empty_streak.load(Ordering::Relaxed) >= min_streak)
         .map(|slot| slot.credentials.id)
         .collect()
   }

   /// Record that a search account answered a tweet search (`Latest`/`Top`/
   /// `Media`) without a `search_timeline.timeline` backend failure.
   ///
   /// Clears any failure streak on this account and, when the page had results,
   /// marks tweet search as globally healthy across the pool.
   pub(crate) fn record_tweet_search_ok(&self, session_id: i64, non_empty: bool) {
      let now = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0);
      self.record_tweet_search_ok_at(session_id, non_empty, now);
   }

   fn record_tweet_search_ok_at(&self, session_id: i64, non_empty: bool, now: u64) {
      if non_empty {
         self.last_tweet_search_ok.store(now, Ordering::Relaxed);
      }
      let Some(slot) = self.slot(session_id) else {
         return;
      };
      if slot.has_search_failures.swap(false, Ordering::AcqRel) {
         let mut state = slot
            .search_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
         state.corroborated_streak = 0;
         state.corroborated_query = 0;
         state.corroborated_at = 0;
         state.uncorroborated_streak = 0;
         state.uncorroborated_query = 0;
      }
   }

   /// True when some account in the pool answered a non-empty tweet search
   /// recently, distinguishing broken individual accounts from a global X
   /// search outage.
   pub(crate) fn tweet_search_healthy(&self) -> bool {
      let now = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0);
      self.tweet_search_healthy_at(now)
   }

   fn tweet_search_healthy_at(&self, now: u64) -> bool {
      let last = self.last_tweet_search_ok.load(Ordering::Relaxed);
      last > 0 && now.saturating_sub(last) <= TWEET_SEARCH_HEALTHY_WINDOW_SECS
   }

   /// Immediately rest an account after `SearchTimeline` returned an upstream
   /// `"Internal server error"` on `search_timeline.timeline`, and drop its
   /// warm-connection preference so concurrent requests pick other accounts.
   pub(crate) fn rest_search_failure(&self, session_id: i64, endpoint: &str, secs: i64) {
      let Some(slot) = self.slot(session_id) else {
         return;
      };
      slot.last_active.store(0, Ordering::Relaxed);
      slot.write_limits(|lim| lim.rest_for(endpoint, secs));
   }

   /// Record a tweet-search `"Internal server error"` on `session_id` after the
   /// request's retry loop finishes.
   ///
   /// * **`corroborated == true`**: another account answered the exact same
   ///   query with HTTP 200. Strike 1 rests the account for 2 minutes; strike 2
   ///   (on a different query at least 10 seconds later, with no tweet-search
   ///   success in between) moves the account to the view pool with
   ///   `demoted_from_search = true`.
   /// * **`corroborated == false`**: every tried account failed this query.
   ///   Never demotes directly (protecting healthy accounts from a bad query or
   ///   a global X outage); after 3 distinct failing queries while other
   ///   accounts are healthy, returns [`SearchFailureAction::NeedsCanary`] so
   ///   one background `"news"` check decides whether the account is suspended.
   pub(crate) async fn record_search_timeline_failure(
      &self,
      session_id: i64,
      endpoint: &str,
      query_hash: u64,
      corroborated: bool,
   ) -> SearchFailureAction {
      let now = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0);
      self
         .record_search_timeline_failure_at(session_id, endpoint, query_hash, corroborated, now)
         .await
   }

   async fn record_search_timeline_failure_at(
      &self,
      session_id: i64,
      endpoint: &str,
      query_hash: u64,
      corroborated: bool,
      now: u64,
   ) -> SearchFailureAction {
      let Some(slot) = self.slot(session_id) else {
         return SearchFailureAction::Rested;
      };
      slot.last_active.store(0, Ordering::Relaxed);
      let now_i64 = i64::try_from(now).unwrap_or(i64::MAX);

      if corroborated {
         let should_demote = {
            let mut state = slot
               .search_failures
               .lock()
               .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.corroborated_streak == 0 {
               state.corroborated_streak = 1;
               state.corroborated_query = query_hash;
               state.corroborated_at = now;
            } else if query_hash != state.corroborated_query
               && now.saturating_sub(state.corroborated_at) >= SEARCH_CORROBORATED_MIN_GAP_SECS
            {
               state.corroborated_streak = state.corroborated_streak.saturating_add(1);
               state.corroborated_query = query_hash;
               state.corroborated_at = now;
            }
            let demote = state.corroborated_streak >= SEARCH_CORROBORATED_DEMOTE_THRESHOLD;
            if demote {
               *state = SearchFailureState::default();
               slot.has_search_failures.store(false, Ordering::Release);
            } else {
               slot.has_search_failures.store(true, Ordering::Release);
            }
            demote
         };

         if should_demote {
            tracing::info!(
               session_id,
               session_user = %slot.credentials.username,
               "search account repeatedly failed with Internal server error while other accounts \
                succeeded; moving to view pool"
            );
            self
               .demote_to_view(
                  session_id,
                  Some(crate::stats::Restriction::SearchBanned),
                  "SearchTimeline repeatedly failed with Internal server error while other \
                   accounts succeeded",
               )
               .await;
            return SearchFailureAction::Demoted;
         }

         slot.write_limits(|lim| {
            lim.rest_for_at(endpoint, SEARCH_CORROBORATED_FAIL_REST_SECS, now_i64);
         });
         return SearchFailureAction::Rested;
      }

      slot.write_limits(|lim| {
         lim.rest_for_at(endpoint, SEARCH_UNCORROBORATED_FAIL_REST_SECS, now_i64);
      });
      if !self.tweet_search_healthy_at(now) {
         return SearchFailureAction::Rested;
      }

      let needs_canary = {
         let mut state = slot
            .search_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
         if state.uncorroborated_streak == 0 || query_hash != state.uncorroborated_query {
            state.uncorroborated_streak = state.uncorroborated_streak.saturating_add(1);
            state.uncorroborated_query = query_hash;
            slot.has_search_failures.store(true, Ordering::Release);
         }
         if state.uncorroborated_streak >= SEARCH_CANARY_TRIGGER_THRESHOLD
            && !state.canary_in_flight
         {
            state.canary_in_flight = true;
            state.uncorroborated_streak = 0;
            state.uncorroborated_query = 0;
            true
         } else {
            false
         }
      };

      if needs_canary {
         SearchFailureAction::NeedsCanary
      } else {
         SearchFailureAction::Rested
      }
   }

   /// Release the per-account canary guard after a background `"news"` check
   /// completes.
   pub(crate) fn finish_search_canary(&self, session_id: i64) {
      let Some(slot) = self.slot(session_id) else {
         return;
      };
      slot
         .search_failures
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner)
         .canary_in_flight = false;
   }

   /// Record one upstream `SearchTimeline` outcome for an account, updating
   /// both the live streak/totals and PostgreSQL telemetry.
   ///
   /// When a supposedly valid (verified, unbanned) search account hits repeated
   /// consecutive empty responses, log an operational event so it stands out on
   /// the admin statistics and restrictions pages.
   pub(crate) async fn record_search_outcome(
      &self,
      session_id: i64,
      endpoint: &str,
      outcome: crate::stats::SearchAccountOutcome,
   ) {
      if let Some(stats) = &self.telemetry {
         stats.search_account_outcome(session_id, outcome);
      }
      let Some(slot) = self.slot(session_id) else {
         return;
      };
      match outcome {
         crate::stats::SearchAccountOutcome::NonEmpty => {
            slot.search_empty_streak.store(0, Ordering::Relaxed);
            slot.search_non_empty_total.fetch_add(1, Ordering::Relaxed);
         },
         crate::stats::SearchAccountOutcome::RateLimited => {
            slot
               .search_rate_limited_total
               .fetch_add(1, Ordering::Relaxed);
         },
         crate::stats::SearchAccountOutcome::Empty => {
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            slot
               .last_search_empty
               .store(u64::try_from(now).unwrap_or(0), Ordering::Relaxed);
            let total_empty = slot.search_empty_total.fetch_add(1, Ordering::Relaxed) + 1;
            let total_non_empty = slot.search_non_empty_total.load(Ordering::Relaxed);
            let streak = slot.search_empty_streak.fetch_add(1, Ordering::Relaxed) + 1;
            if is_search_empty_streak_milestone(streak) {
               let maybe_evidence = {
                  let lim = slot.read_limits();
                  let supposed_valid = lim.search_only
                     && lim.verified_op("SearchTimeline")
                     && !lim.rejected
                     && !lim.ct0_dead
                     && !lim.suspended
                     && !lim.retired
                     && !lim.search_banned;
                  if supposed_valid {
                     let quota = lim.apis.get(endpoint).map_or_else(
                        || "quota unknown".to_owned(),
                        |rate| format!("quota {}/{}", rate.remaining, rate.limit),
                     );
                     let filters = if lim.filters_cleared {
                        "filters cleared"
                     } else if lim.filters_failed {
                        "filters refused"
                     } else {
                        "filters pending"
                     };
                     Some(format!(
                        "Supposed valid search account returned {streak} consecutive empty \
                         responses (since start: {total_empty} empty / {total_non_empty} with \
                         results; {filters}; {quota})"
                     ))
                  } else {
                     None
                  }
               };
               if let Some(evidence) = maybe_evidence {
                  tracing::warn!(
                     session_id,
                     session_user = %slot.credentials.username,
                     streak,
                     total_empty,
                     total_non_empty,
                     "supposed valid search account is consistently returning empty responses"
                  );
                  self.record_event(
                     crate::stats::AccountEvent::new(
                        session_id,
                        crate::stats::Restriction::SearchEmptyStreak,
                        endpoint,
                        &evidence,
                     )
                     .upstream(200, None),
                  );
               }
            }
         },
      }
   }

   pub(crate) fn cookie_credentials(&self, session_id: i64) -> Option<(String, String)> {
      let slot = self.slot(session_id)?;
      (slot.credentials.kind == SessionKind::Cookie)
         .then(|| (slot.credentials.auth_token.clone(), slot.ct0()))
   }

   pub(crate) fn record_event(&self, event: crate::stats::AccountEvent) {
      if let Some(stats) = &self.telemetry {
         stats.account_event(event);
      }
   }

   fn record_restriction(
      &self,
      session_id: i64,
      kind: crate::stats::Restriction,
      endpoint: &str,
      evidence: &str,
   ) {
      if let Some(stats) = &self.telemetry {
         stats.account_event(crate::stats::AccountEvent::new(
            session_id, kind, endpoint, evidence,
         ));
      }
   }

   /// Check if pool is empty.
   pub fn is_empty(&self) -> bool {
      self.slots().is_empty()
   }

   /// Cookie sessions whose hiding settings are not off yet. Only they are
   /// written to, so a restart does not spend thousands of settings POSTs on
   /// accounts that already had theirs changed.
   ///
   /// A refusal is retried when the account's `filters_retry_at` passes: X
   /// answers a settings write on a suspended account with code 64, and
   /// suspensions are lifted, so a permanent skip left those accounts hiding
   /// sensitive media long after they could have been fixed. The writer picks
   /// the wait — a suspension gets a day, a missing transaction id gets one
   /// rescan — and zero means retry on the next pass.
   ///
   /// The order is who the visitor is served by. A full drain is hours at one
   /// write every five seconds, so the search-pool accounts go first, then the
   /// accounts X has already answered a call for, then the rest. Sorting by id
   /// alone put the block of freshly created accounts X has locked at the front
   /// of every batch, and each of those spent two minutes of the queue to be
   /// told 326; the accounts actually carrying search waited behind them.
   ///
   /// `search_banned` does not exempt an account. It means X will not serve it
   /// search, which says nothing about whether it may read a profile — those
   /// accounts stay in the pool for view traffic, and while their filters are
   /// on they still hide sensitive posts from visitors. Exempting them left
   /// hundreds of healthy accounts that never got a settings write at all.
   pub async fn pending_filter_sync_ids(&self) -> Vec<i64> {
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      let mut pending: Vec<((bool, bool, bool), i64)> = self
         .slots()
         .iter()
         .filter(|slot| {
            slot.credentials.kind == SessionKind::Cookie
               && !slot.rejected.load(Ordering::Relaxed)
               && !slot.unavailable.load(Ordering::Relaxed)
         })
         .filter_map(|slot| {
            let lim = slot.read_limits();
            if lim.ct0_dead
               || lim.rejected
               || lim.filters_cleared
               // A suspension is learned once. The write that found it is
               // the last one: repeating it reports the same verdict, so the
               // account is never probed again — neither while it is kept
               // for viewing nor once it has been retired.
               || lim.suspended
               || lim.retired
               || (lim.filters_failed && lim.filters_retry_at > now)
            {
               return None;
            }
            Some((
               (
                  lim.search_pass_only,
                  lim.search_only,
                  !lim.verified_ops.is_empty(),
               ),
               slot.credentials.id,
            ))
         })
         .collect();
      pending
         .sort_unstable_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
      pending.into_iter().map(|(_, id)| id).collect()
   }

   /// Remember that X refused this account the settings write, so the next
   /// pass does not spend the same POSTs on it until `retry_after` has passed.
   pub async fn mark_filters_failed(&self, session_id: i64, evidence: &str, retry_after: Duration) {
      let retry_at = time::OffsetDateTime::now_utc().unix_timestamp()
         + i64::try_from(retry_after.as_secs()).unwrap_or(i64::MAX);
      if let Some(slot) = self.slot(session_id) {
         slot.write_limits(|lim| {
            lim.filters_failed = true;
            lim.filters_retry_at = retry_at;
         });
      }
      self.record_restriction(
         session_id,
         crate::stats::Restriction::SettingsRejected,
         "settings",
         evidence,
      );
      self.persist_soon(session_id);
   }

   /// Remember that the settings that hide posts are off for this account, so
   /// a restart does not write them again.
   pub async fn mark_filters_cleared(&self, session_id: i64) {
      if let Some(slot) = self.slot(session_id) {
         slot.write_limits(|lim| {
            lim.filters_cleared = true;
            lim.filters_failed = false;
            lim.filters_retry_at = 0;
         });
      }
      self.persist_soon(session_id);
   }

   /// One account's credentials, without taking a concurrency slot or checking
   /// whether it may be used. For work that only needs to know who it is.
   pub fn credentials(&self, session_id: i64) -> Option<Arc<SessionCredentials>> {
      self
         .slot(session_id)
         .map(|slot| Arc::clone(&slot.credentials))
   }

   /// The `ct0` to send: one x.com issued on a rejection since startup, or the
   /// one loaded from the account store.
   pub fn ct0(&self, session_id: i64) -> Option<String> {
      Some(self.slot(session_id)?.ct0())
   }

   /// Keep a `ct0` x.com just issued and write it back for the next start.
   ///
   /// No request is made for it: a csrf rejection carries the replacement
   /// token in its own `set-cookie`, so the account recovers on the call that
   /// failed instead of costing a page load of x.com.
   pub fn hold_ct0(&self, session_id: i64, ct0: String) {
      let Some(slot) = self.slot(session_id) else {
         return;
      };
      {
         let mut pending = self
            .writes
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
         *slot
            .fresh_ct0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ct0.clone());
         if self.account_store.is_some() {
            pending.ct0s.insert(session_id, ct0);
         }
      }
      self.writes.ready.notify_one();
   }

   fn spawn_account_writer(&self, store: AccountStore) {
      let writes = Arc::clone(&self.writes);
      tokio::spawn(async move {
         loop {
            writes.ready.notified().await;
            sleep(ACCOUNT_WRITE_DELAY).await;
            let PendingAccountWrites { ct0s, demotions } = std::mem::take(
               &mut *writes
                  .pending
                  .lock()
                  .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            let ct0s = ct0s.into_iter().collect::<Vec<_>>();
            if !ct0s.is_empty()
               && let Err(err) = store.save_ct0(&ct0s).await
            {
               tracing::warn!("could not store the fresh ct0s; will retry: {err}");
               let mut pending = writes
                  .pending
                  .lock()
                  .unwrap_or_else(std::sync::PoisonError::into_inner);
               for (id, ct0) in ct0s {
                  // An update received during the flush supersedes the retry.
                  pending.ct0s.entry(id).or_insert(ct0);
               }
               writes.ready.notify_one();
            }
            for id in demotions {
               if let Err(err) = store.demote(id).await {
                  tracing::warn!(
                     session_id = id,
                     "could not persist view assignment; will retry: {err}"
                  );
                  writes.retry_demotion(id);
               }
            }
         }
      });
   }

   /// Operator override replacing a cookie session's `ct0` CSRF token in memory
   /// and PostgreSQL, clearing `ct0_dead` so the account can be probed or used
   /// immediately. Returns whether `ct0_dead` was cleared.
   pub async fn set_ct0(&self, session_id: i64, ct0: String) -> Result<bool> {
      let Some(slot) = self.slot(session_id) else {
         return Err(Error::NotFound(format!(
            "Account {session_id} is not loaded"
         )));
      };
      if slot.credentials.kind != SessionKind::Cookie {
         return Err(Error::InvalidConfig(
            "ct0 can only be updated for cookie-authenticated accounts".to_owned(),
         ));
      }
      if let Some(store) = &self.account_store {
         store.save_ct0(&[(session_id, ct0.clone())]).await?;
      }
      let ct0_len = ct0.len();
      {
         let mut pending = self
            .writes
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
         *slot
            .fresh_ct0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ct0);
         pending.ct0s.remove(&session_id);
      }
      let cleared_ct0_dead = slot.write_limits(|lim| {
         let was_dead = lim.ct0_dead;
         lim.ct0_dead = false;
         was_dead
      });
      if cleared_ct0_dead {
         self.persist_soon(session_id);
         self.record_restriction(
            session_id,
            crate::stats::Restriction::Reinstated,
            "admin",
            "Operator updated ct0 cookie; cleared stale CSRF flag",
         );
      }
      tracing::info!(
         session_id,
         ct0_len,
         cleared_ct0_dead,
         "operator updated account ct0"
      );
      Ok(cleared_ct0_dead)
   }

   /// Put a retired account back in rotation: x.com issued it a token again.
   pub async fn clear_ct0_dead(&self, session_id: i64) {
      if let Some(slot) = self.slot(session_id) {
         slot.write_limits(|lim| {
            lim.ct0_dead = false;
         });
      }
      self.persist_soon(session_id);
   }

   /// X rejected this account's `ct0`, so it cannot serve csrf-protected calls
   /// without a fresh token. Ordinary imports skip existing accounts.
   ///
   /// Nothing is fetched to replace it. A refresh meant a full page load of
   /// x.com per rejection — the one request a browser makes that an API client
   /// otherwise never makes, and where X's anti-bot check runs — so the token
   /// is left stale and the account is taken out of rotation instead.
   pub async fn mark_ct0_dead(&self, session_id: i64) {
      let Some(slot) = self.slot(session_id) else {
         return;
      };
      let newly_dead = slot.write_limits(|lim| {
         if lim.ct0_dead {
            return false;
         }
         lim.ct0_dead = true;
         true
      });
      if newly_dead {
         self.record_restriction(
            session_id,
            crate::stats::Restriction::CsrfRejected,
            "upstream",
            "Upstream rejected the CSRF token; account is out of rotation",
         );
         self.persist_soon(session_id);
         tracing::info!(session_id, "ct0 is stale; account is out of rotation");
      }
   }

   /// Take a specific session, for startup work that must hit that account.
   /// A search account is handed out only for search, and for the calls it
   /// makes about itself: the settings write that turns its own filters off
   /// and the id lookup that write is keyed by.
   pub async fn acquire_id(&self, session_id: i64, api: &str) -> Result<SessionLease> {
      let slot = self.slot(session_id).ok_or(Error::NoSessions)?;
      let spec = ApiSpec::new(api);
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      {
         let lim = slot.read_limits();
         if !Self::serves_slot(&lim, spec, now) {
            return Err(if lim.rejected {
               Error::SessionRejected(format!("session {session_id} rejected"))
            } else if lim.is_limited(api) {
               Error::RateLimited
            } else {
               Error::NoSessions
            });
         }
      }
      let permit = Arc::clone(&slot.permits)
         .acquire_owned()
         .await
         .map_err(|_| Error::Internal("session request limiter closed".into()))?;
      Ok(Self::lease(&slot, permit, false))
   }

   /// Take a specific session for a health probe the operator asked for.
   ///
   /// `acquire_id` refuses any account the pool has benched, which is the
   /// correct default: nothing should be spent on a retired account. A probe is
   /// the one caller that has to reach it anyway — the whole point is to ask X
   /// whether a verdict is still true. Only the pool-level gates are skipped;
   /// X still answers for itself, and `note_suspended` records whatever it
   /// says.
   ///
   /// The slot is not metered, so a refused probe costs the account no quota.
   pub async fn probe_lease(&self, session_id: i64) -> Result<SessionLease> {
      let slot = self.slot(session_id).ok_or(Error::NoSessions)?;
      let permit = Arc::clone(&slot.permits)
         .acquire_owned()
         .await
         .map_err(|_| Error::Internal("session request limiter closed".into()))?;
      Ok(Self::lease(&slot, permit, false))
   }

   /// A cookie session's id and cookie header, for requests that fetch x.com's
   /// web app rather than the API. Takes no concurrency permit, since those
   /// pages are outside the per-session API rate limits. The id lets the fetch
   /// go out as that account's browser, on its proxy.
   pub(crate) fn cookie_header(&self) -> Option<(i64, String)> {
      let slots = self.slots();
      slots.iter().find_map(|slot| {
         let creds = &slot.credentials;
         if creds.kind != SessionKind::Cookie
            || creds.auth_token.is_empty()
            || slot.search_only.load(Ordering::Relaxed)
            || slot.rejected.load(Ordering::Relaxed)
            || slot.unavailable.load(Ordering::Relaxed)
         {
            return None;
         }
         let ct0 = slot.ct0();
         (!ct0.is_empty()).then(|| {
            (
               creds.id,
               format!("auth_token={}; ct0={ct0}", creds.auth_token),
            )
         })
      })
   }

   /// Get health statistics about the session pool.
   #[expect(
      clippy::iter_over_hash_type,
      reason = "iteration order irrelevant for aggregation"
   )]
   pub async fn get_health(&self) -> HealthResponse {
      let slots = self.slots();
      let now = time::OffsetDateTime::now_utc().unix_timestamp();

      let mut limited_count = 0;
      let mut rejected_count = 0;
      let mut total_requests = 0;
      let mut by_api = HashMap::<String, i32>::new();

      for slot in slots.iter() {
         let lim = slot.read_limits();
         // Counted once by effective state, since a session can carry both
         // flags and a global limit expires on its own.
         if lim.rejected {
            rejected_count += 1;
         } else if lim.is_globally_limited_at(now) {
            limited_count += 1;
         }

         for (api, limit_info) in &lim.apis {
            let used = limit_info.limit - limit_info.remaining;
            total_requests += used;
            *by_api.entry(api.clone()).or_default() += used;
         }
      }
      let total = slots.len();

      HealthResponse {
         sessions:  SessionStats {
            total,
            limited: limited_count,
            rejected: rejected_count,
            // Endpoint budgets are per API, so this counts only sessions that
            // are unusable for every endpoint.
            available: total.saturating_sub(limited_count + rejected_count),
         },
         requests:  RequestStats {
            total: total_requests,
            by_api,
         },
         timestamp: time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap(),
      }
   }

   /// Get detailed debug info about sessions.
   pub async fn get_debug(&self) -> DebugResponse {
      let slots = self.slots();
      let sessions = slots
         .iter()
         .map(|slot| {
            let sess = &slot.credentials;
            let lim = slot.read_limits();
            SessionDetail {
               id:         sess.id,
               username:   sess.username.clone(),
               kind:       sess.kind,
               limited:    lim.limited,
               rejected:   lim.rejected,
               limited_at: lim.limited_at,
               pending:    i32::try_from(
                  slot
                     .max_concurrency
                     .saturating_sub(slot.permits.available_permits()),
               )
               .unwrap_or(i32::MAX),
               apis:       lim.apis.clone(),
            }
         })
         .collect();

      DebugResponse {
         sessions,
         count: slots.len(),
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

#[cfg(test)]
/// Drop `pool` from one account's line, so the next start loads it into the
/// view pool. `false` when the line carried no pool already.
async fn demote_session_line(path: &str, session_id: i64) -> Result<bool> {
   let content = fs::read_to_string(path).await?;
   let mut kept = String::with_capacity(content.len());
   let mut changed = false;
   for line in content.lines() {
      if line.trim().is_empty() {
         continue;
      }
      if session_line_id(line) == Some(session_id) {
         let mut value: serde_json::Value = serde_json::from_str(line)?;
         if let Some(map) = value.as_object_mut()
            && map.remove("pool").is_some()
         {
            kept.push_str(&serde_json::to_string(&value)?);
            kept.push('\n');
            changed = true;
            continue;
         }
      }
      kept.push_str(line);
      kept.push('\n');
   }
   if !changed {
      return Ok(false);
   }
   let tmp = format!(
      "{path}.{}.{}.tmp",
      std::process::id(),
      time::OffsetDateTime::now_utc().unix_timestamp_nanos()
   );
   fs::write(&tmp, kept).await?;
   fs::rename(&tmp, path).await?;
   Ok(true)
}

#[cfg(test)]
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
   use std::{
      env,
      process,
      time::{
         Duration,
         Instant,
      },
   };

   use tokio::{
      fs,
      time::timeout,
   };

   use super::{
      MAX_PARK,
      QUEUE_PER_SLOT,
      SearchFailureAction,
      SessionKind,
      SessionPool,
      SuspensionProbe,
      normalized_parameter_string,
      percent_encode,
   };
   use crate::error::Error;

   /// A cookie account whose filters were refused is retried once the wait
   /// passes. Zero means "retry now", which is how a refusal recorded by a
   /// writer that minted transaction ids X would not take comes back.
   #[tokio::test]
   async fn a_refused_settings_write_waits_out_its_retry_time() {
      let path = env::temp_dir().join(format!("teapot-filter-retry-{}.jsonl", process::id()));
      fs::write(
         &path,
         r#"{"id":7,"username":"x-1","kind":"cookie","auth_token":"a","ct0":"c"}"#,
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 1).await.unwrap();

      assert_eq!(pool.pending_filter_sync_ids().await, vec![7]);

      pool
         .mark_filters_failed(7, "synthetic refusal", Duration::from_hours(24))
         .await;
      assert!(
         pool.pending_filter_sync_ids().await.is_empty(),
         "a refusal inside its wait is not retried"
      );

      pool.slot(7).unwrap().write_limits(|lim| {
         lim.filters_retry_at = 0;
      });
      assert_eq!(
         pool.pending_filter_sync_ids().await,
         vec![7],
         "a refusal with no retry time is retried"
      );

      pool.mark_filters_cleared(7).await;
      assert!(pool.pending_filter_sync_ids().await.is_empty());

      fs::remove_file(path).await.unwrap();
   }

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
   async fn appending_preserves_credentials_leases_and_parked_calls() {
      let pool = SessionPool::empty(1);
      let session = |line| serde_json::from_str(line).unwrap();
      assert_eq!(
         pool
            .append_sessions(vec![session(
               r#"{"id":1,"username":"first","kind":"cookie","auth_token":"a","ct0":"initial"}"#,
            )])
            .await,
         1
      );
      let first = pool.acquire("TweetDetail", None).await.unwrap();
      let original = pool.slots();
      let queued = pool.acquire_where("TweetDetail", None, |id| id == 1);
      tokio::pin!(queued);
      std::future::poll_fn(|cx| {
         assert!(queued.as_mut().poll(cx).is_pending());
         std::task::Poll::Ready(())
      })
      .await;
      assert_eq!(original[0].wanted.load(super::Ordering::Relaxed), 1);
      pool.hold_ct0(1, "fresh".to_owned());

      assert_eq!(
         pool
            .append_sessions(vec![
               session(r#"{"id":1,"username":"replacement","pool":"search","ct0":"stale"}"#),
               session(
                  r#"{"id":2,"username":"second","kind":"cookie","auth_token":"b","ct0":"initial"}"#
               ),
            ])
            .await,
         1
      );
      let after = pool.slots();
      assert!(std::sync::Arc::ptr_eq(&original[0], &after[0]));
      assert_eq!(pool.credentials(1).unwrap().username, "first");
      assert_eq!(pool.ct0(1).as_deref(), Some("fresh"));
      assert_eq!(after[0].permits.available_permits(), 0);
      assert_eq!(after[0].wanted.load(super::Ordering::Relaxed), 1);
      let second = pool.acquire("TweetDetail", None).await.unwrap();
      assert_eq!(second.id, 2);
      drop(first);
      assert_eq!(
         timeout(Duration::from_millis(200), queued)
            .await
            .unwrap()
            .unwrap()
            .id,
         1
      );
      assert_eq!(after[0].wanted.load(super::Ordering::Relaxed), 0);
   }

   #[tokio::test]
   async fn a_full_account_hands_the_next_call_over_immediately() {
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
      pool
         .update_session_limit(1, "TweetDetail", 50, 3, reset)
         .await;
      pool
         .update_session_limit(2, "TweetDetail", 50, 40, reset)
         .await;
      let got = pool
         .acquire("TweetDetail", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(got.id, 2, "the fuller account is used before the thin one");

      pool
         .update_session_limit(2, "TweetDetail", 50, 1, reset)
         .await;
      let next = pool
         .acquire("TweetDetail", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(
         next.id, 1,
         "the account with more calls left is still preferred"
      );

      drop(got);
      drop(next);
      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_full_pool_takes_whichever_account_frees() {
      let path = env::temp_dir().join(format!("teapot-session-pool-frees-{}.jsonl", process::id()));
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

   /// Two accounts, one slot each. Account 1 cannot look up profiles.
   async fn profile_pool(name: &str) -> (SessionPool, std::path::PathBuf) {
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-{name}-{}.jsonl",
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
      pool.mark_refused(1, "x/UserByScreenName").await;
      (pool, path)
   }

   #[tokio::test]
   async fn a_release_wakes_a_call_that_can_use_that_account() {
      let (pool, path) = profile_pool("own-wake").await;
      let lookup = pool.acquire("x/UserByScreenName", None).await.unwrap();
      assert_eq!(lookup.id, 2);
      let detail = pool.acquire("x/TweetDetail", None).await.unwrap();
      assert_eq!(detail.id, 1);

      // Parked first, on account 2 only. One shared wake-up went to it when
      // account 1 freed, and the call that could use account 1 slept on.
      let profile_pool = pool.clone();
      let profile =
         tokio::spawn(async move { profile_pool.acquire("x/UserByScreenName", None).await });
      tokio::time::sleep(Duration::from_millis(30)).await;
      let detail_pool = pool.clone();
      let next_detail =
         tokio::spawn(async move { detail_pool.acquire("x/TweetDetail", None).await });
      tokio::time::sleep(Duration::from_millis(30)).await;

      drop(detail);
      let next_detail = timeout(Duration::from_millis(300), next_detail)
         .await
         .expect("the call that can use the freed account is the one woken")
         .unwrap()
         .unwrap();
      assert_eq!(next_detail.id, 1);
      assert!(!profile.is_finished(), "account 2 is still mid-call");

      drop(lookup);
      let profile = timeout(Duration::from_millis(300), profile)
         .await
         .expect("account 2 freeing wakes the profile lookup")
         .unwrap()
         .unwrap();
      assert_eq!(profile.id, 2);

      drop(next_detail);
      drop(profile);
      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_new_call_leaves_the_account_a_parked_call_waits_on() {
      let (pool, path) = profile_pool("held").await;
      let lookup = pool.acquire("x/UserByScreenName", None).await.unwrap();
      let detail = pool.acquire("x/TweetDetail", None).await.unwrap();

      let profile_pool = pool.clone();
      let profile =
         tokio::spawn(async move { profile_pool.acquire("x/UserByScreenName", None).await });
      tokio::time::sleep(Duration::from_millis(30)).await;

      // Account 2 frees while the profile lookup is still parked on it. A
      // call that could go anywhere does not take the only account the
      // lookup can use.
      drop(lookup);
      let barged = pool.acquire_free("x/TweetDetail", None).await.unwrap();
      assert!(barged.is_none(), "account 2 is held for the parked lookup");

      let profile = timeout(Duration::from_millis(300), profile)
         .await
         .expect("the parked lookup gets the account it waited on")
         .unwrap()
         .unwrap();
      assert_eq!(profile.id, 2);

      drop(detail);
      drop(profile);
      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_free_pick_steps_aside_instead_of_queueing() {
      let path = env::temp_dir().join(format!("teapot-session-pool-free-{}.jsonl", process::id()));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"one","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 1).await.unwrap();

      let held = pool.acquire_free("x/UserTweets", None).await.unwrap();
      assert!(held.is_some(), "an idle account is taken at once");
      let busy = timeout(
         Duration::from_millis(50),
         pool.acquire_free("x/UserTweets", None),
      )
      .await
      .expect("a busy pool answers at once instead of waiting for the call to end")
      .unwrap();
      assert!(busy.is_none());

      drop(held);
      let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 900;
      pool
         .update_session_limit(1, "x/UserTweets", 50, 0, reset)
         .await;
      assert!(matches!(
         pool.acquire_free("x/UserTweets", None).await,
         Err(Error::RateLimited)
      ));

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_call_stuck_behind_a_held_account_is_turned_away_not_kept() {
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-shed-wait-{}.jsonl",
         process::id()
      ));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"one","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 1).await.unwrap();

      let held = pool.acquire("x/UserByScreenName", None).await.unwrap();
      let started = Instant::now();
      let waited = timeout(
         MAX_PARK + Duration::from_secs(1),
         pool.acquire("x/UserByScreenName", None),
      )
      .await
      .expect("a parked call gives up at its limit instead of the page timeout");
      assert!(matches!(waited, Err(Error::TransientUpstream)));
      assert!(started.elapsed() >= MAX_PARK);

      drop(held);
      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_line_too_long_to_clear_is_not_joined() {
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-shed-line-{}.jsonl",
         process::id()
      ));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"one","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 1).await.unwrap();

      let held = pool.acquire("x/UserByScreenName", None).await.unwrap();
      let parked = (0..QUEUE_PER_SLOT)
         .map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move { pool.acquire("x/UserByScreenName", None).await })
         })
         .collect::<Vec<_>>();
      tokio::time::sleep(Duration::from_millis(50)).await;

      let late = timeout(
         Duration::from_millis(100),
         pool.acquire("x/UserByScreenName", None),
      )
      .await
      .expect("a full line answers at once");
      assert!(matches!(late, Err(Error::TransientUpstream)));

      // The calls already in line still get the account as it frees.
      drop(held);
      timeout(Duration::from_millis(500), async {
         while !parked.iter().any(tokio::task::JoinHandle::is_finished) {
            tokio::time::sleep(Duration::from_millis(5)).await;
         }
      })
      .await
      .expect("a parked call is woken when the account frees");
      for task in parked {
         task.abort();
      }
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

      let held = pool
         .acquire("UserTweets", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(held.id, 1);
      let next = pool.acquire("UserTweets", Some(SessionKind::Cookie)).await;
      assert!(matches!(next, Err(Error::RateLimited)));
      drop(held);

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
      pool.mark_verified(2, "old-id/SearchTimeline").await;
      pool.mark_refused(1, "old-id/SearchTimeline").await;

      for _ in 0..4 {
         let lease = pool
            .acquire("new-id/SearchTimeline", Some(SessionKind::Cookie))
            .await
            .unwrap();
         assert_eq!(lease.id, 2);
      }
      // Account 1 was refused before it was reserved, so it stays a viewer.
      // Account 2 can search and is no longer available for views.
      assert_eq!(pool.available_for("new-id/SearchTimeline").await, 1);
      assert_eq!(pool.available_for("x/UserTweets").await, 1);

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn accounts_known_to_search_go_first() {
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
      assert_eq!(
         pool.unchecked_for("b/SearchTimeline").await,
         Vec::<i64>::new()
      );

      for _ in 0..5 {
         let lease = pool
            .acquire("b/SearchTimeline", Some(SessionKind::Cookie))
            .await
            .unwrap();
         assert_eq!(lease.id, 2);
      }
      // The account that can search is no longer in the view pool.
      assert!(
         pool
            .acquire("x/UserTweets", Some(SessionKind::Cookie))
            .await
            .is_err()
            || pool
               .acquire_excluding("x/UserTweets", Some(SessionKind::Cookie), Some(2))
               .await
               .unwrap()
               .id
               != 2
      );
      let view = pool
         .acquire("x/UserTweets", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_ne!(view.id, 2);
      drop(view);
      assert!(
         pool
            .acquire_excluding("b/SearchTimeline", Some(SessionKind::Cookie), Some(2))
            .await
            .is_err()
      );

      pool.mark_refused(2, "b/SearchTimeline").await;
      assert!(pool.unchecked_for("b/SearchTimeline").await.is_empty());
      assert!(
         pool
            .acquire("b/SearchTimeline", Some(SessionKind::Cookie))
            .await
            .is_err()
      );
      // A refused search account is not put back into views. Mixing it back
      // is how a search suspension spreads to the accounts people browse with.
      for _ in 0..4 {
         let back = pool
            .acquire("x/UserTweets", Some(SessionKind::Cookie))
            .await
            .unwrap();
         assert_ne!(back.id, 2);
      }

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn ambiguous_search_404_quarantines_once_and_only_confirmation_moves_the_account() {
      let path = env::temp_dir().join(format!("teapot-search-404-{}.jsonl", process::id()));
      let search = "id/SearchTimeline";
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"first","kind":"cookie","auth_token":"a","ct0":"c","pool":"search"}"#,
            "\n",
            r#"{"id":2,"username":"second","kind":"cookie","auth_token":"b","ct0":"d","pool":"search"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      let (first, duplicate) = tokio::join!(pool.record_search_404(1), pool.record_search_404(1));
      assert_ne!(first, duplicate);
      pool.mark_verified(1, search).await;
      assert!(pool.acquire_id(1, search).await.is_err());
      assert!(pool.acquire_id(1, "id/UserTweets").await.is_err());
      assert!(pool.acquire_id(1, "account/settings").await.is_ok());
      assert_eq!(pool.acquire_id(2, search).await.unwrap().id, 2);
      assert!(pool.acquire(search, None).await.is_err());
      assert!(pool.acquire_free(search, None).await.is_err());
      assert!(pool.try_acquire_where(search, None, |_| true).is_none());
      pool.mark_verified(2, search).await;
      assert_eq!(pool.acquire(search, None).await.unwrap().id, 2);
      pool
         .demote_to_view(
            2,
            Some(crate::stats::Restriction::SearchBanned),
            "search refused",
         )
         .await;
      assert!(pool.acquire(search, None).await.is_err());
      assert!(pool.acquire_id(2, search).await.is_err());

      assert!(pool.acquire_id(1, search).await.is_err());
      assert!(pool.acquire_id(1, "id/UserTweets").await.is_err());
      pool.slot(1).unwrap().write_limits(|lim| {
         lim.search_404_at = Some(0);
         lim.refused_ops.clear();
      });
      assert!(pool.acquire_id(1, search).await.is_ok());
      pool
         .demote_to_view(
            1,
            Some(crate::stats::Restriction::SearchBanned),
            "search refused",
         )
         .await;
      assert!(
         pool
            .slots()
            .iter()
            .all(|slot| slot.read_limits().search_banned)
      );

      let reloaded = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      for pool in [&pool, &reloaded] {
         for id in [1, 2] {
            assert!(pool.acquire_id(id, search).await.is_err());
            assert!(pool.acquire_id(id, "id/UserTweets").await.is_ok());
         }
      }
      fs::remove_file(path).await.unwrap();
   }

   /// A refused settings write is not a verdict on search.
   ///
   /// X answers `account/settings` with 403 code 64 for accounts that search
   /// fine — 10 out of 10 sampled answered `SearchTimeline` with 200 and posts.
   /// Reading that refusal as "suspended" took 2,000 accounts out of the search
   /// pool and left the site returning rate-limit errors with nothing left to
   /// search with. Only a real data call may bench an account.
   #[tokio::test]
   async fn a_refused_settings_write_does_not_take_the_account_out_of_search() {
      let path = env::temp_dir().join(format!("teapot-settings-probe-{}.jsonl", process::id()));
      fs::write(
         &path,
         r#"{"id":1,"username":"searching","kind":"cookie","auth_token":"a","ct0":"c","pool":"search"}
"#,
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      let search = "id/SearchTimeline";
      pool.mark_verified(1, search).await;

      pool
         .note_suspended(
            1,
            SuspensionProbe::Settings,
            None,
            "Settings write refused: account suspended",
         )
         .await;

      // It keeps searching, which is the whole point.
      assert!(pool.acquire_id(1, search).await.is_ok());
      // The write is paced instead, so the rescan does not repeat it today.
      let slot = pool.slot(1).unwrap();
      let lim = slot.read_limits();
      assert!(lim.filters_failed);
      assert!(lim.filters_retry_at > 0);
      // And no search verdict was written at all.
      assert!(!lim.suspended);
      assert!(!lim.search_banned);
      assert!(lim.search_only);
      drop(lim);

      fs::remove_file(path).await.unwrap();
   }

   /// A refused data call does bench the account: X said it may not act on the
   /// path the caller asked for, which is a real verdict.
   #[tokio::test]
   async fn a_refused_search_call_moves_the_account_to_the_view_pool() {
      let path = env::temp_dir().join(format!("teapot-data-probe-{}.jsonl", process::id()));
      fs::write(
         &path,
         r#"{"id":1,"username":"searching","kind":"cookie","auth_token":"a","ct0":"c","pool":"search"}
"#,
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      let search = "id/SearchTimeline";
      pool.mark_verified(1, search).await;

      pool
         .note_suspended(1, SuspensionProbe::Data, None, "Account suspended")
         .await;

      assert!(pool.acquire_id(1, search).await.is_err());
      // Kept for profile reads rather than thrown away.
      assert!(pool.acquire_id(1, "id/UserTweets").await.is_ok());
      let slot = pool.slot(1).unwrap();
      let lim = slot.read_limits();
      assert!(lim.suspended);
      assert!(lim.search_banned);
      assert!(!lim.retired);
      drop(lim);

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_suspended_search_account_moves_to_the_view_pool_and_stays_usable() {
      let path = env::temp_dir().join(format!("teapot-suspend-view-{}.jsonl", process::id()));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"searching","kind":"cookie","auth_token":"a","ct0":"c","pool":"search"}"#,
            "\n",
            r#"{"id":2,"username":"viewing","kind":"cookie","auth_token":"b","ct0":"d"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      let search = "id/SearchTimeline";

      // The search account is reserved for search, so it never takes a view call.
      assert!(pool.acquire_id(1, "id/UserTweets").await.is_err());

      pool
         .demote_to_view(1, None, "Account suspended; moved to the view pool")
         .await;

      // It leaves search for good...
      assert!(pool.acquire_id(1, search).await.is_err());
      assert!(pool.acquire(search, None).await.is_err());
      // ...and the view pool picks it up, which is the point of keeping it.
      assert!(pool.acquire_id(1, "id/UserTweets").await.is_ok());

      // A view account that is refused keeps working for reads; the verdict is
      // already on record as `search_banned`, and no store write is spent on an
      // account that is already in the pool it would move to.
      pool
         .demote_to_view(2, None, "Account suspended; moved to the view pool")
         .await;
      assert!(pool.slot(2).unwrap().read_limits().search_banned);
      assert!(pool.acquire_id(2, "id/UserTweets").await.is_ok());

      // The split survives a restart.
      let reloaded = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      assert!(reloaded.acquire_id(1, search).await.is_err());
      assert!(reloaded.acquire_id(1, "id/UserTweets").await.is_ok());

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_spare_account_is_not_waited_for_behind_a_queued_first_copy() {
      let path = env::temp_dir().join(format!("teapot-session-pool-spare-{}.jsonl", process::id()));
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

      // Holding account 1's per-slot lock never blocks a hedge from claiming
      // account 2.
      let slot1 = pool.slot(1).unwrap();
      let busy1 = slot1.limits.write().unwrap();
      let spare = pool
         .try_acquire_where("TweetDetail", Some(SessionKind::Cookie), |id| id != 1)
         .expect("a free spare on another account does not wait on account 1");
      assert_eq!(spare.id, 2);
      drop(spare);
      drop(busy1);

      // Holding account 2's per-slot lock makes `try_acquire_where` return
      // `None` immediately rather than blocking on it.
      let slot2 = pool.slot(2).unwrap();
      let busy2 = slot2.limits.write().unwrap();
      assert!(
         pool
            .try_acquire_where("TweetDetail", Some(SessionKind::Cookie), |id| id != 1)
            .is_none()
      );
      drop(busy2);
      let spare = pool
         .try_acquire_where("TweetDetail", Some(SessionKind::Cookie), |id| id != 1)
         .expect("a free spare once its slot lock is free");
      assert_eq!(spare.id, 2);

      drop(spare);
      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn accounts_on_a_slow_exit_go_after_the_quick_ones() {
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-slow-exit-{}.jsonl",
         process::id()
      ));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"far","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n",
            r#"{"id":2,"username":"near","kind":"cookie","auth_token":"b","ct0":"d"}"#,
            "\n",
            r#"{"id":3,"username":"near-too","kind":"cookie","auth_token":"e","ct0":"f"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      assert_eq!(pool.mark_slow_exits(|id| id == 1), 1);

      // Known windows: quick exits with at least SLOW_EXIT_FAST_FLOOR calls
      // stay strictly ahead of slow exits, and the slow exit's 40 calls rank
      // as 10 (discounted by SLOW_EXIT_DISCOUNT = 4).
      let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 900;
      pool
         .update_session_limit(1, "UserTweets", 50, 40, reset)
         .await;
      pool
         .update_session_limit(2, "UserTweets", 50, 25, reset)
         .await;
      pool
         .update_session_limit(3, "UserTweets", 50, 10, reset)
         .await;
      let fuller_quick = pool
         .acquire("UserTweets", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(fuller_quick.id, 2);
      drop(fuller_quick);
      pool
         .update_session_limit(2, "UserTweets", 50, 15, reset)
         .await;
      let still_quick = pool
         .acquire("UserTweets", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(
         still_quick.id, 2,
         "a quick exit with ample budget still beats a slow exit"
      );
      drop(still_quick);
      pool
         .update_session_limit(2, "UserTweets", 50, 4, reset)
         .await;
      pool
         .update_session_limit(3, "UserTweets", 50, 3, reset)
         .await;
      let slow_fullest = pool
         .acquire("UserTweets", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(
         slow_fullest.id, 1,
         "a slow exit still serves once the quick ones run low"
      );
      drop(slow_fullest);

      // No window yet: the slow exit's warm connection loses to a quick exit.
      pool.update_session_limit(1, "Other", 50, 50, 0).await;
      pool.slots()[1]
         .last_active
         .store(0, super::Ordering::Relaxed);
      pool.slots()[2]
         .last_active
         .store(0, super::Ordering::Relaxed);
      for _ in 0..4 {
         let unmeasured = pool
            .acquire("TweetDetail", Some(SessionKind::Cookie))
            .await
            .unwrap();
         assert_ne!(unmeasured.id, 1);
      }

      // A hedge wants the quickest path, whatever is left.
      pool
         .update_session_limit(1, "TweetDetail", 150, 140, reset)
         .await;
      pool
         .update_session_limit(2, "TweetDetail", 150, 30, reset)
         .await;
      let spare = pool
         .try_acquire_where("TweetDetail", Some(SessionKind::Cookie), |id| id != 3)
         .unwrap();
      assert_eq!(spare.id, 2);
      drop(spare);

      assert_eq!(pool.mark_slow_exits(|_| false), 0);
      let caught_up = pool
         .acquire("UserTweets", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(
         caught_up.id, 1,
         "back in step, the fullest account goes first again"
      );
      drop(caught_up);

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_warm_account_beats_a_cold_one_for_a_new_endpoint_and_hedge() {
      let path = env::temp_dir().join(format!("teapot-session-pool-warm-{}.jsonl", process::id()));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"cold","kind":"cookie","auth_token":"a","ct0":"c"}"#,
            "\n",
            r#"{"id":2,"username":"warm","kind":"cookie","auth_token":"b","ct0":"d"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 900;
      // Session 2 just answered `UserByScreenName`, so its HTTP/2 connection is warm.
      pool
         .update_session_limit(2, "UserByScreenName", 95, 94, reset)
         .await;

      // For `UserTweets` (where both have Unknown headroom), the warm session wins.
      for _ in 0..4 {
         let lease = pool
            .acquire("UserTweets", Some(SessionKind::Cookie))
            .await
            .unwrap();
         assert_eq!(lease.id, 2);
      }

      // And for a hedge (`try_acquire_where`), a warm Safe session beats a cold Safe
      // session.
      pool
         .update_session_limit(1, "TweetDetail", 150, 140, reset)
         .await;
      pool.slots()[0]
         .last_active
         .store(0, super::Ordering::Relaxed);
      pool
         .update_session_limit(2, "TweetDetail", 150, 100, reset)
         .await;
      let spare = pool
         .try_acquire_where("TweetDetail", Some(SessionKind::Cookie), |_| true)
         .unwrap();
      assert_eq!(spare.id, 2);
      drop(spare);

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn a_warm_account_with_headroom_beats_a_cold_one_on_fresh_acquire() {
      let pool = SessionPool::empty(4);
      let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 900;
      let sessions = vec![
         crate::types::Session {
            id: 1,
            username: "cold".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a".to_owned(),
            ct0: "c".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 2,
            username: "warm".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "b".to_owned(),
            ct0: "d".to_owned(),
            ..crate::types::Session::default()
         },
      ];
      assert_eq!(pool.append_sessions(sessions).await, 2);

      pool
         .update_session_limit(1, "TweetDetail", 150, 140, reset)
         .await;
      pool.slots()[0]
         .last_active
         .store(0, super::Ordering::Relaxed);
      pool
         .update_session_limit(2, "TweetDetail", 150, 20, reset)
         .await;

      let warm = pool
         .acquire("TweetDetail", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(
         warm.id, 2,
         "warm connection with >= WARM_HEADROOM_FLOOR calls beats cold account"
      );
      drop(warm);

      pool
         .update_session_limit(2, "TweetDetail", 150, 10, reset)
         .await;
      let fuller_cold = pool
         .acquire("TweetDetail", Some(SessionKind::Cookie))
         .await
         .unwrap();
      assert_eq!(
         fuller_cold.id, 1,
         "once warm account falls below WARM_HEADROOM_FLOOR, fullest account wins"
      );
      drop(fuller_cold);
   }

   #[tokio::test]
   async fn bounded_scan_still_finds_quick_and_only_available_accounts() {
      let pool = SessionPool::empty(2);
      let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 900;
      let mut sessions = Vec::with_capacity(200);
      for id in 1..=200_i64 {
         let mut apis = std::collections::HashMap::new();
         apis.insert("x/UserTweets".to_owned(), crate::types::RateLimit {
            limit: 150,
            remaining: 100,
            reset,
         });
         apis.insert("x/TweetDetail".to_owned(), crate::types::RateLimit {
            limit: 150,
            remaining: if id == 200 { 50 } else { 0 },
            reset,
         });
         sessions.push(crate::types::Session {
            id,
            username: format!("u{id}"),
            kind: SessionKind::Cookie,
            auth_token: "tok".to_owned(),
            ct0: "ct0".to_owned(),
            apis,
            ..crate::types::Session::default()
         });
      }
      assert_eq!(pool.append_sessions(sessions).await, 200);

      // Even when 180 of 200 accounts (> SCAN_PROBES) are on slow exits, the
      // bounded scan must keep scanning and always pick a quick exit.
      assert_eq!(pool.mark_slow_exits(|id| id <= 180), 180);
      for _ in 0..10 {
         let lease = pool
            .acquire("x/UserTweets", Some(SessionKind::Cookie))
            .await
            .unwrap();
         assert!(lease.id > 180, "picked slow exit account {}", lease.id);
      }

      // Even when 199 of 200 accounts are spent, the scan must still find the
      // lone available account rather than falsely returning RateLimited.
      assert_eq!(pool.mark_slow_exits(|_| false), 0);
      for _ in 0..5 {
         let lease = pool
            .acquire("x/TweetDetail", Some(SessionKind::Cookie))
            .await
            .unwrap();
         assert_eq!(lease.id, 200);
      }
   }

   #[tokio::test]
   async fn demoted_search_lifecycle_and_pool_overrides() {
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-demoted-lifecycle-{}.jsonl",
         process::id()
      ));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"s1","kind":"cookie","auth_token":"a","ct0":"c","pool":"search"}"#,
            "\n",
            r#"{"id":2,"username":"s2","kind":"cookie","auth_token":"b","ct0":"d","pool":"search"}"#,
            "\n",
            r#"{"id":3,"username":"v1","kind":"cookie","auth_token":"e","ct0":"f"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      pool.mark_verified(1, "id/SearchTimeline").await;

      {
         let now = time::OffsetDateTime::now_utc().unix_timestamp();
         assert!(pool.pool_line("UserTweets", now).starts_with(
            "UserTweets accounts: 1 free, 0 mid-call, 0 held for parked calls, 0 resting after a \
             bare 429, 0 spent, 0 out;"
         ));
         assert!(pool.pool_line("SearchTimeline", now).starts_with(
            "SearchTimeline accounts: 1 free, 0 mid-call, 0 held for parked calls, 0 resting \
             after a bare 429, 0 spent, 1 out;"
         ));
      }

      pool
         .note_suspended(1, SuspensionProbe::Data, None, "Account suspended")
         .await;
      pool.demote_to_view(2, None, "search refused").await;

      let states = pool.account_states(&[1, 2, 3, 999]).await;
      assert_eq!(states.len(), 3);
      assert_eq!(states[&1].pool, "view");
      assert!(states[&1].demoted_from_search);
      assert!(states[&1].suspended);
      assert_eq!(states[&2].pool, "view");
      assert!(states[&2].demoted_from_search);
      assert_eq!(states[&3].pool, "view");
      assert!(!states[&3].demoted_from_search);

      assert_eq!(pool.demoted_search_candidates(1).await, vec![1]);
      assert_eq!(pool.demoted_search_candidates(1).await, vec![2]);

      assert!(pool.reinstate(1).await);
      assert!(!pool.reinstate(1).await);
      let s1 = pool.account_state(1).await.unwrap();
      assert_eq!(s1.pool, "search");
      assert!(!s1.demoted_from_search);
      assert!(!s1.suspended);
      assert!(!s1.search_banned);

      pool.clear_demoted_from_search(2).await;
      assert!(!pool.account_state(2).await.unwrap().demoted_from_search);
      assert!(pool.demoted_search_candidates(10).await.is_empty());

      assert!(matches!(
         pool.set_account_pool(999, super::AccountPool::Search).await,
         Err(Error::NotFound(_))
      ));
      assert!(!pool.set_account_pool(3, super::AccountPool::View).await.unwrap());
      assert!(pool.set_account_pool(3, super::AccountPool::Search).await.unwrap());
      assert_eq!(pool.account_state(3).await.unwrap().pool, "search");
      assert!(!pool.set_account_pool(3, super::AccountPool::Search).await.unwrap());
      assert!(
         pool
            .set_account_pool(3, super::AccountPool::SearchPass)
            .await
            .unwrap()
      );
      assert_eq!(pool.account_state(3).await.unwrap().pool, "search-pass");
      assert!(
         !pool
            .set_account_pool(3, super::AccountPool::SearchPass)
            .await
            .unwrap()
      );
      assert!(pool.set_account_pool(3, super::AccountPool::View).await.unwrap());
      assert_eq!(pool.account_state(3).await.unwrap().pool, "view");

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test]
   async fn search_outcome_tracker_counts_streaks_resets_and_excludes_tried_ids() {
      let path = env::temp_dir().join(format!(
         "teapot-session-pool-empty-tracker-{}.jsonl",
         process::id()
      ));
      fs::write(
         &path,
         concat!(
            r#"{"id":1,"username":"broken","kind":"cookie","auth_token":"a","ct0":"c","pool":"search"}"#,
            "\n",
            r#"{"id":2,"username":"healthy","kind":"cookie","auth_token":"b","ct0":"d","pool":"search"}"#,
            "\n"
         ),
      )
      .await
      .unwrap();
      let pool = SessionPool::load(path.to_str().unwrap(), 4).await.unwrap();
      let api = "id/SearchTimeline";
      pool.mark_verified(1, api).await;
      pool.mark_verified(2, api).await;

      assert!(pool.has_unlimited_excluding(api, None, &[1]).await);
      assert!(!pool.has_unlimited_excluding(api, None, &[1, 2]).await);

      for _ in 0..super::SEARCH_EMPTY_STREAK_ALERT {
         pool
            .record_search_outcome(1, api, crate::stats::SearchAccountOutcome::Empty)
            .await;
      }
      pool
         .record_search_outcome(1, api, crate::stats::SearchAccountOutcome::RateLimited)
         .await;

      let s1 = pool.account_state(1).await.unwrap();
      assert_eq!(s1.search_empty_streak, super::SEARCH_EMPTY_STREAK_ALERT);
      assert_eq!(
         s1.search_empty_total,
         u64::from(super::SEARCH_EMPTY_STREAK_ALERT)
      );
      assert_eq!(s1.search_non_empty_total, 0);
      assert_eq!(s1.search_rate_limited_total, 1);
      assert!(s1.last_search_empty.is_some());
      assert_eq!(
         pool.search_empty_streak_ids(super::SEARCH_EMPTY_STREAK_ALERT),
         vec![1]
      );

      pool
         .record_search_outcome(1, api, crate::stats::SearchAccountOutcome::NonEmpty)
         .await;
      let s1_after = pool.account_state(1).await.unwrap();
      assert_eq!(s1_after.search_empty_streak, 0);
      assert_eq!(s1_after.search_non_empty_total, 1);
      assert!(
         pool
            .search_empty_streak_ids(super::SEARCH_EMPTY_STREAK_ALERT)
            .is_empty()
      );

      fs::remove_file(path).await.unwrap();
   }

   #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
   async fn concurrent_acquire_and_update_stays_fast_at_fleet_scale() {
      let pool = SessionPool::empty(2);
      let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 900;
      let endpoints = [
         "abc1234567890abcdef123/UserByScreenName",
         "def1234567890abcdef123/TweetDetail",
         "ghi1234567890abcdef123/UserTweets",
         "jkl1234567890abcdef123/UserMedia",
         "mno1234567890abcdef123/Retweeters",
         "pqr1234567890abcdef123/UserOriginalsTimeline",
         "stu1234567890abcdef123/UserRepostsTimeline",
         "vwx1234567890abcdef123/UserTweetsAndReplies",
         "yza1234567890abcdef123/UserHighlightsTweets",
         "bcd1234567890abcdef123/TweetEditHistory",
         "efg1234567890abcdef123/AudioSpaceById",
         "hij1234567890abcdef123/ListByRestId",
      ];
      let mut sessions = Vec::with_capacity(5440);
      for id in 1..=5440_i64 {
         let search = id <= 546;
         let mut apis = std::collections::HashMap::with_capacity(endpoints.len());
         for &ep in &endpoints {
            apis.insert(ep.to_owned(), crate::types::RateLimit {
               limit: 150,
               remaining: 50 + (id % 100) as i32,
               reset,
            });
         }
         sessions.push(crate::types::Session {
            id,
            username: format!("u{id}"),
            kind: SessionKind::Cookie,
            auth_token: "tok".to_owned(),
            ct0: "ct0".to_owned(),
            pool: if search {
               "search".to_owned()
            } else {
               String::new()
            },
            apis,
            ..crate::types::Session::default()
         });
      }
      assert_eq!(pool.append_sessions(sessions).await, 5440);
      for id in 547..=(547 + 780) {
         pool.mark_ct0_dead(id).await;
      }

      let started = Instant::now();
      let mut tasks = Vec::new();
      for worker in 0..16_usize {
         let pool = pool.clone();
         let ep = endpoints[worker % 4];
         tasks.push(tokio::spawn(async move {
            let mut latencies_us = Vec::with_capacity(200);
            for _ in 0..200 {
               let t0 = Instant::now();
               let lease = pool.acquire(ep, Some(SessionKind::Cookie)).await.unwrap();
               let us = t0.elapsed().as_micros() as u64;
               latencies_us.push(us);
               let id = lease.id;
               pool.update_session_limit(id, ep, 150, 120, reset).await;
               pool.mark_verified(id, ep).await;
               drop(lease);
            }
            latencies_us
         }));
      }
      // Exercise concurrent reads (census / queue_report / cookie_header) beside
      // acquires.
      let reader_pool = pool.clone();
      let reader = tokio::spawn(async move {
         for _ in 0..5 {
            let _ = reader_pool.census().await;
            let _ = reader_pool.cookie_header();
         }
      });

      let mut all = Vec::with_capacity(16 * 200);
      for task in tasks {
         all.extend(task.await.unwrap());
      }
      reader.await.unwrap();
      let elapsed = started.elapsed();
      all.sort_unstable();
      let p50 = all[all.len() / 2];
      let p99 = all[all.len() * 99 / 100];
      let max = *all.last().unwrap();
      let qps = (all.len() as f64) / elapsed.as_secs_f64();
      eprintln!(
         "fleet-scale 5440 accounts: {} ops in {:?} ({:.0} ops/s), acquire p50={}us p99={}us \
          max={}us",
         all.len(),
         elapsed,
         qps,
         p50,
         p99,
         max
      );
      // Even in debug/dev opt-level=1, each acquire must stay well below 100 ms.
      assert!(p99 < 100_000, "p99 acquire latency too high: {p99} us");
   }

   #[tokio::test]
   async fn search_spreads_across_expired_and_warm_accounts_instead_of_pinning_warm() {
      let pool = SessionPool::empty(2);
      let api = "ph2f/SearchTimeline";
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      let sessions = (1..=3_i64)
         .map(|id| {
            crate::types::Session {
               id,
               username: format!("s{id}"),
               kind: SessionKind::Cookie,
               auth_token: "tok".to_owned(),
               ct0: "ct0".to_owned(),
               pool: "search".to_owned(),
               ..crate::types::Session::default()
            }
         })
         .collect();
      assert_eq!(pool.append_sessions(sessions).await, 3);
      for id in 1..=3 {
         pool.mark_verified(id, api).await;
      }

      // Account 1 is active and warm with 160 remaining.
      pool.update_session_limit(1, api, 187, 160, now + 900).await;
      // Account 2 has an expired window (full 187 ready) and is cold.
      pool.update_session_limit(2, api, 187, 0, now - 10).await;
      pool.slots()[1]
         .last_active
         .store(0, super::Ordering::Relaxed);
      // Account 3 has no recorded window yet and is cold.
      pool.slots()[2]
         .last_active
         .store(0, super::Ordering::Relaxed);

      // Expired/fresh accounts (187) must be chosen before hammering warm account 1
      // (160).
      let first = pool.acquire(api, Some(SessionKind::Cookie)).await.unwrap();
      assert_ne!(
         first.id, 1,
         "expired/unknown full search account must beat warm account with 160 left"
      );
      let second = pool.acquire(api, Some(SessionKind::Cookie)).await.unwrap();
      assert_ne!(second.id, 1);
      assert_ne!(
         second.id, first.id,
         "concurrent searches should fan out to idle full accounts"
      );
      drop((first, second));
   }

   #[tokio::test]
   async fn corroborated_search_failures_rest_first_then_demote_on_distinct_later_query() {
      let pool = SessionPool::empty(2);
      let api = "ph2f/SearchTimeline";
      let sessions = vec![
         crate::types::Session {
            id: 1,
            username: "suspended_s1".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a".to_owned(),
            ct0: "c".to_owned(),
            pool: "search".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 2,
            username: "healthy_s2".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "b".to_owned(),
            ct0: "d".to_owned(),
            pool: "search".to_owned(),
            ..crate::types::Session::default()
         },
      ];
      assert_eq!(pool.append_sessions(sessions).await, 2);
      pool.mark_verified(1, api).await;
      pool.mark_verified(2, api).await;

      let now = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap();
      // Strike 1 on query A rests account 1 without demoting it or stripping
      // verified_ops.
      assert_eq!(
         pool
            .record_search_timeline_failure_at(1, api, 111, true, now)
            .await,
         SearchFailureAction::Rested
      );
      let s1 = pool.account_state(1).await.unwrap();
      assert_eq!(s1.pool, "search");
      assert!(!s1.search_banned);
      assert!(s1.verified_operations.contains_key("SearchTimeline"));
      // While resting, acquires route exclusively to healthy account 2.
      let lease = pool.acquire(api, Some(SessionKind::Cookie)).await.unwrap();
      assert_eq!(lease.id, 2);
      drop(lease);

      // Repeating the SAME query after the min gap does NOT double-strike.
      assert_eq!(
         pool
            .record_search_timeline_failure_at(1, api, 111, true, now + 20)
            .await,
         SearchFailureAction::Rested
      );
      // A different query too soon (< 10s after strike 1) does NOT double-strike
      // either.
      assert_eq!(
         pool
            .record_search_timeline_failure_at(1, api, 222, true, now + 3)
            .await,
         SearchFailureAction::Rested
      );
      assert_eq!(pool.account_state(1).await.unwrap().pool, "search");

      // A different query after the min gap demotes the broken account to view.
      assert_eq!(
         pool
            .record_search_timeline_failure_at(1, api, 222, true, now + 15)
            .await,
         SearchFailureAction::Demoted
      );
      let demoted = pool.account_state(1).await.unwrap();
      assert_eq!(demoted.pool, "view");
      assert!(demoted.demoted_from_search);
      assert!(demoted.search_banned);

      // Reinstating clears rest and failure state so it can search again.
      assert!(pool.reinstate(1).await);
      pool.mark_verified(1, api).await;
      assert_eq!(pool.account_state(1).await.unwrap().pool, "search");
      assert!(pool.acquire_id(1, api).await.is_ok());
   }

   #[tokio::test]
   async fn tweet_search_success_resets_corroborated_failure_streak() {
      let pool = SessionPool::empty(2);
      let api = "ph2f/SearchTimeline";
      let sessions = vec![crate::types::Session {
         id: 1,
         username: "healthy_s1".to_owned(),
         kind: SessionKind::Cookie,
         auth_token: "a".to_owned(),
         ct0: "c".to_owned(),
         pool: "search".to_owned(),
         ..crate::types::Session::default()
      }];
      assert_eq!(pool.append_sessions(sessions).await, 1);
      pool.mark_verified(1, api).await;

      let now = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap();
      assert_eq!(
         pool
            .record_search_timeline_failure_at(1, api, 111, true, now)
            .await,
         SearchFailureAction::Rested
      );
      // Answering a tweet search clears the streak.
      pool.record_tweet_search_ok_at(1, true, now + 130);
      // A later corroborated failure starts fresh at strike 1, not demotion.
      assert_eq!(
         pool
            .record_search_timeline_failure_at(1, api, 222, true, now + 150)
            .await,
         SearchFailureAction::Rested
      );
      assert_eq!(pool.account_state(1).await.unwrap().pool, "search");
   }

   #[tokio::test]
   async fn uncorroborated_failures_never_demote_directly_and_canary_only_when_pool_healthy() {
      let pool = SessionPool::empty(2);
      let api = "ph2f/SearchTimeline";
      let sessions = vec![
         crate::types::Session {
            id: 1,
            username: "s1".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a".to_owned(),
            ct0: "c".to_owned(),
            pool: "search".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 2,
            username: "s2".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "b".to_owned(),
            ct0: "d".to_owned(),
            pool: "search".to_owned(),
            ..crate::types::Session::default()
         },
      ];
      assert_eq!(pool.append_sessions(sessions).await, 2);
      pool.mark_verified(1, api).await;
      pool.mark_verified(2, api).await;

      let now = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap();
      // With no recent pool-wide tweet search success (e.g. global outage),
      // uncorroborated failures never trigger a canary or demotion.
      for q in 1..=5_u64 {
         assert_eq!(
            pool
               .record_search_timeline_failure_at(1, api, q, false, now + q)
               .await,
            SearchFailureAction::Rested
         );
      }
      assert_eq!(pool.account_state(1).await.unwrap().pool, "search");

      // Once another account proves tweet search is healthy, 3 distinct
      // uncorroborated failures trigger exactly one background canary.
      pool.record_tweet_search_ok_at(2, true, now + 10);
      assert_eq!(
         pool
            .record_search_timeline_failure_at(1, api, 10, false, now + 11)
            .await,
         SearchFailureAction::Rested
      );
      // Repeating the same bad query does not advance the count.
      assert_eq!(
         pool
            .record_search_timeline_failure_at(1, api, 10, false, now + 12)
            .await,
         SearchFailureAction::Rested
      );
      assert_eq!(
         pool
            .record_search_timeline_failure_at(1, api, 20, false, now + 13)
            .await,
         SearchFailureAction::Rested
      );
      assert_eq!(
         pool
            .record_search_timeline_failure_at(1, api, 30, false, now + 14)
            .await,
         SearchFailureAction::NeedsCanary
      );
      // While the canary is in flight, no duplicate canary is spawned.
      for q in [40_u64, 50, 60] {
         assert_eq!(
            pool
               .record_search_timeline_failure_at(1, api, q, false, now + 15)
               .await,
            SearchFailureAction::Rested
         );
      }
      pool.finish_search_canary(1);
      assert_eq!(pool.account_state(1).await.unwrap().pool, "search");
   }

   #[tokio::test]
   async fn out_accounts_snapshot_matches_census_and_set_ct0_repairs_stale_csrf() {
      let pool = SessionPool::empty(2);
      let sessions = vec![
         crate::types::Session {
            id: 1,
            username: "search_ok".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a1".to_owned(),
            ct0: "c1".to_owned(),
            pool: "search".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 2,
            username: "search_unverified".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a2".to_owned(),
            ct0: "c2".to_owned(),
            pool: "search".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 3,
            username: "search_bad_ct0".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a3".to_owned(),
            ct0: "old_ct0".to_owned(),
            pool: "search".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 4,
            username: "view_ok".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a4".to_owned(),
            ct0: "c4".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 5,
            username: "view_rejected".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a5".to_owned(),
            ct0: "c5".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 6,
            username: "view_demoted".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a6".to_owned(),
            ct0: "c6".to_owned(),
            pool: "search".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 7,
            username: "oauth_acct".to_owned(),
            kind: SessionKind::OAuth,
            oauth_token: "ot".to_owned(),
            oauth_secret: "os".to_owned(),
            ..crate::types::Session::default()
         },
      ];
      assert_eq!(pool.append_sessions(sessions).await, 7);

      pool.mark_verified(1, "id/SearchTimeline").await;
      pool.mark_verified(3, "id/SearchTimeline").await;
      pool.mark_ct0_dead(3).await;
      pool.mark_rejected(5).await;
      pool.demote_to_view(6, None, "search refused").await;

      let census = pool.census().await;
      // 7 accounts partition cleanly into active search (#1,#2), active view
      // (#4,#6,#7), ct0_dead (#3), and rejected (#5).
      assert_eq!(census.total, 7);
      assert_eq!(census.search, 2);
      assert_eq!(census.view, 3);
      assert_eq!(census.ct0_dead_pool, 1);
      assert_eq!(census.rejected_pool, 1);
      let search_op = census
         .operations
         .iter()
         .find(|op| op.operation == "SearchTimeline")
         .unwrap();
      let tweets_op = census
         .operations
         .iter()
         .find(|op| op.operation == "UserTweets")
         .unwrap();
      // Quarantined ct0_dead (#3) and rejected (#5) accounts are excluded from
      // active operation slot counts.
      assert_eq!(search_op.accounts, 2);
      assert_eq!(search_op.out, 1);
      assert_eq!(tweets_op.accounts, 3);
      assert_eq!(tweets_op.out, 0);

      let search_snap =
         pool.out_accounts_snapshot(Some("SearchTimeline"), None, None, None, "priority", 1, 50);
      assert_eq!(search_snap.counts.search_timeline, search_op.out);
      assert_eq!(search_snap.total_filtered, search_op.out);
      assert_eq!(search_snap.counts.ct0_dead_pool, 1);
      assert_eq!(search_snap.counts.rejected_pool, 1);
      // Only active unverified search account (#2) is in SearchTimeline out.
      assert_eq!(
         search_snap
            .accounts
            .iter()
            .map(|a| a.id)
            .collect::<Vec<_>>(),
         vec![2]
      );
      assert_eq!(search_snap.breakdown.unverified_search, 1);

      let tweets_snap =
         pool.out_accounts_snapshot(Some("UserTweets"), None, None, None, "priority", 1, 50);
      assert_eq!(tweets_snap.counts.user_tweets, tweets_op.out);
      assert!(tweets_snap.accounts.is_empty());

      // Stale ct0 (#3) lives in the ct0_dead pool and remembers home_pool=search.
      let ct0_snap =
         pool.out_accounts_snapshot(None, Some("ct0_dead"), None, None, "priority", 1, 50);
      assert_eq!(ct0_snap.accounts.len(), 1);
      assert_eq!(ct0_snap.accounts[0].id, 3);
      assert_eq!(ct0_snap.accounts[0].pool, "ct0_dead");
      assert_eq!(ct0_snap.accounts[0].home_pool, "search");

      // Rejected credentials (#5) live in the rejected pool.
      let rejected_snap =
         pool.out_accounts_snapshot(None, Some("rejected"), None, None, "priority", 1, 50);
      assert_eq!(rejected_snap.accounts.len(), 1);
      assert_eq!(rejected_snap.accounts[0].id, 5);
      assert_eq!(rejected_snap.accounts[0].pool, "rejected");
      assert_eq!(rejected_snap.accounts[0].home_pool, "view");

      let demoted_snap =
         pool.out_accounts_snapshot(None, Some("demoted"), None, None, "priority", 1, 50);
      assert_eq!(demoted_snap.counts.demoted_from_search, 1);
      assert_eq!(
         demoted_snap
            .accounts
            .iter()
            .map(|a| a.id)
            .collect::<Vec<_>>(),
         vec![6]
      );

      // Updating ct0 replaces the token, clears ct0_dead, and returns #3 to the
      // active search pool immediately.
      assert!(
         pool
            .set_ct0(3, "fresh_operator_ct0".to_owned())
            .await
            .unwrap()
      );
      assert_eq!(pool.ct0(3).as_deref(), Some("fresh_operator_ct0"));
      let after = pool.account_state(3).await.unwrap();
      assert!(!after.ct0_dead);
      assert!(after.available);
      assert_eq!(after.pool, "search");
      assert_eq!(after.ct0_len, "fresh_operator_ct0".len());

      let census_after = pool.census().await;
      assert_eq!(census_after.search, 3);
      assert_eq!(census_after.ct0_dead_pool, 0);
      assert_eq!(census_after.rejected_pool, 1);

      // Account #3 is active in SearchTimeline and #2 remains unverified.
      let search_snap_after =
         pool.out_accounts_snapshot(Some("SearchTimeline"), None, None, None, "priority", 1, 50);
      assert_eq!(
         search_snap_after
            .accounts
            .iter()
            .map(|a| a.id)
            .collect::<Vec<_>>(),
         vec![2]
      );
      assert_eq!(search_snap_after.counts.ct0_dead_pool, 0);

      // OAuth sessions cannot receive a ct0 override.
      assert!(matches!(
         pool.set_ct0(7, "nope".to_owned()).await,
         Err(Error::InvalidConfig(_))
      ));
      // Unknown sessions return NotFound.
      assert!(matches!(
         pool.set_ct0(999, "nope".to_owned()).await,
         Err(Error::NotFound(_))
      ));

      // Reinstating a rejected account clears credentials_rejected and returns
      // it to its home pool.
      assert!(pool.reinstate(5).await);
      let s5 = pool.account_state(5).await.unwrap();
      assert!(!s5.credentials_rejected);
      assert!(s5.available);
      assert_eq!(s5.pool, "view");
   }

   #[tokio::test]
   async fn rejected_credentials_stay_permanently_benched_across_headers_and_filters() {
      let pool = SessionPool::empty(2);
      let sessions = vec![
         crate::types::Session {
            id: 10,
            username: "locked_search".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a10".to_owned(),
            ct0: "c10".to_owned(),
            pool: "search".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 11,
            username: "locked_view".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a11".to_owned(),
            ct0: "c11".to_owned(),
            ..crate::types::Session::default()
         },
      ];
      assert_eq!(pool.append_sessions(sessions).await, 2);

      pool.mark_rejected(10).await;
      pool.mark_rejected(11).await;

      // Rate-limit headers from HTTP 200/403 code-326 responses must never
      // un-reject the account.
      let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 900;
      pool
         .update_session_limit(10, "ph2f/SearchTimeline", 187, 186, reset)
         .await;
      pool
         .update_session_limit(11, "x/UserTweets", 150, 149, reset)
         .await;

      let s10 = pool.account_state(10).await.unwrap();
      assert!(s10.credentials_rejected);
      assert!(!s10.available);
      assert_eq!(s10.pool, "rejected");
      assert_eq!(s10.home_pool, "search");
      assert!(s10.filters_failed);
      assert_eq!(s10.filters_retry_at, i64::MAX);

      let s11 = pool.account_state(11).await.unwrap();
      assert!(s11.credentials_rejected);
      assert!(!s11.available);
      assert_eq!(s11.pool, "rejected");
      assert_eq!(s11.home_pool, "view");
      assert!(s11.filters_failed);
      assert_eq!(s11.filters_retry_at, i64::MAX);

      assert!(pool.pending_filter_sync_ids().await.is_empty());
      assert!(pool.search_audit_ids("ph2f/SearchTimeline").await.is_empty());
      assert!(pool.cookie_header().is_none());
      assert!(matches!(
         pool.acquire_id(10, "ph2f/SearchTimeline").await,
         Err(Error::SessionRejected(_))
      ));
      assert!(matches!(
         pool.acquire_id(11, "x/UserTweets").await,
         Err(Error::SessionRejected(_))
      ));

      // Only an explicit reinstate lifts the rejection and resets the settings
      // retry fence.
      assert!(pool.reinstate(10).await);
      let reinstated = pool.account_state(10).await.unwrap();
      assert!(!reinstated.credentials_rejected);
      assert!(reinstated.available);
      assert!(!reinstated.filters_failed);
      assert_eq!(reinstated.filters_retry_at, 0);
      assert_eq!(pool.pending_filter_sync_ids().await, vec![10]);
      assert_eq!(
         pool.search_audit_ids("ph2f/SearchTimeline").await,
         vec![10]
      );
   }

   #[tokio::test]
   async fn search_pass_pool_is_isolated_from_public_and_falls_back_when_rate_limited() {
      let pool = SessionPool::empty(2);
      let api = "ph2f/SearchTimeline";
      let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 900;
      let sessions = vec![
         crate::types::Session {
            id: 1,
            username: "pub_search_1".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a1".to_owned(),
            ct0: "c1".to_owned(),
            pool: "search".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 2,
            username: "pass_search_1".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a2".to_owned(),
            ct0: "c2".to_owned(),
            pool: "search-pass".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 3,
            username: "pass_search_2".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a3".to_owned(),
            ct0: "c3".to_owned(),
            pool: "search-pass".to_owned(),
            ..crate::types::Session::default()
         },
         crate::types::Session {
            id: 4,
            username: "viewer".to_owned(),
            kind: SessionKind::Cookie,
            auth_token: "a4".to_owned(),
            ct0: "c4".to_owned(),
            ..crate::types::Session::default()
         },
      ];
      assert_eq!(pool.append_sessions(sessions).await, 4);
      for id in [1, 2, 3] {
         pool.mark_verified(id, api).await;
      }
      // Give the public account higher remaining quota than search-pass accounts
      // to prove paid requests still choose search-pass first, and public
      // requests never touch search-pass accounts.
      pool.update_session_limit(1, api, 187, 150, reset).await;
      pool.update_session_limit(2, api, 187, 40, reset).await;
      pool.update_session_limit(3, api, 187, 30, reset).await;

      // View requests never touch search or search-pass accounts.
      let view_lease = pool.acquire("x/UserTweets", None).await.unwrap();
      assert_eq!(view_lease.id, 4);
      drop(view_lease);

      // Public search always uses public search (#1) and never touches #2 or #3.
      let pub_lease = pool.acquire(api, None).await.unwrap();
      assert_eq!(pub_lease.id, 1);
      drop(pub_lease);

      // Paid search-pass always picks from the search-pass pool (#2 before #3),
      // even though public #1 has more remaining calls.
      let pass_lease = crate::api::budget::SEARCH_PASS
         .scope(true, pool.acquire(api, None))
         .await
         .unwrap();
      assert_eq!(pass_lease.id, 2);
      drop(pass_lease);

      // Exhaust the public pool: public search gets RateLimited and MUST NOT
      // touch search-pass accounts #2 or #3.
      pool.update_session_limit(1, api, 187, 0, reset).await;
      assert!(!pool.has_unlimited_excluding(api, None, &[]).await);
      assert!(matches!(
         pool.acquire(api, None).await,
         Err(Error::RateLimited)
      ));
      // Paid search-pass STILL works on its dedicated pool while public is exhausted!
      assert!(
         crate::api::budget::SEARCH_PASS
            .scope(true, pool.has_unlimited_excluding(api, None, &[]))
            .await
      );
      let pass_lease = crate::api::budget::SEARCH_PASS
         .scope(true, pool.acquire(api, None))
         .await
         .unwrap();
      assert!(matches!(pass_lease.id, 2 | 3));
      drop(pass_lease);

      // Add a fresh public search account (#5) and exhaust ALL search-pass
      // accounts (#2 spent, #3 resting).
      assert_eq!(
         pool
            .append_sessions(vec![crate::types::Session {
               id: 5,
               username: "pub_search_2".to_owned(),
               kind: SessionKind::Cookie,
               auth_token: "a5".to_owned(),
               ct0: "c5".to_owned(),
               pool: "search".to_owned(),
               ..crate::types::Session::default()
            }])
            .await,
         1
      );
      pool.mark_verified(5, api).await;
      pool.update_session_limit(5, api, 187, 50, reset).await;
      pool.update_session_limit(2, api, 187, 0, reset).await;
      pool.rest_endpoint(3, api).await;

      // Paid search-pass sees public #5 is available and falls back to it!
      assert!(
         crate::api::budget::SEARCH_PASS
            .scope(true, pool.has_unlimited_excluding(api, None, &[2, 3]))
            .await
      );
      let fallback_lease = crate::api::budget::SEARCH_PASS
         .scope(true, pool.acquire(api, None))
         .await
         .unwrap();
      assert_eq!(fallback_lease.id, 5);
      drop(fallback_lease);

      // When BOTH search-pass and public pools are rate-limited, paid search
      // returns RateLimited.
      pool.update_session_limit(5, api, 187, 0, reset).await;
      assert!(matches!(
         crate::api::budget::SEARCH_PASS
            .scope(true, pool.acquire(api, None))
            .await,
         Err(Error::RateLimited)
      ));

      // Demoting a search-pass account and reinstating it returns it to search-pass.
      pool.demote_to_view(2, None, "search refused").await;
      let demoted = pool.account_state(2).await.unwrap();
      assert_eq!(demoted.pool, "view");
      assert!(demoted.demoted_from_search);
      assert!(pool.reinstate(2).await);
      let reinstated = pool.account_state(2).await.unwrap();
      assert_eq!(reinstated.pool, "search-pass");
      assert_eq!(reinstated.home_pool, "search-pass");
      assert!(!reinstated.demoted_from_search);

      // Census separates search and search-pass cleanly.
      let census = pool.census().await;
      assert_eq!(census.search, 2);
      assert_eq!(census.search_pass, 2);
      assert_eq!(census.view, 1);
      let pass_op = census
         .operations
         .iter()
         .find(|op| op.operation == "SearchTimeline" && op.pool == "search-pass")
         .unwrap();
      assert_eq!(pass_op.accounts, 2);

      // When only search-pass accounts exist and all are rate-limited (no public
      // search accounts at all), paid search still reports RateLimited, not NoSessions.
      let pass_only_pool = SessionPool::empty(2);
      assert_eq!(
         pass_only_pool
            .append_sessions(vec![crate::types::Session {
               id: 10,
               username: "solo_pass".to_owned(),
               kind: SessionKind::Cookie,
               auth_token: "a10".to_owned(),
               ct0: "c10".to_owned(),
               pool: "search-pass".to_owned(),
               ..crate::types::Session::default()
            }])
            .await,
         1
      );
      pass_only_pool.mark_verified(10, api).await;
      pass_only_pool
         .update_session_limit(10, api, 187, 0, reset)
         .await;
      assert!(matches!(
         crate::api::budget::SEARCH_PASS
            .scope(true, pass_only_pool.acquire(api, None))
            .await,
         Err(Error::RateLimited)
      ));
   }
}
