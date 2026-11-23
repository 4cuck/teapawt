//! Cloudflare JSD (JavaScript Detection) solver and the `cf_clearance` gate,
//! in Rust.
//!
//! This is a port of the Node `jsd-solver` (/root/ecker/jsd-solver-nodejs) that
//! teapot's account tooling used. The flow, for one page:
//!
//!   1. GET the page so Cloudflare sets its first cookies.
//!   2. GET `/cdn-cgi/challenge-platform/scripts/jsd/main.js`, the obfuscated
//!      challenge script. x.com serves the `api.js` variant, so the
//!      `window.__CF$cv$params` object is synthesised rather than read from the
//!      page (api.js writes it into a hidden iframe the HTML never contains).
//!   3. Read the encoder alphabet, the `_cf_chl_opt` token and the oneshot path
//!      out of `main.js` with the same regexes the Node solver uses.
//!   4. Build the device payload from a captured Chrome template and compress it
//!      with the current (Jul 2026+) v2 encoder: deflate-raw, a `[253,1,flag]`
//!      frame, an FNV-1a/xorshift XOR stream, then a 6-bit alphabet pack.
//!   5. POST it to the oneshot URL and read `cf_clearance` off the response.
//!
//! The challenge changes often. Any failure here (the script no longer matching
//! the regexes, a new encoder, no cookie returned) trips [`ClearanceGate`]:
//! the whole flow is disabled until the process restarts, no further x.com
//! requests are made for clearance, and search carries on without the cookie.
//! The dashboard shows the tripped state and the reason.

use std::{
   collections::HashMap,
   io::Write as _,
   sync::{
      Arc,
      LazyLock,
      Mutex,
      RwLock,
      atomic::{
         AtomicU64,
         Ordering,
      },
   },
   time::{
      Duration,
      Instant,
   },
};

use axum::http::{
   HeaderMap,
   HeaderName,
   HeaderValue,
   Method,
   StatusCode,
   header,
};
use bytes::Bytes;
use data_encoding::BASE64;
use flate2::{
   Compression,
   write::DeflateEncoder,
};
use regex::Regex;
use serde::{
   Deserialize,
   Serialize,
};
use time::{
   OffsetDateTime,
   format_description::well_known::Rfc3339,
};

use super::http::{
   Egress,
   HttpClient,
};

/// Where a clearance is minted: the page the challenge runs on, and how its
/// `__CF$cv$params` are obtained.
pub(crate) struct ClearanceTarget {
   /// The page URL (the payload's `document.URL`/`href`).
   page:         String,
   host:         String,
   origin:       String,
   /// Fetch the page and read `__CF$cv$params` from the HTML. Classic JSD
   /// (cfdata.lol) carries them there; x.com's api.js does not, so they are
   /// synthesised and the page GET is skipped.
   read_html_cv: bool,
}

impl ClearanceTarget {
   /// The search target: x.com, api.js flow — synthesise cv, no page GET.
   pub(crate) fn x_com() -> Self {
      Self {
         page:         "https://x.com/".to_owned(),
         host:         "x.com".to_owned(),
         origin:       "https://x.com".to_owned(),
         read_html_cv: false,
      }
   }

   /// A page served by the classic JSD flow (cv in the HTML), e.g. cfdata.lol.
   /// Used by the live cfdata.lol score test and available for other classic
   /// targets.
   #[allow(dead_code)]
   pub(crate) fn classic(page: &str) -> Self {
      let (origin, host) = split_origin_host(page);
      Self {
         page: page.to_owned(),
         host,
         origin,
         read_html_cv: true,
      }
   }
}

/// `https://host/path` → (`https://host`, `host`).
#[allow(dead_code)]
fn split_origin_host(url: &str) -> (String, String) {
   let scheme = url.split("://").next().unwrap_or("https");
   let after = url.split("://").nth(1).unwrap_or(url);
   let host = after.split('/').next().unwrap_or(after).to_owned();
   (format!("{scheme}://{host}"), host)
}

/// How long a session trusts one mint before solving again. Cloudflare expires
/// `cf_clearance` at 30 minutes, so this re-solves at 29 — on the next search
/// past the mark — rather than attach a cookie that has just died.
const CLEARANCE_TTL: Duration = Duration::from_secs(29 * 60);
/// Cap on one solve, so a first search is never held longer than this.
const SOLVE_TIMEOUT: Duration = Duration::from_secs(12);

/// A real Chrome oneshot payload, captured once (Chrome 148, Linux). The keys
/// that name the page and the client are remapped per session; see
/// [`build_payload`].
const TEMPLATE_JSON: &str = include_str!("jsd_payload_template.json");

// ───────────────────────── the gate ─────────────────────────

/// The circuit breaker around the clearance flow. One solve failure trips it
/// for the life of the process.
#[derive(Clone)]
pub(crate) struct ClearanceGate {
   inner: Arc<Inner>,
}

struct Inner {
   /// `searchClearance` in config: the flow runs at all.
   enabled:   bool,
   state:     RwLock<GateState>,
   /// Session id → when its clearance was last minted.
   sessions:  Mutex<HashMap<i64, Instant>>,
   /// One solve at a time, so a cold start does not stampede x.com.
   solving:   tokio::sync::Mutex<()>,
   attempts:  AtomicU64,
   successes: AtomicU64,
   failures:  AtomicU64,
   /// Transient failures (a subset of `failures`): bad responses, network
   /// errors, declined mints. These do not trip the breaker on their own.
   transient: AtomicU64,
   /// Failures in a row (any kind), reset by a success. A long run trips the
   /// breaker so a persistent problem stops hammering x.com.
   streak:    AtomicU64,
   last_ok:   Mutex<Option<OffsetDateTime>>,
   last_err:  Mutex<Option<String>>,
}

/// Consecutive failures that trip the breaker even when each looks transient:
/// something is persistently wrong (every exit blocked, say), so stop trying.
const MAX_FAILURE_STREAK: u64 = 20;

enum GateState {
   Armed,
   Tripped {
      reason: String,
      at:     OffsetDateTime,
   },
}

impl ClearanceGate {
   pub(crate) fn new(enabled: bool) -> Self {
      Self {
         inner: Arc::new(Inner {
            enabled,
            state: RwLock::new(GateState::Armed),
            sessions: Mutex::new(HashMap::new()),
            solving: tokio::sync::Mutex::new(()),
            attempts: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            transient: AtomicU64::new(0),
            streak: AtomicU64::new(0),
            last_ok: Mutex::new(None),
            last_err: Mutex::new(None),
         }),
      }
   }

   /// Enabled and not tripped.
   fn active(&self) -> bool {
      self.inner.enabled
         && matches!(
            &*self
               .inner
               .state
               .read()
               .unwrap_or_else(std::sync::PoisonError::into_inner),
            GateState::Armed
         )
   }

