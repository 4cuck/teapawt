//! Voluntary Castle debugging capture and admin explorer.
//!
//! `/admin/fingerprint` is a themed voluntary capture page that loads the live
//! Castle script x.com serves (under a neutral asset name so content blockers
//! do not strip it by default) and stores a submission only when the visitor
//! clicks the button.
//!
//! Submitted captures are inspected interactively at
//! `/admin/stats/fingerprints` behind administrator sign-in.

use std::{
   collections::{
      HashMap,
      HashSet,
   },
   net::{
      IpAddr,
      SocketAddr,
   },
   path::{
      Path,
      PathBuf,
   },
   sync::{
      LazyLock,
      atomic::{
         AtomicU32,
         Ordering,
      },
   },
   time::{
      Duration,
      Instant,
   },
};

use axum::{
   Json,
   Router,
   body::Bytes,
   extract::{
      ConnectInfo,
      DefaultBodyLimit,
      Path as UrlPath,
      State,
   },
   http::{
      HeaderMap,
      StatusCode,
      header,
   },
   response::{
      Html,
      IntoResponse,
      Response,
   },
   routing::get,
};
use axum_extra::extract::CookieJar;
use data_encoding::HEXLOWER;
use maud::{
   DOCTYPE,
   html,
};
use regex::Regex;
use serde_json::{
   Value,
   json,
};
use time::OffsetDateTime;
use tokio::sync::Mutex;

use super::{
   admin_stats::forbidden_html,
   import::administrator,
};
use crate::{
   AppState,
   types::Prefs,
   views::{
      admin_stats as stats_view,
      layout::{
         CLEAN_CSS,
         STYLE_CSS,
         theme_stylesheet,
      },
   },
};

const XWEB: &str = "https://abs.twimg.com/x-web/x-web/";
const CJS_WRAPPER: &str = "const __cjs=(factory)=>()=>{const \
                           module={exports:{}};factory(module.exports,module,module.exports);\
                           return module.exports;};const e=__cjs;";
const FETCH_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like \
                        Gecko) Chrome/158.0.0.0 Safari/537.36";
const SDK_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_BODY: usize = 6 * 1024 * 1024;
const MAX_SUBMISSIONS: usize = 2_000;
const HOUR: Duration = Duration::from_secs(60 * 60);

const DIAG_CSS: &str = "/css/diag.css?v=1";
const COLLECTOR_JS: &str = "/js/m7k4q9v2.js?v=2";
const SDK_ROUTE: &str = "/admin/assets/r8x2n5p1.js";
const SUBMIT_ROUTE: &str = "/admin/assets/k3w9v4t6";
const SDK_CACHE_FILE: &str = "sdk-bundle.js";

const ACCEPT_CH: &str = "Sec-CH-UA-Arch, Sec-CH-UA-Bitness, Sec-CH-UA-Full-Version, \
                         Sec-CH-UA-Full-Version-List, Sec-CH-UA-Model, \
                         Sec-CH-UA-Platform-Version, Sec-CH-UA-WoW64, Sec-CH-UA-Form-Factors";

const WIRE_HEADERS: &[&str] = &[
   "user-agent",
   "accept-language",
   "sec-ch-ua",
   "sec-ch-ua-mobile",
   "sec-ch-ua-platform",
   "sec-ch-ua-platform-version",
   "sec-ch-ua-arch",
   "sec-ch-ua-bitness",
   "sec-ch-ua-model",
   "sec-ch-ua-wow64",
   "sec-ch-ua-full-version",
   "sec-ch-ua-full-version-list",
   "sec-ch-ua-form-factors",
   "dnt",
   "sec-gpc",
];

static ENTRY_RE: LazyLock<Regex> = LazyLock::new(|| {
   Regex::new(r"https://abs\.twimg\.com/x-web/x-web/entry-client-logged-out-[A-Za-z0-9_-]+\.js")
      .expect("entry regex")
});
static ASSET_RE: LazyLock<Regex> =
   LazyLock::new(|| Regex::new(r"assets/[A-Za-z0-9_-]+\.js").expect("asset regex"));
static CHUNK_RE: LazyLock<Regex> =
   LazyLock::new(|| Regex::new(r"\./([A-Za-z0-9_-]+\.js)").expect("chunk regex"));

struct CachedSdk {
   fetched: Instant,
   body:    String,
   sha:     String,
}

static SDK: Mutex<Option<CachedSdk>> = Mutex::const_new(None);
static RATES: Mutex<Vec<(String, Instant)>> = Mutex::const_new(Vec::new());
static INDEX_LOCK: Mutex<()> = Mutex::const_new(());
static SEQ: AtomicU32 = AtomicU32::new(0);

