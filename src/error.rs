use std::{
   io,
   result,
   sync::{
      OnceLock,
      atomic::{
         AtomicUsize,
         Ordering,
      },
   },
};

use axum::{
   http::{
      HeaderValue,
      StatusCode,
      header,
   },
   response::{
      Html,
      IntoResponse,
      Response,
   },
};
use thiserror::Error;
use toml::de::Error as TomlError;

use crate::{
   utils::html_escape,
   views::layout::{
      FONTELLO_CSS,
      STYLE_CSS,
   },
};

pub type Result<T> = result::Result<T, Error>;

/// Operator-facing wording, shared by the error page and by `?` propagation so
/// the two cannot drift.
#[derive(Debug, Default)]
pub struct Messages {
   pub client_budget:  Vec<String>,
   pub rate_limited:   Vec<String>,
   pub internal_error: Vec<String>,
}

static MESSAGES: OnceLock<Messages> = OnceLock::new();

/// Install the configured wording. Called once at startup.
pub fn set_messages(messages: Messages) {
   let _ = MESSAGES.set(messages);
}

fn configured() -> &'static Messages {
   static EMPTY: Messages = Messages {
      client_budget:  Vec::new(),
      rate_limited:   Vec::new(),
      internal_error: Vec::new(),
   };
   MESSAGES.get().unwrap_or(&EMPTY)
}

/// Rotate through the configured wordings, one per refusal.
fn pick(configured: &'static [String], fallback: &'static str) -> &'static str {
   static NEXT: AtomicUsize = AtomicUsize::new(0);
   match *configured {
      [] => fallback,
      [ref only] => only,
      ref many => &many[NEXT.fetch_add(1, Ordering::Relaxed) % many.len()],
   }
}

pub const CLIENT_BUDGET_MESSAGE: &str = "You are requesting uncached pages faster than this \
                                         instance can fetch them. Cached pages still work.";
pub const RATE_LIMITED_MESSAGE: &str =
   "This instance has reached its upstream rate limit. Please try again later.";
pub const INTERNAL_ERROR_MESSAGE: &str = "An internal service error occurred.";
pub const UNAVAILABLE_MESSAGE: &str = "The service has no available upstream sessions.";

/// Seconds a refused caller is asked to wait, sent as `Retry-After` with a
/// 429. Account windows are staggered, so some quota is back within a minute,
/// and it is the wait the Anti-DDoS check asks for.
pub const RETRY_AFTER_SECS: u64 = 60;

#[must_use]
pub fn client_budget_message() -> &'static str {
   pick(&configured().client_budget, CLIENT_BUDGET_MESSAGE)
}

#[must_use]
pub fn rate_limited_message() -> &'static str {
   pick(&configured().rate_limited, RATE_LIMITED_MESSAGE)
}

#[must_use]
pub const fn unavailable_message() -> &'static str {
   UNAVAILABLE_MESSAGE
}

#[must_use]
pub fn internal_error_message() -> &'static str {
   pick(&configured().internal_error, INTERNAL_ERROR_MESSAGE)
}

