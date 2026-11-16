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

pub(crate) fn restrictions(
   _theme: &str,
   data: Option<&InvestigationData>,
   query: &InvestigationQuery,
   uri: &str,
) -> Markup {
   frame("Restriction investigation", "restrictions", html! {
      header class="admin-page-header" {
         p class="admin-eyebrow" { "Operations / restrictions" }
         h1 { "Restriction investigation" }
         p { "Chronological observations, not proof of current suspension. Current health is checked against the runtime account pool." }
      }
      section class="admin-stats-panel" {
         form class="admin-filters" action="/admin/stats/restrictions" method="get" {
            label { "Account or ID" input type="search" name="q" value=(query.q.as_deref().unwrap_or("")) placeholder="Username or numeric ID"; }
            label { "Observation" select name="kind" {
               option value="" { "All restriction types" }
               @for kind in [Restriction::Suspended, Restriction::CredentialsRejected, Restriction::CsrfRejected, Restriction::SearchBanned, Restriction::ProfileBanned, Restriction::OperationRefused, Restriction::RateLimited, Restriction::SettingsRejected, Restriction::SearchRested] {
                  option value=(kind as i16) selected[query.kind.as_deref() == Some(&(kind as i16).to_string())] { (kind_label(kind)) }
               }
            } }
            label { "Current state" select name="status" {
               option value="" { "Active and historical" }
               option value="active" selected[query.status.as_deref() == Some("active")] { "Currently active" }
               option value="historical" selected[query.status.as_deref() == Some("historical")] { "Historical only" }
            } }
            label { "Pool" select name="pool" {
               option value="" { "All pools" }
               option value="search" selected[query.pool.as_deref() == Some("search")] { "Search" }
               option value="view" selected[query.pool.as_deref() == Some("view")] { "View" }
            } }
            button type="submit" { "Apply filters" }
         }
      }
      section class="admin-stats-panel" aria-labelledby="observations-title" {
         div class="admin-section-heading" { h2 id="observations-title" { "Observations" } button id="investigation-refresh" type="button" data-uri=(uri) { "Refresh" } }
         p id="investigation-status" role="status" hidden {}
         div class="admin-table-wrap" {
            table class="admin-investigation-table" {
               thead { tr { th { "Observed (UTC)" } th { "Account" } th { "Restriction" } th { "State" } th { "Evidence" } } }
               tbody id="investigation-rows" {
                  @if let Some(data) = data { (rows(&data.events)) }
               }
            }
         }
         @if data.is_none() { p class="admin-empty" { "Investigation data is temporarily unavailable." } }
         @else if data.is_some_and(|data| data.events.is_empty()) { p class="admin-empty" { "No observations match these filters." } }
         @if let Some(data) = data {
            @if !data.events.is_empty() {
               div class="admin-result-summary" {
                  span { "Showing " (data.events.len()) " observation" @if data.events.len() != 1 { "s" } }
                  @if data.next_before.is_some() { span class="admin-muted" { " · More available" } }
               }
            }
         }
         @if let Some(before) = data.and_then(|data| data.next_before) {
            a class="admin-pagination" href=(format!("{uri}{}before={before}", if uri.contains('?') { "&" } else { "?" })) { "Older observations →" }
         }
      }
   })
}

pub(crate) fn rows(events: &[Observation]) -> Markup {
   html! {
      @for observation in events {
         tr {
            td { (minute_label(observation.event.observed_unix)) }
            td { a href=(format!("/admin/stats/sessions/{}", observation.event.session_id)) {
               strong { (observation.identity.as_ref().map_or("Unknown account", |identity| if identity.username.is_empty() { "Unnamed account" } else { &identity.username })) }
               small { "#" (observation.event.session_id) }
            } }
            td { span class="admin-badge" { (kind_label(observation.event.kind)) } }
            td { span class=(if observation.active == Some(true) { "admin-state admin-state--active" } else { "admin-state" }) {
               (match observation.active { Some(true) => "Active", Some(false) => "Historical", None => "Unknown" })
            } }
            td { strong { (observation.event.endpoint) } small { (observation.event.evidence) }
               @if let Some(status) = observation.event.http_status { " · HTTP " (status) }
               @if let Some(code) = observation.event.upstream_code { " · code " (code) }
            }
         }
      }
   }
}