pub fn router() -> Router<AppState> {
   Router::new()
      .route("/admin/fingerprint", get(page).post(submit))
      .route(SDK_ROUTE, get(sdk_bundle))
      .route(SUBMIT_ROUTE, axum::routing::post(submit))
      .route("/admin/stats/fingerprints", get(explorer_page))
      .route("/admin/stats/captures/data", get(list))
      .route("/admin/stats/fingerprints/data", get(list))
      .route("/admin/stats/captures/{name}", get(one).delete(remove))
      .route("/admin/stats/fingerprints/{name}", get(one).delete(remove))
      .layer(DefaultBodyLimit::max(MAX_BODY))
}

fn store_dir() -> PathBuf {
   PathBuf::from("data/fingerprints")
}

fn color_scheme_for(theme: &str) -> &'static str {
   let lower = theme.trim().to_ascii_lowercase();
   if lower.starts_with("auto") {
      "dark light"
   } else if lower.contains("light")
      || lower.contains("latte")
      || lower.contains("dawn")
      || lower == "twitter"
   {
      "light"
   } else {
      "dark"
   }
}

async fn page(State(state): State<AppState>, jar: CookieJar) -> Response {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let is_admin = administrator(&state, &jar).await;
   let theme_name = if prefs.theme.trim().is_empty() {
      "Teapot"
   } else {
      prefs.theme.as_str()
   };
   let scheme = color_scheme_for(theme_name);
   let theme_href = theme_stylesheet(theme_name);

   let markup = html! {
      (DOCTYPE)
      html lang="en" data-scheme=(scheme) {
         head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            meta name="color-scheme" content=(scheme);
            meta name="robots" content="noindex, nofollow";
            title { "Voluntary Castle capture" }
            link rel="stylesheet" href=(STYLE_CSS);
            link rel="stylesheet" href=(CLEAN_CSS);
            @if let Some(ref href) = theme_href {
               link rel="stylesheet" href=(href);
            }
            link rel="stylesheet" href=(DIAG_CSS);
         }
         body class="diag-body" data-theme-name=(theme_name) data-font-size=(prefs.font_size) {
            main class="diag-shell" {
               nav class="diag-topbar" aria-label="Capture navigation" {
                  div class="diag-topbar-links" {
                     a href="/" { "← Home" }
                     a href="/settings" { "Theme settings" }
                     @if is_admin {
                        a href="/admin/stats/fingerprints" { "Submitted captures →" }
                     }
                  }
                  span class="diag-theme-pill" { "Theme: " (theme_name) }
               }

               section class="diag-card" {
                  p class="diag-eyebrow" { "Voluntary Castle debugging" }
                  h1 { "Submit this browser capture" }
                  p class="diag-copy" {
                     "This page runs the live Castle script x.com is currently serving and records "
                     "the exact token alongside the canvas draws, font measurements, WebGL strings, "
                     "screen values, client hints, and — like the real signup phone screen — the "
                     "mouse and keyboard activity your browser folds into the token."
                  }
                  ul class="diag-points" {
                     li { "Nothing loads or transmits until you press the button below." }
                     li { "After you start, a throwaway phone field opens: move your mouse and type a few digits for about ten seconds, then press Continue. That interaction is what makes the capture match a real signup." }
                     li { "The captured token, browser properties, and the IP address this site sees are stored for Castle debugging." }
                     li { "Submissions are private and only visible inside the administrator stats panel." }
                  }

                  label class="diag-field" {
                     span { "Optional label / setup note" }
                     input
                        id="diag-note"
                        type="text"
                        maxlength="160"
                        autocomplete="off"
                        placeholder="e.g. Win11 Chrome 158 clean, Firefox 154 + uBlock";
                  }

                  label class="diag-field" id="diag-dwell" hidden {
                     span { "Throwaway phone number (not stored — type and move your mouse for ~10s)" }
                     input
                        id="diag-phone"
                        type="tel"
                        inputmode="tel"
                        maxlength="20"
                        autocomplete="off"
                        disabled
                        placeholder="e.g. 5551234567";
                  }

                  div class="diag-actions" {
                     button id="diag-submit" class="diag-btn diag-btn--primary" type="button" {
                        "Run & submit this browser"
                     }
                     button id="diag-copy-id" class="diag-btn" type="button" hidden {
                        "Copy ID"
                     }
                  }

                  p id="diag-status" class="diag-status" role="status" hidden {}
                  div id="diag-metrics" class="diag-metrics" hidden {}

                  details id="diag-details" class="diag-details" hidden {
                     summary { "Inspect submitted payload" }
                     pre id="diag-preview" class="diag-pre" {}
                  }
               }
            }
            script src=(COLLECTOR_JS) defer="" {}
         }
      }
   };

   let mut response = Html(markup.into_string()).into_response();
   response.headers_mut().insert(
      header::HeaderName::from_static("accept-ch"),
      header::HeaderValue::from_static(ACCEPT_CH),
   );
   response
      .headers_mut()
      .insert(header::CONTENT_SECURITY_POLICY, capture_csp());
   response
}

