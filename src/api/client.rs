use std::{
   borrow::Cow,
   collections::HashMap,
   fmt,
   iter,
   result::Result as StdResult,
   sync::{
      Arc,
      Mutex as StdMutex,
   },
   time::{
      Duration,
      Instant,
   },
};

use axum::http::header;
use bytes::Bytes;
use serde::{
   Deserialize,
   de::DeserializeOwned,
};
use tokio::{
   sync::{
      Semaphore,
      watch,
   },
   time::{
      interval,
      sleep,
      timeout,
   },
};

use super::{
   ProxyPool,
   SessionLease,
   SessionPool,
   SuspensionProbe,
   TidClient,
   budget::{
      self,
      ClientBudget,
   },
   cookies::Jar,
   endpoints,
   http::{
      Egress,
      HttpClient,
      ProxyConfig,
      Purpose,
   },
   latency::Latency,
   log_events::{
      self,
      Event,
   },
   operations::Operations,
   parser,
   timeline_route::TimelineRoutes,
};
use crate::{
   accounts::AccountStore,
   api::{
      cookies::EditorCookie,
      schema::{
         AboutAccountData,
         ApiError,
         AudioSpaceData,
         AudioSpaceMetadata,
         BroadcastMetadata,
         BroadcastsData,
         ConversationData,
         EditHistoryData,
         GqlResponse,
         ListByIdData,
         ListBySlugData,
         ListMembersData,
         ListTimelineData,
         LiveStreamStatus,
         RetweetersData,
         SearchTimelineData,
         TweetData,
         TweetResultsByRestIdsData,
         UserResultData,
         UserTimelineData,
      },
   },
   config::Config,
   error::{
      Error,
      Result,
      TwitterError,
   },
   pressure::Pressure,
   types::{
      AccountContext,
      Article,
      CardKind,
      Conversation,
      EditHistory,
      List,
      PaginatedResult,
      PhotoRail,
      Profile,
      SessionKind,
      Timeline,
      Translation,
      Tweet,
      User,
   },
   utils::formatters,
};

/// True when the 429 carries X's account window rather than a bare edge page.
fn account_rate_window(headers: &axum::http::HeaderMap) -> bool {
   headers.get("x-rate-limit-remaining").is_some()
      || headers
         .get(header::CONTENT_TYPE)
         .and_then(|value| value.to_str().ok())
         .is_some_and(|value| value.to_ascii_lowercase().contains("json"))
}

/// Empty or HTML 429 with no account window. Those are the proxy IP, and
/// retrying more cookies on that same path only burns the pool.
fn ip_throttle(body: &str, account_window: bool) -> bool {
   if account_window {
      return false;
   }
   let body = body.trim_start();
   body.is_empty() || body.starts_with('<')
}

/// A search spends a `SearchTimeline` call and is never served from cache.
const SEARCH_COST: f64 = 2.0;
/// First try plus this many replacements when the account itself is rejected
/// or out of quota. An empty answer gets one other account, since X refuses
/// some accounts an operation it serves to the rest.
const GRAPHQL_REPLACEMENTS: usize = 2;
/// Per-account 429 (JSON / `x-rate-limit-*`). HTML 429s are the WAF and must
/// not walk the pool — that used to zero every cookie for 15 minutes.
const RATE_LIMIT_REPLACEMENTS: usize = 5;
/// Second copies of slow calls in flight at once. In a steady state about
/// one call in ten is hedged; this cap keeps a sudden slowdown at X from
/// doubling every call while the latency window catches up.
const MAX_HEDGES: usize = 48;
/// How often the upstream latency summary is logged.
const LATENCY_REPORT_EVERY: Duration = Duration::from_secs(60);
/// How often exits are ranked by how far their answers trail the fleet.
const EXIT_RANK_EVERY: Duration = Duration::from_secs(15);
const ACCOUNT_RECONCILE_EVERY: Duration = Duration::from_secs(10);

/// An answer about the thing asked for, which a second account would give
/// too. Anything else is the account or the path, and the other copy of a
/// hedged call is still worth waiting for.
const fn settles_the_call(err: &Error) -> bool {
   matches!(
      err,
      Error::NotFound(_)
         | Error::UserNotFound(_)
         | Error::UserSuspended(_)
         | Error::ProtectedUser(_)
         | Error::TweetNotFound(_)
         | Error::InvalidUrl(_)
         | Error::SearchDenylisted
   )
}

/// Edge 502/503/504, including an empty body and a stream the proxy reset.
fn is_gateway_blip(message: &str) -> bool {
   message.starts_with("Status 502")
      || message.starts_with("Status 503")
      || message.starts_with("Status 504")
}

/// The `rawQuery` inside a SearchTimeline variables string, trimmed so a
/// pasted query cannot flood the log.
fn search_query_of(variables: &str) -> String {
   serde_json::from_str::<serde_json::Value>(variables)
      .ok()
      .and_then(|value| value.get("rawQuery")?.as_str().map(str::to_owned))
      .map(|query| query.chars().take(120).collect())
      .unwrap_or_default()
}

/// True when a `SearchTimeline` request asks for a tweet search (`Latest`,
/// `Top`, or `Media`) rather than `People` or `Lists`.
///
/// Suspended accounts still answer `People` and `Lists` with HTTP 200 and
/// results, while answering every tweet search with `"Internal server error"`
/// on `search_by_raw_query.search_timeline.timeline`.
fn is_tweet_search_variables(variables: &str) -> bool {
   #[derive(Deserialize)]
   struct ProductOnly<'a> {
      #[serde(borrow)]
      product: Option<Cow<'a, str>>,
   }
   serde_json::from_str::<ProductOnly<'_>>(variables)
      .ok()
      .and_then(|parsed| parsed.product)
      .is_none_or(|product| !matches!(product.as_ref(), "People" | "Lists"))
}

/// Stable non-zero hash of the normalized `rawQuery` inside `SearchTimeline`
/// variables, ignoring case, surrounding whitespace, tab, and pagination
/// cursor so repeating one bad query across tabs or pages never counts as two
/// distinct failing queries.
fn search_query_hash(variables: &str) -> u64 {
   let raw = search_query_of(variables);
   let normalized = raw.trim();
   let mut hash = 0xCBF2_9CE4_8422_2325_u64;
   for byte in normalized.bytes() {
      hash ^= u64::from(byte.to_ascii_lowercase());
      hash = hash.wrapping_mul(0x100_0000_01B3);
   }
   hash.max(1)
}

enum WireStr<'a> {
   Borrowed(&'a str),
   Shared(Arc<str>),
   Owned(String),
}

impl WireStr<'_> {
   fn as_str(&self) -> &str {
      match self {
         Self::Borrowed(value) => value,
         Self::Shared(value) => value,
         Self::Owned(value) => value,
      }
   }
}

struct WireOp<'a> {
   path:             WireStr<'a>,
   features:         WireStr<'a>,
   encoded_features: WireStr<'a>,
}

fn graphql_get_url(
   base_url: &str,
   path: &str,
   variables: &str,
   encoded_features: &str,
   field_toggles: Option<&str>,
) -> String {
   let encoded_toggles = field_toggles.map(endpoints::encoded_field_toggles);
   let toggles_len = encoded_toggles
      .as_deref()
      .map_or(0, |toggles| "&fieldToggles=".len() + toggles.len());
   let mut url = String::with_capacity(
      base_url.len()
         + 1
         + path.len()
         + "?variables=".len()
         + variables.len().saturating_mul(2)
         + "&features=".len()
         + encoded_features.len()
         + toggles_len,
   );
   url.push_str(base_url);
   url.push('/');
   url.push_str(path);
   url.push_str("?variables=");
   for chunk in form_urlencoded::byte_serialize(variables.as_bytes()) {
      url.push_str(chunk);
   }
   url.push_str("&features=");
   url.push_str(encoded_features);
   if let Some(ref toggles) = encoded_toggles {
      url.push_str("&fieldToggles=");
      url.push_str(toggles);
   }
   url
}

fn validate_json_object<'a>(field: &str, input: &'a str) -> Result<&'a str> {
   let trimmed = input.trim();
   if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
      return Err(Error::Internal(format!(
         "graphql {field}: expected JSON object"
      )));
   }
   serde_json::from_str::<serde::de::IgnoredAny>(input)
      .map_err(|err| Error::Internal(format!("graphql {field}: {err}")))?;
   Ok(trimmed)
}

fn graphql_post_body(
   variables: &str,
   features: &str,
   field_toggles: Option<&str>,
) -> Result<Bytes> {
   let variables = validate_json_object("variables", variables)?;
   let features = validate_json_object("features", features)?;
   let field_toggles = field_toggles
      .map(|toggles| validate_json_object("fieldToggles", toggles))
      .transpose()?;

   let toggles_len = field_toggles.map_or(0, |toggles| r#","fieldToggles":"#.len() + toggles.len());
   let mut out = Vec::with_capacity(
      r#"{"variables":"#.len()
         + variables.len()
         + r#","features":"#.len()
         + features.len()
         + toggles_len
         + 1,
   );
   out.extend_from_slice(br#"{"variables":"#);
   out.extend_from_slice(variables.as_bytes());
   out.extend_from_slice(br#","features":"#);
   out.extend_from_slice(features.as_bytes());
   if let Some(toggles) = field_toggles {
      out.extend_from_slice(br#","fieldToggles":"#);
      out.extend_from_slice(toggles.as_bytes());
   }
   out.push(b'}');
   Ok(Bytes::from(out))
}

#[derive(Clone, Copy, Debug, Default)]
struct ResponseLimits {
   /// `x-rate-limit-remaining <= 0` with a future `x-rate-limit-reset`, so the
   /// session's real window is already recorded as exhausted.
   limit_recorded:   bool,
   /// `x-rate-limit-remaining <= 0` was present on the response headers, even
   /// if `x-rate-limit-reset` was missing or slightly skewed.
   header_exhausted: bool,
}

fn is_denylisted(message: &str) -> bool {
   message.contains("QueryDenylistedFailure") || message.contains("Query is denylisted")
}

fn is_rate_limit_error(err: &ApiError) -> bool {
   matches!(
      TwitterError::from_code(err.code),
      Some(TwitterError::RateLimited)
   ) || err.message.to_ascii_lowercase().contains("rate limit")
}

fn is_session_error(err: &ApiError) -> bool {
   matches!(
      TwitterError::from_code(err.code),
      Some(TwitterError::Locked | TwitterError::InvalidToken | TwitterError::BadToken)
   )
}

enum SearchDataField {
   Result,
   List,
   Other,
}

impl<'de> Deserialize<'de> for SearchDataField {
   fn deserialize<D>(deserializer: D) -> StdResult<Self, D::Error>
   where
      D: serde::Deserializer<'de>,
   {
      deserializer.deserialize_str(SearchDataFieldVisitor)
   }
}

struct SearchDataFieldVisitor;

#[expect(
   clippy::missing_trait_methods,
   reason = "serde Visitor defaults reject non-string keys"
)]
impl serde::de::Visitor<'_> for SearchDataFieldVisitor {
   type Value = SearchDataField;

   fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
      formatter.write_str("a JSON object key")
   }

   fn visit_str<E>(self, value: &str) -> StdResult<SearchDataField, E>
   where
      E: serde::de::Error,
   {
      Ok(match value {
         "tweet_results" | "user_results" => SearchDataField::Result,
         "list" => SearchDataField::List,
         _ => SearchDataField::Other,
      })
   }
}

/// Streaming zero-allocation scanner that succeeds iff a JSON value under
/// `"data"` contains no `tweet_results`, `user_results`, or `"list": { ... }`.
/// Returns an error immediately to short-circuit as soon as a result is seen.
struct SearchScan {
   reject_object: bool,
}

impl<'de> serde::de::DeserializeSeed<'de> for SearchScan {
   type Value = ();

   fn deserialize<D>(self, deserializer: D) -> StdResult<(), D::Error>
   where
      D: serde::Deserializer<'de>,
   {
      deserializer.deserialize_any(self)
   }
}

#[expect(
   clippy::missing_trait_methods,
   reason = "serde Visitor defaults handle unused scalar types"
)]
impl<'de> serde::de::Visitor<'de> for SearchScan {
   type Value = ();

   fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
      formatter.write_str("a JSON value")
   }

   fn visit_bool<E>(self, _value: bool) -> StdResult<(), E>
   where
      E: serde::de::Error,
   {
      Ok(())
   }

   fn visit_i64<E>(self, _value: i64) -> StdResult<(), E>
   where
      E: serde::de::Error,
   {
      Ok(())
   }

   fn visit_u64<E>(self, _value: u64) -> StdResult<(), E>
   where
      E: serde::de::Error,
   {
      Ok(())
   }

   fn visit_f64<E>(self, _value: f64) -> StdResult<(), E>
   where
      E: serde::de::Error,
   {
      Ok(())
   }

   fn visit_str<E>(self, _value: &str) -> StdResult<(), E>
   where
      E: serde::de::Error,
   {
      Ok(())
   }

   fn visit_unit<E>(self) -> StdResult<(), E>
   where
      E: serde::de::Error,
   {
      Ok(())
   }

   fn visit_seq<A>(self, mut seq: A) -> StdResult<(), A::Error>
   where
      A: serde::de::SeqAccess<'de>,
   {
      while seq
         .next_element_seed(Self {
            reject_object: false,
         })?
         .is_some()
      {}
      Ok(())
   }

   fn visit_map<A>(self, mut map: A) -> StdResult<(), A::Error>
   where
      A: serde::de::MapAccess<'de>,
   {
      if self.reject_object {
         return Err(serde::de::Error::custom("found"));
      }
      while let Some(field) = map.next_key::<SearchDataField>()? {
         match field {
            SearchDataField::Result => return Err(serde::de::Error::custom("found")),
            SearchDataField::List => {
               map.next_value_seed(Self {
                  reject_object: true,
               })?;
            },
            SearchDataField::Other => {
               map.next_value_seed(Self {
                  reject_object: false,
               })?;
            },
         }
      }
      Ok(())
   }
}

struct SearchRootKey(bool);

impl<'de> Deserialize<'de> for SearchRootKey {
   fn deserialize<D>(deserializer: D) -> StdResult<Self, D::Error>
   where
      D: serde::Deserializer<'de>,
   {
      deserializer.deserialize_str(SearchRootKeyVisitor).map(Self)
   }
}

struct SearchRootKeyVisitor;

#[expect(
   clippy::missing_trait_methods,
   reason = "serde Visitor defaults reject non-string keys"
)]
impl serde::de::Visitor<'_> for SearchRootKeyVisitor {
   type Value = bool;

   fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
      formatter.write_str("a JSON object key")
   }

   fn visit_str<E>(self, value: &str) -> StdResult<bool, E>
   where
      E: serde::de::Error,
   {
      Ok(value == "data")
   }
}

