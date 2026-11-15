//! Turn off the settings that hide posts from a cookie account.
//!
//! A fresh account hides sensitive media, and its search leaves out sensitive
//! posts and anything from an account it blocked or muted. That leaks into
//! teapawt search, so each cookie session is asked once to show sensitive
//! media and to switch both search settings off.
//!
//! The adult birthdate that lifts X's age gate is written during signup, so
//! nothing here touches the profile. Rewriting it on an account that already
//! had one was what broke the signup flow.

use std::time::Duration;

use axum::http::header;
use bytes::Bytes;
use serde::Deserialize;
use tokio::time::sleep;

use super::{ApiClient, SessionLease, endpoints};
use crate::{
   error::{Error, Result},
   types::SessionKind,
};

/// "Display media that may contain sensitive content", the toggle on
/// x.com/settings/content_you_see.
const DISPLAY_SENSITIVE_BODY: &str = concat!(
   "include_mention_filter=true&include_nsfw_user_flag=true&include_nsfw_admin_flag=true",
   "&include_ranked_timeline=true&include_alt_text_compose=true",
   "&include_ext_dm_av_call_settings=true&display_sensitive_media=true",
);

/// The two toggles under that page's search settings: `optInFiltering` is
/// "Hide sensitive content", `optInBlocking` is "Remove blocked and muted
/// accounts". Both are on for a new account.
const SEARCH_SAFETY_BODY: &str = r#"{"optInFiltering":false,"optInBlocking":false}"#;

/// Gap between two accounts, so a restart does not reach X as a burst. A
/// settings write is a page-level action, not a read: one account every five
/// seconds is about what a person changing their own settings produces, and
/// twelve at once is a pattern no browser makes.
const GAP: Duration = Duration::from_secs(5);

/// The importer names accounts `x-<id>` when it has a `twid` cookie.
fn imported_user_id(username: &str) -> Option<&str> {
   username
      .strip_prefix("x-")
      .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
   use super::imported_user_id;

   #[test]
   fn only_imported_numeric_user_ids_are_used_for_search_safety() {
      assert_eq!(imported_user_id("x-1989161722440216577"), Some("1989161722440216577"));
      for username in ["legacy123", "x-", "x-123/4", "x-１２３"] {
         assert_eq!(imported_user_id(username), None);
      }
   }
}

#[expect(
   clippy::multiple_inherent_impl,
   reason = "settings sync is split from GraphQL endpoint methods"
)]
impl ApiClient {
   /// Walk the cookie sessions in the background and turn those settings off.
   pub fn spawn_filter_sync(&self) {
      let client = self.clone();
      tokio::spawn(async move {
         client.sync_hiding_settings().await;
      });
   }

   async fn sync_hiding_settings(&self) {
      let ids = self.sessions.pending_filter_sync_ids().await;
      if ids.is_empty() {
         return;
      }
      tracing::info!(count = ids.len(), "turning off the settings that hide posts");

      let total = ids.len();
      let mut ok = 0_usize;
      let mut missing_user_id = 0_usize;
      let mut first_error = None;
      for id in ids {
         match self.clear_session_filters(id).await {
            Ok(true) => {
               self.sessions.mark_filters_cleared(id).await;
               ok += 1;
            },
            Ok(false) => missing_user_id += 1,
            Err(Error::UserSuspended(msg)) => {
               // A settings rejection says nothing conclusive about search access.
               self.sessions.mark_filters_failed(id).await;
               first_error.get_or_insert(msg);
            }
            Err(err) => {
               tracing::debug!(session_id = id, "could not change account settings: {err}");
               first_error.get_or_insert_with(|| err.to_string());
            },
         }
         sleep(GAP).await;
      }

      // One line per run rather than one per account: a start where X refuses
      // every write is a single fact, and thousands of warnings buried the
      // rest of the log the last time it happened.
      let failed = total - ok - missing_user_id;
      tracing::info!(ok, missing_user_id, failed, "account settings done");
      if let Some(err) = first_error {
         tracing::warn!(failed, "X refused some account settings: {err}");
      }
   }

   /// Apply the media setting even if the import didn't have an X user ID.
   /// `false` means `searchSafety` was skipped, not that both writes succeeded.
   async fn clear_session_filters(&self, session_id: i64) -> Result<bool> {
      let session = self
         .sessions
         .acquire_id(session_id, endpoints::ACCOUNT_SETTINGS)
         .await?;
      if session.kind != SessionKind::Cookie {
         return Ok(true);
      }

      self.set_display_sensitive_media(&session).await?;
      let Some(user_id) = imported_user_id(&session.username) else {
         return Ok(false);
      };
      self.set_search_safety(&session, user_id).await?;
      Ok(true)
   }