fn capture_csp() -> header::HeaderValue {
   header::HeaderValue::from_static(
      "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' \
       data:; connect-src 'self' https://castle.io https://*.castle.io; worker-src 'self' blob:; \
       font-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
   )
}

async fn explorer_page(State(state): State<AppState>, jar: CookieJar) -> Response {
   let theme = Prefs::from_cookies(&jar, &state.config).theme;
   if !administrator(&state, &jar).await {
      return forbidden_html(&theme);
   }
   Html(stats_view::fingerprints(&theme).into_string()).into_response()
}

async fn sdk_bundle() -> Response {
   match castle_source().await {
      Ok((body, _)) => {
         let mut response = (
            [
               (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
               (header::CACHE_CONTROL, "private, no-store"),
            ],
            body,
         )
            .into_response();
         response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, capture_csp());
         response
      },
      Err(error) => {
         tracing::warn!("castle sdk: {error}");
         (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            error,
         )
            .into_response()
      },
   }
}

async fn submit(
   State(state): State<AppState>,
   ConnectInfo(peer): ConnectInfo<SocketAddr>,
   headers: HeaderMap,
   body: Bytes,
) -> Response {
   let ip = client_ip(peer.ip(), &headers, &state.trusted_proxies);
   if !rate_ok(&ip.to_string()).await {
      return (
         StatusCode::TOO_MANY_REQUESTS,
         Json(json!({ "error": "too many submissions from this address" })),
      )
         .into_response();
   }
   let parsed: Value = match serde_json::from_slice(&body) {
      Ok(value) => value,
      Err(_) => {
         return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "body is not json" })),
         )
            .into_response();
      },
   };
   if parsed.get("consent") != Some(&Value::Bool(true)) {
      return (
         StatusCode::BAD_REQUEST,
         Json(json!({ "error": "consent is required" })),
      )
         .into_response();
   }
   let Some(token) = parsed.get("token").and_then(Value::as_str) else {
      return (
         StatusCode::BAD_REQUEST,
         Json(json!({ "error": "token is required" })),
      )
         .into_response();
   };
   if !(40..=20_000).contains(&token.len()) {
      return (
         StatusCode::BAD_REQUEST,
         Json(json!({ "error": "token length is not a castle token" })),
      )
         .into_response();
   }

   let id = new_id();
   let sdk = parsed
      .get("sdk")
      .and_then(Value::as_str)
      .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
      .unwrap_or("unknown");
   let record = json!({
      "id": id,
      "receivedAt": OffsetDateTime::now_utc().to_string(),
      "ip": ip.to_string(),
      "sdk": sdk,
      "wire": wire_headers(&headers),
      "submission": parsed,
   });
   if let Err(error) = write_record(&id, &record).await {
      tracing::warn!("fingerprint store: {error}");
      return (
         StatusCode::INTERNAL_SERVER_ERROR,
         Json(json!({ "error": "could not store the submission" })),
      )
         .into_response();
   }
   Json(json!({ "id": id })).into_response()
}

async fn api_authorized(state: &AppState, jar: &CookieJar, headers: &HeaderMap) -> bool {
   administrator(state, jar).await || debug_authorized(state, headers)
}

async fn list(State(state): State<AppState>, jar: CookieJar, headers: HeaderMap) -> Response {
   if !api_authorized(&state, &jar, &headers).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   match load_summaries().await {
      Ok(rows) => Json(rows).into_response(),
      Err(error) => {
         tracing::warn!("fingerprint list: {error}");
         (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "could not read submissions" })),
         )
            .into_response()
      },
   }
}

async fn one(
   State(state): State<AppState>,
   jar: CookieJar,
   headers: HeaderMap,
   UrlPath(name): UrlPath<String>,
) -> Response {
   if !api_authorized(&state, &jar, &headers).await {
      return StatusCode::FORBIDDEN.into_response();
   }
   let Some(path) = record_path(&name) else {
      return StatusCode::NOT_FOUND.into_response();
   };
   match tokio::fs::read(&path).await {
      Ok(bytes) => {
         (
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            bytes,
         )
            .into_response()
      },
      Err(_) => StatusCode::NOT_FOUND.into_response(),
   }
}

