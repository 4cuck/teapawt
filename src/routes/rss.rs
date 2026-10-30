use axum::{
   Router,
   extract::{Path, Query, State},
   http::{HeaderValue, StatusCode, header},
   response::{IntoResponse, Response},
   routing::get,
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;

use super::helpers::{
   cache_rss, check_rss_cache, rss_response, rss_response_with_min_id, with_user,
};
use crate::{
   AppState,
   cache::{Hit, keys as cache_keys},
   error::{Error, Result},
   types::{Prefs, Profile, Query as SearchQuery, QueryKind, Tweet},
   views::rss as rss_view,
};

#[derive(Debug, Deserialize)]
pub struct RssQuery {
   pub cursor: Option<String>,
}

pub fn router() -> Router<AppState> {
   Router::new()
      .route("/{username}/rss", get(user_rss))
      .route("/{username}/with_replies/rss", get(user_replies_rss))
      .route("/{username}/reposts/rss", get(user_reposts_rss))
      .route("/{username}/media/rss", get(user_media_rss))
      .route("/{username}/search/rss", get(user_search_rss))
      .route("/{username}/status/{id}/rss", get(thread_rss))
      .route("/{username}/lists/{slug}/rss", get(list_by_slug_rss))
      .route("/search/rss", get(search_rss))
      .route("/i/lists/{id}/rss", get(list_rss))
}

/// Profile timeline already in cache, fresh, stale, or past the stale window.
fn cached_profile_page(state: &AppState, username: &str, cursor: Option<&str>) -> Option<Profile> {
   let key = match cursor {
      Some(cursor) => cache_keys::profile_cursor(username, cursor),
      None => cache_keys::profile(username),
   };
   match state.cache.lookup(&key) {
      Some(Hit::Fresh(profile) | Hit::Stale(profile)) => Some(profile),
      None => state.cache.recall(&key),
   }
}

/// A proxied feed must not be served from the direct-media cache, or the other way around.
fn feed_key(key: String, proxy: bool) -> String {
   if proxy {
      format!("{key}:proxy")
   } else {
      key
   }
}

/// A search feed that has not pressed the checkbox.
fn search_feed_blocked() -> Response {
   let mut response = (
      StatusCode::FORBIDDEN,
      "Search feeds need the checkbox on the search page.",
   )
      .into_response();
   response.headers_mut().insert(
      header::CACHE_CONTROL,
      HeaderValue::from_static("private, no-store"),
   );
   response
}

/// Return a 404 error when RSS is disabled.
fn check_rss_enabled(state: &AppState) -> Result<()> {
   if !state.config.config.enable_rss {
      return Err(Error::NotFound("RSS feeds are disabled".to_owned()));
   }
   Ok(())
}

/// Which user timeline variant to fetch for RSS.
#[derive(Clone, Copy)]
enum UserRssKind {
   Tweets,
   Replies,
   Reposts,
   Media,
}

/// Unified user RSS handler -- handles tweets, replies, and media variants.
async fn user_rss_handler(
   state: &AppState,
   username: &str,
   cursor: Option<&str>,
   kind: UserRssKind,
   proxy: bool,
) -> Result<Response> {
   let (feed_kind, cache_key_fn): (&str, fn(&str) -> String) = match kind {
      UserRssKind::Tweets => ("tweets", cache_keys::rss_user),
      UserRssKind::Replies => ("replies", cache_keys::rss_replies),
      UserRssKind::Reposts => ("reposts", cache_keys::rss_reposts),
      UserRssKind::Media => ("media", cache_keys::rss_media),
   };

   // Check RSS cache (first page only)
   let rss_cache_key = if cursor.is_none() {
      let key = feed_key(cache_key_fn(username), proxy);
      if let Some(cached) = check_rss_cache(state, &key) {
         return Ok(cached);
      }
      Some(key)
   } else {
      None
   };

   // The profile page already paid for these posts. Rebuild the feed from
   // that copy instead of spending another UserTweets call.
   if matches!(kind, UserRssKind::Tweets)
      && let Some(profile) = cached_profile_page(state, username, cursor.as_deref())
   {
      let tweets = profile
         .tweets
         .content
         .into_iter()
         .flatten()
         .collect::<Vec<_>>();
      let rss = rss_view::render_user_rss(&profile.user, &tweets, &state.config, feed_kind, proxy);
      if let Some(ref key) = rss_cache_key {
         cache_rss(state, key, &rss, tweets.iter().map(|tweet| tweet.id).min());
      }
      return Ok(rss_response(rss, &tweets));
   }

   let (user, timeline) = with_user(state, username, |user_id| async move {
      match kind {
         UserRssKind::Tweets => state.api.get_user_tweets(&user_id, cursor).await,
         UserRssKind::Replies => {
            state
               .api
               .get_user_tweets_and_replies(&user_id, cursor)
               .await
         },
         UserRssKind::Reposts => state.api.get_user_reposts(&user_id, cursor).await,
         UserRssKind::Media => state.api.get_user_media(&user_id, cursor).await,
      }
   })
   .await?;
   let tweets = timeline?.content.into_iter().flatten().collect::<Vec<_>>();
   let rss = rss_view::render_user_rss(&user, &tweets, &state.config, feed_kind, proxy);

   if let Some(ref key) = rss_cache_key {
      cache_rss(state, key, &rss, tweets.iter().map(|tweet| tweet.id).min());
   }

   Ok(rss_response(rss, &tweets))
}

async fn user_rss(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(query): Query<RssQuery>,
) -> Result<Response> {
   check_rss_enabled(&state)?;
   let proxy = Prefs::from_cookies(&jar, &state.config).proxy_media;
   user_rss_handler(
      &state,
      &username,
      query.cursor.as_deref(),
      UserRssKind::Tweets,
      proxy,
   )
   .await
}

async fn user_replies_rss(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(query): Query<RssQuery>,
) -> Result<Response> {
   check_rss_enabled(&state)?;
   let proxy = Prefs::from_cookies(&jar, &state.config).proxy_media;
   user_rss_handler(
      &state,
      &username,
      query.cursor.as_deref(),
      UserRssKind::Replies,
      proxy,
   )
   .await
}

async fn user_reposts_rss(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(query): Query<RssQuery>,
) -> Result<Response> {
   check_rss_enabled(&state)?;
   let proxy = Prefs::from_cookies(&jar, &state.config).proxy_media;
   user_rss_handler(
      &state,
      &username,
      query.cursor.as_deref(),
      UserRssKind::Reposts,
      proxy,
   )
   .await
}

async fn user_media_rss(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(query): Query<RssQuery>,
) -> Result<Response> {
   check_rss_enabled(&state)?;
   let proxy = Prefs::from_cookies(&jar, &state.config).proxy_media;
   user_rss_handler(
      &state,
      &username,
      query.cursor.as_deref(),
      UserRssKind::Media,
      proxy,
   )
   .await
}

async fn search_rss(
   State(state): State<AppState>,
   jar: CookieJar,
   Query(query): Query<super::search::SearchQuery>,
) -> Result<Response> {
   check_rss_enabled(&state)?;
   let proxy = Prefs::from_cookies(&jar, &state.config).proxy_media;
   let search_query = query.query.as_deref().unwrap_or_default();
   if search_query.is_empty() {
      return Err(Error::InvalidUrl("Search query is required".into()));
   }

   let rss_cache_key = feed_key(cache_keys::rss_search(search_query), proxy);
   if let Some(cached) = check_rss_cache(&state, &rss_cache_key) {
      return Ok(cached);
   }
   if !super::search_gate::cleared(&state, &jar) {
      return Ok(search_feed_blocked());
   }

   let timeline = state
      .api
      .search(
         &SearchQuery::parse(search_query, QueryKind::Posts).build(),
         None,
         "Latest",
      )
      .await?;
   let tweets = timeline.content.into_iter().flatten().collect::<Vec<_>>();
   let rss = rss_view::render_search_rss(search_query, &tweets, &state.config, proxy);

   cache_rss(
      &state,
      &rss_cache_key,
      &rss,
      tweets.iter().map(|tweet| tweet.id).min(),
   );
   Ok(rss_response(rss, &tweets))
}

/// User search RSS - search tweets from a specific user.
#[derive(Debug, Deserialize)]
pub struct UserSearchRssQuery {
   #[serde(rename = "q")]
   pub query: Option<String>,
}

async fn user_search_rss(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(username): Path<String>,
   Query(query): Query<UserSearchRssQuery>,
) -> Result<Response> {
   check_rss_enabled(&state)?;
   let proxy = Prefs::from_cookies(&jar, &state.config).proxy_media;
   let search_query = query.query.as_deref().unwrap_or_default();
   if search_query.is_empty() {
      return Err(Error::InvalidUrl("Search query is required".into()));
   }

   let api_query = format!("from:{username} {search_query}");
   let rss_cache_key = feed_key(cache_keys::rss_user_search(&username, search_query), proxy);
   if let Some(cached) = check_rss_cache(&state, &rss_cache_key) {
      return Ok(cached);
   }
   if !super::search_gate::cleared(&state, &jar) {
      return Ok(search_feed_blocked());
   }

   let timeline = state.api.search(&api_query, None, "Latest").await?;
   let tweets = timeline.content.into_iter().flatten().collect::<Vec<_>>();
   let rss = rss_view::render_search_rss(
      &format!("from:{username} {search_query}"),
      &tweets,
      &state.config,
      proxy,
   );

   cache_rss(
      &state,
      &rss_cache_key,
      &rss,
      tweets.iter().map(|tweet| tweet.id).min(),
   );
   Ok(rss_response(rss, &tweets))
}

/// RSS feed for a conversation thread (main tweet + self-thread).
async fn thread_rss(
   State(state): State<AppState>,
   jar: CookieJar,
   Path((_username, id)): Path<(String, String)>,
) -> Result<Response> {
   check_rss_enabled(&state)?;
   let proxy = Prefs::from_cookies(&jar, &state.config).proxy_media;

   let rss_cache_key = feed_key(cache_keys::rss_thread(&id), proxy);
   if let Some(cached) = check_rss_cache(&state, &rss_cache_key) {
      return Ok(cached);
   }

   let conversation = state.api.get_conversation(&id, None, "Relevance").await?;

   // Collect thread tweet references: before → main → after
   let mut tweets: Vec<&Tweet> = Vec::new();
   for tweet in &conversation.before.content {
      if tweet.available {
         tweets.push(tweet);
      }
   }
   if conversation.tweet.available {
      tweets.push(&conversation.tweet);
   }
   for tweet in &conversation.after.content {
      if tweet.available {
         tweets.push(tweet);
      }
   }

   let rss = rss_view::render_thread_rss(&conversation.tweet, &tweets, &state.config, proxy);
   let min_id = tweets.iter().map(|tweet| tweet.id).min();
   cache_rss(&state, &rss_cache_key, &rss, min_id);
   Ok(rss_response_with_min_id(rss, min_id))
}

async fn list_rss(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
   Query(query): Query<RssQuery>,
) -> Result<Response> {
   check_rss_enabled(&state)?;
   let proxy = Prefs::from_cookies(&jar, &state.config).proxy_media;
   let rss_cache_key = if query.cursor.is_none() {
      let key = feed_key(cache_keys::rss_list(&id), proxy);
      if let Some(cached) = check_rss_cache(&state, &key) {
         return Ok(cached);
      }
      Some(key)
   } else {
      None
   };

   let (list, timeline) = tokio::join!(
      state.api.get_list(&id),
      state.api.get_list_tweets(&id, query.cursor.as_deref()),
   );
   let list = list?;
   let timeline = timeline?;
   let tweets = timeline.content.into_iter().flatten().collect::<Vec<_>>();
   let rss = rss_view::render_list_rss(&list, &tweets, &state.config, proxy);

   if let Some(ref key) = rss_cache_key {
      cache_rss(&state, key, &rss, tweets.iter().map(|tweet| tweet.id).min());
   }
   Ok(rss_response(rss, &tweets))
}

/// List RSS by owner username and slug.
async fn list_by_slug_rss(
   State(state): State<AppState>,
   jar: CookieJar,
   Path((username, slug)): Path<(String, String)>,
   Query(query): Query<RssQuery>,
) -> Result<Response> {
   check_rss_enabled(&state)?;
   let proxy = Prefs::from_cookies(&jar, &state.config).proxy_media;
   let rss_cache_key = if query.cursor.is_none() {
      let key = feed_key(cache_keys::rss_list_slug(&username, &slug), proxy);
      if let Some(cached) = check_rss_cache(&state, &key) {
         return Ok(cached);
      }
      Some(key)
   } else {
      None
   };

   let list = state.api.get_list_by_slug(&username, &slug).await?;
   let timeline = state
      .api
      .get_list_tweets(&list.id, query.cursor.as_deref())
      .await?;
   let tweets = timeline.content.into_iter().flatten().collect::<Vec<_>>();
   let rss = rss_view::render_list_rss(&list, &tweets, &state.config, proxy);

   if let Some(ref key) = rss_cache_key {
      cache_rss(&state, key, &rss, tweets.iter().map(|tweet| tweet.id).min());
   }
   Ok(rss_response(rss, &tweets))
}
