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

use std::{
   collections::BTreeMap,
   time::Duration,
};

use axum::http::header;
use bytes::Bytes;
use serde::Deserialize;
use tokio::time::sleep;

use super::{
   ApiClient,
   SessionLease,
   auth::SuspensionProbe,
   endpoints,
};
use crate::{
   error::{
      Error,
      Result,
   },
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

/// Accounts written per batch. Small enough that a batch finishes in a couple
/// of minutes and logs a line, so a long backlog is visible while it drains
/// and a fresh import is picked up by the next batch rather than after hours.
const BATCH: usize = 25;

/// How often to look for accounts whose settings still need writing.
///
/// The first pass runs at startup. The repeat is what picks up accounts
/// imported through `/import` afterwards: reconciliation adds them to the pool
/// within a minute, and this is what notices them.
const RESCAN_EVERY: Duration = Duration::from_mins(30);

/// How long X's refusal of the settings write is believed when it names the
/// account itself (code 64, "not permitted to access this feature"). A
/// suspension is often lifted, so the account is asked again rather than
/// staying wrong forever.
pub(super) const REFUSAL_MEMORY: Duration = Duration::from_hours(24);

/// How long to wait when the write was refused without naming the account: a
/// bodyless 404 with code 34 means X would not take the transaction id, not
/// that the account is bad. That is worth one rescan, not a day.
const UNEXPLAINED_REFUSAL: Duration = Duration::from_hours(1);

/// How long to wait when the numeric id the `searchSafety` write is keyed by
/// could not be resolved. The media toggle already landed; this only retries
/// the second write.
const UNRESOLVED_ID: Duration = Duration::from_hours(6);

/// What one batch of settings writes did.
///
/// The split that matters is between an account X answered about and one that
/// was never asked. A paced account left the pending list and the next batch is
/// a different one, so the queue moved; an account whose own slot was resting
/// or rate-limited is still pending, so the queue did not. Lumping the two
/// together is what made a batch of 25 locked accounts look like no progress at
/// all, and stopped the drain after 25 of them every half hour.
#[derive(Default)]
struct BatchOutcome {
   /// The settings are off.
   ok:          usize,
   /// The media toggle landed but the numeric id is still unknown.
   unresolved:  usize,
   /// X answered about the account and the write is paced.
   refused:     usize,
   /// The account could not be used just now, so it is still pending.
   unusable:    usize,
   /// The first refusal, for the one warning a batch prints.
   first_error: Option<String>,
}

impl BatchOutcome {
   /// Accounts the batch took off the pending list, either fixed or paced.
   const fn settled(&self) -> usize {
      self.ok + self.unresolved + self.refused
   }
}

/// The importer names accounts `x-<id>` when it has a `twid` cookie.
fn imported_user_id(username: &str) -> Option<&str> {
   username
      .strip_prefix("x-")
      .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The account's own id from its `twid` cookie, which X sets on every visit.
fn twid_user_id(cookies: &BTreeMap<String, String>) -> Option<String> {
   let twid = cookies.get("twid")?.trim_matches('"');
   let id = twid
      .strip_prefix("u%3D")
      .or_else(|| twid.strip_prefix("u%3d"))
      .or_else(|| twid.strip_prefix("u="))?;
   (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())).then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
   use std::collections::BTreeMap;

   use super::{
      imported_user_id,
      twid_user_id,
   };

   #[test]
   fn only_imported_numeric_user_ids_are_used_for_search_safety() {
      assert_eq!(
         imported_user_id("x-1989161722440216577"),
         Some("1989161722440216577")
      );
      for username in ["legacy123", "x-", "x-123/4", "x-１２３"] {
         assert_eq!(imported_user_id(username), None);
      }
   }

   #[test]
   fn a_twid_cookie_names_the_account_id() {
      let mut cookies = BTreeMap::new();
      cookies.insert("twid".to_owned(), "u%3D1989161722440216577".to_owned());
      assert_eq!(
         twid_user_id(&cookies).as_deref(),
         Some("1989161722440216577")
      );

      cookies.insert("twid".to_owned(), "\"u%3D42\"".to_owned());
      assert_eq!(twid_user_id(&cookies).as_deref(), Some("42"));

      cookies.insert("twid".to_owned(), "u=7".to_owned());
      assert_eq!(twid_user_id(&cookies).as_deref(), Some("7"));

      for value in ["", "u%3D", "not-an-id", "u%3Dabc", "u%3D1/2"] {
         cookies.insert("twid".to_owned(), value.to_owned());
         assert_eq!(twid_user_id(&cookies), None, "{value}");
      }
      assert_eq!(twid_user_id(&BTreeMap::new()), None);
   }
}

#[expect(
   clippy::multiple_inherent_impl,
   reason = "settings sync is split from GraphQL endpoint methods"
)]
impl ApiClient {
   /// Walk the cookie sessions in the background and turn those settings off.
   ///
   /// Runs at startup, then whenever accounts are imported, then on a slow
   /// timer for anything a pass could not settle. A pass with nothing pending
   /// returns without a request.
   pub fn spawn_filter_sync(&self) {
      let client = self.clone();
      tokio::spawn(async move {
         let mut every = tokio::time::interval(RESCAN_EVERY);
         every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
         loop {
            tokio::select! {
               _ = every.tick() => {},
               () = client.filter_wakeup.notified() => {},
            }
            client.sync_hiding_settings().await;
         }
      });
   }