   async fn post_account_settings(&self, session: &SessionLease, form: &'static str) -> Result<Bytes> {
      let path = "/1.1/account/settings.json";
      let mut headers = self
         .settings_headers(
            session,
            "POST",
            path,
            "application/x-www-form-urlencoded",
         )
         .await?;
      headers.insert(
         header::REFERER,
         header::HeaderValue::from_static("https://x.com/settings/content_you_see"),
      );
      let response = self
         .client
         .post_on(
            endpoints::ACCOUNT_SETTINGS_URL,
            &headers,
            Bytes::from_static(form.as_bytes()),
            Some(&self.egress_for(session)),
         )
         .await?;
      let (bytes, _) = self
         .account_response(session, endpoints::ACCOUNT_SETTINGS, response)
         .await?;
      Self::check_api_errors(&bytes)?;
      Ok(bytes)
   }

   async fn set_display_sensitive_media(&self, session: &SessionLease) -> Result<()> {
      let bytes = self.post_account_settings(session, DISPLAY_SENSITIVE_BODY).await?;

      #[derive(Deserialize)]
      struct Settings {
         display_sensitive_media: bool,
      }
      if !serde_json::from_slice::<Settings>(&bytes)
         .is_ok_and(|settings| settings.display_sensitive_media)
      {
         return Err(Error::Internal(
            "account settings did not enable display_sensitive_media".into(),
         ));
      }
      Ok(())
   }

   async fn set_search_safety(&self, session: &SessionLease, user_id: &str) -> Result<()> {
      let path = format!("/i/api/1.1/strato/column/User/{user_id}/search/searchSafety");
      let url = format!("https://x.com{path}");
      let mut headers = self
         .settings_headers(session, "POST", &path, "application/json")
         .await?;
      headers.insert(
         header::REFERER,
         header::HeaderValue::from_static("https://x.com/settings/search"),
      );
      let response = self
         .client
         .post_on(
            &url,
            &headers,
            Bytes::from_static(SEARCH_SAFETY_BODY.as_bytes()),
            Some(&self.egress_for(session)),
         )
         .await?;
      let _ = self
         .account_response(session, endpoints::SEARCH_SAFETY, response)
         .await?;
      Ok(())
   }

   /// The headers a cookie session uses for its own settings writes, which go
   /// out as the browser the session is presented as.
   async fn settings_headers(
      &self,
      session: &SessionLease,
      method: &str,
      api_path: &str,
      content_type: &'static str,
   ) -> Result<header::HeaderMap> {
      let (bearer, tid) = self.bearer_and_tid_for(method, api_path).await;
      let ct0 = self.sessions.ct0(session.id).unwrap_or_else(|| session.ct0.clone());
      let mut headers = header::HeaderMap::new();
      headers.insert(
         header::AUTHORIZATION,
         header::HeaderValue::from_str(bearer)
            .map_err(|_| Error::Internal("invalid bearer token value".into()))?,
      );
      headers.insert(
         "x-twitter-auth-type",
         header::HeaderValue::from_static("OAuth2Session"),
      );
      headers.insert(
         "x-csrf-token",
         ct0.parse()
            .map_err(|_| Error::Internal("invalid ct0 header value".into()))?,
      );
      headers.insert(
         header::COOKIE,
         format!("auth_token={}; ct0={ct0}", session.auth_token)
            .parse()
            .map_err(|_| Error::Internal("invalid cookie header value".into()))?,
      );
      headers.insert(
         header::ORIGIN,
         header::HeaderValue::from_static("https://x.com"),
      );
      headers.insert(
         header::CONTENT_TYPE,
         header::HeaderValue::from_static(content_type),
      );
      headers.insert(header::ACCEPT, header::HeaderValue::from_static("*/*"));
      headers.insert(
         "x-twitter-active-user",
         header::HeaderValue::from_static("yes"),
      );
      headers.insert(
         "x-twitter-client-language",
         header::HeaderValue::from_static("en"),
      );
      if let Some(tid) = tid
         && let Ok(val) = tid.parse()
      {
         headers.insert("x-client-transaction-id", val);
      }
      Ok(headers)
   }
}