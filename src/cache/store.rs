//! The in-process cache itself.

use std::{
   any::Any,
   collections::{HashMap, HashSet},
   sync::{Arc, Mutex, RwLock},
   time::{Duration, Instant},
};

/// Cache entry holding a type-erased value and its expiry.
struct Entry {
   value: Arc<dyn Any + Send + Sync>,
   fresh_until: Instant,
   stale_until: Instant,
}

/// A cache lookup that may still be served after its fresh TTL.
pub enum Hit<T> {
   Fresh(T),
   Stale(T),
}

/// Drops the in-flight refresh mark when the refresher finishes or panics.
pub struct RefreshGuard {
   cache: Cache,
   key: String,
}

impl Drop for RefreshGuard {
   fn drop(&mut self) {
      self.cache.finish_refresh(&self.key);
   }
}

/// In-process cache with TTL-based expiry and a hard entry-count cap.
///
/// When an insert exceeds `max_entries`, expired entries are purged first. If
/// the map remains over capacity, username/id mappings are dropped before
/// pages: those mappings expire last, so soonest-expiry eviction used to
/// throw away every profile and leave the map full of ids. Only a small batch
/// is removed. Dropping a quarter of the map on every overflow threw away
/// pages a few seconds old, and the TTL never applied while the site was busy.
#[derive(Clone)]
pub struct Cache {
   inner: Arc<RwLock<HashMap<String, Entry>>>,
   inflight: Arc<Mutex<HashSet<String>>>,
   max_entries: usize,
   /// Off for the live process. Cloudflare caches the HTML; keeping another
   /// copy here served posts after they were already old and, once the map
   /// was full, every page waited on its lock.
   enabled: bool,
}

impl Cache {
   pub fn new(max_entries: usize) -> Self {
      Self {
         inner: Arc::new(RwLock::new(HashMap::new())),
         inflight: Arc::new(Mutex::new(HashSet::new())),
         max_entries: max_entries.max(16),
         enabled: true,
      }
   }

   /// No stored pages and no background refresh. Lookups miss.
   pub fn disabled() -> Self {
      Self {
         inner: Arc::new(RwLock::new(HashMap::new())),
         inflight: Arc::new(Mutex::new(HashSet::new())),
         max_entries: 0,
         enabled: false,
      }
   }

   pub const fn enabled(&self) -> bool {
      self.enabled
   }

   /// Get a value from cache, returning [`None`] if missing, expired, or
   /// type-mismatched.
   pub fn get<T>(&self, key: &str) -> Option<T>
   where
      T: Any + Send + Sync + Clone,
   {
      match self.lookup(key) {
         Some(Hit::Fresh(value)) => Some(value),
         _ => None,
      }
   }

   /// Fresh or stale value. Stale is only returned for keys stored with
   /// [`set_swr`](Self::set_swr).
   #[expect(clippy::significant_drop_tightening, reason = "entry borrows from map")]
   pub fn lookup<T>(&self, key: &str) -> Option<Hit<T>>
   where
      T: Any + Send + Sync + Clone,
   {
      if !self.enabled {
         return None;
      }
      let map = self.inner.read().ok()?;
      let entry = map.get(key)?;
      let now = Instant::now();
      if entry.stale_until <= now {
         return None;
      }
      let value = entry.value.downcast_ref::<T>()?.clone();
      if entry.fresh_until > now {
         Some(Hit::Fresh(value))
      } else {
         Some(Hit::Stale(value))
      }
   }

   /// Last stored value, including one past its stale window.
   ///
   /// Eviction still drops it once the cache is over capacity. Until then a
   /// rate-limited profile can be shown instead of the error page.
   pub fn recall<T>(&self, key: &str) -> Option<T>
   where
      T: Any + Send + Sync + Clone,
   {
      if !self.enabled {
         return None;
      }
      let map = self.inner.read().ok()?;
      map.get(key)?.value.downcast_ref::<T>().cloned()
   }

   /// Set a value in cache with TTL in seconds.
   pub fn set<T>(&self, cache_key: &str, value: &T, ttl_seconds: u64)
   where
      T: Any + Send + Sync + Clone,
   {
      self.insert(cache_key, value, ttl_seconds, 0);
   }

   /// Fresh for `fresh_seconds`, then serve stale for `stale_seconds` more.
   pub fn set_swr<T>(&self, cache_key: &str, value: &T, fresh_seconds: u64, stale_seconds: u64)
   where
      T: Any + Send + Sync + Clone,
   {
      self.insert(cache_key, value, fresh_seconds, stale_seconds);
   }

