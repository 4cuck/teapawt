//! HTTP routes.

pub(crate) mod admin_stats;
pub(crate) mod agents;
pub(crate) mod anti_ddos;
pub(crate) use admin_stats::{
   AccountData,
   InvestigationData,
   InvestigationQuery,
   Observation,
   OutAccountRow,
   OutAccountsPageData,
   OutAccountsQuery,
   admin_no_store,
   record_search,
};
mod app;
mod broadcast;
mod community;
mod debug;
mod donate;
mod embed;
mod explore;
mod fingerprint;
mod game;
pub mod helpers;
mod import;
mod intent;
mod list;
mod media;
mod middleware;
mod notes;
mod pages;
mod pass;
mod preferences;
mod redirect;
mod rss;
mod search;
mod search_gate;
mod seo;
mod status;
mod timeline;
mod trending;
mod unsupported;

pub use agents::negotiate;
pub use anti_ddos::gate;
pub use app::router;
pub use middleware::{
   client_middleware,
   prefs_middleware,
   snowflake_guard,
};
