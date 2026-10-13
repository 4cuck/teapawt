//! Card, crypto, and LZT Market donations.
//!
//! Secrets stay in the server config or the environment. Nothing here prints them.

use std::{
   path::PathBuf,
   sync::{Arc, LazyLock},
   time::Duration,
};

use data_encoding::HEXLOWER;
use ring::{
   hmac,
   rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::config::DonationsConfig;

pub const MIN_CENTS: i64 = 100;
pub const MAX_CENTS: i64 = 1_000_000;

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
   reqwest::Client::builder()
      .timeout(Duration::from_secs(20))
      .build()
      .expect("payment client")
});

#[derive(Debug)]
pub struct PayError(pub String);

impl PayError {
   fn new(message: impl Into<String>) -> Self {
      Self(message.into())
   }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
   id: String,
   provider: String,
   #[serde(default)]
   provider_ref: String,
   cents: i64,
   status: String,
}

#[derive(Default, Serialize, Deserialize)]
struct LedgerFile {
   entries: Vec<Entry>,
}

pub struct DonationLedger {
   path: PathBuf,
   lock: Mutex<()>,
}

impl DonationLedger {
   pub fn open(path: &str) -> Arc<Self> {
      Arc::new(Self {
         path: PathBuf::from(path),
         lock: Mutex::new(()),
      })
   }

   pub async fn cents_for(&self, id: &str) -> Option<i64> {
      let file = self.read().await.ok()?;
      file.entries
         .iter()
         .find(|entry| entry.id == id)
         .map(|entry| entry.cents)
   }

   pub async fn raised_cents(&self) -> i64 {
      let Ok(file) = self.read().await else {
         return 0;
      };
      file.entries
         .iter()
         .filter(|entry| entry.status == "paid")
         .map(|entry| entry.cents.max(0))
         .sum()
   }

   pub async fn remember_pending(&self, id: &str, provider: &str, cents: i64) {
      let _guard = self.lock.lock().await;
      let mut file = self.read().await.unwrap_or_default();
      if file.entries.iter().any(|entry| entry.id == id) {
         return;
      }
      file.entries.push(Entry {
         id: id.to_owned(),
         provider: provider.to_owned(),
         provider_ref: String::new(),
         cents,
         status: "pending".to_owned(),
      });
      self.write(&file).await;
   }

   /// Record a confirmed payment. A later notice for the same provider
   /// reference replaces the amount, so a partial crypto payment can be
   /// corrected when more arrives, without counting twice.
   pub async fn mark_paid(&self, id: &str, provider: &str, provider_ref: &str, cents: i64) {
      if provider_ref.is_empty() || cents <= 0 {
         return;
      }
      let _guard = self.lock.lock().await;
      let mut file = self.read().await.unwrap_or_default();
      if let Some(entry) = file.entries.iter_mut().find(|entry| {
         entry.status == "paid" && entry.provider == provider && entry.provider_ref == provider_ref
      }) {
         if entry.cents == cents {
            return;
         }
         entry.cents = cents;
         if !id.is_empty() {
            entry.id = id.to_owned();
         }
         self.write(&file).await;
         return;
      }
      if let Some(entry) = file.entries.iter_mut().find(|entry| !id.is_empty() && entry.id == id) {
         entry.status = "paid".to_owned();
         entry.provider = provider.to_owned();
         entry.provider_ref = provider_ref.to_owned();
         entry.cents = cents;
      } else {
         file.entries.push(Entry {
            id: id.to_owned(),
            provider: provider.to_owned(),
            provider_ref: provider_ref.to_owned(),
            cents,
            status: "paid".to_owned(),
         });
      }
      self.write(&file).await;
   }

   async fn read(&self) -> Result<LedgerFile, ()> {
      match tokio::fs::read(&self.path).await {
         Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| ()),
         Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(LedgerFile::default()),
         Err(_) => Err(()),
      }
   }

   async fn write(&self, file: &LedgerFile) {
      let Ok(bytes) = serde_json::to_vec_pretty(file) else {
         return;
      };
      if let Err(err) = tokio::fs::write(&self.path, bytes).await {
         tracing::warn!(path = %self.path.display(), "could not store donations: {err}");
      }
   }
}

pub fn new_donation_id() -> String {
   let mut bytes = [0_u8; 8];
   let _ = SystemRandom::new().fill(&mut bytes);
   format!("don_{}", HEXLOWER.encode(&bytes))
}

