//! Account import dashboard: public counts, administrator-only writes.

use std::{
   future::Future,
   time::Duration,
};

use axum::{
   Router,
   body::to_bytes,
   extract::{
      Request,
      State,
   },
   http::{
      HeaderMap,
      HeaderValue,
      StatusCode,
      header,
   },
   middleware::{
      self,
      Next,
   },
   response::{
      IntoResponse as _,
      Response,
   },
   routing::get,
};
use axum_extra::extract::CookieJar;
use data_encoding::HEXLOWER;
use ring::hmac;
use time::OffsetDateTime;
pub(super) use view::{
   ADMIN_PATH,
   PUBLIC_PATH,
   REFRESH_PATH,
};

use crate::{
   AppState,
   accounts::ImportSummary,
   passes::SESSION_COOKIE,
   types::Prefs,
   utils::host::current_origin,
   views::import::{
      self as view,
      AdminNotice,
      Snapshot,
   },
};

pub(super) const LEGACY_ADMIN_PATH: &str = "/import/admin";

const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_FORM_BYTES: usize = 3 * MAX_TEXT_BYTES + 1024;
const MAX_LINES: usize = 1_000;
const BODY_TIMEOUT: Duration = Duration::from_secs(10);
const CSRF_LIFETIME: i64 = 30 * 60;

trait Backend: Clone + Send + Sync + 'static {
   fn snapshot(&self) -> impl Future<Output = Option<Snapshot>> + Send;
   fn administrator(&self, jar: &CookieJar) -> impl Future<Output = bool> + Send;
   fn csrf_key(&self) -> &str;
   fn theme(&self, jar: &CookieJar) -> String;
   fn origin(&self) -> String;
   fn upload(&self, text: &str, search: bool)
   -> impl Future<Output = Option<ImportSummary>> + Send;
   fn on_accounts_imported(&self) {}
}

impl Backend for AppState {
   async fn snapshot(&self) -> Option<Snapshot> {
      let stats = self.accounts.stats().await.ok()?;
      let batches = self
         .accounts
         .recent_import_batches()
         .await
         .ok()
         .map(|rows| {
            rows
               .into_iter()
               .map(|(timestamp, counts)| view::Batch { timestamp, counts })
               .collect()
         });
      Some(Snapshot {
         stats,
         batches,
         updated: OffsetDateTime::now_utc(),
      })
   }

   async fn administrator(&self, jar: &CookieJar) -> bool {
      administrator(self, jar).await
   }

   fn theme(&self, jar: &CookieJar) -> String {
      Prefs::from_cookies(jar, &self.config).theme
   }

   fn csrf_key(&self) -> &str {
      &self.config.config.hmac_key
   }

   fn origin(&self) -> String {
      current_origin(self.config.url_prefix())
   }

   async fn upload(&self, text: &str, search: bool) -> Option<ImportSummary> {
      self.accounts.import_text(text, search).await.ok()
   }

   fn on_accounts_imported(&self) {
      self.api.wake_account_reconcile();
   }
}

pub(super) fn router() -> Router<AppState> {
   dashboard_routes::<AppState>().merge(admin_routes::<AppState>())
}

fn dashboard_routes<B: Backend>() -> Router<B> {
   Router::new()
      .route(view::PUBLIC_PATH, get(dashboard::<B>))
      .route(view::REFRESH_PATH, get(refresh::<B>))
}

fn admin_routes<B: Backend>() -> Router<B> {
   let methods = get(admin::<B>)
      .post(upload::<B>)
      .layer(tower_http::timeout::TimeoutLayer::with_status_code(
         StatusCode::REQUEST_TIMEOUT,
         Duration::from_secs(40),
      ))
      .layer(middleware::from_fn(private));
   Router::new()
      .route(ADMIN_PATH, methods.clone())
      .route(LEGACY_ADMIN_PATH, methods)
}

pub(super) async fn administrator(state: &AppState, jar: &CookieJar) -> bool {
   let Some(id) = state.passes.signed_in(jar) else {
      return false;
   };
   state
      .passes
      .account(id)
      .await
      .is_ok_and(|account| account.is_some_and(|a| a.is_admin()))
}

