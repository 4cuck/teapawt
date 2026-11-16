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
   collections::{
      BTreeMap,
      HashMap,
      HashSet,
   },
   sync::Arc,
   time::Duration,
};

use axum::http::{
   HeaderMap,
   header,
};
use serde::Serialize;
use tokio::sync::{
   Notify,
   RwLock,
};

use crate::{
   accounts::AccountStore,
   error::Result,
   types::Session,
};

type CookieBags = HashMap<i64, BTreeMap<String, String>>;

#[derive(Default)]
struct JarState {
   cookies: CookieBags,
   dirty:   HashSet<i64>,
}

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
   state: RwLock<JarState>,
   flush: Notify,
}

impl Jar {
   pub async fn from_store(store: &AccountStore) -> Result<Arc<Self>> {
      Ok(Self::new(store.load_cookies().await?))
   }

   #[cfg(test)]
   pub(super) fn memory() -> Arc<Self> {
      Self::new(CookieBags::new())
   }

   fn new(cookies: CookieBags) -> Arc<Self> {
      Arc::new(Self {
         state: RwLock::new(JarState {
            cookies,
            dirty: HashSet::new(),
         }),
         flush: Notify::new(),
      })
   }

   /// Seed only unpublished accounts, leaving existing runtime jars alone.
   pub async fn seed_imports(&self, sessions: &[Session], mut cookies: CookieBags) {
      let mut state = self.state.write().await;
      for session in sessions {
         if let Some(bag) = cookies.remove(&session.id) {
            state.cookies.entry(session.id).or_insert(bag);
         }
      }
   }

   /// Flush only dirty account jars, including empty jars for deletions.
   pub fn spawn_flusher(self: &Arc<Self>, store: AccountStore) {
      let jar = Arc::clone(self);
      tokio::spawn(async move {
         loop {
            jar.flush.notified().await;
            tokio::time::sleep(FLUSH_EVERY).await;
            let pending = {
               let mut state = jar.state.write().await;
               let ids = std::mem::take(&mut state.dirty);
               ids.into_iter()
                  .map(|id| (id, state.cookies[&id].clone()))
                  .collect::<CookieBags>()
            };
            if !pending.is_empty()
               && let Err(err) = store.save_cookies(&pending).await
            {
               // Re-mark ids, not stale snapshots: the next flush will read
               // the newest jar, including changes made during this write.
               jar.state.write().await.dirty.extend(pending.into_keys());
               jar.flush.notify_one();
               tracing::warn!("cookie jars not written; will retry: {err}");
            }
         }
      });
   }

   /// Keep the cookies an answer set. A value X deletes or expires is dropped
   /// rather than stored, so a cleared cookie stops being sent.
   pub async fn absorb(&self, session: i64, headers: &HeaderMap) {
      let mut changed = false;
      let mut state = self.state.write().await;
      let jar = state.cookies.entry(session).or_default();
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
      if changed {
         state.dirty.insert(session);
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
      let state = self.state.read().await;
      let Some(cookies) = state.cookies.get(&session) else {
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

   #[cfg(test)]
   async fn cookies_for(&self, session: i64) -> BTreeMap<String, String> {
      self.snapshot(session).await
   }

   pub async fn snapshot(&self, session: i64) -> BTreeMap<String, String> {
      self
         .state
         .read()
         .await
         .cookies
         .get(&session)
         .cloned()
         .unwrap_or_default()
   }
}

#[derive(Serialize)]
pub struct EditorCookie {
   pub name:      String,
   pub value:     String,
   pub domain:    &'static str,
   pub path:      &'static str,
   pub secure:    bool,
   #[serde(rename = "httpOnly")]
   pub http_only: bool,
   #[serde(rename = "sameSite")]
   pub same_site: &'static str,
   pub session:   bool,
}

pub(super) fn export_cookies(
   cookies: BTreeMap<String, String>,
   auth_token: String,
   ct0: String,
) -> Vec<EditorCookie> {
   std::iter::once(("auth_token".to_owned(), auth_token))
      .chain(std::iter::once(("ct0".to_owned(), ct0)))
      .chain(
         cookies
            .into_iter()
            .filter(|(name, _)| !SESSION_OWNED.contains(&name.as_str())),
      )
      .map(|(name, value)| {
         EditorCookie {
            http_only: name == "auth_token",
            name,
            value,
            domain: ".x.com",
            path: "/",
            secure: true,
            same_site: "no_restriction",
            session: true,
         }
      })
      .collect()
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
         attr.trim().split_once('=').is_some_and(|(key, val)| {
            key.trim().eq_ignore_ascii_case("max-age") && val.trim() == "0"
         })
      });
   Some((name.to_owned(), (!gone).then(|| value.trim().to_owned())))
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
      let jar = Jar::new(HashMap::new());
      jar.absorb(7, &header_map(&["guest_id=v1%3D1; Path=/; Domain=.x.com"]))
         .await;
      jar.absorb(7, &header_map(&["guest_id=v1%3D2; Path=/; Domain=.x.com"]))
         .await;
      assert_eq!(jar.cookies_for(7).await["guest_id"], "v1%3D2");
   }

   #[tokio::test]
   async fn a_deleted_cookie_stops_being_kept() {
      let jar = Jar::new(HashMap::new());
      jar.absorb(7, &header_map(&["att=1; Path=/", "gt=2; Path=/"]))
         .await;
      jar.absorb(7, &header_map(&["att=deleted; Path=/; Max-Age=0"]))
         .await;
      let held = jar.cookies_for(7).await;
      assert!(!held.contains_key("att"));
      assert_eq!(held["gt"], "2");
   }

   #[tokio::test]
   async fn the_session_cookies_win_over_the_jar() {
      let jar = Jar::new(HashMap::new());
      jar.absorb(
         7,
         &header_map(&["twid=u%3D42; Path=/", "ct0=stale; Path=/"]),
      )
      .await;
      let mut headers = HeaderMap::new();
      headers.insert(header::COOKIE, "auth_token=t; ct0=fresh".parse().unwrap());
      jar.merge_into(7, &mut headers).await;
      let sent = headers[header::COOKIE].to_str().unwrap();
      assert_eq!(sent, "auth_token=t; ct0=fresh; twid=u%3D42");
   }

   #[tokio::test]
   async fn an_account_keeps_its_own_bag() {
      let jar = Jar::new(HashMap::new());
      jar.absorb(1, &header_map(&["twid=u%3D1"])).await;
      jar.absorb(2, &header_map(&["twid=u%3D2"])).await;
      assert_eq!(jar.cookies_for(1).await["twid"], "u%3D1");
      assert_eq!(jar.cookies_for(2).await["twid"], "u%3D2");
   }

   #[tokio::test]
   async fn the_csrf_pair_is_never_kept_or_replayed() {
      let jar = Jar::new(HashMap::new());
      // A refresh visit sends `auth_token` alone so x.com hands back a new
      // ct0; replaying a stored one is what made every refresh fail.
      jar.absorb(
         7,
         &header_map(&["auth_token=t; Path=/", "ct0=stale; Path=/"]),
      )
      .await;
      assert!(jar.cookies_for(7).await.is_empty());

      let mut headers = HeaderMap::new();
      headers.insert(header::COOKIE, "auth_token=t".parse().unwrap());
      jar.merge_into(7, &mut headers).await;
      assert_eq!(headers[header::COOKIE].to_str().unwrap(), "auth_token=t");
   }
}
