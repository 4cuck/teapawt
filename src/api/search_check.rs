//! Verify unchecked search accounts independently of visitor traffic.

use std::time::Duration;

use tokio::time::{
   sleep,
   timeout,
};

use super::{
   ApiClient,
   client::search_answer_is_empty,
   endpoints,
   schema::SearchTimelineData,
};
use crate::error::{
   Error,
   Result,
};

const CHECK_GAP: Duration = Duration::from_secs(10);
const CHECK_EVERY: Duration = Duration::from_mins(30);
const CHECK_TIMEOUT: Duration = Duration::from_secs(25);
const RECOVER_GAP: Duration = Duration::from_secs(20);

#[expect(
   clippy::multiple_inherent_impl,
   reason = "the search check is split from GraphQL endpoint methods"
)]
impl ApiClient {
   pub fn spawn_search_check(&self) {
      let client = self.clone();
      tokio::spawn(async move {
         loop {
            client.check_search_accounts().await;
            client.recover_demoted_search_accounts().await;
            sleep(CHECK_EVERY).await;
         }
      });
   }

   async fn check_search_accounts(&self) {
      let api = endpoints::GRAPH_SEARCH_TIMELINE;
      let ids = self.sessions.search_audit_ids(api).await;
      if ids.is_empty() {
         return;
      }
      tracing::info!(count = ids.len(), "checking which accounts X lets search");
      let (mut allowed, mut demoted, mut unknown) = (0_usize, 0_usize, 0_usize);
      for id in ids {
         match timeout(CHECK_TIMEOUT, self.search_as(id)).await {
            Ok(Ok(true)) => {
               self.sessions.mark_verified(id, api).await;
               self.sessions.record_tweet_search_ok(id, true);
               allowed += 1;
            },
            Ok(Err(Error::SearchTimelineFailed)) if self.sessions.tweet_search_healthy() => {
               self
                  .sessions
                  .demote_to_view(
                     id,
                     Some(crate::stats::Restriction::SearchBanned),
                     "Search audit failed with Internal server error",
                  )
                  .await;
               demoted += 1;
            },
            _ => unknown += 1,
         }
         sleep(CHECK_GAP).await;
      }
      tracing::info!(allowed, demoted, unknown, "search check done");
   }

   async fn recover_demoted_search_accounts(&self) {
      let ids = self.sessions.demoted_search_candidates(8).await;
      if ids.is_empty() {
         return;
      }
      let mut reinstated = 0_usize;
      for id in ids {
         match timeout(CHECK_TIMEOUT, self.probe_search_capable(id)).await {
            Ok(Ok(true)) => {
               self.sessions.reinstate(id).await;
               self
                  .sessions
                  .mark_verified(id, endpoints::GRAPH_SEARCH_TIMELINE)
                  .await;
               self.sessions.record_tweet_search_ok(id, true);
               reinstated += 1;
            },
            Ok(Err(Error::SessionRejected(_))) => {
               self.sessions.mark_rejected(id).await;
               self.sessions.clear_demoted_from_search(id).await;
            },
            Ok(Err(Error::IpThrottled | Error::RateLimited)) => break,
            _ => {},
         }
         sleep(RECOVER_GAP).await;
      }
      if reinstated != 0 {
         tracing::info!(reinstated, "recovered demoted search accounts");
      }
   }

   async fn search_as(&self, session_id: i64) -> Result<bool> {
      let api = endpoints::GRAPH_SEARCH_TIMELINE;
      let session = self.sessions.acquire_id(session_id, api).await?;
      self
         .graphql_request_inner::<SearchTimelineData>(
            &session,
            api,
            &endpoints::search_vars("news", None, "Latest"),
            endpoints::SEARCH_FEATURES,
            None,
         )
         .await
         .map(|(_, bytes)| !search_answer_is_empty(&bytes))
   }
}