struct SearchRoot;

impl<'de> Deserialize<'de> for SearchRoot {
   fn deserialize<D>(deserializer: D) -> StdResult<Self, D::Error>
   where
      D: serde::Deserializer<'de>,
   {
      deserializer.deserialize_map(SearchRootVisitor)
   }
}

struct SearchRootVisitor;

#[expect(
   clippy::missing_trait_methods,
   reason = "serde Visitor defaults reject non-object roots"
)]
impl<'de> serde::de::Visitor<'de> for SearchRootVisitor {
   type Value = SearchRoot;

   fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
      formatter.write_str("a GraphQL response object")
   }

   fn visit_map<A>(self, mut map: A) -> StdResult<SearchRoot, A::Error>
   where
      A: serde::de::MapAccess<'de>,
   {
      while let Some(SearchRootKey(is_data)) = map.next_key()? {
         if is_data {
            map.next_value_seed(SearchScan {
               reject_object: false,
            })?;
         } else {
            map.next_value::<serde::de::IgnoredAny>()?;
         }
      }
      Ok(SearchRoot)
   }
}

/// A decoded search page with no tweet, user, or list results is inconclusive
/// about account health.
pub(super) fn search_answer_is_empty(bytes: &[u8]) -> bool {
   serde_json::from_slice::<SearchRoot>(bytes).is_ok()
}

/// X's own message when it failed a search on its side: `search_timeline`
/// came back empty with an error on its `timeline` field, in practice
/// "Internal server error".
///
/// This is a statement about the query or X's backend, never about the
/// account. One query that trips it gets the same answer from every account
/// it is sent to, 80 ms apart, and an account that got it answers its next
/// search with posts. It was read as a suspension, which moved 364 working
/// accounts out of search in two days, six per query, and retired 61 of them.
///
/// Rate-limit, denylist, credential-lock, and suspension errors are excluded so
/// they go through the normal error path and switch accounts rather than
/// turning into an empty page.
fn search_timeline_failure<'a>(bytes: &[u8], errors: &'a [ApiError]) -> Option<&'a str> {
   let failure = errors.iter().find(|err| {
      err.path.as_slice() == ["search_by_raw_query", "search_timeline", "timeline"]
         && !is_denylisted(&err.message)
         && !is_rate_limit_error(err)
         && !is_session_error(err)
         && !matches!(
            TwitterError::from_code(err.code),
            Some(TwitterError::UserSuspended)
         )
   })?;
   serde_json::from_slice::<serde_json::Value>(bytes)
      .is_ok_and(|body| {
         body
            .pointer("/data/search_by_raw_query/search_timeline")
            .and_then(serde_json::Value::as_object)
            .is_some_and(serde_json::Map::is_empty)
      })
      .then_some(failure.message.as_str())
}

/// The errors that fail a request. X returns the page plus an error for
/// each field it could not fill, such as a list banner it cannot decode or
/// one post's long-form text. Such an error names the field in `path`, so
/// with `data` present the rest of the page stands. Denylisted queries,
/// rate-limit errors, and session credential rejections always keep their
/// meaning wherever X reports them.
fn request_errors(errors: &[ApiError], has_data: bool) -> Vec<ApiError> {
   errors
      .iter()
      .filter(|err| {
         let one_field = !err.path.is_empty() || err.message.contains("DecodeException");
         let critical =
            is_denylisted(&err.message) || is_rate_limit_error(err) || is_session_error(err);
         !(has_data && one_field && !critical)
      })
      .map(|err| {
         ApiError {
            code:    err.code,
            message: err.message.clone(),
            path:    err.path.clone(),
         }
      })
      .collect()
}

/// Community-cache strings are written by third parties, so cap them before
/// they reach a page.
fn clamp_community_value(value: Option<String>) -> String {
   value
      .map(|value| value.trim().chars().take(100).collect())
      .unwrap_or_default()
}

fn space_id_from_url(url: &str) -> Option<&str> {
   url.split("/spaces/")
      .nth(1)?
      .split(['/', '?', '#'])
      .next()
      .filter(|id| !id.is_empty())
}

fn millis_to_time(milliseconds: i64) -> Option<time::OffsetDateTime> {
   time::OffsetDateTime::from_unix_timestamp(milliseconds.checked_div(1_000)?).ok()
}

fn audio_space_status(metadata: &AudioSpaceMetadata) -> String {
   match metadata.state.as_deref() {
      Some("NotStarted") => {
         metadata
            .scheduled_start
            .and_then(millis_to_time)
            .map_or_else(
               || "Scheduled".to_owned(),
               |time| format!("Scheduled · {}", formatters::format_tweet_time(time)),
            )
      },
      Some("Running") => {
         metadata
            .total_live_listeners
            .filter(|count| *count > 0)
            .map_or_else(
               || "Live now".to_owned(),
               |count| {
                  format!(
                     "Live now · {} listening",
                     formatters::abbreviate_number(count)
                  )
               },
            )
      },
      Some("Ended" | "TimedOut") if metadata.is_space_available_for_replay.unwrap_or(false) => {
         metadata
            .total_replay_watched
            .filter(|count| *count > 0)
            .map_or_else(
               || "Replay available".to_owned(),
               |count| {
                  format!(
                     "Replay available · {} plays",
                     formatters::abbreviate_number(count)
                  )
               },
            )
      },
      Some("Ended" | "TimedOut") => "Space ended".to_owned(),
      _ => String::new(),
   }
}

fn audio_space_host(metadata: &AudioSpaceMetadata) -> String {
   let Some(user) = metadata
      .creator_results
      .as_ref()
      .and_then(|results| results.result.as_deref())
   else {
      return "X Space".to_owned();
   };
   let name = user
      .core
      .as_ref()
      .and_then(|core| core.name.as_deref())
      .or_else(|| {
         let legacy = user.legacy.as_ref()?;
         legacy.name.as_deref()
      })
      .unwrap_or_default();
   let username = user
      .core
      .as_ref()
      .and_then(|core| core.screen_name.as_deref())
      .or_else(|| {
         let legacy = user.legacy.as_ref()?;
         legacy.screen_name.as_deref()
      })
      .unwrap_or_default();

   hosted_by(name, username).unwrap_or_else(|| "X Space".to_owned())
}

fn hosted_by(name: &str, username: &str) -> Option<String> {
   match (name.is_empty(), username.is_empty()) {
      (false, false) => Some(format!("Hosted by {name} (@{username})")),
      (false, true) => Some(format!("Hosted by {name}")),
      (true, false) => Some(format!("Hosted by @{username}")),
      (true, true) => None,
   }
}

fn broadcast_id_from_url(url: &str) -> Option<&str> {
   url.split("/broadcasts/")
      .nth(1)?
      .split(['/', '?', '#'])
      .next()
      .filter(|id| !id.is_empty())
}

fn broadcast_status(metadata: &BroadcastMetadata) -> String {
   match metadata.state.as_str() {
      "RUNNING" => {
         metadata
            .total_watching
            .filter(|count| *count > 0)
            .map_or_else(
               || "Live now".to_owned(),
               |count| {
                  format!(
                     "Live now · {} watching",
                     formatters::abbreviate_number(count)
                  )
               },
            )
      },
      "ENDED" if metadata.available_for_replay => {
         metadata
            .total_watched
            .filter(|count| *count > 0)
            .map_or_else(
               || "Replay available".to_owned(),
               |count| {
                  format!(
                     "Replay available · {} views",
                     formatters::abbreviate_number(count)
                  )
               },
            )
      },
      "ENDED" => {
         metadata
            .total_watched
            .filter(|count| *count > 0)
            .map_or_else(
               || "Broadcast ended".to_owned(),
               |count| {
                  format!(
                     "Broadcast ended · {} views",
                     formatters::abbreviate_number(count)
                  )
               },
            )
      },
      _ => String::new(),
   }
}

fn article_tweet_data<'a>(data: &'a ConversationData, tweet_id: &str) -> Option<&'a TweetData> {
   let raw = data
      .tweet_result
      .as_ref()
      .and_then(|nested| nested.result.as_deref())
      .or_else(|| {
         data
            .threaded_conversation_with_injections_v2
            .as_ref()?
            .instructions
            .iter()
            .filter_map(|instruction| instruction.entries.as_deref())
            .flatten()
            .find(|entry| {
               entry
                  .entry_id_str()
                  .starts_with(&format!("tweet-{tweet_id}"))
            })
            .and_then(|entry| entry.tweet_result())
      })?;

   raw.tweet.as_deref().or(Some(raw))
}

/// Cloudflare's "you have been blocked" page. That is the exit IP, not the
/// account.
fn cloudflare_ip_ban(status: axum::http::StatusCode, body: &str) -> bool {
   if status.as_u16() != 403 {
      return false;
   }
   let lower = body.to_ascii_lowercase();
   lower.contains("sorry, you have been blocked")
      || lower.contains("attention required")
      || lower.contains("cf-error-details")
}

fn cloudflare_ray(body: &str) -> Option<String> {
   let tail = body.split("Ray ID:").nth(1)?;
   let mut run = String::new();
   for ch in tail.chars() {
      if ch.is_ascii_hexdigit() {
         run.push(ch);
         if run.len() == 16 {
            return Some(run);
         }
      } else if !run.is_empty() {
         run.clear();
      }
   }
   None
}

/// What to write in a log. A Cloudflare HTML page is one line, not the
/// document.
///
/// Only a body that actually starts with markup is called HTML. Reporting
/// every long body as an "HTML error page" described X's own JSON refusals as
/// edge blocks, which sent a search for a 403 in the wrong direction.
fn log_upstream(body: &str) -> String {
   let trimmed = body.trim_start();
   if trimmed.starts_with('<') {
      if let Some(ray) = cloudflare_ray(body) {
         return format!("Cloudflare block page, ray {ray}");
      }
      return format!("HTML error page, {} bytes", body.len());
   }
   if body.len() > 240 {
      return format!("{} bytes: {}", body.len(), &body[..240.min(body.len())]);
   }
   body.to_owned()
}

/// The `ct0` in one `set-cookie` line, when it is the one X just issued.
pub(super) fn ct0_cookie(value: &header::HeaderValue) -> Option<String> {
   let text = value.to_str().ok()?;
   let (pair, _) = text.split_once(';').unwrap_or((text, ""));
   let (name, cookie) = pair.split_once('=')?;
   (name.trim() == "ct0" && !cookie.trim().is_empty()).then(|| cookie.trim().to_owned())
}

pub(super) fn twitter_error_code(body: &str) -> Option<i64> {
   #[derive(Deserialize)]
   struct ErrorCheck {
      errors: Option<Vec<CodeOnly>>,
   }
   #[derive(Deserialize)]
   struct CodeOnly {
      #[serde(default)]
      code: i64,
   }
   serde_json::from_str::<ErrorCheck>(body)
      .ok()?
      .errors?
      .into_iter()
      .map(|error| error.code)
      .find(|code| *code != 0)
}

fn is_settings_not_found(endpoint: &str, body: &str) -> bool {
   endpoint == endpoints::ACCOUNT_SETTINGS && twitter_error_code(body) == Some(34)
}

fn is_settings_probe_endpoint(endpoint: &str) -> bool {
   endpoint == endpoints::ACCOUNT_SETTINGS
      || endpoint == endpoints::VERIFY_CREDENTIALS
      || endpoint == endpoints::SEARCH_SAFETY
      || endpoint.starts_with("strato/")
}

#[cfg(test)]
#[test]
fn code_34_is_special_only_for_account_settings() {
   let missing = r#"{"errors":[{"code":34}]}"#;
   assert!(is_settings_not_found(endpoints::ACCOUNT_SETTINGS, missing));
   assert!(!is_settings_not_found(
      endpoints::GRAPH_SEARCH_TIMELINE,
      missing
   ));
   assert!(!is_settings_not_found(endpoints::ACCOUNT_SETTINGS, ""));
   assert!(!is_settings_not_found(
      endpoints::ACCOUNT_SETTINGS,
      r#"{"errors":[{"code":64}]}"#
   ));
}

#[cfg(test)]
#[test]
fn auxiliary_endpoints_use_settings_suspension_probe() {
   assert!(is_settings_probe_endpoint(endpoints::ACCOUNT_SETTINGS));
   assert!(is_settings_probe_endpoint(endpoints::VERIFY_CREDENTIALS));
   assert!(is_settings_probe_endpoint(endpoints::SEARCH_SAFETY));
   assert!(is_settings_probe_endpoint(endpoints::STRATO_TRANSLATE));
   assert!(!is_settings_probe_endpoint(
      endpoints::GRAPH_SEARCH_TIMELINE
   ));
   assert!(!is_settings_probe_endpoint(endpoints::GRAPH_USER_TWEETS));
}

const INFLIGHT_SHARDS: usize = 64;

type InflightShard = StdMutex<HashMap<String, watch::Sender<Option<bytes::Bytes>>>>;

fn inflight_shard_index(key: &str) -> usize {
   let mut hash = 0xCBF2_9CE4_8422_2325_u64;
   for byte in key.as_bytes() {
      hash ^= u64::from(*byte);
      hash = hash.wrapping_mul(0x100_0000_01B3);
   }
   let mixed = (hash ^ (hash >> 33)).wrapping_mul(0xFF51_AFD7_ED55_8CCD);
   usize::try_from((mixed >> 58) % (INFLIGHT_SHARDS as u64)).unwrap_or(0)
}

struct InflightCoalescer {
   shards: Box<[InflightShard]>,
}

impl InflightCoalescer {
   fn new() -> Self {
      let shards = iter::repeat_with(|| StdMutex::new(HashMap::new()))
         .take(INFLIGHT_SHARDS)
         .collect::<Vec<_>>()
         .into_boxed_slice();
      Self { shards }
   }

   fn join_or_lead(self: &Arc<Self>, key: String) -> InflightTicket {
      let shard_index = inflight_shard_index(&key);
      let mut map = self.shards[shard_index]
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner);
      if let Some(tx) = map.get(&key) {
         let rx = tx.subscribe();
         drop(map);
         return InflightTicket::Follower(rx);
      }
      let (tx, _rx) = watch::channel(None);
      map.insert(key.clone(), tx.clone());
      drop(map);
      InflightTicket::Leader(InflightGuard {
         coalescer: Arc::clone(self),
         shard_index,
         key,
         tx: Some(tx),
      })
   }

   fn remove_at(&self, shard_index: usize, key: &str) {
      if let Ok(mut map) = self.shards[shard_index].lock() {
         map.remove(key);
      }
   }

   #[cfg(test)]
   fn len(&self) -> usize {
      self
         .shards
         .iter()
         .map(|shard| {
            shard
               .lock()
               .unwrap_or_else(std::sync::PoisonError::into_inner)
               .len()
         })
         .sum()
   }

   #[cfg(test)]
   fn is_empty(&self) -> bool {
      self.len() == 0
   }
}

