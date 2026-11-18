//! Ask X whether a stored verdict is still true.
//!
//! The pool learns a suspension once and never asks again: repeating a probe
//! across thousands of accounts is what starved the search pool before. That
//! makes the verdict cheap but unfalsifiable, and an operator has no way to
//! tell a genuinely suspended account from one that was benched by a wrong
//! reading of a response.
//!
//! This is the manual escape hatch. One account, one call, on demand. It is
//! never scheduled: nothing here runs unless an administrator asks for it.

use axum::http::header;
use bytes::Bytes;
use serde::Deserialize;

use super::{
   ApiClient,
   endpoints,
};
use crate::error::{
   Error,
   Result,
};

/// Form body for the settings write used as the probe. Same payload the filter
/// batch uses; it only toggles display_sensitive_media so it is idempotent.
const PROBE_SETTINGS_BODY: &str = concat!(
   "include_mention_filter=true&include_nsfw_user_flag=true&include_nsfw_admin_flag=true",
   "&include_ranked_timeline=true&include_alt_text_compose=true",
   "&include_ext_dm_av_call_settings=true&display_sensitive_media=true",
);

/// What X said about one account, asked once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
   /// X accepted the settings write. The account is live and can act.
   Alive,
   /// X refused the settings write with code 64 or 63. That is all this
   /// answer shows: X gates the settings endpoint apart from reading, and
   /// accounts refused here have answered search and profile calls with 200.
   /// It is not proof of a suspension, and it clears nothing either.
   SettingsRefused,
   /// The credentials were refused outright (401, 403 code 326, bad csrf). The
   /// account cannot act, but this says nothing about a suspension.
   CredentialsRejected,
}

impl ProbeVerdict {
   /// The word the admin page shows and its script switches on. Keep these
   /// in step with `public/js/admin-stats.js`.
   pub const fn label(self) -> &'static str {
      match self {
         Self::Alive => "alive",
         Self::SettingsRefused => "settings refused",
         Self::CredentialsRejected => "credentials rejected",
      }
   }
}

#[expect(
   clippy::multiple_inherent_impl,
   reason = "the probe is split from GraphQL endpoint methods"
)]
impl ApiClient {
   /// Ask X whether a session can execute an authenticated `SearchTimeline`
   /// read with non-empty results.
   pub(super) async fn probe_search_capable(&self, session_id: i64) -> Result<bool> {
      let session = self.sessions.probe_lease(session_id).await?;
      self
         .graphql_request_inner::<super::schema::SearchTimelineData>(
            &session,
            endpoints::GRAPH_SEARCH_TIMELINE,
            &endpoints::search_vars("news", None, "Latest"),
            endpoints::SEARCH_FEATURES,
            None,
         )
         .await
         .map(|(_, bytes)| !super::client::search_answer_is_empty(&bytes))
   }

   /// Ask X about one account with a settings write, falling back to a single
   /// authenticated GraphQL read when `account/settings` returns 64 or 63.
   ///
   /// A successful POST to `account/settings` proves the account can act:
   /// a suspended account cannot write settings. When X gates the settings
   /// endpoint with 64 or 63 while the account can still search or read
   /// profiles, one GraphQL read over the account's sticky exit verifies
   /// whether it is actually live without loading `x.com` HTML.
   pub async fn probe_account(&self, session_id: i64) -> Result<ProbeVerdict> {
      // Drop the settings lease before any follow-up GraphQL probe acquires
      // another permit on the same slot.
      let (status, body) = {
         let session = self.sessions.probe_lease(session_id).await?;
         let path = "/1.1/account/settings.json";
         let mut headers = self
            .settings_headers(&session, "POST", path, "application/x-www-form-urlencoded")
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
               Bytes::from_static(PROBE_SETTINGS_BODY.as_bytes()),
               Some(&self.egress_for(&session)),
            )
            .await?;
         let status = response.status();
         let body = response.text().await.unwrap_or_default();
         (status, body)
      };

      if status.is_success() {
         #[derive(Deserialize)]
         struct Settings {
            display_sensitive_media: bool,
         }
         return match serde_json::from_str::<Settings>(&body) {
            Ok(settings) => {
               let _ = settings.display_sensitive_media;
               Ok(ProbeVerdict::Alive)
            },
            // A 200 without a parseable settings body is not a confirmed answer.
            Err(_) => Err(Error::TransientUpstream),
         };
      }

      match super::twitter_error_code(&body) {
         Some(64 | 63) => self.probe_read_after_settings_refusal(session_id).await,
         Some(326 | 353 | 89) => Ok(ProbeVerdict::CredentialsRejected),
         _ if status.as_u16() == 401 => Ok(ProbeVerdict::CredentialsRejected),
         _ => Err(Error::TwitterApi(format!("Status {status}: {body}"))),
      }
   }

   async fn probe_read_after_settings_refusal(&self, session_id: i64) -> Result<ProbeVerdict> {
      let wants_search = self
         .sessions
         .account_state(session_id)
         .await
         .is_some_and(|st| st.pool == "search" || st.demoted_from_search);

      if wants_search {
         return match self.probe_search_capable(session_id).await {
            Ok(true) => Ok(ProbeVerdict::Alive),
            Err(Error::SessionRejected(_)) => Ok(ProbeVerdict::CredentialsRejected),
            Ok(false) | Err(_) => Ok(ProbeVerdict::SettingsRefused),
         };
      }

      let read_session = self.sessions.probe_lease(session_id).await?;
      match self
         .graphql_request_inner::<serde_json::Value>(
            &read_session,
            endpoints::GRAPH_USER,
            &endpoints::user_by_screen_name_vars("x"),
            endpoints::GQL_FEATURES,
            Some(endpoints::USER_FIELD_TOGGLES),
         )
         .await
      {
         Ok((val, _)) if has_user_result(&val) => Ok(ProbeVerdict::Alive),
         Err(Error::SessionRejected(_)) => Ok(ProbeVerdict::CredentialsRejected),
         Ok(_) | Err(_) => Ok(ProbeVerdict::SettingsRefused),
      }
   }
}

fn has_user_result(data: &serde_json::Value) -> bool {
   [
      "/user/result",
      "/user_result/result",
      "/user_results/result",
      "/data/user/result",
   ]
   .into_iter()
   .find_map(|path| data.pointer(path))
   .and_then(serde_json::Value::as_object)
   .is_some_and(|obj| !obj.is_empty())
}

#[cfg(test)]
mod tests {
   use serde_json::json;

   use super::has_user_result;

   #[test]
   fn detects_user_result_in_unwrapped_or_envelope_json() {
      assert!(has_user_result(&json!({
         "user": { "result": { "__typename": "User", "rest_id": "783214" } }
      })));
      assert!(has_user_result(&json!({
         "data": { "user": { "result": { "__typename": "User", "rest_id": "783214" } } }
      })));
      assert!(!has_user_result(&json!({ "user": { "result": null } })));
      assert!(!has_user_result(&json!({ "user": {} })));
   }
}