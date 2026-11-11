//! Import cookie accounts and learn which `auth_token`s still sign in.
//!
//! `ct0` is not stored with the account. x.com sets a fresh one, along with
//! `twid`, on the response to a plain visit while the `auth_token` cookie is
//! sent, so a missing `ct0` is fetched rather than borrowed from another
//! account. An account whose token x.com answers by clearing gets no `ct0`
//! and is dead. A live account is appended to the session pool; a dead one is
//! written beside it so it is not imported again.
//!
//! The visit goes out through a primp client impersonating Chrome, so the TLS
//! fingerprint is a browser's, and the headers leave in the order captured
//! from a real Chrome 152 page load of x.com.
//!
//!     cargo run --release --bin import-accounts -- <accounts-file>

use std::{
   collections::HashSet,
   env, fs,
   io::{BufRead, Write},
   sync::Arc,
   time::Duration,
};

use primp::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde_json::json;

const POOL: &str = "sessions.jsonl";
const DEAD: &str = "sessions-dead.jsonl";

/// Header order of a document navigation to `x.com/home`, taken from
/// `chrome-net-export-log_676767.json` (Chrome 152, HTTP/2 send headers).
/// Names this visit does not send are skipped.
const NAVIGATION_HEADER_ORDER: &[&str] = &[
   "cache-control",
   "sec-gpc",
   "user-agent",
   "accept",
   "upgrade-insecure-requests",
   "dnt",
   "sec-fetch-site",
   "sec-fetch-mode",
   "sec-fetch-dest",
   "referer",
   "accept-encoding",
   "accept-language",
   "cookie",
   "priority",
];

#[derive(Serialize)]
struct SessionLine<'a> {
   id: i64,
   username: &'a str,
   kind: &'static str,
   auth_token: &'a str,
   ct0: &'a str,
}

struct Account {
   username: String,
   auth_token: String,
   ct0: String,
}

fn is_hex(value: &str, len: usize) -> bool {
   value.len() == len && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The two export shapes in `accounts_need_adding`:
/// `user:pass:email:...:ct0:auth` and `user----pass----email----null----totp----auth`.
/// The 40-hex field is the `auth_token`; the 160-hex one, when present, is a
/// `ct0` that came with the export. x.com issues a fresh `ct0` on the visit
/// either way, so a row that has none is still usable.
fn parse_account(line: &str) -> Option<Account> {
   let fields: Vec<&str> = if line.contains("----") {
      line.split("----").collect()
   } else {
      line.split(':').collect()
   };
   let auth_token = fields.iter().copied().rev().find(|field| is_hex(field, 40))?;
   let ct0 = fields.iter().copied().find(|field| is_hex(field, 160)).unwrap_or("");
   let username = fields.first().copied().unwrap_or("");
   (!username.is_empty()).then(|| Account {
      username: username.to_owned(),
      auth_token: auth_token.to_owned(),
      ct0: ct0.to_owned(),
   })
}

fn known_tokens(path: &str) -> HashSet<String> {
   let mut tokens = HashSet::new();
   let Ok(file) = fs::File::open(path) else {
      return tokens;
   };
   for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
      if let Some(token) = serde_json::from_str::<serde_json::Value>(&line)
         .ok()
         .and_then(|value| value.get("auth_token").and_then(|token| token.as_str()).map(str::to_owned))
      {
         tokens.insert(token);
      }
   }
   tokens
}

fn next_id(path: &str) -> i64 {
   let mut max_id = 0_i64;
   if let Ok(file) = fs::File::open(path) {
      for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
         if let Some(id) = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|value| value.get("id").and_then(|id| id.as_i64()))
         {
            max_id = max_id.max(id);
         }
      }
   }
   max_id + 1
}

fn proxies() -> Vec<String> {
   let mut proxies = Vec::new();
   if let Ok(file) = fs::File::open("proxies.txt") {
      for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
         let line = line.trim();
         if line.is_empty() || line.starts_with('#') {
            continue;
         }
         let mut parts = line.split(':');
         let Some(host) = parts.next() else { continue };
         let Some(port) = parts.next() else { continue };
         let user = parts.next().unwrap_or("");
         let pass = parts.next().unwrap_or("");
         proxies.push(format!("socks5h://{user}:{pass}@{host}:{port}"));
      }
   }
   proxies
}

#[derive(Clone)]
struct Visit {
   ct0: String,
   twid: String,
   cleared: bool,
}

/// A Chrome client whose TLS fingerprint and header order match the captured
/// page load. One per proxy, shared across the accounts that exit through it.
fn browser(proxy: &str) -> Result<primp::Client, String> {
   let order = NAVIGATION_HEADER_ORDER
      .iter()
      .map(|name| HeaderName::from_static(name))
      .collect();
   let mut client = primp::Client::builder()
      .impersonate(primp::Impersonate::ChromeV152)
      .impersonate_os(primp::ImpersonateOS::Windows)
      .redirect(primp::redirect::Policy::none())
      .timeout(Duration::from_secs(25))
      .proxy(primp::Proxy::all(proxy).map_err(|err| err.to_string())?)
      .http2_headers_order(order)
      .build()
      .map_err(|err| err.to_string())?;
   // Impersonation installs a document-navigation set already, but with its
   // own accept-language. Pin the one the capture sent.
   client.headers_mut().insert(
      HeaderName::from_static("accept-language"),
      HeaderValue::from_static("en-US,en;q=0.9"),
   );
   Ok(client)
}

fn cookie_value(headers: &HeaderMap, want: &str) -> Option<String> {
   headers.get_all(HeaderName::from_static("set-cookie")).iter().find_map(|header| {
      let text = header.to_str().ok()?;
      let (pair, _) = text.split_once(';').unwrap_or((text, ""));
      let (name, value) = pair.split_once('=')?;
      (name.trim() == want).then(|| value.trim().to_owned())
   })
}