pub(super) enum InflightTicket {
   Leader(InflightGuard),
   Follower(watch::Receiver<Option<bytes::Bytes>>),
}

pub(super) struct InflightGuard {
   coalescer:   Arc<InflightCoalescer>,
   shard_index: usize,
   key:         String,
   tx:          Option<watch::Sender<Option<bytes::Bytes>>>,
}

impl InflightGuard {
   pub(super) fn complete(mut self, bytes: bytes::Bytes) {
      self.coalescer.remove_at(self.shard_index, &self.key);
      if let Some(tx) = self.tx.take() {
         let _ = tx.send(Some(bytes));
      }
   }
}

impl Drop for InflightGuard {
   fn drop(&mut self) {
      if self.tx.is_some() {
         self.coalescer.remove_at(self.shard_index, &self.key);
      }
   }
}

/// Twitter/X API client.
#[derive(Clone)]
pub struct ApiClient {
   pub(crate) client:        HttpClient,
   pub(crate) sessions:      SessionPool,
   proxies:                  Option<Arc<ProxyPool>>,
   tid:                      TidClient,
   operations:               Operations,
   budget:                   ClientBudget,
   /// Retweeter lists per caller, on whatever `clientBudget` says.
   list_budget:              ClientBudget,
   pub(crate) tid_enabled:   bool,
   denylist:                 super::search_denylist::SearchDenylist,
   latency:                  Arc<Latency>,
   hedges:                   Arc<Semaphore>,
   /// Which call served each first profile page, reported every minute.
   timeline_routes:          Arc<TimelineRoutes>,
   /// How far the minute's fallback share is past the flood trigger. The
   /// Anti-DDoS gate reads it on every request.
   pressure:                 Arc<Pressure>,
   /// Coalesces identical concurrent in-flight GraphQL calls; entries live
   /// only while the upstream request is in flight and are removed on drop.
   inflight:                 Arc<InflightCoalescer>,
   telemetry:                Option<crate::stats::StatsStore>,
   /// Cloudflare JSD solver + the circuit breaker that disables it after one
   /// failure. Search attaches the `cf_clearance` it mints.
   clearance:                super::clearance::ClearanceGate,
   pub(crate) cookies:       Arc<Jar>,
   /// Set when accounts arrive through `/import`, so their filters are turned
   /// off as soon as they are in the pool instead of on the next rescan.
   pub(crate) filter_wakeup:       Arc<tokio::sync::Notify>,
   /// Set when accounts arrive through `/import` or move into the search pool,
   /// so unverified search accounts are checked right away instead of waiting
   /// for the next periodic scan.
   pub(crate) search_check_wakeup: Arc<tokio::sync::Notify>,
   /// Wakes the account reconciliation task immediately after `/import`.
   reconcile_wakeup:               Arc<tokio::sync::Notify>,
}

impl ApiClient {
   /// Identity headers (user agent, client hints, languages, encodings) come
   /// from each session's browser profile, so none are set here.
   pub async fn new(
      config: &Config,
      sessions: SessionPool,
      proxies: Option<ProxyPool>,
      fallback_proxies: Vec<ProxyConfig>,
      accounts: AccountStore,
   ) -> Result<Self> {
      // Direct first, on the server's own address. apiProxy is only the
      // residential exit used after an HTML 429.
      let mut client = HttpClient::new("", "", Purpose::Api);
      if !config.config.api_proxy.is_empty() {
         client = client.with_fallback_proxy(&config.config.api_proxy, &config.config.proxy_auth);
      } else if let Some(ref pool) = proxies {
         client = client.with_default_proxy(pool.first());
      }
      if let Some(ref pool) = proxies {
         client = client.with_primary_proxies(pool.all());
      }
      if !fallback_proxies.is_empty() {
         client = client.with_fallback_proxies(fallback_proxies);
      }
      // Every account keeps the cookies X hands it, so a `guest_id` or `twid`
      // set on one answer is sent back on the next call instead of being
      // dropped. Nothing is fetched to fill this: it only reads the responses
      // the calls in hand already receive.
      let cookies = Jar::from_store(&accounts).await?;
      cookies.spawn_flusher(accounts.clone());
      client = client.with_cookies(Arc::clone(&cookies));

      let proxies = proxies.map(Arc::new);
      let operations = Operations::load();
      // The script chunks are fetched by a client of their own, and it leaves
      // through the same exits as everything else: abs.twimg.com is X's edge,
      // and a chunk fetched from this host's own address is a signal a browser
      // never sends.
      let mut scripts = HttpClient::new("", "", Purpose::Script);
      if let Some(ref pool) = proxies {
         scripts = scripts.with_primary_proxies(pool.all());
      }
      let tid = TidClient::new(
         client.clone(),
         scripts,
         sessions.clone(),
         proxies.clone(),
         operations.clone(),
      );

      let denylist_path = std::env::var("TEAPOT_SEARCH_DENYLIST")
         .unwrap_or_else(|_| "search-denylist.json".to_owned());
      // Accounts that arrive through `/import` are reconciled into the pool
      // immediately when woken or every ten seconds; `filter_wakeup` tells
      // the filter sync they are there and `search_check_wakeup` wakes the
      // search audit so newly imported search accounts do not wait half an hour.
      let filter_wakeup = Arc::new(tokio::sync::Notify::new());
      let search_check_wakeup = Arc::new(tokio::sync::Notify::new());
      let reconcile_wakeup = Arc::new(tokio::sync::Notify::new());
      Self::spawn_account_reconcile(
         accounts,
         sessions.clone(),
         Arc::clone(&cookies),
         Arc::clone(&filter_wakeup),
         Arc::clone(&search_check_wakeup),
         Arc::clone(&reconcile_wakeup),
      );

      Ok(Self {
         client,
         sessions,
         proxies,
         tid,
         operations,
         budget: ClientBudget::new(config.config.client_budget),
         list_budget: ClientBudget::with_tiers(true, &budget::LIST_PAGE_TIERS),
         tid_enabled: !config.config.disable_tid,
         denylist: super::search_denylist::SearchDenylist::load(denylist_path),
         latency: Arc::new(Latency::default()),
         hedges: Arc::new(Semaphore::new(MAX_HEDGES)),
         timeline_routes: Arc::new(TimelineRoutes::default()),
         pressure: Arc::new(Pressure::with_sustained(
            config.anti_ddos.flood_percent,
            config.anti_ddos.sustained_percent,
            config.anti_ddos.sustained_minutes,
            config.anti_ddos.flood_hold_minutes,
         )),
         inflight: Arc::new(InflightCoalescer::new()),
         telemetry: None,
         clearance: super::clearance::ClearanceGate::new(config.config.search_clearance),
         cookies,
         filter_wakeup,
         search_check_wakeup,
         reconcile_wakeup,
      })
   }

   pub(crate) fn with_telemetry(mut self, telemetry: Option<crate::stats::StatsStore>) -> Self {
      self.telemetry = telemetry;
      self
   }

   /// The pool-pressure verdict the Anti-DDoS gate reads.
   #[must_use]
   pub fn pressure(&self) -> &Arc<Pressure> {
      &self.pressure
   }

   /// The clearance flow's state, for the admin dashboard.
   #[must_use]
   pub fn clearance_status(&self) -> super::clearance::ClearanceStatus {
      self.clearance.status()
   }

   /// Before a search, make sure this session holds a fresh `cf_clearance`.
   ///
   /// Solving visits x.com, reads Cloudflare's JSD challenge and posts the
   /// oneshot; the cookie it returns lands in the session's jar and is sent on
   /// the search that follows. This is best-effort and never blocks search: a
   /// session that already has a fresh cookie, a non-Chrome identity, or a
   /// tripped breaker returns at once. The first solve failure trips the
   /// breaker, after which no further x.com clearance requests are made and
   /// search continues without the cookie.
   pub(crate) async fn ensure_search_clearance(&self, session: &SessionLease) {
      if session.kind != SessionKind::Cookie || !self.clearance.should_solve(session.id) {
         return;
      }
      // The JSD payload describes a Chrome desktop; only mint for Chrome
      // sessions so the token names the browser the requests present as.
      let identity = super::browser::identity_for(session.id);
      if identity.engine != super::browser::Engine::Chrome {
         return;
      }
      let Some(user_agent) = super::browser::profile_header(session.id, "user-agent") else {
         return;
      };
      let info = super::clearance::ClientInfo {
         user_agent,
         sec_ch_ua: super::browser::profile_header(session.id, "sec-ch-ua").unwrap_or_default(),
         sec_ch_ua_platform: super::browser::profile_header(session.id, "sec-ch-ua-platform")
            .unwrap_or_else(|| super::browser::sec_ch_ua_platform(identity.os)),
         navigator_platform: super::browser::navigator_platform(identity.os).to_owned(),
      };

      // One solve at a time. Re-check under the permit: a sibling may have
      // minted for this session, or tripped the breaker, while we waited.
      let _permit = self.clearance.solve_permit().await;
      if !self.clearance.should_solve(session.id) {
         return;
      }
      self.clearance.record_attempt();
      let egress = self.egress_for(session);
      let target = super::clearance::ClearanceTarget::x_com();
      match super::clearance::solve(&self.client, &egress, &info, &target).await {
         Ok(_cookie) => {
            self.clearance.mark_ok(session.id);
            tracing::info!(session_id = session.id, "cf_clearance minted for search");
         },
         Err(err) if err.is_changed() => {
            // Cloudflare changed the challenge: every solve would fail the same
            // way, so disable the whole flow until restart (one strike).
            let reason = err.reason();
            self.clearance.record_failure(reason, false);
            self.clearance.trip(reason);
            tracing::warn!(
               session_id = session.id,
               reason = %reason,
               "cf_clearance challenge changed; disabling the clearance flow until restart"
            );
         },
         Err(err) => {
            // A one-off (bad response, network blip, declined mint): keep the
            // flow alive and let another session retry. Only a long streak of
            // these trips the breaker, so a persistent fault stops hammering.
            let reason = err.reason();
            let streak = self.clearance.record_failure(reason, true);
            if streak >= super::clearance::max_failure_streak() {
               self.clearance.trip(&format!(
                  "{streak} consecutive clearance failures; last: {reason}"
               ));
               tracing::warn!(
                  session_id = session.id,
                  streak,
                  reason = %reason,
                  "too many consecutive cf_clearance failures; disabling the clearance flow until restart"
               );
            } else {
               tracing::info!(
                  session_id = session.id,
                  streak,
                  reason = %reason,
                  "cf_clearance solve failed (transient); flow stays enabled"
               );
            }
         },
      }
   }

   pub(crate) async fn cookie_snapshot(&self, session_id: i64) -> Option<Vec<EditorCookie>> {
      let (auth_token, ct0) = self.sessions.cookie_credentials(session_id)?;
      if auth_token.is_empty() {
         return None;
      }
      let cookies = self.cookies.snapshot(session_id).await;
      Some(super::cookies::export_cookies(cookies, auth_token, ct0))
   }

   pub(crate) async fn search_budget_estimate(&self) -> f64 {
      // The documented SearchTimeline budget is 50 calls in 15 minutes.
      self.sessions.available_for("SearchTimeline").await as f64 * (50.0 / 15.0)
   }

   /// Wake the account reconciliation task so newly imported accounts join the
   /// live pool without waiting for the periodic tick.
   pub fn wake_account_reconcile(&self) {
      self.reconcile_wakeup.notify_one();
   }

   /// Wake the search check task so newly added search accounts start verifying
   /// without waiting for the periodic scan.
   pub fn wake_search_check(&self) {
      self.search_check_wakeup.notify_one();
   }

   fn spawn_account_reconcile(
      store: AccountStore,
      pool: SessionPool,
      cookies: Arc<Jar>,
      filter_wakeup: Arc<tokio::sync::Notify>,
      search_check_wakeup: Arc<tokio::sync::Notify>,
      reconcile_wakeup: Arc<tokio::sync::Notify>,
   ) {
      tokio::spawn(async move {
         let mut every = interval(ACCOUNT_RECONCILE_EVERY);
         every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
         every.tick().await;
         loop {
            tokio::select! {
               _ = every.tick() => {},
               () = reconcile_wakeup.notified() => {},
            }
            match Self::reconcile_accounts(&store, &pool, &cookies).await {
               Ok(0) => {},
               Ok(added) => {
                  tracing::info!(added, "added imported accounts to the live pool");
                  // New accounts hide NSFW media until their settings are
                  // written, and new search accounts need verification before
                  // serving visitors, so both background workers are woken.
                  filter_wakeup.notify_one();
                  search_check_wakeup.notify_one();
               },
               Err(err) => tracing::warn!("could not reconcile imported accounts: {err}"),
            }
         }
      });
   }

   async fn reconcile_accounts(
      store: &AccountStore,
      pool: &SessionPool,
      cookies: &Jar,
   ) -> Result<usize> {
      // Inventory is append-only. Avoid fetching credentials and cookies at
      // steady state; a count increase is enough to trigger an append pass.
      if store.stats().await?.total <= i64::try_from(pool.len()).unwrap_or(i64::MAX) {
         return Ok(0);
      }
      let known = pool
         .session_ids()
         .into_iter()
         .collect::<std::collections::HashSet<_>>();
      let mut new = store.load().await?;
      new.retain(|session| !known.contains(&session.id));
      if new.is_empty() {
         return Ok(0);
      }
      cookies
         .seed_imports(&new, store.load_cookies().await?)
         .await;
      // ProxyPool's immutable legacy pins and deterministic mappings are
      // already defined for these ids. No lease can see an unbound slot.
      Ok(pool.append_sessions(new).await)
   }

   /// Warm the transaction ID client and live GraphQL query IDs in the
   /// background at startup so the first user request does not block on them.
   pub fn spawn_tid_warmup(&self) {
      if !self.tid_enabled {
         return;
      }
      let tid = self.tid.clone();
      tokio::spawn(async move {
         tid.warmup().await;
      });
   }