   async fn sync_hiding_settings(&self) {
      // Batches, not one pass over everything. A pass paced at one write every
      // five seconds needs hours for a full pool, and a pass that long is
      // invisible while it runs and delays a fresh import until it ends.
      // Each batch is bounded, logged, and re-reads the pending list, so an
      // import that wakes the sync is served by the next batch.
      let mut round = 0_u32;
      loop {
         let pending = self.sessions.pending_filter_sync_ids().await;
         if pending.is_empty() {
            if round == 0 {
               return;
            }
            break;
         }
         round += 1;
         if round == 1 {
            tracing::info!(
               pending = pending.len(),
               "turning off the settings that hide posts"
            );
         }
         let ids: Vec<i64> = pending.into_iter().take(BATCH).collect();
         let outcome = self.run_settings_batch(&ids, round).await;
         // Every account in this batch was unusable for a reason that is not
         // about the account — resting, rate-limited, or the pool was empty.
         // The next batch would be the same 25, so stop and let the rescan
         // timer come back rather than walking a thousand slots that cannot be
         // written right now.
         if outcome.settled() == 0 {
            break;
         }
      }
   }

   /// Write the settings for one batch and report how it went.
   async fn run_settings_batch(&self, ids: &[i64], round: u32) -> BatchOutcome {
      let mut outcome = BatchOutcome::default();
      for &id in ids {
         match self.clear_session_filters(id).await {
            Ok(true) => {
               self.sessions.mark_filters_cleared(id).await;
               outcome.ok += 1;
            },
            Ok(false) => {
               // The media toggle landed; only the numeric id the
               // search-safety write is keyed by is missing.
               outcome.unresolved += 1;
               self
                  .sessions
                  .mark_filters_failed(
                     id,
                     "The account's numeric id could not be resolved for the search-safety write",
                     UNRESOLVED_ID,
                  )
                  .await;
            },
            // No transaction id: nothing was asked of this account, and the
            // next account would get the same answer. Stop the batch instead of
            // walking the rest at five seconds each.
            Err(Error::NoTransactionId) => {
               outcome.unusable = ids.len() - outcome.settled();
               break;
            },
            // This account could not be used just now — rate-limited, resting,
            // or gone from the pool. X was never asked about it, so it stays
            // pending and the batch moves on. Logged because a batch of nothing
            // but these looks identical to a batch of refusals, and the two
            // need different fixes.
            Err(err @ Error::NoSessions | err @ Error::RateLimited) => {
               tracing::info!(session_id = id, "settings write skipped: {err}");
               outcome.unusable += 1;
            },
            // X refuses the account itself (403 code 64): it is suspended. This
            // write is the one probe that asks that question, so it runs once
            // and the answer is kept. The account leaves search for the view
            // pool, where its cookie may still read profiles, and nothing ever
            // writes its settings again — a daily retry across thousands of
            // suspended accounts exists only to be told what is already known.
            Err(err @ Error::UserSuspended(_)) => {
               tracing::info!(session_id = id, "X says this account is suspended: {err}");
               self
                  .sessions
                  .note_suspended(
                     id,
                     SuspensionProbe::Settings,
                     None,
                     "Settings write refused: account suspended",
                  )
                  .await;
               outcome.refused += 1;
            },
            // 326 is an account lock and 401 is a dead token. Take the account
            // permanently out of rotation and never schedule a 24-hour retry:
            // locked or rejected credentials stay benched across restarts until
            // an operator explicitly reinstates them.
            Err(err @ Error::SessionRejected(_)) => {
               tracing::info!(
                  session_id = id,
                  "X rejected this account's credentials; taking it out of rotation: {err}"
               );
               self.sessions.mark_rejected(id).await;
               outcome.refused += 1;
            },
            Err(err) => {
               // A settings rejection says nothing conclusive about search
               // access, so it is paced rather than written to `search_banned`.
               tracing::debug!(session_id = id, "could not change account settings: {err}");
               // Code 34 is X refusing the transaction id, not the account.
               // Believing it for a day is what left every imported account
               // hiding NSFW posts after a bad id generator shipped.
               let wait = if matches!(err, Error::AccountSettingsNotFound) {
                  UNEXPLAINED_REFUSAL
               } else {
                  REFUSAL_MEMORY
               };
               self
                  .sessions
                  .mark_filters_failed(
                     id,
                     &format!("X refused the account settings write: {err}"),
                     wait,
                  )
                  .await;
               outcome.refused += 1;
               outcome.first_error.get_or_insert_with(|| err.to_string());
            },
         }
         sleep(GAP).await;
      }

      // One line per batch rather than one per account: a batch where X refuses
      // every write is a single fact, and thousands of warnings buried the rest
      // of the log the last time it happened.
      let BatchOutcome {
         ok,
         unresolved,
         refused,
         unusable,
         ..
      } = outcome;
      tracing::info!(
         round,
         ok,
         unresolved,
         refused,
         unusable,
         "account settings done"
      );
      if let Some(err) = outcome.first_error.as_deref() {
         tracing::warn!(refused, "X refused some account settings: {err}");
      }
      outcome
   }

