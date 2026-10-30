//! Turns an Explore GraphQL payload into tabs, trend rows, and posts.
//!
//! X nests those in a few different envelopes (`ExplorePage`,
//! `GenericTimelineById`, a module of stories, a single trend). The walk
//! below looks for the shapes instead of one fixed path, so a tab an account
//! added in its Explore settings still shows up.

use std::collections::HashSet;

use percent_encoding::percent_decode_str;
use serde_json::Value;

use super::tweet::parse_tweet_object;
use crate::{
   api::schema::TweetData,
   types::{
      ExploreFeed,
      ExploreItem,
      ExploreTab,
      Trend,
      Tweet,
      normalize_slug,
      tab_label,
   },
};

pub fn parse_explore(data: &Value) -> ExploreFeed {
   let mut feed = ExploreFeed::default();
   walk(data, &mut feed);
   dedupe(&mut feed);
   feed
}

/// Posts in a timeline payload, plus the cursor for the following page.
pub struct TimelinePosts {
   pub tweets: Vec<Tweet>,
   pub bottom: Option<String>,
}

pub fn timeline_posts(data: &Value) -> TimelinePosts {
   let feed = parse_explore(data);
   let tweets = feed
      .items
      .into_iter()
      .filter_map(|item| match item {
         ExploreItem::Tweet(tweet) => Some(*tweet),
         ExploreItem::Header(_) | ExploreItem::Trend(_) => None,
      })
      .collect::<Vec<_>>();
   let bottom = if tweets.is_empty() { None } else { feed.bottom };
   TimelinePosts { tweets, bottom }
}

fn walk(value: &Value, feed: &mut ExploreFeed) {
   let Value::Object(object) = value else {
      if let Value::Array(items) = value {
         for item in items {
            walk(item, feed);
         }
      }
      return;
   };

   if let Some(tab) = tab_from_value(value) {
      feed.tabs.push(tab);
   }
   if let Some(instructions) = object.get("instructions").and_then(Value::as_array) {
      for instruction in instructions {
         parse_instruction(instruction, feed);
      }
   }
   for (key, child) in object {
      if key != "instructions" {
         walk(child, feed);
      }
   }
}

fn parse_instruction(instruction: &Value, feed: &mut ExploreFeed) {
   if let Some(items) = instruction.get("moduleItems").and_then(Value::as_array) {
      push_module(None, items, "", feed);
   }

   let kind = instruction
      .get("type")
      .and_then(Value::as_str)
      .unwrap_or("");
   if kind == "TimelineReplaceEntry" {
      let replacing = instruction
         .get("entry_id_to_replace")
         .and_then(Value::as_str)
         .unwrap_or("");
      if is_bottom_cursor(replacing)
         && let Some(entry) = instruction.get("entry")
         && let Some(cursor) = cursor_value(entry)
      {
         feed.bottom = Some(cursor);
      }
      return;
   }

   if let Some(entries) = instruction.get("entries").and_then(Value::as_array) {
      for entry in entries {
         parse_entry(entry, feed);
      }
   } else if let Some(entry) = instruction.get("entry") {
      parse_entry(entry, feed);
   }
}

fn parse_entry(entry: &Value, feed: &mut ExploreFeed) {
   let id = entry.get("entryId").and_then(Value::as_str).unwrap_or("");
   if id.contains("promoted") {
      return;
   }
   if is_bottom_cursor(id) {
      if let Some(cursor) = cursor_value(entry) {
         feed.bottom = Some(cursor);
      }
      return;
   }
   if id.starts_with("cursor-top") {
      return;
   }
   let Some(content) = entry.get("content") else {
      return;
   };
   if let Some(items) = content.get("items").and_then(Value::as_array) {
      push_module(header_text(content).as_deref(), items, id, feed);
      return;
   }
   if let Some(item) = content.get("itemContent") {
      push_content(item, id, feed);
   }
}

fn push_module(
   header: Option<&str>,
   items: &[Value],
   parent_id: &str,
   feed: &mut ExploreFeed,
) {
   let start = feed.items.len();
   for item in items {
      let entry_id = item
         .get("entryId")
         .and_then(Value::as_str)
         .unwrap_or(parent_id);
      let content = item
         .get("item")
         .and_then(|inner| inner.get("itemContent"))
         .or_else(|| item.get("itemContent"))
         .unwrap_or(item);
      push_content(content, entry_id, feed);
   }
   if feed.items.len() > start
      && let Some(header) = header.filter(|text| useful_header(text))
   {
      feed
         .items
         .insert(start, ExploreItem::Header(header.to_owned()));
   }
}

