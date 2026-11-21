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

/// Which live test an operator asked to run on one account.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProbeMode {
   /// Default health probe: POST `account/settings`, with a GraphQL read
   /// fallback when X gates settings with code 64 or 63.
   #[default]
   Health,
   /// Direct `SearchTimeline` probe: executes an authenticated search query so
   /// unverified or fixed search accounts can be verified immediately.
   Search,
}

/// What X said about one account, asked once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
   /// X accepted the settings write or authenticated read. The account is live.
   Alive,
   /// X refused the settings write with code 64 or 63. That is all this
   /// answer shows: X gates the settings endpoint apart from reading, and
   /// accounts refused here have answered search and profile calls with 200.
   /// It is not proof of a suspension, and it clears nothing either.
   SettingsRefused,
   /// X rejected the `ct0` CSRF token (HTTP 403 code 353).
   CsrfRejected,
   /// The credentials were refused outright (401, 403 code 326, code 89). The
   /// account cannot act, but this says nothing about a suspension.
   CredentialsRejected,
   /// `SearchTimeline` returned HTTP 200 with zero tweets (possible search
   /// ban, age gate, or filter).
   SearchEmpty,
}

impl ProbeVerdict {
   /// The word the admin page shows and its script switches on. Keep these
   /// in step with `public/js/admin-stats.js`.
   pub const fn label(self) -> &'static str {
      match self {
         Self::Alive => "alive",
         Self::SettingsRefused => "settings refused",
         Self::CsrfRejected => "stale ct0",
         Self::CredentialsRejected => "credentials rejected",
         Self::SearchEmpty => "search empty",
      }
   }
}

