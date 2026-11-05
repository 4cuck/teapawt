//! Search pass pages, checkout, and the crypto and LZT payment notices.

use axum::{
   Form,
   Router,
   body::Bytes,
   extract::{
      Query,
      State,
   },
   handler::Handler,
   http::{
      HeaderMap,
      HeaderValue,
      StatusCode,
      header,
   },
   middleware,
   response::{
      Html,
      IntoResponse as _,
      Redirect,
      Response,
   },
   routing::{
      MethodRouter,
      get,
      post,
   },
};
use axum_extra::extract::{
   CookieJar,
   cookie::{
      Cookie,
      SameSite,
   },
};
use maud::{
   Markup,
   html,
};
use serde::Deserialize;
use serde_json::Value;
use time::Duration;

use crate::{
   AppState,
   donations,
   passes::{
      self,
      Account,
      Claim,
      Listing,
      NewOrder,
      Payment,
   },
   routes::middleware::same_origin_guard,
   types::Prefs,
   utils::host::current_origin,
   views::{
      layout::PageLayout,
      pass as view,
   },
};

pub fn router() -> Router<AppState> {
   Router::new()
      .route("/search-pass", get(page))
      .route("/search-pass/buy", guarded(buy))
      .route("/search-pass/return", get(back_from_checkout))
      .route("/search-pass/login", guarded(login).get(login_page))
      .route("/search-pass/logout", guarded(logout))
      .route("/search-pass/forgot", guarded(forgot).get(forgot_page))
      .route("/search-pass/ips", guarded(addresses))
      .route("/search-pass/email", guarded(email))
      .route("/search-pass/password", guarded(new_password))
      .route("/search-pass/renewal", guarded(stop_renewal))
      .route("/search-pass/nowpayments/ipn", post(nowpayments_ipn))
      .route("/search-pass/lzt/callback", post(lzt_callback))
}

/// A form post that another site cannot submit.
fn guarded<H, T>(handler: H) -> MethodRouter<AppState>
where
   H: Handler<T, AppState>,
   T: 'static,
{
   post(handler).route_layer(middleware::from_fn(same_origin_guard))
}

fn now() -> i64 {
   time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Logins and account pages must never be stored by Cloudflare or a shared
/// cache.
fn private(mut response: Response) -> Response {
   let headers = response.headers_mut();
   headers.insert(
      header::CACHE_CONTROL,
      HeaderValue::from_static("private, no-store"),
   );
   headers.insert(
      "cloudflare-cdn-cache-control",
      HeaderValue::from_static("no-store"),
   );
   headers.insert("cdn-cache-control", HeaderValue::from_static("no-store"));
   response
}

fn see(path: &str) -> Response {
   private(Redirect::to(path).into_response())
}

fn see_note(key: &str) -> Response {
   see(&format!("/search-pass?note={key}"))
}

fn note(key: &str) -> Option<&'static str> {
   Some(match key {
      "canceled" => "The payment was canceled. Nothing was charged.",
      "method" => "That payment method is not available.",
      "cookies" => {
         "This browser is blocking cookies. An anonymous pass lives in this browser's cookies, so \
          allow cookies for this site or enter an email."
      },
      "checkout" => {
         "The payment service did not start a checkout. Try again, or use another method."
      },
      "busy" => "Too many checkouts from this address. Try again in a few minutes.",
      "renews" => "This pass already renews by card.",
      "added" => "Address added.",
      "removed" => "Address removed.",
      "listed" => "That address is already on the list.",
      "full" => "This pass has as many addresses as it can hold. Remove one first.",
      "bad-ip" => "That is not a public IPv4 or IPv6 address.",
      "email" => "Recovery email saved.",
      "no-email" => "Recovery email removed.",
      "bad-email" => "That email address does not look right.",
      "stopped" => "Card renewal stopped. The pass runs until the date above.",
      "stop-failed" => "Stripe did not stop the renewal. Try again in a minute.",
      "unavailable" => "Something went wrong on our side. Try again in a minute.",
      _ => return None,
   })
}

fn cookie(
   state: &AppState,
   name: &'static str,
   value: String,
   path: &'static str,
   max_age: Duration,
) -> Cookie<'static> {
   Cookie::build((name, value))
      .path(path)
      .http_only(true)
      .secure(state.config.server.https)
      .same_site(SameSite::Lax)
      .max_age(max_age)
      .build()
}

