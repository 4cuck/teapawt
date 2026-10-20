use maud::{
   Markup,
   html,
};

use crate::{
   api::ScheduledGame,
   config::Config,
   types::{
      ExploreFeed,
      ExploreItem,
      ExploreTab,
      Prefs,
      Trend,
   },
   utils::formatters,
   views::{game::render_game_cards, tweet::TweetRenderer},
};

/// Compact "What's happening" list for the front page.
pub fn render_explore_preview(trends: &[Trend]) -> Markup {
   html! {
       section class="landing-explore" aria-labelledby="explore-preview-title" {
           div class="landing-explore-head" {
               h2 id="explore-preview-title" { "What's happening" }
               a href="/explore" { "Explore" }
           }
           @if trends.is_empty() {
               p class="landing-explore-empty" {
                   a href="/explore" { "News, sports, entertainment, and highlights" }
                   " — open Explore to browse them."
               }
           } @else {
               ol class="trend-list trend-list-compact" {
                   @for (index, trend) in trends.iter().enumerate() {
                       li {
                           a href=(trend_href(trend)) {
                               span class="trend-kicker" { (kicker(index + 1, &trend.category)) }
                               span class="trend-name" dir="auto" { (trend.name) }
                               @if !trend.detail.is_empty() {
                                   span class="trend-meta" { (trend.detail) }
                               }
                           }
                       }
                   }
               }
           }
       }
   }
}

/// Explore page body. Tabs are ordinary links, so the page works with
/// JavaScript off.
pub fn render_explore_page(
   feed: &ExploreFeed,
   tabs: &[ExploreTab],
   active: &str,
   config: &Config,
   prefs: &Prefs,
   cursor: Option<&str>,
   continuation: bool,
   games: &[ScheduledGame],
) -> Markup {
   html! {
       div class="timeline-container explore-page" {
           div class="timeline-header explore-head" {
               h1 { "Explore" }
               p class="explore-intro" {
                   "Topics people are posting about. Pick a tab, or search."
               }
               form method="get" action="/search" class="search-field explore-search" autocomplete="off" role="search" {
                   input type="hidden" name="f" value="tweets";
                   div class="pref-group pref-input pref-inline" {
                       label class="sr-only" for="explore-search" { "Search posts" }
                       input id="explore-search" type="text" name="q" placeholder="Search posts" dir="auto";
                   }
                   button type="submit" { span class="icon-search" {} }
               }
           }
           @if active == "sports" {
               (render_game_cards(games))
           }
           nav class="explore-tabbar" aria-label="Explore tabs" {
               ul class="tab explore-tabs" {
                   @for tab in tabs {
                       li class=(if tab.slug == active { "tab-item active" } else { "tab-item" }) {
                           a href=(tab_href(&tab.slug)) aria-current=(if tab.slug == active { "page" } else { "false" }) {
                               (tab.label)
                           }
                       }
                   }
               }
           }
           (render_explore_feed(feed, active, config, prefs, cursor, continuation))
       }
   }
}

/// The feed alone, for the "load more" request infinite scroll already makes.
pub fn render_explore_feed(
   feed: &ExploreFeed,
   active: &str,
   config: &Config,
   prefs: &Prefs,
   cursor: Option<&str>,
   continuation: bool,
) -> Markup {
   let mut rank = 0_usize;
   let mut rows = Vec::new();
   for item in &feed.items {
      match item {
         ExploreItem::Header(title) => {
            rows.push(html! {
                div class="timeline-header explore-section" { (title) }
            });
         },
         ExploreItem::Trend(trend) => {
            rank += 1;
            rows.push(render_trend(rank, trend, config, prefs));
         },
         ExploreItem::Tweet(tweet) => {
            rows.push(
               TweetRenderer::new(tweet, config, false)
                  .maybe_prefs(Some(prefs))
                  .render(),
            );
         },
      }
   }
   let more = cursor.map(|cursor| formatters::cursor_url(&tab_href(active), cursor));

   html! {
       div class="timeline explore-timeline" {
           @if rows.is_empty() {
               div class="timeline-header" {
                   @if continuation {
                       h2 class="timeline-end" { "No more items" }
                   } @else {
                       h2 class="timeline-none" { "Nothing here right now" }
                       p class="explore-intro" { "Try another tab, or search for a topic." }
                   }
               }
           } @else {
               @for row in &rows {
                   (row)
               }
               @if more.is_none() {
                   div class="timeline-footer" {
                       h2 class="timeline-end" { "No more items" }
                   }
               }
           }
           @if let Some(url) = more {
               @if !rows.is_empty() {
                   div class="show-more" {
                       a href=(url) { "Load more" }
                   }
               }
           }
       }
   }
}

