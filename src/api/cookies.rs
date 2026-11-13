//! The cookies X sets on a response, kept per account.
//!
//! A browser keeps every cookie a response hands it and sends the whole bag
//! back until X replaces or expires a value. teapot sent only `auth_token` and
//! `ct0`, so `guest_id`, `twid`, `__cuid`, `personalization_id`, `lang` and the
//! rest that X hands out during a visit were dropped on every response, and
//! the next call looked like a browser whose storage had just been cleared.
//!
//! Nothing here asks x.com for cookies. The jar fills from the responses the
//! calls in hand were already making: a `set-cookie` on any answer replaces the
//! value it names, and a value that is deleted or expired is dropped, which is
//! what a browser does with the same header.

use std::{
   collections::{BTreeMap, HashMap},
   path::PathBuf,
   sync::{
      Arc,
      atomic::{AtomicBool, Ordering},
   },
   time::Duration,
};

use axum::http::{HeaderMap, header};
use tokio::sync::{Notify, RwLock};

/// How long the jar may sit dirty before it is written out.
const FLUSH_EVERY: Duration = Duration::from_secs(15);

/// The two cookies the session owns. `auth_token` is what authenticates and
/// `ct0` is the csrf token this code rotates itself: a visit that carries
/// `auth_token` alone is what makes x.com hand back a new `ct0`. Keeping a
/// second copy in the jar and sending it alongside the real one leaves X
/// answering with the value the refresh was trying to replace, so neither is
/// ever stored or replayed from here.
const SESSION_OWNED: [&str; 2] = ["auth_token", "ct0"];

/// Every account's cookies, keyed by session id.
pub struct Jar {
   path: PathBuf,
   cookies: RwLock<HashMap<i64, BTreeMap<String, String>>>,
   dirty: AtomicBool,
   flush: Notify,
}

impl Jar {
   /// Open the jar at `path`, keeping whatever a previous run stored.
   pub fn load(path: impl Into<PathBuf>) -> Arc<Self> {
      let path = path.into();
      let cookies = std::fs::read_to_string(&path)
         .ok()
         .and_then(|text| serde_json::from_str::<HashMap<i64, BTreeMap<String, String>>>(&text).ok())
         .unwrap_or_default();
      if !cookies.is_empty() {
         tracing::info!(accounts = cookies.len(), file = %path.display(), "cookie jar loaded");
      }
      Arc::new(Self {
         path,
         cookies: RwLock::new(cookies),
         dirty: AtomicBool::new(false),
         flush: Notify::new(),
      })
   }

   /// Write the jar out on a timer, and once more when the process stops.
   pub fn spawn_flusher(self: &Arc<Self>) {
      let jar = Arc::clone(self);
      tokio::spawn(async move {
         loop {
            jar.flush.notified().await;
            tokio::time::sleep(FLUSH_EVERY).await;
            if !jar.dirty.swap(false, Ordering::Relaxed) {
               continue;
            }
            if let Err(err) = jar.write().await {
               jar.dirty.store(true, Ordering::Relaxed);
               tracing::warn!(file = %jar.path.display(), "cookie jar not written: {err}");
            }
         }
      });
   }

   /// Keep the cookies an answer set. A value X deletes or expires is dropped
   /// rather than stored, so a cleared cookie stops being sent.
   pub async fn absorb(&self, session: i64, headers: &HeaderMap) {
      let mut changed = false;
      let mut limits = self.cookies.write().await;
      let jar = limits.entry(session).or_default();
      for value in headers.get_all(header::SET_COOKIE) {
         let Ok(line) = value.to_str() else {
            continue;
         };
         let Some((name, cookie)) = parse(line) else {
            continue;
         };
         if SESSION_OWNED.contains(&name.as_str()) {
            continue;
         }
         match cookie {
            Some(value) => {
               if jar.get(&name) != Some(&value) {
                  jar.insert(name, value);
                  changed = true;
               }
            },
            None => changed |= jar.remove(&name).is_some(),
         }
      }
      drop(limits);
      if changed {
         self.dirty.store(true, Ordering::Relaxed);
         self.flush.notify_one();
      }
   }

