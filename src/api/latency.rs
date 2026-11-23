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
   collections::{
      BTreeMap,
      HashMap,
   },
   fmt::Write as _,
   sync::Mutex,
   time::{
      Duration,
      Instant,
   },
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
/// Minimum hedge delay as a fraction of the median (3/2 = 1.5x). A call that
/// is merely on the slow side of normal is slow because of what was asked for,
/// and a second account takes just as long.
const HEDGE_MEDIAN_NUM: u32 = 3;
const HEDGE_MEDIAN_DEN: u32 = 2;
/// Never sooner than this, so a fast operation is not copied on jitter.
const HEDGE_FLOOR: Duration = Duration::from_millis(150);
/// Out before the HTTP layer abandons the first connection at 1.5 s.
const HEDGE_CAP: Duration = Duration::from_millis(1200);
/// An operation with no history yet.
const HEDGE_DEFAULT: Duration = Duration::from_millis(800);
/// Shortest per-operation header deadline for fast endpoints.
const HEADER_DEADLINE_FLOOR: Duration = Duration::from_millis(600);
/// Default and upper bound for per-operation header deadlines.
const HEADER_DEADLINE_CAP: Duration = Duration::from_millis(1500);
/// Multiplier on an operation's median answer time for its header deadline.
///
/// Six, because an operation's slow answers do not shrink with its median.
/// `TweetDetail` asked for by likes answers in a median 215 ms, but a long
/// thread still takes X most of a second. At four medians (860 ms), 230 of
/// those a minute were taken for stalled and sent again through the
/// account's failover exit while X was still writing the first answer,
/// which nearly every time arrived anyway. Asked for by relevance, with a
/// 310 ms median and a 1,240 ms deadline, it was about 60 a minute. Six
/// puts the deadline back there, at 1,290 ms.
const HEADER_DEADLINE_MEDIANS: u64 = 6;
/// Minimum recent hedges before low-rescue suppression activates.
const HEDGE_SUPPRESS_MIN: u32 = 32;
/// Rolling hedge count at which recent counters halve so suppression adapts
/// continuously to changing upstream behavior.
const HEDGE_DECAY_AT: u32 = 64;
/// Denominator for the minimum rescue rate (1/20 = 5%). Operations below this
/// rescue rate after [`HEDGE_SUPPRESS_MIN`] hedges stop sending copies except
/// on periodic probe turns.
const HEDGE_MIN_RESCUE_DEN: u32 = 20;
/// While hedging is suppressed, allow one probe copy every this many
/// `hedge_delay` checks so recovery is detected automatically.
const HEDGE_PROBE_EVERY: u32 = 64;
/// Answers kept per exit.
const EXIT_WINDOW: usize = 128;
/// Answers an exit needs inside [`EXIT_FRESH`] before it is judged.
const EXIT_MIN_SAMPLES: usize = 40;
/// Only answers this recent judge an exit.
const EXIT_FRESH: Duration = Duration::from_mins(5);
/// How long an already-flagged slow exit stays deprioritized when reduced
/// traffic leaves it with too few fresh samples to judge.
const SLOW_EXIT_HOLD: Duration = Duration::from_mins(10);
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
   ring:            Vec<u32>,
   next:            usize,
   delay:           Option<Duration>,
   median:          Option<u32>,
   since_recompute: u32,
   /// Since the last report.
   answered:        u64,
   hedged:          u64,
   rescued:         u64,
   /// Rolling hedge outcomes, decayed independently of minute reports.
   recent_hedged:   u32,
   recent_rescued:  u32,
   probe_counter:   u32,
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
         let median_floor = median.saturating_mul(HEDGE_MEDIAN_NUM) / HEDGE_MEDIAN_DEN;
         let slow = percentile(&self.ring, HEDGE_PERCENTILE).max(median_floor);
         self.median = Some(median);
         self.delay = Some(Duration::from_millis(u64::from(slow)).clamp(HEDGE_FLOOR, HEDGE_CAP));
      }
   }

   const fn is_suppressed(&self) -> bool {
      self.recent_hedged >= HEDGE_SUPPRESS_MIN
         && self.recent_rescued.saturating_mul(HEDGE_MIN_RESCUE_DEN) < self.recent_hedged
   }

   fn record_hedge(&mut self) {
      self.hedged += 1;
      if self.recent_hedged >= HEDGE_DECAY_AT {
         self.recent_hedged /= 2;
         self.recent_rescued /= 2;
      }
      self.recent_hedged += 1;
   }

   fn record_rescue(&mut self) {
      self.rescued += 1;
      self.recent_rescued = self
         .recent_rescued
         .saturating_add(1)
         .min(self.recent_hedged);
   }

   fn next_hedge_delay(&mut self) -> Option<Duration> {
      let delay = self.delay.unwrap_or(HEDGE_DEFAULT);
      if !self.is_suppressed() {
         self.probe_counter = 0;
         return Some(delay);
      }
      self.probe_counter = self.probe_counter.wrapping_add(1);
      if self.probe_counter >= HEDGE_PROBE_EVERY {
         self.probe_counter = 0;
         return Some(delay);
      }
      None
   }
}

