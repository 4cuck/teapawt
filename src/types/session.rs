use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimit {
   pub limit: i32,
   pub remaining: i32,
   pub reset: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
   #[default]
   OAuth,
   Cookie,
}

/// Immutable session credentials, loaded once and `Arc`-shared.
#[derive(Debug, Clone, Serialize)]
pub struct SessionCredentials {
   pub id: i64,
   pub username: String,
   pub kind: SessionKind,
   pub oauth_token: String,
   pub oauth_secret: String,
   pub auth_token: String,
   pub ct0: String,
}

/// Mutable rate-limit state, stored separately in the pool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionLimits {
   pub limited: bool,
   pub limited_at: i64,
   /// The credentials themselves were refused, which no amount of waiting
   /// fixes, so this is kept apart from the rate-limit state that expires.
   #[serde(skip)]
   pub rejected: bool,
   /// The settings that hide posts are already off for this session: sensitive
   /// media is shown, and search hides neither sensitive posts nor posts from
   /// blocked and muted accounts. A start does not spend the same writes again.
   #[serde(default)]
   pub filters_cleared: bool,
   /// X refused the settings write itself, as it does for a suspended account
   /// ("not permitted to access this feature"). Waiting does not fix that, so
   /// the account is not asked again before `filters_retry_at`.
   #[serde(default)]
   pub filters_failed: bool,
   /// Unix seconds before which the write is not retried. Zero means the next
   /// pass may try. A refusal recorded before this was kept has no time, so it
   /// is retried instead of being trusted: the writer that recorded it minted
   /// transaction ids X refused, and that verdict said nothing about the
   /// account.
   #[serde(default)]
   pub filters_retry_at: i64,
   pub apis: HashMap<String, RateLimit>,
   /// Operations X answers with an empty 404 from this account while another
   /// account gets a result, by operation name, with when that was seen.
   #[serde(default)]
   pub refused_ops: HashMap<String, i64>,
   /// Operations this account has been seen getting results for, by
   /// operation name, with when.
   #[serde(default)]
   pub verified_ops: HashMap<String, i64>,
   /// 429s on profile reads. Two means the account is done viewing profiles.
   #[serde(default)]
   pub profile_strikes: u32,
   /// Kept out of profile pages. Search is tracked separately in `refused_ops`.
   #[serde(default)]
   pub profile_banned: bool,
   /// Most recent ambiguous search 404. It warrants a rest, not a ban.
   #[serde(default)]
   pub search_404_at: Option<i64>,
   /// Reserved for search. Set when the account has returned search results,
   /// or when it was imported as a search account. View calls never take it,
   /// so a search suspension cannot spread through profile traffic.
   #[serde(default)]
   pub search_only: bool,
   /// X will not serve this account search, and waiting does not change it.
   /// It stays in the pool for profile reads and stops being tried for search.
   #[serde(default)]
   pub search_banned: bool,
   /// This account was reserved for search and has been moved to the view
   /// pool. Remembered so that a verdict later shown to be wrong can be
   /// undone: without it nothing says which pool the account came from.
   #[serde(default)]
   pub demoted_from_search: bool,
   /// X refused the account itself with a 403 the settings write also gets
   /// ("not permitted to access this feature"). The account is out of search,
   /// and the view pool gets exactly one attempt to show it can still read
   /// profiles.
   #[serde(default)]
   pub suspended: bool,
   /// That one view attempt also came back suspended, so this account is done:
   /// no search, no view, no settings write, ever. Persisted, because a restart
   /// that forgot it would put the account straight back into rotation and
   /// spend calls re-learning what is already known.
   ///
   /// This is the same verdict a dead `ct0` carries, reached from the account
   /// side: the credentials are kept, and re-importing the account is how it
   /// gets another chance.
   #[serde(default)]
   pub retired: bool,
   /// The `ct0` this account holds is no longer the one X accepts.
   ///
   /// Every call teapot makes is csrf-protected, so an account whose token has
   /// rotated cannot do any of the work the pool asks of it. It is not
   /// refreshed: that meant a full page load of x.com per rejection, which is
   /// where X's anti-bot check runs, and the traffic cost more accounts than
   /// the tokens were worth. The credentials are kept so the account can be
   /// re-imported with a fresh token later.
   #[serde(default)]
   pub ct0_dead: bool,
   /// Calls X refused this account with a bare 429, by endpoint. Kept apart
   /// from `apis`, which holds X's real window.
   #[serde(skip)]
   pub resting: HashMap<String, Rest>,
}