fn is_cross_site_header(headers: &HeaderMap) -> bool {
   headers
      .get("sec-fetch-site")
      .is_some_and(|value| !matches!(value.as_bytes(), b"same-origin" | b"none"))
}

async fn remove(
   State(state): State<AppState>,
   jar: CookieJar,
   headers: HeaderMap,
   UrlPath(name): UrlPath<String>,
) -> Response {
   let bearer = debug_authorized(&state, &headers);
   if !bearer && (!administrator(&state, &jar).await || is_cross_site_header(&headers)) {
      return StatusCode::FORBIDDEN.into_response();
   }
   let Some(path) = record_path(&name) else {
      return StatusCode::NOT_FOUND.into_response();
   };
   let _guard = INDEX_LOCK.lock().await;
   if tokio::fs::remove_file(&path).await.is_err() {
      return StatusCode::NOT_FOUND.into_response();
   }
   let index_path = store_dir().join("index.jsonl");
   if let Ok(text) = tokio::fs::read_to_string(&index_path).await {
      let mut out = String::new();
      for line in text.lines() {
         let keep = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|val| val.get("id").and_then(Value::as_str).map(|id| id != name))
            .unwrap_or(true);
         if keep {
            out.push_str(line);
            out.push('\n');
         }
      }
      let _ = tokio::fs::write(&index_path, out).await;
   }
   Json(json!({ "ok": true, "deleted": name })).into_response()
}

fn debug_authorized(state: &AppState, headers: &HeaderMap) -> bool {
   if !state.config.config.enable_debug || state.config.config.debug_token.is_empty() {
      return false;
   }
   let Some(auth) = headers
      .get(header::AUTHORIZATION)
      .and_then(|value| value.to_str().ok())
   else {
      return false;
   };
   let Some(token) = auth.strip_prefix("Bearer ") else {
      return false;
   };
   super::debug::constant_time_eq(token.as_bytes(), state.config.config.debug_token.as_bytes())
}

fn client_ip(peer: IpAddr, headers: &HeaderMap, trusted: &[IpAddr]) -> IpAddr {
   if !trusted.contains(&peer) {
      return peer;
   }
   headers
      .get("x-forwarded-for")
      .and_then(|value| value.to_str().ok())
      .into_iter()
      .flat_map(|value| value.rsplit(','))
      .filter_map(|entry| entry.trim().parse::<IpAddr>().ok())
      .find(|address| !trusted.contains(address))
      .unwrap_or(peer)
}

fn wire_headers(headers: &HeaderMap) -> Value {
   let mut wire = serde_json::Map::new();
   for name in WIRE_HEADERS {
      if let Some(value) = headers.get(*name).and_then(|value| value.to_str().ok()) {
         wire.insert((*name).to_owned(), Value::String(value.to_owned()));
      }
   }
   Value::Object(wire)
}

fn new_id() -> String {
   let now = OffsetDateTime::now_utc();
   let mix = now.nanosecond() ^ SEQ.fetch_add(1, Ordering::Relaxed);
   format!(
      "{:04}{:02}{:02}T{:02}{:02}{:02}Z-{:08x}",
      now.year(),
      u8::from(now.month()),
      now.day(),
      now.hour(),
      now.minute(),
      now.second(),
      mix
   )
}

fn valid_id(name: &str) -> bool {
   let bytes = name.as_bytes();
   bytes.len() == 25
      && bytes[8] == b'T'
      && bytes[15] == b'Z'
      && bytes[16] == b'-'
      && bytes[..8].iter().all(u8::is_ascii_digit)
      && bytes[9..15].iter().all(u8::is_ascii_digit)
      && bytes[17..].iter().all(u8::is_ascii_hexdigit)
}

fn record_path(name: &str) -> Option<PathBuf> {
   if !valid_id(name) {
      return None;
   }
   Some(store_dir().join(format!("{name}.json")))
}

fn detect_browser(ua: &str) -> &'static str {
   if ua.contains("Edg/") || ua.contains("EdgA/") || ua.contains("EdgiOS/") {
      "Edge"
   } else if ua.contains("Firefox/") || ua.contains("FxiOS/") {
      "Firefox"
   } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
      "Chrome"
   } else if ua.contains("Safari/") {
      "Safari"
   } else {
      "Other"
   }
}

