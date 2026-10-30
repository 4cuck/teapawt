mod api;
mod cache;
mod config;
mod donations;
mod error;
mod routes;
mod transcode;
mod translation;
mod types;
mod utils;
mod views;
mod web_bot_auth;

use std::{
   env, io,
   net::{IpAddr, SocketAddr},
   sync::Arc,
   time::Duration,
};

use axum::http::header;
use axum::{
   Router, ServiceExt,
   extract::Request,
   http::{Extensions, HeaderMap, StatusCode, Version, header::HeaderValue},
   middleware,
};
use tokio::net::TcpListener;
use tower::{Layer as _, limit::ConcurrencyLimitLayer};
use tower_http::{
   CompressionLevel,
   compression::{
      CompressionLayer,
      predicate::{Predicate as _, SizeAbove},
   },
   normalize_path::NormalizePathLayer,
   services::{ServeDir, ServeFile},
   set_header::SetResponseHeaderLayer,
   timeout::TimeoutLayer,
};
use tracing_appender::non_blocking::NonBlockingBuilder;
use tracing_subscriber::{fmt, layer::SubscriberExt as _, util::SubscriberInitExt as _};

use crate::{
   api::{ApiClient, HttpClient, ProxyPool, SessionPool},
   cache::Cache,
   config::{Config, GifTranscodingMode},
   transcode::GifTranscoder,
};

/// Application state shared across all routes.
#[derive(Clone)]
pub struct AppState {
   pub config: Arc<Config>,
   pub cache: Cache,
   pub api: ApiClient,
   pub http_client: HttpClient,
   pub gif_transcoder: Option<Arc<GifTranscoder>>,
   pub translation_limiter: Arc<translation::TranslationLimiter>,
   /// Peers permitted to name a different caller through forwarding headers.
   pub trusted_proxies: Arc<[IpAddr]>,
   pub donations: Arc<donations::DonationLedger>,
}

/// Log lines held for stdout before new ones are dropped.
const LOG_BUFFER_LINES: usize = 10_000;