#[derive(Error, Debug)]
pub enum Error {
   #[error("Configuration error: {0}")]
   Config(#[from] TomlError),

   #[error("Configuration error: {0}")]
   InvalidConfig(String),

   #[error("IO error: {0}")]
   Io(#[from] io::Error),

   #[error("HTTP request error: {0}")]
   Http(String),

   #[error("JSON parsing error: {0}")]
   Json(#[from] serde_json::Error),

   #[error("Twitter API error: {0}")]
   TwitterApi(String),

   #[error("Rate limited")]
   RateLimited,

   /// Cloudflare/WAF 429 against this IP. Other cookie sessions share the
   /// path, so retrying them only marks the whole pool as exhausted.
   #[error("Upstream throttled this IP")]
   IpThrottled,

   #[error("Session rejected: {0}")]
   SessionRejected(String),

   #[error("Client budget exhausted")]
   ClientBudgetExhausted,

   #[error("Upstream returned nothing")]
   TransientUpstream,

   /// X returned an ambiguous search 404. The account is cooling down or
   /// awaiting a settings check; do not retry this error on the same account.
   #[error("Search refused this account")]
   SearchRefused,

   /// X answered `SearchTimeline` with HTTP 200, an empty `search_timeline`,
   /// and an error on `search_by_raw_query.search_timeline.timeline` (typically
   /// `"Internal server error"`). Suspended accounts return this on every tweet
   /// search (`Latest`/`Top`/`Media`), and X occasionally fails an individual
   /// query the same way.
   #[error("X search backend failed this query")]
   SearchTimelineFailed,

   #[error("X account settings returned 404 (code 34)")]
   AccountSettingsNotFound,

   /// No transaction id could be minted, so the request was never sent. X
   /// answers a cookie-session write without one with a bodyless 404, which is
   /// indistinguishable from an account X refuses, so the write is skipped
   /// instead of being sent and blamed on the account.
   #[error("No transaction id is available yet")]
   NoTransactionId,

   #[error("Not found: {0}")]
   NotFound(String),

   #[error("User suspended: {0}")]
   UserSuspended(String),

   #[error("User not found: {0}")]
   UserNotFound(String),

   #[error("Tweet not found: {0}")]
   TweetNotFound(String),

   #[error("Protected user: {0}")]
   ProtectedUser(String),

   #[error("No sessions available")]
   NoSessions,

   #[error("Invalid URL: {0}")]
   InvalidUrl(String),

   /// X's Search Content Control tool rejected this exact query string.
   #[error("Search query denylisted")]
   SearchDenylisted,

   #[error("HMAC verification failed")]
   HmacVerification,

   #[error("Internal error: {0}")]
   Internal(String),
}

impl Error {
   /// Account quota or the edge block. Either one can be covered by a cached
   /// page.
   #[must_use]
   pub const fn is_upstream_limit(&self) -> bool {
      matches!(self, Self::RateLimited | Self::IpThrottled)
   }

   /// How long to ask the caller to wait, for the refusals that are about
   /// pace and not about the page: the three that answer 429.
   #[must_use]
   pub const fn retry_after(&self) -> Option<u64> {
      match *self {
         Self::RateLimited | Self::IpThrottled | Self::ClientBudgetExhausted => {
            Some(RETRY_AFTER_SECS)
         },
         _ => None,
      }
   }

   /// Status, heading and reader-facing text for this failure.
   #[must_use]
   pub fn presentation(&self) -> (StatusCode, &'static str, &str) {
      match *self {
         Self::NotFound(ref message) => (StatusCode::NOT_FOUND, "Not found", message),
         Self::UserNotFound(_) => {
            (
               StatusCode::NOT_FOUND,
               "Account not found",
               "No account exists with that name, or X has removed it.",
            )
         },
         Self::TweetNotFound(_) => {
            (
               StatusCode::NOT_FOUND,
               "Post not found",
               "That post has been deleted, or the account that made it is gone.",
            )
         },
         Self::UserSuspended(_) => {
            (
               StatusCode::FORBIDDEN,
               "Account suspended",
               "X has suspended this account, so its posts are not available here.",
            )
         },
         Self::ProtectedUser(_) => {
            (
               StatusCode::FORBIDDEN,
               "Protected account",
               "This account's posts are only visible to followers it has approved.",
            )
         },
         Self::InvalidUrl(ref message) => (StatusCode::BAD_REQUEST, "Bad request", message),
         Self::SearchDenylisted => {
            (
               StatusCode::BAD_REQUEST,
               "Search blocked",
               "X will not run that search.",
            )
         },
         Self::HmacVerification => {
            (
               StatusCode::FORBIDDEN,
               "Forbidden",
               "The media URL signature is invalid.",
            )
         },
         Self::ClientBudgetExhausted => {
            (
               StatusCode::TOO_MANY_REQUESTS,
               "Slow down",
               client_budget_message(),
            )
         },
         Self::RateLimited | Self::IpThrottled => {
            (
               StatusCode::TOO_MANY_REQUESTS,
               "Try again",
               rate_limited_message(),
            )
         },
         Self::TransientUpstream => {
            (
               StatusCode::SERVICE_UNAVAILABLE,
               "Try again",
               "X did not return a result. Refresh in a moment.",
            )
         },
         // A 404 never proved the query had no matches. A valid empty page
         // takes the normal success path instead.
         Self::SearchRefused => {
            (
               StatusCode::SERVICE_UNAVAILABLE,
               "Search temporarily unavailable",
               "X did not return a search page. Try again later.",
            )
         },
         Self::SearchTimelineFailed => {
            (
               StatusCode::SERVICE_UNAVAILABLE,
               "Search temporarily unavailable",
               "X did not return a search page. Try again in a moment.",
            )
         },
         // Out of sessions is an operator problem, not an upstream quota one.
         Self::NoSessions | Self::SessionRejected(_) => {
            (
               StatusCode::SERVICE_UNAVAILABLE,
               "Unavailable",
               unavailable_message(),
            )
         },
         _ => {
            (
               StatusCode::INTERNAL_SERVER_ERROR,
               "Error",
               internal_error_message(),
            )
         },
      }
   }
}

impl IntoResponse for Error {
   fn into_response(self) -> Response {
      let (status, _, msg) = self.presentation();

      if matches!(
         self,
         Error::TransientUpstream | Error::SearchRefused | Error::SearchTimelineFailed
      ) {
         tracing::trace!(error = ?self, "upstream empty");
      } else if status == StatusCode::TOO_MANY_REQUESTS {
         // A caller told to slow down, or to come back when the accounts have
         // calls again, is the limit working, not a failed request. At error
         // level the budget alone was 33,779 of a day's 33,870 error lines
         // and hid the other 91.
         tracing::debug!(error = ?self, "request refused");
      } else if status.is_server_error() {
         tracing::error!(error = ?self, "request failed");
      }

      let html = format!(
         r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<link rel="stylesheet" type="text/css" href="{STYLE_CSS}">
<link rel="stylesheet" type="text/css" href="{FONTELLO_CSS}">
<title>Error</title>
</head>
<body>
<nav><div class="inner-nav"><a class="site-name" href="/">teapawt</a></div></nav>
<div class="container"><div class="panel-container"><div class="error-panel"><span>{}</span></div></div></div>
</body>
</html>"#,
         html_escape(msg)
      );

      let mut response = (status, Html(html)).into_response();
      // A refusal or a failure is about this caller and this moment. Cloudflare
      // once kept the 429 page for a minute and served it to readers who had
      // not hit the limit, so neither is left for a shared cache to store.
      if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
         response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
         );
      }
      if let Some(seconds) = self.retry_after() {
         response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
      }
      response
   }
}

// Twitter API error codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwitterError {
   NoUserMatches       = 17,
   ProtectedUser       = 22,
   UserNotFound        = 50,
   UserSuspended       = 63,
   RateLimited         = 88,
   InvalidToken        = 89,
   TweetNotFound       = 144,
   TweetUnavailable    = 179,
   NoStatusFound       = 220,
   BadToken            = 239,
   Locked              = 326,
   TweetUnavailable421 = 421,
   TweetCensored       = 422,
}