   /// Mark `key` as refreshing. The guard clears the mark on drop.
   pub fn start_refresh(&self, key: &str) -> Option<RefreshGuard> {
      if !self.enabled {
         return None;
      }
      let inserted = self.inflight.lock().ok()?.insert(key.to_owned());
      inserted.then(|| RefreshGuard {
         cache: self.clone(),
         key: key.to_owned(),
      })
   }

   fn finish_refresh(&self, key: &str) {
      if let Ok(mut inflight) = self.inflight.lock() {
         inflight.remove(key);
      }
   }

   fn insert<T>(&self, cache_key: &str, value: &T, fresh_seconds: u64, stale_seconds: u64)
   where
      T: Any + Send + Sync + Clone,
   {
      if !self.enabled {
         return;
      }
      let now = Instant::now();
      let fresh_until = now + Duration::from_secs(fresh_seconds);
      let cache_entry = Entry {
         value: Arc::new(value.clone()),
         fresh_until,
         stale_until: fresh_until + Duration::from_secs(stale_seconds),
      };
      if let Ok(mut map) = self.inner.write() {
         map.insert(cache_key.to_owned(), cache_entry);

         if map.len() > self.max_entries {
            map.retain(|_, cached| cached.stale_until > now);

            if map.len() > self.max_entries {
               // A handful past the cap, so this scan does not run on every
               // insert, and a page stored seconds ago is not in the batch.
               let room = (self.max_entries / 8).clamp(1, 64);
               let batch = (map.len() - self.max_entries).clamp(1, room);
               // Id mappings live 24 hours, so soonest-expiry eviction deleted
               // every profile and timeline first and left the map full of
               // ids. A repeat view then missed and paid for X again. Drop
               // the ids first; a cached page does not need them.
               let id_keys: Vec<String> = map
                  .keys()
                  .filter(|key| {
                     *key != &cache_key && (key.starts_with("uid:") || key.starts_with("unid:"))
                  })
                  .cloned()
                  .collect();
               let id_drop = id_keys.len().min(batch);
               for eviction_key in id_keys.into_iter().take(id_drop) {
                  map.remove(&eviction_key);
               }

               if map.len() > self.max_entries {
                  let drop_n = (map.len() - self.max_entries).clamp(1, room);
                  let mut by_expiry: Vec<(Instant, String)> = map
                     .iter()
                     .filter(|(stored_key, _)| stored_key.as_str() != cache_key)
                     .map(|(stored_key, stored_entry)| {
                        (stored_entry.stale_until, stored_key.clone())
                     })
                     .collect();
                  if drop_n < by_expiry.len() {
                     by_expiry.select_nth_unstable_by_key(drop_n, |&(expiry, _)| expiry);
                  }
                  for (_, eviction_key) in by_expiry.into_iter().take(drop_n) {
                     map.remove(&eviction_key);
                  }
               }
            }
         }
      }
   }

   /// Delete a key from cache.
   pub fn delete(&self, key: &str) {
      if let Ok(mut map) = self.inner.write() {
         map.remove(key);
      }
   }

   /// Keys currently stored that start with `prefix` (fresh or stale).
   pub fn keys_with_prefix(&self, prefix: &str) -> Vec<String> {
      let Ok(map) = self.inner.read() else {
         return Vec::new();
      };
      map.keys()
         .filter(|key| key.starts_with(prefix))
         .cloned()
         .collect()
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn recall_returns_a_value_past_its_stale_window() {
      let cache = Cache::new(8);
      cache.set("profile", &7_u32, 0);
      assert!(cache.lookup::<u32>("profile").is_none());
      assert_eq!(cache.recall::<u32>("profile"), Some(7));
   }

   #[test]
   fn a_full_cache_drops_id_mappings_before_a_page() {
      let cache = Cache::new(16);
      for i in 0..16 {
         cache.set(&format!("uid:{i}"), &format!("id{i}"), 24 * 60 * 60);
      }
      cache.set("p:nasa", &"profile".to_string(), 5 * 60);
      assert_eq!(cache.get::<String>("p:nasa").as_deref(), Some("profile"));
      let ids = cache.keys_with_prefix("uid:").len();
      assert!(ids < 16, "id mappings were kept and the page was the victim");
   }

   #[test]
   fn a_full_cache_does_not_wipe_a_quarter_of_its_pages() {
      let cache = Cache::new(16);
      for i in 0..16 {
         cache.set(&format!("p:{i}"), &format!("old{i}"), 5 * 60);
      }
      cache.set("p:nasa", &"profile".to_string(), 5 * 60);
      assert_eq!(cache.get::<String>("p:nasa").as_deref(), Some("profile"));
      let pages = cache.keys_with_prefix("p:").len();
      assert!(
         pages >= 15,
         "eviction cleared a quarter of the cache, left {pages}"
      );
   }
}
