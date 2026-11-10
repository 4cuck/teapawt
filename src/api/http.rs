//! Outbound HTTP, presented as a real browser.
//!
//! Every request goes out through a [`primp`] client that carries a browser's
//! TLS and HTTP/2 fingerprint and headers. Requests made on behalf of an
//! account use a client built for that account's [`Identity`] and pinned
//! proxy, so one session always looks like one browser on one IP; anything
//! else (media, bootstrap fetches) uses a shared default client.

use std::{
   collections::{HashMap, HashSet},
   env,
   error::Error as _,
   fmt::Write as _,
   fs,
   hash::{Hash as _, Hasher as _},
   io::Write as _,
   path::{Path, PathBuf},
   pin::{Pin, pin},
   result::Result as StdResult,
   sync::{
      Arc, Mutex,
      atomic::{AtomicU64, AtomicUsize, Ordering},
   },
   task::{Context, Poll},
   time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use bytes::Bytes;
use futures_core::Stream;
use http_body::Frame;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::browser::{self, Engine, Identity};
use crate::error::{Error, Result};

const DEFAULT_BODY_LIMIT: usize = 32 * 1024 * 1024; // 32 MiB
/// Whole API call, connect to last byte.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// A media stream may run long; this is the cap on the whole transfer.
const MEDIA_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest silence tolerated mid-body.
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// Idle connections are kept this long. A session's connection to X is
/// multiplexed HTTP/2 and pinged, so a dead one is noticed rather than hung on.
/// Matching X's 15-minute window keeps active accounts' connections warm
/// across an entire window instead of paying a cold TLS/SOCKS handshake after
/// five minutes.
const POOL_IDLE: Duration = Duration::from_secs(900);
/// A few idle sockets per account. Hundreds of them, times every account on
/// one exit, fills the proxy and every later call waits on headers.
const POOL_PER_HOST: usize = 8;
/// CDN media normally answers in tens of milliseconds. A pooled connection the
/// path has already forgotten sits in TCP retransmits for several seconds and
/// then delivers, which is the video that "loads" after a long pause. Give up
/// on headers by then and open another connection.
const MEDIA_HEADER_DEADLINE: Duration = Duration::from_secs(2);
/// X answers a healthy GraphQL call in well under a second. A pooled HTTP/2
/// connection the proxy has already dropped sits until the gateway gives up
/// (about five seconds) and then returns 503 "remote refused stream reset".
/// Abandon headers by now and open another connection.
const API_HEADER_DEADLINE: Duration = Duration::from_millis(1500);
/// `scrape-socks.py` reaps a tunnel after a quiet minute. Pings have to land
/// well inside that window. A 45s ping left every idle session's socket
/// already closed, and the next visitor waited out the header deadline on it.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(5);
/// After an HTML 429, skip the direct address this long and send every
/// request through the fallback exits.
const DIRECT_BLOCK_FOR: Duration = Duration::from_secs(5 * 60);
/// An X/Cloudflare HTML 429 is tied to the public IP, not the account. Park
/// that SOCKS exit long enough for the edge window to clear, longer each time
/// it is throttled again soon after coming back: a flat five minutes put
/// 1101 and 1103 back in rotation to be throttled about 1,100 times each in
/// one evening.
const EDGE_PARKING: Parking = Parking {
   first: Duration::from_mins(5),
   max: Duration::from_hours(2),
   memory: Duration::from_mins(30),
};
/// An exit X answered a call through this recently is not IP-blocked, so a
/// bare 429 on it is about the account. Without this, one refused account
/// parked an exit 53 other accounts were still being served through.
const EXIT_PROOF: Duration = Duration::from_secs(5);
/// Headers later than this count against the exit that carried the call.
const SLOW_HEADERS: Duration = Duration::from_secs(1);
/// A call dropped unanswered this late, because another copy answered
/// first, counts against its exit too.
const ABANDONED_SLOW: Duration = Duration::from_millis(700);
/// An exit's answers are counted in two buckets of this length.
const HEALTH_BUCKET_MS: u64 = 30_000;
/// Slow answers an exit needs in its window before it can be parked.
const SLOW_EXIT_MIN: u32 = 8;
/// Share of the exit's answers that have to be slow. A healthy exit runs a
/// few percent; one X slow-walks ran about half for a whole day.
const SLOW_EXIT_SHARE: f64 = 0.25;
/// And that share against the fleet's median. When X is slow for every exit,
/// none of them is the problem.
const SLOW_EXIT_VS_FLEET: f64 = 3.0;
/// Parking for slowness, doubling while the exit keeps relapsing.
const SLOW_PARKING: Parking = Parking {
   first: Duration::from_mins(2),
   max: Duration::from_mins(30),
   memory: Duration::from_mins(10),
};

/// How long an exit stays out, by how often it has come back and failed
/// again: `first`, doubling per relapse up to `max`. A relapse is a new
/// parking within `memory` of the last one ending.
#[derive(Clone, Copy, Debug)]
struct Parking {
   first: Duration,
   max: Duration,
   memory: Duration,
}

impl Parking {
   /// Relapse count and parking length for a new parking at `now`, given the
   /// last parking's count and end.
   fn next(&self, (count, last_until): (u32, u64), now: u64) -> (u32, Duration) {
      let memory = u64::try_from(self.memory.as_millis()).unwrap_or(u64::MAX);
      let count = if last_until.saturating_add(memory) > now {
         count.saturating_add(1)
      } else {
         0
      };
      let park = self
         .first
         .saturating_mul(1_u32 << count.min(8))
         .min(self.max);
      (count, park)
   }
}
/// Exits that stay in rotation on a pool this size or smaller. The tests use
/// a handful of ports and still park one.
const SMALL_POOL: usize = 4;
/// Exits Cloudflare has blocked by IP. A header stall does not belong here.
const BANNED_PROXIES_FILE: &str = "disabled-proxies.txt";
/// How long a Cloudflare block keeps an exit out. Cloudflare lifts these on
/// its own, and a block page on the first call back writes a new line.
const CLOUDFLARE_BAN_FOR: Duration = Duration::from_hours(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyKind {
   Http,
   Socks5,
}

/// Parsed proxy configuration.
#[derive(Clone, Debug)]
pub struct ProxyConfig {
   pub host: String,
   pub port: u16,
   pub kind: ProxyKind,
   pub username: Option<String>,
   pub password: Option<String>,
}

impl ProxyConfig {
   /// Proxy URL in the form the client takes. SOCKS5 resolves the origin's
   /// name at the proxy, so X sees the exit's DNS rather than ours.
   #[must_use]
   pub fn url(&self) -> String {
      let scheme = match self.kind {
         ProxyKind::Http => "http",
         ProxyKind::Socks5 => "socks5h",
      };
      let mut url = format!("{scheme}://");
      if let Some(ref user) = self.username {
         url.push_str(
            &percent_encoding::utf8_percent_encode(user, percent_encoding::NON_ALPHANUMERIC)
               .to_string(),
         );
         if let Some(ref pass) = self.password {
            url.push(':');
            url.push_str(
               &percent_encoding::utf8_percent_encode(pass, percent_encoding::NON_ALPHANUMERIC)
                  .to_string(),
            );
         }
         url.push('@');
      }
      let _ = write!(url, "{}:{}", self.host, self.port);
      url
   }
}

/// Who a request goes out as: the account whose browser it presents, and the
/// proxy that account is pinned to.
#[derive(Clone, Debug)]
pub struct Egress {
   pub session: i64,
   pub proxy: Option<ProxyConfig>,
}

/// What a client's requests are for, which decides the fetch-context headers
/// a browser would send with them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
   /// XHR from x.com's web app to its API.
   Api,
   /// Images and video pulled from X's CDN by a page.
   Media,
   /// The web client's own script chunks, loaded by x.com from abs.twimg.com.
   Script,
}

impl Purpose {
   const fn timeout(self) -> Duration {
      match self {
         Self::Api => REQUEST_TIMEOUT,
         Self::Media | Self::Script => MEDIA_TIMEOUT,
      }
   }
}

/// Proxy exits used in turn, one per request, after the direct address is
/// blocked.
struct FallbackPool {
   proxies: Vec<ProxyConfig>,
   /// [`proxy_tag`] of every exit, to recognise them when picking a client.
   tags: HashSet<u64>,
   next: AtomicUsize,
}

impl FallbackPool {
   fn next(&self) -> ProxyConfig {
      let index = self.next.fetch_add(1, Ordering::Relaxed);
      self.proxies[index % self.proxies.len()].clone()
   }
}

/// One exit's recent answers, in two half-minute buckets so the count always
/// covers the last thirty to sixty seconds.
#[derive(Clone, Copy, Default)]
struct ExitSamples {
   bucket: u64,
   fast: [u32; 2],
   slow: [u32; 2],
}

impl ExitSamples {
   const fn roll(&mut self, now: u64) {
      let bucket = now / HEALTH_BUCKET_MS;
      match bucket.saturating_sub(self.bucket) {
         0 => {},
         1 => {
            self.fast = [0, self.fast[0]];
            self.slow = [0, self.slow[0]];
         },
         _ => {
            self.fast = [0, 0];
            self.slow = [0, 0];
         },
      }
      self.bucket = bucket;
   }

   /// Fast and slow answers in the window.
   const fn totals(&self) -> (u32, u32) {
      (
         self.fast[0].saturating_add(self.fast[1]),
         self.slow[0].saturating_add(self.slow[1]),
      )
   }
}

/// The stable SOCKS exits used by cookie sessions. A session normally stays
/// pinned, but temporarily moves to another healthy exit when X rate-limits
/// its public IP or answers it far slower than the other exits.
struct PrimaryPool {
   proxies: Vec<ProxyConfig>,
   tags: HashSet<u64>,
   blocked_until: Mutex<HashMap<u64, u64>>,
   samples: Mutex<HashMap<u64, ExitSamples>>,
   /// Parkings in a row per exit, and when the last one ends: for slowness,
   /// and for X's edge throttling its IP. Kept apart so a slow spell does not
   /// lengthen the next throttle parking or the other way round.
   relapses: Mutex<HashMap<u64, (u32, u64)>>,
   edge_relapses: Mutex<HashMap<u64, (u32, u64)>>,
   /// When X last answered a call through each exit with a success.
   served: Mutex<HashMap<u64, u64>>,
   /// Exits named in the ban file: a Cloudflare block until it lapses, any
   /// other line until it is deleted. Kept across restarts.
   banned: Mutex<HashSet<u64>>,
   banned_file: Option<PathBuf>,
}

impl PrimaryPool {
   fn new(proxies: Vec<ProxyConfig>, tags: HashSet<u64>) -> Self {
      Self {
         proxies,
         tags,
         blocked_until: Mutex::new(HashMap::new()),
         samples: Mutex::new(HashMap::new()),
         relapses: Mutex::new(HashMap::new()),
         edge_relapses: Mutex::new(HashMap::new()),
         served: Mutex::new(HashMap::new()),
         banned: Mutex::new(HashSet::new()),
         banned_file: None,
      }
   }

   fn watch_banned_file(&mut self, path: PathBuf) {
      let loaded = load_banned(&path, &self.proxies, unix_millis());
      self.banned = Mutex::new(loaded);
      self.banned_file = Some(path);
   }

   /// Pick up handshake bans written while teapot is running, and remove the
   /// lines of Cloudflare bans that have lapsed. Deleting a line puts that
   /// port back without a restart.
   fn reload_banned_file(&self) {
      let Some(path) = &self.banned_file else {
         return;
      };
      // Held across the file so a ban written meanwhile is neither lost from
      // the file nor dropped from the set.
      let Ok(mut banned) = self.banned.lock() else {
         return;
      };
      let now = unix_millis();
      drop_lapsed_bans(path, now);
      let loaded = load_banned(path, &self.proxies, now);
      if *banned == loaded {
         return;
      }
      *banned = loaded;
      tracing::warn!(
         ports = ?self.banned_ports_locked(&banned),
         file = %path.display(),
         "proxy ban file changed"
      );
   }

   fn banned_ports_locked(&self, banned: &HashSet<u64>) -> Vec<u16> {
      let mut ports = self
         .proxies
         .iter()
         .filter(|proxy| banned.contains(&proxy_tag(Some(proxy))))
         .map(|proxy| proxy.port)
         .collect::<Vec<_>>();
      ports.sort_unstable();
      ports
   }

   fn banned_tags(&self) -> HashSet<u64> {
      self.banned
         .lock()
         .map(|banned| banned.clone())
         .unwrap_or_default()
   }

   fn banned_ports(&self) -> Vec<u16> {
      let banned = self.banned_tags();
      let mut ports = self
         .proxies
         .iter()
         .filter(|proxy| banned.contains(&proxy_tag(Some(proxy))))
         .map(|proxy| proxy.port)
         .collect::<Vec<_>>();
      ports.sort_unstable();
      ports
   }

   /// Cloudflare blocked this exit's IP. It stays out, across restarts, for
   /// [`CLOUDFLARE_BAN_FOR`]. A header stall must not call this.
   fn ban(&self, proxy: &ProxyConfig) -> bool {
      let tag = proxy_tag(Some(proxy));
      if !self.tags.contains(&tag) {
         return false;
      }
      let Ok(mut banned) = self.banned.lock() else {
         return false;
      };
      if !banned.insert(tag) {
         return false;
      }
      let Some(path) = &self.banned_file else {
         return true;
      };
      let fresh = !path.exists();
      let mut line = String::new();
      if fresh {
         let _ = writeln!(
            line,
            "# Exits kept out of rotation. A cloudflare line is removed {} hours after its time; \
             any other line stays until it is deleted.",
            CLOUDFLARE_BAN_FOR.as_secs() / 3600
         );
      }
      let _ = writeln!(line, "{} {} cloudflare", proxy.port, utc_stamp(unix_millis()));
      if let Err(err) = fs::OpenOptions::new()
         .create(true)
         .append(true)
         .open(path)
         .and_then(|mut file| file.write_all(line.as_bytes()))
      {
         tracing::warn!(port = proxy.port, %err, "could not record the banned proxy exit");
      }
      true
   }

   /// How many exits stay reachable. Search sent through one or two IPs is
   /// what X's edge answers with 429. A header stall is not a reason to take
   /// an exit out for good: X being slow looks the same as a dead port.
   fn must_stay(&self) -> usize {
      let count = self.proxies.len();
      if count <= SMALL_POOL {
         1
      } else {
         (count * 2 / 3).max(3)
      }
   }

   fn out_cap(&self) -> usize {
      self.proxies.len().saturating_sub(self.must_stay())
   }

   /// Count one call against `proxy`: answered, or slow (late headers, no
   /// headers, no connection). Returns the parking time when this call made
   /// the exit one to avoid.
   fn record(&self, proxy: &ProxyConfig, slow: bool) -> Option<Duration> {
      let tag = proxy_tag(Some(proxy));
      if !self.tags.contains(&tag) {
         return None;
      }
      let now = unix_millis();
      let mut samples = self.samples.lock().ok()?;
      let mine = samples.entry(tag).or_default();
      mine.roll(now);
      if slow {
         mine.slow[0] = mine.slow[0].saturating_add(1);
      } else {
         mine.fast[0] = mine.fast[0].saturating_add(1);
         return None;
      }
      let (fast, slow_count) = mine.totals();
      let share = f64::from(slow_count) / f64::from(fast.saturating_add(slow_count));
      if slow_count < SLOW_EXIT_MIN || share < SLOW_EXIT_SHARE {
         return None;
      }
      let mut shares = samples
         .values_mut()
         .filter_map(|other| {
            other.roll(now);
            let (quick, late) = other.totals();
            let total = quick.saturating_add(late);
            (total >= SLOW_EXIT_MIN).then(|| f64::from(late) / f64::from(total))
         })
         .collect::<Vec<_>>();
      shares.sort_by(f64::total_cmp);
      let median = shares.get(shares.len() / 2).copied().unwrap_or(0.0);
      if share < SLOW_EXIT_VS_FLEET * median {
         return None;
      }
      samples.insert(tag, ExitSamples::default());
      drop(samples);
      self.park_slow(tag, now)
   }

   fn park_slow(&self, tag: u64, now: u64) -> Option<Duration> {
      let banned = self.banned_tags().len();
      let mut blocked = self.blocked_until.lock().ok()?;
      blocked.retain(|_, until| *until > now);
      // Parked already, or so many are out that search would share too few IPs.
      // Held across the decision so one slow burst parks once.
      if blocked.contains_key(&tag) || blocked.len() + banned >= self.out_cap() {
         return None;
      }
      let (park, until) = Self::next_parking(&self.relapses, &SLOW_PARKING, tag, now)?;
      blocked.insert(tag, until);
      Some(park)
   }

   /// How long to park this exit under `parking`, given its `history` of
   /// parkings of the same kind, and the moment that ends.
   fn next_parking(
      history: &Mutex<HashMap<u64, (u32, u64)>>,
      parking: &Parking,
      tag: u64,
      now: u64,
   ) -> Option<(Duration, u64)> {
      let mut relapses = history.lock().ok()?;
      let last = relapses.get(&tag).copied().unwrap_or((0, 0));
      let (count, park) = parking.next(last, now);
      let until = now.saturating_add(u64::try_from(park.as_millis()).unwrap_or(u64::MAX));
      relapses.insert(tag, (count, until));
      drop(relapses);
      Some((park, until))
   }

   /// Parked exits by port, with the seconds each has left.
   fn parked(&self) -> Vec<(u16, u64)> {
      let now = unix_millis();
      let Ok(blocked) = self.blocked_until.lock() else {
         return Vec::new();
      };
      let mut parked = self
         .proxies
         .iter()
         .filter_map(|proxy| {
            let until = *blocked.get(&proxy_tag(Some(proxy)))?;
            (until > now).then(|| (proxy.port, (until - now) / 1000))
         })
         .collect::<Vec<_>>();
      drop(blocked);
      parked.sort_unstable();
      parked
   }

   fn contains(&self, proxy: &ProxyConfig) -> bool {
      self.tags.contains(&proxy_tag(Some(proxy)))
   }

   fn is_blocked(&self, proxy: &ProxyConfig) -> bool {
      let tag = proxy_tag(Some(proxy));
      if self.banned_tags().contains(&tag) {
         return true;
      }
      self
         .blocked_until
         .lock()
         .ok()
         .and_then(|blocked| blocked.get(&tag).copied())
         .is_some_and(|until| until > unix_millis())
   }

   /// Parks the exit after X's edge throttled its IP. Returns how long only
   /// for the report that opened the parking, so a burst of in-flight
   /// responses produces one warning rather than hundreds and counts as one
   /// relapse. Refuses a new parking once too few exits would remain.
   fn block(&self, proxy: &ProxyConfig) -> Option<Duration> {
      let tag = proxy_tag(Some(proxy));
      if !self.tags.contains(&tag) {
         return None;
      }
      let now = unix_millis();
      let banned = self.banned_tags().len();
      let mut blocked = self.blocked_until.lock().ok()?;
      blocked.retain(|_, ends| *ends > now);
      if blocked.contains_key(&tag) || blocked.len() + banned >= self.out_cap() {
         return None;
      }
      let (park, until) = Self::next_parking(&self.edge_relapses, &EDGE_PARKING, tag, now)?;
      blocked.insert(tag, until);
      Some(park)
   }

   /// X answered a call through this exit with a success.
   fn note_served(&self, proxy: &ProxyConfig) {
      let tag = proxy_tag(Some(proxy));
      if !self.tags.contains(&tag) {
         return;
      }
      if let Ok(mut served) = self.served.lock() {
         served.insert(tag, unix_millis());
      }
   }

   /// Whether X served a call through this exit within [`EXIT_PROOF`]. Its IP
   /// is then not what X is refusing.
   fn recently_served(&self, proxy: &ProxyConfig) -> bool {
      let proof = u64::try_from(EXIT_PROOF.as_millis()).unwrap_or(u64::MAX);
      let since = unix_millis().saturating_sub(proof);
      self
         .served
         .lock()
         .ok()
         .and_then(|served| served.get(&proxy_tag(Some(proxy))).copied())
         .is_some_and(|at| at >= since)
   }

   /// Pick a deterministic healthy failover for this account. Stability keeps
   /// one temporary IP per account and avoids building clients for every exit.
   fn alternate(&self, session: i64, current: &ProxyConfig) -> Option<ProxyConfig> {
      let now = unix_millis();
      let current_tag = proxy_tag(Some(current));
      let banned = self.banned_tags();
      let mut blocked = self.blocked_until.lock().ok()?;
      blocked.retain(|_, until| *until > now);
      let healthy = self
         .proxies
         .iter()
         .filter(|proxy| {
            let tag = proxy_tag(Some(proxy));
            tag != current_tag && !blocked.contains_key(&tag) && !banned.contains(&tag)
         })
         .collect::<Vec<_>>();
      if healthy.is_empty() {
         return None;
      }
      let mut hasher = std::collections::hash_map::DefaultHasher::new();
      session.hash(&mut hasher);
      current_tag.hash(&mut hasher);
      let index = (hasher.finish() as usize) % healthy.len();
      Some(healthy[index].clone())
   }
}

/// Browser-impersonating client with one connection pool per account.
#[derive(Clone)]
#[expect(
   clippy::module_name_repetitions,
   reason = "HttpClient is clearer than Client"
)]
pub struct HttpClient {
   purpose: Purpose,
   default: primp::Client,
   default_proxy: Option<ProxyConfig>,
   /// Used only after the direct exit returns an HTML 429, or while that
   /// block is still fresh.
   fallback_proxy: Option<ProxyConfig>,
   /// Round-robin exits for the same window. Each call takes the next one.
   fallback_pool: Option<Arc<FallbackPool>>,
   /// Stable session exits plus the shared circuit breaker for an IP that X
   /// temporarily rate-limited.
   primary_pool: Option<Arc<PrimaryPool>>,
   /// Unix millis until which the direct exit is skipped.
   direct_blocked_until: Arc<AtomicU64>,
   extra_headers: HeaderMap,
   /// Keyed by session and exit so a direct client is never reused for the
   /// fallback proxy.
   sessions: Arc<Mutex<HashMap<(i64, u64), primp::Client>>>,
}