   /// Log how long X takes per operation, how often a second copy went out
   /// and won, and which exits are parked. Exits are also ranked by how far
   /// their answers trail the fleet, and accounts on the slow ones go last.
   pub fn spawn_latency_report(&self) {
      let api = self.clone();
      tokio::spawn(async move {
         let mut every = interval(LATENCY_REPORT_EVERY);
         every.tick().await;
         loop {
            every.tick().await;
            for line in api.latency.report() {
               tracing::info!("upstream {line}");
            }
            if let Some(line) = log_events::report() {
               tracing::info!("upstream events {line}");
            }
            for line in api.sessions.queue_report().await {
               tracing::info!("account queue {line}");
            }
            let eligible_accounts = api.user_timeline_accounts().await;
            if let Some(report) = api.timeline_routes.report(eligible_accounts) {
               match api
                  .pressure
                  .observe(report.off_primary, report.total, crate::pressure::now())
               {
                  Some(crate::pressure::Change::Raised { share, cause }) => {
                     let trigger = match cause {
                        crate::pressure::Cause::Spike => "single-minute spike",
                        crate::pressure::Cause::Sustained => "sustained fallback streak",
                     };
                     tracing::warn!(
                        share,
                        cause = ?cause,
                        hold_minutes = api.pressure.hold_minutes(),
                        "Anti-DDoS gate on ({trigger}): {share}% of profile pages ran past the \
                         account pool; every visitor is checked until the flood stops"
                     );
                  },
                  Some(crate::pressure::Change::Lifted) => {
                     tracing::info!("Anti-DDoS gate off: the pool keeps up with demand again");
                  },
                  None => {},
               }
               if report.fallback_needed {
                  tracing::warn!("profile timelines {}", report.line);
               } else {
                  tracing::info!("profile timelines {}", report.line);
               }
            }
            let parked = api.client.parked_exits();
            if !parked.is_empty() {
               tracing::info!(?parked, "proxy exits parked (port, seconds left)");
            }
         }
      });
      if self.proxies.is_none() {
         return;
      }
      let ranker = self.clone();
      tokio::spawn(async move {
         let mut every = interval(EXIT_RANK_EVERY);
         let mut going_last = Vec::new();
         loop {
            every.tick().await;
            let slow = ranker.latency.slow_exits();
            let mut ports = slow.iter().map(|&(port, _)| port).collect::<Vec<_>>();
            let accounts = ranker.sessions.mark_slow_exits(|id| {
               ranker
                  .pinned_port(id)
                  .is_some_and(|port| ports.contains(&port))
            });
            ports.sort_unstable();
            if ports == going_last {
               continue;
            }
            if slow.is_empty() {
               tracing::info!("every proxy exit answers in step with the fleet again");
            } else {
               tracing::info!(
                  ?slow,
                  accounts,
                  "proxy exits trail the fleet (port, ms behind); their accounts go last"
               );
            }
            going_last = ports;
         }
      });
   }

   /// The proxy port this account is pinned to, when there is a SOCKS pool.
   pub(super) fn pinned_port(&self, session_id: i64) -> Option<u16> {
      self.proxies.as_ref().map(|pool| pool.port_for(session_id))
   }

   pub(super) fn inflight_key(
      endpoint: &str,
      variables: &str,
      field_toggles: Option<&str>,
   ) -> String {
      format!(
         "{endpoint}\0{variables}\0{}",
         field_toggles.unwrap_or_default()
      )
   }

   pub(super) fn join_or_lead(&self, key: String) -> InflightTicket {
      self.inflight.join_or_lead(key)
   }

   pub(super) fn parse_coalesced<T>(bytes: &[u8]) -> Result<T>
   where
      T: DeserializeOwned,
   {
      let resp = serde_json::from_slice::<GqlResponse<T>>(bytes)
         .map_err(|err| Error::Internal(format!("Response parse error: {err}")))?;
      resp
         .data
         .ok_or_else(|| Error::Internal("Response parse error: missing data".into()))
   }

   /// The browser and proxy a request for this session goes out as.
   pub(crate) fn egress_for(&self, session: &SessionLease) -> Egress {
      self.egress_for_id(session.id)
   }

   pub(crate) fn egress_for_id(&self, session_id: i64) -> Egress {
      Egress {
         session: session_id,
         proxy:   self
            .proxies
            .as_ref()
            .map(|pool| pool.for_session(session_id)),
      }
   }

   pub(crate) async fn bearer_and_tid(&self, api_path: &str) -> (&'static str, Option<String>) {
      self.bearer_and_tid_for("GET", api_path).await
   }

   /// Same as [`Self::bearer_and_tid`], with the method the request goes out
   /// as. X hashes the method into the transaction ID, so a POST signed with
   /// the GET variant is refused with a bodyless 404 or a bare code 34.
   pub(crate) async fn bearer_and_tid_for(
      &self,
      method: &str,
      api_path: &str,
   ) -> (&'static str, Option<String>) {
      if !self.tid_enabled {
         return (endpoints::BEARER_TOKEN_NO_TID, None);
      }
      self
         .tid
         .generate_for(method, api_path)
         .await
         .map_or((endpoints::BEARER_TOKEN_NO_TID, None), |tid| {
            (endpoints::BEARER_TOKEN, Some(tid))
         })
   }

   /// Check for API-level errors in the raw response bytes.
   ///
   /// For the GraphQL path the envelope is parsed once and its `errors` go
   /// through [`map_api_errors`](Self::map_api_errors) directly; this is for
   /// bodies that are only ever inspected for an error.
   pub(super) fn check_api_errors(bytes: &[u8]) -> Result<()> {
      #[derive(Deserialize)]
      struct ErrorCheck {
         #[serde(default)]
         errors: Vec<ApiError>,
      }

      let Ok(check) = serde_json::from_slice::<ErrorCheck>(bytes) else {
         return Ok(());
      };
      Self::map_api_errors(&check.errors)
   }

   /// Turn the first of X's `errors` into the matching [`Error`].
   fn map_api_errors(errors: &[ApiError]) -> Result<()> {
      let Some(error) = errors.first() else {
         return Ok(());
      };

      if is_denylisted(&error.message) {
         return Err(Error::SearchDenylisted);
      }

      if is_rate_limit_error(error) {
         return Err(Error::RateLimited);
      }

      if error.code == 214 && error.message.contains("Unknown request cursor") {
         return Err(Error::InvalidUrl(
            "This page has expired. Go back and try again.".into(),
         ));
      }

      if let Some(twitter_err) = TwitterError::from_code(error.code) {
         return match twitter_err {
            TwitterError::UserNotFound | TwitterError::NoUserMatches => {
               Err(Error::UserNotFound(error.message.clone()))
            },
            TwitterError::ProtectedUser => Err(Error::ProtectedUser(error.message.clone())),
            TwitterError::UserSuspended => Err(Error::UserSuspended(error.message.clone())),
            TwitterError::RateLimited => Err(Error::RateLimited),
            TwitterError::TweetNotFound
            | TwitterError::TweetUnavailable
            | TwitterError::NoStatusFound
            | TwitterError::TweetUnavailable421
            | TwitterError::TweetCensored => Err(Error::TweetNotFound(error.message.clone())),
            // 326 is *our* cookie session locked, not the profile the reader
            // opened. Mapping it to UserSuspended made live accounts look
            // banned until a refresh hit a different session.
            TwitterError::Locked | TwitterError::InvalidToken | TwitterError::BadToken => {
               Err(Error::SessionRejected(error.message.clone()))
            },
         };
      }

      Err(Error::TwitterApi(format!(
         "Error {}: {}",
         error.code, error.message
      )))
   }

   /// Make a GraphQL request to the Twitter API.
   ///
   /// Identical concurrent calls in flight at the same instant share one
   /// upstream request; the in-flight entry is removed immediately upon
   /// completion so nothing is stored in memory after the request finishes.
   async fn graphql_request<T>(
      &self,
      endpoint: &str,
      variables: &str,
      features: &str,
      field_toggles: Option<&str>,
   ) -> Result<T>
   where
      T: DeserializeOwned,
   {
      let key = if endpoint.ends_with("/SearchTimeline") && budget::has_search_pass() {
         format!(
            "pass\0{}",
            Self::inflight_key(endpoint, variables, field_toggles)
         )
      } else {
         Self::inflight_key(endpoint, variables, field_toggles)
      };
      let guard = match self.join_or_lead(key) {
         InflightTicket::Follower(mut rx) => {
            if rx.changed().await.is_ok()
               && let Some(bytes) = rx.borrow_and_update().clone()
               && let Ok(parsed) = Self::parse_coalesced::<T>(&bytes)
            {
               return Ok(parsed);
            }
            None
         },
         InflightTicket::Leader(guard) => Some(guard),
      };

      let session = self.charged_session(endpoint, None).await?;
      let (data, bytes) = self
         .graphql_request_with_session(session, endpoint, variables, features, field_toggles, None)
         .await?;
      if let Some(guard) = guard {
         guard.complete(bytes);
      }
      Ok(data)
   }

   /// `retry_kind` pins the replacement when the caller picked the endpoint and
   /// variables from the first session's kind, since an OAuth request retried
   /// through a cookie session would still send the OAuth endpoint.
   async fn graphql_request_with_session<T>(
      &self,
      session: SessionLease,
      endpoint: &str,
      variables: &str,
      features: &str,
      field_toggles: Option<&str>,
      retry_kind: Option<SessionKind>,
   ) -> Result<(T, bytes::Bytes)>
   where
      T: DeserializeOwned,
   {
      let is_search = endpoint.ends_with("/SearchTimeline");
      let is_tweet_search = is_search && is_tweet_search_variables(variables);
      let mut tried_ids = vec![session.id];
      let mut bad_ports = Vec::new();
      let mut failed_search_sessions = Vec::new();
      let (answered_on, mut last) = self
         .hedged_attempt(
            session,
            endpoint,
            variables,
            features,
            field_toggles,
            retry_kind,
         )
         .await;
      let mut session = Some(answered_on);

      // Quota failures, ambiguous search 404s, and SearchTimeline backend
      // failures may walk up to RATE_LIMIT_REPLACEMENTS distinct fallback
      // accounts. Other errors keep their smaller limits inside the match.
      for replacement in 0..RATE_LIMIT_REPLACEMENTS {
         let (session_id, session_kind) = {
            let current = session.as_ref().expect("active lease during retry loop");
            (current.id, current.kind)
         };
         if !tried_ids.contains(&session_id) {
            tried_ids.push(session_id);
         }
         if matches!(last, Err(Error::SearchTimelineFailed))
            && !failed_search_sessions.contains(&session_id)
         {
            failed_search_sessions.push(session_id);
         }
         let empty = matches!(
            last,
            Err(Error::TransientUpstream | Error::SearchRefused | Error::SearchTimelineFailed)
         );
         let exit_fault = matches!(&last, Err(Error::IpThrottled))
            || matches!(&last, Err(Error::TwitterApi(message)) if is_gateway_blip(message))
            || matches!(&last, Err(Error::Internal(message)) if message == "API headers stalled");
         if exit_fault
            && let Some(port) = self.pinned_port(session_id)
            && !bad_ports.contains(&port)
         {
            bad_ports.push(port);
         }
         let retry = match &last {
            Err(Error::SessionRejected(_)) if replacement < GRAPHQL_REPLACEMENTS => true,
            Err(Error::SearchRefused) => session_kind == SessionKind::Cookie,
            Err(Error::SearchTimelineFailed) => {
               session_kind == SessionKind::Cookie
                  && self
                     .sessions
                     .has_unlimited_excluding(endpoint, retry_kind, &tried_ids)
                     .await
            },
            Err(Error::TransientUpstream) => {
               replacement == 0 && session_kind == SessionKind::Cookie
            },
            // A 502/503/504 is the edge dropping the stream, not this account's
            // quota. Try one other session. Do not mark the first as refused:
            // that path is for an account X answers with an empty 404.
            Err(Error::TwitterApi(message)) if is_gateway_blip(message) && replacement == 0 => true,
            // Headers never arrived, so the pooled connection is the thing
            // that stalled. Another account is pinned to another exit.
            Err(Error::Internal(message))
               if message == "API headers stalled" && replacement == 0 =>
            {
               true
            },
            // The exit IP was blocked. Keep moving to other accounts, which are
            // pinned to other exits, instead of handing the 429 to the visitor.
            Err(Error::IpThrottled) => true,
            Err(Error::RateLimited) => {
               self
                  .sessions
                  .has_unlimited_excluding(endpoint, retry_kind, &tried_ids)
                  .await
            },
            _ => false,
         };
         if !retry {
            break;
         }

         if !empty {
            log_events::note(Event::SessionRetry);
            tracing::debug!(
               session_id,
               endpoint,
               "session unusable, retrying on another"
            );
         }
         drop(session.take());
         self.charge(endpoint).await?;
         let next_session = if bad_ports.is_empty() {
            self
               .sessions
               .acquire_where(endpoint, retry_kind, |id| !tried_ids.contains(&id))
               .await
         } else {
            match self
               .sessions
               .acquire_where(endpoint, retry_kind, |id| {
                  !tried_ids.contains(&id)
                     && self
                        .pinned_port(id)
                        .is_none_or(|port| !bad_ports.contains(&port))
               })
               .await
            {
               Ok(next) => Ok(next),
               Err(Error::NoSessions) => {
                  self
                     .sessions
                     .acquire_where(endpoint, retry_kind, |id| !tried_ids.contains(&id))
                     .await
               },
               Err(err) => Err(err),
            }
         };
         let next = match next_session {
            Ok(next) => next,
            Err(_) if matches!(last, Err(Error::SearchRefused)) => {
               self.refund(endpoint).await;
               return Err(Error::SearchRefused);
            },
            Err(_) if matches!(last, Err(Error::SearchTimelineFailed)) => {
               self.refund(endpoint).await;
               break;
            },
            // Never degrade an upstream rate limit into `TransientUpstream`.
            Err(Error::NoSessions | Error::TransientUpstream | Error::RateLimited)
               if matches!(last, Err(Error::RateLimited)) =>
            {
               self.refund(endpoint).await;
               return Err(Error::RateLimited);
            },
            Err(err) => {
               self.refund(endpoint).await;
               return Err(err);
            },
         };
         let (answered_on, next_last) = self
            .hedged_attempt(
               next,
               endpoint,
               variables,
               features,
               field_toggles,
               retry_kind,
            )
            .await;
         session = Some(answered_on);
         last = next_last;
      }

      if matches!(last, Err(Error::SearchTimelineFailed))
         && let Some(ref current) = session
         && !failed_search_sessions.contains(&current.id)
      {
         failed_search_sessions.push(current.id);
      }

      if is_tweet_search && !failed_search_sessions.is_empty() {
         let corroborated = last.is_ok();
         let query_hash = search_query_hash(variables);
         for failed_id in failed_search_sessions {
            let action = self
               .sessions
               .record_search_timeline_failure(failed_id, endpoint, query_hash, corroborated)
               .await;
            if action == super::auth::SearchFailureAction::NeedsCanary {
               self.spawn_search_failure_canary(failed_id);
            }
         }
      }

      // An ambiguous search 404 quarantines its account. Only generic
      // transient failures can retry after refreshing the operation ID.
      if let Some(ref current) = session {
         if current.kind == SessionKind::Cookie && matches!(last, Err(Error::TransientUpstream)) {
            let before = self
               .wire_operation(current, endpoint, features)
               .path
               .as_str()
               .to_owned();
            self.tid.recheck_operations().await;
            let after = self
               .wire_operation(current, endpoint, features)
               .path
               .as_str()
               .to_owned();
            if after != before {
               tracing::info!(
                  from = %before,
                  to = %after,
                  "X answered nothing for a retired query id; sending under the new one"
               );
               last = self
                  .graphql_request_inner(current, endpoint, variables, features, field_toggles)
                  .await;
            }
         }

         if let Ok((_, ref bytes)) = last {
            if is_search {
               if is_tweet_search {
                  let non_empty = !search_answer_is_empty(bytes);
                  self.sessions.record_tweet_search_ok(current.id, non_empty);
                  if non_empty {
                     self.sessions.mark_verified(current.id, endpoint).await;
                  }
               }
            } else {
               self.sessions.mark_verified(current.id, endpoint).await;
            }
         }
      }
      last
   }

