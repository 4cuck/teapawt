use maud::{
   Markup,
   html,
};

use super::{
   frame,
   kind_label,
   minute_label,
};
use crate::{
   routes::{
      InvestigationData,
      InvestigationQuery,
      Observation,
   },
   stats::Restriction,
};

const FILTERABLE_KINDS: [Restriction; 12] = [
   Restriction::SearchOnly,
   Restriction::Suspended,
   Restriction::CredentialsRejected,
   Restriction::CsrfRejected,
   Restriction::SearchBanned,
   Restriction::ProfileBanned,
   Restriction::RateLimited,
   Restriction::OperationRefused,
   Restriction::SettingsRejected,
   Restriction::SearchRested,
   Restriction::Reinstated,
   Restriction::SearchEmptyStreak,
];

fn kind_badge_class(kind: Restriction) -> &'static str {
   match kind {
      Restriction::Reinstated => "admin-badge admin-badge--ok",
      Restriction::Suspended
      | Restriction::CredentialsRejected
      | Restriction::CsrfRejected
      | Restriction::SearchEmptyStreak => "admin-badge admin-badge--bad",
      Restriction::SearchOnly
      | Restriction::SearchBanned
      | Restriction::ProfileBanned
      | Restriction::SettingsRejected => "admin-badge admin-badge--warn",
      Restriction::RateLimited | Restriction::OperationRefused | Restriction::SearchRested => {
         "admin-badge admin-badge--neutral"
      },
   }
}

fn pagination_bar(data: Option<&InvestigationData>, query: &InvestigationQuery) -> Markup {
   let count = data.map_or(0, |d| d.events.len());
   let next_before = data.and_then(|d| d.next_before);
   if count == 0 && query.before.is_none() && next_before.is_none() {
      return html! {};
   }

   html! {
      div class="admin-pagination-bar" {
         div class="admin-result-summary" {
            @if count > 0 {
               span {
                  "Showing " (count) " observation" @if count != 1 { "s" }
               }
            }
            @if let Some(cursor) = query.before {
               span class="admin-cursor-tag" { "Before #" (cursor) }
            }
            @if next_before.is_some() {
               span class="admin-muted" { "More history available" }
            }
         }
         div class="admin-pagination-links" {
            @if query.before.is_some() {
               a class="admin-pagination" href=(query.to_uri("/admin/stats/restrictions", Some(None))) {
                  "← Latest observations"
               }
            }
            @if let Some(before) = next_before {
               a class="admin-pagination" href=(query.to_uri("/admin/stats/restrictions", Some(Some(before)))) {
                  "Older observations →"
               }
            }
         }
      }
   }
}

pub(crate) fn restrictions(
   theme: &str,
   data: Option<&InvestigationData>,
   query: &InvestigationQuery,
   uri: &str,
) -> Markup {
   frame(
      theme,
      "Restriction investigation",
      "restrictions",
      None,
      html! {
         header class="admin-page-header" {
            p class="admin-eyebrow" { "Operations / restrictions" }
            h1 { "Account restriction log" }
            p { "Chronological observations joined with live in-memory account state. Select any account to run a live probe or reassign its pool." }
         }

         section class="admin-stats-panel" {
            form class="admin-filters" action="/admin/stats/restrictions" method="get" {
               label class="admin-filter-field admin-filter-field--search" {
                  "Account or ID"
                  input
                     type="search"
                     name="q"
                     value=(query.q.as_deref().unwrap_or(""))
                     placeholder="Username or numeric ID";
               }
               label class="admin-filter-field" {
                  "Observation kind"
                  select name="kind" {
                     option value="" { "All observation kinds" }
                     @for kind in FILTERABLE_KINDS {
                        @let code = (kind as i16).to_string();
                        option value=(code) selected[query.kind.as_deref() == Some(code.as_str())] {
                           (kind_label(kind))
                        }
                     }
                  }
               }
               label class="admin-filter-field" {
                  "Current state"
                  select name="status" {
                     option value="" { "Active & historical" }
                     option value="active" selected[query.status.as_deref() == Some("active")] {
                        "Still active"
                     }
                     option value="historical" selected[query.status.as_deref() == Some("historical")] {
                        "Historical (cleared)"
                     }
                  }
               }
               label class="admin-filter-field" {
                  "Current pool"
                  select name="pool" {
                     option value="" { "All pools" }
                     option value="search" selected[query.pool.as_deref() == Some("search")] { "Search pool" }
                     option value="view" selected[query.pool.as_deref() == Some("view")] { "View pool" }
                  }
               }
               div class="admin-filters-actions" {
                  button class="admin-btn admin-btn--primary" type="submit" { "Filter" }
                  @if query.has_filters() {
                     a class="admin-btn" href="/admin/stats/restrictions" { "Clear" }
                  }
                  button id="investigation-refresh" class="admin-btn" type="button" data-uri=(uri) {
                     "Refresh"
                  }
               }
            }
         }

         section class="admin-stats-panel" aria-labelledby="observations-title" {
            div class="admin-section-heading" {
               h2 id="observations-title" { "Recorded observations" }
            }
            p id="investigation-status" class="admin-notice" role="status" hidden {}

            (pagination_bar(data, query))

            div class="admin-table-wrap" {
               table class="admin-table admin-investigation-table" {
                  thead {
                     tr {
                        th scope="col" class="admin-col-time" { "Observed (UTC)" }
                        th scope="col" { "Account" }
                        th scope="col" { "Pool" }
                        th scope="col" { "Observation" }
                        th scope="col" class="admin-col-state" { "Runtime state" }
                        th scope="col" class="admin-col-evidence" { "Endpoint & evidence" }
                     }
                  }
                  tbody id="investigation-rows" {
                     @if let Some(data) = data {
                        (rows(&data.events))
                     }
                  }
               }
            }

            @if data.is_none() {
               p class="admin-empty" { "Investigation telemetry is temporarily unavailable." }
            } @else if data.is_some_and(|d| d.events.is_empty()) {
               p class="admin-empty" { "No restriction observations match these filters." }
            }

            (pagination_bar(data, query))
         }
      },
   )
}

