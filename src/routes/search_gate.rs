//! A five-minute search pass, granted by a checkbox Turnstile.

use std::{
   collections::HashMap,
   sync::{LazyLock, Mutex},
};

use axum::{
   http::{HeaderValue, header},
   response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::{
   CookieJar,
   cookie::{Cookie, SameSite},
};
use data_encoding::HEXLOWER;
use ring::hmac;
use serde::Deserialize;
use time::{Duration, OffsetDateTime};

use crate::{
   AppState,
   types::Prefs,
   views::{layout, search as search_view},
};

/// How long one checkbox press unlocks search.
const PASS_SECS: i64 = 5 * 60;
const COOKIE: &str = "search_pass";

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
   reqwest::Client::builder()
      .timeout(std::time::Duration::from_secs(5))
      .build()
      .expect("turnstile client")
});

/// A token Cloudflare already accepted. A second submit of the same checkbox
/// (a retry, or the redirect that follows the first) is the same pass.
static ACCEPTED: LazyLock<Mutex<HashMap<String, i64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Deserialize)]
struct Siteverify {
   success: bool,
   #[serde(default, rename = "error-codes")]
   error_codes: Vec<String>,
}

/// Where to send the visitor after the checkbox, and the form that posts it.
pub struct Prompt<'a> {
   pub resume: &'a str,
   pub action: &'a str,
   pub fields: &'a [(&'a str, &'a str)],
}

/// `Ok` means search may call X. `Err` is the checkbox page or the redirect
/// that stores the pass.
pub async fn admit(
   state: &AppState,
   jar: &CookieJar,
   prefs: &Prefs,
   token: Option<&str>,
   prompt: Prompt<'_>,
) -> Result<(), Response> {
   if !state.config.turnstile.configured() {
      return Ok(());
   }
   if pass_valid(&state.config.config.hmac_key, jar) {
      if token.is_some_and(|token| !token.is_empty()) {
         let origin = crate::utils::host::current_origin(state.config.url_prefix());
         return Err(redirect(None, &absolute(&origin, prompt.resume)));
      }
      return Ok(());
   }

   let Some(token) = token.filter(|token| !token.is_empty()) else {
      return Err(challenge(state, prefs, &prompt, false));
   };
   let fresh = verify(&state.config.turnstile.secret_key, token).await;
   let accepted = fresh.is_ok() || (fresh.is_err() && already_accepted(token));
   if accepted {
      if fresh.is_ok() {
         remember(token);
      }
      tracing::info!("search check passed");
      let cookie = pass_cookie(&state.config.config.hmac_key, state.config.server.https);
      let origin = crate::utils::host::current_origin(state.config.url_prefix());
      return Err(redirect(Some(cookie), &absolute(&origin, prompt.resume)));
   }
   let codes = fresh.unwrap_err();
   tracing::info!(errors = %codes.join(","), "search check failed");
   Err(challenge(state, prefs, &prompt, true))
}

/// Search feeds and machine clients have no checkbox. A pass still lets a
/// browser's feed through.
#[must_use]
pub fn cleared(state: &AppState, jar: &CookieJar) -> bool {
   !state.config.turnstile.configured() || pass_valid(&state.config.config.hmac_key, jar)
}

fn challenge(state: &AppState, prefs: &Prefs, prompt: &Prompt<'_>, failed: bool) -> Response {
   let content = search_view::render_turnstile_prompt(
      &state.config.turnstile.site_key,
      prompt.action,
      prompt.fields,
      failed,
   );
   let markup = layout::PageLayout::new(&state.config, "Confirm this search", content)
      .prefs(prefs)
      .referer(prompt.resume)
      .render();
   private(axum::response::Html(markup.into_string()).into_response())
}

fn absolute(origin: &str, url: &str) -> String {
   if url.starts_with("http://") || url.starts_with("https://") {
      url.to_owned()
   } else {
      format!("{origin}{url}")
   }
}

fn redirect(cookie: Option<Cookie<'static>>, url: &str) -> Response {
   let redirect = Redirect::to(url);
   let response = if let Some(cookie) = cookie {
      (CookieJar::new().add(cookie), redirect).into_response()
   } else {
      redirect.into_response()
   };
   private(response)
}

fn private(mut response: Response) -> Response {
   response.headers_mut().insert(
      header::CACHE_CONTROL,
      HeaderValue::from_static("private, no-store"),
   );
   response
}

