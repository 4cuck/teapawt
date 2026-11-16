//! Error types.

use std::{
   error::Error as StdError,
   fmt,
};

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
   /// Interpolation arrays have different lengths.
   MismatchedArguments,
   /// Failed to parse HTML or JavaScript.
   Parse(String),
   /// Required key or attribute not found.
   MissingKey(String),
   /// Base64 decoding failed.
   Base64(data_encoding::DecodeError),
}

impl fmt::Display for Error {
   #[inline]
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      match *self {
         Self::MismatchedArguments => {
            write!(f, "interpolation arrays have different lengths")
         },
         Self::Parse(ref msg) => write!(f, "parse error: {msg}"),
         Self::MissingKey(ref key) => write!(f, "missing required key: {key}"),
         Self::Base64(ref err) => write!(f, "base64 decode error: {err}"),
      }
   }
}

#[expect(
   clippy::missing_trait_methods,
   reason = "the other two, description and cause, are deprecated"
)]
impl StdError for Error {
   #[inline]
   fn source(&self) -> Option<&(dyn StdError + 'static)> {
      match *self {
         Self::Base64(ref err) => Some(err),
         Self::MismatchedArguments | Self::Parse(_) | Self::MissingKey(_) => None,
      }
   }
}

impl From<data_encoding::DecodeError> for Error {
   #[inline]
   fn from(err: data_encoding::DecodeError) -> Self {
      Self::Base64(err)
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn error_display() {
      let mismatched = Error::MismatchedArguments;
      assert!(mismatched.to_string().contains("different lengths"));

      let parse = Error::Parse("test".into());
      assert!(parse.to_string().contains("test"));

      let missing_key = Error::MissingKey("verification".into());
      assert!(missing_key.to_string().contains("verification"));
   }

   #[test]
   fn error_source() {
      let mismatched = Error::MismatchedArguments;
      assert!(mismatched.source().is_none());

      let parse = Error::Parse("test".into());
      assert!(parse.source().is_none());
   }
}