impl TwitterError {
   pub const fn from_code(code: i64) -> Option<Self> {
      match code {
         17 => Some(Self::NoUserMatches),
         22 => Some(Self::ProtectedUser),
         50 => Some(Self::UserNotFound),
         63 => Some(Self::UserSuspended),
         88 => Some(Self::RateLimited),
         89 => Some(Self::InvalidToken),
         144 => Some(Self::TweetNotFound),
         179 => Some(Self::TweetUnavailable),
         220 => Some(Self::NoStatusFound),
         239 => Some(Self::BadToken),
         326 => Some(Self::Locked),
         421 => Some(Self::TweetUnavailable421),
         422 => Some(Self::TweetCensored),
         _ => None,
      }
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn a_rate_limit_answers_429_and_says_when_to_come_back() {
      for err in [
         Error::RateLimited,
         Error::IpThrottled,
         Error::ClientBudgetExhausted,
      ] {
         assert_eq!(err.retry_after(), Some(RETRY_AFTER_SECS));
         let response = err.into_response();
         assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
         assert_eq!(response.headers()[header::RETRY_AFTER], "60");
         assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
         );
      }
   }

   #[test]
   fn other_failures_stay_503_and_a_missing_page_can_still_be_cached() {
      let down = Error::TransientUpstream.into_response();
      assert_eq!(down.status(), StatusCode::SERVICE_UNAVAILABLE);
      assert!(down.headers().get(header::RETRY_AFTER).is_none());
      assert_eq!(down.headers()[header::CACHE_CONTROL], "private, no-store");

      let gone = Error::UserNotFound("nobody".into()).into_response();
      assert_eq!(gone.status(), StatusCode::NOT_FOUND);
      assert!(gone.headers().get(header::RETRY_AFTER).is_none());
      assert!(gone.headers().get(header::CACHE_CONTROL).is_none());
   }
}