pub(crate) fn rows(events: &[Observation]) -> Markup {
   html! {
      @for observation in events {
         @let session_id = observation.event.session_id;
         @let username = observation
            .identity
            .as_ref()
            .map(|ident| ident.username.trim())
            .unwrap_or("");
         @let pool = observation
            .identity
            .as_ref()
            .map(|ident| ident.pool.as_str())
            .filter(|val| !val.is_empty())
            .unwrap_or("view");
         tr {
            td class="admin-col-time" { (minute_label(observation.event.observed_unix)) }
            td class="admin-nowrap" {
               a href=(format!("/admin/stats/sessions/{session_id}")) {
                  @if username.is_empty() {
                     strong { "Account #" (session_id) }
                  } @else {
                     strong { "@" (username) }
                     " "
                     small class="admin-muted" { "#" (session_id) }
                  }
               }
            }
            td class="admin-nowrap" {
               span class=(if pool == "search" { "admin-badge admin-badge--search" } else { "admin-badge admin-badge--view" }) {
                  (pool)
               }
               @if observation.demoted_from_search {
                  " "
                  span class="admin-badge admin-badge--warn" title="Demoted from search pool; eligible for recovery" {
                     "demoted"
                  }
               }
            }
            td class="admin-nowrap" {
               span class=(kind_badge_class(observation.event.kind)) {
                  (kind_label(observation.event.kind))
               }
            }
            td class="admin-col-state" {
               @match observation.active {
                  Some(true) => span class="admin-state admin-state--active" { "Active" },
                  Some(false) => span class="admin-state admin-state--inactive" { "Historical" },
                  None => span class="admin-state" { "Unknown" },
               }
            }
            td class="admin-col-evidence" {
               strong { (observation.event.endpoint) }
               @if let Some(status) = observation.event.http_status {
                  " · HTTP " (status)
               }
               @if let Some(code) = observation.event.upstream_code {
                  " · code " (code)
               }
               div class="admin-muted" { (observation.event.evidence) }
            }
         }
      }
   }
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::stats::AccountEvent;

   #[test]
   fn pagination_replaces_before_cursor_instead_of_duplicating_it() {
      let query = InvestigationQuery {
         kind:   Some("2".to_owned()),
         status: Some("historical".to_owned()),
         pool:   Some("view".to_owned()),
         q:      None,
         before: Some(12_675),
      };
      let canonical = query.to_uri("/admin/stats/restrictions", None);
      let event = AccountEvent {
         id:            12_625,
         session_id:    105,
         kind:          Restriction::Suspended,
         observed_unix: 1_794_903_451,
         endpoint:      "account".to_owned(),
         http_status:   None,
         upstream_code: None,
         evidence:      "Search refused for this account: suspended".to_owned(),
      };
      let data = InvestigationData {
         events:      vec![Observation {
            event,
            identity: None,
            active: Some(false),
            demoted_from_search: true,
         }],
         next_before: Some(12_625),
         now:         1_794_910_000,
      };
      let html = restrictions("dark", Some(&data), &query, &canonical).into_string();
      assert!(html.contains("before=12625"));
      assert!(!html.contains("before=12675&amp;before=12625"));
      assert!(!html.contains("before=12675&before=12625"));
      assert!(html.contains("href=\"/admin/stats/restrictions?kind=2&amp;status=historical&amp;pool=view\""));
   }
}