fn session_cookie(state: &AppState, token: String) -> Cookie<'static> {
   cookie(
      state,
      passes::SESSION_COOKIE,
      token,
      "/",
      Duration::days(passes::SESSION_DAYS),
   )
}

/// `(order id, claim token)` from the buyer's cookie.
fn order_cookie(jar: &CookieJar) -> Option<(&str, &str)> {
   let (order, claim) = jar.get(passes::ORDER_COOKIE)?.value().split_once('.')?;
   (passes::valid_order_id(order) && passes::valid_token(claim)).then_some((order, claim))
}

fn methods(state: &AppState) -> view::Methods {
   let keys = &state.config.donations;
   view::Methods {
      card:   keys.stripe.configured(),
      crypto: keys.nowpayments.configured(),
      lzt:    keys.lzt.configured(),
   }
}

fn render(state: &AppState, jar: &CookieJar, title: &str, content: Markup) -> Response {
   let prefs = Prefs::from_cookies(jar, &state.config);
   let markup = PageLayout::new(&state.config, title, content)
      .description("Skip the search checkbox on search and search RSS feeds")
      .prefs(&prefs)
      .render();
   private(Html(markup.into_string()).into_response())
}

#[derive(Deserialize)]
struct PageQuery {
   note: Option<String>,
}

async fn page(
   State(state): State<AppState>,
   jar: CookieJar,
   Query(query): Query<PageQuery>,
) -> Response {
   let message = query.note.as_deref().and_then(note);
   if let Some(account_id) = state.passes.signed_in(&jar) {
      match state.passes.account(account_id).await {
         Ok(Some(account)) => return account_page(&state, &jar, &account, message),
         Ok(None) => {},
         Err(err) => {
            tracing::warn!("search pass account not read: {err}");
            return buy_page(&state, jar, note("unavailable"));
         },
      }
   }
   // A paid order whose login was never collected: the tab was closed, or
   // the crypto confirmed later.
   if let Some((order_id, _)) = order_cookie(&jar)
      && let Ok(Some(order)) = state.passes.order(order_id).await
      && order.paid
      && !order.renewal
   {
      return see(&format!("/search-pass/return?order={order_id}"));
   }
   buy_page(&state, jar, message)
}

fn buy_page(state: &AppState, jar: CookieJar, message: Option<&str>) -> Response {
   let price = donations::format_usd(state.passes.price_cents());
   let methods = methods(state);
   let content = view::render_buy(&view::BuyPage {
      price: &price,
      methods: &methods,
      mail: state.passes.mail_enabled(),
      message,
   });
   let jar = jar.add(cookie(
      state,
      passes::CHECK_COOKIE,
      "1".to_owned(),
      "/search-pass",
      Duration::days(1),
   ));
   let response = render(state, &jar, "Search pass", content);
   private((jar, response).into_response())
}

fn account_page(
   state: &AppState,
   jar: &CookieJar,
   account: &Account,
   message: Option<&str>,
) -> Response {
   let price = donations::format_usd(state.passes.price_cents());
   let methods = methods(state);
   let paid_until = passes::date(account.paid_until);
   let current = passes::current_address();
   let content = view::render_account(&view::AccountPage {
      username: &account.username,
      email: &account.email,
      paid_until: &paid_until,
      active: account.paid_until > now(),
      renews: account.renews,
      card: account.card,
      addresses: &account.addresses,
      current: current.as_deref(),
      max_addresses: passes::MAX_ADDRESSES,
      price: &price,
      methods: &methods,
      mail: state.passes.mail_enabled(),
      message,
   });
   render(state, jar, "Your search pass", content)
}

/// Back to the account page when signed in, otherwise the purchase page.
fn refuse(state: &AppState, jar: CookieJar, signed_in: bool, key: &str) -> Response {
   if signed_in {
      return see_note(key);
   }
   buy_page(state, jar, note(key))
}

#[derive(Deserialize)]
struct BuyForm {
   #[serde(default)]
   provider: String,
   #[serde(default)]
   email:    String,
}

