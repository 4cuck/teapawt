//! How long X takes to answer each operation, and when a call that has not
//! answered yet is worth a second copy on another account.
//!
//! A copy goes out once the first has run past most of its operation's
//! recent answers, so about one call in ten carries one. A path X answers
//! slowly then costs the reader that much more, not the 1.5 s header deadline
//! plus a second wait on a fresh connection to the same exit.

use std::{cmp::Reverse, collections::HashMap, fmt::Write as _, sync::Mutex, time::Duration};

/// Recent answers kept per operation. Enough for a steady 90th percentile,
/// few enough that the window follows X when it slows down.
const WINDOW: usize = 256;
/// Below this many answers the default delay is used.
const MIN_SAMPLES: usize = 32;
/// The delay is recomputed after this many new answers.
const RECOMPUTE_EVERY: u32 = 16;
/// Percent of recent answers that come back before a second copy is sent.
const HEDGE_PERCENTILE: usize = 95;
/// And never before this many medians. A call that is merely on the slow
/// side of normal is slow because of what was asked for, and a second
/// account takes just as long: at the 90th percentile, under 2% of
/// `TweetDetail` and `UserTweets` copies answered first.
const HEDGE_MEDIANS: u32 = 2;
/// Never sooner than this, so a fast operation is not copied on jitter.
const HEDGE_FLOOR: Duration = Duration::from_millis(250);
/// Out before the HTTP layer abandons the first connection at 1.5 s.
const HEDGE_CAP: Duration = Duration::from_millis(1200);
/// An operation with no history yet.
const HEDGE_DEFAULT: Duration = Duration::from_millis(800);

#[derive(Default)]
struct Operation {
   /// Milliseconds per answered call, oldest overwritten first.
   ring: Vec<u32>,
   next: usize,
   delay: Option<Duration>,
   since_recompute: u32,
   /// Since the last report.
   answered: u64,
   hedged: u64,
   rescued: u64,
}

impl Operation {
   fn push(&mut self, millis: u32) {
      if self.ring.len() < WINDOW {
         self.ring.push(millis);
      } else {
         self.ring[self.next] = millis;
      }
      self.next = (self.next + 1) % WINDOW;
      self.answered += 1;
      self.since_recompute += 1;
      if self.ring.len() >= MIN_SAMPLES
         && (self.delay.is_none() || self.since_recompute >= RECOMPUTE_EVERY)
      {
         self.since_recompute = 0;
         let slow = percentile(&self.ring, HEDGE_PERCENTILE)
            .max(percentile(&self.ring, 50).saturating_mul(HEDGE_MEDIANS));
         self.delay =
            Some(Duration::from_millis(u64::from(slow)).clamp(HEDGE_FLOOR, HEDGE_CAP));
      }
   }
}

/// The value `percent` of `samples` fall at or below.
fn percentile(samples: &[u32], percent: usize) -> u32 {
   if samples.is_empty() {
      return 0;
   }
   let mut sorted = samples.to_vec();
   let rank = (sorted.len() - 1) * percent.min(100) / 100;
   let (_, value, _) = sorted.select_nth_unstable(rank);
   *value
}

/// Operation name without the query id, so a new id X deploys keeps the
/// same history.
fn operation_name(endpoint: &str) -> &str {
   endpoint.rsplit_once('/').map_or(endpoint, |(_, name)| name)
}

#[derive(Default)]
pub struct Latency {
   operations: Mutex<HashMap<String, Operation>>,
}

impl Latency {
   /// A call to `endpoint` came back with data after `took`.
   pub fn record(&self, endpoint: &str, took: Duration) {
      let millis = u32::try_from(took.as_millis()).unwrap_or(u32::MAX);
      self.with(endpoint, |operation| operation.push(millis));
   }

   /// How long a call to `endpoint` may go unanswered before a second copy.
   pub fn hedge_delay(&self, endpoint: &str) -> Duration {
      let Ok(operations) = self.operations.lock() else {
         return HEDGE_DEFAULT;
      };
      operations
         .get(operation_name(endpoint))
         .and_then(|operation| operation.delay)
         .unwrap_or(HEDGE_DEFAULT)
   }

