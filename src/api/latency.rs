//! How long X takes to answer each operation, and when a call that has not
//! answered yet is worth a second copy on another account.
//!
//! A copy goes out once the first has run past most of its operation's
//! recent answers, so about one call in ten carries one. A path X answers
//! slowly then costs the reader that much more, not the 1.5 s header deadline
//! plus a second wait on a fresh connection to the same exit.
//!
//! Each answer is also held against the proxy exit that carried it, as the
//! milliseconds past its operation's median. An exit whose tunnel is long
//! pays that round trip on every call, and its accounts are picked last.

use std::{
   cmp::Reverse,
   collections::{BTreeMap, HashMap},
   fmt::Write as _,
   sync::Mutex,
   time::{Duration, Instant},
};

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
/// Answers kept per exit.
const EXIT_WINDOW: usize = 128;
/// Answers an exit needs inside [`EXIT_FRESH`] before it is judged.
const EXIT_MIN_SAMPLES: usize = 40;
/// Only answers this recent judge an exit. One whose accounts go last gets
/// few calls, so it falls back into normal rotation and is measured again.
const EXIT_FRESH: Duration = Duration::from_mins(5);
/// How far an exit's median answer may trail the fleet's before its accounts
/// go last. Through the quick exits x.com answers 40-60 ms after TLS; the
/// distant ones take 180-280 ms.
const SLOW_EXIT_MARGIN_MS: i32 = 40;
/// A slow exit rejoins normal rotation once it is this close to the fleet.
const SLOW_EXIT_RECOVER_MS: i32 = 20;
/// Fewer judged exits than this is no fleet to compare against.
const MIN_JUDGED_EXITS: usize = 4;
/// At most one judged exit in this many goes last at once.
const SLOW_EXIT_SHARE: usize = 3;

#[derive(Default)]
struct Operation {
   /// Milliseconds per answered call, oldest overwritten first.
   ring: Vec<u32>,
   next: usize,
   delay: Option<Duration>,
   median: Option<u32>,
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
         let median = percentile(&self.ring, 50);
         let slow =
            percentile(&self.ring, HEDGE_PERCENTILE).max(median.saturating_mul(HEDGE_MEDIANS));
         self.median = Some(median);
         self.delay =
            Some(Duration::from_millis(u64::from(slow)).clamp(HEDGE_FLOOR, HEDGE_CAP));
      }
   }
}

/// One exit's recent answers, as milliseconds past their operation's median.
#[derive(Default)]
struct Exit {
   ring: Vec<(Instant, i32)>,
   next: usize,
   slow: bool,
}

impl Exit {
   fn push(&mut self, at: Instant, behind: i32) {
      if self.ring.len() < EXIT_WINDOW {
         self.ring.push((at, behind));
      } else {
         self.ring[self.next] = (at, behind);
      }
      self.next = (self.next + 1) % EXIT_WINDOW;
   }

   /// The median of the answers inside [`EXIT_FRESH`], once there are enough.
   fn judged(&self, now: Instant) -> Option<i32> {
      let mut fresh = self
         .ring
         .iter()
         .filter(|&&(at, _)| now.saturating_duration_since(at) < EXIT_FRESH)
         .map(|&(_, behind)| behind)
         .collect::<Vec<_>>();
      if fresh.len() < EXIT_MIN_SAMPLES {
         return None;
      }
      let middle = fresh.len() / 2;
      Some(*fresh.select_nth_unstable(middle).1)
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
   exits: Mutex<BTreeMap<u16, Exit>>,
}

impl Latency {
   /// A call to `endpoint` came back with data after `took`, through the
   /// proxy exit on port `exit` when there is one.
   pub fn record(&self, endpoint: &str, took: Duration, exit: Option<u16>) {
      let millis = u32::try_from(took.as_millis()).unwrap_or(u32::MAX);
      let median = self
         .with(endpoint, |operation| {
            operation.push(millis);
            operation.median
         })
         .flatten();
      let (Some(port), Some(median)) = (exit, median) else {
         return;
      };
      let behind = i64::from(millis) - i64::from(median);
      let behind = i32::try_from(behind).unwrap_or(if behind > 0 { i32::MAX } else { i32::MIN });
      if let Ok(mut exits) = self.exits.lock() {
         exits.entry(port).or_default().push(Instant::now(), behind);
      }
   }

