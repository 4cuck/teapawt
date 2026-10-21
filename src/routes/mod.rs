//! HTTP routes.

mod agents;
mod app;
mod broadcast;
mod community;
mod debug;
mod donate;
mod embed;
mod explore;
mod game;
pub mod helpers;
mod intent;
mod list;
mod media;
mod middleware;
mod notes;
mod pages;
mod preferences;
mod redirect;
mod rss;
mod search;
mod seo;
mod status;
mod timeline;
mod trending;
mod unsupported;

pub use agents::negotiate;
pub use app::router;
pub use middleware::{client_middleware, prefs_middleware, snowflake_guard};