/// One endpoint an account sits out after a bare 429.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rest {
   pub until: i64,
   /// Refusals in a row, each within [`BARE_429_MEMORY_SECS`] of the last rest
   /// ending.
   pub relapses: u32,
}

/// How long an account first sits out a call X refused it with a bare 429.
/// Half of those refusals cleared within 36 s and nine in ten within 150 s;
/// sitting every account out the whole 15-minute window took 1,130 accounts
/// off profile lookups.
const BARE_429_REST_SECS: i64 = 120;

/// The longest rest, X's own window. Reached only by an account X refuses
/// again each time it comes back: with a flat two minutes, three refusals in
/// four were the account's first call after its rest, about 1,000 wasted
/// calls and visitor retries a minute.
const BARE_429_REST_MAX_SECS: i64 = 15 * 60;

/// A refusal this soon after the last rest ended doubles the next one.
const BARE_429_MEMORY_SECS: i64 = 5 * 60;

/// How long a globally-limited session stays limited before auto-recovery (15
/// min).
const GLOBAL_LIMIT_DURATION_SECS: i64 = 15 * 60;

/// How long an account sits out an operation X refused it, before it is
/// tried again in case X lifted the restriction.
const REFUSED_OP_SECS: i64 = 24 * 60 * 60;

/// How long an account sits out search after an empty page.
///
/// An empty page is weak evidence — a query with no matches, an age gate, or
/// an NSFW filter all produce one — so the account is rested rather than
/// retired. Three minutes keeps it out of the way of the query that just came
/// back empty. A ban is a separate decision: only a settings write that X
/// refuses sets one.
const SEARCH_REFUSED_SECS: i64 = 3 * 60;

/// Keep repeated ambiguous 404s from cycling through the pool every few minutes.
const SEARCH_404_COOLDOWN_SECS: i64 = 30 * 60;

/// `SearchTimeline` from `ph2fARFabkwfxqmSKQ1OPw/SearchTimeline`, so a mark
/// outlives a change of query id.
pub(crate) fn operation_name(api: &str) -> &str {
   api.trim_end_matches('/')
      .rsplit('/')
      .next()
      .filter(|name| !name.is_empty())
      .unwrap_or("rest")
}

/// Calls an account makes about itself rather than on a visitor's behalf.
///
/// A search account is kept out of every non-search call so view traffic
/// cannot burn it, but these two are how it turns its own NSFW filters off:
/// the settings write, and the `verify_credentials` lookup that names the
/// numeric id the search-safety write is keyed by. Without them a search
/// account refuses its own settings write, and its filters stay on forever.
///
/// Both the operation name (`account/settings`) and a full URL
/// (`.../account/settings.json`) are accepted, so a caller cannot land on the
/// wrong side of this by passing the other form.
pub(crate) fn is_self_service(name: &str) -> bool {
   let name = name.strip_suffix(".json").unwrap_or(name);
   matches!(name, "settings" | "verify_credentials")
}

/// A profile read. One 429 sits the account out for the window. A second
/// takes it off profile pages for good.
pub(crate) fn is_profile_op(name: &str) -> bool {
   matches!(
      name,
      "UserTweets"
         | "UserTweetsAndReplies"
         | "UserByScreenName"
         | "UserMedia"
         | "TweetDetail"
         | "UserOriginalsTimeline"
         | "UserHighlightsTweets"
         | "UserArticlesTweets"
         | "UserRepostsTimeline"
   )
}

