use std::collections::HashMap;

use axum::{
   Router,
   extract::{
      Path,
      State,
   },
   response::{
      Html,
      IntoResponse,
   },
   routing::get,
};
use axum_extra::extract::CookieJar;

use maud::html;

use crate::{
   AppState,
   error::Result,
   types::{
      ArticleBlockType,
      ArticleEntityType,
      Prefs,
   },
   utils::formatters,
   views::{
      layout::PageLayout,
      notes as notes_view,
   },
};

pub fn router() -> Router<AppState> {
   Router::new()
      .route("/{username}/article/{id}", get(show_note))
      .route("/i/article/{id}", get(show_note_by_id))
}

/// `/i/article/{id}` — article by tweet ID only.
async fn show_note_by_id(
   State(state): State<AppState>,
   jar: CookieJar,
   Path(id): Path<String>,
) -> Result<impl IntoResponse> {
   show_note_inner(state, jar, id).await
}

/// `/{username}/article/{id}` — article with username context.
async fn show_note(
   State(state): State<AppState>,
   jar: CookieJar,
   Path((_username, id)): Path<(String, String)>,
) -> Result<impl IntoResponse> {
   show_note_inner(state, jar, id).await
}

async fn show_note_inner(
   state: AppState,
   jar: CookieJar,
   id: String,
) -> Result<impl IntoResponse> {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let (_tweet, article) = state.api.get_article_tweet(&id).await?;

   // Fetch embedded tweets in parallel
   let tweet_futures: Vec<_> = article
      .entities
      .iter()
      .filter(|entity| entity.entity_type == ArticleEntityType::Tweet)
      .filter(|entity| !entity.tweet_id.is_empty())
      .map(|entity| state.api.get_tweet(&entity.tweet_id))
      .collect();

   let mut tweets = HashMap::new();
   for future in tweet_futures {
      if let Ok(tweet) = future.await {
         tweets.insert(tweet.id, tweet);
      }
   }

   let content = notes_view::render_note(&article, &tweets, &state.config, Some(&prefs));

   // Build OG description from the first unstyled paragraph text
   let description = article
      .paragraphs
      .iter()
      .find(|para| para.base_type == ArticleBlockType::Unstyled && !para.text.is_empty())
      .map(|para| {
         if para.text.len() > 200 {
            let mut end = 200;
            while !para.text.is_char_boundary(end) {
               end -= 1;
            }
            format!("{}…", &para.text[..end])
         } else {
            para.text.clone()
         }
      })
      .unwrap_or_default();

   // Cover image for og:image — proxy through our media pipeline
   let og_image = if article.cover_image.is_empty() {
      String::new()
   } else {
      let proxied = formatters::get_pic_url(&article.cover_image, state.config.config.base64_media);
      format!("{}{proxied}", state.config.url_prefix())
   };

   // twitter:card = summary_large_image for big cover preview in Discord/etc
   let head_extra = html! {
      @if !article.cover_image.is_empty() {
         meta name="twitter:card" content="summary_large_image";
      }
   };

   let markup = PageLayout::new(&state.config, &article.title, content)
      .prefs(&prefs)
      .description(&description)
      .og_image(&og_image)
      .head_extra(&head_extra)
      .render();

   Ok(Html(markup.into_string()))
}