fn detect_os(ua: &str, platform: &str) -> &'static str {
   if ua.contains("Android") {
      "Android"
   } else if ua.contains("iPhone") || ua.contains("iPad") || ua.contains("iPod") {
      "iOS"
   } else if ua.contains("Windows") || platform.eq_ignore_ascii_case("Win32") {
      "Windows"
   } else if ua.contains("Mac OS X") || platform.to_ascii_lowercase().contains("mac") {
      "macOS"
   } else if ua.contains("Linux") || platform.to_ascii_lowercase().contains("linux") {
      "Linux"
   } else {
      "Other"
   }
}

fn count_fonts(events: &[Value]) -> (usize, usize) {
   let mut bases: HashMap<&str, f64> = HashMap::new();
   for ev in events {
      if ev.get("op").and_then(Value::as_str) != Some("measureText") {
         continue;
      }
      if ev.get("text").and_then(Value::as_str) != Some("mmmmmmmmmmlli") {
         continue;
      }
      let Some(font) = ev.get("font").and_then(Value::as_str) else {
         continue;
      };
      let Some(width) = ev.get("width").and_then(Value::as_f64) else {
         continue;
      };
      if let Some(rest) = font.strip_prefix("100px ")
         && !rest.contains(',')
      {
         bases.entry(rest).or_insert(width);
      }
   }
   let mut hits: HashMap<String, bool> = HashMap::new();
   for ev in events {
      if ev.get("op").and_then(Value::as_str) != Some("measureText") {
         continue;
      }
      if ev.get("text").and_then(Value::as_str) != Some("mmmmmmmmmmlli") {
         continue;
      }
      let Some(font) = ev.get("font").and_then(Value::as_str) else {
         continue;
      };
      let Some(width) = ev.get("width").and_then(Value::as_f64) else {
         continue;
      };
      let Some(rest) = font.strip_prefix("100px ") else {
         continue;
      };
      let Some((fam_raw, fallback)) = rest.rsplit_once(", ") else {
         continue;
      };
      if !matches!(fallback, "monospace" | "sans-serif" | "serif") {
         continue;
      }
      let fam = fam_raw
         .trim_matches(|ch| ch == '"' || ch == '\'')
         .to_owned();
      let different = bases
         .get(fallback)
         .is_some_and(|base| (width - base).abs() >= 0.05);
      let entry = hits.entry(fam).or_insert(false);
      if different {
         *entry = true;
      }
   }
   let installed = hits.values().filter(|&&hit| hit).count();
   (installed, hits.len())
}