   /// Run one background `"news"` tweet search after an account hit repeated
   /// uncorroborated `"Internal server error"` failures while other accounts
   /// were healthy. A healthy account answers `"news"` and resets its streak; a
   /// suspended account fails it and is moved to the view pool.
   fn spawn_search_failure_canary(&self, session_id: i64) {
      let api = self.clone();
      tokio::spawn(async move {
         let outcome = timeout(
            Duration::from_secs(25),
            api.probe_search_capable(session_id),
         )
         .await;
         match outcome {
            Ok(Ok(true)) => {
               api.sessions.record_tweet_search_ok(session_id, true);
            },
            Ok(Err(Error::SearchTimelineFailed)) if api.sessions.tweet_search_healthy() => {
               tracing::info!(
                  session_id,
                  "search account failed canary query with Internal server error; moving to view \
                   pool"
               );
               api.sessions
                  .demote_to_view(
                     session_id,
                     Some(crate::stats::Restriction::SearchBanned),
                     "SearchTimeline failed canary query with Internal server error",
                  )
                  .await;
            },
            Ok(Err(Error::SessionRejected(_))) => {
               api.sessions.mark_rejected(session_id).await;
            },
            _ => {},
         }
         api.sessions.finish_search_canary(session_id);
      });
   }

   /// The first attempt of a call, plus a second copy on another account and
   /// exit once it has run past most of its operation's recent answers.
   /// The first good answer is kept and the other copy is dropped. The copy
   /// is not billed to the caller: it buys latency, not another page.
   async fn hedged_attempt<T>(
      &self,
      session: SessionLease,
      endpoint: &str,
      variables: &str,
      features: &str,
      field_toggles: Option<&str>,
      retry_kind: Option<SessionKind>,
   ) -> (SessionLease, Result<(T, bytes::Bytes)>)
   where
      T: DeserializeOwned,
   {
      let first_id = session.id;
      // Search already has a bounded fallback. A hedge would spend that
      // fallback before we know if the first page is legitimately empty.
      if endpoint.ends_with("/SearchTimeline") {
         return self
            .timed_attempt(session, endpoint, variables, features, field_toggles)
            .await;
      }
      let Some(delay) = self.latency.hedge_delay(endpoint) else {
         return self
            .timed_attempt(session, endpoint, variables, features, field_toggles)
            .await;
      };
      // Boxed: two request futures side by side would double the size of
      // every handler future that awaits this one.
      let mut first =
         Box::pin(self.timed_attempt(session, endpoint, variables, features, field_toggles));
      tokio::select! {
         biased;
         done = &mut first => return done,
         () = sleep(delay) => {},
      }

      let Ok(permit) = Arc::clone(&self.hedges).try_acquire_owned() else {
         return first.await;
      };
      let first_port = self.pinned_port(first_id);
      let Some(spare) = self.sessions.try_acquire_where(endpoint, retry_kind, |id| {
         id != first_id && (first_port.is_none() || self.pinned_port(id) != first_port)
      }) else {
         return first.await;
      };
      self.latency.hedged(endpoint);
      let mut second =
         Box::pin(self.timed_attempt(spare, endpoint, variables, features, field_toggles));
      let outcome = tokio::select! {
         done = &mut first => {
            if done.1.as_ref().is_err_and(|err| !settles_the_call(err)) {
               let other = second.await;
               if other.1.is_ok() {
                  self.latency.rescued(endpoint);
                  other
               } else {
                  done
               }
            } else {
               done
            }
         },
         done = &mut second => {
            if done.1.is_ok() {
               self.latency.rescued(endpoint);
               done
            } else {
               first.await
            }
         },
      };
      drop(permit);
      outcome
   }

   /// One attempt that hands its lease back, timing the ones X answers.
   async fn timed_attempt<T>(
      &self,
      session: SessionLease,
      endpoint: &str,
      variables: &str,
      features: &str,
      field_toggles: Option<&str>,
   ) -> (SessionLease, Result<(T, bytes::Bytes)>)
   where
      T: DeserializeOwned,
   {
      let started = Instant::now();
      let result = self
         .graphql_request_inner(&session, endpoint, variables, features, field_toggles)
         .await;
      if result.is_ok() {
         self
            .latency
            .record(endpoint, started.elapsed(), self.pinned_port(session.id));
      }
      (session, result)
   }

   /// Take a session for one upstream call, billing the caller first.
   ///
   /// The only way to reach X, so a caller cannot spend quota by taking a
   /// route that forgot to charge. Billing precedes acquisition so a spent
   /// caller neither holds a lease nor waits on a permit, and a lease that
   /// never materialises is refunded rather than charged for nothing.
   pub(crate) async fn charged_session(
      &self,
      endpoint: &str,
      kind: Option<SessionKind>,
   ) -> Result<SessionLease> {
      self.charge(endpoint).await?;
      match self.sessions.acquire(endpoint, kind).await {
         Ok(session) => Ok(session),
         Err(err) => {
            self.refund(endpoint).await;
            Err(err)
         },
      }
   }

   /// [`charged_session`](Self::charged_session) that never queues: `None`
   /// when every account with quota is mid-call, so the caller can take a
   /// route with spare quota instead. Nothing stays billed without a lease.
   pub(crate) async fn charged_free_session(&self, endpoint: &str) -> Result<Option<SessionLease>> {
      self.charge(endpoint).await?;
      let free = self.sessions.acquire_free(endpoint, None).await;
      if !matches!(free, Ok(Some(_))) {
         self.refund(endpoint).await;
      }
      free
   }

   /// [`charged_session`](Self::charged_session) where the endpoint depends on
   /// which kind of session is chosen.
   pub(crate) async fn charged_session_by_kind(
      &self,
      cookie_api: &str,
      oauth_api: &str,
   ) -> Result<SessionLease> {
      self.charge(cookie_api).await?;
      match self.sessions.acquire_by_kind(cookie_api, oauth_api).await {
         Ok(session) => Ok(session),
         Err(err) => {
            self.refund(cookie_api).await;
            Err(err)
         },
      }
   }

   /// Give back what [`charge`](Self::charge) took for a call that never ran.
   async fn refund(&self, endpoint: &str) {
      if let Some(client) = budget::current_client() {
         let cost = Self::cost_of(endpoint);
         for budget in self.budgets_for(endpoint) {
            budget.refund(&client, cost).await;
         }
      }
   }

   /// Every budget a call to `endpoint` is charged against.
   fn budgets_for(&self, endpoint: &str) -> impl Iterator<Item = &ClientBudget> {
      let list_page = endpoint == endpoints::GRAPH_RETWEETERS;
      iter::once(&self.budget).chain(list_page.then_some(&self.list_budget))
   }

   /// Sessions that can still open a profile timeline (`UserTweets`).
   pub async fn user_timeline_accounts(&self) -> usize {
      self
         .sessions
         .available_for(endpoints::GRAPH_USER_TWEETS)
         .await
   }

   /// SearchTimeline is billed higher because first-page cache still misses on
   /// a new query and each page spends the tight 50/15m search budget.
   fn cost_of(endpoint: &str) -> f64 {
      if endpoint.contains("SearchTimeline") {
         SEARCH_COST
      } else {
         1.0
      }
   }

   /// Charge one upstream call to the caller of the current request.
   ///
   /// Called before a session is acquired, so a spent caller neither takes a
   /// lease nor waits on a permit, and once per attempt so a retry costs what
   /// it spends.
   async fn charge(&self, endpoint: &str) -> Result<()> {
      let Some(client) = budget::current_client() else {
         return Ok(());
      };
      let cost = Self::cost_of(endpoint);
      let mut charged = Vec::new();
      for budget in self.budgets_for(endpoint) {
         if !budget.try_spend(&client, cost).await {
            for paid in charged {
               ClientBudget::refund(paid, &client, cost).await;
            }
            tracing::debug!(?client, endpoint, "client budget exhausted");
            return Err(Error::ClientBudgetExhausted);
         }
         charged.push(budget);
      }

      Ok(())
   }

   /// Inner implementation of [`graphql_request`].
   ///
   /// A bodyless 404 is how X refuses a client transaction ID it will not
   /// accept, and it answers the same request without one, so the call is
   /// repeated on this session the other way before the caller burns a second
   /// account on it. The refusal is kept only when the call then works without
   /// an ID, and dropped when it works with one again, so an empty answer
   /// that has nothing to do with the ID cannot pin a path to one form.
   pub(super) async fn graphql_request_inner<T>(
      &self,
      session: &SessionLease,
      endpoint: &str,
      variables: &str,
      features: &str,
      field_toggles: Option<&str>,
   ) -> Result<(T, bytes::Bytes)>
   where
      T: DeserializeOwned,
   {
      // Search rides a cf_clearance cookie. Mint one for this session first
      // (no-op once it has a fresh one, or once the breaker has tripped).
      if endpoint.ends_with("/SearchTimeline") {
         self.ensure_search_clearance(session).await;
      }
      let sent_tid = self.tid_enabled && session.kind == SessionKind::Cookie;
      let wire = self.wire_operation(session, endpoint, features);
      let path = format!("/i/api/graphql/{}", wire.path.as_str());
      // POST endpoints carry the method in the TID signature; key the refusal
      // cache on the actual method so a stale GET entry never suppresses a TID
      // on a POST request.
      let use_post = session.kind == SessionKind::Cookie && endpoint.ends_with("/SearchTimeline");
      let method = if use_post { "POST" } else { "GET" };
      let refused = sent_tid && self.tid.is_refused(method, &path).await;
      let mut result = self
         .graphql_attempt(session, endpoint, &wire, variables, field_toggles, !refused)
         .await;

      if sent_tid && matches!(result, Err(Error::TransientUpstream)) {
         if refused {
            self.tid.clear_refused(method, &path).await;
         }
         result = self
            .graphql_attempt(session, endpoint, &wire, variables, field_toggles, refused)
            .await;
         // A search without the transaction id comes back as a parsed page with
         // no posts, which is a success here, for the same query that returns
         // posts when the id is sent. Remembering that as "X refuses the id"
         // made every later search go out without one, and those came back as
         // empty 404s from accounts that search fine in a browser.
         let search = endpoint.ends_with("SearchTimeline");
         if result.is_ok() && !refused && !search && self.tid.note_refused(method, &path).await {
            tracing::info!(
               endpoint,
               "X answers this call without a transaction id and refuses one"
            );
         }
         if let Err(ref err) = result
            && !matches!(err, Error::TransientUpstream)
         {
            tracing::warn!(
               session_id = session.id,
               endpoint,
               "no answer from X with or without a transaction id: {err}"
            );
         }
      }

      result
   }

