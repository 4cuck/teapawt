use std::time::{
   Duration,
   Instant,
};

use axum::{
   Json,
   Router,
   body::to_bytes,
   extract::{
      Path,
      Request,
      State,
   },
   http::{
      HeaderValue,
      Method,
      StatusCode,
      Uri,
      header,
   },
   middleware::Next,
   response::{
      Html,
      IntoResponse as _,
      Response,
   },
   routing::{
      get,
      post,
   },
};
use axum_extra::extract::CookieJar;
use serde::{
   Deserialize,
   Serialize,
};

use super::{
   anti_ddos::status as gate_status,
   import::{
      administrator,
      cross_origin,
      no_store,
   },
};
use crate::{
   AppState,
   accounts::AccountIdentity,
   error::Error,
   stats::{
      AccountEvent,
      Outcome,
      Restriction,
   },
   types::Prefs,
   utils::host::current_origin,
   views::admin_stats::{
      self as view,
      Bucket,
      Dashboard,
      RestrictionCount,
   },
};

const EVENT_PAGE: i64 = 50;
const SCAN_BATCHES: usize = 20;
const SCAN_BATCH_SIZE: i64 = 250;
/// `StatsStore::recent_account_events_filtered` clamps `limit` to 200 rows per
/// query, so any batch smaller than this has reached the end of the table.
const STORE_BATCH_CAP: usize = 200;
const MAX_JSON_BYTES: usize = 4096;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct InvestigationQuery {
   pub kind:   Option<String>,
   pub status: Option<String>,
   pub pool:   Option<String>,
   pub q:      Option<String>,
   pub before: Option<i64>,
}

impl InvestigationQuery {
   pub(crate) fn from_uri(uri: &Uri) -> Self {
      let mut parsed = Self::default();
      let Some(raw) = uri.query() else {
         return parsed;
      };
      for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
         let trimmed = value.trim();
         if trimmed.is_empty() {
            continue;
         }
         match key.as_ref() {
            "kind" => {
               if trimmed.eq_ignore_ascii_case("all") {
                  parsed.kind = None;
               } else {
                  parsed.kind = Some(trimmed.to_owned());
               }
            },
            "status" => {
               if trimmed.eq_ignore_ascii_case("all") {
                  parsed.status = None;
               } else {
                  parsed.status = Some(trimmed.to_owned());
               }
            },
            "pool" => {
               if trimmed.eq_ignore_ascii_case("all") {
                  parsed.pool = None;
               } else {
                  parsed.pool = Some(trimmed.to_owned());
               }
            },
            "q" => parsed.q = Some(trimmed.to_owned()),
            "before" => {
               if let Ok(cursor) = trimmed.parse::<i64>()
                  && cursor > 0
               {
                  parsed.before = Some(cursor);
               }
            },
            _ => {},
         }
      }
      parsed
   }

   pub(crate) fn has_filters(&self) -> bool {
      self.q.as_deref().is_some_and(|val| !val.trim().is_empty())
         || self.kind.as_deref().is_some_and(|val| {
            let trimmed = val.trim();
            !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("all")
         })
         || self.status.as_deref().is_some_and(|val| !val.trim().is_empty())
         || self.pool.as_deref().is_some_and(|val| !val.trim().is_empty())
         || self.before.is_some()
   }

   pub(crate) fn to_uri(
      &self,
      base_path: &str,
      before_override: Option<Option<i64>>,
   ) -> String {
      let mut serializer = form_urlencoded::Serializer::new(String::new());
      let mut any = false;
      if let Some(search) = self.q.as_deref().map(str::trim).filter(|val| !val.is_empty()) {
         serializer.append_pair("q", search);
         any = true;
      }
      if let Some(kind) = self
         .kind
         .as_deref()
         .map(str::trim)
         .filter(|val| !val.is_empty() && !val.eq_ignore_ascii_case("all"))
      {
         serializer.append_pair("kind", kind);
         any = true;
      }
      if let Some(status) = self
         .status
         .as_deref()
         .map(str::trim)
         .filter(|val| !val.is_empty() && !val.eq_ignore_ascii_case("all"))
      {
         serializer.append_pair("status", status);
         any = true;
      }
      if let Some(pool) = self
         .pool
         .as_deref()
         .map(str::trim)
         .filter(|val| !val.is_empty() && !val.eq_ignore_ascii_case("all"))
      {
         serializer.append_pair("pool", pool);
         any = true;
      }
      if let Some(before) = before_override.unwrap_or(self.before).filter(|&id| id > 0) {
         serializer.append_pair("before", &before.to_string());
         any = true;
      }
      if any {
         format!("{base_path}?{}", serializer.finish())
      } else {
         base_path.to_owned()
      }
   }
}

