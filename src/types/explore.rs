//! Explore tabs and the rows they contain.

use std::collections::HashSet;

use super::Tweet;

/// One Explore destination. `slug` is the path segment (`news`, `highlights`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExploreTab {
   pub slug:  String,
   pub label: String,
}

/// A trending topic or news card. `story_id` is X's `/i/trending/{id}` snowflake
/// when the row is a story. Otherwise the row stays a search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trend {
   pub name:     String,
   pub category: String,
   pub detail:   String,
   pub query:    String,
   pub image:    String,
   pub story_id: String,
}

/// One trending-story page: the writeup, the posts, and nearby stories.
#[derive(Debug, Clone, Default)]
pub struct TrendStory {
   pub id:      String,
   pub title:   String,
   pub summary: String,
   pub updated: String,
   pub tweets:  Vec<Tweet>,
   pub related: Vec<Trend>,
}

/// A row on an Explore timeline, in the order X returned it.
#[derive(Debug, Clone)]
pub enum ExploreItem {
   Header(String),
   Trend(Trend),
   Tweet(Box<Tweet>),
}

/// One page of Explore: the rows, any tabs that page advertised, and the
/// cursor for the next page.
#[derive(Debug, Clone, Default)]
pub struct ExploreFeed {
   pub items:  Vec<ExploreItem>,
   pub tabs:   Vec<ExploreTab>,
   pub bottom: Option<String>,
}

/// Tabs this instance always offers. An account's Explore settings can add
/// more, and can reorder these, but Highlights stays reachable either way.
pub fn default_tabs() -> Vec<ExploreTab> {
   [
      "for-you",
      "trending",
      "news",
      "sports",
      "entertainment",
      "highlights",
   ]
   .into_iter()
   .map(|slug| {
      ExploreTab {
         slug:  slug.to_owned(),
         label: tab_label(slug),
      }
   })
   .collect()
}

/// Account order first, then any default tab that account did not return.
/// For you stays first because that is the Explore page itself.
pub fn merge_tabs(discovered: &[ExploreTab]) -> Vec<ExploreTab> {
   let mut tabs = Vec::new();
   let mut seen = HashSet::new();
   seen.insert("for-you".to_owned());
   tabs.push(ExploreTab {
      slug:  "for-you".to_owned(),
      label: tab_label("for-you"),
   });
   for tab in discovered {
      if seen.insert(tab.slug.clone()) {
         tabs.push(tab.clone());
      }
   }
   for tab in default_tabs() {
      if seen.insert(tab.slug.clone()) {
         tabs.push(tab);
      }
   }
   tabs
}

pub fn tab_label(slug: &str) -> String {
   match slug {
      "for-you" => "For you".to_owned(),
      "trending" => "Trending".to_owned(),
      "news" => "News".to_owned(),
      "sports" => "Sports".to_owned(),
      "entertainment" => "Entertainment".to_owned(),
      "highlights" => "Highlights".to_owned(),
      other => title_case_slug(other),
   }
}

/// `for_you` and `foryou` are the same Explore home. Anything else is kept
/// as the timeline name X puts in the tab URL.
pub fn normalize_slug(slug: &str) -> Option<String> {
   let slug = slug.trim().to_ascii_lowercase();
   if slug.is_empty()
      || slug.len() > 40
      || !slug
         .bytes()
         .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
   {
      return None;
   }
   Some(match slug.as_str() {
      "for_you" | "foryou" | "for-you" => "for-you".to_owned(),
      other => other.to_owned(),
   })
}

/// Name encoded inside `GenericTimelineById`. For you is `for_you` on the
/// wire and `for-you` in our URL.
pub fn timeline_name(slug: &str) -> &str {
   if slug == "for-you" { "for_you" } else { slug }
}

fn title_case_slug(slug: &str) -> String {
   let mut label = String::new();
   for (index, word) in slug
      .split(['-', '_'])
      .filter(|word| !word.is_empty())
      .enumerate()
   {
      if index > 0 {
         label.push(' ');
      }
      let mut chars = word.chars();
      if let Some(first) = chars.next() {
         label.extend(first.to_uppercase());
         label.extend(chars.flat_map(char::to_lowercase));
      }
   }
   if label.is_empty() {
      slug.to_owned()
   } else {
      label
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn account_tabs_lead_and_highlights_stays() {
      let discovered = vec![
         ExploreTab {
            slug:  "news".to_owned(),
            label: "News".to_owned(),
         },
         ExploreTab {
            slug:  "fun".to_owned(),
            label: "Fun".to_owned(),
         },
      ];
      let tabs = merge_tabs(&discovered);
      let slugs: Vec<&str> = tabs.iter().map(|tab| tab.slug.as_str()).collect();
      assert_eq!(slugs, [
         "for-you",
         "news",
         "fun",
         "trending",
         "sports",
         "entertainment",
         "highlights",
      ]);
   }

   #[test]
   fn normalizes_the_for_you_aliases() {
      assert_eq!(normalize_slug("for_you").as_deref(), Some("for-you"));
      assert_eq!(normalize_slug("Highlights").as_deref(), Some("highlights"));
      assert_eq!(timeline_name("for-you"), "for_you");
      assert_eq!(timeline_name("highlights"), "highlights");
      assert!(normalize_slug("nope/tab").is_none());
   }
}