   /// The `id/Name` path and features to send for `endpoint`. A cookie
   /// session is the web client, so it sends the web client's current id and
   /// features once they have been read; until then, and for OAuth sessions,
   /// the compiled ones go out.
   fn wire_operation<'a>(
      &self,
      session: &SessionLease,
      endpoint: &'a str,
      features: &'a str,
   ) -> WireOp<'a> {
      if session.kind == SessionKind::Cookie
         && let Some(live) = self.operations.resolve(endpoint)
      {
         return WireOp {
            path:             WireStr::Shared(live.path),
            features:         WireStr::Shared(live.features),
            encoded_features: WireStr::Shared(live.encoded_features),
         };
      }
      let encoded_features = match endpoints::encoded_features(features) {
         Cow::Borrowed(value) => WireStr::Borrowed(value),
         Cow::Owned(value) => WireStr::Owned(value),
      };
      WireOp {
         path: WireStr::Borrowed(endpoint),
         features: WireStr::Borrowed(features),
         encoded_features,
      }
   }

   /// One request. `endpoint` keys the session's rate-limit bookkeeping and
   /// stays the compiled name; `wire` is what goes in the URL.
   async fn graphql_attempt<T>(
      &self,
      session: &SessionLease,
      endpoint: &str,
      wire: &WireOp<'_>,
      variables: &str,
      field_toggles: Option<&str>,
      with_tid: bool,
   ) -> Result<(T, bytes::Bytes)>
   where
      T: DeserializeOwned,
   {
      let path = wire.path.as_str();
      let features = wire.features.as_str();
      let base_url = match session.kind {
         SessionKind::OAuth => endpoints::API_URL,
         SessionKind::Cookie => endpoints::GRAPHQL_URL,
      };

      // X's edge rejects GET on SearchTimeline for cookie sessions; POST works.
      let use_post = session.kind == SessionKind::Cookie && endpoint.ends_with("/SearchTimeline");
      let method = if use_post { "POST" } else { "GET" };

      let (url, body) = if use_post {
         (
            format!("{base_url}/{path}"),
            graphql_post_body(variables, features, field_toggles)?,
         )
      } else {
         (
            graphql_get_url(
               base_url,
               path,
               variables,
               wire.encoded_features.as_str(),
               field_toggles,
            ),
            Bytes::new(),
         )
      };
      let headers = self
         .graphql_headers(
            session,
            method,
            base_url,
            path,
            variables,
            features,
            field_toggles,
            with_tid,
         )
         .await?;

      let response = if use_post {
         match self
            .client
            .post_on(&url, &headers, body, Some(&self.egress_for(session)))
            .await
         {
            Ok(response) => response,
            Err(err) => {
               self.sessions.refund_if_metered(session, endpoint).await;
               return Err(err);
            },
         }
      } else {
         match self
            .client
            .get_on_with_deadline(
               &url,
               &headers,
               Some(&self.egress_for(session)),
               Some(self.latency.header_deadline(endpoint)),
            )
            .await
         {
            Ok(response) => response,
            Err(err) => {
               self.sessions.refund_if_metered(session, endpoint).await;
               return Err(err);
            },
         }
      };
      let (bytes, response_limits) = self
         .account_response_inner(session, endpoint, Some((path, variables)), response)
         .await?;

      if endpoint.ends_with("/SearchTimeline") && bytes.iter().all(u8::is_ascii_whitespace) {
         tracing::warn!(
            session_id = session.id,
            operation_id = path.rsplit_once('/').map_or("", |(id, _)| id),
            body_bytes = bytes.len(),
            "search succeeded at HTTP level but returned no JSON body"
         );
         return Err(Error::SearchRefused);
      }

      // One pass over a body that can run to several megabytes: the envelope
      // carries `errors` beside `data`, so the error check no longer costs a
      // second full tokenisation. If X's error document does not fit the
      // envelope, the cheap error-only parse still names the failure.
      let resp = match serde_json::from_slice::<GqlResponse<T>>(&bytes) {
         Ok(resp) => resp,
         Err(err) => {
            if let Err(api_err) = Self::check_api_errors(&bytes) {
               if matches!(api_err, Error::SessionRejected(_)) {
                  self.sessions.mark_rejected(session.id).await;
               } else if matches!(api_err, Error::RateLimited) {
                  if response_limits.limit_recorded {
                     self.sessions.record_event(
                        crate::stats::AccountEvent::new(
                           session.id,
                           crate::stats::Restriction::RateLimited,
                           endpoint,
                           "Upstream GraphQL response reported rate limit exceeded",
                        )
                        .upstream(200, Some(88)),
                     );
                  } else {
                     self
                        .sessions
                        .mark_endpoint_limited(session.id, endpoint)
                        .await;
                  }
                  if endpoint.ends_with("/SearchTimeline") {
                     self
                        .sessions
                        .record_search_outcome(
                           session.id,
                           endpoint,
                           crate::stats::SearchAccountOutcome::RateLimited,
                        )
                        .await;
                  }
               }
               return Err(api_err);
            }
            return Err(Error::Internal(format!("Response parse error: {err}")));
         },
      };

      // X failed `SearchTimeline` on its `timeline` field (typically "Internal
      // server error"). Suspended accounts answer every tweet search this way
      // while X also occasionally fails an individual query this way. Rest the
      // account immediately so concurrent requests pick another account, and
      // return a dedicated error so the caller does not waste a second TID-less
      // retry on the same account or degrade the failure into a false empty
      // page; corroboration and canary checks decide whether to demote it.
      if endpoint.ends_with("/SearchTimeline")
         && let Some(message) = search_timeline_failure(&bytes, &resp.errors)
      {
         tracing::info!(
            session_id = session.id,
            session_user = %session.username,
            proxy_port = self.pinned_port(session.id).unwrap_or(0),
            query = search_query_of(variables),
            error = message,
            "X failed this search on its side"
         );
         log_events::note(Event::SearchFailed);
         self.sessions.rest_search_failure(
            session.id,
            endpoint,
            super::auth::SEARCH_UNCORROBORATED_FAIL_REST_SECS,
         );
         return Err(Error::SearchTimelineFailed);
      }

      let serious = request_errors(&resp.errors, resp.data.is_some());
      if serious.len() < resp.errors.len() {
         tracing::debug!(
            endpoint,
            fields = resp.errors.len() - serious.len(),
            "X left some fields of the answer empty"
         );
      }
      let api_check = Self::map_api_errors(&serious);
      if let Err(Error::SessionRejected(ref msg)) = api_check {
         self.sessions.mark_rejected(session.id).await;
         return Err(Error::SessionRejected(msg.clone()));
      }
      if matches!(api_check, Err(Error::RateLimited)) {
         if response_limits.limit_recorded {
            self.sessions.record_event(
               crate::stats::AccountEvent::new(
                  session.id,
                  crate::stats::Restriction::RateLimited,
                  endpoint,
                  "Upstream GraphQL response reported rate limit exceeded",
               )
               .upstream(200, Some(88)),
            );
         } else {
            self
               .sessions
               .mark_endpoint_limited(session.id, endpoint)
               .await;
         }
         if endpoint.ends_with("/SearchTimeline") {
            self
               .sessions
               .record_search_outcome(
                  session.id,
                  endpoint,
                  crate::stats::SearchAccountOutcome::RateLimited,
               )
               .await;
         }
      }
      api_check?;

      let data = resp
         .data
         .ok_or_else(|| Error::Internal("Response parse error: missing data".into()))?;
      if endpoint.ends_with("/SearchTimeline") {
         let empty_search = search_answer_is_empty(&bytes);
         if empty_search && response_limits.header_exhausted {
            // The account reached its rate limit (`x-rate-limit-remaining: 0`)
            // and X answered with an empty timeline. Switch to another account
            // instead of returning a false empty page.
            if !response_limits.limit_recorded {
               self.sessions.rest_endpoint(session.id, endpoint).await;
            }
            self.sessions.record_event(
               crate::stats::AccountEvent::new(
                  session.id,
                  crate::stats::Restriction::RateLimited,
                  endpoint,
                  "Search returned empty at x-rate-limit-remaining=0; switching to another account",
               )
               .upstream(200, None),
            );
            self
               .sessions
               .record_search_outcome(
                  session.id,
                  endpoint,
                  crate::stats::SearchAccountOutcome::RateLimited,
               )
               .await;
            log_events::note(Event::Account429);
            tracing::debug!(
               session_id = session.id,
               session_user = %session.username,
               endpoint,
               "search returned empty at rate limit; switching to another account"
            );
            return Err(Error::RateLimited);
         }
         // A valid empty page is a result, not a transport error or suspension signal.
         if empty_search {
            tracing::info!(
               session_id = session.id,
               session_user = %session.username,
               proxy_port = self.pinned_port(session.id).unwrap_or(0),
               query = search_query_of(variables),
               "search came back with no posts"
            );
            log_events::note(Event::SearchEmpty);
            self
               .sessions
               .record_search_outcome(
                  session.id,
                  endpoint,
                  crate::stats::SearchAccountOutcome::Empty,
               )
               .await;
         } else {
            self
               .sessions
               .record_search_outcome(
                  session.id,
                  endpoint,
                  crate::stats::SearchAccountOutcome::NonEmpty,
               )
               .await;
         }
      }
      Ok((data, bytes))
   }

   /// Every authenticated call to X goes through here, or its 429s and refused
   /// credentials never reach the pool. The flag reports whether the headers
   /// recorded a real window, so a code 88 does not overwrite one.
   pub(crate) async fn account_response(
      &self,
      session: &SessionLease,
      endpoint: &str,
      response: super::http::Response,
   ) -> Result<(bytes::Bytes, bool)> {
      let (bytes, limits) = self
         .account_response_inner(session, endpoint, None, response)
         .await?;
      Ok((bytes, limits.limit_recorded))
   }

   async fn account_response_inner(
      &self,
      session: &SessionLease,
      endpoint: &str,
      graphql_request: Option<(&str, &str)>,
      response: super::http::Response,
   ) -> Result<(bytes::Bytes, ResponseLimits)> {
      let response_limits = if let Some(remaining) =
         response.headers().get("x-rate-limit-remaining")
         && let Ok(remaining_str) = remaining.to_str()
         && let Ok(remaining_val) = remaining_str.parse::<i32>()
      {
         let limit = response
            .headers()
            .get("x-rate-limit-limit")
            .and_then(|hv| hv.to_str().ok())
            .and_then(|sv| sv.parse().ok())
            .unwrap_or(0);
         let reset = response
            .headers()
            .get("x-rate-limit-reset")
            .and_then(|hv| hv.to_str().ok())
            .and_then(|sv| sv.parse().ok())
            .unwrap_or(0);

         self.sessions.disarm_meter(session);
         self
            .sessions
            .update_session_limit(session.id, endpoint, limit, remaining_val, reset)
            .await;
         ResponseLimits {
            limit_recorded:   remaining_val <= 0
               && reset > time::OffsetDateTime::now_utc().unix_timestamp(),
            header_exhausted: remaining_val <= 0,
         }
      } else {
         ResponseLimits::default()
      };

      if !response.status().is_success() {
         let status = response.status();
         // Headers have to be read before `text` consumes the response.
         // An empty or HTML body is only an IP block when X did not also
         // send an account window. Search 429s often arrive with
         // `x-rate-limit-*` and no body; treating those as an IP block
         // skipped the other accounts that still had budget and returned
         // 429 to the visitor.
         let account_window = account_rate_window(response.headers());
         // A csrf rejection carries the replacement token in its own
         // `set-cookie`, read here before the body is taken.
         let fresh_ct0 = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .find_map(ct0_cookie);
         let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
         let collected = response.bytes().await;
         let body_bytes = collected.as_ref().ok().map(bytes::Bytes::len);
         let body = collected
            .ok()
            .and_then(|bytes| String::from_utf8(bytes.into()).ok())
            .unwrap_or_default();
         let shown = log_upstream(&body);

         if cloudflare_ip_ban(status, &body) {
            let ray = cloudflare_ray(&body);
            let egress = self.egress_for(session);
            if let Some(proxy) = egress.proxy.as_ref()
               && self.client.ban_exit(proxy)
            {
               tracing::warn!(
                  port = proxy.port,
                  ray = ray.as_deref().unwrap_or(""),
                  "Cloudflare blocked this proxy exit; disabling it"
               );
            } else {
               tracing::warn!(
                  ray = ray.as_deref().unwrap_or(""),
                  "Cloudflare blocked this request"
               );
            }
            self.sessions.refund_if_metered(session, endpoint).await;
            return Err(Error::IpThrottled);
         }

         if status.as_u16() == 429 {
            if ip_throttle(&body, account_window) {
               // A bare 429 while the exit is still being served is X
               // refusing this account the call. Refunded, it stayed the
               // fullest account and was picked again about a thousand
               // times a minute, each pick a visitor's retry and often a 503.
               if self.client.exit_still_served(&self.egress_for(session)) {
                  self.sessions.disarm_meter(session);
                  let rest_secs = self.sessions.rest_endpoint(session.id, endpoint).await;
                  if endpoint.ends_with("/SearchTimeline") {
                     self
                        .sessions
                        .record_search_outcome(
                           session.id,
                           endpoint,
                           crate::stats::SearchAccountOutcome::RateLimited,
                        )
                        .await;
                  }
                  log_events::note(Event::Bare429);
                  tracing::debug!(
                     session_id = session.id,
                     session_user = %session.username,
                     endpoint,
                     rest_secs,
                     "X refused this account the call with a bare 429; resting it"
                  );
                  return Err(Error::RateLimited);
               }
               // The HTTP layer has parked the exit and tried another.
               log_events::note(Event::Edge429);
               tracing::debug!(
                  session_id = session.id,
                  session_user = %session.username,
                  endpoint,
                  "X edge rate-limited this proxy exit"
               );
               self.sessions.refund_if_metered(session, endpoint).await;
               return Err(Error::IpThrottled);
            }
            log_events::note(Event::Account429);
            tracing::debug!(
               session_id = session.id,
               session_user = %session.username,
               endpoint,
               "API request failed: {status} - {shown}"
            );
            self.sessions.disarm_meter(session);
            if response_limits.limit_recorded {
               self.sessions.record_event(
                  crate::stats::AccountEvent::new(
                     session.id,
                     crate::stats::Restriction::RateLimited,
                     endpoint,
                     "Upstream returned HTTP 429 with rate-limit window exhausted",
                  )
                  .upstream(status.as_u16(), twitter_error_code(&body)),
               );
            } else {
               self
                  .sessions
                  .mark_endpoint_limited(session.id, endpoint)
                  .await;
            }
            if endpoint.ends_with("/SearchTimeline") {
               self
                  .sessions
                  .record_search_outcome(
                     session.id,
                     endpoint,
                     crate::stats::SearchAccountOutcome::RateLimited,
                  )
                  .await;
            }
            return Err(Error::RateLimited);
         }

         // 401 is a dead token. 403 + code 326 is a locked cookie session.
         // Anything else 403 is a resource denial and must not sideline the
         // session or the next reader will think the *profile* is gone.
         //
         // A 403 with an HTML body and no X error code is the edge refusing
         // this request, not the account: Cloudflare answers a 269-byte block
         // page this way, and reading it as dead credentials benched healthy
         // accounts for good. Only a JSON 403 can carry X's own verdict.
         if status.as_u16() == 401
            || (status.as_u16() == 403 && twitter_error_code(&body) == Some(326))
         {
            tracing::error!(
               session_id = session.id,
               session_user = %session.username,
               "API request failed: {status} - {shown}"
            );
            self.sessions.record_event(
               crate::stats::AccountEvent::new(
                  session.id,
                  crate::stats::Restriction::CredentialsRejected,
                  endpoint,
                  "Upstream rejected account credentials",
               )
               .upstream(status.as_u16(), twitter_error_code(&body)),
            );
            self.sessions.disarm_meter(session);
            self.sessions.mark_rejected(session.id).await;
            return Err(Error::SessionRejected(format!("Status {status}: {shown}")));
         }

         if status.as_u16() == 403 && twitter_error_code(&body) == Some(353) {
            // The ct0 this account holds is no longer the one x.com accepts.
            // The rejection itself carries the replacement in its set-cookie,
            // so the account recovers on this call: no page load of x.com, and
            // no second request. Only an account X answers without a token is
            // taken out of rotation.
            let current_ct0 = self
               .sessions
               .ct0(session.id)
               .unwrap_or_else(|| session.ct0.clone());
            match fresh_ct0.filter(|ct0| ct0 != &current_ct0) {
               Some(ct0) => {
                  self.sessions.hold_ct0(session.id, ct0);
                  self.sessions.clear_ct0_dead(session.id).await;
               },
               None => self.sessions.mark_ct0_dead(session.id).await,
            }
            self.sessions.refund_if_metered(session, endpoint).await;
            return Err(Error::TransientUpstream);
         }

         // 403 + code 64 is X refusing this account itself ("not permitted to
         // access this feature"): the account is suspended. A suspended account
         // cannot search, so one reserved for search joins the view pool, where
         // its cookie may still read profiles. The view attempt is the only one
         // it gets: if that comes back suspended too, the account is retired and
         // nothing is asked of it again — no search, no view, and no daily
         // settings probe to re-learn a verdict already known.
         let code = twitter_error_code(&body);
         if status.as_u16() == 403 && code == Some(64) {
            // Self-service and auxiliary endpoints (settings, credentials,
            // searchSafety, and strato calls such as translation) are gated
            // separately from search and profile reads: X returns code 64
            // there for accounts that answer SearchTimeline and UserTweets
            // with 200 and posts. Route them through the Settings probe so
            // they only pace filter sync, never benching or demoting the
            // account.
            let is_settings = is_settings_probe_endpoint(endpoint);
            if !is_settings {
               self.sessions.record_event(
                  crate::stats::AccountEvent::new(
                     session.id,
                     crate::stats::Restriction::Suspended,
                     endpoint,
                     "Upstream reports account suspension or access refusal",
                  )
                  .upstream(status.as_u16(), code),
               );
            }
            tracing::debug!(
               session_id = session.id,
               session_user = %session.username,
               endpoint,
               "X refuses this account: {shown}"
            );
            self.sessions.disarm_meter(session);
            let (probe, evidence) = if is_settings {
               (
                  SuspensionProbe::Settings,
                  "Settings write refused (code 64)",
               )
            } else {
               // `None` restriction: the event above already recorded it.
               (SuspensionProbe::Data, "Account suspended")
            };
            self
               .sessions
               .note_suspended(session.id, probe, None, evidence)
               .await;
            return Err(Error::UserSuspended(format!("Status {status}: {shown}")));
         }

         // Empty or HTML 404s are X/WAF blips, not a missing tweet or user.
         // JSON 404s can still be a real gone resource.
         if status.as_u16() == 404 {
            if body.trim().is_empty() || serde_json::from_str::<serde_json::Value>(&body).is_err() {
               if endpoint.ends_with("/SearchTimeline") {
                  let (wire_path, variables) = graphql_request.unwrap_or_default();
                  let operation_id = wire_path.rsplit_once('/').map_or("", |(id, _)| id);
                  tracing::info!(
                     session_id = session.id,
                     session_user = %session.username,
                     proxy_port = self.pinned_port(session.id).unwrap_or(0),
                     query = search_query_of(variables),
                     operation_id,
                     browser_profile = %super::browser::identity_for(session.id),
                     http_status = status.as_u16(),
                     content_type = content_type.as_ref().and_then(|v| v.to_str().ok()).unwrap_or(""),
                     ?body_bytes,
                     "search came back as an empty 404"
                  );
                  log_events::note(Event::SearchEmpty);
                  self.sessions.record_search_404(session.id).await;
                  self.sessions.refund_if_metered(session, endpoint).await;
                  return Err(Error::SearchRefused);
               } else {
                  tracing::trace!(
                     session_id = session.id,
                     session_user = %session.username,
                     endpoint,
                     "bodyless 404, retrying without a transaction id"
                  );
               }
               self.sessions.refund_if_metered(session, endpoint).await;
               return Err(Error::TransientUpstream);
            }
            if is_settings_not_found(endpoint, &body) {
               self.sessions.disarm_meter(session);
               return Err(Error::AccountSettingsNotFound);
            }
            tracing::warn!(
               session_id = session.id,
               session_user = %session.username,
               endpoint,
               "API 404: {shown}"
            );
            self.sessions.disarm_meter(session);
            if let Err(err) = Self::check_api_errors(body.as_bytes()) {
               return Err(err);
            }
            return Err(Error::NotFound("Not found".into()));
         }

         if matches!(status.as_u16(), 502 | 503 | 504) {
            tracing::warn!(
               session_id = session.id,
               session_user = %session.username,
               "API request failed: {status} - {shown}"
            );
            // The gateway reset this stream. Drop the HTTP/2 session so the
            // next call from this account does not wait on it again.
            self.client.retire(Some(&self.egress_for(session)));
         } else if status.is_server_error() {
            tracing::error!(
               session_id = session.id,
               session_user = %session.username,
               "API request failed: {status} - {shown}"
            );
         } else {
            tracing::warn!(
               session_id = session.id,
               session_user = %session.username,
               "API request failed: {status} - {shown}"
            );
         }

         self.sessions.refund_if_metered(session, endpoint).await;
         return Err(Error::TwitterApi(format!("Status {status}: {shown}")));
      }

      self.sessions.disarm_meter(session);
      Ok((response.bytes().await?, response_limits))
   }

   async fn graphql_headers(
      &self,
      session: &SessionLease,
      method: &str,
      base_url: &str,
      endpoint: &str,
      variables: &str,
      features: &str,
      field_toggles: Option<&str>,
      with_tid: bool,
   ) -> Result<header::HeaderMap> {
      let mut headers = header::HeaderMap::new();

      match session.kind {
         SessionKind::OAuth => {
            let auth_url = format!("{base_url}/{endpoint}");
            let mut oauth_params = vec![("variables", variables), ("features", features)];
            if let Some(toggles) = field_toggles {
               oauth_params.push(("fieldToggles", toggles));
            }
            let auth = super::oauth1_sign(
               method,
               &auth_url,
               &oauth_params,
               &session.oauth_token,
               &session.oauth_secret,
            );
            headers.insert(
               header::AUTHORIZATION,
               auth
                  .parse()
                  .map_err(|_| Error::Internal("invalid OAuth header value".into()))?,
            );
         },
         SessionKind::Cookie => {
            let api_path = format!("/i/api/graphql/{endpoint}");
            let (bearer, tid) = if with_tid {
               self.bearer_and_tid_for(method, &api_path).await
            } else {
               (endpoints::BEARER_TOKEN_NO_TID, None)
            };

            headers.insert(
               header::AUTHORIZATION,
               header::HeaderValue::from_str(bearer)
                  .map_err(|_| Error::Internal("invalid bearer token value".into()))?,
            );
            headers.insert(
               "x-twitter-auth-type",
               header::HeaderValue::from_static("OAuth2Session"),
            );
            let ct0 = self
               .sessions
               .ct0(session.id)
               .unwrap_or_else(|| session.ct0.clone());
            headers.insert(
               "x-csrf-token",
               ct0.parse()
                  .map_err(|_| Error::Internal("invalid ct0 header value".into()))?,
            );
            headers.insert(
               header::COOKIE,
               format!("auth_token={}; ct0={ct0}", session.auth_token)
                  .parse()
                  .map_err(|_| Error::Internal("invalid cookie header value".into()))?,
            );
            // A same-origin GraphQL GET does not send Origin; POST requests do.
            if method == "POST" {
               headers.insert(
                  header::ORIGIN,
                  header::HeaderValue::from_static("https://x.com"),
               );
            }
            headers.insert(
               header::CONTENT_TYPE,
               header::HeaderValue::from_static("application/json"),
            );
            // Client hints, fetch metadata and the user agent come from the
            // session's browser profile; setting them here would contradict it.

            if let Some(tid) = tid
               && let Ok(val) = tid.parse()
            {
               headers.insert("x-client-transaction-id", val);
            }
            if let Some(referer) = endpoints::search_referer(endpoint, variables)
               .or_else(|| endpoints::explore_referer(endpoint, variables))
               .or_else(|| endpoints::trend_referer(endpoint, variables))
               .or_else(|| endpoints::game_referer(endpoint, variables))
               && let Ok(val) = header::HeaderValue::from_str(&referer)
            {
               headers.insert(header::REFERER, val);
            }
         },
      }

      // Common headers
      headers.insert(
         "x-twitter-active-user",
         header::HeaderValue::from_static("yes"),
      );
      headers.insert(
         "x-twitter-client-language",
         header::HeaderValue::from_static("en"),
      );

      Ok(headers)
   }

   async fn cookie_json_request<T>(&self, session_key: &str, api_path: &str, url: &str) -> Result<T>
   where
      T: DeserializeOwned,
   {
      let session = self
         .charged_session(session_key, Some(SessionKind::Cookie))
         .await?;

      let (bearer, tid) = self.bearer_and_tid(api_path).await;
      let mut headers = header::HeaderMap::new();
      headers.insert(
         header::AUTHORIZATION,
         header::HeaderValue::from_str(bearer)
            .map_err(|_| Error::Internal("invalid bearer token value".into()))?,
      );
      headers.insert(
         "x-twitter-auth-type",
         header::HeaderValue::from_static("OAuth2Session"),
      );
      let ct0 = self
         .sessions
         .ct0(session.id)
         .unwrap_or_else(|| session.ct0.clone());
      headers.insert(
         "x-csrf-token",
         ct0.parse()
            .map_err(|_| Error::Internal("invalid ct0 header value".into()))?,
      );
      headers.insert(
         header::COOKIE,
         format!("auth_token={}; ct0={ct0}", session.auth_token)
            .parse()
            .map_err(|_| Error::Internal("invalid cookie header value".into()))?,
      );
      headers.insert(
         header::ORIGIN,
         header::HeaderValue::from_static("https://x.com"),
      );
      headers.insert(
         "x-twitter-active-user",
         header::HeaderValue::from_static("yes"),
      );
      headers.insert(
         "x-twitter-client-language",
         header::HeaderValue::from_static("en"),
      );
      if let Some(tid) = tid
         && let Ok(value) = tid.parse()
      {
         headers.insert("x-client-transaction-id", value);
      }

      let response = self
         .client
         .get_on(url, &headers, Some(&self.egress_for(&session)))
         .await?;
      let (bytes, _) = self
         .account_response(&session, session_key, response)
         .await?;
      drop(session);
      serde_json::from_slice(&bytes)
         .map_err(|err| Error::Internal(format!("Response parse error: {err}")))
   }
}