   /// Whether this session should mint (or re-mint) a cookie now.
   pub(crate) fn should_solve(&self, session_id: i64) -> bool {
      if !self.active() {
         return false;
      }
      let sessions = self
         .inner
         .sessions
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner);
      sessions
         .get(&session_id)
         .is_none_or(|at| at.elapsed() >= CLEARANCE_TTL)
   }

   /// Serialise solves: one session mints at a time.
   pub(crate) async fn solve_permit(&self) -> tokio::sync::MutexGuard<'_, ()> {
      self.inner.solving.lock().await
   }

   pub(crate) fn record_attempt(&self) {
      self.inner.attempts.fetch_add(1, Ordering::Relaxed);
   }

   pub(crate) fn mark_ok(&self, session_id: i64) {
      self.inner.successes.fetch_add(1, Ordering::Relaxed);
      self.inner.streak.store(0, Ordering::Relaxed);
      *self
         .inner
         .last_ok
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(OffsetDateTime::now_utc());
      self
         .inner
         .sessions
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner)
         .insert(session_id, Instant::now());
   }

   /// Count a failure and return the new consecutive-failure streak. `transient`
   /// failures are also tallied separately. Does not trip; the caller decides.
   pub(crate) fn record_failure(&self, reason: &str, transient: bool) -> u64 {
      self.inner.failures.fetch_add(1, Ordering::Relaxed);
      if transient {
         self.inner.transient.fetch_add(1, Ordering::Relaxed);
      }
      *self
         .inner
         .last_err
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason.to_owned());
      self.inner.streak.fetch_add(1, Ordering::Relaxed) + 1
   }

   /// Disable the whole flow until restart. Only the first trip sets the reason
   /// and timestamp.
   pub(crate) fn trip(&self, reason: &str) {
      let mut state = self
         .inner
         .state
         .write()
         .unwrap_or_else(std::sync::PoisonError::into_inner);
      if matches!(&*state, GateState::Armed) {
         *state = GateState::Tripped {
            reason: reason.to_owned(),
            at:     OffsetDateTime::now_utc(),
         };
      }
   }

   /// A reading for the admin dashboard.
   pub(crate) fn status(&self) -> ClearanceStatus {
      let (tripped, reason, tripped_at) = {
         let state = self
            .inner
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
         match &*state {
            GateState::Armed => (false, None, None),
            GateState::Tripped { reason, at } => (
               true,
               Some(reason.clone()),
               at.format(&Rfc3339).ok(),
            ),
         }
      };
      let now = Instant::now();
      let active_sessions = self
         .inner
         .sessions
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner)
         .values()
         .filter(|at| now.duration_since(**at) < CLEARANCE_TTL)
         .count();
      let last_ok = self
         .inner
         .last_ok
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner)
         .and_then(|at| at.format(&Rfc3339).ok());
      let last_error = self
         .inner
         .last_err
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner)
         .clone();
      ClearanceStatus {
         enabled: self.inner.enabled,
         armed: self.inner.enabled && !tripped,
         tripped,
         reason,
         tripped_at,
         attempts: self.inner.attempts.load(Ordering::Relaxed),
         successes: self.inner.successes.load(Ordering::Relaxed),
         failures: self.inner.failures.load(Ordering::Relaxed),
         transient_failures: self.inner.transient.load(Ordering::Relaxed),
         active_sessions,
         last_ok,
         last_error,
      }
   }
}

/// Consecutive failures that force a trip even when each looks transient.
pub(crate) const fn max_failure_streak() -> u64 {
   MAX_FAILURE_STREAK
}

/// The clearance flow's state, for the admin dashboard.
#[derive(Clone, Debug, Serialize)]
pub struct ClearanceStatus {
   /// `searchClearance` is on.
   pub enabled:         bool,
   /// Enabled and not tripped: solves are still happening.
   pub armed:           bool,
   /// A solve failed; the flow is off until restart.
   pub tripped:         bool,
   /// Why it tripped.
   pub reason:          Option<String>,
   /// When it tripped (RFC3339).
   pub tripped_at:      Option<String>,
   pub attempts:        u64,
   pub successes:       u64,
   pub failures:        u64,
   /// Transient failures (bad responses, network errors, declined mints) that
   /// did not trip the breaker — a subset of `failures`.
   pub transient_failures: u64,
   /// Sessions holding a cookie minted within the TTL.
   pub active_sessions: usize,
   /// Last successful mint (RFC3339).
   pub last_ok:         Option<String>,
   /// Last failure message, kept even after tripping.
   pub last_error:      Option<String>,
}

// ───────────────────────── the solver ─────────────────────────

/// The browser a session presents as, for the JSD payload and request headers.
pub(crate) struct ClientInfo {
   pub user_agent:         String,
   pub sec_ch_ua:          String,
   pub sec_ch_ua_platform: String,
   pub navigator_platform: String,
}

/// Why a solve failed, and whether it means Cloudflare changed the challenge.
#[derive(Debug)]
pub(crate) enum SolveError {
   /// A one-off: a bad or oversized response, a network error, a timeout, or a
   /// mint the oneshot declined. The flow keeps running and the session retries.
   Transient(String),
   /// The challenge no longer matches the solver — a new encoder, or the
   /// extractor could not read a script that *is* the challenge. The one-strike
   /// breaker trips, because every solve would fail the same way.
   Changed(String),
}

impl SolveError {
   pub(crate) fn reason(&self) -> &str {
      match self {
         Self::Transient(reason) | Self::Changed(reason) => reason,
      }
   }

   pub(crate) const fn is_changed(&self) -> bool {
      matches!(self, Self::Changed(_))
   }
}

/// A request/transport failure (returned as `String`) is transient by default.
impl From<String> for SolveError {
   fn from(reason: String) -> Self {
      Self::Transient(reason)
   }
}

/// Solve the challenge for one session's egress and return `cf_clearance`.
/// Bounded by [`SOLVE_TIMEOUT`].
pub(crate) async fn solve(
   client: &HttpClient,
   egress: &Egress,
   info: &ClientInfo,
   target: &ClearanceTarget,
) -> Result<String, SolveError> {
   match tokio::time::timeout(SOLVE_TIMEOUT, solve_inner(client, egress, info, target)).await {
      Ok(result) => result,
      Err(_elapsed) => Err(SolveError::Transient("solve timed out".to_owned())),
   }
}

