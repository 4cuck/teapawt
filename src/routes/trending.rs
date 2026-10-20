//! Trending story page. Same path as X, on this host: `/i/trending/{id}`.

use axum::{
   Router,
   extract::{Path, State},
   response::{Html, IntoResponse as _, Response},
   routing::get,
};
use axum_extra::extract::CookieJar;

use crate::{
   AppState,
   api,
   cache::{Hit, keys as cache_keys, ttl},
   error::{Error, Result},
   types::{Prefs, TrendStory},
   views::{explore as explore_view, layout},
};

pub fn router() -> Router<AppState> {
   Router::new().route("/i/trending/{id}", get(story))
}

async fn story(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
) -> Result<Response> {
   if !is_story_id(&id) {
      return Err(Error::InvalidUrl("Invalid story".into()));
   }
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let story = load_story(&state, &id).await?;
   let content = explore_view::render_trend_story(&story, &state.config, &prefs);
   let title = if story.title.is_empty() {
      "Trending story".to_owned()
   } else {
      story.title.clone()
   };
   let description = if story.summary.is_empty() {
      "A trending story, without an account or tracking."
   } else {
      story.summary.as_str()
   };
   let referer = format!("/i/trending/{id}");
   let canonical = format!("https://x.com/i/trending/{id}");
   let markup = layout::PageLayout::new(&state.config, &title, content)
      .description(description)
      .prefs(&prefs)
      .referer(&referer)
      .canonical(&canonical)
      .render();
   Ok(Html(markup.into_string()).into_response())
}

async fn load_story(state: &AppState, id: &str) -> Result<TrendStory> {
   let key = cache_keys::trend_story(id);
   if let Some(Hit::Fresh(story) | Hit::Stale(story)) = state.cache.lookup(&key) {
      return Ok(story);
   }
   let data = state.api.ai_trend(id).await?;
   let mut story = api::parse_trend_story(id, &data);
   if story.tweets.is_empty() {
      let refs = api::story_post_refs(&data);
      for timeline_id in refs.timeline_ids.iter().take(2) {
         match state.api.timeline_by_id(timeline_id).await {
            Ok(page) => {
               for tweet in api::tweets_in_timeline(&page) {
                  if !story.tweets.iter().any(|seen| seen.id == tweet.id) {
                     story.tweets.push(tweet);
                  }
               }
               if !story.tweets.is_empty() {
                  break;
               }
            },
            Err(err) => tracing::debug!(story = %id, "trend timeline failed: {err}"),
         }
      }
      if story.tweets.is_empty() && !refs.tweet_ids.is_empty() {
         let ids: Vec<String> = refs.tweet_ids.iter().take(20).cloned().collect();
         match state.api.tweets_by_ids(&ids).await {
            Ok(hydrated) => story.tweets = api::parse_tweets_by_ids(&hydrated, &ids),
            Err(err) => tracing::debug!(story = %id, "trend posts failed: {err}"),
         }
      }
      if story.tweets.is_empty() {
         tracing::warn!(
            story = %id,
            timelines = refs.timeline_ids.len(),
            tweet_ids = refs.tweet_ids.len(),
            shape = %api::json_skeleton(&data),
            clues = %api::story_clues(&data),
            "trending story had no posts"
         );
      }
   }
   state
      .cache
      .set_swr(&key, &story, ttl::EXPLORE, ttl::EXPLORE_STALE);
   Ok(story)
}

fn is_story_id(id: &str) -> bool {
   (15..=22).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit())
}
