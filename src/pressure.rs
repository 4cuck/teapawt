//! How far demand has outrun the account pool, and when that stops being a
//! busy minute and starts being a flood.
//!
//! Every profile page costs one call on a pool of X accounts. When a flood
//! spends that pool, the first page of a timeline has to leave `UserTweets`
//! for `UserOriginalsTimeline`, the replies timeline, or a wait, and
//! `api::timeline_route` counts how many did. That share is a direct reading
//! of the gap between demand and pool: a few percent is a busy minute, half
//! is something loading the site thousands of times a minute.
//!
//! `Pressure` watches the share reported each minute and holds a `Flooding`
//! verdict for `hold_minutes` after the last minute above `flood_percent`,
//! so a flood that pauses for a minute does not walk straight back in. It
//! only decides; `routes::anti_ddos` puts the check in front of the visitor
//! and `views::anti_ddos` explains it.

use std::sync::{
   Mutex,
   PoisonError,
};

use serde::Serialize;
use time::OffsetDateTime;

/// First pages in a minute before the share means anything. A dozen pages
/// that mostly fell back is a quiet minute with a hiccup, not a flood.
const MIN_SAMPLE: u64 = 30;

/// Half the pages falling back means the pool, not the per-account quota, is
/// the bottleneck. Nobody reading profiles reaches that.
pub const DEFAULT_PERCENT: u32 = 50;

/// Sustained fallback share, in percent, that raises the verdict once it holds
/// for [`DEFAULT_SUSTAINED_MINUTES`].
pub const DEFAULT_SUSTAINED_PERCENT: u32 = 3;

/// Consecutive minutes at or above [`DEFAULT_SUSTAINED_PERCENT`] before the
/// sustained trigger fires.
pub const DEFAULT_SUSTAINED_MINUTES: u32 = 5;

/// Long enough that a flood which pauses cannot resume unnoticed, short
/// enough that the check comes down on its own once it is over.
pub const DEFAULT_HOLD_MINUTES: i64 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
   Calm,
   /// Demand is outrunning the pool by more than `flood_percent`, or has stayed
   /// at or above `sustained_percent` for `sustained_minutes`.
   Flooding,
}

/// A verdict that changed, so the caller logs it once instead of every
/// minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
   /// The share crossed a trigger.
   Raised { share: u32, cause: Cause },
   /// The hold lapsed with the share back under the trigger.
   Lifted,
}

/// Which trigger put the verdict up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
   /// One minute ran at or above `flood_percent`.
   Spike,
   /// The share stayed at or above `sustained_percent` for
   /// `sustained_minutes`.
   Sustained,
}

/// Everything the admin dashboard shows about the verdict, read in one lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
   /// The flood verdict is up right now.
   pub flooding:          bool,
   /// What raised it. `None` while calm.
   pub cause:             Option<Cause>,
   /// Unix time the verdict went up. `None` while calm.
   pub since:             Option<i64>,
   /// Unix time the verdict lapses unless another flooded minute extends it.
   /// `None` while calm.
   pub until:             Option<i64>,
   /// Fallback share of the last judged minute, in percent.
   pub share:             u32,
   /// First pages counted in the last reported minute.
   pub sample:            u64,
   /// Unix time of the last reported minute. `None` before the first one.
   pub observed_at:       Option<i64>,
   /// Minutes in a row at or above `sustained_percent`.
   pub streak_minutes:    u32,
   /// The single-minute trigger, in percent.
   pub flood_percent:     u32,
   /// The sustained trigger, in percent. `0` means it is off.
   pub sustained_percent: u32,
   /// Minutes the sustained trigger needs.
   pub sustained_minutes: u32,
   /// Minutes the verdict is held after the last flooded minute.
   pub hold_minutes:      i64,
}

#[derive(Default)]
struct State {
   /// Last measured fallback share, kept for the page copy even after it
   /// falls back under the trigger.
   share:          u32,
   /// First pages counted in the last reported minute.
   sample:         u64,
   /// Unix time of the last reported minute.
   observed_at:    Option<i64>,
   /// Minutes in a row at or above `sustained_percent`.
   streak_minutes: u32,
   /// Which trigger raised the active verdict.
   cause:          Option<Cause>,
   /// Unix time the active verdict went up.
   since:          Option<i64>,
   /// Unix time the flood verdict lapses. `0` when none is up.
   until:          i64,
}

pub struct Pressure {
   /// Single-minute fallback share, in percent, that counts as a flood.
   percent:           u32,
   /// Sustained fallback share, in percent. `0` disables the sustained trigger.
   sustained_percent: u32,
   /// Consecutive minutes at or above `sustained_percent` needed to raise.
   sustained_minutes: u32,
   hold_minutes:      i64,
   state:             Mutex<State>,
}

impl Pressure {
   #[must_use]
   pub fn new(percent: u32, hold_minutes: i64) -> Self {
      Self::with_sustained(percent, 0, 0, hold_minutes)
   }