/// Response wrapper providing convenience methods.
pub struct Response {
   inner: primp::Response,
}

impl HttpClient {
   /// A client for `purpose`, optionally through an HTTP proxy from the
   /// config. Requests without an [`Egress`] present as the default identity:
   /// the newest accepted Chrome on Windows.
   pub fn new(proxy_url: &str, proxy_auth: &str, purpose: Purpose) -> Self {
      let default_proxy = (!proxy_url.is_empty()).then(|| parse_proxy(proxy_url, proxy_auth));
      let extra_headers = HeaderMap::new();
      let default = build_client(
         default_identity(),
         default_proxy.as_ref(),
         purpose,
         &extra_headers,
         POOL_PER_HOST,
      )
      .unwrap_or_else(|err| {
         tracing::error!("browser client for the default identity failed to build: {err}");
         // Without impersonation the client is still a working HTTP client.
         primp::Client::builder()
            .redirect(primp::redirect::Policy::none())
            .timeout(purpose.timeout())
            .build()
            .expect("plain HTTP client builds without I/O")
      });
      Self {
         purpose,
         default,
         default_proxy,
         fallback_proxy: None,
         fallback_pool: None,
         primary_pool: None,
         direct_blocked_until: Arc::new(AtomicU64::new(0)),
         extra_headers,
         sessions: Arc::new(Mutex::new(HashMap::new())),
      }
   }

