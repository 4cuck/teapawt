use std::{fs, net::IpAddr, path::Path, process::Command};

use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GifTranscodingMode {
   Off,
   #[default]
   Local,
   External,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GifTranscodingConfig {
   #[serde(default)]
   pub mode: GifTranscodingMode,
   #[serde(default = "default_gif_cache_dir", rename = "cacheDir")]
   pub cache_dir: String,
   #[serde(default = "default_gif_cache_max_mb", rename = "cacheMaxMb")]
   pub cache_max_mb: u64,
   #[serde(default, rename = "externalDomain")]
   pub external_domain: String,
}

impl Default for GifTranscodingConfig {
   fn default() -> Self {
      Self {
         mode: GifTranscodingMode::Local,
         cache_dir: default_gif_cache_dir(),
         cache_max_mb: default_gif_cache_max_mb(),
         external_domain: String::new(),
      }
   }
}

const fn default_client_budget() -> bool {
   true
}

fn default_gif_cache_dir() -> String {
   "./cache/gif".to_owned()
}
const fn default_gif_cache_max_mb() -> u64 {
   512
}

#[derive(Debug, Clone, Deserialize)]
#[expect(
   clippy::struct_field_names,
   reason = "config field in Config mirrors TOML structure"
)]
pub struct Config {
   pub server: ServerConfig,
   pub cache: CacheConfig,
   pub config: AppConfig,
   #[serde(default)]
   pub preferences: PreferencesConfig,
   #[serde(default, rename = "gifTranscoding")]
   pub gif_transcoding: GifTranscodingConfig,
   #[serde(default)]
   pub donations: DonationsConfig,
   /// Precomputed `{scheme}://{hostname}` value set in `Config::load`.
   #[serde(skip)]
   pub url_prefix: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
   #[serde(default = "default_hostname")]
   pub hostname: String,
   #[serde(default = "default_title")]
   pub title: String,
   #[serde(default = "default_address")]
   pub address: String,
   #[serde(default = "default_port")]
   pub port: u16,
   #[serde(default, rename = "publicPort")]
   pub public_port: Option<u16>,
   #[serde(default)]
   pub https: bool,
   #[serde(
      default = "default_http_max_connections",
      rename = "httpMaxConnections"
   )]
   pub http_max_connections: u32,
   #[serde(default = "default_static_dir", rename = "staticDir")]
   pub static_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
   #[serde(default = "default_list_minutes", rename = "listMinutes")]
   pub list_minutes: u64,
   #[serde(default = "default_rss_minutes", rename = "rssMinutes")]
   pub rss_minutes: u64,
   #[serde(default = "default_max_entries", rename = "maxEntries")]
   pub max_entries: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[expect(
   clippy::struct_excessive_bools,
   reason = "mirrors TOML config booleans"
)]
pub struct AppConfig {
   #[serde(rename = "hmacKey")]
   pub hmac_key: String,
   /// IndexNow ownership key. Served at `/{key}.txt` when it is 8–128 hex
   /// characters. Empty disables the file. Keep the real value in the
   /// gitignored config, not the public repo.
   #[serde(default, rename = "indexNowKey")]
   pub indexnow_key: String,
   #[serde(default, rename = "base64Media")]
   pub base64_media: bool,
   #[serde(default = "default_true", rename = "enableRSS")]
   pub enable_rss: bool,
   #[serde(default = "default_client_budget", rename = "clientBudget")]
   pub client_budget: bool,
   #[serde(default, rename = "trustedProxies")]
   pub trusted_proxies: Vec<String>,
   #[serde(default, rename = "rateLimitedMessage")]
   pub rate_limited_message: Vec<String>,
   #[serde(default, rename = "clientBudgetMessage")]
   pub client_budget_message: Vec<String>,
   #[serde(default, rename = "internalErrorMessage")]
   pub internal_error_message: Vec<String>,
   #[serde(default, rename = "enableDebug")]
   pub enable_debug: bool,
   #[serde(default, rename = "debugToken")]
   pub debug_token: String,
   #[serde(default)]
   pub proxy: String,
   #[serde(default, rename = "proxyAuth")]
   pub proxy_auth: String,
   #[serde(default, rename = "apiProxy")]
   pub api_proxy: String,
   /// `host:port:user:pass` SOCKS5 list used for X API calls.
   #[serde(default, rename = "socksProxiesFile")]
   pub socks_proxies_file: String,
   /// Exits used in turn, one per request, only after the direct address
   /// gets an HTML 429. Lines are `http://user:pass@host:port`,
   /// `socks5h://user:pass@host:port`, or `host:port:user:pass` (HTTP).
   #[serde(default, rename = "fallbackProxiesFile")]
   pub fallback_proxies_file: String,
   /// session id → SOCKS5 port pins so each account keeps the same exit IP.
   #[serde(default, rename = "sessionProxiesFile")]
   pub session_proxies_file: String,
   #[serde(default, rename = "disableTid")]
   pub disable_tid: bool,
   /// Read about-account data from X-Posed's public cache before asking X.
   #[serde(default, rename = "xPosedCommunityCache")]
   pub x_posed_community_cache: bool,
   #[serde(default = "default_max_concurrent_reqs", rename = "maxConcurrentReqs")]
   pub max_concurrent_reqs: u32,
   #[serde(default = "default_paid_emoji", rename = "paidEmoji")]
   pub paid_emoji: String,
   #[serde(default = "default_ai_emoji", rename = "aiEmoji")]
   pub ai_emoji: String,
   /// Kagi session token for server-side translation (inline value).
   #[serde(default, rename = "kagiToken")]
   pub kagi_token: String,
   /// Path to a file containing the Kagi session token.
   #[serde(default, rename = "kagiTokenFile")]
   pub kagi_token_file: String,
}