pub fn valid_donation_id(id: &str) -> bool {
   let Some(rest) = id.strip_prefix("don_") else {
      return false;
   };
   rest.len() == 16 && rest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Dollars the visitor typed. `$1` through `$10,000`, at most two decimal places.
pub fn parse_amount_cents(raw: &str) -> Result<i64, &'static str> {
   let raw = raw.trim();
   if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.') {
      return Err("Enter an amount in dollars, like 25 or 25.50");
   }
   let (whole, frac) = match raw.split_once('.') {
      Some((whole, frac)) => {
         if frac.len() > 2 || frac.is_empty() && whole.is_empty() {
            return Err("Use at most two decimal places");
         }
         (whole, frac)
      },
      None => (raw, ""),
   };
   if whole.is_empty() && frac.is_empty() {
      return Err("Enter an amount in dollars, like 25 or 25.50");
   }
   let dollars: i64 = if whole.is_empty() {
      0
   } else {
      whole
         .parse()
         .map_err(|_| "That amount is too large")?
   };
   let cents_part: i64 = if frac.is_empty() {
      0
   } else {
      format!("{frac:0<2}").parse().unwrap_or(0)
   };
   let cents = dollars
      .checked_mul(100)
      .and_then(|value| value.checked_add(cents_part))
      .ok_or("That amount is too large")?;
   if cents < MIN_CENTS {
      return Err("The minimum donation is $1");
   }
   if cents > MAX_CENTS {
      return Err("The maximum donation is $10,000");
   }
   Ok(cents)
}

pub fn format_usd(cents: i64) -> String {
   let cents = cents.max(0);
   if cents % 100 == 0 {
      format!("${}", cents / 100)
   } else {
      format!("${}.{:02}", cents / 100, cents % 100)
   }
}

pub fn dollars_string(cents: i64) -> String {
   format!("{:.2}", cents as f64 / 100.0)
}

/// The homepage ask runs through the end of `deadline` (`YYYY-MM-DD`, UTC).
pub fn campaign_open(deadline: &str) -> bool {
   let Some((year, rest)) = deadline.split_once('-') else {
      return false;
   };
   let Some((month, day)) = rest.split_once('-') else {
      return false;
   };
   let Ok(year) = year.parse::<i32>() else {
      return false;
   };
   let Ok(month) = month.parse::<u8>() else {
      return false;
   };
   let Ok(day) = day.parse::<u8>() else {
      return false;
   };
   let Ok(month) = time::Month::try_from(month) else {
      return false;
   };
   let Ok(date) = time::Date::from_calendar_date(year, month, day) else {
      return false;
   };
   time::OffsetDateTime::now_utc().date() <= date
}

pub fn allowed_checkout_url(provider: &str, url: &str) -> bool {
   let Some(host) = url
      .strip_prefix("https://")
      .and_then(|rest| rest.split(['/', '?', '#']).next())
      .filter(|host| !host.is_empty() && !host.contains('@'))
   else {
      return false;
   };
   match provider {
      "stripe" => host == "checkout.stripe.com",
      "nowpayments" => host == "nowpayments.io" || host.ends_with(".nowpayments.io"),
      "lzt" => host == "lzt.market" || host.ends_with(".lzt.market"),
      _ => false,
   }
}

pub async fn stripe_checkout(
   secret_key: &str,
   donation_id: &str,
   cents: i64,
   success_url: &str,
   cancel_url: &str,
) -> Result<String, PayError> {
   let fields = [
      ("mode", "payment".to_owned()),
      ("success_url", success_url.to_owned()),
      ("cancel_url", cancel_url.to_owned()),
      ("client_reference_id", donation_id.to_owned()),
      ("metadata[donation_id]", donation_id.to_owned()),
      ("metadata[source]", "teapawt_donation".to_owned()),
      ("line_items[0][quantity]", "1".to_owned()),
      ("line_items[0][price_data][currency]", "usd".to_owned()),
      ("line_items[0][price_data][unit_amount]", cents.to_string()),
      (
         "line_items[0][price_data][product_data][name]",
         "Donation to nitter.cf".to_owned(),
      ),
   ];
   let body = form_urlencoded::Serializer::new(String::new())
      .extend_pairs(fields.iter().map(|(key, value)| (*key, value.as_str())))
      .finish();
   let response = HTTP
      .post("https://api.stripe.com/v1/checkout/sessions")
      .bearer_auth(secret_key)
      .header("content-type", "application/x-www-form-urlencoded")
      .body(body)
      .send()
      .await
      .map_err(|err| PayError::new(format!("stripe request failed: {err}")))?;
   let status = response.status();
   let payload: Value = response.json().await.unwrap_or(Value::Null);
   if !status.is_success() {
      let message = payload["error"]["message"]
         .as_str()
         .unwrap_or("Stripe rejected the checkout");
      return Err(PayError::new(format!("stripe {status}: {message}")));
   }
   payload["url"]
      .as_str()
      .filter(|url| allowed_checkout_url("stripe", url))
      .map(str::to_owned)
      .ok_or_else(|| PayError::new("stripe did not return a checkout url"))
}