async fn buy(State(state): State<AppState>, jar: CookieJar, Form(form): Form<BuyForm>) -> Response {
   let provider = form.provider.as_str();
   let account_id = state.passes.signed_in(&jar);
   let signed_in = account_id.is_some();
   if !donations::provider_ready(&state.config.donations, provider) {
      return refuse(&state, jar, signed_in, "method");
   }
   let email = if signed_in { "" } else { form.email.trim() };
   if let Some(id) = account_id {
      if provider == "stripe"
         && matches!(state.passes.account(id).await, Ok(Some(account)) if account.renews)
      {
         return see_note("renews");
      }
   } else if !email.is_empty() && !passes::plausible_email(email) {
      return refuse(&state, jar, false, "bad-email");
   } else if email.is_empty() && jar.get(passes::CHECK_COOKIE).is_none() {
      return refuse(&state, jar, false, "cookies");
   }
   if !state.passes.allow_checkout() {
      return refuse(&state, jar, signed_in, "busy");
   }
   let origin = current_origin(state.config.url_prefix());
   let order = NewOrder {
      provider,
      email,
      account_id,
      origin: &origin,
   };
   let (order_id, claim) = match state.passes.create_order(&order).await {
      Ok(created) => created,
      Err(err) => {
         tracing::warn!("search pass order not stored: {err}");
         return refuse(&state, jar, signed_in, "unavailable");
      },
   };
   match state
      .passes
      .checkout(&order_id, provider, email, &origin)
      .await
   {
      Ok(url) => {
         let jar = jar.add(cookie(
            &state,
            passes::ORDER_COOKIE,
            format!("{order_id}.{claim}"),
            "/search-pass",
            Duration::days(7),
         ));
         private((jar, Redirect::to(&url)).into_response())
      },
      Err(err) => {
         tracing::warn!(provider, "search pass checkout failed: {err}");
         refuse(&state, jar, signed_in, "checkout")
      },
   }
}

#[derive(Deserialize)]
struct ReturnQuery {
   order:      Option<String>,
   session_id: Option<String>,
}

fn show_return(state: &AppState, jar: CookieJar, page: &view::Return<'_>) -> Response {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let refresh = html! { meta http-equiv="refresh" content="10"; };
   let mut layout = PageLayout::new(&state.config, "Search pass", view::render_return(page))
      .description("Search pass checkout")
      .prefs(&prefs);
   if matches!(page, view::Return::Waiting) {
      layout = layout.head_extra(&refresh);
   }
   private((jar, Html(layout.render().into_string())).into_response())
}

async fn back_from_checkout(
   State(state): State<AppState>,
   jar: CookieJar,
   Query(query): Query<ReturnQuery>,
) -> Response {
   let Some(order_id) = query.order.filter(|id| passes::valid_order_id(id)) else {
      return show_return(&state, jar, &view::Return::Unknown);
   };
   let mut order = match state.passes.order(&order_id).await {
      Ok(Some(order)) => order,
      Ok(None) => return show_return(&state, jar, &view::Return::Unknown),
      Err(err) => {
         tracing::warn!("search pass order not read: {err}");
         return refuse(&state, jar, false, "unavailable");
      },
   };
   // The buyer is here to see the login, so it is not mailed from this check.
   if !order.paid
      && state
         .passes
         .check_order(&order, query.session_id.as_deref(), false)
         .await
   {
      match state.passes.order(&order_id).await {
         Ok(Some(paid)) => order = paid,
         _ => return refuse(&state, jar, false, "unavailable"),
      }
   }
   if !order.paid {
      return show_return(&state, jar, &view::Return::Waiting);
   }

   let claim = order_cookie(&jar)
      .filter(|(id, _)| *id == order.id)
      .map(|(_, claim)| claim.to_owned());
   let jar = jar.remove(Cookie::build((passes::ORDER_COOKIE, "")).path("/search-pass"));
   if order.renewal {
      let paid_until = match order.account_id {
         Some(id) => state.passes.account(id).await.ok().flatten(),
         None => None,
      }
      .map(|account| passes::date(account.paid_until))
      .unwrap_or_default();
      return show_return(&state, jar, &view::Return::Renewed {
         paid_until: &paid_until,
      });
   }

   if let Some(claim) = claim {
      match state.passes.claim(&order.id, &claim).await {
         Ok(Claim::Issued {
            session,
            username,
            password,
            paid_until,
            emailed,
         }) => {
            let jar = jar.add(session_cookie(&state, session));
            let paid_until = passes::date(paid_until);
            let to = emailed.then(|| passes::masked_email(&order.email));
            return show_return(&state, jar, &view::Return::Issued {
               username:   &username,
               password:   &password,
               paid_until: &paid_until,
               emailed:    to.as_deref(),
            });
         },
         Ok(Claim::SignedIn {
            session,
            username,
            paid_until,
         }) => {
            let jar = jar.add(session_cookie(&state, session));
            let paid_until = passes::date(paid_until);
            return show_return(&state, jar, &view::Return::SignedIn {
               username:   &username,
               paid_until: &paid_until,
            });
         },
         Ok(Claim::Refused) => {},
         Err(err) => {
            tracing::warn!("search pass login not handed over: {err}");
            return refuse(&state, jar, false, "unavailable");
         },
      }
   }

   if !order.email.is_empty() && state.passes.mail_enabled() {
      let origin = current_origin(state.config.url_prefix());
      state.passes.send_login(&order, &origin).await;
      let to = passes::masked_email(&order.email);
      return show_return(&state, jar, &view::Return::Emailed { to: &to });
   }
   show_return(&state, jar, &view::Return::OtherBrowser)
}