#[derive(Clone, Deserialize, Default)]
pub struct DonationsConfig {
   #[serde(default = "default_goal_cents", rename = "goalCents")]
   pub goal_cents: i64,
   /// Kept so older config files still load. The public counter is monthly and
   /// ignores this date.
   #[serde(default = "default_goal_deadline", rename = "goalDeadline")]
   pub goal_deadline: String,
   #[serde(default = "default_ledger_file", rename = "ledgerFile")]
   pub ledger_file: String,
   #[serde(default)]
   pub stripe: StripeDonationConfig,
   #[serde(default)]
   pub nowpayments: NowPaymentsDonationConfig,
   #[serde(default)]
   pub lzt: LztDonationConfig,
}

impl std::fmt::Debug for DonationsConfig {
   fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      formatter
         .debug_struct("DonationsConfig")
         .field("goal_cents", &self.goal_cents)
         .field("goal_deadline", &self.goal_deadline)
         .field("ledger_file", &self.ledger_file)
         .field("stripe", &self.stripe.configured())
         .field("nowpayments", &self.nowpayments.configured())
         .field("lzt", &self.lzt.configured())
         .finish()
   }
}

#[derive(Clone, Deserialize, Default)]
pub struct StripeDonationConfig {
   #[serde(default, rename = "secretKey")]
   pub secret_key: String,
   #[serde(default, rename = "webhookSecret")]
   pub webhook_secret: String,
}

impl StripeDonationConfig {
   pub fn configured(&self) -> bool {
      !self.secret_key.is_empty()
   }
}

impl std::fmt::Debug for StripeDonationConfig {
   fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      formatter
         .debug_struct("StripeDonationConfig")
         .field("configured", &self.configured())
         .finish()
   }
}

#[derive(Clone, Deserialize, Default)]
pub struct NowPaymentsDonationConfig {
   #[serde(default, rename = "apiKey")]
   pub api_key: String,
   #[serde(default, rename = "ipnSecret")]
   pub ipn_secret: String,
}

impl NowPaymentsDonationConfig {
   pub fn configured(&self) -> bool {
      !self.api_key.is_empty()
   }
}

impl std::fmt::Debug for NowPaymentsDonationConfig {
   fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      formatter
         .debug_struct("NowPaymentsDonationConfig")
         .field("configured", &self.configured())
         .field("ipn", &!self.ipn_secret.is_empty())
         .finish()
   }
}