   #[must_use]
   pub fn with_sustained(
      percent: u32,
      sustained_percent: u32,
      sustained_minutes: u32,
      hold_minutes: i64,
   ) -> Self {
      Self {
         percent,
         sustained_percent,
         sustained_minutes: if sustained_percent > 0 {
            sustained_minutes.max(1)
         } else {
            sustained_minutes
         },
         hold_minutes: hold_minutes.max(1),
         state: Mutex::new(State::default()),
      }
   }

   /// Record one minute's report: `off_primary` first pages left
   /// `UserTweets` out of `total`. `None` when the minute says nothing new,
   /// or is too small to judge.
   pub fn observe(&self, off_primary: u64, total: u64, now: i64) -> Option<Change> {
      if total < MIN_SAMPLE {
         return None;
      }
      let share = u32::try_from(off_primary.saturating_mul(100) / total).unwrap_or(100);
      let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
      let was_up = state.until > now;
      state.share = share;
      state.sample = total;
      state.observed_at = Some(now);
      if self.sustained_percent > 0 && share >= self.sustained_percent {
         state.streak_minutes = state.streak_minutes.saturating_add(1);
      } else {
         state.streak_minutes = 0;
      }

      let triggered = if share >= self.percent {
         Some(Cause::Spike)
      } else if self.sustained_percent > 0 && state.streak_minutes >= self.sustained_minutes {
         Some(Cause::Sustained)
      } else {
         None
      };

      if let Some(cause) = triggered {
         state.until = now.saturating_add(self.hold_minutes * 60);
         if !was_up {
            state.since = Some(now);
            state.cause = Some(cause);
            return Some(Change::Raised { share, cause });
         }
         if cause == Cause::Spike {
            state.cause = Some(Cause::Spike);
         }
         return None;
      }

      // Under the trigger, but the verdict stands until it lapses.
      if was_up || state.until == 0 {
         return None;
      }
      state.until = 0;
      state.since = None;
      state.cause = None;
      Some(Change::Lifted)
   }

   #[must_use]
   pub fn level(&self, now: i64) -> Level {
      let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
      if state.until > now {
         Level::Flooding
      } else {
         Level::Calm
      }
   }

   /// The share to show while the gate is up for a flood, `None` when it is
   /// not. The gate calls this on every request, so it is one lock.
   #[must_use]
   pub fn flood_share(&self, now: i64) -> Option<u32> {
      let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
      (state.until > now).then_some(state.share)
   }

   /// The last measured fallback share, in percent.
   #[must_use]
   pub fn share(&self) -> u32 {
      self.state
         .lock()
         .unwrap_or_else(PoisonError::into_inner)
         .share
   }

   #[must_use]
   pub const fn hold_minutes(&self) -> i64 {
      self.hold_minutes
   }

   /// The verdict and the numbers behind it, for the admin dashboard.
   #[must_use]
   pub fn status(&self, now: i64) -> Status {
      let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
      let flooding = state.until > now;
      Status {
         flooding,
         cause: state.cause.filter(|_| flooding),
         since: state.since.filter(|_| flooding),
         until: flooding.then_some(state.until),
         share: state.share,
         sample: state.sample,
         observed_at: state.observed_at,
         streak_minutes: state.streak_minutes,
         flood_percent: self.percent,
         sustained_percent: self.sustained_percent,
         sustained_minutes: self.sustained_minutes,
         hold_minutes: self.hold_minutes,
      }
   }
}