fn render_trend(rank: usize, trend: &Trend, config: &Config, prefs: &Prefs) -> Markup {
   let image = if trend.image.is_empty() {
      String::new()
   } else {
      formatters::get_pic_url(&trend.image, config.config.base64_media, prefs.proxy_media)
   };
   html! {
       div class="timeline-item trend-row" {
           a class=(if image.is_empty() { "trend-link" } else { "trend-link has-thumb" }) href=(trend_href(trend)) {
               span class="trend-kicker" { (kicker(rank, &trend.category)) }
               span class="trend-name" dir="auto" { (trend.name) }
               @if !trend.detail.is_empty() {
                   span class="trend-meta" { (trend.detail) }
               }
               @if !image.is_empty() {
                   img class="trend-thumb" src=(image) alt="" loading="lazy";
               }
           }
       }
   }
}

fn kicker(rank: usize, category: &str) -> String {
   if category.is_empty() {
      format!("{rank} · Trending")
   } else {
      format!("{rank} · {category}")
   }
}

fn trend_href(trend: &Trend) -> String {
   if is_story_id(&trend.story_id) {
      format!("/i/trending/{}", trend.story_id)
   } else {
      format!(
         "/search?q={}&f=tweets",
         formatters::url_encode(&trend.query)
      )
   }
}

fn is_story_id(id: &str) -> bool {
   (15..=22).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit())
}

pub fn render_trend_story(
   story: &crate::types::TrendStory,
   config: &Config,
   prefs: &Prefs,
) -> Markup {
   let search = format!(
      "/search?q={}&f=tweets",
      formatters::url_encode(if story.title.is_empty() {
         &story.id
      } else {
         &story.title
      })
   );
   html! {
       div class="timeline-container story-page" {
           header class="explore-head" {
               h1 dir="auto" { (if story.title.is_empty() { "Trending story" } else { story.title.as_str() }) }
               @if !story.updated.is_empty() {
                   p class="explore-intro" { (story.updated) }
               }
           }
           @if !story.summary.is_empty() {
               section class="story-summary" {
                   p dir="auto" { (story.summary) }
               }
           }
           @if !story.related.is_empty() {
               section class="story-related" aria-label="Related" {
                   h2 { "Related" }
                   ul class="trend-list trend-list-compact" {
                       @for trend in story.related.iter().take(8) {
                           li {
                               a href=(trend_href(trend)) {
                                   span class="trend-name" dir="auto" { (trend.name) }
                               }
                           }
                       }
                   }
               }
           }
           div class="timeline explore-timeline" {
               @if story.tweets.is_empty() {
                   div class="timeline-header" {
                       h2 { "Posts" }
                   }
                   p class="explore-intro" {
                       "No posts came back with this story. "
                       a href=(search) { "Search it" }
                       "."
                   }
               } @else {
                   @for tweet in &story.tweets {
                       (TweetRenderer::new(tweet, config, false).maybe_prefs(Some(prefs)).render())
                   }
               }
               div class="timeline-end" {}
           }
       }
   }
}

pub fn tab_href(slug: &str) -> String {
   if slug == "for-you" {
      "/explore".to_owned()
   } else {
      format!("/explore/{slug}")
   }
}