fn summarize_record(record: &Value) -> Value {
   let id = record.get("id").and_then(Value::as_str).unwrap_or("");
   let ua = record
      .pointer("/wire/user-agent")
      .and_then(Value::as_str)
      .or_else(|| {
         record
            .pointer("/submission/browser/userAgent")
            .and_then(Value::as_str)
      })
      .unwrap_or("");
   let platform = record
      .pointer("/submission/browser/userAgentData/platform")
      .and_then(Value::as_str)
      .or_else(|| {
         record
            .pointer("/submission/browser/platform")
            .and_then(Value::as_str)
      })
      .unwrap_or("");
   let token = record
      .pointer("/submission/token")
      .and_then(Value::as_str)
      .unwrap_or("");
   let keyid = token.split_once('|').map_or("", |(head, _)| head);
   let events = record
      .pointer("/submission/tape/events")
      .and_then(Value::as_array)
      .map(Vec::as_slice)
      .unwrap_or(&[]);
   let mut measure_count = 0_usize;
   let mut canvas_count = 0_usize;
   let mut image_data_count = 0_usize;
   for ev in events {
      match ev.get("op").and_then(Value::as_str) {
         Some("measureText") => measure_count += 1,
         Some("toDataURL") => canvas_count += 1,
         Some("getImageData") => image_data_count += 1,
         _ => {},
      }
   }
   let (fonts_installed, fonts_total) = count_fonts(events);
   let screen_w = record
      .pointer("/submission/browser/screen/width")
      .and_then(Value::as_i64);
   let screen_h = record
      .pointer("/submission/browser/screen/height")
      .and_then(Value::as_i64);
   let screen = match (screen_w, screen_h) {
      (Some(w), Some(h)) => format!("{w}×{h}"),
      _ => String::new(),
   };
   let label = record
      .pointer("/submission/label")
      .and_then(Value::as_str)
      .map(str::trim)
      .filter(|value| !value.is_empty())
      .map(|value| value.chars().take(160).collect::<String>());
   let gpu = record
      .pointer("/submission/browser/webgl/unmaskedRenderer")
      .and_then(Value::as_str)
      .or_else(|| {
         record
            .pointer("/submission/browser/webgl/renderer")
            .and_then(Value::as_str)
      })
      .unwrap_or("");
   let tz = record
      .pointer("/submission/browser/timeZone")
      .and_then(Value::as_str)
      .unwrap_or("");
   let cuid = record
      .pointer("/submission/cuid")
      .and_then(Value::as_str)
      .unwrap_or("");
   let cores = record
      .pointer("/submission/browser/hardwareConcurrency")
      .and_then(Value::as_u64);
   let memory = record
      .pointer("/submission/browser/deviceMemory")
      .and_then(Value::as_f64);
   let full_version = record
      .pointer("/submission/browser/userAgentData/highEntropy/uaFullVersion")
      .and_then(Value::as_str)
      .unwrap_or("");
   let platform_version = record
      .pointer("/submission/browser/userAgentData/highEntropy/platformVersion")
      .and_then(Value::as_str)
      .unwrap_or("");
   let elapsed_ms = record
      .pointer("/submission/elapsedMs")
      .and_then(Value::as_u64);
   let interaction_count = record
      .pointer("/submission/interaction/events")
      .and_then(Value::as_array)
      .map_or(0, Vec::len);
   let interaction_ms = record
      .pointer("/submission/interaction/durationMs")
      .and_then(Value::as_u64);

   json!({
      "v": 2,
      "id": id,
      "receivedAt": record.get("receivedAt").and_then(Value::as_str).unwrap_or(""),
      "ip": record.get("ip").and_then(Value::as_str).unwrap_or(""),
      "sdk": record.get("sdk").and_then(Value::as_str).unwrap_or(""),
      "label": label,
      "userAgent": ua,
      "browser": detect_browser(ua),
      "os": detect_os(ua, platform),
      "platform": platform,
      "fullVersion": full_version,
      "platformVersion": platform_version,
      "tokenLength": token.len(),
      "keyid": keyid,
      "cuid": cuid,
      "screen": screen,
      "cores": cores,
      "memory": memory,
      "webglRenderer": gpu,
      "timeZone": tz,
      "fontsInstalled": fonts_installed,
      "fontsTotal": fonts_total,
      "measureCount": measure_count,
      "canvasCount": canvas_count,
      "imageDataCount": image_data_count,
      "eventCount": events.len(),
      "interactionCount": interaction_count,
      "interactionMs": interaction_ms,
      "elapsedMs": elapsed_ms,
      "hasHighEntropy": record.pointer("/wire/sec-ch-ua-full-version-list").is_some(),
   })
}

async fn load_summaries() -> Result<Vec<Value>, String> {
   let dir = store_dir();
   if !dir.exists() {
      return Ok(Vec::new());
   }
   let _guard = INDEX_LOCK.lock().await;
   let mut disk_ids = HashSet::new();
   let mut entries = tokio::fs::read_dir(&dir)
      .await
      .map_err(|error| error.to_string())?;
   while let Some(entry) = entries
      .next_entry()
      .await
      .map_err(|error| error.to_string())?
   {
      let name = entry.file_name();
      let name = name.to_string_lossy();
      if let Some(stem) = name.strip_suffix(".json")
         && valid_id(stem)
      {
         disk_ids.insert(stem.to_owned());
      }
   }

   let index_path = dir.join("index.jsonl");
   let existing_text = tokio::fs::read_to_string(&index_path)
      .await
      .unwrap_or_default();
   let mut by_id: HashMap<String, Value> = HashMap::new();
   let mut dirty = false;
   for line in existing_text.lines() {
      let Ok(row) = serde_json::from_str::<Value>(line) else {
         continue;
      };
      let Some(id) = row.get("id").and_then(Value::as_str) else {
         continue;
      };
      if !disk_ids.contains(id) {
         dirty = true;
         continue;
      }
      if row.get("v").and_then(Value::as_u64) == Some(2) {
         by_id.insert(id.to_owned(), row);
      } else {
         dirty = true;
      }
   }

   for id in &disk_ids {
      if by_id.contains_key(id) {
         continue;
      }
      let path = dir.join(format!("{id}.json"));
      if let Ok(bytes) = tokio::fs::read(&path).await
         && let Ok(record) = serde_json::from_slice::<Value>(&bytes)
      {
         by_id.insert(id.clone(), summarize_record(&record));
         dirty = true;
      }
   }

   let mut rows: Vec<Value> = by_id.into_values().collect();
   rows.sort_unstable_by(|left, right| {
      let lid = left.get("id").and_then(Value::as_str).unwrap_or("");
      let rid = right.get("id").and_then(Value::as_str).unwrap_or("");
      rid.cmp(lid)
   });

   if dirty {
      let mut out = String::new();
      for row in rows.iter().rev() {
         if let Ok(line) = serde_json::to_string(row) {
            out.push_str(&line);
            out.push('\n');
         }
      }
      let _ = tokio::fs::write(&index_path, out).await;
   }

   Ok(rows)
}