   /// Write both settings for one account.
   ///
   /// `false` means the media toggle landed but `searchSafety` could not be
   /// written because the numeric id it is keyed by was not resolvable — not
   /// that the account is fine.
   async fn clear_session_filters(&self, session_id: i64) -> Result<bool> {
      // Resolve the id first. Its fallbacks make their own calls, and holding
      // this account's lease while they run would nest one lease inside
      // another on the same slot.
      let Some(user_id) = self.session_user_id(session_id).await else {
         // The media toggle is still worth writing; it is the one that hides
         // NSFW media. `false` records that search safety is outstanding.
         let session = self
            .sessions
            .acquire_id(session_id, endpoints::ACCOUNT_SETTINGS)
            .await?;
         if session.kind == SessionKind::Cookie {
            // A freshly imported cookie solves the JSD challenge before its
            // first write to x.com, so the settings call carries cf_clearance.
            self.ensure_search_clearance(&session).await;
            self.set_display_sensitive_media(&session).await?;
         }
         return Ok(false);
      };

      let session = self
         .sessions
         .acquire_id(session_id, endpoints::ACCOUNT_SETTINGS)
         .await?;
      if session.kind != SessionKind::Cookie {
         return Ok(true);
      }

      // A freshly imported cookie solves the JSD challenge before its first
      // write to x.com, so the settings calls carry cf_clearance.
      self.ensure_search_clearance(&session).await;
      self.set_display_sensitive_media(&session).await?;
      self.set_search_safety(&session, &user_id).await?;
      Ok(true)
   }

