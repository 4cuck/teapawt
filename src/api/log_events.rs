//! Upstream events that happen hundreds of times a minute. Each one is logged
//! at debug and counted here, and the per-minute report prints the counts, so
//! a warning or an error is not buried under them.

use std::{
   fmt::Write as _,
   sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Clone, Copy)]
pub enum Event {
   /// API headers missed their deadline and a second connection went out.
   HeaderStall,
   /// The first connection answered after all.
   LateHeaders,
   /// Neither connection answered in time.
   StalledTwice,
   /// Media headers missed their deadline.
   MediaStall,
   /// X refused an account a call with a bare 429 and the account rests.
   Bare429,
   /// X's edge refused a proxy exit with a 429.
   Edge429,
   /// A 429 carrying the account's own window.
   Account429,
   /// A call moved to another account.
   SessionRetry,
}

impl Event {
   const ALL: [Self; 8] = [
      Self::HeaderStall,
      Self::LateHeaders,
      Self::StalledTwice,
      Self::MediaStall,
      Self::Bare429,
      Self::Edge429,
      Self::Account429,
      Self::SessionRetry,
   ];

   const fn label(self) -> &'static str {
      match self {
         Self::HeaderStall => "header stalls",
         Self::LateHeaders => "late headers",
         Self::StalledTwice => "stalled twice",
         Self::MediaStall => "media stalls",
         Self::Bare429 => "bare 429s",
         Self::Edge429 => "edge 429s",
         Self::Account429 => "account 429s",
         Self::SessionRetry => "session retries",
      }
   }
}

/// Counts since the last report, by [`Event`].
pub struct LogEvents([AtomicU64; Event::ALL.len()]);

impl LogEvents {
   const fn new() -> Self {
      Self([const { AtomicU64::new(0) }; Event::ALL.len()])
   }

   fn note(&self, event: Event) {
      self.0[event as usize].fetch_add(1, Ordering::Relaxed);
   }

   /// `"12 header stalls, 3 bare 429s"`, leaving out what did not happen, and
   /// the counts reset. `None` when nothing happened.
   fn report(&self) -> Option<String> {
      let mut line = String::new();
      for (event, count) in Event::ALL
         .into_iter()
         .map(|event| (event, self.0[event as usize].swap(0, Ordering::Relaxed)))
         .filter(|&(_, count)| count > 0)
      {
         let separator = if line.is_empty() { "" } else { ", " };
         let _ = write!(line, "{separator}{count} {}", event.label());
      }
      (!line.is_empty()).then_some(line)
   }
}

/// Shared by every client, the media one included.
static EVENTS: LogEvents = LogEvents::new();

pub fn note(event: Event) {
   EVENTS.note(event);
}

pub fn report() -> Option<String> {
   EVENTS.report()
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn every_event_has_its_own_counter() {
      for (index, event) in Event::ALL.into_iter().enumerate() {
         assert_eq!(event as usize, index, "{event:?}");
      }
   }

   #[test]
   fn a_report_lists_what_happened_and_resets() {
      let events = LogEvents::new();
      assert_eq!(events.report(), None);
      events.note(Event::HeaderStall);
      events.note(Event::HeaderStall);
      events.note(Event::Bare429);
      assert_eq!(events.report().as_deref(), Some("2 header stalls, 1 bare 429s"));
      assert_eq!(events.report(), None);
   }
}