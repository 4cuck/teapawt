//! Paid search pass: a login, a browser session and whitelisted addresses
//! that skip the search checkbox.
//!
//! Postgres holds every account, order, session and address. This process
//! keeps a copy of who may search, so the gate never waits on the database
//! and a flood of searches never reaches it.

mod mail;
mod pay;
mod secret;

use std::{
   collections::HashMap,
   env,
   net::IpAddr,
   sync::{
      Arc,
      PoisonError,
      RwLock,
   },
   time::Duration,
};

use axum_extra::extract::CookieJar;
use tokio::sync::{
   Mutex,
   Semaphore,
};
use tokio_postgres::{
   Client,
   NoTls,
};

pub use self::secret::{
   valid_order_id,
   valid_token,
};
use crate::{
   api::budget::{
      self,
      ClientKey,
   },
   config::Config,
   donations::{
      self,
      PayError,
   },
};

/// Cloudflare skips its cache on search paths only when the Cookie header
/// contains `search_pass=`, so this name has to end that way.
pub const SESSION_COOKIE: &str = "paid_search_pass";
/// `{order id}.{claim token}`: which browser may collect a new login.
pub const ORDER_COOKIE: &str = "pass_order";
/// Set by the purchase page. Missing on the POST means cookies are blocked.
pub const CHECK_COOKIE: &str = "pass_check";
pub const SESSION_DAYS: i64 = 180;
/// What one payment adds. Longer than any month, so a card renewal is in
/// before the pass would lapse.
pub const PASS_DAYS: i64 = 31;
pub const MAX_ADDRESSES: usize = 5;
pub const DAY: i64 = 86_400;
const HOUR: i64 = 3600;
/// Card and LZT orders are read back from the provider this long.
const PENDING_WINDOW: i64 = 2 * DAY;
/// An emailed reset changes an account's password at most this often.
const RESET_GAP: i64 = 600;
const CHECKOUT_WINDOW: i64 = 600;
const CHECKOUTS_PER_WINDOW: usize = 5;
const UPKEEP_EVERY: Duration = Duration::from_secs(60);
/// Upkeep rounds between subscription checks and between rereads of older
/// open checkouts.
const SLOW_ROUNDS: u64 = 15;
const DESCRIPTION: &str = "Search pass, one month";
const DEFAULT_URL: &str = "host=/var/run/postgresql user=teapot dbname=teapot";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS pass_accounts (
   id bigserial PRIMARY KEY,
   username text NOT NULL UNIQUE,
   password_hash text NOT NULL DEFAULT '',
   email text NOT NULL DEFAULT '',
   paid_until bigint NOT NULL DEFAULT 0,
   stripe_subscription text NOT NULL DEFAULT '',
   renews boolean NOT NULL DEFAULT false,
   reset_at bigint NOT NULL DEFAULT 0,
   created_at bigint NOT NULL
);
CREATE INDEX IF NOT EXISTS pass_accounts_email ON pass_accounts (lower(email)) WHERE email <> '';
CREATE TABLE IF NOT EXISTS pass_orders (
   id text PRIMARY KEY,
   provider text NOT NULL,
   cents bigint NOT NULL,
   email text NOT NULL DEFAULT '',
   origin text NOT NULL DEFAULT '',
   claim_hash text NOT NULL,
   account_id bigint REFERENCES pass_accounts (id),
   renewal boolean NOT NULL DEFAULT false,
   checkout_ref text NOT NULL DEFAULT '',
   status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'paid')),
   provider_ref text NOT NULL DEFAULT '',
   created_at bigint NOT NULL,
   paid_at bigint NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS pass_orders_payment ON pass_orders (provider, provider_ref)
   WHERE provider_ref <> '';
CREATE INDEX IF NOT EXISTS pass_orders_pending ON pass_orders (created_at) WHERE status = \
                      'pending';
CREATE TABLE IF NOT EXISTS pass_sessions (
   token_hash text PRIMARY KEY,
   account_id bigint NOT NULL REFERENCES pass_accounts (id) ON DELETE CASCADE,
   expires_at bigint NOT NULL
);
CREATE TABLE IF NOT EXISTS pass_ips (
   client_key text NOT NULL,
   account_id bigint NOT NULL REFERENCES pass_accounts (id) ON DELETE CASCADE,
   created_at bigint NOT NULL,
   PRIMARY KEY (client_key, account_id)
);
";

#[derive(Debug)]
pub struct PassError(pub String);

impl std::fmt::Display for PassError {
   fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      formatter.write_str(&self.0)
   }
}

impl From<tokio_postgres::Error> for PassError {
   fn from(err: tokio_postgres::Error) -> Self {
      Self(format!("postgres: {err}"))
   }
}

impl From<PayError> for PassError {
   fn from(err: PayError) -> Self {
      Self(err.0)
   }
}