pub async fn stripe_paid_session(
   secret_key: &str,
   session_id: &str,
) -> Result<Option<(String, i64, String)>, PayError> {
   if !session_id.starts_with("cs_") || session_id.len() > 255 {
      return Ok(None);
   }
   let url = format!(
      "https://api.stripe.com/v1/checkout/sessions/{}",
      urlencoding_path(session_id)
   );
   let response = HTTP
      .get(url)
      .bearer_auth(secret_key)
      .send()
      .await
      .map_err(|err| PayError::new(format!("stripe retrieve failed: {err}")))?;
   if !response.status().is_success() {
      return Ok(None);
   }
   let payload: Value = response.json().await.unwrap_or(Value::Null);
   if payload["payment_status"] != "paid" {
      return Ok(None);
   }
   let id = payload["metadata"]["donation_id"]
      .as_str()
      .or_else(|| payload["client_reference_id"].as_str())
      .unwrap_or("")
      .to_owned();
   if !valid_donation_id(&id) {
      return Ok(None);
   }
   let cents = payload["amount_total"].as_i64().unwrap_or(0);
   let reference = payload["id"].as_str().unwrap_or(session_id).to_owned();
   Ok(Some((id, cents, reference)))
}

/// Stripe signs `{timestamp}.{raw_body}` with the webhook secret.
pub fn stripe_signature_ok(payload: &[u8], header: &str, secret: &str) -> bool {
   if secret.is_empty() || header.is_empty() {
      return false;
   }
   let mut timestamp = None;
   let mut signatures = Vec::new();
   for part in header.split(',') {
      let Some((key, value)) = part.trim().split_once('=') else {
         continue;
      };
      match key {
         "t" => timestamp = value.parse::<i64>().ok(),
         "v1" => signatures.push(value),
         _ => {},
      }
   }
   let Some(timestamp) = timestamp else {
      return false;
   };
   let now = time::OffsetDateTime::now_utc().unix_timestamp();
   if (now - timestamp).abs() > 300 {
      return false;
   }
   let mut signed = timestamp.to_string().into_bytes();
   signed.push(b'.');
   signed.extend_from_slice(payload);
   let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
   signatures.into_iter().any(|signature| {
      let Ok(bytes) = HEXLOWER.decode(signature.as_bytes()) else {
         return false;
      };
      hmac::verify(&key, &signed, &bytes).is_ok()
   })
}

pub async fn nowpayments_invoice(
   api_key: &str,
   donation_id: &str,
   cents: i64,
   ipn_url: &str,
   success_url: &str,
   cancel_url: &str,
) -> Result<String, PayError> {
   let payload = serde_json::json!({
      "price_amount": dollars_string(cents).parse::<f64>().unwrap_or(0.0),
      "price_currency": "usd",
      "order_id": donation_id,
      "order_description": "Donation to nitter.cf",
      "ipn_callback_url": ipn_url,
      "success_url": success_url,
      "cancel_url": cancel_url,
   });
   let response = HTTP
      .post("https://api.nowpayments.io/v1/invoice")
      .header("x-api-key", api_key)
      .json(&payload)
      .send()
      .await
      .map_err(|err| PayError::new(format!("nowpayments request failed: {err}")))?;
   let status = response.status();
   let body: Value = response.json().await.unwrap_or(Value::Null);
   if !status.is_success() {
      let message = body["message"].as_str().unwrap_or("NOWPayments rejected the invoice");
      return Err(PayError::new(format!("nowpayments {status}: {message}")));
   }
   body["invoice_url"]
      .as_str()
      .filter(|url| allowed_checkout_url("nowpayments", url))
      .map(str::to_owned)
      .ok_or_else(|| PayError::new("nowpayments did not return an invoice url"))
}

pub fn nowpayments_signature_ok(raw: &[u8], header: &str, secret: &str) -> bool {
   if secret.is_empty() || header.is_empty() {
      return false;
   }
   let key = hmac::Key::new(hmac::HMAC_SHA512, secret.as_bytes());
   let candidates = [raw.to_vec(), sorted_json(raw)];
   candidates.into_iter().any(|body| {
      let digest = hmac::sign(&key, &body);
      let expected = HEXLOWER.encode(digest.as_ref());
      constant_hex_eq(&expected, header.trim())
   })
}

