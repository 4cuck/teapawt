use maud::{
   Markup,
   html,
};

use super::{
   frame,
   minute_label,
};
use crate::routes::AccountData;

pub(crate) fn account(theme: &str, id: i64, data: Option<&AccountData>) -> Markup {
   let identity = data.and_then(|item| item.identity.as_ref());
   let runtime = data.and_then(|item| item.account.as_ref());
   let raw_name = runtime
      .map(|acct| acct.username.trim())
      .filter(|name| !name.is_empty())
      .or_else(|| {
         identity
            .map(|ident| ident.username.trim())
            .filter(|name| !name.is_empty())
      });
   let heading = match raw_name {
      Some(username) => format!("Account #{id} — @{username}"),
      None => format!("Account #{id} (unnamed)"),
   };

   frame(
      theme,
      &heading,
      "account",
      Some(id),
      html! {
         header class="admin-page-header" {
            a class="admin-text-link" href="/admin/stats/restrictions" { "← Restriction log" }
            p class="admin-eyebrow" { "Operations / session #" (id) }
            h1 { (heading) }
            p { "Live runtime pool flags, authenticated probe & pool reassignment controls, and 30-day restriction evidence." }
         }

         @if let Some(acct) = runtime {
            @let in_search = acct.pool == "search";
            @let in_clean_view = acct.pool != "search" && !acct.demoted_from_search;
            section class="admin-stats-panel admin-actions-card" data-session=(id) {
               h2 { "Operator actions" }
               p class="admin-hint" {
                  "Run Health Probe checks account/settings and falls back to an authenticated GraphQL read over the account's pinned SOCKS5 exit (never loading x.com HTML or triggering bulk Cloudflare challenges). Pool moves apply immediately in memory and PostgreSQL."
               }
               div class="admin-action-buttons admin-probe-actions" {
                  button
                     id="run-probe"
                     class="admin-btn admin-btn--primary"
                     type="button"
                     data-session=(id)
                  {
                     "Run Health Probe"
                  }
                  button
                     id="move-pool-search"
                     class="admin-btn"
                     type="button"
                     data-session=(id)
                     data-pool="search"
                     disabled[in_search]
                  {
                     "Move to Search Pool"
                  }
                  button
                     id="move-pool-view"
                     class="admin-btn"
                     type="button"
                     data-session=(id)
                     data-pool="view"
                     disabled[in_clean_view]
                  {
                     "Move to View Pool"
                  }
               }
               p id="account-action-status" class="admin-notice" role="status" hidden {}
            }

            section id="account-runtime-panel" class="admin-stats-panel" {
               h2 { "Current runtime state" }
               dl class="admin-account-state" {
                  div {
                     dt { "Assigned pool" }
                     dd {
                        span class=(if in_search { "admin-badge admin-badge--search" } else { "admin-badge admin-badge--view" }) {
                           (if acct.pool.is_empty() { "view" } else { acct.pool })
                        }
                        @if acct.demoted_from_search {
                           " "
                           span class="admin-badge admin-badge--warn" { "demoted from search" }
                        }
                     }
                  }
                  div { dt { "Authentication" } dd { (format!("{:?}", acct.kind)) } }
                  div {
                     dt { "Rotation status" }
                     dd {
                        @if acct.available {
                           span class="admin-badge admin-badge--ok" { "In rotation" }
                        } @else {
                           span class="admin-badge admin-badge--bad" { "Out of rotation" }
                        }
                     }
                  }
                  div { dt { "Active calls" } dd { (acct.active_calls) } }
                  div { dt { "Profile strikes" } dd { (acct.profile_strikes) } }
                  div {
                     dt { "Search empty streak" }
                     dd {
                        @if acct.search_empty_streak >= crate::api::auth::SEARCH_EMPTY_STREAK_ALERT {
                           span class="admin-badge admin-badge--bad" { (acct.search_empty_streak) " consecutive" }
                        } @else if acct.search_empty_streak >= 5 {
                           span class="admin-badge admin-badge--warn" { (acct.search_empty_streak) " consecutive" }
                        } @else {
                           (acct.search_empty_streak)
                        }
                     }
                  }
                  div {
                     dt { "Search since start" }
                     dd {
                        (acct.search_empty_total) " empty · "
                        (acct.search_non_empty_total) " with results · "
                        (acct.search_rate_limited_total) " RL switches"
                     }
                  }
                  div {
                     dt { "Last upstream activity" }
                     dd {
                        @if let Some(time) = acct.last_active { (minute_label(time)) }
                        @else { "None since start" }
                     }
                  }
               }

               div class="admin-flags admin-state-list" {
                  (good_flag("Available", acct.available))
                  (good_flag("Filters cleared", acct.filters_cleared))
                  (warn_flag("Demoted from search", acct.demoted_from_search))
                  (bad_flag("Suspended", acct.suspended))
                  (bad_flag("Retired", acct.retired))
                  (bad_flag("Credentials rejected", acct.credentials_rejected))
                  (bad_flag("Stale CSRF (ct0)", acct.ct0_dead))
                  (bad_flag("Search banned", acct.search_banned))
                  (bad_flag("Profile banned", acct.profile_banned))
                  (warn_flag("Settings write refused", acct.filters_failed))
                  (warn_flag("Global 15m limit", acct.globally_limited))
               }

               @if acct.filters_retry_at > 0 && acct.filters_retry_at < 4_000_000_000 {
                  p class="admin-hint" {
                     "Next settings-write attempt after " (minute_label(acct.filters_retry_at))
                  }
               }
            }

            section class="admin-stats-panel" {
               h2 { "Endpoint rate-limit windows & verified operations" }
               @if !acct.verified_operations.is_empty() {
                  div class="admin-badge-row" {
                     @for op in acct.verified_operations.keys() {
                        span class="admin-badge admin-badge--ok" { "✓ " (op) }
                     }
                  }
               }
               @if acct.rate_limits.is_empty() {
                  p class="admin-empty" { "No active endpoint rate-limit windows recorded." }
               } @else {
                  div class="admin-table-wrap" {
                     table class="admin-table" {
                        thead {
                           tr {
                              th scope="col" { "Endpoint" }
                              th scope="col" { "Remaining" }
                              th scope="col" { "Limit" }
                              th scope="col" { "Window resets (UTC)" }
                           }
                        }
                        tbody {
                           @for (endpoint, limit) in &acct.rate_limits {
                              tr {
                                 th scope="row" { (endpoint) }
                                 td { (limit.remaining) }
                                 td { (limit.limit) }
                                 td class="admin-col-time" { (minute_label(limit.reset)) }
                              }
                           }
                        }
                     }
                  }
               }
               @if !acct.refused_operations.is_empty() {
                  h3 { "Operation cooldowns" }
                  ul {
                     @for (operation, seen) in &acct.refused_operations {
                        li { strong { (operation) } " — refused at " (minute_label(*seen)) }
                     }
                  }
               }
            }
         } @else {
            p class="admin-notice admin-notice--warn" {
               "This account is not currently loaded in the runtime session pool. Stored restriction history is shown below."
            }
         }

         @if let Some(item) = data {
            section class="admin-stats-panel" {
               h2 { "Search response telemetry (PostgreSQL)" }
               p class="admin-hint" {
                  "Aggregated SearchTimeline empty vs non-empty responses and rate-limit switches stored in PostgreSQL."
               }
               @if item.search_24h.is_none() && item.search_7d.is_none() {
                  p class="admin-empty" { "No SearchTimeline telemetry recorded for this account in the last 7 days." }
               } @else {
                  div class="admin-table-wrap" {
                     table class="admin-table" {
                        thead {
                           tr {
                              th scope="col" { "Window" }
                              th scope="col" { "Empty" }
                              th scope="col" { "With results" }
                              th scope="col" { "Empty %" }
                              th scope="col" { "RL switches" }
                              th scope="col" { "Last empty (UTC)" }
                              th scope="col" { "Last with results (UTC)" }
                           }
                        }
                        tbody {
                           @for (label, summary) in [("Last 24 hours", item.search_24h.as_ref()), ("Last 7 days", item.search_7d.as_ref())] {
                              @if let Some(stats) = summary {
                                 @let total = stats.empty + stats.non_empty;
                                 @let pct = if total > 0 { 100.0 * stats.empty as f64 / total as f64 } else { 0.0 };
                                 tr {
                                    th scope="row" { (label) }
                                    td { (stats.empty) }
                                    td { (stats.non_empty) }
                                    td {
                                       @if total > 0 { (format!("{pct:.1}%")) }
                                       @else { "—" }
                                    }
                                    td { (stats.rate_limited) }
                                    td class="admin-col-time" {
                                       @if let Some(ts) = stats.last_empty_unix { (minute_label(ts)) }
                                       @else { "—" }
                                    }
                                    td class="admin-col-time" {
                                       @if let Some(ts) = stats.last_non_empty_unix { (minute_label(ts)) }
                                       @else { "—" }
                                    }
                                 }
                              }
                           }
                        }
                     }
                  }
               }
            }
         }

         section class="admin-stats-panel admin-cookie-panel" data-session=(id) {
            h2 { "Cookie export" }
            p class="admin-hint" {
               "Exports a Cookie-Editor JSON bundle with the latest in-memory ct0 token. Keep exported credentials private."
            }
            div class="admin-cookie-actions" {
               button id="show-cookies" class="admin-btn" type="button" { "Reveal cookies" }
               a
                  id="download-cookies"
                  class="admin-btn"
                  href=(format!("/admin/stats/sessions/{id}/cookies"))
                  download=(format!("account-{id}-cookies.json"))
               {
                  "Download JSON"
               }
            }
            div id="cookie-confirmation" hidden {
               p class="admin-hint" { "Confirm revealing raw authentication cookies on screen?" }
               button id="confirm-cookies" type="button" class="admin-btn admin-btn--danger" {
                  "Confirm reveal"
               }
            }
            p id="cookie-status" class="admin-notice" role="status" hidden {}
            div id="cookie-export" hidden {
               div class="admin-cookie-actions" {
                  button id="copy-cookies" class="admin-btn" type="button" { "Copy JSON" }
               }
               textarea id="cookie-json" readonly rows="10" aria-label="Cookie-Editor JSON" {}
            }
         }

         section class="admin-stats-panel" {
            h2 { "Restriction evidence timeline" }
            p class="admin-hint" { "Retained for 30 days. Historical entries remain visible after an account is reinstated." }
            @if let Some(item) = data {
               @if item.events.is_empty() {
                  p class="admin-empty" { "No restriction events recorded for this account." }
               } @else {
                  ol class="admin-timeline" {
                     @for event in &item.events {
                        li {
                           (minute_label(event.observed_unix))
                           strong { (event.kind.label()) }
                           p { (event.evidence) }
                           small {
                              (event.endpoint)
                              @if let Some(status) = event.http_status { " · HTTP " (status) }
                              @if let Some(code) = event.upstream_code { " · code " (code) }
                           }
                        }
                     }
                  }
               }
            } @else {
               p class="admin-empty" { "Account was not found." }
            }
         }
      },
   )
}

fn good_flag(label: &str, yes: bool) -> Markup {
   let (class_name, state) = if yes {
      ("admin-state admin-state--ok", "yes")
   } else {
      ("admin-state admin-state--inactive", "no")
   };
   html! { span class=(class_name) { (label) ": " (state) } }
}

fn warn_flag(label: &str, active: bool) -> Markup {
   let (class_name, state) = if active {
      ("admin-state admin-state--warn", "yes")
   } else {
      ("admin-state admin-state--inactive", "no")
   };
   html! { span class=(class_name) { (label) ": " (state) } }
}

fn bad_flag(label: &str, active: bool) -> Markup {
   let (class_name, state) = if active {
      ("admin-state admin-state--active", "active")
   } else {
      ("admin-state admin-state--inactive", "clear")
   };
   html! { span class=(class_name) { (label) ": " (state) } }
}