fn push_content(content: &Value, entry_id: &str, feed: &mut ExploreFeed) {
   if content.get("promotedMetadata").is_some() || content.get("promoted_metadata").is_some() {
      return;
   }
   if let Some(tab) = tab_from_value(content) {
      feed.tabs.push(tab);
      return;
   }
   if let Some(tweet) = tweet_from(content) {
      feed.items.push(ExploreItem::Tweet(Box::new(tweet)));
      return;
   }
   if let Some(trend) = trend_from(content, entry_id) {
      feed.items.push(ExploreItem::Trend(trend));
   }
}

fn tweet_from(content: &Value) -> Option<Tweet> {
   let result = content.get("tweet_results")?.get("result")?;
   let data: TweetData = serde_json::from_value(result.clone()).ok()?;
   let tweet = parse_tweet_object(&data).ok()?;
   tweet.available.then_some(tweet)
}

fn trend_from(content: &Value, entry_id: &str) -> Option<Trend> {
   let item_type = content
      .get("itemType")
      .and_then(Value::as_str)
      .or_else(|| content.get("__typename").and_then(Value::as_str))
      .unwrap_or("");
   if item_type.contains("User") || item_type.contains("Prompt") || item_type.contains("Cursor") {
      return None;
   }
   let name = text_at(content, "name").or_else(|| text_at(content, "title"))?;
   let metadata = content
      .get("trend_metadata")
      .or_else(|| content.get("trendMetadata"));
   let looks_like_trend = metadata.is_some()
      || content.get("trend_url").is_some()
      || content.get("trendUrl").is_some()
      || content.get("is_ai_trend").is_some()
      || content.get("isAiTrend").is_some()
      || item_type.contains("Trend")
      || content.get("social_context").is_some()
      || content.get("socialContext").is_some();
   if !looks_like_trend {
      return None;
   }

   let url = nested_url(content.get("trend_url"))
      .or_else(|| nested_url(content.get("trendUrl")))
      .or_else(|| metadata.and_then(|meta| nested_url(meta.get("url"))));
   let query = url
      .as_deref()
      .map(|url| search_query(url, &name))
      .unwrap_or_else(|| name.clone());
   let (category, detail) = trend_context(content, metadata);
   let image = trend_image(content);
   let story_id = story_id(entry_id, content, url.as_deref());
   Some(Trend {
      name,
      category,
      detail,
      query,
      image,
      story_id,
   })
}

/// The snowflake in `/i/trending/{id}`. Explore puts it on the entry id
/// (`stories-…-trend-{id}`) and sometimes on the trend URL.
fn story_id(entry_id: &str, content: &Value, url: Option<&str>) -> String {
   snowflake_after(entry_id, "trend-")
      .or_else(|| url.and_then(snowflake_in_trending_url))
      .or_else(|| story_id_in(content))
      .unwrap_or_default()
}

fn snowflake_after(text: &str, marker: &str) -> Option<String> {
   let rest = text.rsplit_once(marker)?.1;
   let id = rest.split(|ch: char| !ch.is_ascii_digit()).next()?;
   is_story_id(id).then(|| id.to_owned())
}

fn snowflake_in_trending_url(url: &str) -> Option<String> {
   let path = url.split(['?', '#']).next()?;
   let rest = path.split("/i/trending/").nth(1)?;
   let id = rest.trim_matches('/').split('/').next()?;
   is_story_id(id).then(|| id.to_owned())
}

pub(crate) fn is_story_id(id: &str) -> bool {
   (15..=22).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit())
}

fn story_id_in(content: &Value) -> Option<String> {
   for key in ["trend_id", "trendId"] {
      if let Some(id) = content.get(key).and_then(Value::as_str)
         && is_story_id(id)
      {
         return Some(id.to_owned());
      }
   }
   let meta = content
      .get("trend_metadata")
      .or_else(|| content.get("trendMetadata"))?;
   for key in ["trend_id", "trendId", "rest_id"] {
      if let Some(id) = meta.get(key).and_then(Value::as_str)
         && is_story_id(id)
      {
         return Some(id.to_owned());
      }
   }
   None
}

