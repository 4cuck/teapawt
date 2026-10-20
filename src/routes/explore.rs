//! Explore: For you, Trending, News, Sports, Entertainment, Highlights, and
//! whatever extra tabs the account turned on.

use std::time::Duration;

use axum::{
   Router,
   extract::{
      Path,
      Query,
      State,
   },
   response::{
      Html,
      IntoResponse as _,
      Redirect,
      Response,
   },
   routing::get,
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use tokio::time::timeout;

use super::helpers;
use crate::{
   AppState,
   cache::{
      Hit,
      keys as cache_keys,
      ttl,
   },
   error::{
      Error,
      Result,
   },
   types::{
      ExploreFeed,
      ExploreItem,
      ExploreTab,
      Prefs,
      Trend,
      merge_tabs,
      normalize_slug,
      timeline_name,
   },
   utils::formatters,
   views::{
      explore as explore_view,
      layout,
   },
};

const PREVIEW_LIMIT: usize = 8;
const HOME_WAIT: Duration = Duration::from_secs(8);

#[derive(Debug, Deserialize)]
struct ExploreQuery {
   cursor: Option<String>,
   scroll: Option<String>,
}

pub fn router() -> Router<AppState> {
   Router::new()
      .route("/explore", get(explore_home))
      .route("/explore/{tab}", get(explore_tab))
}

/// Trending topics for the front page. A slow or failed fetch leaves the
/// page up; the Explore link still works.
pub async fn trending_preview(state: &AppState) -> Vec<Trend> {
   match timeout(HOME_WAIT, load_explore(state, "trending", None)).await {
      Ok(Ok(feed)) => preview_trends(&feed),
      _ => Vec::new(),
   }
}

async fn explore_home(
   State(state): State<AppState>,
   jar: CookieJar,
   Query(query): Query<ExploreQuery>,
) -> Result<Response> {
   render(&state, jar, "for-you", query).await
}

async fn explore_tab(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(raw): Path<String>,
   Query(query): Query<ExploreQuery>,
) -> Result<Response> {
   let Some(slug) = normalize_slug(&raw) else {
      return Err(Error::InvalidUrl("Invalid explore tab".into()));
   };
   let cursor = cleaned_cursor(query.cursor.clone());
   if slug == "for-you" || raw != slug {
      let path = page_path(&slug, cursor.as_deref());
      return Ok(Redirect::to(&path).into_response());
   }
   render(&state, jar, &slug, query).await
}

async fn render(
   state: &AppState,
   jar: CookieJar,
   slug: &str,
   query: ExploreQuery,
) -> Result<Response> {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let cursor = cleaned_cursor(query.cursor);
   let is_scroll = query.scroll.as_deref() == Some("true");
   let feed = match load_explore(state, slug, cursor.as_deref()).await {
      Ok(feed) => feed,
      Err(err) => return Ok(helpers::api_error_titled(&state.config, &err, "Explore")),
   };
   let continuation = cursor.is_some();
   let mut feed = feed;
   // A cursor that comes back with no rows is the end of a finite list.
   // Leaving it in place makes infinite scroll ask for pages that will not exist.
   if feed.items.is_empty() {
      feed.bottom = None;
   }
   remember_posts(state, &feed, cursor.is_none() && !is_scroll);
   let tabs = tabs_for(state, &feed);
   let label = if let Some(tab) = tabs.iter().find(|tab| tab.slug == slug) {
      tab.label.as_str()
   } else {
      "Explore"
   };

   if is_scroll {
      let fragment = explore_view::render_explore_feed(
         &feed,
         slug,
         &state.config,
         &prefs,
         feed.bottom.as_deref(),
         continuation,
      );
      return Ok(Html(fragment.into_string()).into_response());
   }

   let games = if slug == "sports" {
      super::game::sports_games(state).await
   } else {
      Vec::new()
   };
   let content = explore_view::render_explore_page(
      &feed,
      &tabs,
      slug,
      &state.config,
      &prefs,
      feed.bottom.as_deref(),
      continuation,
      &games,
   );
   let title = if slug == "for-you" {
      "Explore".to_owned()
   } else {
      format!("{label} · Explore")
   };
   let description = format!(
      "{label} on Explore. Trending topics, news, sports, and highlights, without an account or \
       tracking."
   );
   let referer = page_path(slug, cursor.as_deref());
   let canonical = x_url(slug);
   let markup = layout::PageLayout::new(&state.config, &title, content)
      .description(&description)
      .prefs(&prefs)
      .canonical(&canonical)
      .referer(&referer)
      .render();
   Ok(Html(markup.into_string()).into_response())
}

async fn load_explore(state: &AppState, slug: &str, cursor: Option<&str>) -> Result<ExploreFeed> {
   let key = cache_keys::explore(slug, cursor);
   if cursor.is_none()
      && let Some(feed) = helpers::swr_take::<ExploreFeed, _, _>(state, &key, {
         let slug = slug.to_owned();
         let key = key.clone();
         move |app| {
            async move {
               if let Ok(feed) = fetch_explore(&app, &slug, None).await {
                  store_explore(&app, &key, &feed, false);
               }
            }
         }
      })
   {
      return Ok(feed);
   }
   if cursor.is_some()
      && let Some(Hit::Fresh(feed) | Hit::Stale(feed)) = state.cache.lookup(&key)
   {
      return Ok(feed);
   }

   let feed = fetch_explore(state, slug, cursor).await?;
   store_explore(state, &key, &feed, cursor.is_some());
   Ok(feed)
}

async fn fetch_explore(state: &AppState, slug: &str, cursor: Option<&str>) -> Result<ExploreFeed> {
   if slug == "for-you" {
      return fetch_for_you(state, cursor).await;
   }
   let mut feed = state
      .api
      .explore_timeline(timeline_name(slug), cursor)
      .await?;
   // The highlights timeline id is accepted, but this account's copy comes
   // back with no rows. The curated headlines live on For you as Today's News.
   if slug == "highlights" && feed.items.is_empty() && cursor.is_none() {
      if let Ok(home) = fetch_for_you(state, None).await {
         let news = section_named(&home, "news");
         if !news.is_empty() {
            feed.items = news;
            feed.bottom = None;
         }
      }
   }
   // Trending, news, sports, entertainment, and highlights are finite lists.
   // X still returns a cursor after the last row, and the next page is empty,
   // so infinite scroll would keep asking for items that will not exist.
   if matches!(
      slug,
      "trending" | "news" | "sports" | "entertainment" | "highlights"
   ) {
      feed.bottom = None;
   }
   if feed.items.is_empty() {
      feed.bottom = None;
   }
   Ok(feed)
}

/// Rows under the first section header whose title contains `needle`.
fn section_named(feed: &ExploreFeed, needle: &str) -> Vec<ExploreItem> {
   let mut taking = false;
   let mut rows = Vec::new();
   for item in &feed.items {
      match item {
         ExploreItem::Header(title) => {
            if taking {
               break;
            }
            if title.to_ascii_lowercase().contains(needle) {
               taking = true;
               rows.push(item.clone());
            }
         },
         ExploreItem::Trend(_) | ExploreItem::Tweet(_) if taking => rows.push(item.clone()),
         ExploreItem::Trend(_) | ExploreItem::Tweet(_) => {},
      }
   }
   rows
}

/// `/explore` is `ExplorePage`. If that answer has no rows, the For you
/// timeline is the same tab under `GenericTimelineById`.
async fn fetch_for_you(state: &AppState, cursor: Option<&str>) -> Result<ExploreFeed> {
   match state.api.explore_page(cursor).await {
      Ok(feed) if !feed.items.is_empty() || cursor.is_some() => Ok(feed),
      Ok(feed) => {
         match state.api.explore_timeline("for_you", None).await {
            Ok(mut timeline) => {
               if timeline.tabs.is_empty() {
                  timeline.tabs = feed.tabs;
               }
               Ok(timeline)
            },
            Err(_) if !feed.tabs.is_empty() || !feed.items.is_empty() => Ok(feed),
            Err(err) => Err(err),
         }
      },
      Err(err) => {
         state
            .api
            .explore_timeline("for_you", cursor)
            .await
            .or(Err(err))
      },
   }
}

fn store_explore(state: &AppState, key: &str, feed: &ExploreFeed, later_page: bool) {
   let (fresh_secs, stale_secs) = if later_page {
      (ttl::EXPLORE_CURSOR, ttl::EXPLORE_CURSOR_STALE)
   } else {
      (ttl::EXPLORE, ttl::EXPLORE_STALE)
   };
   state.cache.set_swr(key, feed, fresh_secs, stale_secs);
   if !later_page && !feed.tabs.is_empty() {
      state.cache.set_swr(
         &cache_keys::explore_tabs(),
         &feed.tabs,
         fresh_secs,
         stale_secs,
      );
   }
}

fn tabs_for(state: &AppState, feed: &ExploreFeed) -> Vec<ExploreTab> {
   let discovered = if feed.tabs.is_empty() {
      match state
         .cache
         .lookup::<Vec<ExploreTab>>(&cache_keys::explore_tabs())
      {
         Some(Hit::Fresh(tabs) | Hit::Stale(tabs)) => tabs,
         None => Vec::new(),
      }
   } else {
      feed.tabs.clone()
   };
   merge_tabs(&discovered)
}

fn preview_trends(feed: &ExploreFeed) -> Vec<Trend> {
   feed
      .items
      .iter()
      .filter_map(|item| {
         match item {
            ExploreItem::Trend(trend) => Some(trend.clone()),
            ExploreItem::Header(_) | ExploreItem::Tweet(_) => None,
         }
      })
      .take(PREVIEW_LIMIT)
      .collect()
}

fn remember_posts(state: &AppState, feed: &ExploreFeed, warm: bool) {
   let tweets: Vec<_> = feed
      .items
      .iter()
      .filter_map(|item| {
         match item {
            ExploreItem::Tweet(tweet) => Some((**tweet).clone()),
            ExploreItem::Header(_) | ExploreItem::Trend(_) => None,
         }
      })
      .collect();
   if tweets.is_empty() {
      return;
   }
   helpers::remember_tweets(state, &tweets);
   if warm {
      helpers::prefetch_profiles(state, &tweets);
   }
}

fn cleaned_cursor(cursor: Option<String>) -> Option<String> {
   cursor.filter(|cursor| !cursor.is_empty())
}

fn page_path(slug: &str, cursor: Option<&str>) -> String {
   let base = explore_view::tab_href(slug);
   match cursor {
      Some(cursor) => formatters::cursor_url(&base, cursor),
      None => base,
   }
}

fn x_url(slug: &str) -> String {
   if slug == "for-you" {
      "https://x.com/explore".to_owned()
   } else {
      format!("https://x.com/explore/tabs/{}", timeline_name(slug))
   }
}