#[must_use]
pub fn now() -> i64 {
   OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests {
   use super::*;

   fn pressure() -> Pressure {
      Pressure::new(50, 10)
   }

   #[test]
   fn a_small_sample_is_never_a_flood() {
      let pressure = pressure();
      assert_eq!(pressure.observe(9, 10, 1_000), None);
      assert_eq!(pressure.level(1_000), Level::Calm);
      assert_eq!(pressure.flood_share(1_000), None);
   }

   #[test]
   fn half_the_pages_falling_back_raises_the_verdict_once() {
      let pressure = pressure();
      assert_eq!(
         pressure.observe(100, 200, 1_000),
         Some(Change::Raised {
            share: 50,
            cause: Cause::Spike,
         })
      );
      assert_eq!(pressure.observe(120, 200, 1_060), None);
      assert_eq!(pressure.share(), 60);
      assert_eq!(pressure.level(1_060), Level::Flooding);
      assert_eq!(pressure.flood_share(1_060), Some(60));
   }

   #[test]
   fn a_busy_minute_under_the_trigger_does_not_raise_it() {
      let pressure = pressure();
      assert_eq!(pressure.observe(98, 200, 1_000), None);
      assert_eq!(pressure.share(), 49);
      assert_eq!(pressure.level(1_000), Level::Calm);
   }

   #[test]
   fn the_verdict_outlives_the_last_minute_above_the_trigger() {
      let pressure = pressure();
      pressure.observe(150, 200, 1_000);
      // Quiet, but held: a flood that pauses does not get straight back in.
      assert_eq!(pressure.observe(0, 200, 1_060), None);
      assert_eq!(pressure.level(1_060), Level::Flooding);
      // Lapsed, and the lift is reported once.
      assert_eq!(pressure.observe(0, 200, 1_600), Some(Change::Lifted));
      assert_eq!(pressure.level(1_600), Level::Calm);
      assert_eq!(pressure.observe(0, 200, 1_660), None);
   }

   #[test]
   fn a_second_flood_after_the_lift_raises_again() {
      let pressure = pressure();
      pressure.observe(100, 200, 1_000);
      pressure.observe(0, 200, 1_600);
      assert_eq!(
         pressure.observe(100, 200, 2_000),
         Some(Change::Raised {
            share: 50,
            cause: Cause::Spike,
         })
      );
   }

   #[test]
   fn sustained_fallback_streak_raises_and_extends_hold() {
      let pressure = Pressure::with_sustained(50, 3, 5, 10);
      for minute in 0..4_i64 {
         assert_eq!(pressure.observe(349, 9_240, 1_000 + minute * 60), None);
         assert_eq!(pressure.level(1_000 + minute * 60), Level::Calm);
      }
      assert_eq!(
         pressure.status(1_180),
         Status {
            flooding:          false,
            cause:             None,
            since:             None,
            until:             None,
            share:             3,
            sample:            9_240,
            observed_at:       Some(1_180),
            streak_minutes:    4,
            flood_percent:     50,
            sustained_percent: 3,
            sustained_minutes: 5,
            hold_minutes:      10,
         }
      );

      // 5th consecutive minute at >= 3% triggers Cause::Sustained.
      assert_eq!(
         pressure.observe(349, 9_240, 1_240),
         Some(Change::Raised {
            share: 3,
            cause: Cause::Sustained,
         })
      );
      assert_eq!(
         pressure.status(1_240),
         Status {
            flooding:          true,
            cause:             Some(Cause::Sustained),
            since:             Some(1_240),
            until:             Some(1_840),
            share:             3,
            sample:            9_240,
            observed_at:       Some(1_240),
            streak_minutes:    5,
            flood_percent:     50,
            sustained_percent: 3,
            sustained_minutes: 5,
            hold_minutes:      10,
         }
      );

      // Continuing sustained pressure extends `until` while preserving `since`.
      assert_eq!(pressure.observe(400, 9_240, 1_300), None);
      let extended = pressure.status(1_300);
      assert!(extended.flooding);
      assert_eq!(extended.cause, Some(Cause::Sustained));
      assert_eq!(extended.since, Some(1_240));
      assert_eq!(extended.until, Some(1_900));
      assert_eq!(extended.streak_minutes, 6);

      // A single-minute spike while already sustained upgrades cause to Spike.
      assert_eq!(pressure.observe(5_000, 9_240, 1_360), None);
      assert_eq!(pressure.status(1_360).cause, Some(Cause::Spike));
      assert_eq!(pressure.status(1_360).until, Some(1_960));

      // Quiet minute inside the hold resets the streak without lifting yet.
      assert_eq!(pressure.observe(0, 9_240, 1_420), None);
      assert_eq!(pressure.status(1_420).streak_minutes, 0);
      assert!(pressure.status(1_420).flooding);

      // Once hold expires (`1_960`), the next calm report lifts and clears state.
      assert_eq!(pressure.observe(0, 9_240, 1_960), Some(Change::Lifted));
      assert_eq!(
         pressure.status(1_960),
         Status {
            flooding:          false,
            cause:             None,
            since:             None,
            until:             None,
            share:             0,
            sample:            9_240,
            observed_at:       Some(1_960),
            streak_minutes:    0,
            flood_percent:     50,
            sustained_percent: 3,
            sustained_minutes: 5,
            hold_minutes:      10,
         }
      );
   }

   #[test]
   fn broken_sustained_streak_resets_before_reaching_threshold() {
      let pressure = Pressure::with_sustained(50, 3, 5, 10);
      for minute in 0..4_i64 {
         assert_eq!(pressure.observe(4, 100, 1_000 + minute * 60), None);
      }
      assert_eq!(pressure.status(1_180).streak_minutes, 4);

      // Below sustained_percent (2%) resets streak to 0.
      assert_eq!(pressure.observe(2, 100, 1_240), None);
      assert_eq!(pressure.status(1_240).streak_minutes, 0);
      assert_eq!(pressure.level(1_240), Level::Calm);

      // Must accumulate another full 5 minutes to trigger.
      for minute in 0..4_i64 {
         assert_eq!(pressure.observe(3, 100, 1_300 + minute * 60), None);
      }
      assert_eq!(pressure.level(1_480), Level::Calm);
      assert_eq!(
         pressure.observe(3, 100, 1_540),
         Some(Change::Raised {
            share: 3,
            cause: Cause::Sustained,
         })
      );
   }
}