   /// The numeric id `searchSafety` is keyed by.
   ///
   /// Only the importer's `twid` cookie and the id cache name it for free. A
   /// handle-named import has neither, so the account's own
   /// `verify_credentials` answer is asked for it: that call names the session
   /// itself, so it works whatever the import was called, and it does not
   /// depend on the profile query X sometimes refuses.
   async fn session_user_id(&self, session_id: i64) -> Option<String> {
      let credentials = self.sessions.credentials(session_id)?;
      let username = credentials.username.as_str();
      if let Some(id) = imported_user_id(username) {
         return Some(id.to_owned());
      }
      if let Some(id) = crate::cache::ids::get(username) {
         return Some(id);
      }
      if let Some(id) = twid_user_id(&self.cookies.snapshot(session_id).await) {
         crate::cache::ids::remember(username, &id);
         return Some(id);
      }
      if let Some(id) = self.own_user_id(session_id).await {
         crate::cache::ids::remember(username, &id);
         return Some(id);
      }
      match self.get_user(username).await {
         Ok(user) => Some(user.id),
         Err(err) => {
            tracing::debug!(
               session_id,
               user = %username,
               "could not resolve the numeric id for search safety: {err}"
            );
            None
         },
      }
   }

   /// The signed-in account's own id, from the session's own credentials.
   async fn own_user_id(&self, session_id: i64) -> Option<String> {
      let session = self
         .sessions
         .acquire_id(session_id, endpoints::VERIFY_CREDENTIALS)
         .await
         .ok()?;
      let path = "/1.1/account/verify_credentials.json";
      let headers = self
         .settings_headers(&session, "GET", path, "application/x-www-form-urlencoded")
         .await
         .ok()?;
      let response = self
         .client
         .get_on(
            endpoints::VERIFY_CREDENTIALS_URL,
            &headers,
            Some(&self.egress_for(&session)),
         )
         .await
         .ok()?;
      let (bytes, _) = self
         .account_response(&session, endpoints::VERIFY_CREDENTIALS, response)
         .await
         .ok()?;

      #[derive(Deserialize)]
      struct OwnAccount {
         id_str: String,
      }
      let own: OwnAccount = serde_json::from_slice(&bytes).ok()?;
      (!own.id_str.is_empty() && own.id_str.bytes().all(|byte| byte.is_ascii_digit()))
         .then_some(own.id_str)
   }

   async fn post_account_settings(
      &self,
      session: &SessionLease,
      form: &'static str,
   ) -> Result<Bytes> {
      let path = "/1.1/account/settings.json";
      let mut headers = self
         .settings_headers(session, "POST", path, "application/x-www-form-urlencoded")
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
      let bytes = self
         .post_account_settings(session, DISPLAY_SENSITIVE_BODY)
         .await?;

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
   pub(super) async fn settings_headers(
      &self,
      session: &SessionLease,
      method: &str,
      api_path: &str,
      content_type: &'static str,
   ) -> Result<header::HeaderMap> {
      let (bearer, tid) = self.bearer_and_tid_for(method, api_path).await;
      // A cookie session's settings write needs a transaction id: X answers
      // one without it with a bodyless 404 (code 34), which reads exactly like
      // an account that X will not let write settings. Refuse the write here
      // instead, so a bootstrap that is still failing does not cost accounts
      // the hour that refusal is believed for.
      if self.tid_enabled && tid.is_none() {
         return Err(Error::NoTransactionId);
      }
      let ct0 = self
         .sessions
         .ct0(session.id)
         .unwrap_or_else(|| session.ct0.clone());
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
