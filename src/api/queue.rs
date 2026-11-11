//! How long calls wait for an account before they reach X.
//!
//! [`Latency`](super::latency::Latency) times X's answer from the moment the
//! request leaves. Time spent before that, queued on the limits lock or for
//! an account with a free slot, never showed up there. A busy minute could
//! read p50 400 ms upstream while readers waited a second.

use std::{
   cmp::Reverse,
   collections::HashMap,
   fmt::Write as _,
   sync::Mutex,
   time::Duration,
};

/// Waits kept per operation between reports. A busy operation is sampled,
/// not stored whole.
const WINDOW: usize = 4096;
/// A pick slower than this counts as waiting. One uncontended pass over the
/// pool takes well under it.
const WAITED: Duration = Duration::from_millis(5);

/// What happened when a call asked the pool for an account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
   /// Leased after `waited` (lock plus any wait for a free slot).
   Leased,
   /// Every account that could serve it is out of quota.
   Spent,
   /// No account can serve it at all.
   Nobody,
   /// Every usable account was mid-call and the line was too long to join,
   /// or the call waited its limit; the visitor is told to try again.
   Shed,
}

/// An operation whose leases waited this often gets a pool breakdown in the
/// report, showing whether its accounts were busy, resting or spent.
const CROWDED_PERCENT: u64 = 5;

#[derive(Default)]
struct Operation {
   /// The endpoint as last asked for, query id included, so the pool can be
   /// read for the same key it is charged against.
   endpoint: String,
   leased: u64,
   waited: u64,
   spent: u64,
   nobody: u64,
   /// Turned away instead of waiting on a line that would not clear.
   shed: u64,
   /// Bare 429s from X that sat an account out of this operation.
   rested: u64,
   /// Milliseconds per lease since the last report, sampled once full.
   ring: Vec<u32>,
   seen: u64,
   longest: u32,
}

/// The minute's lines, and the endpoints that queued for an account often
/// enough to be worth a look at the pool.
pub struct Report {
   pub lines: Vec<String>,
   pub crowded: Vec<String>,
}

impl Operation {
   fn push(&mut self, took: Duration, outcome: Outcome) {
      match outcome {
         Outcome::Spent => self.spent += 1,
         Outcome::Nobody => self.nobody += 1,
         Outcome::Shed => self.shed += 1,
         Outcome::Leased => {
            self.leased += 1;
            if took >= WAITED {
               self.waited += 1;
            }
            let millis = u32::try_from(took.as_millis()).unwrap_or(u32::MAX);
            self.longest = self.longest.max(millis);
            self.seen += 1;
            if self.ring.len() < WINDOW {
               self.ring.push(millis);
            } else {
               // Reservoir sample: the window stays representative of the
               // whole minute, not its last few seconds.
               let slot = usize::try_from(fastrand_below(self.seen)).unwrap_or(usize::MAX);
               if let Some(kept) = self.ring.get_mut(slot) {
                  *kept = millis;
               }
            }
         },
      }
   }
}

/// A cheap uniform draw below `n`. Statistical sampling only.
fn fastrand_below(n: u64) -> u64 {
   use std::{
      hash::{BuildHasher as _, RandomState},
      sync::atomic::{AtomicU64, Ordering},
   };
   static COUNTER: AtomicU64 = AtomicU64::new(0);
   let tick = COUNTER.fetch_add(1, Ordering::Relaxed);
   RandomState::new().hash_one(tick) % n.max(1)
}

/// The value `percent` of `samples` fall at or below.
fn percentile(samples: &mut [u32], percent: usize) -> u32 {
   if samples.is_empty() {
      return 0;
   }
   let rank = (samples.len() - 1) * percent.min(100) / 100;
   *samples.select_nth_unstable(rank).1
}

/// Operation name without the query id.
fn operation_name(endpoint: &str) -> &str {
   endpoint.rsplit_once('/').map_or(endpoint, |(_, name)| name)
}

#[derive(Default)]
pub struct AccountQueue {
   operations: Mutex<HashMap<String, Operation>>,
}

impl AccountQueue {
   pub fn record(&self, endpoint: &str, took: Duration, outcome: Outcome) {
      self.with(endpoint, |operation| operation.push(took, outcome));
   }

