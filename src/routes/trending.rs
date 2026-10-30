//! Trending story page. Same path as X, on this host: `/i/trending/{id}`.

use axum::{
   Router,
   extract::{Path, Query, State},
   response::{Html, IntoResponse as _, Response},
   routing::get,
};
use axum_extra::extract::CookieJar;
use maud::html;
use serde::Deserialize;

use crate::{
   AppState,
   api,
   cache::{Hit, keys as cache_keys, ttl},
   error::{Error, Result},
   types::{Prefs, TrendStory, Tweet},
   views::{explore as explore_view, layout},
};

#[derive(Debug, Deserialize)]
struct StoryQuery {
   cursor:   Option<String>,
   timeline: Option<String>,
   scroll:   Option<String>,
}

pub fn router() -> Router<AppState> {
   Router::new().route("/i/trending/{id}", get(story))
}

async fn story(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
   Query(query): Query<StoryQuery>,
) -> Result<Response> {
   if !is_story_id(&id) {
      return Err(Error::InvalidUrl("Invalid story".into()));
   }
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let cursor = query.cursor.as_deref().filter(|cursor| acceptable_cursor(cursor));
   let timeline = query.timeline.as_deref().filter(|timeline| acceptable_timeline(timeline));
   if let (Some(timeline), Some(cursor)) = (timeline, cursor) {
      let page = load_timeline(&state, timeline, Some(cursor)).await?;
      let more = page
         .bottom
         .as_deref()
         .map(|next| more_url(&id, timeline, next));
      if query.scroll.as_deref() == Some("true") {
         let search = format!("/search?q={}&f=tweets", percent_encoding::utf8_percent_encode(&id, percent_encoding::NON_ALPHANUMERIC));
         let fragment = explore_view::render_story_posts(
            &page.tweets,
            &state.config,
            &prefs,
            more.as_deref(),
            &search,
         );
         return Ok(Html(fragment.into_string()).into_response());
      }
      let mut story = load_story(&state, &id).await?;
      story.tweets = page.tweets;
      story.timeline_id = timeline.to_owned();
      story.bottom = page.bottom;
      return Ok(story_page(&state, &prefs, &story));
   }
   let story = load_story(&state, &id).await?;
   Ok(story_page(&state, &prefs, &story))
}

fn story_page(state: &AppState, prefs: &Prefs, story: &TrendStory) -> Response {
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
   let referer = format!("/i/trending/{}", story.id);
   let canonical = format!("https://x.com/i/trending/{}", story.id);
   let more = story
      .bottom
      .as_deref()
      .filter(|_| !story.timeline_id.is_empty())
      .map(|cursor| more_url(&story.id, &story.timeline_id, cursor));
   let content = explore_view::render_trend_story(story, &state.config, prefs, more.as_deref());
   let scroll = html! {
       script src="/js/infiniteScroll.js?teapawt=7" defer {}
   };
   let mut markup = layout::PageLayout::new(&state.config, &title, content)
      .description(description)
      .prefs(prefs)
      .referer(&referer)
      .canonical(&canonical);
   if !prefs.infinite_scroll {
      markup = markup.head_extra(&scroll);
   }
   Html(markup.render().into_string()).into_response()
}

async fn load_story(state: &AppState, id: &str) -> Result<TrendStory> {
   let key = cache_keys::trend_story(id);
   if let Some(Hit::Fresh(story) | Hit::Stale(story)) = state.cache.lookup(&key) {
      return Ok(story);
   }
   let data = state.api.ai_trend(id).await?;
   let mut story = api::parse_trend_story(id, &data);
   let refs = api::story_post_refs(&data);
   if let Some(timeline_id) = refs.timeline_ids.first() {
      match load_timeline(state, timeline_id, None).await {
         Ok(page) if !page.tweets.is_empty() => {
            story.tweets = page.tweets;
            story.timeline_id.clone_from(timeline_id);
            story.bottom = page.bottom;
         },
         Ok(_) => {},
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
   state
      .cache
      .set_swr(&key, &story, ttl::EXPLORE, ttl::EXPLORE_STALE);
   Ok(story)
}

fn is_story_id(id: &str) -> bool {
   (15..=22).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit())
}

#[derive(Clone)]
struct Page {
   tweets: Vec<Tweet>,
   bottom: Option<String>,
}

async fn load_timeline(state: &AppState, timeline_id: &str, cursor: Option<&str>) -> Result<Page> {
   let key = cache_keys::trend_timeline(timeline_id, cursor);
   if let Some(Hit::Fresh(page) | Hit::Stale(page)) = state.cache.lookup(&key) {
      return Ok(page);
   }
   let data = state.api.timeline_by_id_at(timeline_id, cursor).await?;
   let mut parsed = api::timeline_posts(&data);
   if parsed.bottom.as_deref() == cursor {
      parsed.bottom = None;
   }
   let page = Page {
      tweets: parsed.tweets,
      bottom: parsed.bottom,
   };
   state
      .cache
      .set_swr(&key, &page, ttl::EXPLORE, ttl::EXPLORE_STALE);
   Ok(page)
}

fn more_url(story: &str, timeline: &str, cursor: &str) -> String {
   format!(
      "/i/trending/{story}?timeline={}&cursor={}",
      percent_encoding::utf8_percent_encode(timeline, percent_encoding::NON_ALPHANUMERIC),
      percent_encoding::utf8_percent_encode(cursor, percent_encoding::NON_ALPHANUMERIC),
   )
}

fn acceptable_timeline(timeline: &str) -> bool {
   (16..=512).contains(&timeline.len())
      && timeline
         .chars()
         .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '=' | '+' | '/'))
      && timeline.chars().any(|ch| ch.is_ascii_alphabetic())
}

fn acceptable_cursor(cursor: &str) -> bool {
   (1..=4096).contains(&cursor.len())
      && cursor
         .chars()
         .all(|ch| ch.is_ascii() && !ch.is_ascii_control() && ch != ' ')
}
