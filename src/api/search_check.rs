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
use crate::error::Result;

const CHECK_GAP: Duration = Duration::from_secs(10);
const CHECK_EVERY: Duration = Duration::from_mins(30);
const CHECK_TIMEOUT: Duration = Duration::from_secs(25);

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
      let (mut allowed, mut unknown) = (0_usize, 0_usize);
      for id in ids {
         match timeout(CHECK_TIMEOUT, self.search_as(id)).await {
            Ok(Ok(true)) => {
               self.sessions.mark_verified(id, api).await;
               allowed += 1;
            },
            _ => unknown += 1,
         }
         sleep(CHECK_GAP).await;
      }
      tracing::info!(allowed, unknown, "search check done");
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
