use maud::{Markup, html};

use crate::{
   api::{GameSide, NflGame, ScheduledGame},
   config::Config,
   types::{Prefs, Tweet},
   views::tweet::TweetRenderer,
};

pub fn render_game_cards(games: &[ScheduledGame]) -> Markup {
   html! {
       @if !games.is_empty() {
           section class="game-strip" aria-label="Games" {
               h2 { "Games" }
               ul class="game-strip-list" {
                   @for game in games {
                       li {
                           a class="game-card" href=(format!("/i/nfl/game/{}?page=highlights", game.id)) {
                               @if !game.status.is_empty() {
                                   span class="game-card-status" { (game.status) }
                               }
                               span class="game-card-label" { (game.label) }
                               span class="game-card-link" { "Highlights" }
                           }
                       }
                   }
               }
           }
       }
   }
}

pub fn render_game_page(
   game: &NflGame,
   tweets: &[Tweet],
   page: &str,
   config: &Config,
   prefs: &Prefs,
) -> Markup {
   let title = match (game.away.abbr.as_str(), game.home.abbr.as_str()) {
      ("", "") => "Game".to_owned(),
      (away, "") => away.to_owned(),
      (away, home) => format!("{away} at {home}"),
   };
   html! {
       div class="timeline-container game-page" {
           header class="game-scoreboard" {
               (side_markup(&game.away, "away"))
               div class="game-status" {
                   @if game.status.is_empty() {
                       span { "Game" }
                   } @else {
                       span { (game.status) }
                   }
               }
               (side_markup(&game.home, "home"))
           }
           nav class="explore-tabbar" aria-label="Game" {
               ul class="tab explore-tabs" {
                   @for (id, label) in [("highlights", "Highlights"), ("top", "Top"), ("latest", "Latest"), ("stats", "Stats")] {
                       li class=(if page == id { "tab-item active" } else { "tab-item" }) {
                           a href=(format!("/i/nfl/game/{}?page={id}", game.id)) aria-current=(if page == id { "page" } else { "false" }) {
                               (label)
                           }
                       }
                   }
               }
           }
           @if page == "stats" {
               section class="game-stats" {
                   h2 { (title) }
                   p class="explore-intro" { "Box score and play clips are the posts in Highlights." }
                   @if tweets.is_empty() {
                       p class="explore-intro" { "No stat lines came back for this game." }
                   }
               }
           }
           div class="timeline explore-timeline" {
               @if tweets.is_empty() {
                   div class="timeline-header" {
                       h2 class="timeline-none" { "No highlights yet" }
                       p class="explore-intro" { "This game did not include play clips." }
                   }
               } @else {
                   @for tweet in tweets {
                       (TweetRenderer::new(tweet, config, false).maybe_prefs(Some(prefs)).render())
                   }
                   div class="timeline-footer" {
                       h2 class="timeline-end" { "No more items" }
                   }
               }
           }
       }
   }
}

fn side_markup(side: &GameSide, which: &str) -> Markup {
   html! {
       div class=(format!("game-side game-side-{which}")) {
           strong class="game-abbr" { (if side.abbr.is_empty() { "—" } else { side.abbr.as_str() }) }
           @if !side.name.is_empty() {
               span class="game-team" { (side.name) }
           }
           span class="game-score" { (if side.score.is_empty() { "–" } else { side.score.as_str() }) }
           @if !side.record.is_empty() {
               span class="game-record" { (side.record) }
           }
       }
   }
}
