use std::{collections::VecDeque, time::Duration};

use tokio::{
   sync::{Mutex, Semaphore, SemaphorePermit},
   time::Instant,
};

use crate::error::{Error, Result};

const MAX_TRANSLATIONS_PER_WINDOW: usize = 60;
const WINDOW: Duration = Duration::from_secs(60);
const MAX_CONCURRENT_TRANSLATIONS: usize = 1024;

/// Global guard for translation backends.
pub struct TranslationLimiter {
   recent: Mutex<VecDeque<Instant>>,
   concurrent: Semaphore,
}

impl TranslationLimiter {
   pub fn new() -> Self {
      Self {
         recent: Mutex::new(VecDeque::new()),
         concurrent: Semaphore::new(MAX_CONCURRENT_TRANSLATIONS),
      }
   }

   /// Automatic translations (link previews, status pages). These stop once the
   /// site has done [`MAX_TRANSLATIONS_PER_WINDOW`] in the last minute.
   pub async fn acquire(&self) -> Result<SemaphorePermit<'_>> {
      self.acquire_in(true).await
   }

   /// A click on Translate. It does not spend the automatic minute, which page
   /// views and unfurls would otherwise fill before anyone can press the button.
   pub async fn acquire_interactive(&self) -> Result<SemaphorePermit<'_>> {
      self.acquire_in(false).await
   }

   async fn acquire_in(&self, paced: bool) -> Result<SemaphorePermit<'_>> {
      if paced {
         let now = Instant::now();
         let mut recent = self.recent.lock().await;
         while recent
            .front()
            .is_some_and(|started| now.duration_since(*started) >= WINDOW)
         {
            recent.pop_front();
         }
         if recent.len() >= MAX_TRANSLATIONS_PER_WINDOW {
            return Err(Error::RateLimited);
         }
         recent.push_back(now);
      }

      self
         .concurrent
         .acquire()
         .await
         .map_err(|_| Error::Internal("translation limiter closed".into()))
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[tokio::test]
   async fn a_click_still_translates_after_the_automatic_minute_is_full() {
      let limiter = TranslationLimiter::new();
      for _ in 0..MAX_TRANSLATIONS_PER_WINDOW {
         drop(limiter.acquire().await.unwrap());
      }
      assert!(matches!(limiter.acquire().await, Err(Error::RateLimited)));
      assert!(limiter.acquire_interactive().await.is_ok());
   }
}