async fn write_record(id: &str, record: &Value) -> Result<(), String> {
   let dir = store_dir();
   tokio::fs::create_dir_all(&dir)
      .await
      .map_err(|error| error.to_string())?;
   let _guard = INDEX_LOCK.lock().await;
   if submission_count(&dir).await? >= MAX_SUBMISSIONS {
      return Err("fingerprint store is full".to_owned());
   }
   let path = dir.join(format!("{id}.json"));
   let bytes = serde_json::to_vec_pretty(record).map_err(|error| error.to_string())?;
   tokio::fs::write(&path, &bytes)
      .await
      .map_err(|error| error.to_string())?;
   let summary = summarize_record(record);
   let mut line = serde_json::to_string(&summary).map_err(|error| error.to_string())?;
   line.push('\n');
   let mut index = tokio::fs::OpenOptions::new()
      .create(true)
      .append(true)
      .open(dir.join("index.jsonl"))
      .await
      .map_err(|error| error.to_string())?;
   use tokio::io::AsyncWriteExt as _;
   index
      .write_all(line.as_bytes())
      .await
      .map_err(|error| error.to_string())?;
   Ok(())
}

async fn submission_count(dir: &Path) -> Result<usize, String> {
   let mut entries = tokio::fs::read_dir(dir)
      .await
      .map_err(|error| error.to_string())?;
   let mut count = 0_usize;
   while let Some(entry) = entries
      .next_entry()
      .await
      .map_err(|error| error.to_string())?
   {
      let name = entry.file_name();
      let name = name.to_string_lossy();
      if name.ends_with(".json") {
         count += 1;
      }
   }
   Ok(count)
}

async fn rate_ok(ip: &str) -> bool {
   let mut rates = RATES.lock().await;
   let now = Instant::now();
   rates.retain(|(_, seen)| now.saturating_duration_since(*seen) < HOUR);
   let recent = rates.iter().filter(|(key, _)| key == ip).count();
   if recent >= 30 {
      return false;
   }
   rates.push((ip.to_owned(), now));
   true
}

async fn castle_source() -> Result<(String, String), String> {
   {
      let guard = SDK.lock().await;
      if let Some(hit) = guard.as_ref()
         && hit.fetched.elapsed() < SDK_TTL
      {
         return Ok((hit.body.clone(), hit.sha.clone()));
      }
   }
   match fetch_sdk().await {
      Ok(body) => {
         let sha = sha256(body.as_bytes());
         let script = format!("globalThis.__r8x2h={sha:?};\n{body}");
         let sha = sha256(script.as_bytes());
         {
            let mut guard = SDK.lock().await;
            *guard = Some(CachedSdk {
               fetched: Instant::now(),
               body:    script.clone(),
               sha:     sha.clone(),
            });
         }
         let path = store_dir().join(SDK_CACHE_FILE);
         if tokio::fs::create_dir_all(store_dir()).await.is_ok() {
            let _ = tokio::fs::write(path, &script).await;
         }
         Ok((script, sha))
      },
      Err(error) => {
         for file in [SDK_CACHE_FILE, "castle.js"] {
            if let Ok(script) = tokio::fs::read_to_string(store_dir().join(file)).await
               && !script.is_empty()
            {
               return Ok((script.clone(), sha256(script.as_bytes())));
            }
         }
         Err(error)
      },
   }
}