/// USD cents actually received. A partial crypto payment counts only the share
/// of the invoice that arrived, not the full `price_amount`.
pub fn nowpayments_received_cents(body: &Value) -> Option<i64> {
   let status = body["payment_status"]
      .as_str()
      .unwrap_or("")
      .to_ascii_lowercase();
   if matches!(status.as_str(), "failed" | "expired" | "refunded") {
      return None;
   }
   let price = json_f64(&body["price_amount"])?;
   let expected = json_f64(&body["pay_amount"])?;
   let received = json_f64(&body["actually_paid"]).unwrap_or(0.0);
   if price <= 0.0 || expected <= 0.0 || received <= 0.0 {
      return None;
   }
   let ratio = (received / expected).clamp(0.0, 1.0);
   let cents = if ratio >= 0.999 && matches!(status.as_str(), "finished" | "confirmed") {
      (price * 100.0).round() as i64
   } else {
      (ratio * price * 100.0).round() as i64
   };
   (cents > 0).then_some(cents)
}

pub async fn lzt_invoice(
   config: &crate::config::LztDonationConfig,
   donation_id: &str,
   cents: i64,
   success_url: &str,
   callback_url: &str,
) -> Result<String, PayError> {
   let payload = serde_json::json!({
      "currency": config.currency,
      "amount": dollars_string(cents).parse::<f64>().unwrap_or(0.0),
      "payment_id": donation_id,
      "comment": "Donation to nitter.cf",
      "url_success": success_url,
      "url_callback": callback_url,
      "merchant_id": config.merchant_id,
   });
   let response = HTTP
      .post("https://prod-api.lzt.market/invoice")
      .bearer_auth(&config.access_token)
      .json(&payload)
      .send()
      .await
      .map_err(|err| PayError::new(format!("lzt request failed: {err}")))?;
   let status = response.status();
   let body: Value = response.json().await.unwrap_or(Value::Null);
   if !(200..300).contains(&status.as_u16()) {
      let message = body["error"]
         .as_str()
         .or_else(|| body["errors"][0].as_str())
         .unwrap_or("LZT Market rejected the invoice");
      return Err(PayError::new(format!("lzt {status}: {message}")));
   }
   body["invoice"]["url"]
      .as_str()
      .filter(|url| allowed_checkout_url("lzt", url))
      .map(str::to_owned)
      .ok_or_else(|| PayError::new("lzt did not return an invoice url"))
}

pub async fn lzt_paid_invoice(
   access_token: &str,
   donation_id: &str,
) -> Result<Option<(i64, String)>, PayError> {
   if !valid_donation_id(donation_id) {
      return Ok(None);
   }
   let url = format!(
      "https://prod-api.lzt.market/invoice?payment_id={}",
      urlencoding_path(donation_id)
   );
   let response = HTTP
      .get(url)
      .bearer_auth(access_token)
      .send()
      .await
      .map_err(|err| PayError::new(format!("lzt retrieve failed: {err}")))?;
   if !response.status().is_success() {
      return Ok(None);
   }
   let body: Value = response.json().await.unwrap_or(Value::Null);
   let invoice = &body["invoice"];
   if invoice["status"].as_str().unwrap_or("").eq_ignore_ascii_case("paid") {
      let cents = dollars_value_to_cents(invoice.get("amount").unwrap_or(&Value::Null));
      let reference = invoice["invoice_id"]
         .as_i64()
         .map(|id| id.to_string())
         .or_else(|| invoice["invoice_id"].as_str().map(str::to_owned))
         .unwrap_or_else(|| donation_id.to_owned());
      return Ok(Some((cents, reference)));
   }
   Ok(None)
}

pub fn lzt_secret_ok(header: &str, secret: &str) -> bool {
   bytes_eq(header.as_bytes(), secret.as_bytes())
}

pub fn dollars_value_to_cents(value: &Value) -> i64 {
   if let Some(number) = value.as_f64() {
      return (number * 100.0).round() as i64;
   }
   if let Some(number) = value.as_i64() {
      return number.saturating_mul(100);
   }
   if let Some(text) = value.as_str() {
      return parse_amount_cents(text).unwrap_or(0);
   }
   0
}

pub fn provider_ready(config: &DonationsConfig, provider: &str) -> bool {
   match provider {
      "stripe" => config.stripe.configured(),
      "nowpayments" => config.nowpayments.configured(),
      "lzt" => config.lzt.configured(),
      _ => false,
   }
}