#[derive(Serialize)]
pub(crate) struct Observation {
   pub event:               AccountEvent,
   pub identity:            Option<AccountIdentity>,
   pub active:              Option<bool>,
   pub demoted_from_search: bool,
}

#[derive(Serialize)]
pub(crate) struct InvestigationData {
   pub(crate) events:      Vec<Observation>,
   pub(crate) next_before: Option<i64>,
   pub(crate) now:         i64,
}

#[derive(Serialize)]
pub(crate) struct AccountData {
   pub(crate) account:  Option<crate::api::auth::AccountState>,
   pub(crate) identity: Option<AccountIdentity>,
   pub(crate) events:   Vec<AccountEvent>,
}

/// What a manual health probe found, and whether it changed the stored verdict.
#[derive(Serialize)]
pub(crate) struct ProbeResult {
   pub(crate) verdict:    &'static str,
   pub(crate) detail:     Option<String>,
   /// The stored flags before the probe, so the operator can see what changed.
   pub(crate) before:     ProbeFlags,
   pub(crate) after:      ProbeFlags,
   /// True when a stored suspension was contradicted and cleared.
   pub(crate) reinstated: bool,
}

#[derive(Serialize)]
pub(crate) struct ProbeFlags {
   pub(crate) suspended: bool,
   pub(crate) retired:   bool,
   pub(crate) pool:      &'static str,
}

#[derive(Deserialize)]
struct PoolUpdateRequest {
   pool: String,
}

#[derive(Serialize)]
struct PoolUpdateResponse {
   ok:      bool,
   changed: bool,
   pool:    &'static str,
}

pub(super) fn router() -> Router<AppState> {
   Router::new()
      .route("/admin/stats", get(page))
      .route("/admin/stats/data", get(data))
      .route("/admin/stats/restrictions", get(restrictions))
      .route("/admin/stats/restrictions/data", get(restrictions_data))
      .route("/admin/stats/sessions/{id}", get(session_page))
      .route("/admin/stats/sessions/{id}/data", get(session_data))
      .route("/admin/stats/sessions/{id}/cookies", get(cookies))
      .route("/admin/stats/sessions/{id}/probe", post(probe))
      .route("/admin/stats/sessions/{id}/pool", post(set_pool))
}

pub(crate) fn private_path(path: &str) -> bool {
   path == "/admin"
      || path.starts_with("/admin/")
      || path == "/import/admin"
      || path.starts_with("/import/admin/")
}

pub(crate) async fn admin_no_store(request: Request, next: Next) -> Response {
   let path = request.uri().path();
   let private = private_path(path);
   let stats = path == "/admin/stats" || path.starts_with("/admin/stats/");
   let mut response = next.run(request).await;
   if private {
      let headers = response.headers_mut();
      no_store(headers);
      headers.insert(
         "x-robots-tag",
         HeaderValue::from_static("noindex, nofollow"),
      );
      if stats {
         headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
               "default-src 'none'; style-src 'self'; script-src 'self'; connect-src 'self'; \
                img-src 'self'; font-src 'self'; form-action 'self'; base-uri 'none'; \
                frame-ancestors 'none'",
            ),
         );
      }
   }
   response
}

fn range_from_uri(uri: &Uri) -> i32 {
   let Some(raw) = uri.query() else {
      return 60;
   };
   let mut selected = None;
   for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
      if key == "minutes"
         && let Ok(minutes) = value.trim().parse::<i32>()
         && matches!(minutes, 15 | 60 | 360 | 1440 | 10080)
      {
         selected = Some(minutes);
      }
   }
   selected.unwrap_or(60)
}

fn range(request: &Request) -> i32 {
   range_from_uri(request.uri())
}

fn forbidden_html(theme: &str) -> Response {
   (
      StatusCode::FORBIDDEN,
      Html(view::forbidden(theme).into_string()),
   )
      .into_response()
}

