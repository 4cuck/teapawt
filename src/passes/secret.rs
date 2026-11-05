//! Order ids, session tokens, logins and the stored password hash.

use std::num::NonZeroU32;

use data_encoding::HEXLOWER;
use ring::{
   digest,
   pbkdf2,
   rand::{
      SecureRandom as _,
      SystemRandom,
   },
};

const ITERATIONS: NonZeroU32 = NonZeroU32::new(100_000).expect("nonzero");
/// No 0/o or 1/l/i: a login is read off a screen and typed somewhere else.
const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

fn random<const N: usize>() -> [u8; N] {
   let mut bytes = [0_u8; N];
   SystemRandom::new()
      .fill(&mut bytes)
      .expect("the system random source failed");
   bytes
}

pub fn order_id() -> String {
   format!("pas_{}", HEXLOWER.encode(&random::<8>()))
}

pub fn valid_order_id(id: &str) -> bool {
   id.strip_prefix("pas_")
      .is_some_and(|rest| rest.len() == 16 && rest.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// 256 random bits, hex. Session cookies and order claims.
pub fn token() -> String {
   HEXLOWER.encode(&random::<32>())
}

pub fn valid_token(token: &str) -> bool {
   token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// What Postgres keeps of a token, so a copy of the table opens no session.
pub fn digest(token: &str) -> String {
   HEXLOWER.encode(digest::digest(&digest::SHA256, token.as_bytes()).as_ref())
}

fn readable(len: usize) -> String {
   // Bytes past the last whole multiple of the alphabet would favour its
   // first letters.
   let limit = 256 - 256 % ALPHABET.len();
   let mut out = String::with_capacity(len);
   while out.len() < len {
      for byte in random::<32>() {
         if usize::from(byte) < limit && out.len() < len {
            out.push(char::from(ALPHABET[usize::from(byte) % ALPHABET.len()]));
         }
      }
   }
   out
}

pub fn username() -> String {
   format!("pass-{}", readable(8))
}

/// About 99 bits.
pub fn password() -> String {
   readable(20)
}

pub fn hash_password(password: &str) -> String {
   let salt = random::<16>();
   let mut hash = [0_u8; 32];
   pbkdf2::derive(
      pbkdf2::PBKDF2_HMAC_SHA256,
      ITERATIONS,
      &salt,
      password.as_bytes(),
      &mut hash,
   );
   format!(
      "pbkdf2-sha256${ITERATIONS}${}${}",
      HEXLOWER.encode(&salt),
      HEXLOWER.encode(&hash)
   )
}

pub fn verify_password(password: &str, stored: &str) -> bool {
   let mut parts = stored.split('$');
   let (Some("pbkdf2-sha256"), Some(rounds), Some(salt), Some(hash), None) = (
      parts.next(),
      parts.next(),
      parts.next(),
      parts.next(),
      parts.next(),
   ) else {
      return false;
   };
   let (Some(rounds), Ok(salt), Ok(hash)) = (
      rounds.parse().ok().and_then(NonZeroU32::new),
      HEXLOWER.decode(salt.as_bytes()),
      HEXLOWER.decode(hash.as_bytes()),
   ) else {
      return false;
   };
   pbkdf2::verify(
      pbkdf2::PBKDF2_HMAC_SHA256,
      rounds,
      &salt,
      password.as_bytes(),
      &hash,
   )
   .is_ok()
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn a_password_checks_against_its_own_hash_only() {
      let password = password();
      assert_eq!(password.len(), 20);
      let stored = hash_password(&password);
      assert!(verify_password(&password, &stored));
      assert!(!verify_password("not-it", &stored));
      assert!(!verify_password(&password, "pbkdf2-sha256$1$00$00"));
      assert!(!verify_password(&password, ""));
   }

   #[test]
   fn logins_use_the_readable_alphabet() {
      let name = username();
      assert!(name.starts_with("pass-"));
      assert!(
         name["pass-".len()..]
            .bytes()
            .all(|byte| ALPHABET.contains(&byte))
      );
   }

   #[test]
   fn order_ids_and_tokens_have_one_shape() {
      assert!(valid_order_id(&order_id()));
      assert!(!valid_order_id("don_0123456789abcdef"));
      assert!(!valid_order_id("pas_0123"));
      assert!(valid_token(&token()));
      assert!(!valid_token("abc"));
      assert_eq!(digest("x").len(), 64);
   }
}