pub struct NewOrder<'a> {
   pub provider:   &'a str,
   pub email:      &'a str,
   /// Set when a signed-in pass is buying more time.
   pub account_id: Option<i64>,
   pub origin:     &'a str,
}

pub struct Order {
   pub id:           String,
   pub provider:     String,
   pub email:        String,
   pub account_id:   Option<i64>,
   pub renewal:      bool,
   pub checkout_ref: String,
   pub paid:         bool,
}

/// A payment reported by a provider.
pub struct Payment<'a> {
   pub order_id:     &'a str,
   pub provider:     &'a str,
   /// The provider's id for the payment. One payment settles one order.
   pub reference:    &'a str,
   pub cents:        i64,
   pub subscription: Option<&'a str>,
}

struct Settled {
   account_id: i64,
   fresh:      bool,
   renewal:    bool,
   email:      String,
   origin:     String,
}

struct Login {
   username:   String,
   password:   String,
   paid_until: i64,
}

pub struct Account {
   pub username:   String,
   pub email:      String,
   pub paid_until: i64,
   pub renews:     bool,
   pub card:       bool,
   pub addresses:  Vec<String>,
}

pub enum Claim {
   /// The login was made just now and is shown once.
   Issued {
      session:    String,
      username:   String,
      password:   String,
      paid_until: i64,
      emailed:    bool,
   },
   /// The password went out before, on this page or by email.
   SignedIn {
      session:    String,
      username:   String,
      paid_until: i64,
   },
   Refused,
}

pub enum Listing {
   Added,
   Full,
   AlreadyListed,
}

#[derive(Default)]
struct Index {
   paid_until: HashMap<i64, i64>,
   sessions:   HashMap<String, i64>,
   addresses:  HashMap<String, Vec<i64>>,
}

impl Index {
   fn active(&self, account: i64, now: i64) -> bool {
      self
         .paid_until
         .get(&account)
         .is_some_and(|until| *until > now)
   }

   fn address_active(&self, now: i64) -> bool {
      budget::current_client()
         .and_then(|client| self.addresses.get(client.as_str()))
         .is_some_and(|accounts| accounts.iter().any(|id| self.active(*id, now)))
   }
}

pub struct PassStore {
   config:    Arc<Config>,
   db:        Mutex<Option<Client>>,
   index:     RwLock<Index>,
   /// PBKDF2 runs on blocking threads. This keeps a flood of logins from
   /// taking all of them.
   hashing:   Semaphore,
   checkouts: std::sync::Mutex<HashMap<String, Vec<i64>>>,
}

fn now() -> i64 {
   time::OffsetDateTime::now_utc().unix_timestamp()
}

/// `November 30, 2026`, UTC.
pub fn date(unix: i64) -> String {
   let format = time::macros::format_description!("[month repr:long] [day padding:none], [year]");
   time::OffsetDateTime::from_unix_timestamp(unix)
      .ok()
      .and_then(|at| at.format(format).ok())
      .unwrap_or_default()
}