   /// Proxy used only when a direct request is answered with an HTML 429.
   #[must_use]
   pub fn with_fallback_proxy(mut self, proxy_url: &str, proxy_auth: &str) -> Self {
      if !proxy_url.is_empty() {
         let proxy = parse_proxy(proxy_url, proxy_auth);
         tracing::info!(
            host = %proxy.host,
            port = proxy.port,
            "residential proxy is the fallback for HTML 429s"
         );
         self.fallback_proxy = Some(proxy);
      }
      self
   }

   /// Exits used, one after another, while the direct address is blocked.
   #[must_use]
   pub fn with_fallback_proxies(mut self, proxies: Vec<ProxyConfig>) -> Self {
      if !proxies.is_empty() {
         tracing::info!(
            proxies = proxies.len(),
            "proxy exits are the fallback for HTML 429s"
         );
         let tags = proxies.iter().map(|proxy| proxy_tag(Some(proxy))).collect();
         self.fallback_pool = Some(Arc::new(FallbackPool {
            proxies,
            tags,
            next: AtomicUsize::new(0),
         }));
      }
      self
   }

   /// Register all stable session exits so one Cloudflare 1015 can park only
   /// the affected port while requests continue through the healthy ports.
   #[must_use]
   pub fn with_primary_proxies(mut self, mut proxies: Vec<ProxyConfig>) -> Self {
      let mut tags = HashSet::new();
      proxies.retain(|proxy| tags.insert(proxy_tag(Some(proxy))));
      if !proxies.is_empty() {
         tracing::info!(
            exits = proxies.len(),
            first_cooldown_secs = EDGE_PARKING.first.as_secs(),
            max_cooldown_secs = EDGE_PARKING.max.as_secs(),
            "proxy exit circuit breaker ready"
         );
         let mut pool = PrimaryPool::new(proxies, tags);
         if let Some(path) = banned_proxies_file() {
            pool.watch_banned_file(path.clone());
            let banned = pool.banned_ports();
            if !banned.is_empty() {
               tracing::warn!(
                  ports = ?banned,
                  file = %path.display(),
                  "proxy exits staying out because the ban file says so"
               );
            }
         }
         let pool = Arc::new(pool);
         if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let refresh = Arc::clone(&pool);
            handle.spawn(async move {
               let mut tick = tokio::time::interval(Duration::from_secs(30));
               loop {
                  tick.tick().await;
                  refresh.reload_banned_file();
               }
            });
         }
         self.primary_pool = Some(pool);
      }
      self
   }

   /// Primary exits parked right now, by port, with the seconds each has left.
   pub fn parked_exits(&self) -> Vec<(u16, u64)> {
      self
         .primary_pool
         .as_ref()
         .map_or_else(Vec::new, |pool| pool.parked())
   }

   /// Cloudflare blocked this exit's IP. Header stalls must not call this.
   pub fn ban_exit(&self, proxy: &ProxyConfig) -> bool {
      self.primary_pool
         .as_ref()
         .is_some_and(|pool| pool.ban(proxy))
   }

   /// Whether X served a call through this exit within [`EXIT_PROOF`]. A bare
   /// 429 on such an exit is the account's, and the exit stays in rotation.
   /// A parked exit is judged by the failover its account is sent through.
   pub fn exit_still_served(&self, egress: &Egress) -> bool {
      let failover = self
         .primary_is_blocked(egress)
         .then(|| self.alternate_primary(egress))
         .flatten();
      let Some(proxy) = failover.as_ref().unwrap_or(egress).proxy.as_ref() else {
         return false;
      };
      self
         .primary_pool
         .as_ref()
         .is_some_and(|pool| pool.recently_served(proxy))
   }

   fn note_answer(&self, via: Option<&Egress>, answer: &Result<Response>) {
      if let Ok(response) = answer
         && response.status().is_success()
         && let Some(proxy) = via.and_then(|egress| egress.proxy.as_ref())
         && let Some(pool) = self.primary_pool.as_ref()
      {
         pool.note_served(proxy);
      }
   }

   fn note_exit(&self, via: Option<&Egress>, slow: bool) {
      let Some(pool) = self.primary_pool.as_ref() else {
         return;
      };
      let Some(proxy) = via.and_then(|egress| egress.proxy.as_ref()) else {
         return;
      };
      if let Some(park) = pool.record(proxy, slow) {
         tracing::warn!(
            port = proxy.port,
            park_secs = park.as_secs(),
            "X answers this proxy exit far slower than the rest; parking it"
         );
      }
   }

   fn has_fallback(&self) -> bool {
      self.fallback_proxy.is_some() || self.fallback_pool.is_some()
   }

   fn take_fallback(&self) -> Option<ProxyConfig> {
      if let Some(pool) = &self.fallback_pool {
         return Some(pool.next());
      }
      self.fallback_proxy.clone()
   }

   /// Headers added to every request this client makes.
   #[must_use]
   pub fn with_default_headers(mut self, headers: HeaderMap) -> Self {
      self.extra_headers = headers;
      self.rebuild_default();
      self
   }

   /// Route requests made without an [`Egress`] through `proxy`.
   #[must_use]
   pub fn with_default_proxy(mut self, proxy: ProxyConfig) -> Self {
      self.default_proxy = Some(proxy);
      self.rebuild_default();
      self
   }

   fn rebuild_default(&mut self) {
      match build_client(
         default_identity(),
         self.default_proxy.as_ref(),
         self.purpose,
         &self.extra_headers,
         POOL_PER_HOST,
      ) {
         Ok(client) => self.default = client,
         Err(err) => {
            tracing::error!("browser client for the default identity failed to build: {err}")
         },
      }
   }

   /// The client for an account, built on first use and kept for the life of
   /// the process, so its connections and fingerprint persist.
   ///
   /// A request through the round-robin pool lands on a different exit each
   /// time, so those clients belong to the browser rather than the account:
   /// every account presenting that browser shares them. Kept per account,
   /// each one would gather a client per exit and reuse almost none.
   fn client_key(&self, egress: &Egress) -> (i64, u64) {
      let identity = browser::identity_for(egress.session);
      let tag = proxy_tag(egress.proxy.as_ref());
      let pooled = self
         .fallback_pool
         .as_ref()
         .is_some_and(|pool| pool.tags.contains(&tag));
      let owner = if pooled {
         identity_owner(identity)
      } else {
         egress.session
      };
      (owner, tag)
   }

   /// Drop the pooled connection for this account so the next call dials a
   /// new one. A wedged HTTP/2 session otherwise accepts the next stream and
   /// stalls it until the proxy resets it.
   pub(crate) fn retire(&self, via: Option<&Egress>) {
      let Some(egress) = via else {
         return;
      };
      let key = self.client_key(egress);
      if let Ok(mut clients) = self.sessions.lock() {
         clients.remove(&key);
      }
   }

   fn client_for(&self, via: Option<&Egress>) -> Result<primp::Client> {
      let Some(egress) = via else {
         return Ok(self.default.clone());
      };
      let key = self.client_key(egress);
      if let Some(client) = self
         .sessions
         .lock()
         .ok()
         .and_then(|clients| clients.get(&key).cloned())
      {
         return Ok(client);
      }
      let idle = if self.is_fallback(egress.proxy.as_ref()) {
         // A new connection asks the residential proxy for a new exit IP.
         0
      } else {
         POOL_PER_HOST
      };
      let identity = browser::identity_for(egress.session);
      let client = build_client(
         identity,
         egress.proxy.as_ref(),
         self.purpose,
         &self.extra_headers,
         idle,
      )?;
      tracing::debug!(session_id = egress.session, %identity, "browser client built");
      if let Ok(mut clients) = self.sessions.lock() {
         clients.entry(key).or_insert_with(|| client.clone());
      }
      Ok(client)
   }

   fn is_fallback(&self, proxy: Option<&ProxyConfig>) -> bool {
      let Some(proxy) = proxy else {
         return false;
      };
      if self
         .fallback_pool
         .as_ref()
         .is_some_and(|pool| pool.tags.contains(&proxy_tag(Some(proxy))))
      {
         return true;
      }
      self
         .fallback_proxy
         .as_ref()
         .is_some_and(|fallback| proxy.host == fallback.host && proxy.port == fallback.port)
   }

   fn primary_is_blocked(&self, egress: &Egress) -> bool {
      let Some(proxy) = egress.proxy.as_ref() else {
         return false;
      };
      self
         .primary_pool
         .as_ref()
         .is_some_and(|pool| pool.is_blocked(proxy))
   }

   fn alternate_primary(&self, egress: &Egress) -> Option<Egress> {
      let proxy = egress.proxy.as_ref()?;
      let alternate = self
         .primary_pool
         .as_ref()?
         .alternate(egress.session, proxy)?;
      Some(Egress {
         session: egress.session,
         proxy: Some(alternate),
      })
   }

   /// Park a primary exit after an HTML 429. Returns the cooldown when this
   /// call opened a new one.
   fn block_primary(&self, egress: &Egress) -> Option<Duration> {
      let proxy = egress.proxy.as_ref()?;
      self.primary_pool.as_ref()?.block(proxy)
   }

   fn is_primary(&self, egress: &Egress) -> bool {
      let Some(proxy) = egress.proxy.as_ref() else {
         return false;
      };
      self
         .primary_pool
         .as_ref()
         .is_some_and(|pool| pool.contains(proxy))
   }

   fn direct_is_blocked(&self) -> bool {
      let until = self.direct_blocked_until.load(Ordering::Relaxed);
      until > unix_millis()
   }

   fn block_direct(&self) {
      let until = unix_millis().saturating_add(DIRECT_BLOCK_FOR.as_millis() as u64);
      self
         .direct_blocked_until
         .fetch_max(until, Ordering::Relaxed);
   }

   /// Send a GET request.
   pub async fn get(&self, uri: &str) -> Result<Response> {
      self
         .send_on(Method::GET, uri, &HeaderMap::new(), Bytes::new(), None)
         .await
   }

   /// Send a GET request with additional headers.
   pub async fn get_with_headers(&self, uri: &str, extra_headers: &HeaderMap) -> Result<Response> {
      self
         .send_on(Method::GET, uri, extra_headers, Bytes::new(), None)
         .await
   }

   /// GET as an account, through its pinned proxy.
   pub async fn get_on(
      &self,
      uri: &str,
      extra_headers: &HeaderMap,
      via: Option<&Egress>,
   ) -> Result<Response> {
      self
         .send_on(Method::GET, uri, extra_headers, Bytes::new(), via)
         .await
   }

   /// Send a POST request with additional headers and a body.
   pub async fn post_with_headers(
      &self,
      uri: &str,
      extra_headers: &HeaderMap,
      body: Bytes,
   ) -> Result<Response> {
      self
         .send_on(Method::POST, uri, extra_headers, body, None)
         .await
   }

   /// POST as an account, through its pinned proxy.
   pub async fn post_on(
      &self,
      uri: &str,
      extra_headers: &HeaderMap,
      body: Bytes,
      via: Option<&Egress>,
   ) -> Result<Response> {
      self
         .send_on(Method::POST, uri, extra_headers, body, via)
         .await
   }

   /// Send a HEAD request.
   pub async fn head(&self, uri: &str) -> Result<Response> {
      self
         .send_on(Method::HEAD, uri, &HeaderMap::new(), Bytes::new(), None)
         .await
   }

   async fn send_on(
      &self,
      method: Method,
      uri: &str,
      extra_headers: &HeaderMap,
      body: Bytes,
      via: Option<&Egress>,
   ) -> Result<Response> {
      let direct = via.is_some_and(|egress| egress.proxy.is_none());
      let skip_direct = direct && self.has_fallback() && self.direct_is_blocked();
      let routed = if skip_direct {
         Some(Egress {
            session: via.expect("direct egress").session,
            proxy: self.take_fallback(),
         })
      } else if let Some(egress) = via
         && self.primary_is_blocked(egress)
      {
         self.alternate_primary(egress)
      } else {
         None
      };
      let first_via = routed.as_ref().or(via);

      let first = self
         .dispatch(&method, uri, extra_headers, &body, first_via)
         .await;
      self.note_answer(first_via, &first);

      // An HTML 429 is a per-IP Cloudflare block. Park this exact proxy port
      // for every account and retry safe calls once through another healthy
      // primary exit. Future POSTs also avoid the parked exit, but this first
      // POST is not repeated because X may have applied it before answering.
      let edge_block =
         matches!(&first, Ok(response) if is_edge_block(response.status(), response.headers()));
      if edge_block
         && let Some(blocked_via) = first_via
         && self.is_primary(blocked_via)
      {
         // X is still serving other accounts through this IP, so it refused
         // the account. The caller rests the account; parking the exit and
         // resending the account elsewhere only spread its 429 to more exits.
         if self.exit_still_served(blocked_via) {
            return first;
         }
         let port = blocked_via
            .proxy
            .as_ref()
            .map(|proxy| proxy.port)
            .unwrap_or(0);
         if let Some(cooldown) = self.block_primary(blocked_via) {
            tracing::warn!(
               port,
               cooldown_secs = cooldown.as_secs(),
               "X edge rate-limited proxy exit; parking it"
            );
         }
         self.retire(Some(blocked_via));
         if is_idempotent(&method)
            && let Some(alternate) = self.alternate_primary(blocked_via)
         {
            let alternate_port = alternate
               .proxy
               .as_ref()
               .map(|proxy| proxy.port)
               .unwrap_or(0);
            tracing::warn!(
               port,
               alternate_port,
               "retrying through a healthy proxy exit"
            );
            let retry = self
               .dispatch(&method, uri, extra_headers, &body, Some(&alternate))
               .await;
            self.note_answer(Some(&alternate), &retry);
            if matches!(&retry, Ok(response) if is_edge_block(response.status(), response.headers()))
            {
               let retry_port = alternate_port;
               if let Some(cooldown) = self.block_primary(&alternate) {
                  tracing::warn!(
                     port = retry_port,
                     cooldown_secs = cooldown.as_secs(),
                     "X edge rate-limited replacement proxy exit; parking it"
                  );
               }
               self.retire(Some(&alternate));
            }
            return retry;
         }
         return first;
      }

      // Already on a proxy, or this client has no fallback exits.
      if !direct || !self.has_fallback() || skip_direct {
         return first;
      }
      let blocked = match &first {
         Ok(response) if is_edge_block(response.status(), response.headers()) => true,
         Err(err) if is_idempotent(&method) && is_direct_failure(err) => true,
         _ => false,
      };
      if !blocked {
         return first;
      }
      if matches!(&first, Ok(response) if is_edge_block(response.status(), response.headers())) {
         self.block_direct();
      }
      tracing::warn!(
         uri,
         "direct exit blocked, retrying through a fallback proxy"
      );
      let proxied = Egress {
         session: via.expect("direct egress").session,
         proxy: self.take_fallback(),
      };
      self
         .dispatch(&method, uri, extra_headers, &body, Some(&proxied))
         .await
   }

   async fn dispatch(
      &self,
      method: &Method,
      uri: &str,
      extra_headers: &HeaderMap,
      body: &Bytes,
      via: Option<&Egress>,
   ) -> Result<Response> {
      let client = self.client_for(via)?;
      let send = || {
         let mut request = client
            .request(method.clone(), uri)
            .headers(extra_headers.clone());
         if !body.is_empty() {
            request = request.body(body.clone());
         }
         request.send()
      };
      if self.purpose == Purpose::Media && is_idempotent(method) {
         let inner = match tokio::time::timeout(MEDIA_HEADER_DEADLINE, send()).await {
            Ok(Ok(inner)) => inner,
            Ok(Err(err)) if is_transport_blip(&err) || err.is_connect() => {
               tracing::warn!(
                  uri = loggable(uri),
                  error = %describe(err.without_url()),
                  "media fetch failed, retrying"
               );
               send().await.map_err(describe)?
            },
            Err(_elapsed) => {
               tracing::warn!(
                  uri = loggable(uri),
                  "media headers stalled, opening a new connection"
               );
               send().await.map_err(describe)?
            },
            Ok(Err(err)) => return Err(describe(err)),
         };
         return Ok(Response { inner });
      }

      if self.purpose == Purpose::Api && is_idempotent(method) {
         let probe = ExitProbe::start(self, via);
         let mut first = pin!(send());
         let inner = match tokio::time::timeout(API_HEADER_DEADLINE, first.as_mut()).await {
            Ok(Ok(inner)) => {
               probe.answered();
               inner
            },
            Ok(Err(err)) if is_transport_blip(&err) || err.is_connect() => {
               probe.failed();
               tracing::warn!(
                  uri = loggable(uri),
                  error = %describe(err.without_url()),
                  "API connection failed, opening another"
               );
               self
                  .fresh_headers(method, uri, extra_headers, body, via)
                  .await?
            },
            Err(_elapsed) => {
               probe.failed();
               tracing::warn!(
                  uri = loggable(uri),
                  "API headers stalled, opening another connection"
               );
               // Boxed: the race holds a second request future, and only a
               // stall pays for it instead of every caller's future.
               Box::pin(self.race_stalled(first, method, uri, extra_headers, body, via)).await?
            },
            Ok(Err(err)) => {
               probe.failed();
               return Err(describe(err));
            },
         };
         return Ok(Response { inner });
      }

      let inner = match send().await {
         Ok(inner) => inner,
         // A pooled connection the far end closed a moment ago fails on the
         // first write ("broken pipe", "connection reset") rather than with a
         // GOAWAY the client would retry itself. Safe to repeat for requests
         // that change nothing.
         Err(err) if is_idempotent(method) && is_transport_blip(&err) => {
            tracing::debug!(uri, "retrying after transport error: {}", describe(err));
            send().await.map_err(describe)?
         },
         Err(err) => return Err(describe(err)),
      };
      Ok(Response { inner })
   }

   /// One more attempt on a connection that is not the one that just failed.
   /// When the account is pinned to a primary proxy exit, the retry moves to a
   /// healthy alternate exit so a stalled or dead tunnel does not stall the
   /// retry for another header deadline.
   async fn fresh_headers(
      &self,
      method: &Method,
      uri: &str,
      extra_headers: &HeaderMap,
      body: &Bytes,
      via: Option<&Egress>,
   ) -> Result<primp::Response> {
      self.retire(via);
      let alternate = via.and_then(|egress| self.alternate_primary(egress));
      self
         .send_fresh(method, uri, extra_headers, body, alternate.as_ref().or(via))
         .await
   }

   /// Headers are late on the first connection. Another connection gets one
   /// more deadline, and the first request keeps running beside it: late
   /// headers are mostly X taking its time over a big page, and dropping that
   /// call only starts the same wait over. Whichever answers first is kept.
   async fn race_stalled<F>(
      &self,
      mut first: Pin<&mut F>,
      method: &Method,
      uri: &str,
      extra_headers: &HeaderMap,
      body: &Bytes,
      via: Option<&Egress>,
   ) -> Result<primp::Response>
   where
      F: Future<Output = primp::Result<primp::Response>>,
   {
      let alternate = via.and_then(|egress| self.alternate_primary(egress));
      if alternate.is_none() {
         // Same account on the same exit: only a new connection differs. The
         // request already on the old one keeps its own handle to it.
         self.retire(via);
      }
      let race = async {
         let mut fresh = pin!(self.send_fresh(
            method,
            uri,
            extra_headers,
            body,
            alternate.as_ref().or(via),
         ));
         let (mut first_done, mut fresh_done) = (false, false);
         // A 5xx or an error is held in case the other side does no better.
         // A 5xx goes back up as a response, which callers retry elsewhere.
         let mut fallback: Option<Result<primp::Response>> = None;
         while !(first_done && fresh_done) {
            tokio::select! {
               late = first.as_mut(), if !first_done => {
                  first_done = true;
                  match late {
                     Ok(inner) if !inner.status().is_server_error() => {
                        tracing::info!(
                           uri = loggable(uri),
                           "late headers arrived on the first connection"
                        );
                        return Ok(inner);
                     },
                     Ok(inner) => fallback = Some(Ok(inner)),
                     Err(err) => {
                        if fallback.is_none() {
                           fallback = Some(Err(describe(err)));
                        }
                     },
                  }
               },
               outcome = fresh.as_mut(), if !fresh_done => {
                  fresh_done = true;
                  match outcome {
                     Ok(inner) if !inner.status().is_server_error() => {
                        if alternate.is_some() {
                           // Silent past its deadline while another answered.
                           self.retire(via);
                        }
                        return Ok(inner);
                     },
                     Ok(inner) => fallback = Some(Ok(inner)),
                     Err(err) => {
                        if fallback.is_none() {
                           fallback = Some(Err(err));
                        }
                     },
                  }
               },
            }
         }
         fallback.unwrap_or_else(|| Err(Error::Internal("API headers stalled".into())))
      };
      if let Ok(outcome) = tokio::time::timeout(API_HEADER_DEADLINE, race).await {
         return outcome;
      }
      self.retire(via);
      if let Some(alternate) = alternate.as_ref() {
         self.retire(Some(alternate));
      }
      tracing::warn!(uri = loggable(uri), "API headers stalled again");
      Err(Error::Internal("API headers stalled".into()))
   }

   /// One request on the client for `retry_via`, counted against its exit.
   async fn send_fresh(
      &self,
      method: &Method,
      uri: &str,
      extra_headers: &HeaderMap,
      body: &Bytes,
      retry_via: Option<&Egress>,
   ) -> Result<primp::Response> {
      let client = self.client_for(retry_via)?;
      let mut request = client
         .request(method.clone(), uri)
         .headers(extra_headers.clone());
      if !body.is_empty() {
         request = request.body(body.clone());
      }
      let probe = ExitProbe::start(self, retry_via);
      match request.send().await {
         Ok(inner) => {
            probe.answered();
            Ok(inner)
         },
         Err(err) => {
            probe.failed();
            self.retire(retry_via);
            Err(describe(err))
         },
      }
   }
}

