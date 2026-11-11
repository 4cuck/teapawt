//! A Turnstile in front of scrapers that spend the account pool.
//!
//! Every page a scraper loads costs a call on one of a limited number of X
//! accounts. Networks in `scrape.blockedAsns` are asked to solve a check on
//! every page; a country in `scrape.watchedCountries` is asked while its
//! pages run above `scrape.surgePerMinute`. A solved check lets that address
//! through for `scrape.passMinutes`. With `scrape.everyone` set, every
//! visitor is asked. Search pass holders (signed-in browsers and
//! whitelisted addresses) and crawlers Cloudflare has verified never see it.

use std::{
   collections::HashMap,
   net::SocketAddr,
   sync::{LazyLock, Mutex, PoisonError},
};

use axum::{
   Form, Router,
   extract::{ConnectInfo, Request, State},
   http::{HeaderMap, HeaderValue, Method, StatusCode, header},
   middleware::Next,
   response::{Html, IntoResponse as _, Redirect, Response},
   routing::post,
};
use axum_extra::extract::{
   CookieJar,
   cookie::{Cookie, SameSite},
};
use data_encoding::HEXLOWER;
use ring::hmac;
use serde::Deserialize;
use time::{Duration, OffsetDateTime};

use crate::{
   AppState,
   api::budget,
   config::ScrapeConfig,
   donations,
   types::Prefs,
   views::{
      layout::PageLayout,
      scrape::{self as view, GatePage, Why},
   },
};

const COOKIE: &str = "scrape_pass";
const VERIFY_PATH: &str = "/.scrape-check";
/// A surge keeps the check up this long after the rate falls back, so a
/// scraper that pauses for a minute does not walk straight back in.
const SURGE_HOLD_SECS: i64 = 10 * 60;
const MAX_RESUME: usize = 2048;

/// Paths a challenged visitor still needs: how to pay, how to get in touch,
/// the check itself, and the files pages are drawn with. The first list is
/// a page and everything under it; the second is raw prefixes.
const OPEN_PAGES: &[&str] = &["/contact", "/search-pass", "/donate", "/about"];
const OPEN_PREFIXES: &[&str] = &[
   "/.",
   "/css/",
   "/js/",
   "/fonts/",
   "/public/",
   "/md/",
   "/pic/",
   "/video/",
   "/gif/",
   "/robots.txt",
   "/sitemap.xml",
   "/sitemap-",
   "/favicon.",
   "/favicon-",
   "/logo.",
   "/apple-touch-icon",
   "/android-chrome-",
   "/site.webmanifest",
   "/opensearch.xml",
];

/// Requests over the last minute for one network or country.
#[derive(Clone, Copy, Default)]
struct Window {
   minute:        i64,
   current:       u32,
   previous:      u32,
   surging_until: i64,
}

impl Window {
   fn roll(&mut self, now: i64) {
      let minute = now.div_euclid(60);
      if minute != self.minute {
         self.previous = if minute == self.minute + 1 { self.current } else { 0 };
         self.current = 0;
         self.minute = minute;
      }
   }

   fn hit(&mut self, now: i64) {
      self.roll(now);
      self.current = self.current.saturating_add(1);
   }

   /// This minute so far, plus the part of the last one still inside the
   /// past 60 seconds.
   fn rate(&mut self, now: i64) -> u32 {
      self.roll(now);
      let left = u32::try_from(60 - now.rem_euclid(60)).unwrap_or(0);
      self.current + self.previous * left / 60
   }
}

static WINDOWS: LazyLock<Mutex<HashMap<String, Window>>> =
   LazyLock::new(|| Mutex::new(HashMap::new()));

fn with_window<T>(key: &str, work: impl FnOnce(&mut Window) -> T) -> T {
   let mut windows = WINDOWS.lock().unwrap_or_else(PoisonError::into_inner);
   work(windows.entry(key.to_owned()).or_default())
}

fn asn_key(asn: u32) -> String {
   format!("asn:{asn}")
}

fn country_key(country: &str) -> String {
   format!("cc:{}", country.to_ascii_uppercase())
}

/// Requests from `asn` that met the check in the last minute.
#[must_use]
pub fn network_rate(asn: u32) -> u32 {
   with_window(&asn_key(asn), |window| window.rate(now()))
}