#[derive(Clone, Deserialize)]
pub struct LztDonationConfig {
   #[serde(default, rename = "accessToken")]
   pub access_token: String,
   #[serde(default, rename = "merchantId")]
   pub merchant_id: u64,
   #[serde(default, rename = "secretKey")]
   pub secret_key: String,
   #[serde(default = "default_lzt_currency")]
   pub currency: String,
}

impl Default for LztDonationConfig {
   fn default() -> Self {
      Self {
         access_token: String::new(),
         merchant_id: 0,
         secret_key: String::new(),
         currency: default_lzt_currency(),
      }
   }
}

impl LztDonationConfig {
   pub fn configured(&self) -> bool {
      !self.access_token.is_empty() && self.merchant_id > 0
   }
}

impl std::fmt::Debug for LztDonationConfig {
   fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      formatter
         .debug_struct("LztDonationConfig")
         .field("configured", &self.configured())
         .field("merchant_id", &self.merchant_id)
         .field("currency", &self.currency)
         .field("webhook", &!self.secret_key.is_empty())
         .finish()
   }
}

fn default_goal_cents() -> i64 {
   12_900
}

fn default_goal_deadline() -> String {
   "2026-10-23".to_owned()
}

fn default_ledger_file() -> String {
   "donations.json".to_owned()
}

fn default_lzt_currency() -> String {
   "usd".to_owned()
}

fn env_nonempty(name: &str) -> Option<String> {
   std::env::var(name)
      .ok()
      .map(|value| value.trim().to_owned())
      .filter(|value| !value.is_empty())
}

