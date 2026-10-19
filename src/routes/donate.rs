//! Donation checkouts. Provider secrets are read from config and never rendered.

use axum::{
   Form, Router,
   body::Bytes,
   extract::State,
   http::{HeaderMap, StatusCode, header},
   response::{Html, IntoResponse, Redirect, Response},
   routing::{get, post},
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use serde_json::Value;

use crate::{
   AppState,
   donations::{self, PayError},
   types::Prefs,
   views::{donate as donate_view, layout::PageLayout},
};

pub fn router() -> Router<AppState> {
   Router::new()
      .route("/donate", get(donate_page).post(start_donation))
      .route("/donate/thanks", get(thanks))
      .route("/donate/stripe/webhook", post(stripe_webhook))
      .route("/donate/nowpayments/ipn", post(nowpayments_ipn))
      .route("/donate/lzt/callback", post(lzt_callback))
}

#[derive(Debug, Deserialize)]
struct DonateForm {
   amount: String,
   provider: String,
}

#[derive(Debug, Deserialize)]
struct DonateQuery {
   canceled: Option<String>,
   error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ThanksQuery {
   provider: Option<String>,
   session_id: Option<String>,
   payment_id: Option<String>,
}

async fn donate_page(
   State(state): State<AppState>,
   jar: CookieJar,
   axum::extract::Query(query): axum::extract::Query<DonateQuery>,
) -> Response {
   page(
      &state,
      &jar,
      query.error.as_deref(),
      query.canceled.as_deref() == Some("1"),
   )
   .await
}

async fn page(state: &AppState, jar: &CookieJar, message: Option<&str>, canceled: bool) -> Response {
   let prefs = Prefs::from_cookies(jar, &state.config);
   let progress = donations::month_progress(&state.donations, state.config.donations.goal_cents).await;
   let donations = &state.config.donations;
   let content = donate_view::render_donate_page(
      &progress,
      donations.stripe.configured(),
      donations.nowpayments.configured(),
      donations.lzt.configured(),
      message,
      canceled,
   );
   let markup = PageLayout::new(&state.config, "Donate", content)
      .description("Donate to keep xitter.cf online")
      .prefs(&prefs)
      .referer("/donate")
      .render();
   let mut response = Html(markup.into_string()).into_response();
   response.headers_mut().insert(
      header::CACHE_CONTROL,
      header::HeaderValue::from_static("private, no-store"),
   );
   response
}

async fn start_donation(
   State(state): State<AppState>,
   jar: CookieJar,
   Form(form): Form<DonateForm>,
) -> Response {
   let cents = match donations::parse_amount_cents(&form.amount) {
      Ok(cents) => cents,
      Err(message) => return page(&state, &jar, Some(message), false).await,
   };
   let provider = form.provider.as_str();
   if !donations::provider_ready(&state.config.donations, provider) {
      return page(&state, &jar, Some("That payment method is not available."), false).await;
   }
   let id = donations::new_donation_id();
   let base = state.config.url_prefix();
   let cancel = format!("{base}/donate?canceled=1");
   let result = match provider {
      "stripe" => {
         let success = format!(
            "{base}/donate/thanks?provider=stripe&session_id={{CHECKOUT_SESSION_ID}}"
         );
         donations::stripe_checkout(
            &state.config.donations.stripe.secret_key,
            &id,
            cents,
            &success,
            &cancel,
         )
         .await
      },
      "nowpayments" => donations::nowpayments_invoice(
         &state.config.donations.nowpayments.api_key,
         &id,
         cents,
         &format!("{base}/donate/nowpayments/ipn"),
         &format!("{base}/donate/thanks?provider=nowpayments"),
         &cancel,
      )
      .await,
      "lzt" => donations::lzt_invoice(
         &state.config.donations.lzt,
         &id,
         cents,
         &format!("{base}/donate/thanks?provider=lzt&payment_id={id}"),
         &format!("{base}/donate/lzt/callback"),
      )
      .await,
      _ => Err(PayError("unknown provider".into())),
   };
   match result {
      Ok(url) => {
         state.donations.remember_pending(&id, provider, cents).await;
         Redirect::to(&url).into_response()
      },
      Err(err) => {
         tracing::warn!(provider, "donation checkout failed: {}", err.0);
         page(
            &state,
            &jar,
            Some("The payment service did not start a checkout. Try again, or use another method."),
            false,
         )
         .await
      },
   }
}

async fn thanks(
   State(state): State<AppState>,
   jar: CookieJar,
   axum::extract::Query(query): axum::extract::Query<ThanksQuery>,
) -> Response {
   let provider = query.provider.unwrap_or_default();
   let mut confirmed = false;
   if provider == "stripe"
      && let Some(session_id) = query.session_id.as_deref()
      && let Ok(Some((id, cents, reference))) = donations::stripe_paid_session(
         &state.config.donations.stripe.secret_key,
         session_id,
      )
      .await
   {
      state.donations.mark_paid(&id, "stripe", &reference, cents).await;
      confirmed = true;
   }
   if provider == "lzt"
      && let Some(payment_id) = query.payment_id.as_deref()
      && let Ok(Some((cents, reference))) =
         donations::lzt_paid_invoice(&state.config.donations.lzt.access_token, payment_id).await
      && cents > 0
   {
      state
         .donations
         .mark_paid(payment_id, "lzt", &reference, cents)
         .await;
      confirmed = true;
   }
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let content = donate_view::render_thanks(confirmed, &provider);
   let markup = PageLayout::new(&state.config, "Thank you", content)
      .description("Thank you for donating to xitter.cf")
      .prefs(&prefs)
      .referer("/donate")
      .render();
   Html(markup.into_string()).into_response()
}

async fn stripe_webhook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
   let secret = &state.config.donations.stripe.webhook_secret;
   if secret.is_empty() {
      return StatusCode::SERVICE_UNAVAILABLE.into_response();
   }
   let signature = headers
      .get("stripe-signature")
      .and_then(|value| value.to_str().ok())
      .unwrap_or("");
   if !donations::stripe_signature_ok(&body, signature, secret) {
      return StatusCode::BAD_REQUEST.into_response();
   }
   let Ok(event) = serde_json::from_slice::<Value>(&body) else {
      return StatusCode::BAD_REQUEST.into_response();
   };
   if event["type"] != "checkout.session.completed" {
      return StatusCode::OK.into_response();
   }
   let session = &event["data"]["object"];
   if session["payment_status"] != "paid" {
      return StatusCode::OK.into_response();
   }
   let id = session["metadata"]["donation_id"]
      .as_str()
      .or_else(|| session["client_reference_id"].as_str())
      .unwrap_or("");
   let cents = session["amount_total"].as_i64().unwrap_or(0);
   let reference = session["id"].as_str().unwrap_or("");
   if donations::valid_donation_id(id) && cents > 0 && !reference.is_empty() {
      state.donations.mark_paid(id, "stripe", reference, cents).await;
      tracing::info!(donation_id = id, cents, "stripe donation paid");
   }
   StatusCode::OK.into_response()
}

async fn nowpayments_ipn(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
   let secret = &state.config.donations.nowpayments.ipn_secret;
   if secret.is_empty() {
      return StatusCode::SERVICE_UNAVAILABLE.into_response();
   }
   let signature = headers
      .get("x-nowpayments-sig")
      .and_then(|value| value.to_str().ok())
      .unwrap_or("");
   if !donations::nowpayments_signature_ok(&body, signature, secret) {
      tracing::warn!("nowpayments ipn rejected");
      return StatusCode::UNAUTHORIZED.into_response();
   }
   let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
      return StatusCode::BAD_REQUEST.into_response();
   };
   let Some(cents) = donations::nowpayments_received_cents(&payload) else {
      return StatusCode::OK.into_response();
   };
   let id = payload["order_id"].as_str().unwrap_or("");
   let reference = payload["payment_id"]
      .as_i64()
      .map(|value| value.to_string())
      .or_else(|| payload["payment_id"].as_str().map(str::to_owned))
      .unwrap_or_default();
   if donations::valid_donation_id(id) && !reference.is_empty() {
      state
         .donations
         .mark_paid(id, "nowpayments", &reference, cents)
         .await;
      tracing::info!(donation_id = id, cents, "nowpayments donation received");
   }
   StatusCode::OK.into_response()
}

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
      tracing::warn!("lzt callback rejected");
      return StatusCode::UNAUTHORIZED.into_response();
   }
   let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
      return StatusCode::BAD_REQUEST.into_response();
   };
   let invoice = if payload.get("invoice").is_some() {
      &payload["invoice"]
   } else {
      &payload
   };
   let status = invoice["status"].as_str().unwrap_or("").to_ascii_lowercase();
   if status != "paid" {
      return StatusCode::OK.into_response();
   }
   let id = invoice["payment_id"].as_str().unwrap_or("");
   let reference = invoice["invoice_id"]
      .as_i64()
      .map(|value| value.to_string())
      .or_else(|| invoice["invoice_id"].as_str().map(str::to_owned))
      .unwrap_or_else(|| id.to_owned());
   let mut cents = donations::dollars_value_to_cents(invoice.get("amount").unwrap_or(&Value::Null));
   if cents <= 0 {
      cents = state.donations.cents_for(id).await.unwrap_or(0);
   }
   if donations::valid_donation_id(id) && cents > 0 {
      state.donations.mark_paid(id, "lzt", &reference, cents).await;
      tracing::info!(donation_id = id, cents, "lzt donation paid");
   }
   StatusCode::OK.into_response()
}