/// What a 429 did to the account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Damage {
   /// Out of this call until its window resets.
   Window,
   /// Out of search for good. Profile pages still use it.
   Search,
   /// Out of profile pages for good.
   Profile,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Spend {
   /// Remaining was decremented.
   Metered,
   /// No active window, so the call proceeds without a local hold.
   Unmetered,
   /// The account cannot make this call.
   Denied,
}

/// How much room `api` has. [`Headroom::Safe`] is a known window with at
/// least one call left. The account with the most left is used next, so the
/// last recorded call is spent only after every fuller account is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Headroom {
   Safe(i32),
   /// No window yet. Usable only when nobody has a known budget.
   Unknown,
   Blocked,
}

impl SessionLimits {
   /// Room for another call to `api` that will not be the one X rejects.
   pub(crate) fn headroom_at(&self, api: &str, op_name: &str, now: i64) -> Headroom {
      self.headroom_at_classified(
         api,
         op_name,
         op_name == "SearchTimeline",
         is_self_service(op_name),
         is_profile_op(op_name),
         now,
      )
   }

   /// Same as [`Self::headroom_at`], reusing pre-classified flags for `op_name`
   /// across a full pool scan.
   pub(crate) fn headroom_at_classified(
      &self,
      api: &str,
      op_name: &str,
      is_search: bool,
      self_service: bool,
      profile_op: bool,
      now: i64,
   ) -> Headroom {
      if self.rejected
         || self.is_globally_limited_at(now)
         || self.refuses_at_classified(op_name, is_search, self_service, profile_op, now)
         || self.resting_at(api, now)
      {
         return Headroom::Blocked;
      }
      if self.apis.is_empty() {
         return Headroom::Unknown;
      }
      let Some(rate) = self.apis.get(api) else {
         return Headroom::Unknown;
      };
      if rate.reset <= now {
         return Headroom::Unknown;
      }
      if rate.remaining <= 0 {
         return Headroom::Blocked;
      }
      Headroom::Safe(rate.remaining)
   }
   /// Check if rate limited for a specific API.
   pub fn is_limited(&self, api: &str) -> bool {
      if self.is_globally_limited() {
         return true;
      }
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      if self.resting_at(api, now) {
         return true;
      }
      if let Some(limit) = self.apis.get(api) {
         if limit.remaining == 0 && limit.reset > now {
            return true;
         }
      }
      false
   }

   /// Sit this account out of `api`, leaving its recorded window as X last
   /// reported it. [`BARE_429_REST_SECS`] at first, doubling while X keeps
   /// refusing it straight after each rest. Returns the rest in seconds.
   pub fn rest(&mut self, api: &str) -> i64 {
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      self.rest_at(api, now)
   }

   fn rest_at(&mut self, api: &str, now: i64) -> i64 {
      let remembered = |rest: &Rest| rest.until + BARE_429_MEMORY_SECS > now;
      let relapses = self
         .resting
         .get(api)
         .filter(|last| remembered(last))
         .map_or(0, |last| last.relapses.saturating_add(1));
      let secs = BARE_429_REST_SECS
         .saturating_mul(1 << relapses.min(8))
         .min(BARE_429_REST_MAX_SECS);
      self.resting.retain(|_, rest| remembered(rest));
      self.resting.insert(api.to_owned(), Rest {
         until: now + secs,
         relapses,
      });
      secs
   }

   pub(crate) fn resting_at(&self, api: &str, now: i64) -> bool {
      !self.resting.is_empty() && self.resting.get(api).is_some_and(|rest| rest.until > now)
   }

   /// Whether X refuses this account `api`. A confirmed suspension is
   /// permanent; every other refusal, search included, expires. A search-only
   /// account refuses every call that is not search — and not about itself —
   /// so view traffic cannot burn it.
   #[must_use]
   pub fn refuses(&self, api: &str) -> bool {
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      self.refuses_at(operation_name(api), now)
   }

   pub(crate) fn refuses_at(&self, name: &str, now: i64) -> bool {
      self.refuses_at_classified(
         name,
         name == "SearchTimeline",
         is_self_service(name),
         is_profile_op(name),
         now,
      )
   }