async fn snapshot(state: &AppState, minutes: i32) -> Option<Dashboard> {
   let store = state.stats.as_ref()?;
   tokio::time::timeout(Duration::from_secs(5), async {
      let now = now_unix();
      let (data, pool, capacity, census, gate) = tokio::join!(
         store.snapshot(minutes),
         state.accounts.stats(),
         state.api.search_budget_estimate(),
         state.api.sessions.census(),
         async { gate_status(state, now) },
      );
      let data = data.ok()?;
      let pool = pool
         .ok()
         .map(|stats| (stats.search, stats.view))
         .or_else(|| Some((census.search as i64, census.view as i64)));
      Some(Dashboard {
         minutes,
         pool,
         capacity: Some(capacity),
         census,
         gate,
         dropped_events: data.dropped_events,
         buckets: data
            .buckets
            .into_iter()
            .map(|row| {
               Bucket {
                  minute:       row.minute_unix,
                  incoming:     row.counts.received,
                  success:      row.counts.success,
                  empty:        row.counts.empty,
                  rejected:     row.counts.rejected,
                  rate_limited: row.counts.rate_limited,
                  failed:       row.counts.failed,
                  other:        row.counts.other,
                  challenged:   row.counts.challenged,
                  latency_ms:   row.latency.avg_ms.unwrap_or_default().round() as i64,
                  finished:     row.latency.samples,
               }
            })
            .collect(),
         restrictions: data
            .restrictions
            .into_iter()
            .map(|row| {
               RestrictionCount {
                  minute:      row.minute_unix,
                  search_only: row.search_only,
                  suspended:   row.suspended,
               }
            })
            .collect(),
      })
   })
   .await
   .ok()
   .flatten()
}

async fn page(State(state): State<AppState>, jar: CookieJar, request: Request) -> Response {
   let theme = Prefs::from_cookies(&jar, &state.config).theme;
   if !administrator(&state, &jar).await {
      return forbidden_html(&theme);
   }
   let minutes = range(&request);
   let stats = snapshot(&state, minutes).await;
   let status = if stats.is_some() {
      StatusCode::OK
   } else {
      StatusCode::SERVICE_UNAVAILABLE
   };
   (
      status,
      Html(view::page(&theme, stats.as_ref(), minutes).into_string()),
   )
      .into_response()
}

async fn data(State(state): State<AppState>, jar: CookieJar, request: Request) -> Response {
   if !administrator(&state, &jar).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   match snapshot(&state, range(&request)).await {
      Some(snapshot) => Json(snapshot).into_response(),
      None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
   }
}

async fn restrictions_data(
   State(state): State<AppState>,
   jar: CookieJar,
   request: Request,
) -> Response {
   if !administrator(&state, &jar).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   let query = InvestigationQuery::from_uri(request.uri());
   match investigation(&state, &query).await {
      Some(result) => Json(result).into_response(),
      None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
   }
}

fn restriction_kind(value: Option<&str>) -> Option<Restriction> {
   let raw = value?.trim();
   if raw.is_empty() || raw.eq_ignore_ascii_case("all") {
      return None;
   }
   Restriction::from_i16(raw.parse().ok()?)
}

fn active_restriction(
   event: &AccountEvent,
   account: &crate::api::auth::AccountState,
   now: i64,
) -> bool {
   match event.kind {
      Restriction::Suspended => account.suspended || account.retired,
      Restriction::CredentialsRejected => account.credentials_rejected,
      Restriction::CsrfRejected => account.ct0_dead,
      Restriction::SearchOnly | Restriction::SearchBanned => account.search_banned,
      Restriction::ProfileBanned => account.profile_banned,
      Restriction::SettingsRejected => account.filters_failed,
      Restriction::RateLimited => {
         account.globally_limited
            || account
               .rate_limits
               .get(&event.endpoint)
               .is_some_and(|limit| limit.remaining <= 0 && limit.reset > now)
      },
      Restriction::OperationRefused | Restriction::SearchRested => {
         account.refused_operations.contains_key(&event.endpoint)
      },
      // A reinstatement is history by definition: the verdict it cleared is
      // gone, so it can never be "still active".
      Restriction::Reinstated => false,
   }
}