pub(super) fn no_store(headers: &mut HeaderMap) {
   headers.insert(
      header::CACHE_CONTROL,
      HeaderValue::from_static("private, no-store"),
   );
   headers.insert("cdn-cache-control", HeaderValue::from_static("no-store"));
   headers.insert(
      "cloudflare-cdn-cache-control",
      HeaderValue::from_static("no-store"),
   );
}

async fn private(request: Request, next: Next) -> Response {
   let mut response = next.run(request).await;
   let headers = response.headers_mut();
   no_store(headers);
   headers.insert(
      "x-robots-tag",
      HeaderValue::from_static("noindex, nofollow"),
   );
   // `no-referrer` can also null out Origin on a browser's form POST.
   headers.insert(
      header::REFERRER_POLICY,
      HeaderValue::from_static("same-origin"),
   );
   headers.insert(
      header::CONTENT_SECURITY_POLICY,
      HeaderValue::from_static(
         "default-src 'none'; style-src 'self'; form-action 'self'; base-uri 'none'; \
          frame-ancestors 'none'",
      ),
   );
   response
}

fn public_response(
   snapshot: Option<Snapshot>,
   render: impl FnOnce(Option<&Snapshot>) -> maud::Markup,
) -> Response {
   let status = if snapshot.is_some() {
      StatusCode::OK
   } else {
      StatusCode::SERVICE_UNAVAILABLE
   };
   let cache = if snapshot.is_some() {
      "public, max-age=10"
   } else {
      "no-store"
   };
   (
      status,
      [(header::CACHE_CONTROL, cache)],
      render(snapshot.as_ref()),
   )
      .into_response()
}

async fn dashboard<B: Backend>(State(state): State<B>, jar: CookieJar) -> Response {
   let theme = state.theme(&jar);
   let (snapshot, is_admin) = tokio::join!(state.snapshot(), state.administrator(&jar));
   let mut response = public_response(snapshot, |snap| view::dashboard(&theme, snap, is_admin));
   no_store(response.headers_mut());
   response
}

async fn refresh<B: Backend>(State(state): State<B>) -> Response {
   public_response(state.snapshot().await, view::figures)
}

fn csrf_message(session: &str, origin: &str, expires: i64) -> String {
   // Keep the signing context stable for forms issued before the route moved.
   format!("teapot/import/admin/POST\n{origin}\n{expires}\n{session}")
}

fn csrf_key(secret: &str) -> hmac::Key {
   hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes())
}

fn issue_csrf(secret: &str, session: &str, origin: &str, now: i64) -> String {
   let expires = now + CSRF_LIFETIME;
   let tag = hmac::sign(
      &csrf_key(secret),
      csrf_message(session, origin, expires).as_bytes(),
   );
   format!("{expires}.{}", HEXLOWER.encode(tag.as_ref()))
}

fn valid_csrf(secret: &str, session: &str, origin: &str, token: &str, now: i64) -> bool {
   let Some((expires, tag)) = token.split_once('.') else {
      return false;
   };
   let Ok(expires) = expires.parse::<i64>() else {
      return false;
   };
   if expires < now || expires > now + CSRF_LIFETIME || tag.len() != 64 {
      return false;
   }
   let Ok(tag) = HEXLOWER.decode(tag.as_bytes()) else {
      return false;
   };
   hmac::verify(
      &csrf_key(secret),
      csrf_message(session, origin, expires).as_bytes(),
      &tag,
   )
   .is_ok()
}