/// Detailed outcome of a single manual operator probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
   pub verdict:         ProbeVerdict,
   pub detail:          Option<String>,
   /// True when this probe received non-empty `SearchTimeline` results.
   pub verified_search: bool,
   /// Replacement `ct0` returned in `Set-Cookie`, if X issued one.
   pub fresh_ct0:       Option<String>,
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

   /// Ask X about one account using the requested [`ProbeMode`].
   pub async fn probe_account_with_mode(
      &self,
      session_id: i64,
      mode: ProbeMode,
   ) -> Result<ProbeReport> {
      match mode {
         ProbeMode::Health => self.probe_health(session_id).await,
         ProbeMode::Search => self.probe_search(session_id).await,
      }
   }

   /// Record that an operator probe verified `SearchTimeline` for a search-pool
   /// account so it immediately counts as verified instead of unverified.
   pub(crate) async fn record_probe_search_verified(&self, session_id: i64) {
      self
         .sessions
         .mark_verified(session_id, endpoints::GRAPH_SEARCH_TIMELINE)
         .await;
      self.sessions.record_tweet_search_ok(session_id, true);
   }

   /// Ask X about one account with a settings write, falling back to a single
   /// authenticated GraphQL read when `account/settings` returns 64 or 63.
   ///
   /// A successful POST to `account/settings` proves the account can act:
   /// a suspended account cannot write settings. When X gates the settings
   /// endpoint with 64 or 63 while the account can still search or read
   /// profiles, one GraphQL read over the account's sticky exit verifies
   /// whether it is actually live without loading `x.com` HTML.
   async fn probe_health(&self, session_id: i64) -> Result<ProbeReport> {
      // Drop the settings lease before any follow-up GraphQL probe acquires
      // another permit on the same slot.
      let (status, fresh_ct0, body) = {
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
         let fresh_ct0 = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .find_map(super::client::ct0_cookie);
         let body = response.text().await.unwrap_or_default();
         (status, fresh_ct0, body)
      };

      if status.is_success() {
         #[derive(Deserialize)]
         struct Settings {
            display_sensitive_media: bool,
         }
         return match serde_json::from_str::<Settings>(&body) {
            Ok(settings) => {
               let _ = settings.display_sensitive_media;
               Ok(ProbeReport {
                  verdict: ProbeVerdict::Alive,
                  detail: Some("account/settings accepted (HTTP 200)".to_owned()),
                  verified_search: false,
                  fresh_ct0,
               })
            },
            // A 200 without a parseable settings body is not a confirmed answer.
            Err(_) => Err(Error::TransientUpstream),
         };
      }

      match super::twitter_error_code(&body) {
         Some(64 | 63) => self.probe_read_after_settings_refusal(session_id).await,
         Some(353) => {
            Ok(ProbeReport {
               verdict: ProbeVerdict::CsrfRejected,
               detail: Some(format!(
                  "HTTP {status} code 353 — CSRF token (ct0) rejected by X"
               )),
               verified_search: false,
               fresh_ct0,
            })
         },
         Some(326) => {
            Ok(ProbeReport {
               verdict: ProbeVerdict::CredentialsRejected,
               detail: Some(format!(
                  "HTTP {status} code 326 — account locked or requires verification"
               )),
               verified_search: false,
               fresh_ct0,
            })
         },
         Some(89) => {
            Ok(ProbeReport {
               verdict: ProbeVerdict::CredentialsRejected,
               detail: Some(format!("HTTP {status} code 89 — invalid or expired token")),
               verified_search: false,
               fresh_ct0,
            })
         },
         _ if status.as_u16() == 401 => {
            Ok(ProbeReport {
               verdict: ProbeVerdict::CredentialsRejected,
               detail: Some(format!("HTTP {status} — unauthorized (auth_token rejected)")),
               verified_search: false,
               fresh_ct0,
            })
         },
         _ => Err(Error::TwitterApi(format!("Status {status}: {body}"))),
      }
   }

   async fn probe_search(&self, session_id: i64) -> Result<ProbeReport> {
      let ct0_before = self.sessions.ct0(session_id);
      match self.probe_search_capable(session_id).await {
         Ok(true) => {
            Ok(ProbeReport {
               verdict:         ProbeVerdict::Alive,
               detail:          Some("SearchTimeline returned posts (HTTP 200)".to_owned()),
               verified_search: true,
               fresh_ct0:       None,
            })
         },
         Ok(false) => {
            Ok(ProbeReport {
               verdict:         ProbeVerdict::SearchEmpty,
               detail:          Some(
                  "SearchTimeline returned HTTP 200 with 0 results (possible search filter, age \
                   gate, or search ban)"
                     .to_owned(),
               ),
               verified_search: false,
               fresh_ct0:       None,
            })
         },
         Err(Error::SessionRejected(msg)) => {
            Ok(ProbeReport {
               verdict:         ProbeVerdict::CredentialsRejected,
               detail:          Some(msg),
               verified_search: false,
               fresh_ct0:       None,
            })
         },
         Err(Error::TransientUpstream) => {
            let ct0_after = self.sessions.ct0(session_id);
            let ct0_dead = self
               .sessions
               .account_state(session_id)
               .await
               .is_some_and(|st| st.ct0_dead);
            if ct0_dead {
               Ok(ProbeReport {
                  verdict:         ProbeVerdict::CsrfRejected,
                  detail:          Some(
                     "SearchTimeline rejected the CSRF token (HTTP 403 code 353)".to_owned(),
                  ),
                  verified_search: false,
                  fresh_ct0:       None,
               })
            } else if ct0_after != ct0_before && ct0_after.is_some() {
               Ok(ProbeReport {
                  verdict:         ProbeVerdict::CsrfRejected,
                  detail:          Some(
                     "SearchTimeline returned code 353 and X issued a replacement ct0 (saved; \
                      run probe again to verify)"
                        .to_owned(),
                  ),
                  verified_search: false,
                  fresh_ct0:       None,
               })
            } else {
               Err(Error::TransientUpstream)
            }
         },
         Err(err) => Err(err),
      }
   }

   async fn probe_read_after_settings_refusal(&self, session_id: i64) -> Result<ProbeReport> {
      let wants_search = self
         .sessions
         .account_state(session_id)
         .await
         .is_some_and(|st| st.pool == "search" || st.demoted_from_search);

      if wants_search {
         return match self.probe_search_capable(session_id).await {
            Ok(true) => {
               Ok(ProbeReport {
                  verdict:         ProbeVerdict::Alive,
                  detail:          Some(
                     "account/settings refused (code 64/63), but SearchTimeline returned posts"
                        .to_owned(),
                  ),
                  verified_search: true,
                  fresh_ct0:       None,
               })
            },
            Err(Error::SessionRejected(msg)) => {
               Ok(ProbeReport {
                  verdict:         ProbeVerdict::CredentialsRejected,
                  detail:          Some(msg),
                  verified_search: false,
                  fresh_ct0:       None,
               })
            },
            Ok(false) => {
               Ok(ProbeReport {
                  verdict:         ProbeVerdict::SettingsRefused,
                  detail:          Some(
                     "account/settings refused (code 64/63) and SearchTimeline returned 0 results"
                        .to_owned(),
                  ),
                  verified_search: false,
                  fresh_ct0:       None,
               })
            },
            Err(err) => {
               Ok(ProbeReport {
                  verdict:         ProbeVerdict::SettingsRefused,
                  detail:          Some(format!(
                     "account/settings refused (code 64/63); SearchTimeline fallback failed: {err}"
                  )),
                  verified_search: false,
                  fresh_ct0:       None,
               })
            },
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
         Ok((val, _)) if has_user_result(&val) => {
            Ok(ProbeReport {
               verdict:         ProbeVerdict::Alive,
               detail:          Some(
                  "account/settings refused (code 64/63), but UserByScreenName succeeded"
                     .to_owned(),
               ),
               verified_search: false,
               fresh_ct0:       None,
            })
         },
         Err(Error::SessionRejected(msg)) => {
            Ok(ProbeReport {
               verdict:         ProbeVerdict::CredentialsRejected,
               detail:          Some(msg),
               verified_search: false,
               fresh_ct0:       None,
            })
         },
         Ok(_) | Err(_) => {
            Ok(ProbeReport {
               verdict:         ProbeVerdict::SettingsRefused,
               detail:          Some(
                  "account/settings refused (code 64/63) and UserByScreenName fallback returned \
                   no user"
                     .to_owned(),
               ),
               verified_search: false,
               fresh_ct0:       None,
            })
         },
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