fn trend_context(content: &Value, metadata: Option<&Value>) -> (String, String) {
   let social = content
      .get("social_context")
      .or_else(|| content.get("socialContext"));
   let social_text = social
      .and_then(|value| text_at(value, "text"))
      .unwrap_or_default();
   let mut category = metadata
      .and_then(|meta| text_at(meta, "domain_context").or_else(|| text_at(meta, "domainContext")))
      .unwrap_or_default();
   let meta_description = metadata
      .and_then(|meta| {
         text_at(meta, "meta_description").or_else(|| text_at(meta, "metaDescription"))
      })
      .unwrap_or_default();

   let mut time_ago = String::new();
   let mut posts = String::new();
   for part in social_text
      .split('·')
      .map(str::trim)
      .filter(|part| !part.is_empty())
   {
      let lower = part.to_ascii_lowercase();
      if lower.contains("ago") {
         time_ago = part.to_owned();
      } else if lower.contains("post") {
         posts = part.to_owned();
      } else if category.is_empty() {
         category = part.to_owned();
      }
   }

   let detail = if !meta_description.is_empty() {
      meta_description
   } else if !posts.is_empty() && !time_ago.is_empty() {
      format!("{posts} · {time_ago}")
   } else if !posts.is_empty() {
      posts
   } else if !time_ago.is_empty() {
      time_ago
   } else {
      text_at(content, "description").unwrap_or_default()
   };
   (category, detail)
}

fn trend_image(content: &Value) -> String {
   nested_url(content.get("thumbnail_image"))
      .or_else(|| nested_url(content.get("thumbnailImage")))
      .or_else(|| {
         content
            .get("images")
            .and_then(Value::as_array)
            .and_then(|images| images.first())
            .and_then(|image| nested_url(Some(image)))
      })
      .unwrap_or_default()
}

fn tab_from_value(value: &Value) -> Option<ExploreTab> {
   let url = nested_url(value.get("url"))
      .or_else(|| nested_url(value.get("landingUrl")))
      .or_else(|| nested_url(value.get("landing_url")))?;
   let slug = explore_slug_from_url(&url)?;
   let label = text_at(value, "name")
      .or_else(|| text_at(value, "title"))
      .filter(|label| !label.eq_ignore_ascii_case(&slug))
      .unwrap_or_else(|| tab_label(&slug));
   Some(ExploreTab { slug, label })
}

fn explore_slug_from_url(url: &str) -> Option<String> {
   let path = url.split(['?', '#']).next().unwrap_or(url);
   let rest = path.trim_end_matches('/').rsplit_once("/explore/tabs/")?.1;
   if rest.contains('/') {
      return None;
   }
   let decoded = percent_decode_str(rest).decode_utf8_lossy();
   normalize_slug(&decoded)
}

fn search_query(url: &str, fallback: &str) -> String {
   let Some(query) = url.split_once('?').map(|(_, query)| query) else {
      return fallback.to_owned();
   };
   for pair in query.split('&') {
      let Some((key, value)) = pair.split_once('=') else {
         continue;
      };
      if key == "q" {
         let decoded = percent_decode_str(value).decode_utf8_lossy();
         let decoded = decoded.trim();
         if !decoded.is_empty() {
            return decoded.to_owned();
         }
      }
   }
   fallback.to_owned()
}

fn header_text(content: &Value) -> Option<String> {
   let header = content.get("header")?;
   text_at(header, "text")
      .or_else(|| {
         header
            .get("richText")
            .and_then(|rich| text_at(rich, "text"))
      })
      .or_else(|| text_at(header, "content"))
}

fn useful_header(text: &str) -> bool {
   let lower = text.to_ascii_lowercase();
   !lower.is_empty() && !lower.contains("promoted")
}

fn cursor_value(entry: &Value) -> Option<String> {
   let content = entry.get("content")?;
   text_at(content, "value").or_else(|| {
      content
         .get("itemContent")
         .and_then(|item| text_at(item, "value"))
   })
}

fn is_bottom_cursor(id: &str) -> bool {
   id.starts_with("cursor-bottom") || id.contains("cursor-bottom-")
}

fn text_at(value: &Value, key: &str) -> Option<String> {
   let text = value.get(key)?.as_str()?.trim();
   if text.is_empty() {
      None
   } else {
      Some(text.to_owned())
   }
}