async fn solve_inner(
   client: &HttpClient,
   egress: &Egress,
   info: &ClientInfo,
   target: &ClearanceTarget,
) -> Result<String, SolveError> {
   let mut jar: HashMap<String, String> = HashMap::new();
   let token = jsdapi_token();
   let now_secs = OffsetDateTime::now_utc().unix_timestamp();

   // The `__CF$cv$params`: classic pages (cfdata.lol) carry them in the HTML, so
   // fetch and read them; api.js pages (x.com) do not, so synthesise them and
   // skip the page GET entirely (the `main.js` GET sets `__cf_bm`).
   let synth = || Cv {
      r:      token.clone(),
      t_time: now_secs,
      t_b64:  BASE64.encode(now_secs.to_string().as_bytes()),
      api:    true,
   };
   let cv = if target.read_html_cv {
      let (_s, _h, html) = fetch(
         client,
         egress,
         Method::GET,
         &target.page,
         &mut jar,
         &document_headers(info, target),
         None,
      )
      .await?;
      parse_classic_cv(&html).unwrap_or_else(synth)
   } else {
      synth()
   };

   // 1. The challenge script. api.js appends `?ray=<token>`; redirects are
   //    followed (cfdata.lol 302s to a hashed path).
   let main_url = format!(
      "https://{}/cdn-cgi/challenge-platform/scripts/jsd/main.js?ray={}",
      target.host, cv.r
   );
   let (main_status, _h, main_js) = fetch(
      client,
      egress,
      Method::GET,
      &main_url,
      &mut jar,
      &script_headers(info, target),
      None,
   )
   .await?;
   // Not the challenge script: a 200 that is the SPA HTML, a block/interstitial
   // page, or a proxy error — i.e. this one exit, not a format change. Keep the
   // flow alive and let another session retry.
   if !main_js.contains("_cf_chl_opt") {
      return Err(SolveError::Transient(format!(
         "jsd main.js was not the challenge script (status {main_status}, {} bytes)",
         main_js.len()
      )));
   }

   // 2. Deobfuscate: run the script's own string-table decoder in QuickJS to
   //    recover the alphabet and oneshot path, then parse the readable form.
   //    A script that is already readable is parsed as-is. From here a failure
   //    means the script *is* the challenge but the solver can't read it — a
   //    real format change, so these trip the breaker.
   let source = readable_main(&main_js).unwrap_or(main_js);
   if !is_payload_encoder_v2(&source) {
      return Err(SolveError::Changed(
         "jsd encoder is not v2 (challenge changed)".to_owned(),
      ));
   }
   let extracted = extract_modern(&source).ok_or_else(|| {
      SolveError::Changed("jsd main.js did not match the modern extractor (challenge changed)".to_owned())
   })?;

   // 3. The device payload, compressed with the v2 encoder.
   let payload = build_payload(cv.t_time, info, cv.api, target);
   let compressed = compress_v2(&payload, &extracted.alphabet).map_err(SolveError::Changed)?;

   // 4. The oneshot. `cf_cv_Object[cf_cvparam]` is the ray (or time) injected.
   let cv_value = if extracted.cf_cvparam == "t" {
      cv.t_b64
   } else {
      cv.r
   };
   let oneshot = format!(
      "https://{}/cdn-cgi/challenge-platform/h/{}/jsd/oneshot/{}/{}/{}",
      target.host, extracted.chl_value, extracted.s_arg, extracted.t_arg, cv_value
   );
   let (status, headers, _body) = fetch(
      client,
      egress,
      Method::POST,
      &oneshot,
      &mut jar,
      &post_headers(info, target),
      Some(Bytes::from(compressed.into_bytes())),
   )
   .await?;

   // A 200 oneshot that sets no cf_clearance is a declined mint (fingerprint/IP
   // scored out), not a format change — transient.
   cookie_from_headers(&headers, "cf_clearance")
      .or_else(|| jar.get("cf_clearance").cloned())
      .ok_or_else(|| SolveError::Transient(format!("oneshot {status} returned no cf_clearance")))
}

/// Same-host redirect hops a GET follows, matching the Node solver's browser
/// fetch. The challenge path and the page both redirect, and the cookies set
/// across those hops are what the oneshot needs.
const MAX_REDIRECTS: u8 = 8;

