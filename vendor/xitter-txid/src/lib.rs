//! Generate Xitter client transaction IDs.
//!
//! Fetching is left to you, since x.com returns the shell this reads from only
//! to a request carrying session cookies.
//!
//! ```ignore
//! use xitter_txid::transaction::ClientTransaction;
//!
//! let html = your_client.get("https://x.com").cookie(session).text()?;
//! let js_url = ClientTransaction::extract_ondemand_url(&html)?;
//! let js = your_client.get(&js_url).text()?;
//! let client = ClientTransaction::new(&html, &js)?;
//!
//! let id = client.generate_transaction_id("GET", "/i/api/1.1/jot/client_event.json");
//! ```

#![cfg_attr(test, allow(clippy::unwrap_used, reason = "tests may unwrap"))]

mod cubic_curve;
pub mod error;
mod interpolate;
mod rotation;
pub mod transaction;
mod utils;