async fn investigation(state: &AppState, query: &InvestigationQuery) -> Option<InvestigationData> {
   let store = state.stats.as_ref()?;
   let kind = restriction_kind(query.kind.as_deref());
   let pool = query
      .pool
      .as_deref()
      .filter(|pool| matches!(*pool, "search" | "view"));
   let status = query
      .status
      .as_deref()
      .filter(|status| matches!(*status, "active" | "historical"));
   let ids = match query.q.as_deref().map(str::trim).filter(|val| !val.is_empty()) {
      Some(search) => Some(state.accounts.find_identities(search, 500).await.ok()?),
      None => None,
   };
   let now = now_unix();
   if ids.as_ref().is_some_and(Vec::is_empty) {
      return Some(InvestigationData {
         events: Vec::new(),
         next_before: None,
         now,
      });
   }

   let mut observations: Vec<Observation> = Vec::with_capacity(EVENT_PAGE as usize);
   let mut cursor = query.before;
   let mut next_before = None;
   let mut exhausted = false;

   // Current-state filters need joining to runtime state. Scan up to 20 chunks
   // and look for `EVENT_PAGE + 1` matching rows so `next_before` is only set
   // when older history genuinely remains.
   for _ in 0..SCAN_BATCHES {
      let events = store
         .recent_account_events_filtered(kind, ids.as_deref(), cursor, SCAN_BATCH_SIZE)
         .await
         .ok()?;
      if events.is_empty() {
         exhausted = true;
         break;
      }
      let batch_len = events.len();
      cursor = events.last().map(|event| event.id);

      let mut unique_ids = events
         .iter()
         .map(|event| event.session_id)
         .collect::<Vec<_>>();
      unique_ids.sort_unstable();
      unique_ids.dedup();

      let (identities_res, states) = tokio::join!(
         state.accounts.identities(&unique_ids),
         state.api.sessions.account_states(&unique_ids),
      );
      let identities = identities_res.unwrap_or_default();

      for event in events {
         let current = states.get(&event.session_id);
         let live_pool = current
            .map(|account| {
               if account.pool.is_empty() {
                  "view"
               } else {
                  account.pool
               }
            })
            .or_else(|| {
               identities
                  .get(&event.session_id)
                  .map(|identity| identity.pool.as_str())
            })
            .unwrap_or("");
         if pool.is_some_and(|selected| live_pool != selected) {
            continue;
         }
         let active = current.map(|account| active_restriction(&event, account, now));
         if status.is_some_and(|selected| active != Some(selected == "active")) {
            continue;
         }

         if observations.len() == EVENT_PAGE as usize {
            next_before = observations.last().map(|obs| obs.event.id);
            break;
         }

         let mut identity = identities.get(&event.session_id).cloned().or_else(|| {
            current.map(|account| {
               AccountIdentity {
                  id:       account.id,
                  username: account.username.clone(),
                  kind:     account.kind,
                  pool:     live_pool.to_owned(),
               }
            })
         });
         if let Some(ref mut ident) = identity
            && !live_pool.is_empty()
         {
            ident.pool = live_pool.to_owned();
         }
         let demoted_from_search =
            current.is_some_and(|account| account.demoted_from_search);

         observations.push(Observation {
            event,
            identity,
            active,
            demoted_from_search,
         });
      }

      if next_before.is_some() {
         break;
      }
      if batch_len < STORE_BATCH_CAP {
         exhausted = true;
         break;
      }
   }

   if next_before.is_none() && !exhausted {
      next_before = cursor;
   }

   Some(InvestigationData {
      events: observations,
      next_before,
      now,
   })
}

async fn restrictions(
   State(state): State<AppState>,
   jar: CookieJar,
   request: Request,
) -> Response {
   let theme = Prefs::from_cookies(&jar, &state.config).theme;
   if !administrator(&state, &jar).await {
      return forbidden_html(&theme);
   }
   let query = InvestigationQuery::from_uri(request.uri());
   let data = investigation(&state, &query).await;
   let canonical_uri = query.to_uri("/admin/stats/restrictions", None);
   Html(view::restrictions(&theme, data.as_ref(), &query, &canonical_uri).into_string())
      .into_response()
}

async fn account_data(state: &AppState, id: i64) -> Option<AccountData> {
   let identity = state.accounts.identities(&[id]).await.ok()?.remove(&id);
   let account = state.api.sessions.account_state(id).await;
   if identity.is_none() && account.is_none() {
      return None;
   }
   let events = state
      .stats
      .as_ref()?
      .account_timeline(id)
      .await
      .unwrap_or_default();
   Some(AccountData {
      account,
      identity,
      events,
   })
}