#[path = "client_endpoints.rs"] mod endpoint_methods;
pub use endpoint_methods::{
   OpenProfile,
   recheck_empty,
};
#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn graphql_post_body_encodes_objects_and_omits_unused_toggles() {
      let variables = r##"{"rawQuery":"#かんぱに","count":20}"##;
      let features = r#"{"enabled":true}"#;
      let body = graphql_post_body(variables, features, None).unwrap();
      let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
      assert_eq!(
         payload,
         serde_json::json!({
            "variables": {"rawQuery": "#かんぱに", "count": 20},
            "features": {"enabled": true},
         })
      );

      let body = graphql_post_body(variables, features, Some(r#"{"extra":false}"#)).unwrap();
      let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
      assert_eq!(payload["fieldToggles"], serde_json::json!({"extra": false}));
      assert!(graphql_post_body(variables, "[]", None).is_err());
      assert!(graphql_post_body(variables, features, Some("null")).is_err());
      assert!(graphql_post_body("{}{}", features, None).is_err());
      assert!(graphql_post_body("{bad}", features, None).is_err());
   }

   #[test]
   fn graphql_get_url_matches_form_urlencoded_serializer() {
      let variables = r##"{"rawQuery":"cats & dogs #かんぱに","count":20}"##;
      let features = endpoints::GQL_FEATURES;
      let encoded_features = endpoints::encoded_features(features);
      for toggles in [
         None,
         Some(endpoints::TWEET_DETAIL_FIELD_TOGGLES),
         Some(r#"{"custom":true}"#),
      ] {
         let built = graphql_get_url(
            endpoints::GRAPHQL_URL,
            endpoints::GRAPH_TWEET_DETAIL,
            variables,
            &encoded_features,
            toggles,
         );
         let mut qs = form_urlencoded::Serializer::new(String::new());
         qs.append_pair("variables", variables);
         qs.append_pair("features", features);
         if let Some(raw_toggles) = toggles {
            qs.append_pair("fieldToggles", raw_toggles);
         }
         let expected = format!(
            "{}/{}?{}",
            endpoints::GRAPHQL_URL,
            endpoints::GRAPH_TWEET_DETAIL,
            qs.finish()
         );
         assert_eq!(built, expected);
      }
   }

   #[tokio::test]
   #[ignore = "requires a fresh isolated PostgreSQL cluster on a /tmp Unix socket"]
   async fn postgres_runtime_import_and_restart() {
      let url = std::env::var("TEAPOT_LIMITS_DATABASE_URL").unwrap();
      let socket = url
         .strip_prefix("host=")
         .and_then(|url| url.strip_suffix(" user=postgres dbname=postgres"))
         .unwrap();
      let suffix = socket.strip_prefix("/tmp/teapot-runtime-test-").unwrap();
      assert!(!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()));
      let store = AccountStore::connect().await.unwrap();
      assert!(matches!(SessionPool::from_store(store.clone(), 1).await,
         Err(Error::Internal(message)) if message.contains("migration is not complete")));
      let file = format!("{socket}/fixture.jsonl");
      tokio::fs::write(
         &file,
         r#"{"id":1,"username":"view","kind":"cookie","auth_token":"fixture","ct0":"old"}"#,
      )
      .await
      .unwrap();
      store.migrate_legacy(&file, "").await.unwrap();
      let pool = SessionPool::from_store(store.clone(), 1).await.unwrap();
      let jar = Jar::from_store(&store).await.unwrap();
      jar.spawn_flusher(store.clone());
      let lease = pool.acquire("TweetDetail", None).await.unwrap();
      let export = concat!(
         "# Netscape HTTP Cookie File\n",
         ".x.com\tTRUE\t/\tTRUE\t0\tauth_token\timported-token\n",
         ".x.com\tTRUE\t/\tTRUE\t0\tct0\timported-ct0\n",
         ".x.com\tTRUE\t/\tTRUE\t0\tguest_id\timported-guest\n",
      );
      assert_eq!(
         store
            .import_text(export, crate::types::AccountPool::Search)
            .await
            .unwrap()
            .imported,
         1
      );
      assert_eq!(
         store
            .import_text(export, crate::types::AccountPool::View)
            .await
            .unwrap()
            .duplicates,
         1
      );
      assert_eq!(
         ApiClient::reconcile_accounts(&store, &pool, &jar)
            .await
            .unwrap(),
         1
      );
      assert_eq!(
         ApiClient::reconcile_accounts(&store, &pool, &jar)
            .await
            .unwrap(),
         0
      );
      assert!(
         pool
            .acquire_free("TweetDetail", None)
            .await
            .unwrap()
            .is_none()
      );
      let search = pool.acquire("SearchTimeline", None).await.unwrap();
      let id = search.id;
      let mut headers = axum::http::HeaderMap::new();
      headers.insert(header::COOKIE, "ct0=imported-ct0".parse().unwrap());
      jar.merge_into(id, &mut headers).await;
      assert_eq!(
         headers[header::COOKIE],
         "ct0=imported-ct0; guest_id=imported-guest"
      );

      pool.hold_ct0(id, "fresh".to_owned());
      pool
         .demote_to_view(
            id,
            Some(crate::stats::Restriction::SearchBanned),
            "search refused",
         )
         .await;
      let mut answer = axum::http::HeaderMap::new();
      answer.append(
         header::SET_COOKIE,
         "guest_id=deleted; Max-Age=0".parse().unwrap(),
      );
      jar.absorb(id, &answer).await;
      drop((search, lease));
      timeout(Duration::from_secs(25), async {
         loop {
            let sessions = store.load().await.unwrap();
            let saved = sessions.iter().find(|session| session.id == id).unwrap();
            if saved.ct0 == "fresh"
               && saved.pool.is_empty()
               && !store.load_cookies().await.unwrap().contains_key(&id)
            {
               break;
            }
            sleep(Duration::from_millis(100)).await;
         }
      })
      .await
      .unwrap();
      let restart = SessionPool::from_store(store, 1).await.unwrap();
      assert_eq!(restart.ct0(id).as_deref(), Some("fresh"));
      assert_eq!(restart.available_for("SearchTimeline").await, 0);
      assert_eq!(restart.available_for("TweetDetail").await, 2);
   }

   #[test]
   fn a_cloudflare_block_page_is_one_log_line() {
      let page = "<html><title>Attention Required! | Cloudflare</title><h1>Sorry, you have been \
                  blocked</h1>Cloudflare Ray ID: <strong>a5267bf16cead69c</strong></html>";
      assert!(cloudflare_ip_ban(axum::http::StatusCode::FORBIDDEN, page));
      assert!(!cloudflare_ip_ban(
         axum::http::StatusCode::FORBIDDEN,
         r#"{"errors":[{"code":326}]}"#
      ));
      let note = log_upstream(page);
      assert!(note.contains("a5267bf16cead69c"));
      assert!(!note.contains("<html"));
      assert!(note.len() < 80);
   }

   #[test]
   fn formats_scheduled_audio_space_status() {
      let metadata = AudioSpaceMetadata {
         state: Some("NotStarted".to_owned()),
         scheduled_start: Some(1_784_203_200_000),
         ..AudioSpaceMetadata::default()
      };

      assert_eq!(
         audio_space_status(&metadata),
         "Scheduled · Jul 16, 2026 · 12:00 PM UTC"
      );
   }

   #[test]
   fn parses_recorded_audio_space_metadata() {
      let data: AudioSpaceData = serde_json::from_str(
         r#"{
            "audioSpace": {
               "metadata": {
                  "ended_at": "1784107519245",
                  "is_space_available_for_replay": true,
                  "state": "TimedOut",
                  "title": "Interlink enters a new phase",
                  "total_replay_watched": 4
               }
            }
         }"#,
      )
      .unwrap();
      let metadata = data.audio_space.unwrap().metadata.unwrap();

      assert_eq!(audio_space_status(&metadata), "Replay available · 4 plays");
   }

   #[test]
   fn a_search_x_failed_needs_both_the_missing_timeline_and_its_error() {
      let failed = br#"{"data":{"search_by_raw_query":{"search_timeline":{}}},"errors":[{"locations":[{"column":7,"line":4}],"message":"Internal server error","path":["search_by_raw_query","search_timeline","timeline"]}]}"#;
      let empty = br#"{"data":{"search_by_raw_query":{"search_timeline":{"timeline":{"instructions":[{"type":"TimelineAddEntries","entries":[{"content":{"__typename":"TimelineTimelineCursor","cursorType":"Bottom","value":"0"}}]},{"type":"TimelineTerminateTimeline","direction":"Bottom"}]}}}}}"#;
      let partial = br#"{"data":{"search_by_raw_query":{"search_timeline":{"timeline":{"instructions":[{"entries":[{"content":{"tweet_results":{"result":{"rest_id":"42"}}}}]}]}}}},"errors":[{"message":"Internal server error","path":["another_field","timeline"]}]}"#;
      let classify = |body: &[u8]| {
         let envelope: GqlResponse<SearchTimelineData> = serde_json::from_slice(body).unwrap();
         (
            search_timeline_failure(body, &envelope.errors).map(str::to_owned),
            search_answer_is_empty(body),
         )
      };
      assert_eq!(
         classify(failed),
         (Some("Internal server error".to_owned()), true)
      );
      assert_eq!(classify(empty), (None, true));
      assert_eq!(classify(partial), (None, false));
      assert_eq!(
         classify(br#"{"data":{"search_by_raw_query":{"search_timeline":{}}}}"#),
         (None, true)
      );

      let unrelated = [ApiError {
         code:    0,
         message: "Internal server error".into(),
         path:    vec![
            serde_json::json!("another_field"),
            serde_json::json!("timeline"),
         ],
      }];
      assert_eq!(search_timeline_failure(failed, &unrelated), None);
      let envelope: GqlResponse<SearchTimelineData> = serde_json::from_slice(failed).unwrap();
      assert_eq!(search_timeline_failure(empty, &envelope.errors), None);
      assert_eq!(search_timeline_failure(partial, &envelope.errors), None);
      assert_eq!(search_timeline_failure(b"", &envelope.errors), None);
   }

   #[test]
   fn a_search_page_without_posts_is_empty_and_one_with_posts_is_not() {
      assert!(search_answer_is_empty(
         br#"{"data":{"search_by_raw_query":{}}}"#
      ));
      assert!(search_answer_is_empty(br#"{"data":null}"#));
      assert!(!search_answer_is_empty(
         br#"{"data":{"search_by_raw_query":{"tweet_results":{}}}}"#
      ));
      assert!(!search_answer_is_empty(
         br#"{"data":{"search_by_raw_query":{"search_timeline":{"timeline":{"instructions":[{"entries":[{"content":{"itemContent":{"user_results":{"result":{"rest_id":"42"}}}}}]}}]}}}}}"#
      ));
      assert!(!search_answer_is_empty(
         br#"{"data":{"search_by_raw_query":{"search_timeline":{"timeline":{"instructions":[{"entries":[{"content":{"items":[{"item":{"itemContent":{"list":{"id_str":"7","name":"News"}}}}]}}]}}]}}}}}"#
      ));
      assert!(search_answer_is_empty(br#"{"data":{"list":null}}"#));
      assert!(search_answer_is_empty(br#"{"data":{"list":"x"}}"#));
      assert!(!search_answer_is_empty(
         br#"{"data":{"list":[{"tweet_results":{}}]}}"#
      ));
      assert!(search_answer_is_empty(
         br#"{"errors":[{"tweet_results":{}}],"data":null}"#
      ));
      assert!(!search_answer_is_empty(b"not json"));
      assert!(!search_answer_is_empty(b"[]"));
   }

   #[test]
   fn search_rate_limit_and_lock_errors_are_never_swallowed_as_empty_or_backend_failure() {
      let rate_limited_body = br#"{"data":{"search_by_raw_query":{"search_timeline":{}}},"errors":[{"code":88,"message":"Rate limit exceeded","path":["search_by_raw_query","search_timeline","timeline"]}]}"#;
      let env: GqlResponse<SearchTimelineData> = serde_json::from_slice(rate_limited_body).unwrap();
      assert_eq!(
         search_timeline_failure(rate_limited_body, &env.errors),
         None
      );
      let serious = request_errors(&env.errors, env.data.is_some());
      assert_eq!(serious.len(), 1);
      assert!(matches!(
         ApiClient::map_api_errors(&serious),
         Err(Error::RateLimited)
      ));

      let rate_msg_only: Vec<ApiError> = serde_json::from_str(
         r#"[{"code": 0, "message": "Rate limit exceeded for SearchTimeline", "path": ["search_by_raw_query", "search_timeline", "timeline"]}]"#,
      )
      .unwrap();
      assert_eq!(
         search_timeline_failure(rate_limited_body, &rate_msg_only),
         None
      );
      let serious_msg = request_errors(&rate_msg_only, true);
      assert_eq!(serious_msg.len(), 1);
      assert!(matches!(
         ApiClient::map_api_errors(&serious_msg),
         Err(Error::RateLimited)
      ));

      let locked: Vec<ApiError> = serde_json::from_str(
         r#"[{"code": 326, "message": "account is temporarily locked", "path": ["search_by_raw_query", "search_timeline", "timeline"]}]"#,
      )
      .unwrap();
      assert_eq!(search_timeline_failure(rate_limited_body, &locked), None);
      let serious_locked = request_errors(&locked, true);
      assert_eq!(serious_locked.len(), 1);
      assert!(matches!(
         ApiClient::map_api_errors(&serious_locked),
         Err(Error::SessionRejected(_))
      ));

      let denylisted: Vec<ApiError> = serde_json::from_str(
         r#"[{"code": 0, "message": "QueryDenylistedFailure", "path": ["search_by_raw_query", "search_timeline", "timeline"]}]"#,
      )
      .unwrap();
      assert_eq!(
         search_timeline_failure(rate_limited_body, &denylisted),
         None
      );
      let serious_deny = request_errors(&denylisted, true);
      assert_eq!(serious_deny.len(), 1);
      assert!(matches!(
         ApiClient::map_api_errors(&serious_deny),
         Err(Error::SearchDenylisted)
      ));
   }

   #[test]
   fn parses_live_broadcast_metadata() {
      let data: BroadcastsData = serde_json::from_str(
         r#"{
            "broadcasts": {
               "1XxyggAaLzvGM": {
                  "available_for_replay": true,
                  "image_url": "https://video.pscp.tv/latest.jpg",
                  "state": "RUNNING",
                  "status": "Stripe x PayPal",
                  "total_watched": "26",
                  "total_watching": "26",
                  "twitter_username": "tbpn",
                  "user_display_name": "TBPN"
               }
            }
         }"#,
      )
      .unwrap();
      let metadata = &data.broadcasts["1XxyggAaLzvGM"];

      assert_eq!(broadcast_status(metadata), "Live now · 26 watching");
      assert_eq!(
         hosted_by(&metadata.user_display_name, &metadata.twitter_username),
         Some("Hosted by TBPN (@tbpn)".to_owned())
      );
   }

   #[test]
   fn extracts_audio_space_id_from_canonical_url() {
      assert_eq!(
         space_id_from_url("https://x.com/i/spaces/1AxRnnrNvyDxl/peek?foo=bar"),
         Some("1AxRnnrNvyDxl")
      );
   }

   #[test]
   fn an_error_in_one_field_leaves_the_page() {
      let errors: Vec<ApiError> = serde_json::from_str(
         r#"[
            {"code": 214, "message": "BadRequest: BadRequest", "path": ["user", "result", "timeline", "timeline", "instructions", 1, "entries", 10, "content", "itemContent", "tweet_results", "result", "note_tweet", "note_tweet_results", "result", "text"]},
            {"code": 214, "message": "BadRequest: com.twitter.strato.serialization.DecodeException"}
         ]"#,
      )
      .unwrap();
      assert!(request_errors(&errors, true).is_empty());
      assert_eq!(request_errors(&errors, false).len(), 2);

      let whole: Vec<ApiError> =
         serde_json::from_str(r#"[{"code": 88, "message": "Rate limit exceeded"}]"#).unwrap();
      assert_eq!(request_errors(&whole, true).len(), 1);

      let denylisted: Vec<ApiError> = serde_json::from_str(
         r#"[{"code": 0, "message": "QueryDenylistedFailure", "path": ["search_by_raw_query"]}]"#,
      )
      .unwrap();
      assert_eq!(request_errors(&denylisted, true).len(), 1);
   }

   #[test]
   fn empty_429_with_an_account_window_is_not_an_ip_block() {
      use axum::http::{
         HeaderMap,
         HeaderValue,
      };

      let mut window = HeaderMap::new();
      window.insert("x-rate-limit-remaining", HeaderValue::from_static("0"));
      window.insert("x-rate-limit-limit", HeaderValue::from_static("50"));
      assert!(account_rate_window(&window));
      assert!(!ip_throttle("", true));
      assert!(!ip_throttle("<html>blocked</html>", true));

      let mut html = HeaderMap::new();
      html.insert(
         header::CONTENT_TYPE,
         HeaderValue::from_static("text/html; charset=utf-8"),
      );
      assert!(!account_rate_window(&html));
      assert!(ip_throttle("<!DOCTYPE html><title>1015</title>", false));
      assert!(ip_throttle("   ", false));
      assert!(!ip_throttle("Rate limit exceeded", false));
   }

   #[test]
   fn extracts_broadcast_id_from_canonical_url() {
      assert_eq!(
         broadcast_id_from_url("https://x.com/i/broadcasts/1XxyggAaLzvGM?foo=bar"),
         Some("1XxyggAaLzvGM")
      );
   }

   #[tokio::test]
   async fn inflight_coalescing_shares_payload_and_leaves_nothing_stored() {
      let coalescer = Arc::new(InflightCoalescer::new());
      let key = ApiClient::inflight_key("x/TweetDetail", r#"{"focalTweetId":"1"}"#, None);

      let guard = match coalescer.join_or_lead(key.clone()) {
         InflightTicket::Leader(guard) => guard,
         InflightTicket::Follower(_) => unreachable!("first caller should lead"),
      };
      let mut follower = match coalescer.join_or_lead(key) {
         InflightTicket::Follower(rx) => rx,
         InflightTicket::Leader(_) => unreachable!("second caller should follow"),
      };
      assert_eq!(coalescer.len(), 1);

      guard.complete(bytes::Bytes::from_static(b"{\"data\":{\"ok\":true}}"));
      assert!(
         coalescer.is_empty(),
         "completed request must not stay stored in memory"
      );

      follower.changed().await.unwrap();
      let payload = follower.borrow_and_update().clone().unwrap();
      let parsed: serde_json::Value = ApiClient::parse_coalesced(&payload).unwrap();
      assert_eq!(parsed["ok"], true);
   }

   #[test]
   fn tweet_search_detection_excludes_people_and_lists() {
      assert!(is_tweet_search_variables(&endpoints::search_vars(
         "news", None, "Latest"
      )));
      assert!(is_tweet_search_variables(&endpoints::search_vars(
         "news", None, "Top"
      )));
      assert!(is_tweet_search_variables(&endpoints::search_vars(
         "news", None, "Media"
      )));
      assert!(!is_tweet_search_variables(&endpoints::search_vars(
         "news", None, "People"
      )));
      assert!(!is_tweet_search_variables(&endpoints::search_list_vars(
         "news", None
      )));
   }

   #[test]
   fn search_query_hash_normalizes_case_whitespace_tab_and_cursor() {
      let latest = endpoints::search_vars("News", None, "Latest");
      let top_paged = endpoints::search_vars("  news  ", Some("CURSOR"), "Top");
      let other = endpoints::search_vars("cats", None, "Latest");
      let h1 = search_query_hash(&latest);
      let h2 = search_query_hash(&top_paged);
      let h3 = search_query_hash(&other);
      assert_ne!(h1, 0);
      assert_eq!(h1, h2);
      assert_ne!(h1, h3);
   }
}