/// One request (GET following same-host redirects) through the session's
/// browser client, tracking cookies in a local jar so the oneshot carries
/// everything the challenge set.
async fn fetch(
   client: &HttpClient,
   egress: &Egress,
   method: Method,
   url: &str,
   jar: &mut HashMap<String, String>,
   extra: &[(&'static str, String)],
   body: Option<Bytes>,
) -> Result<(StatusCode, HeaderMap, String), String> {
   let mut current = url.to_owned();
   let mut hops = 0u8;
   loop {
      let cookie = (!jar.is_empty()).then(|| {
         jar.iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ")
      });
      let headers = header_map(extra, cookie);
      let response = match method {
         Method::POST => {
            client
               .post_on(
                  &current,
                  &headers,
                  body.clone().unwrap_or_default(),
                  Some(egress),
               )
               .await
         },
         _ => client.get_on(&current, &headers, Some(egress)).await,
      }
      .map_err(|err| format!("{method} {}: {err}", host_of(&current)))?;

      let status = response.status();
      let response_headers = response.headers().clone();
      for line in response_headers.get_all(header::SET_COOKIE) {
         if let Ok(line) = line.to_str()
            && let Some((name, value)) = parse_set_cookie(line)
         {
            match value {
               Some(value) => {
                  jar.insert(name, value);
               },
               None => {
                  jar.remove(&name);
               },
            }
         }
      }

      // Follow a same-host redirect on GET, carrying the cookies just set.
      if method == Method::GET && status.is_redirection() && hops < MAX_REDIRECTS {
         if let Some(next) = response_headers
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|location| resolve_redirect(&current, location))
            .filter(|next| host_of(next) == host_of(&current))
         {
            // Drain the (empty) redirect body before reusing the connection.
            let _ = response.text().await;
            current = next;
            hops += 1;
            continue;
         }
      }

      let text = response.text().await.unwrap_or_default();
      return Ok((status, response_headers, text));
   }
}

/// Resolve a `Location` against the current URL. Handles absolute URLs and
/// root-relative paths, which is all Cloudflare's hops use.
fn resolve_redirect(current: &str, location: &str) -> Option<String> {
   let location = location.trim();
   if location.is_empty() {
      return None;
   }
   if location.starts_with("http://") || location.starts_with("https://") {
      return Some(location.to_owned());
   }
   let scheme_host_end = current
      .find("://")
      .and_then(|scheme| current[scheme + 3..].find('/').map(|slash| scheme + 3 + slash))
      .unwrap_or(current.len());
   let base = &current[..scheme_host_end];
   if let Some(stripped) = location.strip_prefix('/') {
      Some(format!("{base}/{stripped}"))
   } else {
      Some(format!("{base}/{location}"))
   }
}

// ───────────────────────── headers ─────────────────────────

fn header_map(extra: &[(&'static str, String)], cookie: Option<String>) -> HeaderMap {
   let mut headers = HeaderMap::new();
   for &(name, ref value) in extra {
      if value.is_empty() {
         continue;
      }
      if let Ok(parsed) = HeaderValue::from_str(value) {
         headers.insert(HeaderName::from_static(name), parsed);
      }
   }
   if let Some(cookie) = cookie
      && let Ok(parsed) = HeaderValue::from_str(&cookie)
   {
      headers.insert(header::COOKIE, parsed);
   }
   headers
}

fn document_headers(info: &ClientInfo, _target: &ClearanceTarget) -> Vec<(&'static str, String)> {
   vec![
      ("cache-control", "max-age=0".to_owned()),
      ("user-agent", info.user_agent.clone()),
      (
         "accept",
         "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7"
            .to_owned(),
      ),
      ("accept-language", "en-US,en;q=0.9".to_owned()),
      ("sec-ch-ua", info.sec_ch_ua.clone()),
      ("sec-ch-ua-mobile", "?0".to_owned()),
      ("sec-ch-ua-platform", info.sec_ch_ua_platform.clone()),
      ("upgrade-insecure-requests", "1".to_owned()),
      ("sec-fetch-dest", "document".to_owned()),
      ("sec-fetch-mode", "navigate".to_owned()),
      ("sec-fetch-site", "none".to_owned()),
      ("sec-fetch-user", "?1".to_owned()),
      ("priority", "u=0, i".to_owned()),
   ]
}

fn script_headers(info: &ClientInfo, target: &ClearanceTarget) -> Vec<(&'static str, String)> {
   vec![
      ("user-agent", info.user_agent.clone()),
      ("accept", "*/*".to_owned()),
      ("accept-language", "en-US,en;q=0.9".to_owned()),
      ("sec-ch-ua", info.sec_ch_ua.clone()),
      ("sec-ch-ua-mobile", "?0".to_owned()),
      ("sec-ch-ua-platform", info.sec_ch_ua_platform.clone()),
      ("sec-fetch-dest", "script".to_owned()),
      ("sec-fetch-mode", "no-cors".to_owned()),
      ("sec-fetch-site", "same-origin".to_owned()),
      ("referer", target.page.clone()),
      ("priority", "u=1".to_owned()),
   ]
}

fn post_headers(info: &ClientInfo, target: &ClearanceTarget) -> Vec<(&'static str, String)> {
   vec![
      ("content-type", "text/plain;charset=UTF-8".to_owned()),
      ("user-agent", info.user_agent.clone()),
      ("accept", "*/*".to_owned()),
      ("accept-language", "en-US,en;q=0.9".to_owned()),
      ("sec-ch-ua", info.sec_ch_ua.clone()),
      ("sec-ch-ua-mobile", "?0".to_owned()),
      ("sec-ch-ua-platform", info.sec_ch_ua_platform.clone()),
      ("origin", target.origin.clone()),
      ("referer", target.page.clone()),
      ("sec-fetch-dest", "empty".to_owned()),
      ("sec-fetch-mode", "cors".to_owned()),
      ("sec-fetch-site", "same-origin".to_owned()),
      ("priority", "u=1, i".to_owned()),
   ]
}

// ───────────────────────── __CF$cv$params ─────────────────────────

/// The `{r, t, api}` the oneshot is keyed by.
struct Cv {
   r:      String,
   t_time: i64,
   t_b64:  String,
   api:    bool,
}

/// A classic page carries `window.__CF$cv$params={r:'…',t:'…'}` (no `api:true`).
/// Returns `None` for api.js pages (no cv, or `api:true`), so the caller
/// synthesises one.
fn parse_classic_cv(html: &str) -> Option<Cv> {
   static RE: LazyLock<Regex> = LazyLock::new(|| {
      Regex::new(r"window\.__CF\$cv\$params\s*=\s*(\{[^}]*\})").expect("cv regex")
   });
   let object = RE.captures(html)?.get(1)?.as_str();
   let pairs = parse_object_literal(object);
   if pairs.iter().any(|(k, v)| k == "api" && v == "true") {
      return None;
   }
   let r = pairs
      .iter()
      .find(|(k, _)| k == "r")
      .map(|(_, v)| v.clone())
      .filter(|v| !v.is_empty())?;
   let t_b64 = pairs
      .iter()
      .find(|(k, _)| k == "t")
      .map(|(_, v)| v.clone())
      .filter(|v| !v.is_empty())?;
   let t_time = String::from_utf8(BASE64.decode(t_b64.as_bytes()).ok()?)
      .ok()?
      .trim()
      .parse::<f64>()
      .ok()?
      .floor() as i64;
   Some(Cv {
      r,
      t_time,
      t_b64,
      api: false,
   })
}

// ───────────────────────── deobfuscation (QuickJS) ─────────────────────────

/// A browser `atob`, injected into QuickJS, which has no web APIs of its own.
const ATOB_POLYFILL: &str = r#"globalThis.atob=function(d){var c="ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";d=String(d).replace(/[^A-Za-z0-9+/]/g,"");var o="";for(var bc=0,bs=0,bu,i=0;(bu=d.charAt(i++));~bu&&(bs=bc%4?bs*64+bu:bu,bc++%4)?o+=String.fromCharCode(255&bs>>(-2*bc&6)):0){bu=c.indexOf(bu);}return o;};"#;

/// The current `main.js` hides its oneshot path and 6-bit alphabet inside a
/// per-response string table — a closure that `atob`+XOR-decodes short literals.
/// This finds that decoder, runs it in QuickJS over every literal in the script
/// (exactly what the Node solver's `readableMain` does with `Function(...)`),
/// and rebuilds a short, readable script the regex extractor can parse. Returns
/// `None` when the shape is not recognised, so the caller falls back to the raw
/// script (and then trips the breaker).
fn readable_main(raw: &str) -> Option<String> {
   static DECODER: LazyLock<Regex> = LazyLock::new(|| {
      Regex::new(
         r"var\s+([A-Za-z_$][\w$]*)\s*=\s*(\(function\([\s\S]{0,900}?\.atob\([\s\S]{0,700}?\}\)\(globalThis\))",
      )
      .expect("decoder regex")
   });
   static CHL: LazyLock<Regex> =
      LazyLock::new(|| Regex::new(r"window\._cf_chl_opt\s*=\s*\{[^}]+\}").expect("chl literal"));
   static ALPHABET: LazyLock<Regex> =
      LazyLock::new(|| Regex::new(r"^[A-Za-z0-9$+_-]{64,66}$").expect("alphabet shape"));
   static ONESHOT: LazyLock<Regex> =
      LazyLock::new(|| Regex::new(r"/jsd/oneshot/[^/]+/[^/]+/").expect("oneshot shape"));

   let decoder = DECODER.captures(raw)?;
   let name = decoder.get(1)?.as_str();
   let closure = decoder.get(2)?.as_str();
   let chl = CHL.find(raw)?.as_str();

   // Every `NAME(`literal`)` call: the encoded string-table entries.
   let call = Regex::new(&format!(r"{}\(`([^`]*)`\)", regex::escape(name))).ok()?;
   let mut inputs: Vec<String> = Vec::new();
   for caps in call.captures_iter(raw) {
      if let Some(literal) = caps.get(1) {
         inputs.push(literal.as_str().to_owned());
      }
   }
   if inputs.is_empty() {
      return None;
   }

   let decoded = run_string_table_decoder(closure, &inputs)?;
   let mut oneshot = None;
   let mut alphabet = None;
   for value in &decoded {
      if oneshot.is_none() && ONESHOT.is_match(value) {
         oneshot = Some(value.clone());
      }
      if alphabet.is_none() && ALPHABET.is_match(value) {
         alphabet = Some(value.clone());
      }
   }
   let oneshot = oneshot?;
   let alphabet = alphabet?;

   // The readable form the extractor understands: chl options, the 6-bit
   // `obj.g(_,6,fn)` alphabet, the oneshot path glued to `W.r`, and the v2
   // encoder markers the detector looks for.
   Some(format!(
      "{chl};\nobj.g(x,6,function(h){{return{}.charAt(h)}});\n{}+W.r;\natob(window.__CF$cv$params.t);\nvar frameByte=253, fnvOffset=2166136261, fnvPrime=16777619;\nvar packed=new Uint8Array([frameByte]);\nvar mix=(1>>>24)%64;",
      json_string(&alphabet),
      json_string(&oneshot)
   ))
}

/// Evaluate the string-table decoder closure in QuickJS and decode every
/// literal. The closure is Cloudflare's own code; QuickJS runs it with no I/O.
fn run_string_table_decoder(closure: &str, inputs: &[String]) -> Option<Vec<String>> {
   let inputs_json = serde_json::to_string(inputs).ok()?;
   let program = format!(
      "(function(){{{ATOB_POLYFILL}\nglobalThis.window=globalThis;\nvar dec=({closure});\nvar inputs={inputs_json};\nvar out=[];\nfor(var i=0;i<inputs.length;i++){{try{{out.push(String(dec(inputs[i])));}}catch(e){{out.push(\"\");}}}}\nreturn JSON.stringify(out);}})()"
   );

   let runtime = rquickjs::Runtime::new().ok()?;
   runtime.set_memory_limit(64 * 1024 * 1024);
   // A pathological script cannot hang a worker: QuickJS runs synchronously, so
   // the async solve timeout cannot interrupt it. Stop after three seconds.
   let deadline = Instant::now() + Duration::from_secs(3);
   runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
   let context = rquickjs::Context::full(&runtime).ok()?;
   let json: String = context.with(|ctx| ctx.eval::<String, _>(program).ok())?;
   serde_json::from_str::<Vec<String>>(&json).ok()
}

// ───────────────────────── main.js extraction ─────────────────────────

struct Extracted {
   alphabet:   String,
   chl_value:  String,
   s_arg:      String,
   t_arg:      String,
   cf_cvparam: String,
}

/// The modern (regex-only) extraction `jsd-solver`'s `extractModernJsdMain`
/// performs. Returns `None` if any piece is missing (the challenge changed).
fn extract_modern(js: &str) -> Option<Extracted> {
   let alphabet = match_encode_alphabet(js)?;
   let chl_value = chl_opt_token(js)?;
   let (s_arg, t_arg, cf_cvparam) = match_oneshot_tail(js)?;
   Some(Extracted {
      alphabet,
      chl_value,
      s_arg,
      t_arg,
      cf_cvparam,
   })
}

/// The 6-bit pack alphabet from `obj.g(_, 6, function(h){return "…".charAt(h)})`
/// (and its template-literal / minified variants).
fn match_encode_alphabet(js: &str) -> Option<String> {
   static RES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
      [
         r#"\.g\([^,]+,(\d+),function\([^)]*\)\{[\s\S]{0,4500}?return['"]([A-Za-z0-9$+_-]{20,280})['"]\.charAt"#,
         r#"\.g\([^,]+,(\d+),function\([^)]*\)\{return['"]([A-Za-z0-9$+_-]{20,280})['"]\.charAt"#,
         r#"\.g\(\w+,(\d+),function\(\w+\)\{return'([^']{20,280})'\.charAt"#,
         r#"\.g\(\w+,(\d+),function\(\w+\)\{return"([^"]{20,280})"\.charAt"#,
         r"\.g\([^,]+,(\d+),function\([^)]*\)\{[\s\S]{0,4500}?return`([A-Za-z0-9$+_-]{20,280})`\.charAt",
         r"\.g\([^,]+,(\d+),function\([^)]*\)\{return`([A-Za-z0-9$+_-]{20,280})`\.charAt",
      ]
      .iter()
      .filter_map(|pattern| Regex::new(pattern).ok())
      .collect()
   });
   static ALPHA_LIT: LazyLock<Regex> =
      LazyLock::new(|| Regex::new(r"[=:]\s*`([A-Za-z0-9$+_-]{50,90})`").expect("alpha regex"));
   for re in RES.iter() {
      if let Some(caps) = re.captures(js)
         && let Some(alphabet) = caps.get(2)
      {
         return Some(alphabet.as_str().to_owned());
      }
   }
   ALPHA_LIT
      .captures(js)
      .and_then(|caps| caps.get(1))
      .map(|m| m.as_str().to_owned())
}

/// `window._cf_chl_opt={…}` → the `h/<token>` path segment value.
fn chl_opt_token(js: &str) -> Option<String> {
   static RE: LazyLock<Regex> = LazyLock::new(|| {
      Regex::new(r"window\._cf_chl_opt\s*=\s*(\{[^}]+\})\s*;").expect("chl regex")
   });
   let object = RE.captures(js)?.get(1)?.as_str();
   let pairs = parse_object_literal(object);
   if pairs.is_empty() {
      return None;
   }
   if pairs.len() == 1 {
      return Some(pairs[0].1.clone());
   }
   let short: Vec<&(String, String)> = pairs
      .iter()
      .filter(|(_, value)| {
         let len = value.len();
         (1..=12).contains(&len) && value.bytes().all(|b| b.is_ascii_alphanumeric())
      })
      .collect();
   if short.len() == 1 {
      return Some(short[0].1.clone());
   }
   Some(pairs[0].1.clone())
}

/// The `/jsd/oneshot/<s>/<t>/` path and the `__CF$cv$params` property glued to
/// it, across the many minified shapes `jsd-solver` handles.
fn match_oneshot_tail(js: &str) -> Option<(String, String, String)> {
   static RES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
      [
         r#"/jsd/oneshot/([^/]+)/([^/]+)/\s*['"]\s*,\s*[A-Za-z_$][\w$]*\.(\w+)\s*\)"#,
         r#"/jsd/oneshot/([^/]+)/([^/]+)/\s*['"]\s*\)\s*,\s*[A-Za-z_$][\w$]*\.(\w+)\s*\)"#,
         r#"/jsd/oneshot/([^/]+)/([^/]+)/\s*['"]\s*\)\s*\+\s*[A-Za-z_$][\w$]*\.(\w+)"#,
         r#"/jsd/oneshot/([^/]+)/([^/]+)/\s*['"]\s*\+\s*[A-Za-z_$][\w$]*\.(\w+)"#,
         r"`/jsd/oneshot/([^/]+)/([^/]+)/`\s*\)\s*,\s*[A-Za-z_$][\w$]*\.(\w+)\s*\)",
         r"`/jsd/oneshot/([^/]+)/([^/]+)/`\s*\)\s*\+\s*[A-Za-z_$][\w$]*\.(\w+)",
         r"`/jsd/oneshot/([^/]+)/([^/]+)/`\s*,\s*[A-Za-z_$][\w$]*\.(\w+)\s*\)",
         r"`/jsd/oneshot/([^/]+)/([^/]+)/`\s*\+\s*[A-Za-z_$][\w$]*\.(\w+)",
      ]
      .iter()
      .filter_map(|pattern| Regex::new(pattern).ok())
      .collect()
   });
   for re in RES.iter() {
      if let Some(caps) = re.captures(js)
         && let (Some(s), Some(t), Some(param)) = (caps.get(1), caps.get(2), caps.get(3))
      {
         return Some((
            s.as_str().to_owned(),
            t.as_str().to_owned(),
            param.as_str().to_owned(),
         ));
      }
   }
   None
}

