//! The page behind `/i/trending/{id}`: title, writeup, and the posts X attached.

use std::fmt::Write as _;

use serde_json::Value;
use time::format_description::well_known::Rfc3339;

use super::{
   explore::parse_explore,
   tweet::parse_tweet_object,
};
use crate::{
   api::schema::TweetData,
   types::{ExploreItem, Trend, TrendStory, Tweet},
};

/// Timeline ids and tweet ids hanging off `post_timelines`. The story payload
/// often names those timelines instead of inlining the posts.
pub struct StoryPostRefs {
   pub timeline_ids: Vec<String>,
   pub tweet_ids:    Vec<String>,
}

pub fn story_post_refs(value: &Value) -> StoryPostRefs {
   let mut refs = StoryPostRefs {
      timeline_ids: Vec::new(),
      tweet_ids:    Vec::new(),
   };
   collect_refs(value, false, &mut refs);
   refs
}

fn collect_refs(value: &Value, in_posts: bool, refs: &mut StoryPostRefs) {
   match value {
      Value::Array(items) => {
         for item in items {
            collect_refs(item, in_posts, refs);
         }
      },
      Value::Object(object) => {
         for (key, child) in object {
            let posts = in_posts || key == "post_timelines" || key == "post_timeline" || key == "postTimelines";
            if posts && let Some(text) = child.as_str() {
               classify_ref(text, refs);
            }
            collect_refs(child, posts, refs);
         }
      },
      Value::String(text) if in_posts => classify_ref(text, refs),
      _ => {},
   }
}

fn classify_ref(text: &str, refs: &mut StoryPostRefs) {
   if super::explore::is_story_id(text) {
      if !refs.tweet_ids.iter().any(|seen| seen == text) && refs.tweet_ids.len() < 30 {
         refs.tweet_ids.push(text.to_owned());
      }
      return;
   }
   let timeline = text.len() >= 16
      && text.len() <= 512
      && text
         .chars()
         .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '=' | '+' | '/'))
      && text.chars().any(|ch| ch.is_ascii_alphabetic());
   if timeline && !refs.timeline_ids.iter().any(|seen| seen == text) && refs.timeline_ids.len() < 4 {
      refs.timeline_ids.push(text.to_owned());
   }
}

/// Key paths that look like posts or the writeup, with lengths instead of text.
pub fn story_clues(value: &Value) -> String {
   let mut out = String::new();
   clue(value, "", &mut out);
   out
}

fn clue(value: &Value, path: &str, out: &mut String) {
   if out.len() > 1_200 {
      return;
   }
   match value {
      Value::Array(items) => {
         if let Some(first) = items.first() {
            clue(first, path, out);
         }
      },
      Value::Object(object) => {
         for (key, child) in object {
            let next = if path.is_empty() {
               key.clone()
            } else {
               format!("{path}.{key}")
            };
            let name = key.to_ascii_lowercase();
            if name.contains("tweet")
               || name.contains("timeline")
               || name.contains("post")
               || name.contains("rest")
               || name.contains("article")
               || name.contains("summary")
               || name.contains("title")
            {
               match child {
                  Value::Array(items) => {
                     let _ = write!(out, "{next}[{}]; ", items.len());
                  },
                  Value::String(text) => {
                     let _ = write!(out, "{next}:s{}; ", text.len());
                  },
                  Value::Number(number) => {
                     let _ = write!(out, "{next}:{number}; ");
                  },
                  _ => {
                     let _ = write!(out, "{next}; ");
                  },
               }
            }
            if !child.is_string() && !child.is_number() {
               clue(child, &next, out);
            }
         }
      },
      _ => {},
   }
}

pub fn parse_trend_story(id: &str, data: &Value) -> TrendStory {
   let feed = parse_explore(data);
   let mut story = TrendStory {
      id: id.to_owned(),
      ..TrendStory::default()
   };
   walk(data, "", &mut story);
   for item in feed.items {
      match item {
         ExploreItem::Tweet(tweet) => push_tweet(&mut story.tweets, *tweet),
         ExploreItem::Trend(trend) => push_related(&mut story, trend),
         ExploreItem::Header(_) => {},
      }
   }
   collect_tweets(data, &mut story.tweets);
   if story.title.is_empty() {
      if let Some(trend) = story.related.iter().find(|trend| trend.story_id == id) {
         story.title.clone_from(&trend.name);
      }
   }
   story.summary = plain(&story.summary);
   story
}

