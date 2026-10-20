//! NFL game page opened from the Sports tab. Highlights are the play clips.

use axum::{
   Router,
   extract::{Path, Query, State},
   response::{Html, IntoResponse as _, Response},
   routing::get,
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;

use crate::{
   AppState,
   api::{self, ScheduledGame},
   cache::{Hit, keys as cache_keys, ttl},
   error::{Error, Result},
   types::{Prefs, Tweet},
   views::{game as game_view, layout},
};

#[derive(Debug, Deserialize)]
struct GameQuery {
   page: Option<String>,
}

pub fn router() -> Router<AppState> {
   Router::new().route("/i/nfl/game/{id}", get(game))
}

pub async fn sports_games(state: &AppState) -> Vec<ScheduledGame> {
   let key = cache_keys::nfl_schedule();
   if let Some(Hit::Fresh(games) | Hit::Stale(games)) = state.cache.lookup::<Vec<ScheduledGame>>(&key)
   {
      return games;
   }
   match state.api.nfl_schedule().await {
      Ok(data) => {
         let games = api::parse_nfl_schedule(&data);
         if games.is_empty() {
            tracing::debug!(shape = %api::json_skeleton(&data), "nfl schedule had no games");
         }
         state.cache.set_swr(&key, &games, ttl::EXPLORE, ttl::EXPLORE_STALE);
         games
      },
      Err(err) => {
         tracing::debug!("nfl schedule failed: {err}");
         Vec::new()
      },
   }
}

async fn game(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
   Query(query): Query<GameQuery>,
) -> Result<Response> {
   if !is_game_id(&id) {
      return Err(Error::InvalidUrl("Invalid game id".into()));
   }
   let page = match query.page.as_deref() {
      Some("top") => "top",
      Some("latest") => "latest",
      Some("stats") => "stats",
      _ => "highlights",
   };
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let (board, timeline) = tokio::join!(state.api.nfl_game(&id), state.api.sports_game_top(&id));
   let board = board?;
   let mut parsed = api::parse_nfl_game(&board);
   parsed.id = id.clone();
   if parsed.away.abbr.is_empty() && parsed.home.abbr.is_empty() {
      tracing::debug!(game = %id, shape = %api::json_skeleton(&board), "nfl game scoreboard was empty");
   }
   let mut tweets = if page == "highlights" && !parsed.highlight_ids.is_empty() {
      match state.api.tweets_by_ids(&parsed.highlight_ids).await {
         Ok(data) => api::parse_tweets_by_ids(&data, &parsed.highlight_ids),
         Err(err) => {
            tracing::debug!("highlight clips failed: {err}");
            Vec::new()
         },
      }
   } else {
      timeline
         .as_ref()
         .map(api::tweets_in_timeline)
         .unwrap_or_default()
   };
   if tweets.is_empty() {
      if let Ok(data) = &timeline {
         tweets = api::tweets_in_timeline(data);
      }
   }
   if page == "latest" {
      tweets.sort_by_key(|tweet| std::cmp::Reverse(tweet.time));
   }
   if page == "highlights" {
      let clips: Vec<Tweet> = tweets
         .iter()
         .filter(|tweet| tweet.video.is_some() || !tweet.gifs.is_empty())
         .cloned()
         .collect();
      if !clips.is_empty() {
         tweets = clips;
      }
   }
   let game = parsed;
   let content = game_view::render_game_page(&game, &tweets, page, &state.config, &prefs);
   let title = if game.away.abbr.is_empty() {
      "Game highlights".to_owned()
   } else {
      format!("{} at {} highlights", game.away.abbr, game.home.abbr)
   };
   let referer = format!("/i/nfl/game/{id}?page={page}");
   let canonical = format!("https://x.com/i/nfl/game/{id}?page={page}");
   let markup = layout::PageLayout::new(&state.config, &title, content)
      .description("Game highlights, without an account or tracking.")
      .prefs(&prefs)
      .referer(&referer)
      .canonical(&canonical)
      .render();
   Ok(Html(markup.into_string()).into_response())
}

fn is_game_id(id: &str) -> bool {
   (15..=20).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit())
}