pub(super) fn cross_origin(headers: &HeaderMap, origin: &str) -> bool {
   if headers
      .get("sec-fetch-site")
      .is_some_and(|value| !matches!(value.as_bytes(), b"same-origin" | b"none"))
   {
      return true;
   }
   let Some(raw_origin) = headers.get(header::ORIGIN) else {
      return false;
   };
   if raw_origin.as_bytes() == origin.as_bytes() {
      return false;
   }
   let Some(authority) = raw_origin
      .to_str()
      .ok()
      .and_then(|value| {
         value
            .strip_prefix("https://")
            .or_else(|| value.strip_prefix("http://"))
      })
      .filter(|candidate| {
         !candidate.is_empty()
            && !candidate
               .bytes()
               .any(|byte| matches!(byte, b'/' | b'?' | b'#' | b'@' | b' ' | b'\t'))
      })
   else {
      return true;
   };
   let host = headers
      .get(header::HOST)
      .and_then(|value| value.to_str().ok())
      .map(str::trim)
      .filter(|value| !value.is_empty());
   !host.is_some_and(|value| authority.eq_ignore_ascii_case(value))
}

fn now() -> i64 {
   OffsetDateTime::now_utc().unix_timestamp()
}

fn pool_selection(pool: Option<&str>) -> &'static str {
   if pool == Some("search") {
      "search"
   } else {
      "view"
   }
}

fn form<B: Backend>(
   state: &B,
   theme: &str,
   session: &str,
   selected_pool: &str,
   status: StatusCode,
   notice: Option<AdminNotice>,
) -> Response {
   let token = issue_csrf(state.csrf_key(), session, &state.origin(), now());
   (
      status,
      view::admin(
         theme,
         &token,
         selected_pool,
         MAX_TEXT_BYTES,
         MAX_LINES,
         notice.as_ref(),
      ),
   )
      .into_response()
}

async fn admin<B: Backend>(State(state): State<B>, jar: CookieJar) -> Response {
   let theme = state.theme(&jar);
   let Some(session) = jar
      .get(SESSION_COOKIE)
      .filter(|_| !state.csrf_key().is_empty())
   else {
      return (StatusCode::FORBIDDEN, view::forbidden(&theme)).into_response();
   };
   if !state.administrator(&jar).await {
      return (StatusCode::FORBIDDEN, view::forbidden(&theme)).into_response();
   }
   form(
      &state,
      &theme,
      session.value(),
      "view",
      StatusCode::OK,
      None,
   )
}