async fn session_data(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<i64>,
) -> Response {
   if !administrator(&state, &jar).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   match account_data(&state, id).await {
      Some(account) => Json(account).into_response(),
      None => StatusCode::NOT_FOUND.into_response(),
   }
}

async fn session_page(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<i64>,
) -> Response {
   let theme = Prefs::from_cookies(&jar, &state.config).theme;
   if !administrator(&state, &jar).await {
      return forbidden_html(&theme);
   }
   let data = account_data(&state, id).await;
   let status = if data.is_some() {
      StatusCode::OK
   } else {
      StatusCode::NOT_FOUND
   };
   (
      status,
      Html(view::account(&theme, id, data.as_ref()).into_string()),
   )
      .into_response()
}

/// Ask X about one account and, when the answer contradicts a stored verdict,
/// clear it.
///
/// This is the only path that spends a call on a benched account, and it only
/// runs because an operator asked. It is deliberately not scheduled: the pool
/// learns a suspension once, and re-asking across thousands of accounts is what
/// starved search before.
async fn probe(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<i64>,
   request: Request,
) -> Response {
   let origin = current_origin(state.config.url_prefix());
   if cross_origin(request.headers(), &origin) {
      return (
         StatusCode::FORBIDDEN,
         "Cross-origin requests are not permitted.",
      )
         .into_response();
   }
   if !administrator(&state, &jar).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   let Some(before_state) = state.api.sessions.account_state(id).await else {
      return StatusCode::NOT_FOUND.into_response();
   };
   let before = ProbeFlags {
      suspended: before_state.suspended,
      retired:   before_state.retired,
      pool:      before_state.pool,
   };

   let (verdict, detail) = match state.api.probe_account(id).await {
      Ok(verdict) => (verdict.label(), None),
      Err(err) => ("error", Some(err.to_string())),
   };

   // Only a live answer clears a verdict. A refused settings write or a dead
   // credential clears nothing, and an error says nothing at all. `reinstate`
   // decides whether there is anything to undo: an account that was moved out
   // of search goes back to it even when no suspension flag is left standing.
   let reinstated = verdict == "alive" && state.api.sessions.reinstate(id).await;

   let after = state
      .api
      .sessions
      .account_state(id)
      .await
      .map_or(before_pool(), |account| {
         ProbeFlags {
            suspended: account.suspended,
            retired:   account.retired,
            pool:      account.pool,
         }
      });

   Json(ProbeResult {
      verdict,
      detail,
      before,
      after,
      reinstated,
   })
   .into_response()
}

const fn before_pool() -> ProbeFlags {
   ProbeFlags {
      suspended: false,
      retired:   false,
      pool:      "view",
   }
}

async fn set_pool(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<i64>,
   request: Request,
) -> Response {
   let origin = current_origin(state.config.url_prefix());
   if cross_origin(request.headers(), &origin) {
      return (
         StatusCode::FORBIDDEN,
         "Cross-origin requests are not permitted.",
      )
         .into_response();
   }
   if !administrator(&state, &jar).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   let Ok(body) = to_bytes(request.into_body(), MAX_JSON_BYTES).await else {
      return (StatusCode::BAD_REQUEST, "Could not read request body.").into_response();
   };
   let Ok(payload) = serde_json::from_slice::<PoolUpdateRequest>(&body) else {
      return (
         StatusCode::BAD_REQUEST,
         "Expected JSON {\"pool\": \"search\" | \"view\"}.",
      )
         .into_response();
   };
   let to_search = match payload.pool.trim() {
      "search" => true,
      "view" => false,
      _ => {
         return (
            StatusCode::BAD_REQUEST,
            "Pool must be \"search\" or \"view\".",
         )
            .into_response();
      },
   };
   match state.api.sessions.set_account_pool(id, to_search).await {
      Ok(changed) => {
         let fallback_pool = if to_search { "search" } else { "view" };
         let after_pool = state
            .api
            .sessions
            .account_state(id)
            .await
            .map_or(fallback_pool, |account| account.pool);
         Json(PoolUpdateResponse {
            ok: true,
            changed,
            pool: after_pool,
         })
         .into_response()
      },
      Err(Error::NotFound(message)) => (StatusCode::NOT_FOUND, message).into_response(),
      Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
   }
}