fn pass_cookie(key: &str, secure: bool) -> Cookie<'static> {
   let expiry = OffsetDateTime::now_utc().unix_timestamp() + PASS_SECS;
   let value = format!("{expiry}.{}", sign(key, expiry));
   Cookie::build((COOKIE, value))
      .path("/")
      .http_only(true)
      .secure(secure)
      .same_site(SameSite::Lax)
      .max_age(Duration::seconds(PASS_SECS))
      .build()
}

fn pass_valid(key: &str, jar: &CookieJar) -> bool {
   let Some(cookie) = jar.get(COOKIE) else {
      return false;
   };
   let Some((expiry, mac)) = cookie.value().split_once('.') else {
      return false;
   };
   let Ok(expiry) = expiry.parse::<i64>() else {
      return false;
   };
   let now = OffsetDateTime::now_utc().unix_timestamp();
   if expiry <= now || expiry > now + PASS_SECS + 60 {
      return false;
   }
   let Ok(bytes) = HEXLOWER.decode(mac.as_bytes()) else {
      return false;
   };
   hmac::verify(&mac_key(key), expiry.to_string().as_bytes(), &bytes).is_ok()
}

fn sign(key: &str, expiry: i64) -> String {
   HEXLOWER.encode(hmac::sign(&mac_key(key), expiry.to_string().as_bytes()).as_ref())
}

fn mac_key(key: &str) -> hmac::Key {
   hmac::Key::new(hmac::HMAC_SHA256, key.as_bytes())
}

fn token_id(token: &str) -> String {
   HEXLOWER.encode(ring::digest::digest(&ring::digest::SHA256, token.as_bytes()).as_ref())
}

fn remember(token: &str) {
   let now = OffsetDateTime::now_utc().unix_timestamp();
   let mut accepted = ACCEPTED.lock().unwrap_or_else(|err| err.into_inner());
   accepted.retain(|_, at| now - *at < 120);
   accepted.insert(token_id(token), now);
}

fn already_accepted(token: &str) -> bool {
   let now = OffsetDateTime::now_utc().unix_timestamp();
   ACCEPTED
      .lock()
      .unwrap_or_else(|err| err.into_inner())
      .get(&token_id(token))
      .is_some_and(|at| now - *at < 120)
}

async fn verify(secret: &str, token: &str) -> Result<(), Vec<String>> {
   let remoteip = crate::api::budget::current_client().and_then(|client| {
      client
         .as_str()
         .parse::<std::net::IpAddr>()
         .ok()
         .map(|ip| ip.to_string())
   });
   let mut form = vec![("secret", secret), ("response", token)];
   if let Some(ip) = &remoteip {
      form.push(("remoteip", ip.as_str()));
   }
   let response = CLIENT
      .post("https://challenges.cloudflare.com/turnstile/v0/siteverify")
      .form(&form)
      .send()
      .await
      .map_err(|err| {
         tracing::warn!("search check could not reach Cloudflare: {err}");
         vec!["connection".to_owned()]
      })?;
   let body: Siteverify = response.json().await.map_err(|err| {
      tracing::warn!("search check answer was not JSON: {err}");
      vec!["parse".to_owned()]
   })?;
   if body.success {
      Ok(())
   } else {
      Err(body.error_codes)
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   fn jar_with(value: &str) -> CookieJar {
      CookieJar::new().add(Cookie::new(COOKIE, value.to_owned()))
   }

   #[test]
   fn a_pass_lasts_five_minutes() {
      let key = "0123456789abcdef0123456789abcdef";
      let expiry = OffsetDateTime::now_utc().unix_timestamp() + PASS_SECS;
      let jar = jar_with(&format!("{expiry}.{}", sign(key, expiry)));
      assert!(pass_valid(key, &jar));
   }

   #[test]
   fn an_expired_or_rewritten_pass_is_refused() {
      let key = "0123456789abcdef0123456789abcdef";
      let old = OffsetDateTime::now_utc().unix_timestamp() - 1;
      let expired = jar_with(&format!("{old}.{}", sign(key, old)));
      assert!(!pass_valid(key, &expired));

      let expiry = OffsetDateTime::now_utc().unix_timestamp() + PASS_SECS;
      let flipped = expiry + 1;
      let rewritten = jar_with(&format!("{flipped}.{}", sign(key, expiry)));
      assert!(!pass_valid(key, &rewritten));

      let distant = OffsetDateTime::now_utc().unix_timestamp() + PASS_SECS + 120;
      let too_long = jar_with(&format!("{distant}.{}", sign(key, distant)));
      assert!(!pass_valid(key, &too_long));
   }
}