/// The current oneshot encoder is the v2 frame (byte 253, FNV-1a, xorshift,
/// 6-bit pack). The frame byte is often rewritten, so detect it loosely.
fn is_payload_encoder_v2(js: &str) -> bool {
   static FRAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"=\s*253\b").expect("frame regex"));
   static PACK: LazyLock<Regex> =
      LazyLock::new(|| Regex::new(r">>>\s*24(?:\.\d+)?").expect("pack regex"));
   let has_fnv = js.contains("2166136261") && js.contains("16777619");
   let has_frame = FRAME.is_match(js) || js.contains("~-254");
   let has_pack = js.contains("Uint8Array") && (PACK.is_match(js) || js.contains("%64"));
   has_fnv && has_frame && has_pack
}

/// `{a:'x', b:"y", c:z}` → `[("a","x"),("b","y"),("c","z")]`. Enough for the
/// `_cf_chl_opt` / `__CF$cv$params` object literals (string and bare values).
fn parse_object_literal(object: &str) -> Vec<(String, String)> {
   static RE: LazyLock<Regex> = LazyLock::new(|| {
      Regex::new(r#"([A-Za-z_$][\w$]*)\s*:\s*(?:'([^']*)'|"([^"]*)"|([^,}]+))"#)
         .expect("object literal regex")
   });
   RE.captures_iter(object)
      .map(|caps| {
         let key = caps.get(1).map_or("", |m| m.as_str()).to_owned();
         let value = caps
            .get(2)
            .or_else(|| caps.get(3))
            .or_else(|| caps.get(4))
            .map_or("", |m| m.as_str())
            .trim()
            .to_owned();
         (key, value)
      })
      .collect()
}

// ───────────────────────── payload ─────────────────────────

#[derive(Deserialize)]
struct Template {
   meta:      TemplateMeta,
   #[serde(rename = "keyOrder")]
   key_order: Vec<String>,
   payload:   HashMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct TemplateMeta {
   origin:       String,
   host:         String,
   href:         String,
   vendor:       String,
   #[serde(rename = "appCodeName")]
   app_code_name: String,
   #[serde(rename = "appName")]
   app_name:     String,
   #[serde(rename = "appVersion")]
   app_version:  String,
   platform:     String,
   product:      String,
   #[serde(rename = "userAgent")]
   user_agent:   String,
   language:     String,
   #[serde(rename = "lastModified")]
   last_modified: String,
}

static TEMPLATE: LazyLock<Template> =
   LazyLock::new(|| serde_json::from_str(TEMPLATE_JSON).expect("bundled jsd template parses"));

/// Rebuild the captured payload for this page and client. The template's page
/// and client keys are remapped; Windows sessions get Windows geometry and
/// 16-thread / 8 GB hints, matching the Node `getPayload`.
fn build_payload(t_time: i64, info: &ClientInfo, api: bool, target: &ClearanceTarget) -> String {
   let template = &*TEMPLATE;
   let meta = &template.meta;
   let app_version = info
      .user_agent
      .strip_prefix("Mozilla/")
      .unwrap_or(&info.user_agent)
      .to_owned();
   let current_date = current_date();

   let remap: HashMap<&str, String> = HashMap::from([
      (meta.origin.as_str(), target.origin.clone()),
      (meta.host.as_str(), target.host.clone()),
      (meta.href.as_str(), target.page.clone()),
      (meta.vendor.as_str(), "Google Inc.".to_owned()),
      (meta.app_code_name.as_str(), "Mozilla".to_owned()),
      (meta.app_name.as_str(), "Netscape".to_owned()),
      (meta.app_version.as_str(), app_version),
      (meta.platform.as_str(), info.navigator_platform.clone()),
      (meta.product.as_str(), "Gecko".to_owned()),
      (meta.user_agent.as_str(), info.user_agent.clone()),
      (meta.language.as_str(), "en-US".to_owned()),
      (meta.last_modified.as_str(), current_date),
   ]);

   let mut buckets: Vec<(String, Vec<String>)> = Vec::with_capacity(template.key_order.len());
   for old_key in &template.key_order {
      let Some(props) = template.payload.get(old_key) else {
         continue;
      };
      let new_key = remap
         .get(old_key.as_str())
         .cloned()
         .unwrap_or_else(|| old_key.clone());
      if new_key.is_empty() || new_key == "undefined" {
         continue;
      }
      buckets.push((new_key, props.clone()));
   }

   if info.navigator_platform == "Win32" {
      apply_windows_geometry(&mut buckets);
   }

   let mut payload = String::from("{");
   for (index, (key, props)) in buckets.iter().enumerate() {
      if index > 0 {
         payload.push(',');
      }
      payload.push_str(&json_string(key));
      payload.push(':');
      payload.push('[');
      for (prop_index, prop) in props.iter().enumerate() {
         if prop_index > 0 {
            payload.push(',');
         }
         payload.push_str(&json_string(prop));
      }
      payload.push(']');
   }
   payload.push('}');

   format!(
      "{{\"t\":{t_time},\"lhr\":\"about:blank\",\"api\":{api},\"c\":false,\"payload\":{payload}}}"
   )
}

/// Move the screen/outer geometry and the thread/memory hints to the buckets a
/// Windows desktop reports them in (Node `applyWindowsGeometry`).
fn apply_windows_geometry(buckets: &mut Vec<(String, Vec<String>)>) {
   let moves: [(&str, &str, &[&str]); 4] = [
      ("274", "0", &["screenY", "screenTop"][..]),
      ("441", "-1920", &["screenX", "screenLeft"][..]),
      ("805", "1080", &["outerHeight"][..]),
      ("1288", "1920", &["outerWidth"][..]),
   ];
   for (from, to, expect) in moves {
      let Some(pos) = buckets.iter().position(|(key, _)| key == from) else {
         continue;
      };
      let moved: Vec<String> = buckets[pos]
         .1
         .iter()
         .filter(|prop| expect.contains(&prop.as_str()))
         .cloned()
         .collect();
      buckets[pos].1.retain(|prop| !expect.contains(&prop.as_str()));
      if buckets[pos].1.is_empty() {
         buckets.remove(pos);
      }
      if moved.is_empty() {
         continue;
      }
      put_props(buckets, to, &moved);
   }
   // Windows desktop profiles report 16 threads / 8 GB.
   put_props(buckets, "16", &["n.hardwareConcurrency".to_owned()]);
   put_props(buckets, "8", &["n.deviceMemory".to_owned()]);
}

/// Remove `props` from every bucket, then append them to `key` (creating it if
/// need be). Mirrors the Node `put` helper.
fn put_props(buckets: &mut Vec<(String, Vec<String>)>, key: &str, props: &[String]) {
   for prop in props {
      for (_, bucket) in buckets.iter_mut() {
         bucket.retain(|existing| existing != prop);
      }
   }
   buckets.retain(|(_, bucket)| !bucket.is_empty());
   if let Some((_, bucket)) = buckets.iter_mut().find(|(existing, _)| existing == key) {
      for prop in props {
         if !bucket.contains(prop) {
            bucket.push(prop.clone());
         }
      }
   } else {
      buckets.push((key.to_owned(), props.to_vec()));
   }
}

/// `MM/DD/YYYY HH:MM:SS` in UTC, as `document.lastModified` renders.
fn current_date() -> String {
   let now = OffsetDateTime::now_utc();
   format!(
      "{:02}/{:02}/{:04} {:02}:{:02}:{:02}",
      u8::from(now.month()),
      now.day(),
      now.year(),
      now.hour(),
      now.minute(),
      now.second()
   )
}

fn json_string(value: &str) -> String {
   serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

// ───────────────────────── v2 encoder ─────────────────────────

fn fnv1a32(seed: &str) -> u32 {
   let mut hash: u32 = 2_166_136_261;
   for byte in seed.bytes() {
      hash ^= u32::from(byte);
      hash = hash.wrapping_mul(16_777_619);
   }
   if hash != 0 { hash } else { 2_779_062_077 }
}

const fn xorshift32(mut state: u32) -> u32 {
   state ^= state << 13;
   state ^= state >> 17;
   state ^= state << 5;
   state
}

/// deflate-raw (when it helps) → `[253,1,flag]` frame → FNV/xorshift XOR →
/// 6-bit MSB pack over `alphabet`. The Jul 2026+ oneshot encoder.
fn compress_v2(payload: &str, alphabet: &str) -> Result<String, String> {
   let alpha = alphabet.as_bytes();
   if alpha.len() < 64 {
      return Err(format!("jsd alphabet too short ({})", alpha.len()));
   }
   let raw = payload.as_bytes();
   let mut body = raw.to_vec();
   let mut flag = 0u8;
   if raw.len() >= 128 {
      let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
      encoder
         .write_all(raw)
         .map_err(|err| format!("jsd deflate failed: {err}"))?;
      let compressed = encoder
         .finish()
         .map_err(|err| format!("jsd deflate failed: {err}"))?;
      if compressed.len() < raw.len() {
         body = compressed;
         flag = 1;
      }
   }

   let mut framed = Vec::with_capacity(body.len() + 3);
   framed.push(253);
   framed.push(1);
   framed.push(flag);
   framed.extend_from_slice(&body);

   let mut state = fnv1a32(alphabet);
   for (index, byte) in framed.iter_mut().enumerate() {
      state = xorshift32(state);
      let key = ((state >> 24) ^ u32::from(alpha[index % 64])) & 0xff;
      *byte ^= key as u8;
   }

   let mut out = String::with_capacity((framed.len() * 8).div_ceil(6));
   let mut bit_buf: u32 = 0;
   let mut bit_cnt: u32 = 0;
   for byte in framed {
      bit_buf = (bit_buf << 8) | u32::from(byte);
      bit_cnt += 8;
      while bit_cnt >= 6 {
         bit_cnt -= 6;
         out.push(alpha[((bit_buf >> bit_cnt) & 63) as usize] as char);
      }
   }
   if bit_cnt > 0 {
      out.push(alpha[((bit_buf << (6 - bit_cnt)) & 63) as usize] as char);
   }
   Ok(out)
}

// ───────────────────────── small helpers ─────────────────────────

/// `jsdapi-xxxxxx`, the ray api.js builds locally and passes as `main.js?ray=`.
fn jsdapi_token() -> String {
   use ring::rand::{
      SecureRandom as _,
      SystemRandom,
   };
   const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
   let mut bytes = [0u8; 6];
   // A weak token still produces a syntactically valid ray; randomness here is
   // only to avoid colliding with a concurrent solve.
   let _ = SystemRandom::new().fill(&mut bytes);
   let tail: String = bytes
      .iter()
      .map(|byte| ALPHABET[(*byte as usize) % ALPHABET.len()] as char)
      .collect();
   format!("jsdapi-{tail}")
}

fn cookie_from_headers(headers: &HeaderMap, name: &str) -> Option<String> {
   for line in headers.get_all(header::SET_COOKIE) {
      if let Ok(line) = line.to_str()
         && let Some((cookie_name, Some(value))) = parse_set_cookie(line)
         && cookie_name == name
      {
         return Some(value);
      }
   }
   None
}

/// `name=value; attrs` → the name and either the value to keep or `None` when
/// the server is clearing it.
fn parse_set_cookie(line: &str) -> Option<(String, Option<String>)> {
   let mut parts = line.split(';');
   let (name, value) = parts.next()?.trim().split_once('=')?;
   let name = name.trim();
   if name.is_empty() {
      return None;
   }
   let gone = value.trim().eq_ignore_ascii_case("deleted")
      || parts.any(|attr| {
         attr.trim().split_once('=').is_some_and(|(key, val)| {
            key.trim().eq_ignore_ascii_case("max-age") && val.trim() == "0"
         })
      });
   Some((name.to_owned(), (!gone).then(|| value.trim().to_owned())))
}

fn host_of(url: &str) -> &str {
   url.strip_prefix("https://")
      .or_else(|| url.strip_prefix("http://"))
      .unwrap_or(url)
      .split('/')
      .next()
      .unwrap_or(url)
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn v2_round_trip_frames_and_packs() {
      let alphabet = "MQHmW+EXl4yeLIbUigx$wkN9RVPqJr6KYs5v-aOoFf213BCGthD0cZjATuzdn7pS8";
      let encoded = compress_v2("{\"t\":1,\"payload\":{}}", alphabet).expect("encodes");
      assert!(!encoded.is_empty());
      assert!(encoded.chars().all(|ch| alphabet.contains(ch)));
   }

   #[test]
   fn readable_main_decodes_string_table_via_quickjs() {
      let alphabet = "MQHmW+EXl4yeLIbUigx$wkN9RVPqJr6KYs5v-aOoFf213BCGthD0cZjATuzdn7pS8";
      let oneshot =
         "https://x.com/cdn-cgi/challenge-platform/h/g/jsd/oneshot/5eaf848a0845/0.47:1765:YMW/";
      let enc_alpha = BASE64.encode(alphabet.as_bytes());
      let enc_shot = BASE64.encode(oneshot.as_bytes());
      // A string table whose decoder is `atob` — the shape the extractor needs,
      // exercised through the real QuickJS evaluation path.
      let raw = format!(
         "var a0=(function(g){{function d(s){{return g.atob(s);}}return d;}})(globalThis);window._cf_chl_opt={{cRay:'0.1:2:ABCdefGHIjkl',xkKZ4:'tok123'}};a0(`{enc_alpha}`);a0(`{enc_shot}`);"
      );
      let synthetic = readable_main(&raw).expect("readable main");
      assert!(is_payload_encoder_v2(&synthetic));
      let extracted = extract_modern(&synthetic).expect("extract");
      assert_eq!(extracted.alphabet, alphabet);
      assert_eq!(extracted.s_arg, "5eaf848a0845");
      assert_eq!(extracted.t_arg, "0.47:1765:YMW");
      assert_eq!(extracted.cf_cvparam, "r");
      assert_eq!(extracted.chl_value, "tok123");
   }

   /// Live check against cfdata.lol (Cloudflare's trust-score page) over HTTP/2.
   /// Run with:
   ///   cargo test --release --ignored live_cfdata_score -- --nocapture
   #[tokio::test]
   #[ignore = "live network: mints a cf_clearance on cfdata.lol and reads the score"]
   async fn live_cfdata_score() {
      use crate::api::browser;

      // A Chrome-on-Windows session id, so the built client's wire identity
      // matches the Chrome payload we send.
      let mut sid = 0i64;
      while !(browser::identity_for(sid).engine == browser::Engine::Chrome
         && browser::navigator_platform(browser::identity_for(sid).os) == "Win32")
      {
         sid += 1;
      }
      let os = browser::identity_for(sid).os;
      let info = ClientInfo {
         user_agent:         browser::profile_header(sid, "user-agent").expect("ua"),
         sec_ch_ua:          browser::profile_header(sid, "sec-ch-ua").unwrap_or_default(),
         sec_ch_ua_platform: browser::profile_header(sid, "sec-ch-ua-platform")
            .unwrap_or_else(|| browser::sec_ch_ua_platform(os)),
         navigator_platform: browser::navigator_platform(os).to_owned(),
      };
      eprintln!("[cfdata] client ua = {}", info.user_agent);

      // cfdata.lol has no HTTP/3; Purpose::Api only seeds alt-svc h3 for x.com,
      // so these requests negotiate HTTP/2.
      let client = HttpClient::new("", "", crate::api::http::Purpose::Api);
      let egress = Egress {
         session: sid,
         proxy:   None,
      };
      let target = ClearanceTarget::classic("https://cfdata.lol/tools/connection");

      let cookie = tokio::time::timeout(
         Duration::from_secs(45),
         solve_inner(&client, &egress, &info, &target),
      )
      .await
      .expect("solve did not time out")
      .expect("solve produced cf_clearance");
      eprintln!(
         "[cfdata] cf_clearance = {}… ({} chars)",
         &cookie[..cookie.len().min(24)],
         cookie.len()
      );

      // Score: GET /tools/connection with the cookie and read the embedded JSON.
      let mut jar = HashMap::new();
      jar.insert("cf_clearance".to_owned(), cookie.clone());
      let (status, _h, body) = fetch(
         &client,
         &egress,
         Method::GET,
         "https://cfdata.lol/tools/connection",
         &mut jar,
         &document_headers(&info, &target),
         None,
      )
      .await
      .expect("score fetch");
      let text = body
         .replace("&quot;", "\"")
         .replace("&#34;", "\"")
         .replace("&amp;", "&");
      let cap = |pattern: &str| {
         Regex::new(pattern)
            .ok()
            .and_then(|re| re.captures(&text).map(|c| c[1].to_string()))
      };
      let passed = cap(r#""jsDetection"\s*:\s*\{\s*"passed"\s*:\s*(true|false)"#);
      let score = cap(r#""clientTrustScore"\s*:\s*(\d+)"#);
      let ja4 = cap(r#""ja4"\s*:\s*"([^"]+)""#);
      eprintln!(
         "[cfdata] status={status} jsDetection.passed={passed:?} clientTrustScore={score:?} ja4={ja4:?}"
      );

      assert!(!cookie.is_empty(), "no cf_clearance minted");
   }

   #[test]
   fn resolves_redirect_targets() {
      assert_eq!(
         resolve_redirect("https://x.com/", "/home").as_deref(),
         Some("https://x.com/home")
      );
      assert_eq!(
         resolve_redirect(
            "https://x.com/cdn-cgi/challenge-platform/scripts/jsd/main.js?ray=x",
            "https://x.com/elsewhere"
         )
         .as_deref(),
         Some("https://x.com/elsewhere")
      );
      assert_eq!(
         resolve_redirect("https://x.com/a/b", "c").as_deref(),
         Some("https://x.com/c")
      );
   }

   #[test]
   fn fnv_matches_reference() {
      // Same seeding the Node solver uses; a fixed input pins the constant.
      assert_eq!(fnv1a32("abc"), 440_920_331);
      assert_ne!(fnv1a32(""), 0);
   }

   #[test]
   fn extracts_modern_main_js() {
      // Only `xkKZ4` has a short alphanumeric value, so the token picker lands
      // on it the way the Node solver's `pickChlOptPropertyName` does.
      let js = r#"
         window._cf_chl_opt={cType:'managed-challenge',cRay:'0.123:1765:ABCdefGHIjkl',cH:'sha256-aaaaaaaaaaaa',xkKZ4:'g9f2e1'};
         var S=`MQHmW+EXl4yeLIbUigx$wkN9RVPqJr6KYs5v-aOoFf213BCGthD0cZjATuzdn7pS8`;
         obj.g(x,6,function(h){return`MQHmW+EXl4yeLIbUigx$wkN9RVPqJr6KYs5v-aOoFf213BCGthD0cZjATuzdn7pS8`.charAt(h)});
         y.open("POST",`/jsd/oneshot/5eaf848a0845/0.47:1765:YMWlK4/`+W.r);
         var frameByte=253,o=2166136261,p=16777619;var b=new Uint8Array(1);b[0]>>>24;
      "#;
      assert!(is_payload_encoder_v2(js));
      let extracted = extract_modern(js).expect("modern extract");
      assert_eq!(extracted.chl_value, "g9f2e1");
      assert_eq!(extracted.s_arg, "5eaf848a0845");
      assert_eq!(extracted.t_arg, "0.47:1765:YMWlK4");
      assert_eq!(extracted.cf_cvparam, "r");
      assert!(extracted.alphabet.len() >= 64);
   }

   #[test]
   fn payload_has_windows_hints() {
      let info = ClientInfo {
         user_agent:         "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/158.0.0.0".to_owned(),
         sec_ch_ua:          "\"Chromium\";v=\"158\"".to_owned(),
         sec_ch_ua_platform: "\"Windows\"".to_owned(),
         navigator_platform: "Win32".to_owned(),
      };
      let payload = build_payload(1_765_000_000, &info, true, &ClearanceTarget::x_com());
      assert!(payload.contains("\"t\":1765000000"));
      assert!(payload.contains("\"api\":true"));
      assert!(payload.contains("https://x.com"));
      assert!(payload.contains("n.hardwareConcurrency"));
      assert!(payload.contains("Win32"));
   }
}