async fn connect() -> Result<Client, PassError> {
   let url = env::var("TEAPOT_LIMITS_DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
   let (client, connection) = tokio_postgres::connect(&url, NoTls).await?;
   tokio::spawn(async move {
      if let Err(err) = connection.await {
         tracing::warn!("postgres pass connection closed: {err}");
      }
   });
   client.batch_execute(SCHEMA).await?;
   Ok(client)
}

fn session_hash(jar: &CookieJar) -> Option<String> {
   jar.get(SESSION_COOKIE)
      .map(axum_extra::extract::cookie::Cookie::value)
      .filter(|token| secret::valid_token(token))
      .map(secret::digest)
}

impl PassStore {
   pub async fn open(config: Arc<Config>) -> Arc<Self> {
      let store = Arc::new(Self {
         config,
         db: Mutex::new(None),
         index: RwLock::default(),
         hashing: Semaphore::new(4),
         checkouts: std::sync::Mutex::default(),
      });
      store.reload().await;
      store
   }

   /// Run `work` on the connection, opening it first if it is closed.
   async fn with<T>(
      &self,
      work: impl AsyncFnOnce(&mut Client) -> Result<T, PassError>,
   ) -> Result<T, PassError> {
      let mut db = self.db.lock().await;
      if db.as_ref().is_none_or(Client::is_closed) {
         *db = Some(connect().await?);
      }
      let client = db
         .as_mut()
         .ok_or_else(|| PassError("no database connection".into()))?;
      work(client).await
   }

   /// Read who may search back from Postgres.
   pub async fn reload(&self) {
      let now = now();
      let loaded = self
         .with(async |db| {
            let mut index = Index::default();
            for row in db
               .query("SELECT id, paid_until FROM pass_accounts", &[])
               .await?
            {
               index.paid_until.insert(row.get(0), row.get(1));
            }
            for row in db
               .query(
                  "SELECT token_hash, account_id FROM pass_sessions WHERE expires_at > $1",
                  &[&now],
               )
               .await?
            {
               index.sessions.insert(row.get(0), row.get(1));
            }
            for row in db
               .query("SELECT client_key, account_id FROM pass_ips", &[])
               .await?
            {
               index
                  .addresses
                  .entry(row.get(0))
                  .or_default()
                  .push(row.get(1));
            }
            Ok(index)
         })
         .await;
      match loaded {
         Ok(index) => *self.index.write().unwrap_or_else(PoisonError::into_inner) = index,
         Err(err) => tracing::warn!("search passes were not reloaded: {err}"),
      }
   }

   /// Whether this request may search without the checkbox: a signed-in pass
   /// or a whitelisted address, either still paid up.
   pub fn admits(&self, jar: &CookieJar) -> bool {
      let now = now();
      let index = self.index.read().unwrap_or_else(PoisonError::into_inner);
      let by_session = session_hash(jar)
         .and_then(|hash| index.sessions.get(&hash).copied())
         .is_some_and(|id| index.active(id, now));
      by_session || index.address_active(now)
   }

   /// [`admits`](Self::admits) for callers that send no cookies.
   pub fn admits_address(&self) -> bool {
      self
         .index
         .read()
         .unwrap_or_else(PoisonError::into_inner)
         .address_active(now())
   }

   /// The account this browser is signed in to, paid up or not.
   pub fn signed_in(&self, jar: &CookieJar) -> Option<i64> {
      let hash = session_hash(jar)?;
      self
         .index
         .read()
         .unwrap_or_else(PoisonError::into_inner)
         .sessions
         .get(&hash)
         .copied()
   }

   pub fn price_cents(&self) -> i64 {
      self.config.pass.price_cents
   }

   pub fn mail_enabled(&self) -> bool {
      mail::enabled(&self.config.pass)
   }

   /// Starting a checkout makes an object at the provider and an order that
   /// is read back for two days, so one address gets a few at a time.
   pub fn allow_checkout(&self) -> bool {
      let Some(client) = budget::current_client() else {
         return true;
      };
      let now = now();
      let mut starts = self
         .checkouts
         .lock()
         .unwrap_or_else(PoisonError::into_inner);
      starts.retain(|_, times| {
         times.retain(|at| now - at < CHECKOUT_WINDOW);
         !times.is_empty()
      });
      let times = starts.entry(client.as_str().to_owned()).or_default();
      if times.len() >= CHECKOUTS_PER_WINDOW {
         return false;
      }
      times.push(now);
      true
   }

   /// Store a new order. Returns its id and the claim token for the buyer's
   /// cookie.
   pub async fn create_order(&self, order: &NewOrder<'_>) -> Result<(String, String), PassError> {
      let id = secret::order_id();
      let claim = secret::token();
      let claim_hash = secret::digest(&claim);
      let now = now();
      let cents = self.price_cents();
      self
         .with(async |db| {
            db.execute(
               "INSERT INTO pass_orders (id, provider, cents, email, origin, claim_hash, \
                account_id, renewal, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
               &[
                  &id,
                  &order.provider,
                  &cents,
                  &order.email,
                  &order.origin,
                  &claim_hash,
                  &order.account_id,
                  &order.account_id.is_some(),
                  &now,
               ],
            )
            .await?;
            Ok(())
         })
         .await?;
      Ok((id, claim))
   }

   /// Open the provider's payment page for an order. Returns its URL.
   pub async fn checkout(
      &self,
      order_id: &str,
      provider: &str,
      email: &str,
      origin: &str,
   ) -> Result<String, PassError> {
      let cents = self.price_cents();
      let keys = &self.config.donations;
      let back = format!("{origin}/search-pass/return?order={order_id}");
      let cancel = format!("{origin}/search-pass?note=canceled");
      match provider {
         "stripe" => {
            let (url, session) = pay::checkout(
               &keys.stripe.secret_key,
               order_id,
               cents,
               email,
               &format!("{back}&session_id={{CHECKOUT_SESSION_ID}}"),
               &cancel,
            )
            .await?;
            self
               .with(async |db| {
                  db.execute("UPDATE pass_orders SET checkout_ref = $2 WHERE id = $1", &[
                     &order_id, &session,
                  ])
                  .await?;
                  Ok(())
               })
               .await?;
            Ok(url)
         },
         "nowpayments" => {
            Ok(donations::nowpayments_invoice(
               &keys.nowpayments.api_key,
               order_id,
               cents,
               DESCRIPTION,
               &format!("{origin}/search-pass/nowpayments/ipn"),
               &back,
               &cancel,
            )
            .await?)
         },
         "lzt" => {
            Ok(donations::lzt_invoice(
               &keys.lzt,
               order_id,
               cents,
               DESCRIPTION,
               &back,
               &format!("{origin}/search-pass/lzt/callback"),
            )
            .await?)
         },
         _ => Err(PassError("unknown provider".into())),
      }
   }

   pub async fn order(&self, id: &str) -> Result<Option<Order>, PassError> {
      let row = self
         .with(async |db| {
            Ok(db
               .query_opt(
                  "SELECT provider, email, account_id, renewal, checkout_ref, status FROM \
                   pass_orders WHERE id = $1",
                  &[&id],
               )
               .await?)
         })
         .await?;
      Ok(row.map(|row| {
         Order {
            id:           id.to_owned(),
            provider:     row.get(0),
            email:        row.get(1),
            account_id:   row.get(2),
            renewal:      row.get(3),
            checkout_ref: row.get(4),
            paid:         row.get::<_, &str>(5) == "paid",
         }
      }))
   }

   /// Ask the provider whether a pending card or LZT order was paid. Crypto
   /// is reported by its IPN instead. `mail` sends a new login to the
   /// order's email; leave it off when the buyer is on the page to see it.
   pub async fn check_order(&self, order: &Order, session_id: Option<&str>, mail: bool) -> bool {
      let keys = &self.config.donations;
      match order.provider.as_str() {
         "stripe" => {
            let session = session_id
               .filter(|id| !id.is_empty())
               .unwrap_or(&order.checkout_ref);
            match pay::paid_checkout(&keys.stripe.secret_key, session, &order.id).await {
               Ok(Some(paid)) => {
                  self
                     .settle(
                        &Payment {
                           order_id:     &order.id,
                           provider:     "stripe",
                           reference:    session,
                           cents:        paid.cents,
                           subscription: Some(&paid.subscription),
                        },
                        mail,
                     )
                     .await
               },
               Ok(None) => false,
               Err(err) => {
                  tracing::warn!(order = order.id, "stripe checkout not read: {}", err.0);
                  false
               },
            }
         },
         "lzt" => {
            match donations::lzt_paid_invoice(&keys.lzt.access_token, &order.id).await {
               Ok(Some((cents, reference))) => {
                  self
                     .settle(
                        &Payment {
                           order_id: &order.id,
                           provider: "lzt",
                           reference: &reference,
                           cents,
                           subscription: None,
                        },
                        mail,
                     )
                     .await
               },
               Ok(None) => false,
               Err(err) => {
                  tracing::warn!(order = order.id, "lzt invoice not read: {}", err.0);
                  false
               },
            }
         },
         _ => false,
      }
   }

   /// Record a payment once and say whether its order is paid. `mail` sends
   /// a new login to the order's email.
   pub async fn settle(&self, payment: &Payment<'_>, mail: bool) -> bool {
      match self.record(payment).await {
         Ok(Some(settled)) => {
            if mail && settled.fresh && !settled.renewal && !settled.email.is_empty() {
               self
                  .deliver(settled.account_id, &settled.email, &settled.origin)
                  .await;
            }
            true
         },
         Ok(None) => false,
         Err(err) => {
            tracing::warn!(
               order = payment.order_id,
               "search pass payment not recorded: {err}"
            );
            false
         },
      }
   }

   async fn record(&self, payment: &Payment<'_>) -> Result<Option<Settled>, PassError> {
      let now = now();
      let username = secret::username();
      let days = PASS_DAYS * DAY;
      let settled = self
         .with(async |db| {
            let tx = db.transaction().await?;
            let Some(row) = tx
               .query_opt(
                  "SELECT provider, cents, email, account_id, renewal, status, origin FROM \
                   pass_orders WHERE id = $1 FOR UPDATE",
                  &[&payment.order_id],
               )
               .await?
            else {
               return Ok(None);
            };
            let owed: i64 = row.get(1);
            let email: String = row.get(2);
            let account_id: Option<i64> = row.get(3);
            let renewal: bool = row.get(4);
            let origin: String = row.get(6);
            if row.get::<_, &str>(0) != payment.provider {
               return Ok(None);
            }
            if row.get::<_, &str>(5) == "paid" {
               return Ok(account_id.map(|account_id| {
                  Settled {
                     account_id,
                     fresh: false,
                     renewal,
                     email,
                     origin,
                  }
               }));
            }
            if payment.cents < owed {
               tracing::warn!(
                  order = payment.order_id,
                  paid = payment.cents,
                  owed,
                  "search pass paid short"
               );
               return Ok(None);
            }
            let account_id = match account_id {
               Some(id) if renewal => {
                  tx.execute(
                     "UPDATE pass_accounts SET paid_until = GREATEST(paid_until, $2) + $3 WHERE \
                      id = $1",
                     &[&id, &now, &days],
                  )
                  .await?;
                  id
               },
               _ => {
                  tx.query_one(
                     "INSERT INTO pass_accounts (username, email, paid_until, created_at) VALUES \
                      ($1, $2, $3, $4) RETURNING id",
                     &[&username, &email, &(now + days), &now],
                  )
                  .await?
                  .get(0)
               },
            };
            if let Some(subscription) = payment.subscription.filter(|id| !id.is_empty()) {
               tx.execute(
                  "UPDATE pass_accounts SET stripe_subscription = $2, renews = true WHERE id = $1",
                  &[&account_id, &subscription],
               )
               .await?;
            }
            tx.execute(
               "UPDATE pass_orders SET status = 'paid', provider_ref = $2, paid_at = $3, \
                account_id = $4 WHERE id = $1",
               &[&payment.order_id, &payment.reference, &now, &account_id],
            )
            .await?;
            tx.commit().await?;
            Ok(Some(Settled {
               account_id,
               fresh: true,
               renewal,
               email,
               origin,
            }))
         })
         .await?;
      if settled.as_ref().is_some_and(|settled| settled.fresh) {
         tracing::info!(
            order = payment.order_id,
            provider = payment.provider,
            "search pass paid"
         );
         self.reload().await;
      }
      Ok(settled)
   }

   /// Mail the first login of a paid order to its email, unless it was
   /// already made.
   pub async fn send_login(&self, order: &Order, origin: &str) {
      if let Some(account_id) = order.account_id
         && !order.renewal
         && !order.email.is_empty()
      {
         self.deliver(account_id, &order.email, origin).await;
      }
   }

   /// Make the first password and mail it, unless one was already made.
   async fn deliver(&self, account_id: i64, email: &str, origin: &str) {
      match self.issue_password(account_id).await {
         Ok(Some(login)) => self.mail_login(email, &login, origin),
         Ok(None) => {},
         Err(err) => tracing::warn!(account_id, "search pass login not made: {err}"),
      }
   }

   /// The first password for an account. `None` once one exists, so a page
   /// and a background check cannot each hand out a different one.
   async fn issue_password(&self, account_id: i64) -> Result<Option<Login>, PassError> {
      let password = secret::password();
      let hash = self.hash(password.clone()).await?;
      let row = self
         .with(async |db| {
            Ok(db
               .query_opt(
                  "UPDATE pass_accounts SET password_hash = $2 WHERE id = $1 AND password_hash = \
                   '' RETURNING username, paid_until",
                  &[&account_id, &hash],
               )
               .await?)
         })
         .await?;
      Ok(row.map(|row| {
         Login {
            username: row.get(0),
            password,
            paid_until: row.get(1),
         }
      }))
   }

   fn mail_login(&self, email: &str, login: &Login, origin: &str) {
      let origin = if origin.is_empty() {
         self.config.url_prefix()
      } else {
         origin
      };
      let site = crate::utils::host::host_label(origin);
      let text = format!(
         "Your search pass for {site}:\n\nUsername: {}\nPassword: {}\n\nSign in at \
          {origin}/search-pass/login\nThe pass runs until {}.\n\nSigned in, you can whitelist the \
          IP address of your feed reader so search RSS works there without cookies.\n",
         login.username,
         login.password,
         date(login.paid_until),
      );
      mail::send(
         &self.config.pass,
         email,
         &format!("Your {site} search pass"),
         &text,
      );
   }

   /// Hand the login for a paid order to the browser that started it.
   pub async fn claim(&self, order_id: &str, claim: &str) -> Result<Claim, PassError> {
      if !secret::valid_token(claim) {
         return Ok(Claim::Refused);
      }
      let claim_hash = secret::digest(claim);
      let row = self
         .with(async |db| {
            Ok(db
               .query_opt(
                  "SELECT account_id, email, origin FROM pass_orders WHERE id = $1 AND claim_hash \
                   = $2 AND status = 'paid' AND NOT renewal",
                  &[&order_id, &claim_hash],
               )
               .await?)
         })
         .await?;
      let Some((Some(account_id), email, origin)) = row.map(|row| {
         (
            row.get::<_, Option<i64>>(0),
            row.get::<_, String>(1),
            row.get::<_, String>(2),
         )
      }) else {
         return Ok(Claim::Refused);
      };
      let issued = self.issue_password(account_id).await?;
      let session = self.start_session(account_id).await?;
      if let Some(login) = issued {
         let emailed = !email.is_empty() && self.mail_enabled();
         if emailed {
            self.mail_login(&email, &login, &origin);
         }
         return Ok(Claim::Issued {
            session,
            username: login.username,
            password: login.password,
            paid_until: login.paid_until,
            emailed,
         });
      }
      let Some(account) = self.account(account_id).await? else {
         return Ok(Claim::Refused);
      };
      Ok(Claim::SignedIn {
         session,
         username: account.username,
         paid_until: account.paid_until,
      })
   }

   async fn start_session(&self, account_id: i64) -> Result<String, PassError> {
      let token = secret::token();
      let hash = secret::digest(&token);
      let expires = now() + SESSION_DAYS * DAY;
      self
         .with(async |db| {
            db.execute(
               "INSERT INTO pass_sessions (token_hash, account_id, expires_at) VALUES ($1, $2, $3)",
               &[&hash, &account_id, &expires],
            )
            .await?;
            Ok(())
         })
         .await?;
      self.reload().await;
      Ok(token)
   }

   /// A new session token for a matching username and password.
   pub async fn login(&self, username: &str, password: &str) -> Result<Option<String>, PassError> {
      let username = username.trim().to_ascii_lowercase();
      if username.is_empty() || username.len() > 64 || password.is_empty() || password.len() > 256 {
         return Ok(None);
      }
      let row = self
         .with(async |db| {
            Ok(db
               .query_opt(
                  "SELECT id, password_hash FROM pass_accounts WHERE username = $1",
                  &[&username],
               )
               .await?)
         })
         .await?;
      let Some(row) = row else {
         return Ok(None);
      };
      let id: i64 = row.get(0);
      let stored: String = row.get(1);
      if stored.is_empty() || !self.verify(password.trim().to_owned(), stored).await {
         return Ok(None);
      }
      self.start_session(id).await.map(Some)
   }

   pub async fn logout(&self, jar: &CookieJar) {
      let Some(hash) = session_hash(jar) else {
         return;
      };
      let removed = self
         .with(async |db| {
            db.execute("DELETE FROM pass_sessions WHERE token_hash = $1", &[&hash])
               .await?;
            Ok(())
         })
         .await;
      if let Err(err) = removed {
         tracing::warn!("search pass session not removed: {err}");
      }
      self.reload().await;
   }

   pub async fn account(&self, id: i64) -> Result<Option<Account>, PassError> {
      self
         .with(async |db| {
            let Some(row) = db
               .query_opt(
                  "SELECT username, email, paid_until, renews, stripe_subscription <> '' FROM \
                   pass_accounts WHERE id = $1",
                  &[&id],
               )
               .await?
            else {
               return Ok(None);
            };
            let addresses = db
               .query(
                  "SELECT client_key FROM pass_ips WHERE account_id = $1 ORDER BY created_at",
                  &[&id],
               )
               .await?
               .iter()
               .map(|address| address.get(0))
               .collect();
            Ok(Some(Account {
               username: row.get(0),
               email: row.get(1),
               paid_until: row.get(2),
               renews: row.get(3),
               card: row.get(4),
               addresses,
            }))
         })
         .await
   }

   pub async fn add_address(&self, account_id: i64, key: &str) -> Result<Listing, PassError> {
      let now = now();
      let added = self
         .with(async |db| {
            let tx = db.transaction().await?;
            // Two adds for one account wait on each other, so the cap holds.
            tx.execute("SELECT 1 FROM pass_accounts WHERE id = $1 FOR UPDATE", &[
               &account_id,
            ])
            .await?;
            let listed = tx
               .query("SELECT client_key FROM pass_ips WHERE account_id = $1", &[
                  &account_id,
               ])
               .await?;
            if listed.iter().any(|row| row.get::<_, &str>(0) == key) {
               return Ok(Listing::AlreadyListed);
            }
            if listed.len() >= MAX_ADDRESSES {
               return Ok(Listing::Full);
            }
            tx.execute(
               "INSERT INTO pass_ips (client_key, account_id, created_at) VALUES ($1, $2, $3)",
               &[&key, &account_id, &now],
            )
            .await?;
            tx.commit().await?;
            Ok(Listing::Added)
         })
         .await?;
      if matches!(added, Listing::Added) {
         self.reload().await;
      }
      Ok(added)
   }

   pub async fn remove_address(&self, account_id: i64, key: &str) -> Result<(), PassError> {
      self
         .with(async |db| {
            db.execute(
               "DELETE FROM pass_ips WHERE account_id = $1 AND client_key = $2",
               &[&account_id, &key],
            )
            .await?;
            Ok(())
         })
         .await?;
      self.reload().await;
      Ok(())
   }

   pub async fn set_email(&self, account_id: i64, email: &str) -> Result<(), PassError> {
      self
         .with(async |db| {
            db.execute("UPDATE pass_accounts SET email = $2 WHERE id = $1", &[
               &account_id,
               &email,
            ])
            .await?;
            Ok(())
         })
         .await
   }

   /// Replace the password. Returns the new one, which is shown once.
   pub async fn new_password(&self, account_id: i64) -> Result<String, PassError> {
      let password = secret::password();
      let hash = self.hash(password.clone()).await?;
      self
         .with(async |db| {
            db.execute(
               "UPDATE pass_accounts SET password_hash = $2 WHERE id = $1",
               &[&account_id, &hash],
            )
            .await?;
            Ok(())
         })
         .await?;
      Ok(password)
   }

   /// Mail a fresh password to every pass under `email`. Sessions and
   /// addresses keep working, so a stranger who knows the email only sends
   /// its owner a new login.
   pub async fn forgot(&self, email: &str, origin: &str) -> Result<(), PassError> {
      let now = now();
      let cutoff = now - RESET_GAP;
      let rows = self
         .with(async |db| {
            Ok(db
               .query(
                  "SELECT id FROM pass_accounts WHERE lower(email) = lower($1) AND reset_at <= $2",
                  &[&email, &cutoff],
               )
               .await?)
         })
         .await?;
      for row in rows {
         let id: i64 = row.get(0);
         let password = secret::password();
         let hash = self.hash(password.clone()).await?;
         let updated = self
            .with(async |db| {
               Ok(db
                  .query_opt(
                     "UPDATE pass_accounts SET password_hash = $2, reset_at = $3 WHERE id = $1 \
                      AND reset_at <= $4 RETURNING username, paid_until",
                     &[&id, &hash, &now, &cutoff],
                  )
                  .await?)
            })
            .await?;
         if let Some(account) = updated {
            let login = Login {
               username: account.get(0),
               password,
               paid_until: account.get(1),
            };
            self.mail_login(email, &login, origin);
         }
      }
      Ok(())
   }

   /// Cancel the card subscription at the end of the paid month.
   pub async fn stop_renewal(&self, account_id: i64) -> Result<(), PassError> {
      let subscription: String = self
         .with(async |db| {
            Ok(db
               .query_one(
                  "SELECT stripe_subscription FROM pass_accounts WHERE id = $1",
                  &[&account_id],
               )
               .await?
               .get(0))
         })
         .await?;
      pay::stop_renewal(&self.config.donations.stripe.secret_key, &subscription).await?;
      self
         .with(async |db| {
            db.execute("UPDATE pass_accounts SET renews = false WHERE id = $1", &[
               &account_id,
            ])
            .await?;
            Ok(())
         })
         .await
   }

   async fn hash(&self, password: String) -> Result<String, PassError> {
      let _permit = self
         .hashing
         .acquire()
         .await
         .map_err(|err| PassError(err.to_string()))?;
      tokio::task::spawn_blocking(move || secret::hash_password(&password))
         .await
         .map_err(|err| PassError(err.to_string()))
   }

   async fn verify(&self, password: String, stored: String) -> bool {
      let Ok(_permit) = self.hashing.acquire().await else {
         return false;
      };
      tokio::task::spawn_blocking(move || secret::verify_password(&password, &stored))
         .await
         .unwrap_or(false)
   }

   /// Reload the index, read open checkouts back, follow card renewals and
   /// drop expired sessions.
   pub fn spawn_upkeep(self: &Arc<Self>) {
      let store = Arc::clone(self);
      tokio::spawn(async move {
         let mut round = 0_u64;
         loop {
            tokio::time::sleep(UPKEEP_EVERY).await;
            store.reload().await;
            store.check_pending(round).await;
            if round.is_multiple_of(SLOW_ROUNDS) {
               store.follow_renewals().await;
               store.prune().await;
            }
            round += 1;
         }
      });
   }

   async fn check_pending(&self, round: u64) {
      let now = now();
      let since = now - PENDING_WINDOW;
      let rows = self
         .with(async |db| {
            Ok(db
               .query(
                  "SELECT id, created_at FROM pass_orders WHERE status = 'pending' AND provider \
                   IN ('stripe', 'lzt') AND created_at > $1 ORDER BY created_at DESC LIMIT 200",
                  &[&since],
               )
               .await?)
         })
         .await;
      let rows = match rows {
         Ok(rows) => rows,
         Err(err) => {
            tracing::warn!("open search pass orders not read: {err}");
            return;
         },
      };
      for row in rows {
         let id: String = row.get(0);
         let created: i64 = row.get(1);
         if now - created > HOUR && !round.is_multiple_of(SLOW_ROUNDS) {
            continue;
         }
         if let Ok(Some(order)) = self.order(&id).await {
            self.check_order(&order, None, true).await;
         }
      }
   }

   async fn follow_renewals(&self) {
      let key = &self.config.donations.stripe.secret_key;
      if key.is_empty() {
         return;
      }
      let rows = self
         .with(async |db| {
            Ok(db
               .query(
                  "SELECT id, stripe_subscription FROM pass_accounts WHERE stripe_subscription <> \
                   ''",
                  &[],
               )
               .await?)
         })
         .await;
      let rows = match rows {
         Ok(rows) => rows,
         Err(err) => {
            tracing::warn!("card renewals not read: {err}");
            return;
         },
      };
      for row in rows {
         let id: i64 = row.get(0);
         let subscription: String = row.get(1);
         let renewal = match pay::renewal(key, &subscription).await {
            Ok(renewal) => renewal,
            Err(err) => {
               tracing::warn!(account_id = id, "card renewal not read: {}", err.0);
               continue;
            },
         };
         let saved = match renewal {
            Some(state) if state.active => {
               let until = state.period_end + DAY;
               self
                  .with(async |db| {
                     db.execute(
                        "UPDATE pass_accounts SET paid_until = GREATEST(paid_until, $2), renews = \
                         $3 WHERE id = $1",
                        &[&id, &until, &state.renews],
                     )
                     .await?;
                     Ok(())
                  })
                  .await
            },
            // Past due: Stripe is still retrying the card.
            Some(state) if !state.ended => continue,
            _ => {
               self
                  .with(async |db| {
                     db.execute(
                        "UPDATE pass_accounts SET stripe_subscription = '', renews = false WHERE \
                         id = $1",
                        &[&id],
                     )
                     .await?;
                     Ok(())
                  })
                  .await
            },
         };
         if let Err(err) = saved {
            tracing::warn!(account_id = id, "card renewal not saved: {err}");
         }
      }
      self.reload().await;
   }

   async fn prune(&self) {
      let now = now();
      let pruned = self
         .with(async |db| {
            db.execute("DELETE FROM pass_sessions WHERE expires_at < $1", &[&now])
               .await?;
            Ok(())
         })
         .await;
      if let Err(err) = pruned {
         tracing::warn!("expired search pass sessions not removed: {err}");
      }
   }
}

/// The whitelist entry for a typed address: the address itself, or the /64
/// an IPv6 address sits in, as the request side keys callers.
pub fn address_key(raw: &str) -> Option<String> {
   let ip = raw.trim().parse::<IpAddr>().ok()?.to_canonical();
   public(ip).then(|| ClientKey::from_ip(ip).as_str().to_owned())
}

/// This request's own entry. Loopback and private callers only ever reach
/// this process from the inside.
pub fn current_address() -> Option<String> {
   let client = budget::current_client()?;
   let key = client.as_str();
   let ip = key
      .strip_suffix("/64")
      .unwrap_or(key)
      .parse::<IpAddr>()
      .ok()?;
   public(ip).then(|| key.to_owned())
}

fn public(ip: IpAddr) -> bool {
   match ip {
      IpAddr::V4(v4) => {
         let [first, second, ..] = v4.octets();
         let shared = first == 100 && (64..128).contains(&second);
         !(v4.is_private()
            || v4.is_loopback()
            || v4.is_link_local()
            || v4.is_unspecified()
            || v4.is_broadcast()
            || v4.is_multicast()
            || v4.is_documentation()
            || shared)
      },
      IpAddr::V6(v6) => {
         let [first, second, ..] = v6.segments();
         let unique_local = first & 0xFE00 == 0xFC00;
         let link_local = first & 0xFFC0 == 0xFE80;
         let documentation = first == 0x2001 && second == 0x0DB8;
         !(v6.is_loopback()
            || v6.is_unspecified()
            || v6.is_multicast()
            || unique_local
            || link_local
            || documentation)
      },
   }
}

/// Loose on purpose. It only has to reach the sender as one recipient.
pub fn plausible_email(raw: &str) -> bool {
   raw.len() <= 254
      && !raw.contains(|ch: char| {
         ch.is_whitespace() || ch.is_control() || "<>\"',;:()[]\\".contains(ch)
      })
      && raw.split_once('@').is_some_and(|(local, domain)| {
         !local.is_empty()
            && !domain.contains('@')
            && domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
      })
}

/// `reader@example.com` shown as `r***@example.com`.
pub fn masked_email(email: &str) -> String {
   let Some((local, domain)) = email.split_once('@') else {
      return String::new();
   };
   let first = local.chars().next().map(String::from).unwrap_or_default();
   format!("{first}***@{domain}")
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn whitelist_keys_follow_the_request_side() {
      assert_eq!(address_key(" 203.0.113.9 "), None);
      assert_eq!(address_key("8.8.8.8").as_deref(), Some("8.8.8.8"));
      assert_eq!(address_key("::ffff:8.8.4.4").as_deref(), Some("8.8.4.4"));
      assert_eq!(
         address_key("2a01:4f8:1:2:3:4:5:6").as_deref(),
         Some("2a01:4f8:1:2::/64")
      );
      assert_eq!(address_key("127.0.0.1"), None);
      assert_eq!(address_key("10.1.2.3"), None);
      assert_eq!(address_key("100.64.0.1"), None);
      assert_eq!(address_key("fe80::1"), None);
      assert_eq!(address_key("not an ip"), None);
   }

   #[test]
   fn emails_reach_the_sender_as_one_recipient() {
      assert!(plausible_email("reader@example.com"));
      assert!(!plausible_email("a@b.com, c@d.com"));
      assert!(!plausible_email("a@b.com\nBcc: c@d.com"));
      assert!(!plausible_email("nobody"));
      assert!(!plausible_email("a@localhost"));
      assert_eq!(masked_email("reader@example.com"), "r***@example.com");
   }

   #[test]
   fn a_session_or_address_counts_only_while_paid() {
      let now = 1_000_000;
      let mut index = Index::default();
      index.paid_until.insert(1, now + 10);
      index.paid_until.insert(2, now - 10);
      assert!(index.active(1, now));
      assert!(!index.active(2, now));
      assert!(!index.active(3, now));
   }

   #[test]
   fn dates_read_as_words() {
      assert_eq!(date(1_796_083_200), "December 1, 2026");
   }
}