/// Counts one API call against the exit that carried it: when headers
/// arrive, when it fails, or when it is dropped unanswered because another
/// copy of the call answered first.
struct ExitProbe<'a> {
   client: &'a HttpClient,
   via: Option<&'a Egress>,
   started: Instant,
   settled: bool,
}

impl<'a> ExitProbe<'a> {
   fn start(client: &'a HttpClient, via: Option<&'a Egress>) -> Self {
      Self {
         client,
         via,
         started: Instant::now(),
         settled: false,
      }
   }

   fn answered(mut self) {
      let slow = self.started.elapsed() >= SLOW_HEADERS;
      self.settle(slow);
   }

   fn failed(mut self) {
      self.settle(true);
   }

   fn settle(&mut self, slow: bool) {
      self.settled = true;
      self.client.note_exit(self.via, slow);
   }
}

impl Drop for ExitProbe<'_> {
   fn drop(&mut self) {
      if !self.settled && self.started.elapsed() >= ABANDONED_SLOW {
         self.settle(true);
      }
   }
}

/// The browser presented when no account is involved.
fn default_identity() -> Identity {
   Identity {
      engine: Engine::Chrome,
      os: primp::ImpersonateOS::Windows,
   }
}

/// Header order from logged-in Chrome 152 sessions on x.com.
///
/// Names that a given request does not send are skipped, so a GraphQL GET
/// stays in the order of those calls and an image or video fetch stays in
/// the order of the CDN calls. A SearchTimeline GET sends this API order and
/// does not send `x-twitter-polling` or `origin`; those names stay in the
/// list for the calls that do.
fn wire_header_order(purpose: Purpose) -> Vec<HeaderName> {
   let names: &[&str] = match purpose {
      Purpose::Api => &[
         "sec-gpc",
         "sec-ch-ua-platform",
         "authorization",
         "x-csrf-token",
         "sec-ch-ua",
         "x-twitter-client-language",
         "sec-ch-ua-mobile",
         "x-twitter-active-user",
         "x-client-transaction-id",
         "x-twitter-auth-type",
         "user-agent",
         "dnt",
         "x-twitter-polling",
         "content-type",
         "accept",
         "origin",
         "sec-fetch-site",
         "sec-fetch-mode",
         "sec-fetch-dest",
         "referer",
         "accept-encoding",
         "accept-language",
         "cookie",
         "priority",
      ],
      Purpose::Media => &[
         "sec-gpc",
         "sec-ch-ua-platform",
         "user-agent",
         "sec-ch-ua",
         "dnt",
         "sec-ch-ua-mobile",
         "accept",
         "origin",
         "sec-fetch-site",
         "sec-fetch-mode",
         "sec-fetch-dest",
         "sec-fetch-storage-access",
         "referer",
         "accept-encoding",
         "accept-language",
         "range",
         "if-range",
         "cookie",
         "priority",
      ],
      Purpose::Script => &[
         "sec-gpc",
         "origin",
         "sec-ch-ua-platform",
         "user-agent",
         "sec-ch-ua",
         "dnt",
         "sec-ch-ua-mobile",
         "accept",
         "sec-fetch-site",
         "sec-fetch-mode",
         "sec-fetch-dest",
         "referer",
         "accept-encoding",
         "accept-language",
         "cookie",
         "priority",
      ],
   };
   names
      .iter()
      .map(|name| HeaderName::from_static(name))
      .collect()
}