async fn visit(client: &primp::Client, auth_token: &str) -> Result<Visit, String> {
   let response = client
      .get("https://x.com/")
      .header(
         HeaderName::from_static("cookie"),
         HeaderValue::from_str(&format!("auth_token={auth_token}")).map_err(|err| err.to_string())?,
      )
      .send()
      .await
      .map_err(|err| err.to_string())?;
   let headers = response.headers().clone();
   Ok(Visit {
      ct0: cookie_value(&headers, "ct0").unwrap_or_default(),
      twid: cookie_value(&headers, "twid").unwrap_or_default(),
      cleared: cookie_value(&headers, "auth_token").is_some_and(|value| value.is_empty()),
   })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
   let accounts_path = env::args().nth(1).ok_or("usage: import-accounts <accounts-file>")?;
   let accounts: Vec<Account> = fs::read_to_string(&accounts_path)?
      .lines()
      .filter(|line| !line.trim().is_empty())
      .filter_map(parse_account)
      .collect();

   let mut seen = known_tokens(POOL);
   seen.extend(known_tokens(DEAD));
   let mut id = next_id(POOL);
   let proxies = proxies();
   if proxies.is_empty() {
      return Err("no proxies in proxies.txt".into());
   }

   let pending: Vec<&Account> = accounts
      .iter()
      .filter(|account| !seen.contains(&account.auth_token))
      .collect();
   println!("{} accounts, {} new, {} exits", accounts.len(), pending.len(), proxies.len());

   let concurrency = 6;
   let clients: Vec<Arc<primp::Client>> = proxies
      .iter()
      .map(|proxy| browser(proxy).map(Arc::new))
      .collect::<Result<_, _>>()?;
   let mut outcomes: Vec<Option<Result<Visit, String>>> = vec![None; pending.len()];
   for start in (0..pending.len()).step_by(concurrency) {
      let end = (start + concurrency).min(pending.len());
      let mut tasks = tokio::task::JoinSet::new();
      for (offset, account) in pending[start..end].iter().enumerate() {
         let client = clients[(start + offset) % clients.len()].clone();
         let auth = account.auth_token.clone();
         tasks.spawn(async move { visit(&client, &auth).await });
      }
      let mut slot = start;
      while let Some(joined) = tasks.join_next().await {
         outcomes[slot] = Some(joined.unwrap_or_else(|err| Err(err.to_string())));
         slot += 1;
      }
      if end % 300 == 0 || end == pending.len() {
         println!("checked {end} of {}", pending.len());
      }
   }
   let dead = fs::OpenOptions::new().create(true).append(true).open(DEAD)?;
   let pool = fs::OpenOptions::new().create(true).append(true).open(POOL)?;
   let mut pool = std::io::BufWriter::new(pool);
   let mut dead = std::io::BufWriter::new(dead);

   let mut added = 0_usize;
   let mut rejected = 0_usize;
   let skipped = accounts.len() - pending.len();
   let mut failed = 0_usize;

   for (account, outcome) in pending.iter().zip(outcomes) {
      let username = &account.username;
      let auth = &account.auth_token;
      let given_ct0 = &account.ct0;
      let visit = match outcome.as_ref() {
         Some(Ok(visit)) => visit.clone(),
         _ => match visit(&clients[clients.len() / 2], auth).await {
            Ok(visit) => visit,
            Err(err) => {
               failed += 1;
               eprintln!("visit failed for {username}: {err}");
               continue;
            },
         },
      };
      if visit.ct0.is_empty() {
         rejected += 1;
         writeln!(dead, "{}", json!({ "username": username, "auth_token": auth, "cleared": visit.cleared }))?;
         continue;
      }
      let named = visit
         .twid
         .split_once("%3D")
         .map(|(_, rest)| format!("x-{rest}"))
         .filter(|name| name.len() > 2)
         .unwrap_or_else(|| username.clone());
      let ct0: &str = if given_ct0.is_empty() { &visit.ct0 } else { given_ct0 };
      writeln!(
         pool,
         "{}",
         serde_json::to_string(&SessionLine { id, username: &named, kind: "cookie", auth_token: auth, ct0 })?
      )?;
      id += 1;
      added += 1;
   }
   pool.flush()?;
   dead.flush()?;
   println!("done: {added} added, {rejected} dead, {skipped} already known, {failed} not reached");
   Ok(())
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn both_export_shapes_yield_the_auth_token() {
      let dash = parse_account(
         "Florence----pw----a@b.c----null----TOTP----fee0e814717d0b7c035f241ed1d35fbb13420251",
      )
      .unwrap();
      assert_eq!(dash.username, "Florence");
      assert_eq!(dash.auth_token, "fee0e814717d0b7c035f241ed1d35fbb13420251");
      assert!(dash.ct0.is_empty());

      let ct0 = "ab".repeat(80);
      let token = "5a".repeat(20);
      let colon = parse_account(&format!("Harper:pw:a@b.c:oauth:{ct0}:{token}")).unwrap();
      assert_eq!(colon.ct0.len(), 160);
      assert_eq!(colon.auth_token, token);
   }

   #[test]
   fn a_token_that_follows_the_long_field_is_still_the_auth_token() {
      let long = "ab".repeat(80);
      let line = format!("Name:pw:a@b.c:fake:TOTP:{long}:ad336e60b79aec6bb893753a1d622ec7e8b5e2a4");
      let account = parse_account(&line).unwrap();
      assert_eq!(account.auth_token, "ad336e60b79aec6bb893753a1d622ec7e8b5e2a4");
      assert_eq!(account.ct0.len(), 160);
   }
}