fn json_f64(value: &Value) -> Option<f64> {
   value
      .as_f64()
      .or_else(|| value.as_i64().map(|number| number as f64))
      .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn urlencoding_path(value: &str) -> String {
   form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn constant_hex_eq(expected_hex: &str, provided: &str) -> bool {
   bytes_eq(expected_hex.as_bytes(), provided.to_ascii_lowercase().as_bytes())
}

fn bytes_eq(left: &[u8], right: &[u8]) -> bool {
   if left.len() != right.len() || left.is_empty() {
      return false;
   }
   let mut diff = 0_u8;
   for (a, b) in left.iter().zip(right) {
      diff |= a ^ b;
   }
   diff == 0
}

fn sorted_json(raw: &[u8]) -> Vec<u8> {
   let Ok(value) = serde_json::from_slice::<Value>(raw) else {
      return Vec::new();
   };
   serde_json::to_vec(&sort_json(value)).unwrap_or_default()
}

fn sort_json(value: Value) -> Value {
   match value {
      Value::Object(map) => {
         let mut keys: Vec<String> = map.keys().cloned().collect();
         keys.sort();
         let mut sorted = Map::new();
         for key in keys {
            if let Some(child) = map.get(&key) {
               sorted.insert(key, sort_json(child.clone()));
            }
         }
         Value::Object(sorted)
      },
      Value::Array(items) => Value::Array(items.into_iter().map(sort_json).collect()),
      other => other,
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn amount_bounds() {
      assert_eq!(parse_amount_cents("1"), Ok(100));
      assert_eq!(parse_amount_cents("25.5"), Ok(2550));
      assert_eq!(parse_amount_cents("10000"), Ok(1_000_000));
      assert!(parse_amount_cents("0.50").is_err());
      assert!(parse_amount_cents("10000.01").is_err());
      assert!(parse_amount_cents("10.999").is_err());
      assert!(parse_amount_cents("-5").is_err());
   }

   #[test]
   fn stripe_signature_round_trip() {
      let payload = br#"{"id":"evt_test"}"#;
      let secret = "whsec_test";
      let timestamp = time::OffsetDateTime::now_utc().unix_timestamp();
      let mut signed = timestamp.to_string().into_bytes();
      signed.push(b'.');
      signed.extend_from_slice(payload);
      let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
      let digest = HEXLOWER.encode(hmac::sign(&key, &signed).as_ref());
      let header = format!("t={timestamp},v1={digest}");
      assert!(stripe_signature_ok(payload, &header, secret));
      assert!(!stripe_signature_ok(payload, &header, "whsec_other"));
   }

   #[test]
   fn partial_crypto_payment_counts_only_what_arrived() {
      let body = serde_json::json!({
         "payment_status": "partially_paid",
         "price_amount": 25,
         "pay_amount": 0.4069642,
         "actually_paid": 0.02107742,
         "invoice_status": "finished"
      });
      assert_eq!(nowpayments_received_cents(&body), Some(129));
   }

   #[test]
   fn finished_crypto_payment_counts_the_invoice() {
      let body = serde_json::json!({
         "payment_status": "finished",
         "price_amount": 25,
         "pay_amount": 0.4,
         "actually_paid": 0.4
      });
      assert_eq!(nowpayments_received_cents(&body), Some(2500));
   }
   #[test]
   fn nowpayments_accepts_sorted_body() {
      let raw = br#"{"b":1,"a":{"d":2,"c":3}}"#;
      let secret = "ipn-secret";
      let sorted = sorted_json(raw);
      let key = hmac::Key::new(hmac::HMAC_SHA512, secret.as_bytes());
      let digest = HEXLOWER.encode(hmac::sign(&key, &sorted).as_ref());
      assert!(nowpayments_signature_ok(raw, &digest, secret));
      assert!(!nowpayments_signature_ok(raw, "00", secret));
   }

   #[tokio::test]
   async fn paid_amount_is_counted_once() {
      let path = std::env::temp_dir().join(format!("teapot-donations-{}.json", new_donation_id()));
      let ledger = DonationLedger::open(path.to_str().unwrap());
      ledger.remember_pending("don_0123456789abcdef", "stripe", 500).await;
      ledger
         .mark_paid("don_0123456789abcdef", "stripe", "cs_test", 500)
         .await;
      ledger
         .mark_paid("don_0123456789abcdef", "stripe", "cs_test", 500)
         .await;
      assert_eq!(ledger.raised_cents().await, 500);
      let _ = tokio::fs::remove_file(path).await;
   }
}