   /// Add this account's cookies to a request that already carries a `cookie`
   /// header. Values the call set itself (`auth_token`, `ct0`) win: they are
   /// the ones the session is authenticated with.
   pub async fn merge_into(&self, session: i64, headers: &mut HeaderMap) {
      let Some(existing) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) else {
         return;
      };
      let mut names: Vec<&str> = existing
         .split(';')
         .filter_map(|pair| pair.split('=').next())
         .map(str::trim)
         .collect();
      let jar = self.cookies.read().await;
      let Some(cookies) = jar.get(&session) else {
         return;
      };
      let mut merged = existing.to_owned();
      for (name, value) in cookies {
         if names.contains(&name.as_str()) || SESSION_OWNED.contains(&name.as_str()) {
            continue;
         }
         merged.push_str("; ");
         merged.push_str(name);
         merged.push('=');
         merged.push_str(value);
         names.push(name);
      }
      if let Ok(value) = merged.parse() {
         headers.insert(header::COOKIE, value);
      }
   }

   /// The bag this account holds, for tests and for seeding from an import.
   pub async fn cookies_for(&self, session: i64) -> BTreeMap<String, String> {
      self.cookies.read().await.get(&session).cloned().unwrap_or_default()
   }

   async fn write(&self) -> std::io::Result<()> {
      let body = {
         let cookies = self.cookies.read().await;
         serde_json::to_string(&*cookies).map_err(std::io::Error::other)?
      };
      let tmp = format!("{}.tmp", self.path.display());
      tokio::fs::write(&tmp, body).await?;
      tokio::fs::rename(&tmp, &self.path).await
   }
}

/// `name=value; Path=/; Max-Age=0` → the name, and the value to keep, or
/// `None` when X is telling the browser to drop it.
fn parse(line: &str) -> Option<(String, Option<String>)> {
   let mut parts = line.split(';');
   let pair = parts.next()?.trim();
   let (name, value) = pair.split_once('=')?;
   let name = name.trim();
   if name.is_empty() {
      return None;
   }
   let gone = value.trim().eq_ignore_ascii_case("deleted")
      || parts.any(|attr| {
         attr.trim()
            .split_once('=')
            .is_some_and(|(key, val)| key.trim().eq_ignore_ascii_case("max-age") && val.trim() == "0")
      });
   Some((
      name.to_owned(),
      (!gone).then(|| value.trim().to_owned()),
   ))
}

#[cfg(test)]
mod tests {
   use super::*;

   fn header_map(pairs: &[&str]) -> HeaderMap {
      let mut headers = HeaderMap::new();
      for line in pairs {
         headers.append(header::SET_COOKIE, line.parse().unwrap());
      }
      headers
   }

   #[tokio::test]
   async fn a_response_replaces_the_value_it_names() {
      let jar = Jar::load("/tmp/teapot-jar-test.json");
      jar.absorb(7, &header_map(&["guest_id=v1%3D1; Path=/; Domain=.x.com"]))
         .await;
      jar.absorb(7, &header_map(&["guest_id=v1%3D2; Path=/; Domain=.x.com"]))
         .await;
      assert_eq!(jar.cookies_for(7).await["guest_id"], "v1%3D2");
   }

   #[tokio::test]
   async fn a_deleted_cookie_stops_being_kept() {
      let jar = Jar::load("/tmp/teapot-jar-test.json");
      jar.absorb(7, &header_map(&["att=1; Path=/", "gt=2; Path=/"])).await;
      jar.absorb(7, &header_map(&["att=deleted; Path=/; Max-Age=0"])).await;
      let held = jar.cookies_for(7).await;
      assert!(!held.contains_key("att"));
      assert_eq!(held["gt"], "2");
   }

   #[tokio::test]
   async fn the_session_cookies_win_over_the_jar() {
      let jar = Jar::load("/tmp/teapot-jar-test.json");
      jar.absorb(7, &header_map(&["twid=u%3D42; Path=/", "ct0=stale; Path=/"]))
         .await;
      let mut headers = HeaderMap::new();
      headers.insert(header::COOKIE, "auth_token=t; ct0=fresh".parse().unwrap());
      jar.merge_into(7, &mut headers).await;
      let sent = headers[header::COOKIE].to_str().unwrap();
      assert_eq!(sent, "auth_token=t; ct0=fresh; twid=u%3D42");
   }

   #[tokio::test]
   async fn an_account_keeps_its_own_bag() {
      let jar = Jar::load("/tmp/teapot-jar-test.json");
      jar.absorb(1, &header_map(&["twid=u%3D1"])).await;
      jar.absorb(2, &header_map(&["twid=u%3D2"])).await;
      assert_eq!(jar.cookies_for(1).await["twid"], "u%3D1");
      assert_eq!(jar.cookies_for(2).await["twid"], "u%3D2");
   }

   #[tokio::test]
   async fn the_csrf_pair_is_never_kept_or_replayed() {
      let jar = Jar::load("/tmp/teapot-jar-test.json");
      // A refresh visit sends `auth_token` alone so x.com hands back a new
      // ct0; replaying a stored one is what made every refresh fail.
      jar.absorb(7, &header_map(&["auth_token=t; Path=/", "ct0=stale; Path=/"]))
         .await;
      assert!(jar.cookies_for(7).await.is_empty());

      let mut headers = HeaderMap::new();
      headers.insert(header::COOKIE, "auth_token=t".parse().unwrap());
      jar.merge_into(7, &mut headers).await;
      assert_eq!(headers[header::COOKIE].to_str().unwrap(), "auth_token=t");
   }
}