fn apply_donation_env(donations: &mut DonationsConfig) {
   if let Some(value) = env_nonempty("STRIPE_SECRET_KEY") {
      donations.stripe.secret_key = value;
   }
   if let Some(value) = env_nonempty("STRIPE_WEBHOOK_SECRET") {
      donations.stripe.webhook_secret = value;
   }
   if let Some(value) = env_nonempty("NOWPAYMENTS_API_KEY") {
      donations.nowpayments.api_key = value;
   }
   if let Some(value) = env_nonempty("NOWPAYMENTS_IPN_SECRET") {
      donations.nowpayments.ipn_secret = value;
   }
   if let Some(value) = env_nonempty("LZT_ACCESS_TOKEN") {
      donations.lzt.access_token = value;
   }
   if let Some(value) = env_nonempty("LZT_SECRET_KEY") {
      donations.lzt.secret_key = value;
   }
   if let Some(value) = env_nonempty("LZT_MERCHANT_ID")
      && let Ok(id) = value.parse::<u64>()
   {
      donations.lzt.merchant_id = id;
   }
   if let Some(value) = env_nonempty("LZT_CURRENCY") {
      donations.lzt.currency = value.to_ascii_lowercase();
   }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PreferencesConfig {
   #[serde(default = "default_theme")]
   pub theme: String,
   #[serde(default, rename = "replaceTwitter")]
   pub replace_twitter: String,
   #[serde(default, rename = "replaceYouTube")]
   pub replace_youtube: String,
   #[serde(default, rename = "replaceReddit")]
   pub replace_reddit: String,
   #[serde(default, rename = "infiniteScroll")]
   pub infinite_scroll: bool,
}

// Default value functions
fn default_hostname() -> String {
   "localhost".to_owned()
}
fn default_title() -> String {
   "teapawt".to_owned()
}
fn default_address() -> String {
   "0.0.0.0".to_owned()
}
const fn default_port() -> u16 {
   8080
}
const fn default_http_max_connections() -> u32 {
   100
}
fn default_static_dir() -> String {
   "./public".to_owned()
}
const fn default_list_minutes() -> u64 {
   120
}
const fn default_rss_minutes() -> u64 {
   10
}
const fn default_max_entries() -> usize {
   50_000
}
const fn default_max_concurrent_reqs() -> u32 {
   2
}
fn default_theme() -> String {
   "black".to_owned()
}
fn default_paid_emoji() -> String {
   "🤝".to_owned()
}
fn default_ai_emoji() -> String {
   "🤖".to_owned()
}
const fn default_true() -> bool {
   true
}

impl Config {
   #[expect(clippy::cognitive_complexity, reason = "validation is straightforward")]
   pub fn load<P>(path: P) -> Result<Self>
   where
      P: AsRef<Path>,
   {
      let content = fs::read_to_string(path)?;
      let mut config = toml::from_str::<Self>(&content)?;
      let hmac_key = config.config.hmac_key.trim();
      if hmac_key.is_empty() || hmac_key == "secretkey" || hmac_key.len() < 32 {
         return Err(Error::InvalidConfig(
            "config.hmacKey must be a non-default secret with at least 32 characters".into(),
         ));
      }
      if config.config.enable_debug && config.config.debug_token.len() < 32 {
         return Err(Error::InvalidConfig(
            "config.debugToken must be set to at least 32 characters when enableDebug is true"
               .into(),
         ));
      }
      // A typo here is invisible at runtime and silently collapses every
      // visitor behind the proxy into one shared budget, so refuse to start.
      for proxy in &config.config.trusted_proxies {
         if proxy.parse::<IpAddr>().is_err() {
            return Err(Error::InvalidConfig(format!(
               "config.trustedProxies entry {proxy:?} is not an IP address"
            )));
         }
      }
      if config.server.http_max_connections == 0 {
         return Err(Error::InvalidConfig(
            "server.httpMaxConnections must be greater than zero".into(),
         ));
      }
      if config.config.max_concurrent_reqs == 0 {
         return Err(Error::InvalidConfig(
            "config.maxConcurrentReqs must be greater than zero".into(),
         ));
      }
      if config.cache.max_entries == 0 {
         return Err(Error::InvalidConfig(
            "cache.maxEntries must be greater than zero".into(),
         ));
      }
      if config.cache.list_minutes == 0 || config.cache.rss_minutes == 0 {
         return Err(Error::InvalidConfig(
            "cache listMinutes and rssMinutes must be greater than zero".into(),
         ));
      }
      // Validate GIF transcoding config
      match config.gif_transcoding.mode {
         GifTranscodingMode::Local => {
            if Command::new("ffmpeg").arg("-version").output().is_err() {
               tracing::error!(
                  "gifTranscoding.mode is 'local' but ffmpeg was not found on PATH — falling back \
                   to 'off'"
               );
               config.gif_transcoding.mode = GifTranscodingMode::Off;
            }
         },
         GifTranscodingMode::External => {
            if config.gif_transcoding.external_domain.is_empty() {
               tracing::error!(
                  "gifTranscoding.mode is 'external' but externalDomain is empty — falling back \
                   to 'off'"
               );
               config.gif_transcoding.mode = GifTranscodingMode::Off;
            }
         },
         GifTranscodingMode::Off => {},
      }

      // The Kagi token file takes precedence over the inline value
      if !config.config.kagi_token_file.is_empty() {
         match fs::read_to_string(&config.config.kagi_token_file) {
            Ok(token) => {
               token.trim().clone_into(&mut config.config.kagi_token);
               tracing::info!("Loaded Kagi token from {}", config.config.kagi_token_file);
            },
            Err(err) => {
               tracing::error!(
                  "Failed to read kagiTokenFile '{}': {err}",
                  config.config.kagi_token_file
               );
            },
         }
      }
      if !config.config.kagi_token.is_empty() {
         tracing::info!("Kagi Translate enabled for server-side translations");
      }

      apply_donation_env(&mut config.donations);
      tracing::info!(
         stripe = config.donations.stripe.configured(),
         nowpayments = config.donations.nowpayments.configured(),
         lzt = config.donations.lzt.configured(),
         "donation providers"
      );

      let scheme = if config.server.https { "https" } else { "http" };
      let default_port = if config.server.https { 443 } else { 80 };
      let url_port = config.server.public_port.unwrap_or(config.server.port);
      if url_port == default_port || config.server.hostname.contains(':') {
         config.url_prefix = format!("{scheme}://{}", config.server.hostname);
      } else {
         config.url_prefix = format!("{scheme}://{}:{}", config.server.hostname, url_port);
      }
      Ok(config)
   }

   /// Get the full URL prefix for generating links.
   pub fn url_prefix(&self) -> &str {
      &self.url_prefix
   }
}