async fn login_page(State(state): State<AppState>, jar: CookieJar) -> Response {
   if state.passes.signed_in(&jar).is_some() {
      return see("/search-pass");
   }
   render(&state, &jar, "Log in", view::render_login(None))
}

#[derive(Deserialize)]
struct LoginForm {
   #[serde(default)]
   username: String,
   #[serde(default)]
   password: String,
}

async fn login(
   State(state): State<AppState>,
   jar: CookieJar,
   Form(form): Form<LoginForm>,
) -> Response {
   match state.passes.login(&form.username, &form.password).await {
      Ok(Some(session)) => {
         let jar = jar.add(session_cookie(&state, session));
         private((jar, Redirect::to("/search-pass")).into_response())
      },
      Ok(None) => {
         render(
            &state,
            &jar,
            "Log in",
            view::render_login(Some("That username and password do not match.")),
         )
      },
      Err(err) => {
         tracing::warn!("search pass login failed: {err}");
         render(
            &state,
            &jar,
            "Log in",
            view::render_login(note("unavailable")),
         )
      },
   }
}

async fn logout(State(state): State<AppState>, jar: CookieJar) -> Response {
   state.passes.logout(&jar).await;
   let jar = jar.remove(Cookie::build((passes::SESSION_COOKIE, "")).path("/"));
   private((jar, Redirect::to("/search-pass")).into_response())
}

async fn forgot_page(State(state): State<AppState>, jar: CookieJar) -> Response {
   render(
      &state,
      &jar,
      "Forgot your login",
      view::render_forgot(state.passes.mail_enabled(), false),
   )
}

#[derive(Deserialize)]
struct EmailForm {
   #[serde(default)]
   email: String,
}

async fn forgot(
   State(state): State<AppState>,
   jar: CookieJar,
   Form(form): Form<EmailForm>,
) -> Response {
   let email = form.email.trim();
   if state.passes.mail_enabled() && passes::plausible_email(email) {
      let origin = current_origin(state.config.url_prefix());
      if let Err(err) = state.passes.forgot(email, &origin).await {
         tracing::warn!("search pass reset not sent: {err}");
      }
   }
   render(
      &state,
      &jar,
      "Forgot your login",
      view::render_forgot(state.passes.mail_enabled(), true),
   )
}

#[derive(Deserialize)]
struct AddressForm {
   #[serde(default)]
   action: String,
   #[serde(default)]
   ip:     String,
}

async fn addresses(
   State(state): State<AppState>,
   jar: CookieJar,
   Form(form): Form<AddressForm>,
) -> Response {
   let Some(account_id) = state.passes.signed_in(&jar) else {
      return see("/search-pass/login");
   };
   let key = match form.action.as_str() {
      "current" => passes::current_address(),
      "add" => passes::address_key(&form.ip),
      "remove" => {
         return match state
            .passes
            .remove_address(account_id, form.ip.trim())
            .await
         {
            Ok(()) => see_note("removed"),
            Err(err) => {
               tracing::warn!("search pass address not removed: {err}");
               see_note("unavailable")
            },
         };
      },
      _ => None,
   };
   let Some(key) = key else {
      return see_note("bad-ip");
   };
   match state.passes.add_address(account_id, &key).await {
      Ok(Listing::Added) => see_note("added"),
      Ok(Listing::AlreadyListed) => see_note("listed"),
      Ok(Listing::Full) => see_note("full"),
      Err(err) => {
         tracing::warn!("search pass address not added: {err}");
         see_note("unavailable")
      },
   }
}

