//! Public instance hostnames.
//!
//! Sitemap, canonical, and rewritten links follow the host a crawler or
//! visitor actually requested, so each Search Console property stays on its
//! own domain. An unknown host falls back to the configured `url_prefix`.

use std::future::Future;

use axum::http::{HeaderMap, HeaderName, header};

/// Apex hosts that serve this instance.
pub const APEX_HOSTS: &[&str] = &[
   "nitter.cf",
   "nitter.gq",
   "xitter.cf",
   "xitter.gq",
   "teapaw.org",
   "teapawt.net",
   "teapawt.org",
];

const FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");

tokio::task_local! {
   static REQUEST_ORIGIN: String;
}

/// Run `fut` with `origin` visible to [`current_origin`].
pub async fn scope<F>(origin: String, fut: F) -> F::Output
where
   F: Future,
{
   REQUEST_ORIGIN.scope(origin, fut).await
}

/// Origin chosen for this request, when middleware set one.
pub fn request_origin() -> Option<String> {
   REQUEST_ORIGIN.try_with(Clone::clone).ok()
}

/// Request origin, or `fallback` (usually `Config::url_prefix`) with no trailing slash.
pub fn current_origin(fallback: &str) -> String {
   request_origin().unwrap_or_else(|| fallback.trim_end_matches('/').to_owned())
}

/// `https://host` when `Host` or `X-Forwarded-Host` is one of the public instances.
pub fn origin_from_headers(headers: &HeaderMap, fallback: &str) -> String {
   for name in [header::HOST, FORWARDED_HOST] {
      let Some(host) = header_host(headers, name) else {
         continue;
      };
      if is_public_host(&host) {
         return format!("https://{host}");
      }
   }
   fallback.trim_end_matches('/').to_owned()
}

/// Host portion of an origin, without a scheme or trailing slash.
pub fn host_label(origin: &str) -> &str {
   origin
      .trim_start_matches("https://")
      .trim_start_matches("http://")
      .trim_end_matches('/')
}

fn header_host(headers: &HeaderMap, name: HeaderName) -> Option<String> {
   let raw = headers.get(name)?.to_str().ok()?;
   let host = raw.split(',').next()?.trim();
   let host = host.split(':').next()?.trim().to_ascii_lowercase();
   (!host.is_empty()).then_some(host)
}

/// IndexNow keys are 8–128 hex characters. Anything else is ignored.
pub fn indexnow_key(raw: &str) -> Option<&str> {
   let key = raw.trim();
   let hex = key.bytes().all(|byte| byte.is_ascii_hexdigit());
   ((8..=128).contains(&key.len()) && hex).then_some(key)
}

fn is_public_host(host: &str) -> bool {
   let apex = host.strip_prefix("www.").unwrap_or(host);
   APEX_HOSTS.contains(&apex)
}

#[cfg(test)]
mod tests {
   use axum::http::{HeaderMap, HeaderValue, header};

   use super::{indexnow_key, origin_from_headers};

   #[test]
   fn indexnow_key_accepts_only_hex() {
      assert_eq!(indexnow_key("abcdef0123456789"), Some("abcdef0123456789"));
      assert_eq!(indexnow_key("  abcdef0123456789 "), Some("abcdef0123456789"));
      assert_eq!(indexnow_key("short"), None);
      assert_eq!(indexnow_key("abcdef0123456789zz"), None);
   }

   #[test]
   fn public_host_becomes_https_origin() {
      let mut headers = HeaderMap::new();
      headers.insert(header::HOST, HeaderValue::from_static("xitter.cf"));
      assert_eq!(
         origin_from_headers(&headers, "https://nitter.cf"),
         "https://xitter.cf"
      );
   }

   #[test]
   fn www_host_is_kept() {
      let mut headers = HeaderMap::new();
      headers.insert(
         header::HOST,
         HeaderValue::from_static("www.teapawt.org:443"),
      );
      assert_eq!(
         origin_from_headers(&headers, "https://nitter.cf"),
         "https://www.teapawt.org"
      );
   }

   #[test]
   fn unknown_host_uses_fallback() {
      let mut headers = HeaderMap::new();
      headers.insert(header::HOST, HeaderValue::from_static("evil.example"));
      assert_eq!(
         origin_from_headers(&headers, "https://nitter.cf/"),
         "https://nitter.cf"
      );
   }
}
