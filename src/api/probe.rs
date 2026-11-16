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
   /// X refused the account itself with 403 code 64 on a real write path,
   /// which is distinct from the settings gate that refuses some live accounts.
   Suspended,
   /// The credentials were refused outright (401, 403 code 326, bad csrf). The
   /// account cannot act, but this says nothing about a suspension.
   CredentialsRejected,
}

impl ProbeVerdict {
   pub const fn label(self) -> &'static str {
      match self {
         Self::Alive => "alive",
         Self::Suspended => "suspended",
         Self::CredentialsRejected => "credentials rejected",
      }
   }
}

#[expect(
   clippy::multiple_inherent_impl,
   reason = "the probe is split from GraphQL endpoint methods"
)]
impl ApiClient {
   /// Ask X about one account with a settings write.
   ///
   /// A successful POST to `account/settings` proves the account can act:
   /// a suspended account cannot write settings. `verify_credentials` only
   /// checks whether credentials are accepted, which a suspended account can
   /// pass while still being unable to perform any real action.
   pub async fn probe_account(&self, session_id: i64) -> Result<ProbeVerdict> {
      // A probe deliberately ignores the pool's verdict: that is the point.
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

      if status.is_success() {
         #[derive(Deserialize)]
         struct Settings {
            display_sensitive_media: bool,
         }
         return match serde_json::from_str::<Settings>(&body) {
            Ok(_) => Ok(ProbeVerdict::Alive),
            // A 200 without a parseable settings body is not a confirmed answer.
            Err(_) => Err(Error::TransientUpstream),
         };
      }

      match super::twitter_error_code(&body) {
         Some(64 | 63) => Ok(ProbeVerdict::Suspended),
         Some(326 | 353) | Some(89) => Ok(ProbeVerdict::CredentialsRejected),
         _ if status.as_u16() == 401 => Ok(ProbeVerdict::CredentialsRejected),
         _ => Err(Error::TwitterApi(format!("Status {status}: {body}"))),
      }
   }
}