   pub(crate) fn refuses_at_classified(
      &self,
      name: &str,
      is_search: bool,
      self_service: bool,
      profile_op: bool,
      now: i64,
   ) -> bool {
      // A rotated `ct0` closes every call the pool makes, not one of them.
      if self.ct0_dead {
         return true;
      }
      // X refused the account itself twice: once at the settings write, then
      // again on the one view attempt. Nothing is asked of it again.
      if self.retired {
         return true;
      }
      // A suspended account does not get another settings write. The write is
      // what confirmed the suspension, so repeating it only spends a call to
      // learn the same thing. Its one view attempt is still allowed — that is
      // how a suspended account shows whether it can read profiles.
      if self.suspended && self_service {
         return true;
      }
      if self.search_only && !is_search && !self_service {
         return true;
      }
      if is_search
         && (self.search_banned
            || self
               .search_404_at
               .is_some_and(|at| now - at < SEARCH_404_COOLDOWN_SECS))
      {
         return true;
      }
      if self.profile_banned && profile_op {
         return true;
      }
      if self.refused_ops.is_empty() {
         return false;
      }
      let Some(&at) = self.refused_ops.get(name) else {
         return false;
      };
      // An empty search page is what a query with no matches gets, and what an
      // account X age-gated or filtered hides, so a search refusal is a rest
      // rather than a verdict: the account is tried again within the hour.
      let window = if is_search {
         SEARCH_REFUSED_SECS
      } else {
         REFUSED_OP_SECS
      };
      now - at < window
   }

   /// Leave this account out of `api` for a while. The account stays reserved
   /// for search, so it is not mixed back into views. Anything older than
   /// [`REFUSED_OP_SECS`], or [`SEARCH_REFUSED_SECS`] for search, is dropped.
   pub fn refuse(&mut self, api: &str) {
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      let name = operation_name(api);
      self
         .refused_ops
         .retain(|_, at| now - *at < REFUSED_OP_SECS);
      self.refused_ops.insert(name.to_owned(), now);
      self.verified_ops.remove(name);
   }

   /// Whether this account has been seen getting results for `api`.
   #[must_use]
   pub fn verified(&self, api: &str) -> bool {
      self.verified_op(operation_name(api))
   }

   #[must_use]
   pub(crate) fn verified_op(&self, op_name: &str) -> bool {
      !self.verified_ops.is_empty() && self.verified_ops.contains_key(op_name)
   }

   /// Record that this account got results for `api`.
   ///
   /// A page with posts contradicts an earlier refusal of the same call, which
   /// is only ever recorded from an empty answer, so the refusal is dropped.
   /// Otherwise an account the search check just proved works would still sit
   /// out its hour, since [`SessionLimits::refuses`] reads the refusal first.
   pub fn verify(&mut self, api: &str) {
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      let name = operation_name(api);
      self.refused_ops.remove(name);
      self.verified_ops.insert(name.to_owned(), now);
   }

   #[cfg(test)]
   pub fn unchecked(&self, api: &str) -> bool {
      if operation_name(api) == "SearchTimeline" && !self.search_only {
         return false;
      }
      !self.refuses(api) && !self.verified(api)
   }

   /// Keep this account for search alone. View calls never take it. A later
   /// failed search does not clear this: the account stays out of both pools
   /// rather than being mixed back into profiles.
   pub fn reserve_for_search(&mut self) {
      self.search_only = true;
   }

   /// Spend one local call against `api` when its window is known.
   ///
   /// The decrement happens before the request is handed out, so the next
   /// acquire sees the account as exhausted instead of piling onto it.
   pub(crate) fn try_spend(&mut self, api: &str) -> Spend {
      if self.rejected || self.is_globally_limited() || self.refuses(api) {
         return Spend::Denied;
      }
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      if self.resting_at(api, now) {
         return Spend::Denied;
      }
      let Some(rate) = self.apis.get_mut(api) else {
         return Spend::Unmetered;
      };
      if rate.reset <= now {
         return Spend::Unmetered;
      }
      if rate.remaining <= 0 {
         return Spend::Denied;
      }
      rate.remaining -= 1;
      Spend::Metered
   }