fn build_client(
   identity: Identity,
   proxy: Option<&ProxyConfig>,
   purpose: Purpose,
   extra_headers: &HeaderMap,
   idle_per_host: usize,
) -> Result<primp::Client> {
   // Images and video each get their own HTTP/1 connection, and none are
   // kept afterwards. One shared HTTP/2 session to video.twimg.com was
   // stalling every later fetch for seconds once the CDN had dropped it.
   let media = purpose == Purpose::Media;
   let mut builder = primp::Client::builder()
      .impersonate(identity.profile())
      .impersonate_os(identity.os)
      // Redirects are the caller's business: t.co resolution reads them.
      .redirect(primp::redirect::Policy::none())
      .timeout(purpose.timeout())
      .read_timeout(BODY_IDLE_TIMEOUT)
      .pool_idle_timeout(if media { Duration::from_secs(1) } else { POOL_IDLE })
      .pool_max_idle_per_host(if media { 0 } else { idle_per_host })
      .http2_keep_alive_interval(KEEP_ALIVE_INTERVAL)
      .http2_keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
      .http2_keep_alive_while_idle(true)
      .tcp_keepalive(Duration::from_secs(10))
      .tcp_nodelay(true)
      // Impersonation installs a document-navigation order. x.com's own calls
      // use a different order, captured from Chrome 152, and headers missing
      // from that list are appended after `priority`.
      .http2_headers_order(wire_header_order(purpose));
   if media {
      builder = builder
         .http1_only()
         .connect_timeout(Duration::from_secs(3));
   }
   builder =
      match proxy {
         Some(proxy) => builder.proxy(primp::Proxy::all(proxy.url()).map_err(|err| {
            Error::Internal(format!("proxy {}:{}: {err}", proxy.host, proxy.port))
         })?),
         None => builder.no_proxy(),
      };
   let mut client = builder
      .build()
      .map_err(|err| Error::Internal(format!("browser client for {identity}: {err}")))?;
   shape_headers(client.headers_mut(), purpose, extra_headers);
   Ok(client)
}

/// Turn the profile's page-navigation headers into the set a browser sends
/// with the kind of request this client makes.
///
/// The profile ships the headers of a top-level page load. An XHR to the API
/// or an image fetch carries the same identity headers (`user-agent`,
/// `sec-ch-ua*`, `accept-language`, `accept-encoding`) but a different fetch
/// context, and never `upgrade-insecure-requests` or `sec-fetch-user`. Keys
/// are only rewritten where the profile sent them, so Safari, which sends no
/// `sec-fetch-*` at all, stays Safari.
fn shape_headers(headers: &mut HeaderMap, purpose: Purpose, extra: &HeaderMap) {
   headers.remove("upgrade-insecure-requests");
   headers.remove("sec-fetch-user");

   let (accept, dest, mode, site, priority) = match purpose {
      Purpose::Api => ("*/*", "empty", "cors", "same-origin", Some("u=1, i")),
      Purpose::Media => (
         "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
         "image",
         "no-cors",
         "cross-site",
         Some("u=1, i"),
      ),
      // A chunk webpack loads on demand carries no priority.
      Purpose::Script => ("*/*", "script", "cors", "cross-site", None),
   };
   for (name, value) in [
      (header::ACCEPT, accept),
      (header::HeaderName::from_static("sec-fetch-dest"), dest),
      (header::HeaderName::from_static("sec-fetch-mode"), mode),
      (header::HeaderName::from_static("sec-fetch-site"), site),
   ] {
      if headers.contains_key(&name) {
         headers.insert(name, HeaderValue::from_static(value));
      }
   }
   let priority_name = header::HeaderName::from_static("priority");
   match priority {
      Some(value) if headers.contains_key(&priority_name) => {
         headers.insert(priority_name, HeaderValue::from_static(value));
      },
      Some(_) => {},
      None => {
         headers.remove(&priority_name);
      },
   }
   match purpose {
      Purpose::Api => {
         headers.insert(header::REFERER, HeaderValue::from_static("https://x.com/"));
      },
      Purpose::Script => {
         headers.insert(header::ORIGIN, HeaderValue::from_static("https://x.com"));
         headers.insert(header::REFERER, HeaderValue::from_static("https://x.com/"));
      },
      Purpose::Media => {},
   }
   for (name, value) in extra {
      headers.insert(name.clone(), value.clone());
   }
}

const fn is_idempotent(method: &Method) -> bool {
   matches!(*method, Method::GET | Method::HEAD)
}

/// HTML (or headerless) 429 from X's edge. A JSON 429 carries
/// `x-rate-limit-remaining` or `application/json` and is the account quota,
/// which a different IP does not reset.
fn is_edge_block(status: StatusCode, headers: &HeaderMap) -> bool {
   if status != StatusCode::TOO_MANY_REQUESTS {
      return false;
   }
   if headers.get("x-rate-limit-remaining").is_some() {
      return false;
   }
   headers
      .get(header::CONTENT_TYPE)
      .and_then(|value| value.to_str().ok())
      .is_none_or(|value| !value.to_ascii_lowercase().contains("json"))
}