   /// Exits whose answers trail the rest of the fleet, each with how many
   /// milliseconds its median answer trails the fleet's, slowest first.
   pub fn slow_exits(&self) -> Vec<(u16, i32)> {
      let now = Instant::now();
      let Ok(mut exits) = self.exits.lock() else {
         return Vec::new();
      };
      let judged = exits
         .iter()
         .filter_map(|(&port, exit)| Some((port, exit.judged(now)?, exit.slow)))
         .collect::<Vec<_>>();
      let mut slow = Vec::new();
      if judged.len() >= MIN_JUDGED_EXITS {
         let mut medians = judged.iter().map(|&(_, median, _)| median).collect::<Vec<_>>();
         let middle = medians.len() / 2;
         let fleet = *medians.select_nth_unstable(middle).1;
         slow = judged
            .iter()
            .filter_map(|&(port, median, was_slow)| {
               let trails = median.saturating_sub(fleet);
               let margin = if was_slow {
                  SLOW_EXIT_RECOVER_MS
               } else {
                  SLOW_EXIT_MARGIN_MS
               };
               (trails > margin).then_some((port, trails))
            })
            .collect();
         slow.sort_by_key(|&(_, trails)| Reverse(trails));
         slow.truncate(judged.len() / SLOW_EXIT_SHARE);
      }
      for (&port, exit) in exits.iter_mut() {
         exit.slow = slow.iter().any(|&(slow_port, _)| slow_port == port);
      }
      drop(exits);
      slow
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

   fn with<R>(&self, endpoint: &str, update: impl FnOnce(&mut Operation) -> R) -> Option<R> {
      let mut operations = self.operations.lock().ok()?;
      let name = operation_name(endpoint);
      if !operations.contains_key(name) {
         operations.insert(name.to_owned(), Operation::default());
      }
      let result = operations.get_mut(name).map(update);
      drop(operations);
      result
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
         latency.record("abc/UserTweets", Duration::from_millis(millis * 10), None);
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
         latency.record("x/SearchTimeline", Duration::from_millis(millis), None);
      }
      assert_eq!(latency.hedge_delay("x/SearchTimeline"), Duration::from_millis(1000));
   }

   #[test]
   fn the_delay_stays_inside_its_bounds() {
      let latency = Latency::default();
      for _ in 0..64 {
         latency.record("x/UserByScreenName", Duration::from_millis(40), None);
         latency.record("x/SearchTimeline", Duration::from_secs(9), None);
      }
      assert_eq!(latency.hedge_delay("x/UserByScreenName"), HEDGE_FLOOR);
      assert_eq!(latency.hedge_delay("x/SearchTimeline"), HEDGE_CAP);
   }

   #[test]
   fn a_report_resets_its_counters() {
      let latency = Latency::default();
      for _ in 0..40 {
         latency.record("x/TweetDetail", Duration::from_millis(300), None);
      }
      latency.hedged("x/TweetDetail");
      latency.rescued("x/TweetDetail");
      let first = latency.report();
      assert_eq!(first.len(), 1);
      assert!(first[0].starts_with("TweetDetail: 40 answered"), "{}", first[0]);
      assert!(first[0].ends_with("1 hedged, 1 rescued"), "{}", first[0]);
      assert!(latency.report().is_empty());
   }

   /// Ten exits answering `UserTweets` around 400 ms, plus `extra` ms on
   /// the ports listed in it.
   fn answer_through_exits(latency: &Latency, rounds: u64, extra: &[(u16, u64)]) {
      for round in 0..rounds {
         for port in 1081..=1090_u16 {
            let base = 380 + (round % 5) * 10;
            let added = extra
               .iter()
               .find(|&&(slow, _)| slow == port)
               .map_or(0, |&(_, millis)| millis);
            latency.record("x/UserTweets", Duration::from_millis(base + added), Some(port));
         }
      }
   }

   #[test]
   fn an_exit_far_behind_the_fleet_goes_last_until_it_catches_up() {
      let latency = Latency::default();
      answer_through_exits(&latency, 80, &[(1087, 150)]);
      let slow = latency.slow_exits();
      assert_eq!(slow.len(), 1, "{slow:?}");
      assert_eq!(slow[0].0, 1087);
      assert!((120..=180).contains(&slow[0].1), "{slow:?}");

      answer_through_exits(&latency, EXIT_WINDOW as u64, &[]);
      assert!(latency.slow_exits().is_empty());
   }

   #[test]
   fn jitter_and_a_fleet_wide_slowdown_send_no_exit_last() {
      let latency = Latency::default();
      answer_through_exits(&latency, 80, &[(1082, 15), (1085, 25)]);
      assert!(latency.slow_exits().is_empty());

      let everyone = (1081..=1090_u16).map(|port| (port, 300)).collect::<Vec<_>>();
      answer_through_exits(&latency, EXIT_WINDOW as u64, &everyone);
      assert!(latency.slow_exits().is_empty());
   }

   #[test]
   fn at_most_a_third_of_the_exits_go_last() {
      let latency = Latency::default();
      let half = (1081..=1085_u16).map(|port| (port, 200)).collect::<Vec<_>>();
      answer_through_exits(&latency, 80, &half);
      assert!(latency.slow_exits().len() <= 10 / SLOW_EXIT_SHARE);
   }

   #[test]
   fn an_exit_with_too_few_answers_is_not_judged() {
      let latency = Latency::default();
      answer_through_exits(&latency, 80, &[]);
      for _ in 0..(EXIT_MIN_SAMPLES - 1) {
         latency.record("x/UserTweets", Duration::from_millis(900), Some(1099));
      }
      assert!(latency.slow_exits().is_empty());
   }
}