fn push_related(story: &mut TrendStory, trend: Trend) {
   if trend.name.is_empty() || trend.name == story.title {
      return;
   }
   if story
      .related
      .iter()
      .any(|seen| seen.name == trend.name || (!trend.story_id.is_empty() && seen.story_id == trend.story_id))
   {
      return;
   }
   story.related.push(trend);
}

fn push_tweet(tweets: &mut Vec<Tweet>, tweet: Tweet) {
   if tweets.iter().any(|seen| seen.id == tweet.id) {
      return;
   }
   tweets.push(tweet);
}

fn walk(value: &Value, parent: &str, story: &mut TrendStory) {
   match value {
      Value::Array(items) => {
         for item in items {
            walk(item, parent, story);
         }
      },
      Value::Object(object) => {
         for (key, child) in object {
            consider(key, parent, child, story);
            if !child.is_string() && !child.is_number() {
               walk(child, key, story);
            }
         }
      },
      _ => {},
   }
}

fn consider(key: &str, parent: &str, child: &Value, story: &mut TrendStory) {
   if let Some(text) = child.as_str() {
      let text = text.trim();
      if text.is_empty() || text.starts_with("http") {
         return;
      }
      if is_title_key(key) && (12..=240).contains(&text.len()) && text.len() > story.title.len() {
         story.title = text.to_owned();
      }
      if is_summary(key, parent) && text.len() > story.summary.len() && text.len() < 8_000 {
         story.summary = text.to_owned();
      }
      if story.updated.is_empty() && is_updated_key(key) && text.len() < 80 {
         story.updated = text.to_owned();
      }
      return;
   }
   let Some(millis) = child.as_i64().filter(|value| *value > 1_000_000_000_000) else {
      return;
   };
   if story.updated.is_empty() && is_updated_key(key) {
      story.updated = format_millis(millis);
   }
}

fn is_title_key(key: &str) -> bool {
   let key = key.to_ascii_lowercase();
   key == "title" || key == "headline"
}

fn is_summary(key: &str, parent: &str) -> bool {
   let key = key.to_ascii_lowercase();
   let parent = parent.to_ascii_lowercase();
   key.contains("summary")
      || key.contains("preview")
      || key == "article_text"
      || key == "post_text"
      || parent.contains("article")
      || parent.contains("summary")
      || parent.contains("grok")
}

fn is_updated_key(key: &str) -> bool {
   let key = key.to_ascii_lowercase();
   key.contains("update") || key.contains("modified")
}

fn format_millis(millis: i64) -> String {
   time::OffsetDateTime::from_unix_timestamp(millis / 1000)
      .ok()
      .and_then(|when| when.format(&Rfc3339).ok())
      .unwrap_or_default()
}

fn collect_tweets(value: &Value, tweets: &mut Vec<Tweet>) {
   match value {
      Value::Array(items) => {
         for item in items {
            collect_tweets(item, tweets);
         }
      },
      Value::Object(object) => {
         if let Some(result) = object.get("tweet_results").and_then(|value| value.get("result"))
            && let Ok(data) = serde_json::from_value::<TweetData>(result.clone())
            && let Ok(tweet) = parse_tweet_object(&data)
            && tweet.available
         {
            push_tweet(tweets, tweet);
         }
         for child in object.values() {
            collect_tweets(child, tweets);
         }
      },
      _ => {},
   }
}

fn plain(text: &str) -> String {
   if !text.contains('<') {
      return text.trim().to_owned();
   }
   let mut out = String::new();
   let mut in_tag = false;
   for ch in text.chars() {
      match ch {
         '<' => in_tag = true,
         '>' => {
            in_tag = false;
            out.push(' ');
         },
         _ if !in_tag => out.push(ch),
         _ => {},
      }
   }
   out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
   use super::parse_trend_story;
   use serde_json::json;

   #[test]
   fn a_story_keeps_its_title_and_writeup() {
      let data = json!({
         "ai_trend_by_rest_id": {
            "result": {
               "title": "Tesla Cybercab Robotaxis Launch in Austin with Mixed Rider Reactions",
               "article": {
                  "summary": "<p>Riders in Austin reported a quiet cabin and a slow pickup.</p>"
               },
               "last_updated_at_ms": 1760000000000_i64
            }
         }
      });
      let story = parse_trend_story("2112391398032908610", &data);
      assert_eq!(
         story.title,
         "Tesla Cybercab Robotaxis Launch in Austin with Mixed Rider Reactions"
      );
      assert_eq!(
         story.summary,
         "Riders in Austin reported a quiet cabin and a slow pickup."
      );
      assert!(!story.updated.is_empty());
   }
}