fn is_direct_failure(err: &Error) -> bool {
   let Error::Internal(message) = err else {
      return false;
   };
   let message = message.to_ascii_lowercase();
   message.contains("timed out")
      || message.contains("connect failed")
      || message.contains("error sending request")
      || message.contains("connection")
      || message.contains("broken pipe")
}

/// Client-cache owner for a browser rather than an account. Session ids are
/// positive, so browsers take negative keys.
fn identity_owner(identity: Identity) -> i64 {
   let mut hasher = std::collections::hash_map::DefaultHasher::new();
   identity.to_string().hash(&mut hasher);
   -1 - (hasher.finish() >> 1).cast_signed()
}

fn proxy_tag(proxy: Option<&ProxyConfig>) -> u64 {
   let Some(proxy) = proxy else {
      return 0;
   };
   let mut tag = 0xcbf2_9ce4_8422_2325_u64;
   tag ^= u64::from(proxy.port);
   tag = tag.wrapping_mul(0x100_0000_01b3);
   for byte in proxy.host.bytes() {
      tag ^= u64::from(byte);
      tag = tag.wrapping_mul(0x100_0000_01b3);
   }
   tag | 1
}

fn banned_proxies_file() -> Option<PathBuf> {
   let path =
      env::var("TEAPOT_DISABLED_PROXIES_FILE").unwrap_or_else(|_| BANNED_PROXIES_FILE.to_owned());
   (!path.is_empty()).then(|| PathBuf::from(path))
}

/// One line of the ban file: `port time reason [detail...]`.
struct BanLine<'a> {
   port: u16,
   /// Unix millis the line was written, when its time parses.
   written: Option<u64>,
   reason: &'a str,
}

impl<'a> BanLine<'a> {
   fn parse(raw: &'a str) -> Option<Self> {
      let line = raw.trim();
      if line.is_empty() || line.starts_with('#') {
         return None;
      }
      let mut fields = line.split_whitespace();
      let port = fields.next()?.parse().ok()?;
      let written = fields.next().and_then(parse_utc_stamp);
      let reason = fields.next().unwrap_or("");
      Some(Self {
         port,
         written,
         reason,
      })
   }

   /// A Cloudflare ban whose time is up. A line without a readable time
   /// stays, like every line teapot did not write.
   fn lapsed(&self, now: u64) -> bool {
      let lasts = u64::try_from(CLOUDFLARE_BAN_FOR.as_millis()).unwrap_or(u64::MAX);
      self.reason == "cloudflare"
         && self
            .written
            .is_some_and(|at| at.saturating_add(lasts) <= now)
   }
}

/// Ports out of rotation at `now`. Lines written for header timeouts are
/// ignored, so a leftover false positive cannot take the pool down again.
fn load_banned(path: &Path, proxies: &[ProxyConfig], now: u64) -> HashSet<u64> {
   let text = match fs::read_to_string(path) {
      Ok(text) => text,
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HashSet::new(),
      Err(err) => {
         tracing::warn!(%err, "banned proxy file unreadable; no exit stays out");
         return HashSet::new();
      },
   };
   let mut tags = HashSet::new();
   for line in text.lines().filter_map(BanLine::parse) {
      if line.reason == "timeouts" || line.lapsed(now) {
         continue;
      }
      let Some(proxy) = proxies.iter().find(|proxy| proxy.port == line.port) else {
         tracing::warn!(port = line.port, "banned proxy file names a port that is not an exit");
         continue;
      };
      tags.insert(proxy_tag(Some(proxy)));
   }
   tags
}

/// Remove lapsed Cloudflare lines from the ban file. scrape-handshake-watch.py
/// counts every port in the file as already out and will not ban it again,
/// so a lapsed line cannot stay behind.
fn drop_lapsed_bans(path: &Path, now: u64) {
   let Ok(text) = fs::read_to_string(path) else {
      return;
   };
   let mut lapsed = Vec::new();
   let mut kept = String::with_capacity(text.len());
   for raw in text.lines() {
      match BanLine::parse(raw) {
         Some(line) if line.lapsed(now) => lapsed.push(line.port),
         _ => {
            kept.push_str(raw);
            kept.push('\n');
         },
      }
   }
   if lapsed.is_empty() {
      return;
   }
   lapsed.sort_unstable();
   lapsed.dedup();
   let mut temporary = path.as_os_str().to_owned();
   temporary.push(".tmp");
   match fs::write(&temporary, kept).and_then(|()| fs::rename(&temporary, path)) {
      Ok(()) => tracing::info!(ports = ?lapsed, "Cloudflare bans lapsed; removed their lines"),
      Err(err) => tracing::warn!(%err, ports = ?lapsed, "could not remove lapsed proxy bans"),
   }
}

/// Unix millis for a time written by [`utc_stamp`].
fn parse_utc_stamp(stamp: &str) -> Option<u64> {
   let at = OffsetDateTime::parse(stamp, &Rfc3339).ok()?;
   u64::try_from(at.unix_timestamp()).ok()?.checked_mul(1000)
}

fn utc_stamp(now_ms: u64) -> String {
   let secs = now_ms / 1000;
   let days = i64::try_from(secs / 86_400).unwrap_or(0);
   let tod = secs % 86_400;
   let (year, month, day) = civil_from_days(days);
   format!(
      "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
      tod / 3600,
      (tod % 3600) / 60,
      tod % 60
   )
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
   let z = days + 719_468;
   let era = z.div_euclid(146_097);
   let doe = u64::try_from(z - era * 146_097).unwrap_or(0);
   let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
   let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
   let mp = (5 * doy + 2) / 153;
   let day = doy - (153 * mp + 2) / 5 + 1;
   let month = if mp < 10 { mp + 3 } else { mp - 9 };
   let year = i32::try_from(yoe as i64 + era * 400).unwrap_or(1970) + i32::from(month <= 2);
   (
      year,
      u32::try_from(month).unwrap_or(1),
      u32::try_from(day).unwrap_or(1),
   )
}

fn unix_millis() -> u64 {
   SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
      .unwrap_or(0)
}

/// A GraphQL URI as logged: the operation and its variables. The feature and
/// field-toggle blobs after them are the same on every call and most of the
/// length, and every line of them is another write pm2 has to keep up with.
fn loggable(uri: &str) -> &str {
   uri.find("&features=").map_or(uri, |at| &uri[..at])
}

/// A failure to send at all, as opposed to a timeout, a connect failure or a
/// bad response: the far end dropped a connection we thought was open.
fn is_transport_blip(err: &primp::Error) -> bool {
   err.is_request() && !err.is_timeout() && !err.is_connect() && !err.is_body() && !err.is_decode()
}

/// Error text that keeps the cause, since the client's own `Display` stops at
/// "error sending request".
fn describe(err: primp::Error) -> Error {
   if err.is_timeout() {
      return Error::Internal("HTTP request timed out".into());
   }
   let mut text = if err.is_connect() {
      String::from("connect failed")
   } else {
      err.to_string()
   };
   let mut source = err.source();
   while let Some(cause) = source {
      let _ = write!(text, ": {cause}");
      source = cause.source();
   }
   Error::Internal(text)
}

/// Parse proxy URL (e.g. `http://host:port`) and optional `user:pass` auth.
fn parse_proxy(url: &str, auth: &str) -> ProxyConfig {
   let (kind, stripped) = if let Some(rest) = url
      .strip_prefix("socks5h://")
      .or_else(|| url.strip_prefix("socks5://"))
   {
      (ProxyKind::Socks5, rest)
   } else {
      (
         ProxyKind::Http,
         url.strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap_or(url),
      )
   };
   let (host, port) = if let Some((host_part, port_part)) = stripped.rsplit_once(':') {
      (host_part.to_owned(), port_part.parse().unwrap_or(8080))
   } else {
      (stripped.to_owned(), 8080)
   };
   let (username, password) = match auth.split_once(':') {
      Some((user, pass)) => (Some(user.to_owned()), Some(pass.to_owned())),
      None if auth.is_empty() => (None, None),
      None => (Some(auth.to_owned()), None),
   };
   ProxyConfig {
      host,
      port,
      kind,
      username,
      password,
   }
}

impl Response {
   pub fn status(&self) -> StatusCode {
      self.inner.status()
   }

   pub fn headers(&self) -> &HeaderMap {
      self.inner.headers()
   }

   /// The body as a stream, for media that is relayed rather than read.
   pub fn into_body(self) -> StreamingBody {
      StreamingBody {
         inner: Box::pin(self.inner.bytes_stream()),
      }
   }

   /// Collect the response body as bytes; compression is already undone.
   pub async fn bytes(self) -> Result<Bytes> {
      self.bytes_limited(DEFAULT_BODY_LIMIT).await
   }

   /// Collect the response body as bytes, rejecting oversized bodies.
   pub async fn bytes_limited(mut self, max_bytes: usize) -> Result<Bytes> {
      if let Some(declared) = self.inner.content_length()
         && usize::try_from(declared).is_ok_and(|declared| declared > max_bytes)
      {
         return Err(Error::Internal(format!(
            "response body exceeded {max_bytes} bytes"
         )));
      }
      let mut collected = Vec::new();
      while let Some(chunk) = self.inner.chunk().await.map_err(describe)? {
         if collected.len().saturating_add(chunk.len()) > max_bytes {
            return Err(Error::Internal(format!(
               "response body exceeded {max_bytes} bytes"
            )));
         }
         collected.extend_from_slice(&chunk);
      }
      Ok(Bytes::from(collected))
   }

   /// Collect the response body as a UTF-8 string.
   pub async fn text(self) -> Result<String> {
      let data = self.bytes().await?;
      String::from_utf8(data.into()).map_err(|err| Error::Internal(format!("invalid UTF-8: {err}")))
   }
}

/// A response body relayed chunk by chunk.
pub struct StreamingBody {
   inner: Pin<Box<dyn Stream<Item = primp::Result<Bytes>> + Send>>,
}

impl http_body::Body for StreamingBody {
   type Data = Bytes;
   type Error = std::io::Error;

   fn poll_frame(
      mut self: Pin<&mut Self>,
      cx: &mut Context<'_>,
   ) -> Poll<Option<StdResult<Frame<Self::Data>, Self::Error>>> {
      match self.inner.as_mut().poll_next(cx) {
         Poll::Ready(Some(Ok(chunk))) => Poll::Ready(Some(Ok(Frame::data(chunk)))),
         Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(std::io::Error::other(err)))),
         Poll::Ready(None) => Poll::Ready(None),
         Poll::Pending => Poll::Pending,
      }
   }
}