   /// X refused an account this call with a bare 429 and it now rests.
   pub fn note_rest(&self, endpoint: &str) {
      self.with(endpoint, |operation| operation.rested += 1);
   }

   fn with(&self, endpoint: &str, update: impl FnOnce(&mut Operation)) {
      let Ok(mut operations) = self.operations.lock() else {
         return;
      };
      let name = operation_name(endpoint);
      let operation = if let Some(operation) = operations.get_mut(name) {
         operation
      } else {
         operations.entry(name.to_owned()).or_default()
      };
      if operation.endpoint != endpoint {
         endpoint.clone_into(&mut operation.endpoint);
      }
      update(operation);
   }

   /// One line per operation asked for since the last report, busiest
   /// first, and the counters behind it reset.
   pub fn report(&self) -> Report {
      let Ok(mut operations) = self.operations.lock() else {
         return Report {
            lines: Vec::new(),
            crowded: Vec::new(),
         };
      };
      let mut drained = operations
         .drain()
         .filter(|(_, op)| op.leased + op.spent + op.nobody + op.shed + op.rested > 0)
         .collect::<Vec<_>>();
      drop(operations);
      drained.sort_by_key(|(_, op)| Reverse(op.leased + op.spent + op.nobody + op.shed));
      let crowded = drained
         .iter()
         .filter(|(_, op)| {
            op.shed > 0 || op.waited * 100 >= op.leased.max(1) * CROWDED_PERCENT
         })
         .map(|(_, op)| op.endpoint.clone())
         .collect();
      let lines = drained
         .into_iter()
         .map(|(name, mut op)| {
            let mut line = String::new();
            let _ = write!(
               line,
               "{name}: {} leased, {} waited, p50 {} ms, p90 {} ms, p99 {} ms, max {} ms, {} \
                out of quota, {} no account, {} rested, {} shed",
               op.leased,
               op.waited,
               percentile(&mut op.ring, 50),
               percentile(&mut op.ring, 90),
               percentile(&mut op.ring, 99),
               op.longest,
               op.spent,
               op.nobody,
               op.rested,
               op.shed,
            );
            line
         })
         .collect();
      Report { lines, crowded }
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn reports_waits_and_refusals_per_operation_then_resets() {
      let queue = AccountQueue::default();
      for millis in [0, 1, 2, 300, 900] {
         queue.record("q1/UserTweets", Duration::from_millis(millis), Outcome::Leased);
      }
      queue.record("q2/UserTweets", Duration::ZERO, Outcome::Spent);
      queue.record("q1/TweetDetail", Duration::ZERO, Outcome::Nobody);
      queue.note_rest("q2/UserTweets");

      let report = queue.report();
      let lines = report.lines;
      assert_eq!(lines.len(), 2);
      assert_eq!(
         lines[0],
         "UserTweets: 5 leased, 2 waited, p50 2 ms, p90 300 ms, p99 300 ms, max 900 ms, 1 out \
          of quota, 0 no account, 1 rested, 0 shed"
      );
      assert!(lines[1].starts_with("TweetDetail: 0 leased"), "{}", lines[1]);
      // Two of five waited, and the pool is read under the newest query id.
      assert_eq!(report.crowded, ["q2/UserTweets"]);
      assert!(queue.report().lines.is_empty());
   }

   #[test]
   fn an_operation_that_rarely_waits_is_not_crowded() {
      let queue = AccountQueue::default();
      for _ in 0..100 {
         queue.record("q/UserMedia", Duration::ZERO, Outcome::Leased);
      }
      queue.record("q/UserMedia", Duration::from_millis(50), Outcome::Leased);
      assert!(queue.report().crowded.is_empty());
   }

   #[test]
   fn a_busy_minute_is_sampled_not_stored_whole() {
      let queue = AccountQueue::default();
      for _ in 0..(WINDOW * 3) {
         queue.record("x/UserMedia", Duration::from_millis(10), Outcome::Leased);
      }
      let operations = queue.operations.lock().unwrap();
      let op = &operations["UserMedia"];
      assert_eq!(op.ring.len(), WINDOW);
      assert_eq!(op.leased, (WINDOW * 3) as u64);
   }
}