async fn cookies(State(state): State<AppState>, jar: CookieJar, Path(id): Path<i64>) -> Response {
   if !administrator(&state, &jar).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   if state
      .accounts
      .identities(&[id])
      .await
      .ok()
      .and_then(|mut ids| ids.remove(&id))
      .is_none()
   {
      return StatusCode::NOT_FOUND.into_response();
   }
   match state.api.cookie_snapshot(id).await {
      Some(snapshot) => {
         (
            [(
               header::CONTENT_DISPOSITION,
               format!("attachment; filename=\"account-{id}-cookies.json\""),
            )],
            Json(snapshot),
         )
            .into_response()
      },
      None => {
         (
            StatusCode::CONFLICT,
            "Cookie export is available only for complete cookie-authenticated accounts.",
         )
            .into_response()
      },
   }
}

fn now_unix() -> i64 {
   time::OffsetDateTime::now_utc().unix_timestamp()
}

fn search_route(path: &str) -> bool {
   let mut segments = path.split('/').skip(1);
   match (
      segments.next(),
      segments.next(),
      segments.next(),
      segments.next(),
   ) {
      (Some("search"), None | Some("rss"), None, None)
      | (Some("hashtag"), Some(_), None, None)
      | (Some(_), Some("search"), None | Some("rss"), None) => true,
      (Some(_), Some("status" | "statuses"), Some(_), Some("quotes")) => segments.next().is_none(),
      _ => false,
   }
}

pub(super) fn search_result(mut response: Response, empty: bool) -> Response {
   response.extensions_mut().insert(if empty {
      Outcome::Empty
   } else {
      Outcome::Success
   });
   response
}

fn outcome(response: &Response) -> Outcome {
   if let Some(recorded) = response.extensions().get::<Outcome>() {
      return *recorded;
   }
   let status = response.status();
   match status {
      StatusCode::TOO_MANY_REQUESTS => Outcome::RateLimited,
      StatusCode::UNAUTHORIZED | StatusCode::PAYMENT_REQUIRED | StatusCode::FORBIDDEN => {
         Outcome::Rejected
      },
      StatusCode::REQUEST_TIMEOUT => Outcome::Failed,
      _ if status.is_server_error() => Outcome::Failed,
      // A redirect finished the request: the canonical-query redirect and the
      // pass redirect after a solved challenge both land here. Counting them as
      // "other" made normal navigation look like unexplained failures.
      _ if status.is_redirection() => Outcome::Success,
      _ => Outcome::Other,
   }
}

pub(crate) async fn record_search(
   State(state): State<AppState>,
   request: Request,
   next: Next,
) -> Response {
   let measured = state.stats.as_ref().filter(|_| {
      matches!(*request.method(), Method::GET | Method::POST) && search_route(request.uri().path())
   });
   let start = measured.map(|stats| {
      stats.request_started();
      Instant::now()
   });
   let response = next.run(request).await;
   if let Some((stats, started_at)) = measured.zip(start) {
      stats.request_finished(outcome(&response), started_at.elapsed());
   }
   response
}

#[cfg(test)]
mod tests {
   use std::collections::BTreeMap;

   use super::*;
   use crate::types::SessionKind;

   #[test]
   fn scopes_admin_and_search_paths() {
      for path in [
         "/admin",
         "/admin/stats/data",
         "/import/admin",
         "/import/admin/index.md",
      ] {
         assert!(private_path(path));
      }
      for path in ["/administration", "/import", "/import/admins"] {
         assert!(!private_path(path));
      }
      for path in [
         "/search",
         "/search/rss",
         "/alice/search",
         "/hashtag/rust",
         "/alice/status/1/quotes",
      ] {
         assert!(search_route(path));
      }
      assert!(!search_route("/admin/stats"));
      assert!(!search_route("/opensearch"));
   }

   #[test]
   fn duplicate_before_query_takes_last_value_without_error() {
      let uri: Uri = "/admin/stats/restrictions?before=12675&before=12625"
         .parse()
         .unwrap();
      let query = InvestigationQuery::from_uri(&uri);
      assert_eq!(query.before, Some(12625));

      let noisy: Uri =
         "/admin/stats/restrictions?q=alice&q=bob&kind=2&kind=5&status=active&pool=view&before=999&before=bad&before=-3"
            .parse()
            .unwrap();
      let parsed = InvestigationQuery::from_uri(&noisy);
      assert_eq!(parsed.q.as_deref(), Some("bob"));
      assert_eq!(parsed.kind.as_deref(), Some("5"));
      assert_eq!(parsed.status.as_deref(), Some("active"));
      assert_eq!(parsed.pool.as_deref(), Some("view"));
      assert_eq!(parsed.before, Some(999));

      let invalid_only: Uri = "/admin/stats/restrictions?before=notanumber"
         .parse()
         .unwrap();
      assert_eq!(InvestigationQuery::from_uri(&invalid_only).before, None);
   }