   /// Give back a local spend when the call never reached X's quota.
   pub(crate) fn refund_spend(&mut self, api: &str) {
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      let Some(rate) = self.apis.get_mut(api) else {
         return;
      };
      if rate.reset <= now {
         return;
      }
      if rate.limit > 0 && rate.remaining >= rate.limit {
         return;
      }
      rate.remaining += 1;
   }

   /// Whether the session-wide limit is set and has not yet expired.
   #[must_use]
   pub fn is_globally_limited(&self) -> bool {
      self.is_globally_limited_at(time::OffsetDateTime::now_utc().unix_timestamp())
   }

   #[must_use]
   pub(crate) const fn is_globally_limited_at(&self, now: i64) -> bool {
      // Cleared on the next mutable access rather than here.
      self.limited && now - self.limited_at < GLOBAL_LIMIT_DURATION_SECS
   }

   pub fn update_limit(&mut self, api: &str, limit: i32, remaining: i32, reset: i64) {
      self.apis.insert(
         api.to_owned(),
         RateLimit {
            limit,
            remaining,
            reset,
         },
      );
   }

   /// Record a single API as exhausted for the rest of the window.
   ///
   /// For a 429 that arrives without `x-rate-limit-*` headers, where nothing
   /// would otherwise be recorded and the session would retry immediately.
   pub fn limit_endpoint(&mut self, api: &str) {
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      let limit = self.apis.get(api).map_or(0, |rate| rate.limit);
      self.update_limit(api, limit, 0, now + GLOBAL_LIMIT_DURATION_SECS);
   }

   /// Record a 429. Search is removed immediately. A profile read is removed
   /// for the window, and a second one removes the account from profile pages.
   pub fn note_rate_limit(&mut self, api: &str) -> Damage {
      self.limit_endpoint(api);
      let name = operation_name(api);
      if name == "SearchTimeline" {
         self.refuse(api);
         return Damage::Search;
      }
      if is_profile_op(name) {
         self.profile_strikes = self.profile_strikes.saturating_add(1);
         if self.profile_strikes >= 2 {
            self.profile_banned = true;
            return Damage::Profile;
         }
      }
      Damage::Window
   }
}

/// Authentication session for Twitter API (used for JSONL deserialization).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
   pub id: i64,
   pub username: String,
   pub pending: i32,
   pub limited: bool,
   pub limited_at: i64,
   pub apis: HashMap<String, RateLimit>,
   pub kind: SessionKind,

   // OAuth credentials
   pub oauth_token: String,
   pub oauth_secret: String,

   // Cookie credentials
   pub auth_token: String,
   pub ct0: String,
   /// `"search"` keeps this account out of profile and tweet reads. Absent
   /// means the shared view pool, which is every account already on disk.
   #[serde(default)]
   pub pool: String,
   /// `"search"` when this account was reserved for search and a verdict
   /// moved it to the view pool. Written with the move and removed when the
   /// account goes back, so the move can be undone.
   #[serde(default)]
   pub demoted_from: String,
}