fn nested_url(value: Option<&Value>) -> Option<String> {
   let value = value?;
   if let Some(url) = value.as_str() {
      let url = url.trim();
      return (!url.is_empty()).then(|| url.to_owned());
   }
   text_at(value, "url").or_else(|| value.get("url").and_then(|url| text_at(url, "url")))
}

fn dedupe(feed: &mut ExploreFeed) {
   let mut seen_tabs = HashSet::new();
   feed.tabs.retain(|tab| seen_tabs.insert(tab.slug.clone()));

   let mut seen_trends = HashSet::new();
   let mut seen_tweets = HashSet::new();
   feed.items.retain(|item| {
      match item {
         ExploreItem::Trend(trend) => seen_trends.insert(trend.name.to_lowercase()),
         ExploreItem::Tweet(tweet) if tweet.id > 0 => seen_tweets.insert(tweet.id),
         ExploreItem::Header(_) | ExploreItem::Tweet(_) => true,
      }
   });

   let mut index = 0;
   while index < feed.items.len() {
      let orphan = matches!(feed.items.get(index), Some(ExploreItem::Header(_)))
         && !feed
            .items
            .get(index + 1)
            .is_some_and(|item| !matches!(item, ExploreItem::Header(_)));
      if orphan {
         feed.items.remove(index);
      } else {
         index += 1;
      }
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn reads_tabs_trends_and_the_next_cursor() {
      let data = serde_json::json!({
         "timeline": {
            "timeline": {
               "instructions": [{
                  "type": "TimelineAddEntries",
                  "entries": [
                     {
                        "entryId": "tabs",
                        "content": {
                           "items": [
                              {"item": {"itemContent": {
                                 "name": "Highlights",
                                 "url": {"url": "https://x.com/explore/tabs/highlights"}
                              }}},
                              {"item": {"itemContent": {
                                 "name": "News",
                                 "url": {"url": "https://x.com/explore/tabs/news"}
                              }}}
                           ]
                        }
                     },
                     {
                        "entryId": "stories",
                        "content": {
                           "header": {"text": "Today's News"},
                           "items": [{"entryId": "stories-1111111111111111111-trend-2112391398032908610", "item": {"itemContent": {
                              "itemType": "TimelineTrend",
                              "name": "A headline about the world",
                              "trend_metadata": {
                                 "meta_description": "12.4K posts",
                                 "domain_context": "News"
                              },
                              "trend_url": {"url": "https://x.com/search?q=headline%20world&src=trend_click"},
                              "thumbnail_image": {"url": "https://pbs.twimg.com/media/thumb.jpg"}
                           }}}]
                        }
                     },
                     {
                        "entryId": "trend-1",
                        "content": {"itemContent": {
                           "itemType": "TimelineTrend",
                           "name": "#Teapot",
                           "social_context": {"text": "Technology · 2 hours ago · 4,221 posts"},
                           "trend_url": {"url": "https://x.com/search?q=%23Teapot&src=trend_click"}
                        }}
                     },
                     {
                        "entryId": "promoted-trend",
                        "content": {"itemContent": {"itemType": "TimelineTrend", "name": "Ad"}}
                     },
                     {
                        "entryId": "cursor-bottom-0",
                        "content": {"value": "CURSOR+1="}
                     }
                  ]
               }]
            }
         }
      });

      let feed = parse_explore(&data);
      assert_eq!(
         feed
            .tabs
            .iter()
            .map(|tab| tab.slug.as_str())
            .collect::<Vec<_>>(),
         ["highlights", "news"]
      );
      assert_eq!(feed.bottom.as_deref(), Some("CURSOR+1="));

      let mut rows = feed.items.iter();
      assert!(matches!(rows.next(), Some(ExploreItem::Header(title)) if title == "Today's News"));
      assert!(matches!(
         rows.next(),
         Some(ExploreItem::Trend(trend))
            if trend.name == "A headline about the world"
               && trend.category == "News"
               && trend.detail == "12.4K posts"
               && trend.query == "headline world"
               && trend.image.contains("thumb.jpg")
               && trend.story_id == "2112391398032908610"
      ));
      assert!(matches!(
         rows.next(),
         Some(ExploreItem::Trend(trend))
            if trend.name == "#Teapot"
               && trend.category == "Technology"
               && trend.detail == "4,221 posts · 2 hours ago"
               && trend.query == "#Teapot"
      ));
      assert!(rows.next().is_none());
   }
}
