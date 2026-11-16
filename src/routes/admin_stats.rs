use std::time::{
   Duration,
   Instant,
};

use axum::{
   Json,
   Router,
   extract::{
      Path,
      Query,
      Request,
      State,
   },
   http::{
      HeaderValue,
      Method,
      StatusCode,
      header,
   },
   middleware::Next,
   response::{
      Html,
      IntoResponse as _,
      Response,
   },
   routing::get,
};
use axum_extra::extract::CookieJar;
use serde::{
   Deserialize,
   Serialize,
};

use super::import::{
   administrator,
   no_store,
};
use crate::{
   AppState,
   accounts::AccountIdentity,
   stats::{
      AccountEvent,
      Outcome,
      Restriction,
   },
   types::Prefs,
   views::admin_stats::{
      self as view,
      Bucket,
      Dashboard,
      RestrictionCount,
   },
};

const EVENT_PAGE: i64 = 50;

#[derive(Clone, Default, Deserialize)]
pub(crate) struct InvestigationQuery {
   pub kind:   Option<String>,
   pub status: Option<String>,
   pub pool:   Option<String>,
   pub q:      Option<String>,
   pub before: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct Observation {
   pub event:    AccountEvent,
   pub identity: Option<AccountIdentity>,
   pub active:   Option<bool>,
}

#[derive(Serialize)]
pub(crate) struct InvestigationData {
   pub(crate) events:      Vec<Observation>,
   pub(crate) next_before: Option<i64>,
   now:                    i64,
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
   pub(crate) verdict:     &'static str,
   pub(crate) detail:      Option<String>,
   /// The stored flags before the probe, so the operator can see what changed.
   pub(crate) before:      ProbeFlags,
   pub(crate) after:       ProbeFlags,
   /// True when a stored suspension was contradicted and cleared.
   pub(crate) reinstated:  bool,
}

#[derive(Serialize)]
pub(crate) struct ProbeFlags {
   pub(crate) suspended: bool,
   pub(crate) retired:   bool,
   pub(crate) pool:      &'static str,
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
      .route("/admin/stats/sessions/{id}/probe", axum::routing::post(probe))
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

fn range(request: &Request) -> i32 {
   request
      .uri()
      .query()
      .and_then(|query| {
         query
            .split('&')
            .find_map(|part| part.strip_prefix("minutes="))
            .and_then(|value| value.parse().ok())
      })
      .filter(|value| matches!(value, 15 | 60 | 1440 | 10080))
      .unwrap_or(60)
}

async fn snapshot(state: &AppState, minutes: i32) -> Option<Dashboard> {
   let store = state.stats.as_ref()?;
   tokio::time::timeout(Duration::from_secs(5), async {
      let (data, pool, capacity) = tokio::join!(
         store.snapshot(minutes),
         state.accounts.stats(),
         state.api.search_budget_estimate(),
      );
      let data = data.ok()?;
      let pool = pool.ok().map(|pool| (pool.search, pool.view));
      Some(Dashboard {
         minutes,
         pool,
         capacity: pool.map(|_| capacity),
         dropped_events: data.dropped_events,
         recent: vec![],
         buckets: data
            .buckets
            .into_iter()
            .map(|row| {
               Bucket {
                  minute:       row.minute_unix,
                  incoming:     row.counts.received,
                  success:      row.counts.success + row.counts.empty,
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
   if !administrator(&state, &jar).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   let minutes = range(&request);
   let theme = Prefs::from_cookies(&jar, &state.config).theme;
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
      Some(data) => Json(data).into_response(),
      None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
   }
}

async fn restrictions_data(
   State(state): State<AppState>,
   jar: CookieJar,
   Query(query): Query<InvestigationQuery>,
) -> Response {
   if !administrator(&state, &jar).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   match investigation(&state, &query).await {
      Some(data) => Json(data).into_response(),
      None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
   }
}

fn restriction_kind(value: Option<&str>) -> Option<Option<Restriction>> {
   match value {
      None | Some("") | Some("all") => Some(None),
      Some(value) => Restriction::from_i16(value.parse().ok()?).map(Some),
   }
}

fn active_restriction(
   event: &AccountEvent,
   account: &crate::api::auth::AccountState,
   now: i64,
) -> bool {
   match event.kind {
      Restriction::Suspended => {
         account.credentials_rejected
            || account.filters_failed
            || account.search_banned
            || account.profile_banned
      },
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
   let kind = restriction_kind(query.kind.as_deref())?;
   let pool = query
      .pool
      .as_deref()
      .filter(|pool| matches!(*pool, "search" | "view"));
   let status = query
      .status
      .as_deref()
      .filter(|status| matches!(*status, "active" | "historical"));
   let ids = match query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
      Some(search) => Some(state.accounts.find_identities(search, 500).await.ok()?),
      None => None,
   };
   let now = now_unix();
   let mut observations = Vec::with_capacity(EVENT_PAGE as usize);
   let mut cursor = query.before;
   let mut next_before = None;
   // Current-state filters need joining to runtime state. Fetch bounded batches
   // instead of truncating each batch before applying those filters.
   for _ in 0..8 {
      let events = store
         .recent_account_events_filtered(kind, ids.as_deref(), cursor, 200)
         .await
         .ok()?;
      if events.is_empty() {
         break;
      }
      cursor = events.last().map(|event| event.id);
      let event_ids = events
         .iter()
         .map(|event| event.session_id)
         .collect::<Vec<_>>();
      let identities = state
         .accounts
         .identities(&event_ids)
         .await
         .unwrap_or_default();
      for event in events {
         let current = state.api.sessions.account_state(event.session_id).await;
         let identity = identities.get(&event.session_id).cloned().or_else(|| {
            current.as_ref().map(|account| {
               AccountIdentity {
                  id:       account.id,
                  username: account.username.clone(),
                  kind:     account.kind,
                  pool:     if account.pool.is_empty() {
                     "view".to_owned()
                  } else {
                     account.pool.to_owned()
                  },
               }
            })
         });
         let event_pool = identity
            .as_ref()
            .map(|identity| identity.pool.as_str())
            .or_else(|| current.as_ref().map(|account| account.pool))
            .unwrap_or("");
         if pool.is_some_and(|selected| event_pool != selected) {
            continue;
         }
         let active = current
            .as_ref()
            .map(|account| active_restriction(&event, account, now));
         if status.is_some_and(|selected| active != Some(selected == "active")) {
            continue;
         }
         let event_id = event.id;
         observations.push(Observation {
            event,
            identity,
            active,
         });
         if observations.len() == EVENT_PAGE as usize {
            next_before = Some(event_id);
            break;
         }
      }
      if next_before.is_some() {
         break;
      }
   }
   Some(InvestigationData {
      events: observations,
      next_before,
      now,
   })
}

#[axum::debug_handler]
async fn restrictions(
   State(state): State<AppState>,
   jar: CookieJar,
   Query(query): Query<InvestigationQuery>,
   request: Request,
) -> Response {
   if !administrator(&state, &jar).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   let data = investigation(&state, &query).await;
   let theme = Prefs::from_cookies(&jar, &state.config).theme;
   Html(view::restrictions(&theme, data.as_ref(), &query, &request.uri().to_string()).into_string())
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
      Some(data) => Json(data).into_response(),
      None => StatusCode::NOT_FOUND.into_response(),
   }
}

async fn session_page(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<i64>,
) -> Response {
   if !administrator(&state, &jar).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   let data = account_data(&state, id).await;
   let theme = Prefs::from_cookies(&jar, &state.config).theme;
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
async fn probe(State(state): State<AppState>, jar: CookieJar, Path(id): Path<i64>) -> Response {
   if !administrator(&state, &jar).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   let Some(before) = state.api.sessions.account_state(id).await else {
      return StatusCode::NOT_FOUND.into_response();
   };
   let before = ProbeFlags {
      suspended: before.suspended,
      retired:   before.retired,
      pool:      before.pool,
   };

   let (verdict, detail) = match state.api.probe_account(id).await {
      Ok(verdict) => (verdict.label(), None),
      Err(err) => ("error", Some(err.to_string())),
   };

   // Only a live answer clears a verdict. A suspension or a dead credential
   // confirms it, and an error says nothing at all.
   let reinstated = verdict == "alive" && (before.suspended || before.retired);
   if reinstated {
      state.api.sessions.reinstate(id).await;
   }

   let after = state
      .api
      .sessions
      .account_state(id)
      .await
      .map_or(before_pool(), |account| ProbeFlags {
         suspended: account.suspended,
         retired:   account.retired,
         pool:      account.pool,
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
      Some(cookies) => {
         (
            [(
               header::CONTENT_DISPOSITION,
               format!("attachment; filename=\"account-{id}-cookies.json\""),
            )],
            Json(cookies),
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
   if let Some(outcome) = response.extensions().get::<Outcome>() {
      return *outcome;
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
   if let Some((stats, start)) = measured.zip(start) {
      stats.request_finished(outcome(&response), start.elapsed());
   }
   response
}

#[cfg(test)]
mod tests {
   use super::*;

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