/// Count one request against `key`, and say whether it is surging.
fn surging(key: &str, threshold: u32, now: i64) -> bool {
   with_window(key, |window| {
      window.hit(now);
      if window.rate(now) > threshold {
         window.surging_until = now + SURGE_HOLD_SECS;
      }
      window.surging_until > now
   })
}

/// What Cloudflare says about a request: the headers its transform rules
/// set, which the origin only takes from a trusted proxy.
struct Visitor<'a> {
   asn:          Option<u32>,
   country:      Option<&'a str>,
   verified_bot: bool,
}

impl<'a> Visitor<'a> {
   fn from_headers(headers: &'a HeaderMap) -> Self {
      let text = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
      Self {
         asn:          text("asn-num").and_then(|asn| asn.trim().parse().ok()),
         country:      text("cf-ipcountry").map(str::trim).filter(|cc| cc.len() == 2),
         verified_bot: text("client-bot").is_some_and(|bot| bot.trim() == "true"),
      }
   }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
   Network(u32),
   Surge,
   Everyone,
}

/// Why this visitor should solve the check, counting the request.
fn reason(config: &ScrapeConfig, visitor: &Visitor<'_>, now: i64) -> Option<Reason> {
   if let Some(asn) = visitor
      .asn
      .filter(|asn| config.blocked_asns.contains(asn))
   {
      with_window(&asn_key(asn), |window| window.hit(now));
      return Some(Reason::Network(asn));
   }
   if config.everyone {
      return Some(Reason::Everyone);
   }
   let country = visitor.country.filter(|country| {
      config
         .watched_countries
         .iter()
         .any(|watched| watched.eq_ignore_ascii_case(country))
   })?;
   surging(&country_key(country), config.surge_per_minute, now).then_some(Reason::Surge)
}

/// The reason to show again after a failed check, without counting.
fn reason_shown(config: &ScrapeConfig, visitor: &Visitor<'_>) -> Reason {
   let fallback = if config.everyone { Reason::Everyone } else { Reason::Surge };
   visitor
      .asn
      .filter(|asn| config.blocked_asns.contains(asn))
      .map_or(fallback, Reason::Network)
}

/// The home page costs no X call and carries the notice explaining the
/// check, so it stays readable before the check is solved.
fn is_open(path: &str) -> bool {
   path == "/"
      || OPEN_PAGES
         .iter()
         .any(|page| path == *page || path.strip_prefix(page).is_some_and(|rest| rest.starts_with('/')))
      || OPEN_PREFIXES.iter().any(|prefix| path.starts_with(prefix))
}

fn gated_request(request: &Request) -> bool {
   matches!(*request.method(), Method::GET | Method::HEAD) && !is_open(request.uri().path())
}

pub async fn scrape_gate(
   State(state): State<AppState>,
   ConnectInfo(peer): ConnectInfo<SocketAddr>,
   jar: CookieJar,
   request: Request,
   next: Next,
) -> Response {
   let config = &state.config.scrape;
   if !config.enabled
      || !state.config.turnstile.configured()
      || !state.trusted_proxies.contains(&peer.ip())
      || !gated_request(&request)
   {
      return next.run(request).await;
   }
   let visitor = Visitor::from_headers(request.headers());
   if visitor.verified_bot || cleared(&state, &jar) {
      return next.run(request).await;
   }
   let Some(reason) = reason(config, &visitor, now()) else {
      return next.run(request).await;
   };
   let resume = request
      .uri()
      .path_and_query()
      .map_or("/", |path| path.as_str())
      .to_owned();
   challenge(&state, &jar, reason, &resume, false)
}

pub fn router() -> Router<AppState> {
   Router::new().route(VERIFY_PATH, post(check))
}

#[derive(Deserialize)]
struct Check {
   #[serde(default, rename = "cf-turnstile-response")]
   token:  String,
   #[serde(default)]
   resume: String,
}

async fn check(
   State(state): State<AppState>,
   jar: CookieJar,
   headers: HeaderMap,
   Form(check): Form<Check>,
) -> Response {
   let resume = safe_resume(&check.resume);
   if check.token.is_empty() {
      let reason = reason_shown(&state.config.scrape, &Visitor::from_headers(&headers));
      return challenge(&state, &jar, reason, resume, true);
   }
   if let Err(codes) = super::search_gate::verify(&state.config.turnstile.secret_key, &check.token).await {
      tracing::info!(errors = %codes.join(","), "scrape check failed");
      let reason = reason_shown(&state.config.scrape, &Visitor::from_headers(&headers));
      return challenge(&state, &jar, reason, resume, true);
   }
   let Some(client) = budget::current_client() else {
      return private(Redirect::to(resume).into_response());
   };
   tracing::info!("scrape check passed");
   let cookie = pass_cookie(
      &state.config.config.hmac_key,
      client.as_str(),
      state.config.scrape.pass_minutes,
      state.config.server.https,
   );
   let origin = crate::utils::host::current_origin(state.config.url_prefix());
   let target = super::search_gate::absolute(&origin, resume);
   private((CookieJar::new().add(cookie), Redirect::to(&target)).into_response())
}

