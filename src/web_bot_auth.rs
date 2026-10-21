//! Public key directory for Web Bot Auth.
//!
//! The directory is signed so another site can verify it. The X fetches stay
//! unsigned: those calls impersonate a browser, and a bot signature on them
//! would identify the scraper.

use std::{
   fs,
   io::Write as _,
   os::unix::fs::OpenOptionsExt,
   path::Path,
   sync::OnceLock,
};

use axum::{
   http::{HeaderMap, HeaderValue, StatusCode, header},
   response::{IntoResponse, Response},
};
use data_encoding::{BASE64, BASE64URL_NOPAD};
use ring::{
   rand::{self, SystemRandom},
   signature::{Ed25519KeyPair, KeyPair},
};

const KEY_FILE: &str = "web-bot-auth.pkcs8";

struct Identity {
   pair: Ed25519KeyPair,
   jwks: String,
   thumbprint: String,
}

pub fn directory(headers: &HeaderMap) -> Response {
   let Some(identity) = identity() else {
      return StatusCode::INTERNAL_SERVER_ERROR.into_response();
   };
   let authority = authority_from(headers);
   let created = time::OffsetDateTime::now_utc().unix_timestamp();
   let expires = created + 86_400;
   let nonce = random_nonce();
   let params = format!(
      "(\"@authority\";req);alg=\"ed25519\";keyid=\"{}\";nonce=\"{nonce}\";tag=\"http-message-signatures-directory\";created={created};expires={expires}",
      identity.thumbprint
   );
   let base = format!("\"@authority\";req: {authority}\n\"@signature-params\": {params}\n");
   let signature = BASE64.encode(identity.pair.sign(base.as_bytes()).as_ref());
   let mut response = (
      [
         (
            header::CONTENT_TYPE,
            "application/http-message-signatures-directory+json",
         ),
         (header::CACHE_CONTROL, "max-age=3600"),
         (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
      ],
      identity.jwks.clone(),
   )
      .into_response();
   if let Ok(value) = HeaderValue::from_str(&format!("sig1=:{signature}:")) {
      response.headers_mut().insert(
         axum::http::HeaderName::from_static("signature"),
         value,
      );
   }
   if let Ok(value) = HeaderValue::from_str(&format!("sig1={params}")) {
      response.headers_mut().insert(
         axum::http::HeaderName::from_static("signature-input"),
         value,
      );
   }
   response
}

fn authority_from(headers: &HeaderMap) -> String {
   let raw = headers
      .get("x-forwarded-host")
      .or_else(|| headers.get(header::HOST))
      .and_then(|value| value.to_str().ok())
      .unwrap_or("xitter.cf");
   let host = raw.split(',').next().unwrap_or("").trim();
   if host.is_empty()
      || !host
         .bytes()
         .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'))
   {
      "xitter.cf".to_owned()
   } else {
      host.to_owned()
   }
}

fn identity() -> Option<&'static Identity> {
   static KEY: OnceLock<Option<Identity>> = OnceLock::new();
   KEY.get_or_init(load_or_create).as_ref()
}

fn load_or_create() -> Option<Identity> {
   let path = Path::new(KEY_FILE);
   let pkcs8 = if path.exists() {
      fs::read(path).ok()?
   } else {
      let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).ok()?;
      let mut file = fs::OpenOptions::new()
         .write(true)
         .create_new(true)
         .mode(0o600)
         .open(path)
         .ok()?;
      file.write_all(document.as_ref()).ok()?;
      document.as_ref().to_vec()
   };
   let pair = Ed25519KeyPair::from_pkcs8(&pkcs8).ok()?;
   let public = pair.public_key().as_ref();
   let x = BASE64URL_NOPAD.encode(public);
   let canonical = format!("{{\"crv\":\"Ed25519\",\"kty\":\"OKP\",\"x\":\"{x}\"}}");
   let thumbprint =
      BASE64URL_NOPAD.encode(&crate::routes::agents::sha256(canonical.as_bytes()));
   let jwks = format!(
      "{{\"keys\":[{{\"kty\":\"OKP\",\"crv\":\"Ed25519\",\"x\":\"{x}\",\"kid\":\"{thumbprint}\",\"use\":\"sig\",\"alg\":\"EdDSA\"}}]}}"
   );
   Some(Identity {
      pair,
      jwks,
      thumbprint,
   })
}

fn random_nonce() -> String {
   let mut bytes = [0_u8; 32];
   if SystemRandom::new().fill(&mut bytes).is_err() {
      return "0".to_owned();
   }
   BASE64.encode(&bytes)
}

trait FillRandom {
   fn fill(&self, dest: &mut [u8]) -> Result<(), ring::error::Unspecified>;
}

impl FillRandom for SystemRandom {
   fn fill(&self, dest: &mut [u8]) -> Result<(), ring::error::Unspecified> {
      rand::SecureRandom::fill(self, dest)
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn directory_signature_verifies_for_the_request_host() {
      let mut headers = HeaderMap::new();
      headers.insert(header::HOST, HeaderValue::from_static("xitter.cf"));
      let response = directory(&headers);
      assert_eq!(
         response.headers().get(header::CONTENT_TYPE).unwrap(),
         "application/http-message-signatures-directory+json"
      );
      assert!(response.headers().get("signature").is_some());
      assert!(response.headers().get("signature-input").is_some());
   }
}
