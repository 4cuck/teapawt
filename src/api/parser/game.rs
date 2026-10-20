//! NFL game scoreboard, highlight clip ids, and the season schedule.
//!
//! The web client returns these in a few different envelopes, so this walks
//! the JSON instead of requiring one path.

use serde_json::Value;

use super::{explore::parse_explore, tweet::parse_tweet_object};
use crate::{
   api::schema::TweetData,
   types::{ExploreItem, Tweet},
};

#[derive(Clone, Debug, Default)]
pub struct GameSide {
   pub abbr: String,
   pub name: String,
   pub score: String,
   pub record: String,
}

#[derive(Clone, Debug, Default)]
pub struct NflGame {
   pub id: String,
   pub status: String,
   pub away: GameSide,
   pub home: GameSide,
   pub highlight_ids: Vec<String>,
}

/// One row on the Sports tab. `id` is the game rest id.
#[derive(Clone, Debug)]
pub struct ScheduledGame {
   pub id: String,
   pub label: String,
   pub status: String,
}

pub fn parse_nfl_game(data: &Value) -> NflGame {
   if let Some(game) = data.get("nfl_game") {
      return parse_known_game(game);
   }
   let mut game = NflGame::default();
   let mut sides = Vec::new();
   let mut ids = Vec::new();
   walk_game(data, &mut game, &mut sides, &mut ids);
   if sides.len() >= 2 {
      game.away = sides[0].clone();
      game.home = sides[1].clone();
   } else if let Some(side) = sides.first() {
      game.away = side.clone();
   }
   game.highlight_ids = ids;
   game
}

fn parse_known_game(game: &Value) -> NflGame {
   let score = game.get("score");
   let summaries = score.and_then(|score| score.get("team_score_summaries"));
   let status = pretty_status(
      score
         .and_then(|score| score.get("status"))
         .and_then(string_leaf)
         .as_deref()
         .unwrap_or(""),
      score.and_then(|score| score.get("quarter")).and_then(Value::as_i64),
      score
         .and_then(|score| score.get("clock"))
         .and_then(string_leaf)
         .as_deref()
         .unwrap_or(""),
   );
   let away = team_side(
      summaries.and_then(|summary| summary.get("away_team")),
      game.pointer("/lineups/away_team/name"),
   );
   let home = team_side(
      summaries.and_then(|summary| summary.get("home_team")),
      game.pointer("/lineups/home_team/name"),
   );
   let mut ids = Vec::new();
   walk_game(game, &mut NflGame::default(), &mut Vec::new(), &mut ids);
   NflGame {
      id: game
         .get("rest_id")
         .and_then(snowflake)
         .unwrap_or_default(),
      status,
      away,
      home,
      highlight_ids: ids,
   }
}

fn team_side(summary: Option<&Value>, lineup_name: Option<&Value>) -> GameSide {
   let Some(summary) = summary else {
      return GameSide::default();
   };
   let team = summary.get("team");
   let info = team.and_then(|team| team.get("team_information"));
   let abbr = info
      .and_then(|info| info.get("abbreviation"))
      .and_then(string_leaf)
      .unwrap_or_default();
   let short = info
      .and_then(|info| info.get("short_name"))
      .and_then(string_leaf)
      .unwrap_or_default();
   let location = info
      .and_then(|info| info.get("location"))
      .and_then(string_leaf)
      .unwrap_or_default();
   let name = lineup_name.and_then(string_leaf).unwrap_or_else(|| {
      if location.is_empty() {
         short.clone()
      } else if short.is_empty() {
         location.clone()
      } else {
         format!("{location} {short}")
      }
   });
   let score = summary
      .get("points")
      .and_then(|points| {
         points
            .as_i64()
            .map(|number| number.to_string())
            .or_else(|| string_leaf(points))
      })
      .unwrap_or_default();
   let record = team
      .and_then(|team| team.get("team_record"))
      .map(|record| {
         let wins = record.get("wins").and_then(Value::as_i64);
         let losses = record.get("losses").and_then(Value::as_i64);
         match (wins, losses) {
            (Some(wins), Some(losses)) => format!("{wins}-{losses}"),
            _ => String::new(),
         }
      })
      .unwrap_or_default();
   GameSide {
      abbr,
      name,
      score,
      record,
   }
}