/// Only a path on this site, so the check cannot be used to bounce
/// visitors elsewhere.
fn safe_resume(resume: &str) -> &str {
   let local = resume.starts_with('/')
      && !resume.starts_with("//")
      && !resume.starts_with("/\\")
      && resume.len() <= MAX_RESUME
      && !resume.bytes().any(|byte| byte.is_ascii_control());
   if local { resume } else { "/" }
}

fn cleared(state: &AppState, jar: &CookieJar) -> bool {
   state.passes.admits(jar)
      || budget::current_client().is_some_and(|client| {
         pass_valid(
            &state.config.config.hmac_key,
            jar,
            client.as_str(),
            state.config.scrape.pass_minutes,
         )
      })
}

fn challenge(state: &AppState, jar: &CookieJar, reason: Reason, resume: &str, failed: bool) -> Response {
   let prefs = Prefs::from_cookies(jar, &state.config);
   let price = donations::format_usd(state.passes.price_cents());
   let goal = donations::format_usd(state.config.donations.goal_cents);
   let why = match reason {
      Reason::Network(asn) => Why::Network {
         asn,
         per_minute: network_rate(asn),
      },
      Reason::Surge => Why::Surge,
      Reason::Everyone => Why::Everyone,
   };
   let title = why.title();
   let content = view::render_gate(&GatePage {
      site_key: &state.config.turnstile.site_key,
      action: VERIFY_PATH,
      resume,
      why,
      price: &price,
      goal: &goal,
      pass_minutes: state.config.scrape.pass_minutes,
      failed,
   });
   let markup = PageLayout::new(&state.config, title, content)
      .prefs(&prefs)
      .render();
   // 429 is the status nginx and Cloudflare keep out of the page cache.
   // A cached copy would show this page to every reader of the URL.
   let mut response = (StatusCode::TOO_MANY_REQUESTS, Html(markup.into_string())).into_response();
   response
      .headers_mut()
      .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
   private(response)
}

fn private(mut response: Response) -> Response {
   let headers = response.headers_mut();
   headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
   headers.insert("cloudflare-cdn-cache-control", HeaderValue::from_static("no-store"));
   headers.insert("cdn-cache-control", HeaderValue::from_static("no-store"));
   response
}

fn now() -> i64 {
   OffsetDateTime::now_utc().unix_timestamp()
}

/// A pass is tied to the address that solved it. A scraper that rotates
/// addresses has to solve one per address.
fn pass_cookie(key: &str, client: &str, minutes: i64, secure: bool) -> Cookie<'static> {
   let expiry = now() + minutes * 60;
   let value = format!("{expiry}.{}", sign(key, client, expiry));
   Cookie::build((COOKIE, value))
      .path("/")
      .http_only(true)
      .secure(secure)
      .same_site(SameSite::Lax)
      .max_age(Duration::minutes(minutes))
      .build()
}

fn pass_valid(key: &str, jar: &CookieJar, client: &str, minutes: i64) -> bool {
   let Some((expiry, mac)) = jar.get(COOKIE).and_then(|cookie| cookie.value().split_once('.')) else {
      return false;
   };
   let Ok(expiry) = expiry.parse::<i64>() else {
      return false;
   };
   let now = now();
   if expiry <= now || expiry > now + minutes * 60 + 60 {
      return false;
   }
   let Ok(bytes) = HEXLOWER.decode(mac.as_bytes()) else {
      return false;
   };
   hmac::verify(&mac_key(key), &message(client, expiry), &bytes).is_ok()
}

fn message(client: &str, expiry: i64) -> Vec<u8> {
   format!("scrape|{client}|{expiry}").into_bytes()
}

fn sign(key: &str, client: &str, expiry: i64) -> String {
   HEXLOWER.encode(hmac::sign(&mac_key(key), &message(client, expiry)).as_ref())
}

