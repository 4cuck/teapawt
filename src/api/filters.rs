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
const DISPLAY_SENSITIVE_BODY: &str = "display_sensitive_media=true";

/// The two toggles under that page's search settings: `optInFiltering` is
/// "Hide sensitive content", `optInBlocking` is "Remove blocked and muted
/// accounts". Both are on for a new account.
const SEARCH_SAFETY_BODY: &str = r#"{"optInFiltering":false,"optInBlocking":false}"#;

/// Gap between two accounts, so a restart does not reach X as a burst. A
/// settings write is a page-level action, not a read: one account every five
/// seconds is about what a person changing their own settings produces, and
/// twelve at once is a pattern no browser makes.
const GAP: Duration = Duration::from_secs(5);

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
      let mut first_error = None;
      for id in ids {
         match self.clear_session_filters(id).await {
            Ok(()) => {
               self.sessions.mark_filters_cleared(id).await;
               ok += 1;
            },
            Err(Error::UserSuspended(msg)) => {
               // X answers a suspended account's settings write with "not
               // permitted to access this feature". Nothing about waiting
               // changes that, so this account is not written again.
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
      let failed = total - ok;
      tracing::info!(ok, failed, "account settings done");
      if let Some(err) = first_error {
         tracing::warn!(failed, "X refused some account settings: {err}");
      }
   }

   /// Both writes for one account. A session that is not a cookie has no
   /// settings to change.
   async fn clear_session_filters(&self, session_id: i64) -> Result<()> {
      let session = self
         .sessions
         .acquire_id(session_id, endpoints::ACCOUNT_SETTINGS)
         .await?;
      if session.kind != SessionKind::Cookie {
         return Ok(());
      }

      let user_id = self.session_user_id(&session).await?;
      self.set_display_sensitive_media(&session).await?;
      self.set_search_safety(&session, &user_id).await
   }

   /// The numeric id in the `searchSafety` path. Imported accounts are named
   /// `x-<id>`, so most never ask; the rest are told by x.com.
   async fn session_user_id(&self, session: &SessionLease) -> Result<String> {
      if let Some(rest_id) = session.username.strip_prefix("x-")
         && rest_id.chars().all(|ch| ch.is_ascii_digit())
         && !rest_id.is_empty()
      {
         return Ok(rest_id.to_owned());
      }

      let path = "/1.1/account/verify_credentials.json";
      let headers = self
         .settings_headers(session, "GET", path, "application/json")
         .await?;
      let response = self
         .client
         .get_on(
            endpoints::VERIFY_CREDENTIALS_URL,
            &headers,
            Some(&self.egress_for(session)),
         )
         .await?;
      let (bytes, _) = self
         .account_response(session, endpoints::VERIFY_CREDENTIALS, response)
         .await?;

      #[derive(Deserialize)]
      struct Verify {
         id_str: Option<String>,
      }
      let verify: Verify = serde_json::from_slice(&bytes)
         .map_err(|err| Error::Internal(format!("verify_credentials parse: {err}")))?;
      verify
         .id_str
         .filter(|id| !id.is_empty())
         .ok_or_else(|| Error::Internal("verify_credentials missing id_str".into()))
   }

   /// Ask X whether this account is suspended, by writing one setting.
   ///
   /// An empty search page is also what a query with no matches and an
   /// age-gated account produce, so it cannot be read as a verdict. A settings
   /// write can: a suspended account is answered "not permitted to access this
   /// feature", a working one takes the write. `None` means the answer said
   /// nothing either way and the account should be asked again later.
   pub(crate) async fn probe_search_suspension(&self, session_id: i64) -> Option<bool> {
      let session = self
         .sessions
         .acquire_id(session_id, endpoints::ACCOUNT_SETTINGS)
         .await
         .ok()?;
      match self.set_display_sensitive_media(&session).await {
         Ok(()) => Some(false),
         Err(Error::UserSuspended(_)) => Some(true),
         Err(err) => {
            tracing::debug!(session_id, "suspension probe inconclusive: {err}");
            None
         },
      }
   }

   async fn set_display_sensitive_media(&self, session: &SessionLease) -> Result<()> {
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
            Bytes::from_static(DISPLAY_SENSITIVE_BODY.as_bytes()),
            Some(&self.egress_for(session)),
         )
         .await?;
      let (bytes, _) = self
         .account_response(session, endpoints::ACCOUNT_SETTINGS, response)
         .await?;

      // The reply carries the whole settings object. A refusal that still
      // answers 200 would otherwise look like success and never be retried.
      #[derive(Deserialize)]
      struct Settings {
         display_sensitive_media: Option<bool>,
      }
      if let Ok(settings) = serde_json::from_slice::<Settings>(&bytes)
         && settings.display_sensitive_media != Some(true)
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