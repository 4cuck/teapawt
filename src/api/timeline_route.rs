//! Which call served each first page of a profile timeline, reported every
//! minute.
//!
//! The first page of a profile or its RSS feed goes to `UserTweets` when an
//! account with quota for it has a free slot. When none has, the page comes
//! from `UserOriginalsTimeline` instead of queueing for seconds; failing
//! that, from the replies timeline; failing both, it waits for `UserTweets`.
//! Any page off the first route means the pool is short of `UserTweets`
//! quota, and the report says how many accounts the demand needs.

use std::sync::atomic::{AtomicU64, Ordering};

/// Calls one account may make to `UserTweets` per window, as X reports it.
const USER_TWEETS_PER_WINDOW: u64 = 50;
const WINDOW_MINUTES: u64 = 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Route {
   /// `UserTweets`, on an account that was free.
   Primary,
   /// `UserOriginalsTimeline`: original posts, no reposts.
   Originals,
   /// The replies timeline with replies removed.
   Replies,
   /// Both fallbacks failed, so it queued for `UserTweets`.
   Waited,
}

#[derive(Default)]
pub struct TimelineRoutes {
   primary:   AtomicU64,
   originals: AtomicU64,
   replies:   AtomicU64,
   waited:    AtomicU64,
}

pub struct Report {
   /// Some page left `UserTweets`, so the pool is short.
   pub fallback_needed: bool,
   pub line:            String,
}

impl TimelineRoutes {
   pub fn count(&self, route: Route) {
      let counter = match route {
         Route::Primary => &self.primary,
         Route::Originals => &self.originals,
         Route::Replies => &self.replies,
         Route::Waited => &self.waited,
      };
      counter.fetch_add(1, Ordering::Relaxed);
   }

   /// The counts since the last report, which are then reset. `None` when no
   /// first page was served. `accounts` is the size of the pool.
   pub fn report(&self, accounts: usize) -> Option<Report> {
      let primary = self.primary.swap(0, Ordering::Relaxed);
      let originals = self.originals.swap(0, Ordering::Relaxed);
      let replies = self.replies.swap(0, Ordering::Relaxed);
      let waited = self.waited.swap(0, Ordering::Relaxed);
      let total = primary + originals + replies + waited;
      if total == 0 {
         return None;
      }
      let off_primary = originals + replies + waited;
      let mut line = format!(
         "{total} first pages: {primary} on UserTweets, {originals} on UserOriginalsTimeline, \
          {replies} from replies, {waited} waited for UserTweets"
      );
      if off_primary > 0 {
         let share = off_primary * 100 / total;
         let needed = (total * WINDOW_MINUTES).div_ceil(USER_TWEETS_PER_WINDOW);
         line.push_str(&format!(
            "; FALLBACK REQUIRED for {share}% because every account with UserTweets quota was \
             busy or spent. Serving all of them on UserTweets at this rate needs about {needed} \
             accounts; {accounts} are loaded"
         ));
      }
      Some(Report {
         fallback_needed: off_primary > 0,
         line,
      })
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn a_quiet_minute_reports_nothing() {
      assert!(TimelineRoutes::default().report(100).is_none());
   }

   #[test]
   fn all_on_user_tweets_is_not_a_fallback() {
      let routes = TimelineRoutes::default();
      for _ in 0..10 {
         routes.count(Route::Primary);
      }
      let report = routes.report(100).unwrap();
      assert!(!report.fallback_needed);
      assert!(!report.line.contains("FALLBACK"));
   }

   #[test]
   fn a_fallback_says_how_short_the_pool_is_and_resets() {
      let routes = TimelineRoutes::default();
      for _ in 0..4_000 {
         routes.count(Route::Primary);
      }
      for _ in 0..1_000 {
         routes.count(Route::Originals);
      }
      routes.count(Route::Replies);
      routes.count(Route::Waited);
      let report = routes.report(1_602).unwrap();
      assert!(report.fallback_needed);
      assert!(report.line.starts_with("5002 first pages: 4000 on UserTweets, 1000 on"));
      assert!(report.line.contains("FALLBACK REQUIRED for 20%"));
      // 5002 a minute is 75,030 per 15 minutes, at 50 per account.
      assert!(report.line.contains("about 1501 accounts; 1602 are loaded"));
      assert!(routes.report(1_602).is_none());
   }
}