fn mac_key(key: &str) -> hmac::Key {
   hmac::Key::new(hmac::HMAC_SHA256, key.as_bytes())
}

#[cfg(test)]
mod tests {
   use super::*;

   const KEY: &str = "0123456789abcdef0123456789abcdef";

   fn config() -> ScrapeConfig {
      ScrapeConfig {
         surge_per_minute: 5,
         watched_countries: vec!["SG".to_owned()],
         ..ScrapeConfig::default()
      }
   }

   fn visitor(asn: Option<u32>, country: Option<&str>) -> Visitor<'_> {
      Visitor {
         asn,
         country,
         verified_bot: false,
      }
   }

   #[test]
   fn a_blocked_network_is_always_checked() {
      let config = config();
      let tencent = visitor(Some(132_203), Some("SG"));
      assert_eq!(reason(&config, &tencent, 1_000), Some(Reason::Network(132_203)));
      let elsewhere = visitor(Some(132_203), Some("US"));
      assert_eq!(reason(&config, &elsewhere, 1_000), Some(Reason::Network(132_203)));
   }

   #[test]
   fn a_watched_country_is_checked_only_while_it_surges_and_a_while_after() {
      let config = config();
      let start = 7_000_000_020;
      let reader = visitor(Some(56_300), Some("sg"));
      let key = country_key("SG");
      WINDOWS.lock().unwrap().remove(&key);
      for second in 0..5 {
         assert_eq!(reason(&config, &reader, start + second), None);
      }
      assert_eq!(reason(&config, &reader, start + 5), Some(Reason::Surge));
      // Quiet, but held.
      assert_eq!(reason(&config, &reader, start + 300), Some(Reason::Surge));
      assert_eq!(reason(&config, &reader, start + 5 + SURGE_HOLD_SECS + 1), None);
      assert_eq!(reason(&config, &visitor(None, Some("US")), start), None);
   }

   #[test]
   fn the_rate_counts_the_part_of_last_minute_still_inside() {
      let mut window = Window::default();
      // 60 requests in the minute before 6000.
      for _ in 0..60 {
         window.hit(5_999);
      }
      assert_eq!(window.rate(5_999), 60);
      // Halfway into the next minute, half of the last one still counts.
      assert_eq!(window.rate(6_030), 30);
      // Two minutes on, none of it does.
      assert_eq!(window.rate(6_125), 0);
   }

   #[test]
   fn paying_and_static_pages_stay_open() {
      for open in [
         "/",
         "/contact",
         "/search-pass/login",
         "/donate",
         "/about",
         "/.scrape-check",
         "/css/clean.css",
         "/pic/abc",
         "/robots.txt",
         "/sitemap-profiles.xml",
      ] {
         assert!(is_open(open), "{open}");
      }
      for gated in ["/jack", "/contactlenses", "/donatello/rss", "/jack/status/20", "/search"] {
         assert!(!is_open(gated), "{gated}");
      }
   }

   #[test]
   fn everyone_is_checked_but_a_blocked_network_keeps_its_own_reason() {
      let config = ScrapeConfig {
         everyone: true,
         ..config()
      };
      assert_eq!(reason(&config, &visitor(Some(7922), Some("US")), 1_000), Some(Reason::Everyone));
      assert_eq!(reason(&config, &visitor(None, None), 1_000), Some(Reason::Everyone));
      assert_eq!(
         reason(&config, &visitor(Some(132_203), Some("SG")), 1_000),
         Some(Reason::Network(132_203))
      );
      assert_eq!(reason_shown(&config, &visitor(None, None)), Reason::Everyone);
   }

   #[test]
   fn a_pass_belongs_to_the_address_that_solved_it() {
      let cookie = pass_cookie(KEY, "203.0.113.9", 60, true);
      let jar = CookieJar::new().add(cookie);
      assert!(pass_valid(KEY, &jar, "203.0.113.9", 60));
      assert!(!pass_valid(KEY, &jar, "203.0.113.10", 60));
      assert!(!pass_valid("another-key-another-key-another!!", &jar, "203.0.113.9", 60));
   }

   #[test]
   fn resume_stays_on_this_site() {
      assert_eq!(safe_resume("/jack?x=1"), "/jack?x=1");
      assert_eq!(safe_resume("//evil.example/"), "/");
      assert_eq!(safe_resume("https://evil.example/"), "/");
      assert_eq!(safe_resume("/\\evil.example"), "/");
   }
}