impl Session {
   /// Split into immutable credentials and mutable rate-limit state.
   pub fn into_credentials_and_limits(self) -> (SessionCredentials, SessionLimits) {
      (
         SessionCredentials {
            id: self.id,
            username: self.username,
            kind: self.kind,
            oauth_token: self.oauth_token,
            oauth_secret: self.oauth_secret,
            auth_token: self.auth_token,
            ct0: self.ct0,
         },
         SessionLimits {
            limited: self.limited,
            limited_at: self.limited_at,
            rejected: false,
            filters_cleared: false,
            filters_failed: false,
            filters_retry_at: 0,
            apis: self.apis,
            refused_ops: HashMap::new(),
            verified_ops: HashMap::new(),
            profile_strikes: 0,
            profile_banned: false,
            search_404_at: None,
            search_only: self.pool.eq_ignore_ascii_case("search"),
            search_banned: false,
            demoted_from_search: self.demoted_from.eq_ignore_ascii_case("search")
               && !self.pool.eq_ignore_ascii_case("search"),
            suspended: false,
            retired: false,
            ct0_dead: false,
            resting: HashMap::new(),
         },
      )
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn a_bare_429_rests_the_call_briefly_and_keeps_the_window() {
      let mut limits = SessionLimits::default();
      let now = time::OffsetDateTime::now_utc().unix_timestamp();
      limits.update_limit("x/UserByScreenName", 150, 90, now + 600);
      limits.rest("x/UserByScreenName");

      assert_eq!(limits.try_spend("x/UserByScreenName"), super::Spend::Denied);
      assert!(limits.is_limited("x/UserByScreenName"));
      assert_eq!(limits.apis["x/UserByScreenName"].remaining, 90);
      assert_eq!(limits.profile_strikes, 0);
      assert_eq!(limits.try_spend("x/TweetDetail"), super::Spend::Unmetered);

      let later = now + BARE_429_REST_SECS + 1;
      assert_eq!(
         limits.headroom_at("x/UserByScreenName", "UserByScreenName", later),
         Headroom::Safe(90)
      );
   }

   #[test]
   fn a_refusal_straight_after_a_rest_doubles_the_next_one() {
      let mut limits = SessionLimits::default();
      let api = "x/UserTweets";
      let mut now = 1_000_000;
      let mut rests = Vec::new();
      for _ in 0..5 {
         let secs = limits.rest_at(api, now);
         rests.push(secs);
         now += secs + 1;
      }
      assert_eq!(rests, [120, 240, 480, 900, 900]);
      assert!(!limits.resting_at("x/TweetDetail", now));
   }

   #[test]
   fn a_refusal_long_after_the_last_rest_starts_over() {
      let mut limits = SessionLimits::default();
      let api = "x/UserTweets";
      let now = 1_000_000;
      assert_eq!(limits.rest_at(api, now), 120);
      assert_eq!(limits.rest_at(api, now + 121), 240);
      let later = now + 121 + 240 + BARE_429_MEMORY_SECS + 1;
      assert_eq!(limits.rest_at(api, later), 120);
   }

   #[test]
   fn a_search_account_never_serves_a_view() {
      let mut limits = SessionLimits::default();
      limits.reserve_for_search();
      assert!(limits.refuses("x/UserTweets"));
      assert!(limits.refuses("x/UserByScreenName"));
      assert!(limits.refuses("x/TweetDetail"));
      assert!(!limits.refuses("ph2f/SearchTimeline"));
      assert!(limits.unchecked("ph2f/SearchTimeline"));

      limits.verify("ph2f/SearchTimeline");
      assert!(!limits.unchecked("ph2f/SearchTimeline"));
      assert!(limits.refuses("x/UserTweets"));
   }

   #[test]
   fn a_view_account_is_not_probed_for_search() {
      let limits = SessionLimits::default();
      assert!(limits.unchecked("x/UserTweets"));
      assert!(!limits.unchecked("ph2f/SearchTimeline"));
      assert!(!limits.refuses("x/UserTweets"));
   }

   #[test]
   fn an_imported_search_account_starts_search_only() {
      let session = Session {
         id: 9,
         pool: "search".to_owned(),
         ..Session::default()
      };
      let (_, limits) = session.into_credentials_and_limits();
      assert!(limits.search_only);
      assert!(limits.refuses("x/UserTweets"));
      assert!(!limits.refuses("ph2f/SearchTimeline"));
   }

   #[test]
   fn a_search_account_still_may_turn_its_own_filters_off() {
      let mut limits = SessionLimits::default();
      limits.reserve_for_search();

      // Kept out of everything a visitor's page would ask for...
      for api in ["x/UserTweets", "x/UserByScreenName", "x/TweetDetail"] {
         assert!(limits.refuses(api), "{api}");
      }

      // ...but not out of the two calls that are how it stops hiding NSFW
      // posts. Refusing those left every search-pool account with its filters
      // on, which is the opposite of what the pool is for.
      assert!(!limits.refuses("account/settings"));
      assert!(!limits.refuses("/1.1/account/verify_credentials.json"));
      assert!(!limits.refuses("ph2f/SearchTimeline"));
   }

   #[test]
   fn a_search_refusal_expires_after_three_minutes() {
      let mut limits = SessionLimits::default();
      limits.refuse("old/SearchTimeline");
      assert!(limits.refuses("new/SearchTimeline"));

      // Three minutes of rest, then search is tried again: the empty page may
      // have been the query's, or the account's own filters hiding it.
      *limits.refused_ops.get_mut("SearchTimeline").unwrap() -= 4 * 60;
      assert!(!limits.refuses("new/SearchTimeline"));

      // Other operations come back after a day.
      let mut limits = SessionLimits::default();
      limits.refuse("x/UserTweets");
      *limits.refused_ops.get_mut("UserTweets").unwrap() -= 25 * 60 * 60;
      assert!(!limits.refuses("x/UserTweets"));
   }

   #[test]
   fn repeated_empty_searches_do_not_ban_the_account() {
      let mut limits = SessionLimits::default();

      // A query with no matches comes back empty from every account, so a
      // count of them is not a suspension. Only the settings probe sets this.
      for _ in 0..6 {
         limits.refuse("ph2f/SearchTimeline");
      }
      assert!(!limits.search_banned);
      assert!(limits.refuses("ph2f/SearchTimeline"));
      assert!(!limits.refuses("x/UserTweets"));
   }

   #[test]
   fn a_stale_ct0_closes_every_call() {
      let mut limits = SessionLimits::default();
      assert!(!limits.refuses("ph2f/SearchTimeline"));

      // X rotated the token: nothing the pool asks of the account can run,
      // and no page load is made to replace it.
      limits.ct0_dead = true;
      assert!(limits.refuses("ph2f/SearchTimeline"));
      assert!(limits.refuses("x/UserTweets"));
      assert!(limits.refuses("x/UserByScreenName"));
      assert!(!limits.unchecked("ph2f/SearchTimeline"));
   }

   #[test]
   fn a_verified_call_drops_the_refusal_it_contradicts() {      let mut limits = SessionLimits::default();
      limits.refuse("old/SearchTimeline");
      assert!(limits.refuses("new/SearchTimeline"));

      // The account returned posts, so the empty page was not its verdict.
      limits.verify("ph2f/SearchTimeline");
      assert!(!limits.refuses("new/SearchTimeline"));
      assert!(limits.verified("ph2f/SearchTimeline"));
   }

   #[test]
   fn a_search_limit_removes_the_account_at_once_and_a_profile_limit_waits_for_a_second_strike() {
      let mut limits = SessionLimits::default();
      assert_eq!(
         limits.note_rate_limit("ph2f/SearchTimeline"),
         Damage::Search
      );
      assert!(limits.refuses("other/SearchTimeline"));
      assert!(!limits.profile_banned);
      assert!(limits.try_spend("ph2f/SearchTimeline") == super::Spend::Denied);

      assert_eq!(limits.note_rate_limit("x/UserTweets"), Damage::Window);
      assert!(!limits.profile_banned);
      assert!(!limits.refuses("x/TweetDetail"));
      assert_eq!(limits.note_rate_limit("x/TweetDetail"), Damage::Profile);
      assert!(limits.profile_banned);
      assert!(limits.refuses("x/UserByScreenName"));
      assert!(limits.refuses("x/UserTweets"));
      assert!(limits.refuses("other/SearchTimeline"));
      assert_eq!(limits.try_spend("x/UserTweets"), super::Spend::Denied);
   }

   #[test]
   fn operation_name_handles_trailing_slashes_and_empty_inputs() {
      assert_eq!(operation_name("abc/UserTweets"), "UserTweets");
      assert_eq!(operation_name("account/settings/"), "settings");
      assert_eq!(operation_name(""), "rest");
      assert_eq!(operation_name("/"), "rest");
   }
}