async fn upload<B: Backend>(State(state): State<B>, jar: CookieJar, request: Request) -> Response {
   let origin = state.origin();
   if cross_origin(request.headers(), &origin) {
      return (
         StatusCode::FORBIDDEN,
         "The form must be submitted from the same site. Reload /admin/import and try again.",
      )
         .into_response();
   }
   let theme = state.theme(&jar);
   let Some(session) = jar
      .get(SESSION_COOKIE)
      .filter(|_| !state.csrf_key().is_empty())
   else {
      return (StatusCode::FORBIDDEN, view::forbidden(&theme)).into_response();
   };
   if !state.administrator(&jar).await {
      return (StatusCode::FORBIDDEN, view::forbidden(&theme)).into_response();
   }
   let session = session.value();
   let error_with_pool = |selected_pool, status, text| {
      form(
         &state,
         &theme,
         session,
         selected_pool,
         status,
         Some(AdminNotice::Error(text)),
      )
   };
   if request.uri().query().is_some() {
      return error_with_pool(
         "view",
         StatusCode::BAD_REQUEST,
         "Use the form without URL parameters.",
      );
   }
   let form_encoded = request
      .headers()
      .get(header::CONTENT_TYPE)
      .and_then(|value| value.to_str().ok())
      .is_some_and(|value| {
         value.split(';').next().is_some_and(|kind| {
            kind
               .trim()
               .eq_ignore_ascii_case("application/x-www-form-urlencoded")
         })
      });
   if !form_encoded {
      return error_with_pool(
         "view",
         StatusCode::UNSUPPORTED_MEDIA_TYPE,
         "Use the upload form to submit account data.",
      );
   }
   if request
      .headers()
      .get(header::CONTENT_LENGTH)
      .and_then(|length| length.to_str().ok())
      .and_then(|length| length.parse::<u64>().ok())
      .is_some_and(|length| length > MAX_FORM_BYTES as u64)
   {
      return error_with_pool(
         "view",
         StatusCode::PAYLOAD_TOO_LARGE,
         "The submitted form is too large.",
      );
   }
   let body =
      match tokio::time::timeout(BODY_TIMEOUT, to_bytes(request.into_body(), MAX_FORM_BYTES)).await
      {
         Ok(Ok(body)) => body,
         Ok(Err(err))
            if std::error::Error::source(&err)
               .is_some_and(|cause| cause.is::<http_body_util::LengthLimitError>()) =>
         {
            return error_with_pool(
               "view",
               StatusCode::PAYLOAD_TOO_LARGE,
               "The submitted form is too large.",
            );
         },
         Ok(Err(_)) => {
            return error_with_pool(
               "view",
               StatusCode::BAD_REQUEST,
               "Could not read the submitted form.",
            );
         },
         Err(_) => {
            return error_with_pool(
               "view",
               StatusCode::REQUEST_TIMEOUT,
               "Timed out reading the submitted form.",
            );
         },
      };
   let (mut csrf, mut pool, mut accounts) = (None, None, None);
   for (key, value) in form_urlencoded::parse(&body) {
      let field = match key.as_ref() {
         "csrf" => &mut csrf,
         "pool" => &mut pool,
         "accounts" => &mut accounts,
         _ => {
            return error_with_pool(
               pool_selection(pool.as_deref()),
               StatusCode::BAD_REQUEST,
               "Unexpected upload form fields.",
            );
         },
      };
      if field.replace(value).is_some() {
         return error_with_pool(
            pool_selection(pool.as_deref()),
            StatusCode::BAD_REQUEST,
            "Unexpected upload form fields.",
         );
      }
   }
   let selected_pool = pool_selection(pool.as_deref());
   if !csrf.is_some_and(|token| valid_csrf(state.csrf_key(), session, &origin, &token, now())) {
      return error_with_pool(
         selected_pool,
         StatusCode::FORBIDDEN,
         "The form expired or could not be verified. Reload the page and try again.",
      );
   }
   let search = match pool.as_deref() {
      Some("view") => false,
      Some("search") => true,
      _ => {
         return error_with_pool(
            selected_pool,
            StatusCode::BAD_REQUEST,
            "Choose an import pool.",
         );
      },
   };
   let Some(accounts) = accounts.filter(|value| !value.trim().is_empty()) else {
      return error_with_pool(
         selected_pool,
         StatusCode::BAD_REQUEST,
         "Enter account data before importing.",
      );
   };
   if accounts.len() > MAX_TEXT_BYTES || accounts.lines().take(MAX_LINES + 1).count() > MAX_LINES {
      return error_with_pool(
         selected_pool,
         StatusCode::PAYLOAD_TOO_LARGE,
         "The account text exceeds the upload limit.",
      );
   }
   match state.upload(&accounts, search).await {
      Some(counts) => {
         if counts.imported > 0 {
            state.on_accounts_imported();
         }
         form(
            &state,
            &theme,
            session,
            selected_pool,
            StatusCode::OK,
            Some(AdminNotice::Saved(counts)),
         )
      },
      None => {
         error_with_pool(
            selected_pool,
            StatusCode::SERVICE_UNAVAILABLE,
            "The import could not be saved. Check the dashboard before retrying.",
         )
      },
   }
}

#[cfg(test)]
mod tests {
   use std::sync::{
      Arc,
      atomic::{
         AtomicUsize,
         Ordering,
      },
   };

   use axum::body::Body;
   use tower::ServiceExt as _;

   use super::*;
   use crate::accounts::Stats;

   const ADMIN: &str = "synthetic-admin-cookie";
   const SECRET: &str = "synthetic private import input <script>";
   const ORIGIN: &str = "https://example.test";
   const KEY: &str = "synthetic non-production signing key";

   #[derive(Clone, Default)]
   struct Fixture {
      writes: Arc<AtomicUsize>,
      failed: bool,
   }

   impl Backend for Fixture {
      async fn snapshot(&self) -> Option<Snapshot> {
         (!self.failed).then(|| {
            Snapshot {
               stats:   Stats {
                  total:    21,
                  search:   8,
                  view:     13,
                  ct0_dead: 0,
                  rejected: 0,
               },
               batches: Some(vec![view::Batch {
                  timestamp: 1_700_000_000,
                  counts:    ImportSummary {
                     imported:   7,
                     duplicates: 2,
                     invalid:    1,
                  },
               }]),
               updated: OffsetDateTime::from_unix_timestamp(1_700_000_030).unwrap(),
            }
         })
      }

