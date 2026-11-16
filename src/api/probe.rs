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

use serde::Deserialize;

use super::{
   ApiClient,
   endpoints,
};
use crate::error::{
   Error,
   Result,
};

/// What X said about one account, asked once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
   /// X answered for the account. The credentials are alive and the account is
   /// not suspended, whatever the stored verdict says.
   Alive,
   /// X refused the account itself: 403 code 64, or code 63 in a JSON body.
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
   /// Ask X about one account with `verify_credentials`.
   ///
   /// The call is the one the pool already uses to resolve an account's own id,
   /// so it is known to answer for a cookie session without a transaction id
   /// for the settings path. A suspended account refuses it with code 64, which
   /// is the same answer the settings write gives — and that is exactly why a
   /// settings refusal alone was never proof of anything.
   pub async fn probe_account(&self, session_id: i64) -> Result<ProbeVerdict> {
      // A probe deliberately ignores the pool's verdict: that is the point.
      let session = self.sessions.probe_lease(session_id).await?;
      let path = "/1.1/account/verify_credentials.json";
      let headers = self
         .settings_headers(&session, "GET", path, "application/x-www-form-urlencoded")
         .await?;
      let response = self
         .client
         .get_on(
            endpoints::VERIFY_CREDENTIALS_URL,
            &headers,
            Some(&self.egress_for(&session)),
         )
         .await?;
      let status = response.status();
      let body = response.text().await.unwrap_or_default();

      if status.is_success() {
         // A body without an id is not an answer about this account.
         #[derive(Deserialize)]
         struct Own {
            id_str: String,
         }
         return match serde_json::from_str::<Own>(&body) {
            Ok(own) if !own.id_str.is_empty() => Ok(ProbeVerdict::Alive),
            _ => Err(Error::TransientUpstream),
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