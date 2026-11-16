use maud::{
   Markup,
   html,
};

use super::{
   frame,
   minute_label,
};
use crate::routes::AccountData;

pub(crate) fn account(_theme: &str, id: i64, data: Option<&AccountData>) -> Markup {
   let identity = data.and_then(|data| data.identity.as_ref());
   let account = data.and_then(|data| data.account.as_ref());
   let name = account
      .map(|account| account.username.as_str())
      .or_else(|| identity.map(|identity| identity.username.as_str()))
      .unwrap_or("Unknown account");
   frame("Account investigation", "restrictions", html! {
      header class="admin-page-header" {
         a class="admin-text-link" href="/admin/stats/restrictions" { "← Restriction observations" }
         p class="admin-eyebrow" { "Account #" (id) }
         h1 { "@" (name) }
         p { "Persisted observations and current runtime health. Cookie credentials are hidden until explicitly requested." }
      }
      @if let Some(account) = account {
         section class="admin-stats-panel" {
            h2 { "Current runtime state" }
            dl class="admin-account-state" {
               div { dt { "Pool" } dd { (account.pool) } }
               div { dt { "Authentication" } dd { (format!("{:?}", account.kind)) } }
               div { dt { "Availability" } dd { (if account.available { "Available" } else { "Unavailable" }) } }
               div { dt { "Active calls" } dd { (account.active_calls) } }
               div { dt { "Profile strikes" } dd { (account.profile_strikes) } }
               div { dt { "Last upstream activity" } dd { @if let Some(time) = account.last_active { (minute_label(time)) } @else { "Not observed" } } }
            }
            div class="admin-state-list" {
               (flag("Credentials rejected", account.credentials_rejected))
               (flag("Stale CSRF", account.ct0_dead))
               (flag("Suspended", account.suspended))
               (flag("Retired", account.retired))
               (flag("Search ban", account.search_banned))
               (flag("Profile ban", account.profile_banned))
               (flag("Filters cleared", account.filters_cleared))
               (flag("Settings write refused", account.filters_failed))
               (flag("Global rate limit", account.globally_limited))
            }
            @if account.filters_retry_at > 0 {
               p class="admin-hint" { "Settings write retry after " (minute_label(account.filters_retry_at)) }
            }
         }
         section class="admin-stats-panel admin-probe-panel" data-session=(id) {
            h2 { "Account health check" }
            p class="admin-hint" {
               "Writes account settings once to test whether this account can act. "
               "A successful write clears a stored suspension and returns the account to its pool. "
               "Nothing schedules this — it only runs when you ask."
            }
            div class="admin-probe-actions" {
               button id="run-probe" type="button" { "Check account health" }
            }
            p id="probe-status" role="status" {}
            div id="probe-result" hidden {
               dl class="admin-account-state" {
                  div { dt { "Verdict" } dd id="probe-verdict" {} }
                  div { dt { "Suspended" } dd id="probe-suspended" {} }
                  div { dt { "Retired" } dd id="probe-retired" {} }
                  div { dt { "Pool" } dd id="probe-pool" {} }
               }
               p id="probe-detail" class="admin-hint" {}
            }
         }
         section class="admin-stats-panel" {
            h2 { "Endpoint windows" }
            @if account.rate_limits.is_empty() { p class="admin-empty" { "No endpoint rate-limit windows are recorded." } }
            @else { div class="admin-table-wrap" { table { thead { tr { th { "Endpoint" } th { "Remaining" } th { "Limit" } th { "Resets" } } }
               tbody { @for (endpoint, limit) in &account.rate_limits { tr { th scope="row" { (endpoint) } td { (limit.remaining) } td { (limit.limit) } td { (minute_label(limit.reset)) } } } }
            } } }
            @if !account.refused_operations.is_empty() { h3 { "Operation cooldowns" } ul { @for (operation, seen) in &account.refused_operations { li { strong { (operation) } " — last refused " (minute_label(*seen)) } } } }
         }
      } @else { p class="admin-notice" { "This account is not in the current runtime pool. Historical evidence may still be available." } }
      section class="admin-stats-panel admin-cookie-panel" data-session=(id) {
         h2 { "Cookie export" }
         p { "Exports a Cookie-Editor-compatible JSON array using the latest runtime CSRF token. Anyone with these cookies can act as this account." }
         button id="show-cookies" type="button" { "Show cookies" }
         div id="cookie-confirmation" hidden {
            p { "Reveal account credentials on this screen?" }
            button id="confirm-cookies" type="button" class="admin-danger" { "Reveal credentials" }
         }
         p id="cookie-status" role="status" {}
         div id="cookie-export" hidden {
            div class="admin-cookie-actions" { button id="copy-cookies" type="button" { "Copy JSON" } a id="download-cookies" href=(format!("/admin/stats/sessions/{id}/cookies")) download=(format!("account-{id}-cookies.json")) { "Download JSON" } }
            textarea id="cookie-json" readonly rows="12" aria-label="Cookie-Editor JSON" {}
         }
      }
      section class="admin-stats-panel" {
         h2 { "Evidence timeline" }
         p { "Events are retained for 30 days. A temporary cooldown or rate limit does not mean the account is suspended." }
         @if let Some(data) = data {
            @if data.events.is_empty() { p class="admin-empty" { "No restriction evidence has been recorded for this account." } }
            @else { ol class="admin-timeline" { @for event in &data.events { li {
               (minute_label(event.observed_unix))
               strong { (event.kind.label()) }
               p { (event.evidence) }
               small { (event.endpoint) @if let Some(status) = event.http_status { " · HTTP " (status) } @if let Some(code) = event.upstream_code { " · code " (code) } }
            } } } }
         }
      }
   })
}

fn flag(label: &str, active: bool) -> Markup {
   html! { span class=(if active { "admin-state admin-state--active" } else { "admin-state" }) { (label) ": " (if active { "active" } else { "clear" }) } }
}
