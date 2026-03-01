use axum::{
   Router,
   extract::State,
   response::{
      Html,
      IntoResponse as _,
      Response,
   },
   routing::get,
};
use maud::html;

use crate::{
   AppState,
   views::layout,
};

pub fn router() -> Router<AppState> {
   Router::new()
      .route("/login", get(unsupported_login))
      .route("/login/{*path}", get(unsupported_login))
      .route("/about/feature", get(unsupported_feature))
}

/// Catch-all router for `/i/*` routes - must be merged AFTER specific `/i/*`
/// routes.
pub fn i_catchall_router() -> Router<AppState> {
   Router::new().route("/i/{*path}", get(i_unsupported))
}

/// Catch-all for unsupported `/i/*` routes.
async fn i_unsupported(State(state): State<AppState>) -> Response {
   render_unsupported_feature(&state, "Feature").into_response()
}

async fn unsupported_login(State(state): State<AppState>) -> Response {
   let content = html! {
       div class="overlay-panel" {
           h1 { "Login Not Supported" }
           p { "teapot does not support logging in to Twitter." }
           p { "teapot is a privacy-focused frontend that allows you to browse Twitter without an account." }
           p {
               a href="/" { "Go to Homepage" }
           }
       }
   };

   let markup = layout::PageLayout::new(&state.config, "Login Not Supported", content).render();
   Html(markup.into_string()).into_response()
}

async fn unsupported_feature(State(state): State<AppState>) -> Response {
   render_unsupported_feature(&state, "Feature").into_response()
}

/// Render the unsupported feature page.
pub fn render_unsupported_feature(state: &AppState, feature_name: &str) -> Response {
   let content = html! {
       div class="overlay-panel" {
           h1 { "Unsupported feature" }
           p {
               "teapot doesn't support this feature yet, but it might in the future. "
               "You can check for an issue and open one if needed here: "
               a href="https://github.com/amaanq/teapot/issues" target="_blank" {
                   "https://github.com/amaanq/teapot/issues"
               }
           }
           p {
               "To find out more about the teapot project, see the "
               a href="/about" { "About page" }
           }
       }
   };

   let title = format!("{feature_name} Not Available");
   let markup = layout::PageLayout::new(&state.config, &title, content).render();
   Html(markup.into_string()).into_response()
}