fn sha256(bytes: &[u8]) -> String {
   HEXLOWER.encode(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

async fn fetch_sdk() -> Result<String, String> {
   let client = reqwest::Client::builder()
      .user_agent(FETCH_UA)
      .timeout(Duration::from_secs(20))
      .build()
      .map_err(|error| error.to_string())?;
   let page = first_html(&client).await?;
   let Some(entry) = ENTRY_RE.find(&page).map(|found| found.as_str().to_owned()) else {
      return Err("x.com did not name the logged-out entry script".to_owned());
   };
   let entry_text = get_text(&client, &entry).await?;
   let mut assets = Vec::new();
   for found in ASSET_RE.find_iter(&entry_text) {
      let asset = found.as_str().to_owned();
      if !assets.contains(&asset) {
         assets.push(asset);
      }
      if assets.len() == 80 {
         break;
      }
   }
   let mut set = tokio::task::JoinSet::new();
   for asset in assets {
      let client = client.clone();
      set.spawn(async move {
         let url = format!("{XWEB}{asset}");
         let text = get_text(&client, &url).await.ok()?;
         if !text.contains("configure({pk:") {
            return None;
         }
         let at = text.find("configure({pk:")?;
         let window = &text[at.saturating_sub(400)..at];
         let name = CHUNK_RE.captures(window)?.get(1)?.as_str().to_owned();
         Some(name)
      });
   }
   let mut chunk = None;
   while let Some(joined) = set.join_next().await {
      if let Ok(Some(name)) = joined {
         chunk = Some(name);
         set.abort_all();
         break;
      }
   }
   let Some(name) = chunk else {
      return Err("could not find the castle chunk".to_owned());
   };
   let source = get_text(&client, &format!("{XWEB}assets/{name}")).await?;
   instrument_sdk(&source)
}

async fn first_html(client: &reqwest::Client) -> Result<String, String> {
   let mut last = "x.com fetch failed".to_owned();
   for url in ["https://x.com/i/flow/signup", "https://x.com/"] {
      match get_text(client, url).await {
         Ok(text) if ENTRY_RE.is_match(&text) => return Ok(text),
         Ok(_) => last = format!("{url} had no logged-out entry"),
         Err(error) => last = error,
      }
   }
   Err(last)
}

async fn get_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
   let response = client
      .get(url)
      .header(header::ACCEPT, "*/*")
      .header(header::ACCEPT_ENCODING, "identity")
      .send()
      .await
      .map_err(|error| error.to_string())?;
   if !response.status().is_success() {
      return Err(format!("{url} returned HTTP {}", response.status()));
   }
   response.text().await.map_err(|error| error.to_string())
}

fn instrument_sdk(src: &str) -> Result<String, String> {
   let start = src
      .find("import{t as e}")
      .ok_or("castle sdk has no runtime import")?;
   let mut body = src[start..].to_owned();
   let end = body.find(';').ok_or("castle sdk import does not end")?;
   body.replace_range(..=end, CJS_WRAPPER);
   let export = "export default t();";
   if !body.contains(export) {
      return Err("castle sdk has no default export".to_owned());
   }
   body = body.replacen(export, "globalThis.__r8x2=t();", 1);
   Ok(body)
}

#[cfg(test)]
mod tests {
   use super::{
      color_scheme_for,
      instrument_sdk,
      valid_id,
   };

   #[test]
   fn instruments_the_live_module_shape() {
      let src = "/* banner */ import.meta.url;import{t as e}from\"./y8ipOul8.js\";var \
                 t=function(){return 1};export default t();\n";
      let out = instrument_sdk(src).expect("instrument");
      assert!(out.starts_with("const __cjs="));
      assert!(out.contains("globalThis.__r8x2=t();"));
      assert!(!out.contains("export default"));
      assert!(!out.contains("import"));
   }

   #[test]
   fn accepts_only_store_ids() {
      assert!(valid_id("20261122T084500Z-ab12cd34"));
      assert!(!valid_id("../castle.js"));
      assert!(!valid_id("castle.js"));
      assert!(!valid_id("20261122T084500Z-ab12cd3"));
   }

   #[test]
   fn picks_matching_color_scheme_for_user_themes() {
      assert_eq!(color_scheme_for("Teapot"), "dark");
      assert_eq!(color_scheme_for("Dracula"), "dark");
      assert_eq!(color_scheme_for("Teapot Light"), "light");
      assert_eq!(color_scheme_for("Catppuccin Latte"), "light");
      assert_eq!(color_scheme_for("Rose Pine Dawn"), "light");
      assert_eq!(color_scheme_for("Twitter"), "light");
      assert_eq!(color_scheme_for("Twitter Dark"), "dark");
      assert_eq!(color_scheme_for("Auto"), "dark light");
   }

   #[test]
   fn instruments_a_cached_live_sdk() {
      let Ok(src) = std::fs::read_to_string("/tmp/xreg-castle-sdk.js") else {
         return;
      };
      let out = instrument_sdk(&src).expect("live sdk");
      assert!(out.contains("globalThis.__r8x2=t();"));
      assert!(!out.contains("export default"));
      assert!(!out.contains("import{"));
   }
}