   #[test]
   fn investigation_query_to_uri_never_duplicates_parameters() {
      let initial: Uri =
         "/admin/stats/restrictions?q=@alice+test&kind=2&status=active&pool=view&before=12675&before=12625"
            .parse()
            .unwrap();
      let page1 = InvestigationQuery::from_uri(&initial);
      let uri2 = page1.to_uri("/admin/stats/restrictions", Some(Some(12575)));
      assert_eq!(uri2.matches("before=").count(), 1);
      assert_eq!(uri2.matches("q=").count(), 1);
      assert_eq!(uri2.matches("kind=").count(), 1);
      assert_eq!(uri2.matches("status=").count(), 1);
      assert_eq!(uri2.matches("pool=").count(), 1);
      assert!(uri2.contains("before=12575"));
      assert!(!uri2.contains("12625"));

      let page2 = InvestigationQuery::from_uri(&uri2.parse().unwrap());
      assert_eq!(page2.q.as_deref(), Some("@alice test"));
      assert_eq!(page2.before, Some(12575));

      let uri3 = page2.to_uri("/admin/stats/restrictions", Some(Some(12525)));
      assert_eq!(uri3.matches("before=").count(), 1);
      assert!(uri3.contains("before=12525"));

      let latest = page2.to_uri("/admin/stats/restrictions", Some(None));
      assert_eq!(latest.matches("before=").count(), 0);
      assert_eq!(latest.matches("q=").count(), 1);
      assert_eq!(latest.matches("pool=").count(), 1);
   }

   #[test]
   fn range_accepts_six_hours_and_last_valid_minutes() {
      assert_eq!(
         range_from_uri(&"/admin/stats?minutes=360".parse().unwrap()),
         360
      );
      assert_eq!(
         range_from_uri(&"/admin/stats?minutes=15&minutes=1440&minutes=999".parse().unwrap()),
         1440
      );
      assert_eq!(range_from_uri(&"/admin/stats".parse().unwrap()), 60);
   }

   #[test]
   fn suspended_active_restriction_checks_suspended_or_retired() {
      let event = AccountEvent::new(7, Restriction::Suspended, "account/settings", "403");
      let mut account = crate::api::auth::AccountState {
         id:                   7,
         username:             "alice".to_owned(),
         kind:                 SessionKind::Cookie,
         pool:                 "view",
         demoted_from_search:  false,
         available:            true,
         credentials_rejected: false,
         search_banned:        false,
         profile_banned:       false,
         ct0_dead:             false,
         suspended:            false,
         retired:              false,
         filters_cleared:      false,
         filters_retry_at:     0,
         filters_failed:       true,
         globally_limited:     false,
         limited_at:           None,
         profile_strikes:      0,
         active_calls:         0,
         last_active:          None,
         rate_limits:          BTreeMap::new(),
         refused_operations:   BTreeMap::new(),
         verified_operations:  BTreeMap::new(),
      };
      // `filters_failed` alone is not an active suspension.
      assert!(!active_restriction(&event, &account, 1_000));
      account.suspended = true;
      assert!(active_restriction(&event, &account, 1_000));
      account.suspended = false;
      account.retired = true;
      assert!(active_restriction(&event, &account, 1_000));
   }

   #[tokio::test]
   async fn private_response_headers_cover_errors() {
      use tower::ServiceExt as _;
      let app = Router::new()
         .route("/admin/stats", get(|| async { StatusCode::FORBIDDEN }))
         .layer(axum::middleware::from_fn(admin_no_store));
      let response = app
         .oneshot(
            Request::builder()
               .uri("/admin/stats")
               .body(axum::body::Body::empty())
               .unwrap(),
         )
         .await
         .unwrap();
      assert_eq!(response.status(), StatusCode::FORBIDDEN);
      assert_eq!(
         response.headers()[header::CACHE_CONTROL],
         "private, no-store"
      );
      assert_eq!(
         response.headers()["cloudflare-cdn-cache-control"],
         "no-store"
      );
   }
}