      async fn administrator(&self, jar: &CookieJar) -> bool {
         jar.get(SESSION_COOKIE)
            .is_some_and(|cookie| matches!(cookie.value(), ADMIN | "another-admin"))
      }

      fn theme(&self, jar: &CookieJar) -> String {
         jar.get("theme")
            .map_or("Teapot", |cookie| cookie.value())
            .into()
      }

      fn csrf_key(&self) -> &str {
         KEY
      }
      fn origin(&self) -> String {
         ORIGIN.into()
      }

      async fn upload(&self, text: &str, search: bool) -> Option<ImportSummary> {
         assert_eq!(text, SECRET);
         assert!(search);
         self.writes.fetch_add(1, Ordering::Relaxed);
         (!self.failed).then_some(ImportSummary {
            imported:   1,
            duplicates: 0,
            invalid:    0,
         })
      }
   }

   fn app(state: Fixture) -> Router {
      dashboard_routes::<Fixture>()
         .merge(admin_routes::<Fixture>())
         .layer(middleware::from_fn(
            super::super::middleware::prefs_middleware,
         ))
         .with_state(state)
   }

   fn get_request(path: &str, session: Option<&str>) -> Request {
      let mut request = Request::builder().uri(path);
      if let Some(session) = session {
         request = request.header(header::COOKIE, format!("{SESSION_COOKIE}={session}"));
      }
      request.body(Body::empty()).unwrap()
   }

   fn post_request(path: &str, session: Option<&str>, origin: &str, body: String) -> Request {
      let mut request = Request::builder()
         .method("POST")
         .uri(path)
         .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
         .header(header::ORIGIN, origin);
      if let Some(session) = session {
         request = request.header(header::COOKIE, format!("{SESSION_COOKIE}={session}"));
      }
      request.body(Body::from(body)).unwrap()
   }

   fn payload(csrf: &str, accounts: &str) -> String {
      form_urlencoded::Serializer::new(String::new())
         .append_pair("csrf", csrf)
         .append_pair("pool", "search")
         .append_pair("accounts", accounts)
         .finish()
   }

   async fn body(response: Response) -> String {
      String::from_utf8(
         to_bytes(response.into_body(), MAX_FORM_BYTES)
            .await
            .unwrap()
            .to_vec(),
      )
      .unwrap()
   }

   fn assert_private(response: &Response, status: StatusCode) {
      assert_eq!(response.status(), status);
      assert_eq!(
         response.headers()[header::CACHE_CONTROL],
         "private, no-store"
      );
      assert_eq!(response.headers()["cdn-cache-control"], "no-store");
      assert_eq!(response.headers()["x-robots-tag"], "noindex, nofollow");
   }