fn pretty_status(raw: &str, quarter: Option<i64>, clock: &str) -> String {
   let lower = raw.to_ascii_lowercase();
   if lower == "closed" || lower == "complete" || lower == "final" {
      return "Final".to_owned();
   }
   if lower.contains("progress") || lower == "live" || lower == "inprogress" {
      return match quarter {
         Some(quarter) if !clock.is_empty() => format!("Q{quarter} {clock}"),
         Some(quarter) => format!("Q{quarter}"),
         None => "Live".to_owned(),
      };
   }
   raw.to_owned()
}

pub fn parse_nfl_schedule(data: &Value) -> Vec<ScheduledGame> {
   let mut games = Vec::new();
   let mut seen = std::collections::HashSet::new();
   collect_schedule(data, &mut games, &mut seen);
   games.truncate(8);
   games
}

fn collect_schedule(
   value: &Value,
   games: &mut Vec<ScheduledGame>,
   seen: &mut std::collections::HashSet<String>,
) {
   match value {
      Value::Array(items) => {
         for item in items {
            collect_schedule(item, games, seen);
         }
      },
      Value::Object(object) => {
         if object.contains_key("score") || object.contains_key("basic_data") {
            if let Some(id) = object.get("rest_id").and_then(snowflake) {
               let parsed = parse_known_game(value);
               if !parsed.away.abbr.is_empty()
                  && !parsed.home.abbr.is_empty()
                  && seen.insert(id.clone())
               {
                  games.push(ScheduledGame {
                     id,
                     label: format!(
                        "{} {} – {} {}",
                        parsed.away.abbr, parsed.away.score, parsed.home.score, parsed.home.abbr
                     ),
                     status: parsed.status,
                  });
               }
            }
         }
         for child in object.values() {
            collect_schedule(child, games, seen);
         }
      },
      Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {},
   }
}

pub fn parse_tweets_by_ids(data: &Value, order: &[String]) -> Vec<Tweet> {
   let mut tweets = Vec::new();
   collect_tweets(data, &mut tweets);
   if order.is_empty() {
      return tweets;
   }
   let mut by_id = std::collections::HashMap::new();
   for tweet in tweets {
      by_id.insert(tweet.id.to_string(), tweet);
   }
   order
      .iter()
      .filter_map(|id| by_id.remove(id))
      .collect()
}

/// Posts already expanded inside a game timeline.
pub fn tweets_in_timeline(data: &Value) -> Vec<Tweet> {
   parse_explore(data)
      .items
      .into_iter()
      .filter_map(|item| match item {
         ExploreItem::Tweet(tweet) => Some(*tweet),
         ExploreItem::Header(_) | ExploreItem::Trend(_) => None,
      })
      .collect()
}

fn walk_game(value: &Value, game: &mut NflGame, sides: &mut Vec<GameSide>, ids: &mut Vec<String>) {
   match value {
      Value::Array(items) => {
         for item in items {
            walk_game(item, game, sides, ids);
         }
      },
      Value::Object(object) => {
         if game.status.is_empty() {
            if let Some(status) = status_text(object) {
               game.status = status;
            }
         }
         if let Some(side) = side_from(object) {
            let key = format!("{}:{}", side.abbr, side.score);
            if !sides.iter().any(|seen| format!("{}:{}", seen.abbr, seen.score) == key) {
               sides.push(side);
            }
         }
         if let Some(id) = highlight_id(object) {
            if !ids.iter().any(|seen| seen == &id) {
               ids.push(id);
            }
         }
         for child in object.values() {
            walk_game(child, game, sides, ids);
         }
      },
      Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {},
   }
}

fn side_from(object: &serde_json::Map<String, Value>) -> Option<GameSide> {
   let abbr = first_string(
      object,
      &[
         "abbreviation",
         "abbr",
         "team_abbreviation",
         "short_name",
         "tricode",
      ],
   )?;
   if abbr.len() < 2 || abbr.len() > 4 || !abbr.chars().all(|ch| ch.is_ascii_uppercase()) {
      return None;
   }
   let score = first_score(object)?;
   let name = first_string(
      object,
      &["team_name", "teamName", "full_name", "fullName", "name", "location"],
   )
   .unwrap_or_default();
   let record = first_string(object, &["record", "overall_record", "overallRecord"])
      .unwrap_or_default();
   Some(GameSide {
      abbr,
      name,
      score,
      record,
   })
}

