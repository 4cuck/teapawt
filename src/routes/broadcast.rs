use axum::{
   Router,
   extract::{Path, State},
   response::{Html, IntoResponse as _, Response},
   routing::get,
};
use axum_extra::extract::CookieJar;
use maud::html;

use crate::{
   AppState,
   error::Result,
   types::Prefs,
   utils::formatters,
   views::{layout, renderutils::proxy_media},
};

pub fn router() -> Router<AppState> {
   Router::new().route("/i/broadcasts/{broadcast_id}", get(broadcast))
}

async fn broadcast(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(broadcast_id): Path<String>,
) -> Result<Response> {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let broadcast = state.api.get_broadcast(&broadcast_id).await?;
   let proxy = proxy_media(Some(&prefs));
   let thumb = if broadcast.thumb.is_empty() {
      String::new()
   } else {
      formatters::get_pic_url(&broadcast.thumb, state.config.config.base64_media, proxy)
   };
   let stream = if broadcast.m3u8.is_empty() {
      String::new()
   } else {
      formatters::get_vid_url(
         &broadcast.m3u8,
         &state.config.config.hmac_key,
         state.config.config.base64_media,
         proxy,
      )
   };
   let live = broadcast.state.eq_ignore_ascii_case("running")
      || broadcast.status_line.to_ascii_lowercase().contains("live");
   let title = broadcast.title.clone();
   let content = html! {
       div class="timeline-container broadcast-page" {
           div class="broadcast-panel" {
               div class="broadcast-player" {
                   @if !stream.is_empty() && prefs.hls_playback {
                       video class="hls" poster=(thumb) data-url=(stream) data-autoload="false" muted[prefs.mute_videos] {}
                       div class="video-overlay hls-overlay" {
                           div class="overlay-circle" { span class="overlay-triangle" {} }
                           @if live {
                               div class="broadcast-live" { "LIVE" }
                           }
                       }
                   } @else if !stream.is_empty() {
                       @if !thumb.is_empty() {
                           img src=(thumb) alt=(crate::views::renderutils::alt_text(&broadcast.title, "Broadcast"));
                       }
                       div class="video-overlay" {
                           form method="post" action="/enablehls" {
                               input type="hidden" name="referer" value=(format!("/i/broadcasts/{broadcast_id}"));
                               button type="submit" { "Enable hls playback" }
                           }
                           @if live {
                               div class="broadcast-live" { "LIVE" }
                           }
                       }
                   } @else if !thumb.is_empty() {
                       img src=(thumb) alt=(crate::views::renderutils::alt_text(&broadcast.title, "Broadcast"));
                       div class="video-overlay" {
                           p { @if broadcast.replay { "Stream unavailable" } @else { "Replay is not available" } }
                       }
                   } @else {
                       div class="video-overlay" { p { "Broadcast not found" } }
                   }
               }
               div class="broadcast-info" {
                   h2 class="broadcast-title" { (broadcast.title) }
                   div class="broadcast-user-row" {
                       @if !broadcast.username.is_empty() {
                           a class="broadcast-user" href=(format!("/{}", broadcast.username)) {
                               strong { (broadcast.display_name) }
                               span class="broadcast-username" { "@" (broadcast.username) }
                           }
                       }
                       div class="broadcast-meta" {
                           @if broadcast.watched > 0 {
                               span { (formatters::format_with_commas(broadcast.watched)) " views" }
                           }
                           @if live && broadcast.watching > 0 {
                               span class="broadcast-live" { (broadcast.watching) " watching" }
                           } @else if !broadcast.status_line.is_empty() {
                               span { (broadcast.status_line) }
                           }
                       }
                   }
               }
           }
       }
   };
   let markup = layout::PageLayout::new(&state.config, &title, content)
      .prefs(&prefs)
      .render();
   Ok(Html(markup.into_string()).into_response())
}
