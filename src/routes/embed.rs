use axum::{
   Router,
   extract::{
      Path,
      Query,
      State,
   },
   http::{
      HeaderMap,
      header,
   },
   response::{
      Html,
      IntoResponse as _,
      Json,
      Redirect,
      Response,
   },
   routing::get,
};
use serde::Deserialize;

use crate::{
   AppState,
   error::{
      Error,
      Result,
   },
   views::embed as embed_view,
};

#[derive(Debug, Deserialize)]
pub struct OEmbedQuery {
   pub text:   Option<String>,
   pub author: Option<String>,
   pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LegacyEmbedQuery {
   pub id: Option<String>,
}

pub fn router() -> Router<AppState> {
   Router::new()
        .route("/{username}/status/{id}/embed", get(tweet_embed))
        .route("/i/videos/tweet/{id}", get(video_embed))
        // Legacy embed URL support (Twitter's old embed format)
        .route("/embed/Tweet.html", get(legacy_embed_redirect))
        // ActivityPub endpoint for Discord multi-image support
        .route("/users/{username}/statuses/{id}", get(activity_pub_status))
        // oEmbed endpoint for engagement metrics in Discord embeds
        .route("/owoembed", get(oembed))
}

/// Redirect legacy `/embed/Tweet.html?id=XXX` to `/i/status/XXX/embed`.
async fn legacy_embed_redirect(Query(query): Query<LegacyEmbedQuery>) -> Response {
   query.id.map_or_else(
      || Redirect::to("/").into_response(),
      |id| Redirect::to(&format!("/i/status/{id}/embed")).into_response(),
   )
}

/// Tweet embed endpoint - returns HTML with proper OG meta tags for
/// Discord/etc.
async fn tweet_embed(
   State(state): State<AppState>,
   Path((_username, id)): Path<(String, String)>,
) -> Result<Response> {
   // Fetch tweet from API
   let tweet = state.api.get_tweet(&id).await?;

   let markup = embed_view::render_tweet_embed(&tweet, &state.config);
   Ok(Html(markup.into_string()).into_response())
}

/// Video embed endpoint - returns HTML with video player.
async fn video_embed(State(state): State<AppState>, Path(id): Path<String>) -> Result<Response> {
   // Fetch tweet from API
   let tweet = state.api.get_tweet(&id).await?;

   if tweet.video.is_none() && tweet.gif.is_none() {
      return Err(Error::NotFound("No video found".into()));
   }

   let markup = embed_view::render_video_embed(&tweet, &state.config);
   Ok(Html(markup.into_string()).into_response())
}

/// `ActivityPub` JSON endpoint for Discord multi-image support.
/// Discord fetches this to get all media attachments for carousel display.
async fn activity_pub_status(
   State(state): State<AppState>,
   Path((username, id)): Path<(String, String)>,
   headers: HeaderMap,
) -> Result<Response> {
   let accept = headers
      .get(header::ACCEPT)
      .and_then(|hv| hv.to_str().ok())
      .unwrap_or("");

   // Check if client wants ActivityPub JSON
   if accept.contains("application/activity+json") || accept.contains("application/ld+json") {
      // Fetch tweet from API
      let tweet = state.api.get_tweet(&id).await?;

      // Build ActivityPub JSON
      let activity = embed_view::build_activity_pub(&tweet, &state.config);

      return Ok((
         [(
            header::CONTENT_TYPE,
            "application/activity+json; charset=utf-8",
         )],
         Json(activity),
      )
         .into_response());
   }

   // Otherwise redirect to normal status page
   Ok(Redirect::to(&format!("/{username}/status/{id}")).into_response())
}

/// oEmbed JSON endpoint for engagement metrics in Discord embed footers.
async fn oembed(
   State(state): State<AppState>,
   Query(query): Query<OEmbedQuery>,
) -> Result<Response> {
   #[derive(serde::Serialize)]
   struct OEmbedData {
      author_name:   String,
      author_url:    String,
      provider_name: String,
      provider_url:  String,
      title:         &'static str,
      #[serde(rename = "type")]
      kind:          &'static str,
      version:       &'static str,
   }

   let text = query.text.unwrap_or_default();
   let author = query.author.unwrap_or_default();
   let status = query.status.unwrap_or_default();

   let author_url = format!("https://x.com/{author}/status/{status}");

   let oembed = OEmbedData {
      author_name: text,
      author_url,
      provider_name: state.config.server.title.clone(),
      provider_url: state.config.url_prefix().to_owned(),
      title: "Post",
      kind: "rich",
      version: "1.0",
   };

   Ok((
      [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
      Json(oembed),
   )
      .into_response())
}
