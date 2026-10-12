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
   api::CommunityInfo,
   error::Result,
   types::Prefs,
   utils::formatters,
   views::{layout, renderutils::proxy_media, timeline as timeline_view},
};

#[derive(Debug, Deserialize)]
struct CommunityQuery {
   cursor: Option<String>,
}

pub fn router() -> Router<AppState> {
   Router::new()
      .route("/i/communities/{id}", get(community_top))
      .route("/i/communities/{id}/latest", get(community_latest))
      .route("/i/communities/{id}/media", get(community_media))
      .route("/i/communities/{id}/about", get(community_about))
}

async fn community_top(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
   Query(query): Query<CommunityQuery>,
) -> Result<Response> {
   render_posts(
      &state,
      &jar,
      &id,
      query.cursor.as_deref(),
      "Relevance",
      "top",
   )
   .await
}

async fn community_latest(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
   Query(query): Query<CommunityQuery>,
) -> Result<Response> {
   render_posts(
      &state,
      &jar,
      &id,
      query.cursor.as_deref(),
      "Recency",
      "latest",
   )
   .await
}

async fn community_media(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
   Query(query): Query<CommunityQuery>,
) -> Result<Response> {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let info = state.api.get_community(&id).await?;
   let timeline = state
      .api
      .get_community_media(&id, query.cursor.as_deref())
      .await?;
   let view = prefs.resolved_media_view(None);
   render_community(
      &state,
      &prefs,
      &info,
      "media",
      Some(timeline_view::render_timeline_with_view(
         &timeline.content,
         &state.config,
         timeline.bottom.as_deref(),
         Some(&format!("/i/communities/{id}/media")),
         &prefs,
         None,
         &view,
      )),
   )
}

async fn community_about(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
) -> Result<Response> {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let info = state.api.get_community(&id).await?;
   render_community(&state, &prefs, &info, "about", None)
}

async fn render_posts(
   state: &AppState,
   jar: &CookieJar,
   id: &str,
   cursor: Option<&str>,
   ranking: &str,
   tab: &str,
) -> Result<Response> {
   let prefs = Prefs::from_cookies(jar, &state.config);
   let info = state.api.get_community(id).await?;
   let timeline = state.api.get_community_tweets(id, cursor, ranking).await?;
   let base = if tab == "latest" {
      format!("/i/communities/{id}/latest")
   } else {
      format!("/i/communities/{id}")
   };
   render_community(
      state,
      &prefs,
      &info,
      tab,
      Some(timeline_view::render_timeline_with_prefs(
         &timeline.content,
         &state.config,
         timeline.bottom.as_deref(),
         Some(&base),
         &prefs,
         None,
      )),
   )
}

fn render_community(
   state: &AppState,
   prefs: &Prefs,
   info: &CommunityInfo,
   tab: &str,
   body: Option<maud::Markup>,
) -> Result<Response> {
   let proxy = proxy_media(Some(prefs));
   let banner = if info.banner.is_empty() {
      String::new()
   } else {
      formatters::get_pic_url(&info.banner, state.config.config.base64_media, proxy)
   };
   let base = format!("/i/communities/{}", info.id);
   let content = html! {
       div class="timeline-container community-page" {
           @if !banner.is_empty() {
               div class="timeline-banner" {
                   a href=(banner) target="_blank" {
                       img src=(banner) alt="";
                   }
               }
           }
           div class="community-header" {
               h1 class="community-name" {
                   a href=(&base) { (info.name) }
               }
               @if !info.category.is_empty() {
                   span class="community-category" { (info.category) }
               }
               @if !info.description.is_empty() {
                   div class="community-description" { (info.description) }
               }
               @if info.members > 0 {
                   div class="community-members" { (formatters::format_with_commas(info.members)) " members" }
               }
           }
           ul class="tab" {
               li class=(tab_class(tab == "top")) { a href=(&base) { "Top" } }
               li class=(tab_class(tab == "latest")) { a href=(format!("{base}/latest")) { "Latest" } }
               li class=(tab_class(tab == "media")) { a href=(format!("{base}/media")) { "Media" } }
               li class=(tab_class(tab == "about")) { a href=(format!("{base}/about")) { "About" } }
           }
           @if tab == "about" {
               div class="community-about" {
                   p {
                       @if info.join_policy.eq_ignore_ascii_case("open") {
                           "Anyone can join this Community."
                       } @else if !info.join_policy.is_empty() {
                           "Membership is by approval only."
                       } @else {
                           "All Communities are publicly visible."
                       }
                   }
                   @if !info.rules.is_empty() {
                       h2 { "Rules" }
                       @for (index, (name, description)) in info.rules.iter().enumerate() {
                           div class="community-rule" {
                               strong { (index + 1) ". " (name) }
                               @if !description.is_empty() {
                                   p { (description) }
                               }
                           }
                       }
                   }
               }
           } @else if let Some(body) = body {
               (body)
           }
       }
   };
   let markup = layout::PageLayout::new(&state.config, &info.name, content)
      .prefs(prefs)
      .description(&info.description)
      .render();
   Ok(Html(markup.into_string()).into_response())
}

fn tab_class(active: bool) -> &'static str {
   if active {
      "tab-item active"
   } else {
      "tab-item"
   }
}