fn status_text(object: &serde_json::Map<String, Value>) -> Option<String> {
   for key in ["status", "game_status", "gameStatus", "display_status", "displayStatus", "state"] {
      if let Some(text) = object.get(key).and_then(string_leaf) {
         let lower = text.to_ascii_lowercase();
         if lower.contains("final")
            || lower.contains("quarter")
            || lower.contains("progress")
            || lower.contains("live")
            || lower.contains("schedul")
            || lower == "ft"
         {
            return Some(text);
         }
      }
   }
   None
}

fn highlight_id(object: &serde_json::Map<String, Value>) -> Option<String> {
   for key in ["tweet_id", "tweetId", "focalTweetId", "status_id", "statusId"] {
      if let Some(id) = object.get(key).and_then(snowflake) {
         return Some(id);
      }
   }
   let kind = object
      .get("__typename")
      .or_else(|| object.get("itemType"))
      .and_then(Value::as_str)
      .unwrap_or("");
   if kind.contains("Highlight") || kind.contains("Play") {
      if let Some(id) = object.get("rest_id").or_else(|| object.get("id")).and_then(snowflake) {
         return Some(id);
      }
   }
   None
}

fn collect_tweets(value: &Value, tweets: &mut Vec<Tweet>) {
   match value {
      Value::Array(items) => {
         for item in items {
            collect_tweets(item, tweets);
         }
      },
      Value::Object(object) => {
         if let Some(result) = object.get("result").or_else(|| object.get("tweet")) {
            if let Ok(data) = serde_json::from_value::<TweetData>(result.clone()) {
               if let Ok(tweet) = parse_tweet_object(&data) {
                  if tweet.available && tweet.id > 0 && !tweets.iter().any(|seen| seen.id == tweet.id)
                  {
                     tweets.push(tweet);
                     return;
                  }
               }
            }
         }
         for child in object.values() {
            collect_tweets(child, tweets);
         }
      },
      Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {},
   }
}

fn first_string(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
   for key in keys {
      if let Some(text) = object.get(*key).and_then(string_leaf) {
         return Some(text);
      }
   }
   None
}

fn first_score(object: &serde_json::Map<String, Value>) -> Option<String> {
   for key in ["score", "points", "total_score", "totalScore", "runs"] {
      if let Some(value) = object.get(key) {
         if let Some(text) = string_leaf(value) {
            if text.chars().all(|ch| ch.is_ascii_digit()) {
               return Some(text);
            }
         }
         if let Some(number) = value.as_i64() {
            return Some(number.to_string());
         }
         if let Some(number) = value.as_u64() {
            return Some(number.to_string());
         }
      }
   }
   None
}

fn string_leaf(value: &Value) -> Option<String> {
   match value {
      Value::String(text) => {
         let text = text.trim();
         if text.is_empty() {
            None
         } else {
            Some(text.to_owned())
         }
      },
      Value::Object(object) => object
         .get("text")
         .or_else(|| object.get("label"))
         .or_else(|| object.get("name"))
         .and_then(Value::as_str)
         .map(str::trim)
         .filter(|text| !text.is_empty())
         .map(str::to_owned),
      Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => None,
   }
}

fn snowflake(value: &Value) -> Option<String> {
   let text = value.as_str()?.trim();
   if (15..=20).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit()) {
      Some(text.to_owned())
   } else {
      None
   }
}

/// Keys only, so a missed scoreboard can be diagnosed without logging posts.
pub fn json_skeleton(value: &Value) -> String {
   let mut out = String::new();
   skim(value, 0, &mut out);
   out
}

fn skim(value: &Value, depth: usize, out: &mut String) {
   if depth > 4 || out.len() > 900 {
      return;
   }
   match value {
      Value::Object(map) => {
         out.push('{');
         for (index, (key, child)) in map.iter().enumerate() {
            if index > 8 {
               out.push('…');
               break;
            }
            if index > 0 {
               out.push(',');
            }
            out.push_str(key);
            out.push(':');
            skim(child, depth + 1, out);
         }
         out.push('}');
      },
      Value::Array(items) => {
         out.push('[');
         out.push_str(&items.len().to_string());
         if let Some(first) = items.first() {
            skim(first, depth + 1, out);
         }
         out.push(']');
      },
      Value::Null => out.push_str("null"),
      Value::String(text) => {
         out.push('s');
         out.push_str(&text.len().to_string());
      },
      Value::Bool(_) => out.push('b'),
      Value::Number(_) => out.push('n'),
   }
}
