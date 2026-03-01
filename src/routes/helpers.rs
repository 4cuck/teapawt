use axum::{
   http::header,
   response::{
      IntoResponse as _,
      Response,
   },
};

use crate::{
   AppState,
   cache::{
      keys as cache_keys,
      ttl,
   },
   error::Result,
   types::{
      Timeline,
      Tweet,
      Tweets,
      User,
   },
};

/// Fetch a user, using cache when available.
pub async fn get_cached_user(state: &AppState, username: &str) -> Result<User> {
   let cache_key = cache_keys::user(username);
   if let Some(cached) = state.cache.get::<User>(&cache_key) {
      return Ok(cached);
   }
   let fetched = state.api.get_user(username).await?;
   state.cache.set(&cache_key, &fetched, ttl::DEFAULT);
   Ok(fetched)
}

/// Build an RSS response with `Content-Type` and `Min-Id` headers.
pub fn rss_response(rss: String, tweets: &[Tweet]) -> Response {
   let min_id = tweets.iter().map(|tweet| tweet.id).min();
   let mut response = (
      [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
      rss,
   )
      .into_response();
   if let Some(id) = min_id {
      response.headers_mut().insert(
         header::HeaderName::from_static("min-id"),
         header::HeaderValue::from(id),
      );
   }
   response
}

/// Check RSS cache and return early if hit.
pub fn check_rss_cache(state: &AppState, key: &str) -> Option<Response> {
   let cached = state.cache.get::<String>(key)?;
   Some(
      (
         [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
         cached,
      )
         .into_response(),
   )
}

/// Cache an RSS result.
pub fn cache_rss(state: &AppState, key: &str, rss: &str) {
   let owned = rss.to_owned();
   state
      .cache
      .set(key, &owned, state.config.cache.rss_minutes * 60);
}

/// Extract tweet groups and cursor from a Timeline.
/// Preserves conversation grouping: each inner [`Vec<Tweet>`] is a conversation
/// thread (parent → reply chain) from a single `profile-conversation-*` entry.
pub fn extract_timeline(timeline: Timeline) -> (Vec<Tweets>, Option<String>) {
   (timeline.content, timeline.bottom)
}
