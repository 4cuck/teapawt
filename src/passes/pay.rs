//! The card side of a pass: a Stripe subscription that renews every month.
//! Checkouts and renewals are read back from the API, so no webhook is needed.

use serde_json::Value;

use crate::donations::{
   self,
   HTTP,
   PayError,
};

const API: &str = "https://api.stripe.com/v1";

pub struct PaidCheckout {
   pub cents:        i64,
   pub subscription: String,
}

pub struct Renewal {
   /// Paid up, so the pass runs at least to `period_end`.
   pub active:     bool,
   /// Canceled or given up on after failed charges. Nothing more will come.
   pub ended:      bool,
   pub period_end: i64,
   /// Stripe will charge again at `period_end`.
   pub renews:     bool,
}

/// A hosted checkout for a monthly subscription. Returns its URL and id.
pub async fn checkout(
   secret_key: &str,
   order_id: &str,
   cents: i64,
   email: &str,
   success_url: &str,
   cancel_url: &str,
) -> Result<(String, String), PayError> {
   let mut fields = vec![
      ("mode", "subscription".to_owned()),
      ("success_url", success_url.to_owned()),
      ("cancel_url", cancel_url.to_owned()),
      ("client_reference_id", order_id.to_owned()),
      ("metadata[pass_order]", order_id.to_owned()),
      (
         "subscription_data[metadata][pass_order]",
         order_id.to_owned(),
      ),
      ("line_items[0][quantity]", "1".to_owned()),
      ("line_items[0][price_data][currency]", "usd".to_owned()),
      ("line_items[0][price_data][unit_amount]", cents.to_string()),
      (
         "line_items[0][price_data][recurring][interval]",
         "month".to_owned(),
      ),
      (
         "line_items[0][price_data][product_data][name]",
         "Search pass".to_owned(),
      ),
   ];
   if !email.is_empty() {
      fields.push(("customer_email", email.to_owned()));
   }
   let session = post(secret_key, "checkout/sessions", &fields).await?;
   let url = session["url"]
      .as_str()
      .filter(|url| donations::allowed_checkout_url("stripe", url))
      .ok_or_else(|| PayError("stripe did not return a checkout url".into()))?;
   let id = session["id"]
      .as_str()
      .ok_or_else(|| PayError("stripe did not return a checkout id".into()))?;
   Ok((url.to_owned(), id.to_owned()))
}

/// The subscription a checkout started, once it is paid. A session made for
/// another order is ignored, so a paid id cannot be replayed onto this one.
pub async fn paid_checkout(
   secret_key: &str,
   session_id: &str,
   order_id: &str,
) -> Result<Option<PaidCheckout>, PayError> {
   if !session_id.starts_with("cs_") || session_id.len() > 255 {
      return Ok(None);
   }
   let Some(session) = get(
      secret_key,
      &format!("checkout/sessions/{}", encode(session_id)),
   )
   .await?
   else {
      return Ok(None);
   };
   if session["mode"] != "subscription"
      || session["payment_status"] != "paid"
      || session["metadata"]["pass_order"] != order_id
   {
      return Ok(None);
   }
   Ok(Some(PaidCheckout {
      cents:        session["amount_total"].as_i64().unwrap_or(0),
      subscription: session["subscription"]
         .as_str()
         .unwrap_or_default()
         .to_owned(),
   }))
}

/// Where a subscription stands. `None` when Stripe no longer knows it.
pub async fn renewal(secret_key: &str, subscription: &str) -> Result<Option<Renewal>, PayError> {
   if !subscription.starts_with("sub_") {
      return Ok(None);
   }
   let Some(sub) = get(
      secret_key,
      &format!("subscriptions/{}", encode(subscription)),
   )
   .await?
   else {
      return Ok(None);
   };
   let status = sub["status"].as_str().unwrap_or_default();
   // API versions from 2025 keep the period on the subscription item.
   let period_end = sub["current_period_end"]
      .as_i64()
      .or_else(|| sub["items"]["data"][0]["current_period_end"].as_i64())
      .unwrap_or(0);
   Ok(Some(Renewal {
      active: matches!(status, "active" | "trialing"),
      ended: matches!(status, "canceled" | "unpaid" | "incomplete_expired"),
      period_end,
      renews: !sub["cancel_at_period_end"].as_bool().unwrap_or(false) && sub["cancel_at"].is_null(),
   }))
}

/// Let the paid month run out without another charge.
pub async fn stop_renewal(secret_key: &str, subscription: &str) -> Result<(), PayError> {
   if !subscription.starts_with("sub_") {
      return Err(PayError("not a subscription id".into()));
   }
   post(
      secret_key,
      &format!("subscriptions/{}", encode(subscription)),
      &[("cancel_at_period_end", "true".to_owned())],
   )
   .await
   .map(|_| ())
}

async fn post(secret_key: &str, path: &str, fields: &[(&str, String)]) -> Result<Value, PayError> {
   let body = form_urlencoded::Serializer::new(String::new())
      .extend_pairs(fields.iter().map(|(key, value)| (*key, value.as_str())))
      .finish();
   let response = HTTP
      .post(format!("{API}/{path}"))
      .bearer_auth(secret_key)
      .header("content-type", "application/x-www-form-urlencoded")
      .body(body)
      .send()
      .await
      .map_err(|err| PayError(format!("stripe request failed: {err}")))?;
   let status = response.status();
   let payload: Value = response.json().await.unwrap_or(Value::Null);
   if !status.is_success() {
      let message = payload["error"]["message"]
         .as_str()
         .unwrap_or("Stripe refused the request");
      return Err(PayError(format!("stripe {status}: {message}")));
   }
   Ok(payload)
}

async fn get(secret_key: &str, path: &str) -> Result<Option<Value>, PayError> {
   let response = HTTP
      .get(format!("{API}/{path}"))
      .bearer_auth(secret_key)
      .send()
      .await
      .map_err(|err| PayError(format!("stripe request failed: {err}")))?;
   let status = response.status();
   if status == reqwest::StatusCode::NOT_FOUND {
      return Ok(None);
   }
   if !status.is_success() {
      return Err(PayError(format!("stripe {status}")));
   }
   Ok(Some(response.json().await.unwrap_or(Value::Null)))
}

fn encode(value: &str) -> String {
   form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
