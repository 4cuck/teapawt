//! Operator-only routes, behind `config.adminToken`.
//!
//! `GET /.admin/pass` is a form; `POST /.admin/pass` issues a search pass for
//! a donor whose donation the operator found in the Stripe dashboard. The
//! token goes in an `Authorization: Bearer` header or the form's `token`
//! field. With no token configured the routes answer 404.

use axum::{
   Form, Router,
   extract::State,
   http::{HeaderMap, HeaderValue, StatusCode, header},
   response::{Html, IntoResponse as _, Response},
   routing::get,
};
use maud::{Markup, html};
use serde::Deserialize;

use crate::{AppState, passes, utils::host::current_origin};

/// Longest pass one request can hand out.
const MAX_DAYS: i64 = 366;
const DEFAULT_DAYS: i64 = 31;

pub fn router() -> Router<AppState> {
   Router::new().route("/.admin/pass", get(form).post(grant))
}

#[derive(Deserialize)]
struct Grant {
   #[serde(default)]
   token: String,
   #[serde(default)]
   email: String,
   #[serde(default)]
   days:  String,
}

async fn form(State(state): State<AppState>) -> Response {
   if state.config.config.admin_token.is_empty() {
      return StatusCode::NOT_FOUND.into_response();
   }
   page(StatusCode::OK, &html! {})
}

async fn grant(State(state): State<AppState>, headers: HeaderMap, Form(grant): Form<Grant>) -> Response {
   let expected = &state.config.config.admin_token;
   if expected.is_empty() {
      return StatusCode::NOT_FOUND.into_response();
   }
   let bearer = headers
      .get(header::AUTHORIZATION)
      .and_then(|value| value.to_str().ok())
      .and_then(|value| value.strip_prefix("Bearer "));
   let token = bearer.unwrap_or(&grant.token);
   if !super::debug::constant_time_eq(token.as_bytes(), expected.as_bytes()) {
      tracing::warn!("admin pass request refused: bad token");
      return page(StatusCode::UNAUTHORIZED, &html! { p class="admin-error" { "Wrong admin token." } });
   }
   let days = match parse_days(&grant.days) {
      Ok(days) => days,
      Err(message) => return page(StatusCode::BAD_REQUEST, &html! { p class="admin-error" { (message) } }),
   };
   let email = grant.email.trim();
   if !email.is_empty() && (!email.contains('@') || email.len() > 254) {
      return page(StatusCode::BAD_REQUEST, &html! { p class="admin-error" { "That is not an email address." } });
   }
   match state.passes.grant(email, days).await {
      Ok(granted) => {
         let origin = current_origin(state.config.url_prefix());
         page(StatusCode::OK, &issued(&granted, &origin, days))
      },
      Err(err) => {
         tracing::warn!("admin pass not issued: {err}");
         page(
            StatusCode::INTERNAL_SERVER_ERROR,
            &html! { p class="admin-error" { "The pass was not saved. Check the logs." } },
         )
      },
   }
}

fn parse_days(raw: &str) -> Result<i64, &'static str> {
   let raw = raw.trim();
   if raw.is_empty() {
      return Ok(DEFAULT_DAYS);
   }
   match raw.parse::<i64>() {
      Ok(days) if (1..=MAX_DAYS).contains(&days) => Ok(days),
      _ => Err("Days must be a whole number from 1 to 366."),
   }
}

fn issued(granted: &passes::Granted, origin: &str, days: i64) -> Markup {
   html! {
       div class="admin-result" {
           @if granted.extended {
               p { "Added " (days) " days to the pass already on that email." }
               p { "Username: " code { (granted.username) } " (their existing password still works)" }
           } @else {
               p { "New pass, " (days) " days. Send them:" }
               pre class="admin-login" {
                   "Search pass for " (origin) "\n"
                   "Username: " (granted.username) "\n"
                   "Password: " (granted.password.as_deref().unwrap_or_default()) "\n"
                   "Sign in at " (origin) "/search-pass/login\n"
               }
               p { "The password is shown only here. It is not stored in a form anyone can read back." }
           }
           p { "Runs until " (passes::date(granted.paid_until)) "." }
       }
   }
}

fn page(status: StatusCode, result: &Markup) -> Response {
   let markup = html! {
       (maud::DOCTYPE)
       html lang="en" {
           head {
               meta charset="utf-8";
               meta name="robots" content="noindex, nofollow";
               meta name="viewport" content="width=device-width, initial-scale=1";
               title { "Issue a search pass" }
               link rel="stylesheet" href="/css/clean.css";
           }
           body {
               div class="overlay-panel admin-panel" {
                   h1 { "Issue a search pass" }
                   (result)
                   form method="post" action="/.admin/pass" autocomplete="off" {
                       label for="admin-token" { "Admin token" }
                       input id="admin-token" name="token" type="password" required;
                       label for="admin-email" { "Donor email (optional)" }
                       input id="admin-email" name="email" type="email" maxlength="254"
                           placeholder="the email from the Stripe dashboard";
                       label for="admin-days" { "Days" }
                       input id="admin-days" name="days" type="number" min="1" max=(MAX_DAYS) value=(DEFAULT_DAYS);
                       button type="submit" { "Issue pass" }
                   }
                   p {
                       "With an email that already has a pass, the days are added to it. "
                       "Otherwise a new username and password are made."
                   }
               }
           }
       }
   };
   let mut response = (status, Html(markup.into_string())).into_response();
   let headers = response.headers_mut();
   headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
   headers.insert("cloudflare-cdn-cache-control", HeaderValue::from_static("no-store"));
   headers.insert("x-robots-tag", HeaderValue::from_static("noindex, nofollow"));
   response
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn days_default_to_a_month_and_stay_in_range() {
      assert_eq!(parse_days(""), Ok(31));
      assert_eq!(parse_days(" 90 "), Ok(90));
      for bad in ["0", "367", "ten"] {
         assert!(matches!(parse_days(bad), Err(_)), "{bad}");
      }
   }
}