/// Pages, feeds, JSON, scripts and playlists. Images, video and audio come
/// compressed already, and a video relay must stream byte ranges as they are.
fn compressible(_: StatusCode, _: Version, headers: &HeaderMap, _: &Extensions) -> bool {
   let Some(kind) = headers
      .get(header::CONTENT_TYPE)
      .and_then(|value| value.to_str().ok())
   else {
      return false;
   };
   let kind = kind.to_ascii_lowercase();
   kind.starts_with("text/")
      || kind.contains("json")
      || kind.contains("xml")
      || kind.contains("javascript")
      || kind.contains("mpegurl")
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
   // pm2 reads stdout. While it lags, a direct write blocks the thread that
   // is logging and every request scheduled on it; once this buffer is full,
   // lines are dropped instead.
   let (log_writer, _log_flush) = NonBlockingBuilder::default()
      .buffered_lines_limit(LOG_BUFFER_LINES)
      .lossy(true)
      .finish(io::stdout());
   tracing_subscriber::registry()
      .with(
         tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "teapot=debug,tower_http=debug".into()),
      )
      .with(fmt::layer().with_writer(log_writer))
      .init();

   // Load configuration
   let config_path =
      env::var("TEAPOT_CONF_FILE").unwrap_or_else(|_| "config/teapot.toml".to_owned());
   let config = Config::load(&config_path)?;
   let config = Arc::new(config);

   tracing::info!(
      "Starting {} on {}:{}",
      config.server.title,
      config.server.address,
      config.server.port
   );

   error::set_messages(error::Messages {
      client_budget: config.config.client_budget_message.clone(),
      rate_limited: config.config.rate_limited_message.clone(),
      internal_error: config.config.internal_error_message.clone(),
   });

   // Cloudflare caches the HTML (`s-maxage`). This process does not keep a
   // second copy: that map served timelines after new posts and, full, made
   // every request wait on the cache lock. Numeric ids are not pages; they
   // live in Postgres so the next view can ask for the header and the posts
   // at the same time.
   let cache = Cache::disabled();
   cache::ids::load().await;

   // Initialize session pool
   let sessions_path =
      env::var("TEAPOT_SESSIONS_FILE").unwrap_or_else(|_| "sessions.jsonl".to_owned());
   let sessions = SessionPool::load(&sessions_path, config.config.max_concurrent_reqs).await?;
   let mut proxies = ProxyPool::load(&config).await?;
   if let Some(pool) = proxies.take() {
      proxies = Some(pool.bind_sessions(&sessions.session_ids(), &config).await?);
   }

   // Find the newest browser version per engine that X's edge accepts, so
   // every session client built from here on presents one X will talk to.
   api::browser::probe_accepted(proxies.as_ref().map(ProxyPool::first)).await;

   let fallback_proxies = api::load_fallback_proxies(&config.config.fallback_proxies_file).await?;

   // Initialize API client
   let api = ApiClient::new(&config, sessions, proxies, fallback_proxies);
   api.spawn_filter_sync();
   api.spawn_search_check();
   api.spawn_latency_report();

   // Initialize GIF transcoder if local mode
   let http_client = HttpClient::new(
      &config.config.proxy,
      &config.config.proxy_auth,
      api::http::Purpose::Media,
   );
   let gif_transcoder = if config.gif_transcoding.mode == GifTranscodingMode::Local {
      match GifTranscoder::new(http_client.clone(), config.gif_transcoding.clone()).await {
         Ok(transcoder) => {
            tracing::info!("GIF transcoder enabled (local mode)");
            Some(Arc::new(transcoder))
         },
         Err(err) => {
            tracing::error!("Failed to initialize GIF transcoder: {err}");
            None
         },
      }
   } else {
      if config.gif_transcoding.mode == GifTranscodingMode::External {
         tracing::info!(
            "GIF transcoding enabled (external mode, domain: {})",
            config.gif_transcoding.external_domain
         );
      }
      None
   };

   // Validated at load, so anything here parses.
   let trusted_proxies = config
      .config
      .trusted_proxies
      .iter()
      .filter_map(|proxy| proxy.parse::<IpAddr>().ok())
      .collect::<Vec<_>>();
   if trusted_proxies.is_empty() {
      tracing::info!("No trustedProxies configured, billing clients by socket address");
   }

   // Create application state
   let state = AppState {
      config: Arc::clone(&config),
      cache,
      api,
      http_client,
      gif_transcoder,
      translation_limiter: Arc::new(translation::TranslationLimiter::new()),
      trusted_proxies: trusted_proxies.into(),
      donations: donations::DonationLedger::open(&config.donations.ledger_file),
   };

   // Build the router with specific routes before static files
   let static_dir = config.server.static_dir.clone();
   let app = Router::new()
        .merge(routes::router(&config.config.indexnow_key))
        // Serve static files at various paths
        .nest_service("/public", ServeDir::new(&static_dir))
        .nest_service("/css", ServeDir::new(format!("{static_dir}/css")))
        .nest_service("/js", ServeDir::new(format!("{static_dir}/js")))
        .nest_service("/fonts", ServeDir::new(format!("{static_dir}/fonts")))
        .nest_service("/md", ServeDir::new(format!("{static_dir}/md")))
        // Root-level static files
        .route_service("/logo.svg", ServeFile::new(format!("{static_dir}/logo.svg")))
        .route_service("/logo.png", ServeFile::new(format!("{static_dir}/logo.png")))
        .route_service("/favicon.ico", ServeFile::new(format!("{static_dir}/favicon.ico")))
        .route_service("/favicon-16x16.png", ServeFile::new(format!("{static_dir}/favicon-16x16.png")))
        .route_service("/favicon-32x32.png", ServeFile::new(format!("{static_dir}/favicon-32x32.png")))
        .route_service("/apple-touch-icon.png", ServeFile::new(format!("{static_dir}/apple-touch-icon.png")))
        .route_service("/android-chrome-192x192.png", ServeFile::new(format!("{static_dir}/android-chrome-192x192.png")))
        .route_service("/android-chrome-384x384.png", ServeFile::new(format!("{static_dir}/android-chrome-384x384.png")))
        .route_service("/android-chrome-512x512.png", ServeFile::new(format!("{static_dir}/android-chrome-512x512.png")))
        .route_service("/site.webmanifest", ServeFile::new(format!("{static_dir}/site.webmanifest")))
        .route_service("/opensearch.xml", ServeFile::new(format!("{static_dir}/opensearch.xml")))
        .layer(middleware::from_fn(routes::prefs_middleware))
        .layer(middleware::from_fn(routes::snowflake_guard))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            routes::client_middleware,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'none'; \
                 script-src 'self' https://challenges.cloudflare.com; \
                 style-src 'self' 'unsafe-inline'; \
                 img-src 'self' data: https://*.twimg.com https://video.pscp.tv https://*.video.pscp.tv; \
                 media-src 'self' blob: https://*.twimg.com; \
                 font-src 'self'; \
                 connect-src 'self' https://challenges.cloudflare.com https://*.twimg.com https://video.pscp.tv https://*.video.pscp.tv; \
                 frame-src https://challenges.cloudflare.com; \
                 manifest-src 'self'; \
                 form-action 'self' https://nitter.cf https://www.nitter.cf https://xitter.cf https://www.xitter.cf \
                 https://checkout.stripe.com https://nowpayments.io https://*.nowpayments.io \
                 https://lzt.market https://*.lzt.market; \
                 base-uri 'self'; \
                 frame-ancestors 'none'"
            ),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        ))
        .layer(middleware::from_fn(routes::negotiate))
        // Cloudflare compresses for visitors, but it reaches this process
        // through a tunnel, and a page sent as-is crosses that at full size.
        // Only gzip is built in. primp turns on tower-http's brotli and zstd
        // decoders, and with those the layer picks `br` from Cloudflare's
        // `gzip, br` and then sends the page uncompressed.
        .layer(
            CompressionLayer::new()
                .no_br()
                .no_zstd()
                .no_deflate()
                .quality(CompressionLevel::Fastest)
                .compress_when(SizeAbove::new(1024).and(compressible)),
        )
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(45),
        ))
        .layer(ConcurrencyLimitLayer::new(
            usize::try_from(config.server.http_max_connections).unwrap_or(usize::MAX),
        ))
        .with_state(state);

   // Start server
   let addr = SocketAddr::new(config.server.address.parse()?, config.server.port);
   let listener = TcpListener::bind(addr).await?;

   tracing::info!("Listening on {addr}");
   // `/user/` and `/user/status/id/` are the same pages as the slash-free paths.
   axum::serve(
      listener,
      ServiceExt::<Request>::into_make_service_with_connect_info::<SocketAddr>(
         NormalizePathLayer::trim_trailing_slash().layer(app),
      ),
   )
   .await?;

   Ok(())
}