async fn email(
   State(state): State<AppState>,
   jar: CookieJar,
   Form(form): Form<EmailForm>,
) -> Response {
   let Some(account_id) = state.passes.signed_in(&jar) else {
      return see("/search-pass/login");
   };
   let email = form.email.trim();
   if !email.is_empty() && !passes::plausible_email(email) {
      return see_note("bad-email");
   }
   match state.passes.set_email(account_id, email).await {
      Ok(()) => {
         see_note(if email.is_empty() {
            "no-email"
         } else {
            "email"
         })
      },
      Err(err) => {
         tracing::warn!("search pass email not saved: {err}");
         see_note("unavailable")
      },
   }
}

async fn new_password(State(state): State<AppState>, jar: CookieJar) -> Response {
   let Some(account_id) = state.passes.signed_in(&jar) else {
      return see("/search-pass/login");
   };
   let account = match state.passes.account(account_id).await {
      Ok(Some(account)) => account,
      Ok(None) => return see("/search-pass/login"),
      Err(err) => {
         tracing::warn!("search pass account not read: {err}");
         return see_note("unavailable");
      },
   };
   match state.passes.new_password(account_id).await {
      Ok(password) => {
         render(
            &state,
            &jar,
            "New password",
            view::render_new_password(&account.username, &password),
         )
      },
      Err(err) => {
         tracing::warn!("search pass password not changed: {err}");
         see_note("unavailable")
      },
   }
}

async fn stop_renewal(State(state): State<AppState>, jar: CookieJar) -> Response {
   let Some(account_id) = state.passes.signed_in(&jar) else {
      return see("/search-pass/login");
   };
   match state.passes.stop_renewal(account_id).await {
      Ok(()) => see_note("stopped"),
      Err(err) => {
         tracing::warn!("card renewal not stopped: {err}");
         see_note("stop-failed")
      },
   }
}

async fn nowpayments_ipn(
   State(state): State<AppState>,
   headers: HeaderMap,
   body: Bytes,
) -> Response {
   let secret = &state.config.donations.nowpayments.ipn_secret;
   if secret.is_empty() {
      return StatusCode::SERVICE_UNAVAILABLE.into_response();
   }
   let signature = headers
      .get("x-nowpayments-sig")
      .and_then(|value| value.to_str().ok())
      .unwrap_or("");
   if !donations::nowpayments_signature_ok(&body, signature, secret) {
      tracing::warn!("search pass nowpayments ipn rejected");
      return StatusCode::UNAUTHORIZED.into_response();
   }
   let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
      return StatusCode::BAD_REQUEST.into_response();
   };
   let order_id = payload["order_id"].as_str().unwrap_or("");
   let status = payload["payment_status"]
      .as_str()
      .unwrap_or("")
      .to_ascii_lowercase();
   // Coins that are only seen on the chain can still be double spent.
   if !passes::valid_order_id(order_id)
      || !matches!(status.as_str(), "confirmed" | "sending" | "finished")
   {
      return StatusCode::OK.into_response();
   }
   let reference = payload["payment_id"]
      .as_i64()
      .map(|id| id.to_string())
      .or_else(|| payload["payment_id"].as_str().map(str::to_owned))
      .unwrap_or_default();
   if let Some(cents) = donations::nowpayments_received_cents(&payload)
      && !reference.is_empty()
   {
      state
         .passes
         .settle(
            &Payment {
               order_id,
               provider: "nowpayments",
               reference: &reference,
               cents,
               subscription: None,
            },
            true,
         )
         .await;
   }
   StatusCode::OK.into_response()
}

/// Taken as a hint: the invoice is read back from LZT before it counts.
async fn lzt_callback(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
   let secret = &state.config.donations.lzt.secret_key;
   if secret.is_empty() {
      return StatusCode::SERVICE_UNAVAILABLE.into_response();
   }
   let provided = headers
      .get("x-secret-key")
      .and_then(|value| value.to_str().ok())
      .unwrap_or("");
   if !donations::lzt_secret_ok(provided, secret) {
      tracing::warn!("search pass lzt callback rejected");
      return StatusCode::UNAUTHORIZED.into_response();
   }
   let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
      return StatusCode::BAD_REQUEST.into_response();
   };
   let invoice = payload.get("invoice").unwrap_or(&payload);
   let order_id = invoice["payment_id"].as_str().unwrap_or("");
   if passes::valid_order_id(order_id)
      && let Ok(Some(order)) = state.passes.order(order_id).await
      && !order.paid
   {
      state.passes.check_order(&order, None, true).await;
   }
   StatusCode::OK.into_response()
}