/// One exit's recent answers, as milliseconds past their operation's median.
#[derive(Default)]
struct Exit {
   ring:        Vec<(Instant, i32)>,
   next:        usize,
   slow:        bool,
   slow_since:  Option<Instant>,
   last_trails: i32,
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

   /// The median of the answers inside [`EXIT_FRESH`], once there are at least
   /// `min_samples`.
   fn judged_at(&self, now: Instant, min_samples: usize) -> Option<i32> {
      let mut fresh = self
         .ring
         .iter()
         .filter(|&&(at, _)| now.saturating_duration_since(at) < EXIT_FRESH)
         .map(|&(_, behind)| behind)
         .collect::<Vec<_>>();
      if fresh.len() < min_samples {
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
   exits:      Mutex<BTreeMap<u16, Exit>>,
}

impl Latency {
   /// A call to `endpoint` came back with data after `took`, through the
   /// proxy exit on port `exit` when there is one.
   pub fn record(&self, endpoint: &str, took: Duration, exit: Option<u16>) {
      self.record_at(endpoint, took, exit, Instant::now());
   }

   fn record_at(&self, endpoint: &str, took: Duration, exit: Option<u16>, at: Instant) {
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
         exits.entry(port).or_default().push(at, behind);
      }
   }

   /// Exits whose answers trail the rest of the fleet, each with how many
   /// milliseconds its median answer trails the fleet's, slowest first.
   pub fn slow_exits(&self) -> Vec<(u16, i32)> {
      self.slow_exits_at(Instant::now())
   }

   fn slow_exits_at(&self, now: Instant) -> Vec<(u16, i32)> {
      let Ok(mut exits) = self.exits.lock() else {
         return Vec::new();
      };
      let mut judged = Vec::new();
      let mut held = Vec::new();
      for (&port, exit) in exits.iter() {
         let min_samples = if exit.slow {
            EXIT_MIN_SAMPLES / 2
         } else {
            EXIT_MIN_SAMPLES
         };
         if let Some(median) = exit.judged_at(now, min_samples) {
            judged.push((port, median, exit.slow));
         } else if exit.slow
            && exit
               .slow_since
               .is_some_and(|since| now.saturating_duration_since(since) < SLOW_EXIT_HOLD)
         {
            held.push((port, exit.last_trails));
         }
      }
      let mut slow = Vec::new();
      if judged.len() >= MIN_JUDGED_EXITS {
         let mut medians = judged
            .iter()
            .map(|&(_, median, _)| median)
            .collect::<Vec<_>>();
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
      }
      let total_considered = judged.len() + held.len();
      slow.extend(held.iter().copied());
      slow.sort_by_key(|&(_, trails)| Reverse(trails));
      slow.truncate(total_considered / SLOW_EXIT_SHARE);
      for (&port, exit) in exits.iter_mut() {
         if let Some(&(_, trails)) = slow.iter().find(|&&(slow_port, _)| slow_port == port) {
            let was_held = held.iter().any(|&(held_port, _)| held_port == port);
            exit.slow = true;
            exit.last_trails = trails;
            if !was_held {
               exit.slow_since = Some(now);
            }
         } else {
            exit.slow = false;
            exit.slow_since = None;
            exit.last_trails = 0;
         }
      }
      drop(exits);
      slow
   }

   /// How long a call to `endpoint` may go unanswered before a second copy,
   /// or `None` when hedging is suppressed for this operation.
   pub fn hedge_delay(&self, endpoint: &str) -> Option<Duration> {
      let Ok(mut operations) = self.operations.lock() else {
         return Some(HEDGE_DEFAULT);
      };
      let delay = operations
         .get_mut(operation_name(endpoint))
         .map_or(Some(HEDGE_DEFAULT), Operation::next_hedge_delay);
      drop(operations);
      delay
   }

   /// Header deadline for `endpoint`, set to six times its recent median and
   /// clamped between 600 ms and 1500 ms.
   #[must_use]
   pub fn header_deadline(&self, endpoint: &str) -> Duration {
      let Ok(operations) = self.operations.lock() else {
         return HEADER_DEADLINE_CAP;
      };
      let median = operations
         .get(operation_name(endpoint))
         .and_then(|operation| operation.median);
      drop(operations);
      median.map_or(HEADER_DEADLINE_CAP, |millis| {
         Duration::from_millis(u64::from(millis).saturating_mul(HEADER_DEADLINE_MEDIANS))
            .clamp(HEADER_DEADLINE_FLOOR, HEADER_DEADLINE_CAP)
      })
   }

   /// A second copy went out.
   pub fn hedged(&self, endpoint: &str) {
      self.with(endpoint, Operation::record_hedge);
   }

   /// The second copy answered first.
   pub fn rescued(&self, endpoint: &str) {
      self.with(endpoint, Operation::record_rescue);
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
            let suppressed = if operation.is_suppressed() {
               " (suppressed)"
            } else {
               ""
            };
            let _ = write!(
               line,
               "{name}: {} answered, p50 {} ms, p90 {} ms, p99 {} ms, hedge at {} ms{suppressed}, \
                {} hedged, {} rescued",
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
      assert_eq!(latency.hedge_delay("abc/UserTweets"), Some(HEDGE_DEFAULT));
      for millis in 1..=100_u64 {
         latency.record("abc/UserTweets", Duration::from_millis(millis * 10), None);
      }
      // Recomputed every few answers, so it can trail the newest ones. The
      // 95th percentile (about 910 ms) exceeds 1.5x the median (about 720 ms).
      let delay = latency.hedge_delay("other-id/UserTweets").unwrap();
      assert!(
         (Duration::from_millis(900)..=Duration::from_millis(1000)).contains(&delay),
         "{delay:?}"
      );
   }

   #[test]
   fn a_wide_tail_is_hedged_at_its_ninety_fifth_percentile() {
      let latency = Latency::default();
      for index in 0..96_u64 {
         let millis = if index.is_multiple_of(4) { 1000 } else { 200 };
         latency.record("x/SearchTimeline", Duration::from_millis(millis), None);
      }
      assert_eq!(
         latency.hedge_delay("x/SearchTimeline"),
         Some(Duration::from_millis(1000))
      );
   }

   #[test]
   fn the_delay_stays_inside_its_bounds() {
      let latency = Latency::default();
      for _ in 0..64 {
         latency.record("x/UserByScreenName", Duration::from_millis(40), None);
         latency.record("x/UserTweets", Duration::from_millis(500), None);
         latency.record("x/SearchTimeline", Duration::from_secs(9), None);
      }
      assert_eq!(latency.hedge_delay("x/UserByScreenName"), Some(HEDGE_FLOOR));
      assert_eq!(
         latency.hedge_delay("x/UserTweets"),
         Some(Duration::from_millis(750))
      );
      assert_eq!(latency.hedge_delay("x/SearchTimeline"), Some(HEDGE_CAP));
   }

   #[test]
   fn low_rescue_rate_suppresses_hedging_and_probes_to_recover() {
      let latency = Latency::default();
      for _ in 0..40 {
         latency.record("x/TweetDetail", Duration::from_millis(400), None);
      }
      let expected = Duration::from_millis(600);
      assert_eq!(latency.hedge_delay("x/TweetDetail"), Some(expected));

      // 32 hedges with 1 rescue (~3.1% < 5%) activates suppression.
      for attempt in 0..HEDGE_SUPPRESS_MIN {
         latency.hedged("x/TweetDetail");
         if attempt == 0 {
            latency.rescued("x/TweetDetail");
         }
      }
      // Suppressed calls return None except once every HEDGE_PROBE_EVERY calls.
      for _ in 0..(HEDGE_PROBE_EVERY - 1) {
         assert_eq!(latency.hedge_delay("x/TweetDetail"), None);
      }
      assert_eq!(latency.hedge_delay("x/TweetDetail"), Some(expected));
      assert_eq!(latency.hedge_delay("x/TweetDetail"), None);

      // Reporting resets minute counters without clearing suppression.
      let report = latency.report();
      assert_eq!(report.len(), 1);
      assert!(report[0].contains("(suppressed)"), "{}", report[0]);
      assert_eq!(latency.hedge_delay("x/TweetDetail"), None);

      // Once probes lift the rescue rate back to >= 5% (2/33 >= 5%), hedging
      // re-enables.
      latency.hedged("x/TweetDetail");
      latency.rescued("x/TweetDetail");
      assert_eq!(latency.hedge_delay("x/TweetDetail"), Some(expected));
      assert_eq!(latency.hedge_delay("x/TweetDetail"), Some(expected));

      // Rolling window decay re-suppresses an operation if rescues stop again.
      for _ in 0..HEDGE_DECAY_AT {
         latency.hedged("x/TweetDetail");
      }
      assert_eq!(latency.hedge_delay("x/TweetDetail"), None);
   }

   #[test]
   fn header_deadline_clamps_between_floor_and_cap() {
      let latency = Latency::default();
      assert_eq!(
         latency.header_deadline("x/UserByScreenName"),
         Duration::from_millis(1500)
      );
      for _ in 0..40 {
         latency.record("x/UserByScreenName", Duration::from_millis(97), None);
         latency.record("x/TweetDetail", Duration::from_millis(215), None);
         latency.record("x/UserTweets", Duration::from_millis(500), None);
      }
      assert_eq!(
         latency.header_deadline("x/UserByScreenName"),
         Duration::from_millis(600)
      );
      // Six medians: clear of the long threads that take X 800 to 900 ms.
      assert_eq!(
         latency.header_deadline("x/TweetDetail"),
         Duration::from_millis(1290)
      );
      assert_eq!(
         latency.header_deadline("x/UserTweets"),
         Duration::from_millis(1500)
      );
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
      assert!(
         first[0].starts_with("TweetDetail: 40 answered"),
         "{}",
         first[0]
      );
      assert!(first[0].ends_with("1 hedged, 1 rescued"), "{}", first[0]);
      assert!(latency.report().is_empty());
   }

   /// Ten exits answering `UserTweets` around 400 ms, plus `extra` ms on
   /// the ports listed in it.
   fn answer_through_exits(latency: &Latency, rounds: u64, extra: &[(u16, u64)]) {
      answer_through_exits_at(latency, rounds, extra, Instant::now());
   }

   fn answer_through_exits_at(latency: &Latency, rounds: u64, extra: &[(u16, u64)], at: Instant) {
      for round in 0..rounds {
         for port in 1081..=1090_u16 {
            let base = 380 + (round % 5) * 10;
            let added = extra
               .iter()
               .find(|&&(slow, _)| slow == port)
               .map_or(0, |&(_, millis)| millis);
            latency.record_at(
               "x/UserTweets",
               Duration::from_millis(base + added),
               Some(port),
               at,
            );
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
   fn slow_exit_holds_without_flapping_until_recovered_or_hold_expires() {
      let latency = Latency::default();
      let start = Instant::now();
      answer_through_exits_at(&latency, 80, &[(1087, 150)], start);
      let initial = latency.slow_exits_at(start);
      assert_eq!(initial.len(), 1, "{initial:?}");
      assert_eq!(initial[0].0, 1087);

      // Six minutes later (> EXIT_FRESH, < SLOW_EXIT_HOLD), the deprioritized
      // exit has no fresh samples while the rest of the fleet keeps answering.
      // It stays flagged rather than flapping straight back into rotation.
      let mid = start + Duration::from_mins(6);
      for round in 0..80_u64 {
         for port in 1081..=1090_u16 {
            if port == 1087 {
               continue;
            }
            let base = 380 + (round % 5) * 10;
            latency.record_at("x/UserTweets", Duration::from_millis(base), Some(port), mid);
         }
      }
      let held = latency.slow_exits_at(mid);
      assert_eq!(held, initial, "slow exit must stay held during hold window");

      // Once SLOW_EXIT_HOLD elapses without fresh samples, it clears so it can
      // be re-measured.
      let expired = start + SLOW_EXIT_HOLD + Duration::from_secs(1);
      for round in 0..80_u64 {
         for port in 1081..=1090_u16 {
            if port == 1087 {
               continue;
            }
            let base = 380 + (round % 5) * 10;
            latency.record_at(
               "x/UserTweets",
               Duration::from_millis(base),
               Some(port),
               expired,
            );
         }
      }
      assert!(latency.slow_exits_at(expired).is_empty());

      // Re-flag and verify early recovery once it gets >= EXIT_MIN_SAMPLES / 2
      // fast fresh answers before the hold expires.
      let reflagged_at = expired + Duration::from_mins(1);
      answer_through_exits_at(&latency, 80, &[(1087, 150)], reflagged_at);
      assert_eq!(latency.slow_exits_at(reflagged_at).len(), 1);
      let recovered_at = reflagged_at + Duration::from_mins(6);
      for round in 0..80_u64 {
         for port in 1081..=1090_u16 {
            if port == 1087 && round >= (EXIT_MIN_SAMPLES / 2) as u64 {
               continue;
            }
            let base = 380 + (round % 5) * 10;
            latency.record_at(
               "x/UserTweets",
               Duration::from_millis(base),
               Some(port),
               recovered_at,
            );
         }
      }
      assert!(latency.slow_exits_at(recovered_at).is_empty());
   }

   #[test]
   fn jitter_and_a_fleet_wide_slowdown_send_no_exit_last() {
      let latency = Latency::default();
      answer_through_exits(&latency, 80, &[(1082, 15), (1085, 25)]);
      assert!(latency.slow_exits().is_empty());

      let everyone = (1081..=1090_u16)
         .map(|port| (port, 300))
         .collect::<Vec<_>>();
      answer_through_exits(&latency, EXIT_WINDOW as u64, &everyone);
      assert!(latency.slow_exits().is_empty());
   }

   #[test]
   fn at_most_a_third_of_the_exits_go_last() {
      let latency = Latency::default();
      let half = (1081..=1085_u16)
         .map(|port| (port, 200))
         .collect::<Vec<_>>();
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
