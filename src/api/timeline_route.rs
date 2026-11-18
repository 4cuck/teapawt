//! Which call served each first page of a profile timeline, reported every
//! minute.
//!
//! The first page of a profile or its RSS feed goes to `UserTweets` when an
//! account with quota for it has a free slot. When none has, the page comes
//! from `UserOriginalsTimeline` instead of queueing for seconds; failing
//! that, from the replies timeline; failing both, it waits for `UserTweets`.
//! Any page off the first route means the pool is short of `UserTweets`
//! quota, and the report says how many accounts the demand needs.

use std::{
   fmt::Write as _,
   sync::atomic::{
      AtomicU64,
      Ordering,
   },
};

/// Calls one account may make to `UserTweets` per window, as X reports it.
const USER_TWEETS_PER_WINDOW: u64 = 50;
const WINDOW_MINUTES: u64 = 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Route {
   /// `UserTweets`, on an account that was free.
   Primary,
   /// `UserTweets` ran on a free account and answered empty, then a second
   /// route confirmed it — not a pool shortage.
   EmptyVerified,
   /// `UserOriginalsTimeline`: original posts, no reposts.
   Originals,
   /// The replies timeline with replies removed.
   Replies,
   /// Both fallbacks failed, so it queued for `UserTweets`.
   Waited,
}

#[derive(Default)]
pub struct TimelineRoutes {
   primary:        AtomicU64,
   empty_verified: AtomicU64,
   originals:      AtomicU64,
   replies:        AtomicU64,
   waited:         AtomicU64,
}

pub struct Report {
   /// Some page left `UserTweets`, so the pool is short.
   pub fallback_needed: bool,
   /// First pages served off `UserTweets` this minute.
   pub off_primary:     u64,
   /// First pages served this minute.
   pub total:           u64,
   pub line:            String,
}

impl TimelineRoutes {
   pub fn count(&self, route: Route) {
      let counter = match route {
         Route::Primary => &self.primary,
         Route::EmptyVerified => &self.empty_verified,
         Route::Originals => &self.originals,
         Route::Replies => &self.replies,
         Route::Waited => &self.waited,
      };
      counter.fetch_add(1, Ordering::Relaxed);
   }

   /// The counts since the last report, which are then reset. `None` when no
   /// first page was served. `accounts` is the number of eligible view
   /// accounts in the pool.
   pub fn report(&self, accounts: usize) -> Option<Report> {
      let primary = self.primary.swap(0, Ordering::Relaxed);
      let empty_verified = self.empty_verified.swap(0, Ordering::Relaxed);
      let originals = self.originals.swap(0, Ordering::Relaxed);
      let replies = self.replies.swap(0, Ordering::Relaxed);
      let waited = self.waited.swap(0, Ordering::Relaxed);
      let total = primary + empty_verified + originals + replies + waited;
      if total == 0 {
         return None;
      }
      let off_primary = originals + replies + waited;
      let mut line = format!("{total} first pages: {primary} on UserTweets");
      if empty_verified > 0 {
         let _ = write!(line, ", {empty_verified} empty-profile checks");
      }
      let _ = write!(
         line,
         ", {originals} on UserOriginalsTimeline, {replies} from replies, {waited} waited for \
          UserTweets"
      );
      if off_primary > 0 {
         let share = off_primary * 100 / total;
         let needed = (total * WINDOW_MINUTES).div_ceil(USER_TWEETS_PER_WINDOW);
         let _ = write!(
            line,
            "; FALLBACK REQUIRED for {share}% because every account with UserTweets quota was \
             busy or spent. Serving all of them on UserTweets at this rate needs about {needed} \
             view accounts; {accounts} eligible"
         );
      }
      Some(Report {
         fallback_needed: off_primary > 0,
         off_primary,
         total,
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
   fn empty_profile_verification_counts_toward_total_without_fallback_pressure() {
      let routes = TimelineRoutes::default();
      for _ in 0..97 {
         routes.count(Route::Primary);
      }
      for _ in 0..3 {
         routes.count(Route::EmptyVerified);
      }
      let report = routes.report(100).unwrap();
      assert!(!report.fallback_needed);
      assert_eq!(report.total, 100);
      assert_eq!(report.off_primary, 0);
      assert_eq!(
         report.line,
         "100 first pages: 97 on UserTweets, 3 empty-profile checks, 0 on UserOriginalsTimeline, \
          0 from replies, 0 waited for UserTweets"
      );
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
      assert_eq!(report.total, 5_002);
      assert_eq!(report.off_primary, 1_002);
      assert!(
         report
            .line
            .starts_with("5002 first pages: 4000 on UserTweets, 1000 on UserOriginalsTimeline")
      );
      assert!(report.line.contains(
         "; FALLBACK REQUIRED for 20% because every account with UserTweets quota was busy or \
          spent. Serving all of them on UserTweets at this rate needs about 1501 view accounts; \
          1602 eligible"
      ));
      assert!(routes.report(1_602).is_none());
   }
}