   #[tokio::test]
   async fn public_dashboard_and_refresh_show_only_counts_and_times() {
      let app = app(Fixture::default());
      for (path, dashboard) in [(view::PUBLIC_PATH, true), (view::REFRESH_PATH, false)] {
         let response = app
            .clone()
            .oneshot(get_request(path, Some(ADMIN)))
            .await
            .unwrap();
         assert_eq!(response.status(), StatusCode::OK);
         assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            if dashboard {
               "private, no-store"
            } else {
               "public, max-age=10"
            }
         );
         assert!(!response.headers().contains_key(header::SET_COOKIE));
         let html = body(response).await;
         assert_eq!(html.contains("every 60s"), dashboard);
         assert!(html.contains("2023-11-14T22:13:50Z"));
         assert!(html.contains("2023-11-14T22:13:20Z"));
         assert!(html.contains("<dd>21</dd>"));
         assert!(html.contains(">7<") && html.contains("added"));
         assert!(!html.contains("<form"));
         assert_eq!(html.contains("href=\"/admin/import\""), dashboard);
         assert!(!html.contains(ADMIN));
         assert!(!html.contains("name=\"csrf\""));
      }
      for visitor in [None, Some("ordinary-pass-holder")] {
         let response = app
            .clone()
            .oneshot(get_request(view::PUBLIC_PATH, visitor))
            .await
            .unwrap();
         assert_eq!(response.status(), StatusCode::OK);
         let html = body(response).await;
         assert!(!html.contains("href=\"/admin/import\""));
         assert!(!html.contains("Import accounts"));
      }
   }

   #[tokio::test]
   async fn saved_theme_is_used_only_in_full_pages() {
      let app = app(Fixture::default());
      for (path, themed) in [
         (view::PUBLIC_PATH, true),
         (view::ADMIN_PATH, true),
         (view::REFRESH_PATH, false),
      ] {
         let request = Request::builder()
            .uri(path)
            .header(
               header::COOKIE,
               format!("{SESSION_COOKIE}={ADMIN}; theme=Dracula"),
            )
            .body(Body::empty())
            .unwrap();
         let response = app.clone().oneshot(request).await.unwrap();
         assert_eq!(response.status(), StatusCode::OK);
         if themed {
            assert_eq!(
               response.headers()["cloudflare-cdn-cache-control"],
               "no-store"
            );
         }
         let html = body(response).await;
         assert_eq!(html.contains("/css/themes/dracula.css?teapawt17"), themed);
         assert_eq!(html.contains("/css/import.css?v=4"), themed);
      }
   }

   #[tokio::test]
   async fn admin_post_binds_csrf_to_session_origin_and_time() {
      for path in ["/admin/import", "/import/admin"] {
         exercise_admin_csrf(path).await;
      }
   }

   async fn exercise_admin_csrf(path: &str) {
      let state = Fixture::default();
      let app = app(state.clone());
      for session in [None, Some("ordinary-pass-holder")] {
         let response = app
            .clone()
            .oneshot(get_request(path, session))
            .await
            .unwrap();
         assert_private(&response, StatusCode::FORBIDDEN);
         assert!(!body(response).await.contains("<form"));
      }
      let response = app
         .clone()
         .oneshot(get_request(path, Some(ADMIN)))
         .await
         .unwrap();
      assert_private(&response, StatusCode::OK);
      assert_eq!(response.headers()[header::REFERRER_POLICY], "same-origin");
      let html = body(response).await;
      assert!(html.contains("method=\"post\" action=\"/admin/import\""));
      assert!(html.contains("href=\"/admin/stats\""));
      assert!(html.contains("<option value=\"view\" selected>View</option>"));
      assert!(!html.contains(ADMIN));
      let csrf = html
         .split_once("name=\"csrf\" value=\"")
         .unwrap()
         .1
         .split_once('"')
         .unwrap()
         .0;
      let expired = issue_csrf(KEY, ADMIN, ORIGIN, now() - CSRF_LIFETIME - 5);
      for (session, origin, token) in [
         (None, ORIGIN, csrf),
         (Some("ordinary-pass-holder"), ORIGIN, csrf),
         (Some("another-admin"), ORIGIN, csrf),
         (Some(ADMIN), "https://other.example.test", csrf),
         (Some(ADMIN), "null", csrf),
         (Some(ADMIN), ORIGIN, ""),
         (Some(ADMIN), ORIGIN, expired.as_str()),
      ] {
         let response = app
            .clone()
            .oneshot(post_request(path, session, origin, payload(token, SECRET)))
            .await
            .unwrap();
         assert_private(&response, StatusCode::FORBIDDEN);
         assert!(!body(response).await.contains(SECRET));
      }
      assert_eq!(state.writes.load(Ordering::Relaxed), 0);
      let response = app
         .oneshot(post_request(
            path,
            Some(ADMIN),
            ORIGIN,
            payload(csrf, SECRET),
         ))
         .await
         .unwrap();
      assert_private(&response, StatusCode::OK);
      let html = body(response).await;
      assert!(html.contains("Saved: 1 added"));
      assert!(html.contains("<option value=\"search\" selected>Search</option>"));
      assert!(!html.contains("synthetic private import input"));
      assert_eq!(state.writes.load(Ordering::Relaxed), 1);
   }

   #[test]
   fn cross_origin_accepts_same_host_origin_and_rejects_mismatches() {
      let mut same_origin_headers = HeaderMap::new();
      same_origin_headers.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
      assert!(!cross_origin(&same_origin_headers, ORIGIN));

      let mut tunnel_headers = HeaderMap::new();
      tunnel_headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8471"));
      tunnel_headers.insert(
         header::ORIGIN,
         HeaderValue::from_static("http://127.0.0.1:8471"),
      );
      assert!(!cross_origin(&tunnel_headers, ORIGIN));

      let mut mirror_headers = HeaderMap::new();
      mirror_headers.insert(header::HOST, HeaderValue::from_static("nitter.cf"));
      mirror_headers.insert(
         header::ORIGIN,
         HeaderValue::from_static("https://NITTER.CF"),
      );
      assert!(!cross_origin(&mirror_headers, ORIGIN));

      let mut wrong_origin = HeaderMap::new();
      wrong_origin.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8471"));
      wrong_origin.insert(
         header::ORIGIN,
         HeaderValue::from_static("https://evil.example"),
      );
      assert!(cross_origin(&wrong_origin, ORIGIN));

      let mut wrong_port = HeaderMap::new();
      wrong_port.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8471"));
      wrong_port.insert(
         header::ORIGIN,
         HeaderValue::from_static("http://127.0.0.1:9999"),
      );
      assert!(cross_origin(&wrong_port, ORIGIN));

      let mut null_origin = HeaderMap::new();
      null_origin.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8471"));
      null_origin.insert(header::ORIGIN, HeaderValue::from_static("null"));
      assert!(cross_origin(&null_origin, ORIGIN));

      let mut cross_site_fetch = HeaderMap::new();
      cross_site_fetch.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8471"));
      cross_site_fetch.insert(
         header::ORIGIN,
         HeaderValue::from_static("http://127.0.0.1:8471"),
      );
      cross_site_fetch.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
      assert!(cross_origin(&cross_site_fetch, ORIGIN));
   }

   #[tokio::test]
   async fn admin_limits_and_store_errors_are_private_and_do_not_echo_data() {
      let state = Fixture {
         failed: true,
         ..Fixture::default()
      };
      let app = app(state.clone());
      let csrf = issue_csrf(KEY, ADMIN, ORIGIN, now());
      for (path, submitted, status) in [
         (
            view::ADMIN_PATH,
            "~".repeat(MAX_FORM_BYTES),
            StatusCode::PAYLOAD_TOO_LARGE,
         ),
         (
            view::ADMIN_PATH,
            "x".repeat(MAX_TEXT_BYTES + 1),
            StatusCode::PAYLOAD_TOO_LARGE,
         ),
         (
            view::ADMIN_PATH,
            "line\n".repeat(MAX_LINES + 1),
            StatusCode::PAYLOAD_TOO_LARGE,
         ),
         (
            "/admin/import?prefs=theme%3Dteapot&accounts=synthetic-secret-needle",
            SECRET.into(),
            StatusCode::BAD_REQUEST,
         ),
         (
            "/import/admin?prefs=theme%3Dteapot&accounts=synthetic-secret-needle",
            SECRET.into(),
            StatusCode::BAD_REQUEST,
         ),
         (
            view::ADMIN_PATH,
            SECRET.into(),
            StatusCode::SERVICE_UNAVAILABLE,
         ),
      ] {
         let response = app
            .clone()
            .oneshot(post_request(
               path,
               Some(ADMIN),
               ORIGIN,
               payload(&csrf, &submitted),
            ))
            .await
            .unwrap();
         assert_private(&response, status);
         let html = body(response).await;
         assert!(!html.contains("synthetic private import input"));
         assert!(!html.contains("synthetic-secret-needle"));
      }
      assert_eq!(state.writes.load(Ordering::Relaxed), 1);
      let response = app
         .oneshot(get_request(view::PUBLIC_PATH, None))
         .await
         .unwrap();
      assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
      assert!(body(response).await.contains("temporarily unavailable"));
   }
}