   /// A second copy went out.
   pub fn hedged(&self, endpoint: &str) {
      self.with(endpoint, |operation| operation.hedged += 1);
   }

   /// The second copy answered first.
   pub fn rescued(&self, endpoint: &str) {
      self.with(endpoint, |operation| operation.rescued += 1);
   }

   fn with(&self, endpoint: &str, update: impl FnOnce(&mut Operation)) {
      let Ok(mut operations) = self.operations.lock() else {
         return;
      };
      let name = operation_name(endpoint);
      if let Some(operation) = operations.get_mut(name) {
         update(operation);
      } else {
         let mut operation = Operation::default();
         update(&mut operation);
         operations.insert(name.to_owned(), operation);
      }
   }

   /// One line per operation answered since the last report, busiest first,
   /// and the counters behind it reset.
   pub fn report(&self) -> Vec<String> {
      let Ok(mut operations) = self.operations.lock() else {
         return Vec::new();
      };
      let mut busy = operations
         .iter_mut()
         .filter(|entry| entry.1.answered > 0 || entry.1.hedged > 0)
         .collect::<Vec<_>>();
      busy.sort_by_key(|entry| Reverse(entry.1.answered));
      let lines = busy
         .into_iter()
         .map(|(name, operation)| {
            let mut line = String::new();
            let _ = write!(
               line,
               "{name}: {} answered, p50 {} ms, p90 {} ms, p99 {} ms, hedge at {} ms, {} hedged, {} \
                rescued",
               operation.answered,
               percentile(&operation.ring, 50),
               percentile(&operation.ring, 90),
               percentile(&operation.ring, 99),
               operation.delay.unwrap_or(HEDGE_DEFAULT).as_millis(),
               operation.hedged,
               operation.rescued,
            );
            operation.answered = 0;
            operation.hedged = 0;
            operation.rescued = 0;
            line
         })
         .collect();
      drop(operations);
      lines
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn the_delay_follows_the_slow_tail() {
      let latency = Latency::default();
      assert_eq!(latency.hedge_delay("abc/UserTweets"), HEDGE_DEFAULT);
      for millis in 1..=100_u64 {
         latency.record("abc/UserTweets", Duration::from_millis(millis * 10));
      }
      // Recomputed every few answers, so it can trail the newest ones. Two
      // medians (about 960 ms) is later than the 95th percentile here.
      let delay = latency.hedge_delay("other-id/UserTweets");
      assert!(
         (Duration::from_millis(900)..=Duration::from_millis(1000)).contains(&delay),
         "{delay:?}"
      );
   }

   #[test]
   fn a_wide_tail_is_hedged_at_its_ninety_fifth_percentile() {
      let latency = Latency::default();
      for index in 0..96_u64 {
         let millis = if index % 4 == 0 { 1000 } else { 200 };
         latency.record("x/SearchTimeline", Duration::from_millis(millis));
      }
      assert_eq!(latency.hedge_delay("x/SearchTimeline"), Duration::from_millis(1000));
   }

   #[test]
   fn the_delay_stays_inside_its_bounds() {
      let latency = Latency::default();
      for _ in 0..64 {
         latency.record("x/UserByScreenName", Duration::from_millis(40));
         latency.record("x/SearchTimeline", Duration::from_secs(9));
      }
      assert_eq!(latency.hedge_delay("x/UserByScreenName"), HEDGE_FLOOR);
      assert_eq!(latency.hedge_delay("x/SearchTimeline"), HEDGE_CAP);
   }

   #[test]
   fn a_report_resets_its_counters() {
      let latency = Latency::default();
      for _ in 0..40 {
         latency.record("x/TweetDetail", Duration::from_millis(300));
      }
      latency.hedged("x/TweetDetail");
      latency.rescued("x/TweetDetail");
      let first = latency.report();
      assert_eq!(first.len(), 1);
      assert!(first[0].starts_with("TweetDetail: 40 answered"), "{}", first[0]);
      assert!(first[0].ends_with("1 hedged, 1 rescued"), "{}", first[0]);
      assert!(latency.report().is_empty());
   }
}