#[cfg(test)]
fn proxy_on(port: u16) -> ProxyConfig {
   ProxyConfig {
      host: "proxy.example".into(),
      port,
      kind: ProxyKind::Socks5,
      username: None,
      password: None,
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn fallback_pool_uses_a_different_exit_each_call() {
      let proxies = vec![proxy_on(10001), proxy_on(10002), proxy_on(10003)];
      let pool = FallbackPool {
         tags: proxies.iter().map(|proxy| proxy_tag(Some(proxy))).collect(),
         proxies,
         next: AtomicUsize::new(0),
      };
      let ports: Vec<u16> = (0..5).map(|_| pool.next().port).collect();
      assert_eq!(ports, vec![10001, 10002, 10003, 10001, 10002]);
   }

   #[test]
   fn a_rate_limited_primary_exit_is_parked_for_every_account() {
      let client = HttpClient::new("", "", Purpose::Api).with_primary_proxies(vec![
         proxy_on(10001),
         proxy_on(10002),
         proxy_on(10003),
      ]);
      let pinned = Egress {
         session: 42,
         proxy: Some(proxy_on(10001)),
      };
      assert!(!client.primary_is_blocked(&pinned));
      assert_eq!(client.block_primary(&pinned), Some(EDGE_PARKING.first));
      assert_eq!(client.block_primary(&pinned), None);
      assert!(client.primary_is_blocked(&pinned));

      let alternate = client.alternate_primary(&pinned).unwrap();
      let alternate_again = client.alternate_primary(&pinned).unwrap();
      assert_ne!(alternate.proxy.as_ref().unwrap().port, 10001);
      assert_eq!(
         alternate.proxy.as_ref().unwrap().port,
         alternate_again.proxy.as_ref().unwrap().port
      );

      // If the replacement is blocked too, the remaining healthy exit wins.
      assert!(client.block_primary(&alternate).is_some());
      let next = client.alternate_primary(&pinned).unwrap();
      assert_ne!(
         next.proxy.as_ref().unwrap().port,
         alternate.proxy.as_ref().unwrap().port
      );
   }

   #[test]
   fn a_bare_429_on_an_exit_still_served_is_the_accounts() {
      let client = HttpClient::new("", "", Purpose::Api).with_primary_proxies(vec![
         proxy_on(10001),
         proxy_on(10002),
         proxy_on(10003),
      ]);
      let pinned = Egress {
         session: 42,
         proxy: Some(proxy_on(10001)),
      };
      assert!(!client.exit_still_served(&pinned), "nothing has been served yet");

      client.primary_pool.as_ref().unwrap().note_served(&proxy_on(10001));
      assert!(client.exit_still_served(&pinned));
      assert!(!client.primary_is_blocked(&pinned), "judging an exit does not park it");

      // Parked, the account goes out through its failover and is judged there.
      assert!(client.block_primary(&pinned).is_some());
      assert!(!client.exit_still_served(&pinned));
      let failover = client.alternate_primary(&pinned).unwrap();
      client
         .primary_pool
         .as_ref()
         .unwrap()
         .note_served(failover.proxy.as_ref().unwrap());
      assert!(client.exit_still_served(&pinned));
   }

   #[test]
   fn a_logged_uri_keeps_its_variables_and_drops_the_feature_blob() {
      let uri = "https://x.com/i/api/graphql/abc/TweetDetail?variables=%7B%22focalTweetId%22%3A%221%22%7D&features=%7B%22a%22%3Atrue%7D&fieldToggles=%7B%7D";
      assert_eq!(
         loggable(uri),
         "https://x.com/i/api/graphql/abc/TweetDetail?variables=%7B%22focalTweetId%22%3A%221%22%7D"
      );
      let media = "https://pbs.twimg.com/media/abc.jpg?name=small";
      assert_eq!(loggable(media), media);
   }

   #[test]
   fn an_exit_x_answers_slowly_is_parked_and_the_rest_are_not() {
      let proxies = (10001..=10006).map(proxy_on).collect::<Vec<_>>();
      let client = HttpClient::new("", "", Purpose::Api).with_primary_proxies(proxies.clone());
      let pool = client.primary_pool.as_ref().unwrap();
      for proxy in &proxies {
         for _ in 0..40 {
            assert!(pool.record(proxy, false).is_none());
         }
         for _ in 0..2 {
            assert!(pool.record(proxy, true).is_none());
         }
      }
      let slow = &proxies[1];
      let parked = (0..30).find_map(|_| pool.record(slow, true));
      assert_eq!(parked, Some(SLOW_PARKING.first));
      assert_eq!(client.parked_exits().len(), 1);

      let pinned = Egress {
         session: 7,
         proxy: Some(slow.clone()),
      };
      assert!(client.primary_is_blocked(&pinned));
      let moved = client.alternate_primary(&pinned).unwrap();
      assert_ne!(moved.proxy.unwrap().port, slow.port);
      let healthy = Egress {
         session: 7,
         proxy: Some(proxies[0].clone()),
      };
      assert!(!client.primary_is_blocked(&healthy));
   }

   #[test]
   fn every_exit_slow_at_once_parks_none() {
      let proxies = (10001..=10006).map(proxy_on).collect::<Vec<_>>();
      let client = HttpClient::new("", "", Purpose::Api).with_primary_proxies(proxies.clone());
      let pool = client.primary_pool.as_ref().unwrap();
      for _ in 0..20 {
         for proxy in &proxies {
            assert!(pool.record(proxy, false).is_none());
            assert!(pool.record(proxy, true).is_none());
         }
      }
      assert!(client.parked_exits().is_empty());
   }

   #[test]
   fn an_exit_that_relapses_is_parked_longer() {
      let proxies = (10001..=10003).map(proxy_on).collect::<Vec<_>>();
      let pool = PrimaryPool::new(
         proxies.clone(),
         proxies.iter().map(|proxy| proxy_tag(Some(proxy))).collect(),
      );
      let tag = proxy_tag(Some(&proxies[0]));
      let now = unix_millis();
      let first = SLOW_PARKING.first;
      assert_eq!(pool.park_slow(tag, now), Some(first));
      assert_eq!(pool.park_slow(tag, now), None, "already parked");
      let after = now + first.as_millis() as u64 + 1;
      assert_eq!(pool.park_slow(tag, after), Some(first * 2));
      let healed =
         after + (first * 2).as_millis() as u64 + SLOW_PARKING.memory.as_millis() as u64 + 1;
      assert_eq!(pool.park_slow(tag, healed), Some(first));
   }

   #[test]
   fn an_exit_the_edge_keeps_throttling_stays_out_longer_each_time() {
      let parking = EDGE_PARKING;
      let mins = |n| Duration::from_mins(n);
      let at = |park: Duration| u64::try_from(park.as_millis()).unwrap();
      let mut history = (0, 0);
      let mut now = unix_millis();
      let mut parked = Vec::new();
      for _ in 0..7 {
         let (count, park) = parking.next(history, now);
         parked.push(park);
         history = (count, now + at(park));
         // Throttled again a minute after coming back.
         now += at(park) + at(mins(1));
      }
      assert_eq!(
         parked,
         [mins(5), mins(10), mins(20), mins(40), mins(80), mins(120), mins(120)]
      );
      // Half an hour clean starts it over.
      now += at(parking.memory);
      assert_eq!(parking.next(history, now).1, mins(5));
   }

   #[test]
   fn edge_and_slow_parkings_keep_separate_histories() {
      let proxies = (10001..=10003).map(proxy_on).collect::<Vec<_>>();
      let pool = PrimaryPool::new(
         proxies.clone(),
         proxies.iter().map(|proxy| proxy_tag(Some(proxy))).collect(),
      );
      let tag = proxy_tag(Some(&proxies[0]));
      assert_eq!(pool.park_slow(tag, unix_millis()), Some(SLOW_PARKING.first));
      // Out early by hand: a slow spell just before must not double this.
      pool.blocked_until.lock().unwrap().clear();
      assert_eq!(pool.block(&proxies[0]), Some(EDGE_PARKING.first));
   }

   #[test]
   fn a_cloudflare_ban_stays_out_and_a_timeout_line_does_not() {
      let path = std::env::temp_dir().join(format!(
         "teapot-banned-proxies-{}.txt",
         std::process::id()
      ));
      let stamp = utc_stamp(unix_millis());
      std::fs::write(
         &path,
         format!("10001 {stamp} timeouts\n10002 {stamp} cloudflare\n"),
      )
      .unwrap();
      let proxies = (10001..=10006).map(proxy_on).collect::<Vec<_>>();
      let mut pool = PrimaryPool::new(
         proxies.clone(),
         proxies.iter().map(|proxy| proxy_tag(Some(proxy))).collect(),
      );
      pool.watch_banned_file(path.clone());
      assert!(!pool.is_blocked(&proxies[0]), "a header-timeout line stays ignored");
      assert!(pool.is_blocked(&proxies[1]));
      assert!(pool.ban(&proxies[2]));
      assert!(!pool.ban(&proxies[2]), "banning the same exit twice is one line");
      assert!(pool.is_blocked(&proxies[2]));
      let text = std::fs::read_to_string(&path).unwrap();
      assert!(text.contains("10003 "));
      assert!(text.contains("cloudflare"));
      assert!(!pool.is_blocked(&proxies[3]));
      std::fs::remove_file(&path).unwrap();
   }

   #[test]
   fn a_lapsed_cloudflare_ban_comes_back_and_leaves_the_file() {
      let path = env::temp_dir().join(format!(
         "teapot-lapsed-proxies-{}.txt",
         std::process::id()
      ));
      let now = unix_millis();
      let lasts = u64::try_from(CLOUDFLARE_BAN_FOR.as_millis()).unwrap();
      let old = utc_stamp(now - lasts - 60_000);
      let recent = utc_stamp(now - 60_000);
      fs::write(
         &path,
         format!(
            "# header\n10001 {old} cloudflare\n10002 {recent} cloudflare\n10003 {old} handshake \
             scrape1 192.0.2.1:51820\n10004 {old} cloudflare\n10004 {recent} cloudflare\n"
         ),
      )
      .unwrap();
      let proxies = (10001..=10006).map(proxy_on).collect::<Vec<_>>();
      let mut pool = PrimaryPool::new(
         proxies.clone(),
         proxies.iter().map(|proxy| proxy_tag(Some(proxy))).collect(),
      );
      pool.watch_banned_file(path.clone());
      assert!(!pool.is_blocked(&proxies[0]), "the old Cloudflare ban has lapsed");
      assert!(pool.is_blocked(&proxies[1]));
      assert!(pool.is_blocked(&proxies[2]), "a handshake ban waits to be deleted");
      assert!(pool.is_blocked(&proxies[3]), "a newer block on the same exit holds");

      pool.reload_banned_file();
      let text = fs::read_to_string(&path).unwrap();
      assert_eq!(
         text,
         format!(
            "# header\n10002 {recent} cloudflare\n10003 {old} handshake scrape1 \
             192.0.2.1:51820\n10004 {recent} cloudflare\n"
         )
      );
      assert!(!pool.is_blocked(&proxies[0]));
      assert!(pool.ban(&proxies[0]), "Cloudflare blocking it again bans it again");
      assert!(pool.is_blocked(&proxies[0]));
      fs::remove_file(&path).unwrap();
   }

   #[test]
   fn a_run_of_header_stalls_parks_briefly_and_leaves_the_rest_up() {
      let proxies = (10001..=10030).map(proxy_on).collect::<Vec<_>>();
      let pool = PrimaryPool::new(
         proxies.clone(),
         proxies.iter().map(|proxy| proxy_tag(Some(proxy))).collect(),
      );
      for proxy in &proxies {
         for _ in 0..20 {
            let _ = pool.record(proxy, true);
         }
         let _ = pool.block(proxy);
      }
      let open = proxies
         .iter()
         .filter(|proxy| !pool.is_blocked(proxy))
         .count();
      assert!(
         open >= pool.must_stay(),
         "{open} exits left, need at least {}",
         pool.must_stay()
      );
      let slow = &proxies[0];
      assert!(pool
         .parked()
         .iter()
         .all(|(_, secs)| *secs <= EDGE_PARKING.first.max(SLOW_PARKING.first).as_secs()));
      assert_ne!(pool.alternate(7, slow).unwrap().port, slow.port);
   }

   #[test]
   fn pool_exit_clients_are_shared_per_browser() {
      let client = HttpClient::new("", "", Purpose::Api)
         .with_fallback_proxies(vec![proxy_on(10001), proxy_on(10002)]);
      for session in 1..=60 {
         for _ in 0..2 {
            let egress = Egress {
               session,
               proxy: client.take_fallback(),
            };
            client.client_for(Some(&egress)).unwrap();
         }
      }
      let browsers: HashSet<String> = (1..=60)
         .map(|session| browser::identity_for(session).to_string())
         .collect();
      let cached = client.sessions.lock().unwrap().len();
      assert!(
         cached <= browsers.len() * 2,
         "{cached} clients for {} browsers",
         browsers.len()
      );
      assert!(identity_owner(browser::identity_for(1)) < 0);
   }

   #[test]
   fn socks_proxy_url_carries_credentials() {
      let proxy = ProxyConfig {
         host: "proxy.example".into(),
         port: 10001,
         kind: ProxyKind::Socks5,
         username: Some("user".into()),
         password: Some("p@ss:word".into()),
      };
      assert_eq!(
         proxy.url(),
         "socks5h://user:p%40ss%3Aword@proxy.example:10001"
      );
   }

   #[test]
   fn html_429_is_an_edge_block_and_json_429_is_not() {
      let mut html = HeaderMap::new();
      html.insert(
         header::CONTENT_TYPE,
         HeaderValue::from_static("text/html; charset=utf-8"),
      );
      assert!(is_edge_block(StatusCode::TOO_MANY_REQUESTS, &html));

      let mut quota = HeaderMap::new();
      quota.insert(
         header::CONTENT_TYPE,
         HeaderValue::from_static("application/json"),
      );
      quota.insert("x-rate-limit-remaining", HeaderValue::from_static("0"));
      assert!(!is_edge_block(StatusCode::TOO_MANY_REQUESTS, &quota));
      assert!(!is_edge_block(StatusCode::OK, &html));
   }

   #[test]
   fn http_proxy_from_config() {
      let proxy = parse_proxy("http://squid.local:3128", "alice:secret");
      assert_eq!(proxy.kind, ProxyKind::Http);
      assert_eq!(proxy.url(), "http://alice:secret@squid.local:3128");
      let bare = parse_proxy("squid.local", "");
      assert_eq!(bare.url(), "http://squid.local:8080");
   }

   #[test]
   fn api_header_order_matches_the_chrome_capture() {
      let order = wire_header_order(Purpose::Api);
      let mut headers = HeaderMap::new();
      // Inserted backwards on purpose: the wire order is the capture, not
      // insertion order.
      for name in [
         "priority",
         "cookie",
         "accept-language",
         "accept-encoding",
         "referer",
         "sec-fetch-dest",
         "sec-fetch-mode",
         "sec-fetch-site",
         "accept",
         "content-type",
         "user-agent",
         "x-twitter-auth-type",
         "x-client-transaction-id",
         "x-twitter-active-user",
         "sec-ch-ua-mobile",
         "x-twitter-client-language",
         "sec-ch-ua",
         "x-csrf-token",
         "authorization",
         "sec-ch-ua-platform",
      ] {
         headers.insert(HeaderName::from_static(name), HeaderValue::from_static("x"));
      }
      let positions: std::collections::HashMap<&HeaderName, usize> = order
         .iter()
         .enumerate()
         .map(|(index, name)| (name, index))
         .collect();
      let mut names: Vec<HeaderName> = headers.keys().cloned().collect();
      names.sort_by_key(|name| positions.get(name).copied().unwrap_or(order.len()));
      let joined: Vec<&str> = names.iter().map(HeaderName::as_str).collect();
      assert_eq!(
         joined,
         vec![
            "sec-ch-ua-platform",
            "authorization",
            "x-csrf-token",
            "sec-ch-ua",
            "x-twitter-client-language",
            "sec-ch-ua-mobile",
            "x-twitter-active-user",
            "x-client-transaction-id",
            "x-twitter-auth-type",
            "user-agent",
            "content-type",
            "accept",
            "sec-fetch-site",
            "sec-fetch-mode",
            "sec-fetch-dest",
            "referer",
            "accept-encoding",
            "accept-language",
            "cookie",
            "priority",
         ]
      );
   }

   #[test]
   fn api_headers_look_like_an_xhr() {
      let mut headers = HeaderMap::new();
      headers.insert(header::ACCEPT, HeaderValue::from_static("text/html"));
      headers.insert("upgrade-insecure-requests", HeaderValue::from_static("1"));
      headers.insert("sec-fetch-user", HeaderValue::from_static("?1"));
      headers.insert("sec-fetch-mode", HeaderValue::from_static("navigate"));
      headers.insert(
         "sec-ch-ua",
         HeaderValue::from_static("\"Chromium\";v=\"151\""),
      );
      shape_headers(&mut headers, Purpose::Api, &HeaderMap::new());
      assert_eq!(headers.get(header::ACCEPT).unwrap(), "*/*");
      assert_eq!(headers.get("sec-fetch-mode").unwrap(), "cors");
      assert!(headers.get("upgrade-insecure-requests").is_none());
      assert!(headers.get("sec-fetch-user").is_none());
      assert_eq!(headers.get("sec-ch-ua").unwrap(), "\"Chromium\";v=\"151\"");
      assert_eq!(headers.get(header::REFERER).unwrap(), "https://x.com/");
   }

   #[test]
   fn script_headers_match_a_chrome_chunk_load() {
      let mut headers = HeaderMap::new();
      headers.insert(header::ACCEPT, HeaderValue::from_static("text/html"));
      headers.insert("sec-fetch-mode", HeaderValue::from_static("navigate"));
      headers.insert("sec-fetch-dest", HeaderValue::from_static("document"));
      headers.insert("sec-fetch-site", HeaderValue::from_static("none"));
      headers.insert("priority", HeaderValue::from_static("u=0, i"));
      shape_headers(&mut headers, Purpose::Script, &HeaderMap::new());
      assert_eq!(headers.get(header::ACCEPT).unwrap(), "*/*");
      assert_eq!(headers.get("sec-fetch-mode").unwrap(), "cors");
      assert_eq!(headers.get("sec-fetch-dest").unwrap(), "script");
      assert_eq!(headers.get("sec-fetch-site").unwrap(), "cross-site");
      assert_eq!(headers.get(header::ORIGIN).unwrap(), "https://x.com");
      assert_eq!(headers.get(header::REFERER).unwrap(), "https://x.com/");
      assert!(headers.get("priority").is_none());

      let order = wire_header_order(Purpose::Script);
      let names: Vec<&str> = order.iter().map(HeaderName::as_str).collect();
      assert_eq!(&names[..3], &["sec-gpc", "origin", "sec-ch-ua-platform"]);
   }

   /// HTTP/1.1 on localhost whose n-th request waits `delays[n]` before
   /// answering with its own index.
   async fn delayed_server(delays: Vec<Duration>) -> String {
      use tokio::{
         io::{AsyncReadExt as _, AsyncWriteExt as _},
         net::TcpListener,
         time::sleep,
      };

      let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
      let addr = listener.local_addr().unwrap();
      let delays = Arc::new(delays);
      let count = Arc::new(AtomicUsize::new(0));
      tokio::spawn(async move {
         while let Ok((mut socket, _)) = listener.accept().await {
            let delays = Arc::clone(&delays);
            let count = Arc::clone(&count);
            tokio::spawn(async move {
               let mut buf = [0_u8; 4096];
               let mut seen = Vec::new();
               while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
                  match socket.read(&mut buf).await {
                     Ok(0) | Err(_) => return,
                     Ok(read) => seen.extend_from_slice(&buf[..read]),
                  }
               }
               let index = count.fetch_add(1, Ordering::SeqCst);
               sleep(delays.get(index).copied().unwrap_or_default()).await;
               let body = index.to_string();
               let response = format!(
                  "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                  body.len()
               );
               let _ = socket.write_all(response.as_bytes()).await;
            });
         }
      });
      format!("http://{addr}/")
   }

   #[tokio::test]
   async fn a_dead_first_connection_is_answered_by_a_fresh_one() {
      let uri = delayed_server(vec![Duration::from_secs(10), Duration::ZERO]).await;
      let client = HttpClient::new("", "", Purpose::Api);
      let started = Instant::now();
      let body = client.get(&uri).await.unwrap().text().await.unwrap();
      assert_eq!(body, "1");
      assert!(started.elapsed() < API_HEADER_DEADLINE * 2, "{:?}", started.elapsed());
   }

   #[tokio::test]
   async fn late_headers_on_the_first_connection_are_kept() {
      let uri = delayed_server(vec![
         API_HEADER_DEADLINE + Duration::from_millis(300),
         Duration::from_secs(10),
      ])
      .await;
      let client = HttpClient::new("", "", Purpose::Api);
      let started = Instant::now();
      let body = client.get(&uri).await.unwrap().text().await.unwrap();
      assert_eq!(body, "0", "the slow first call answered and was dropped");
      assert!(started.elapsed() < API_HEADER_DEADLINE * 2, "{:?}", started.elapsed());
   }

   #[test]
   fn safari_gets_no_fetch_metadata_it_never_sends() {
      let mut headers = HeaderMap::new();
      headers.insert(header::ACCEPT, HeaderValue::from_static("text/html"));
      shape_headers(&mut headers, Purpose::Api, &HeaderMap::new());
      assert!(headers.get("sec-fetch-mode").is_none());
      assert!(headers.get("sec-fetch-dest").is_none());
   }
}
