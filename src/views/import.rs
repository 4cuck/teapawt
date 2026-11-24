//! Public import figures and the separate admin form.

use maud::{
   DOCTYPE,
   Markup,
   html,
};
use time::{
   OffsetDateTime,
   format_description::well_known::Rfc3339,
};

use super::layout::{
   CLEAN_CSS,
   STYLE_CSS,
   theme_stylesheet,
};
use crate::{
   accounts::{
      ImportSummary,
      Stats,
   },
   types::AccountPool,
};

const IMPORT_CSS: &str = "/css/import.css?v=4";

pub(crate) const PUBLIC_PATH: &str = "/import";
pub(crate) const ADMIN_PATH: &str = "/admin/import";
pub(crate) const REFRESH_PATH: &str = "/import/summary";

pub(crate) struct Batch {
   pub timestamp: i64,
   pub counts:    ImportSummary,
}

pub(crate) struct Snapshot {
   pub stats:   Stats,
   pub batches: Option<Vec<Batch>>,
   pub updated: OffsetDateTime,
}

fn page(theme: &str, title: &str, dashboard: bool, content: Markup) -> Markup {
   html! {
      (DOCTYPE)
      html lang="en" {
         head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            title { (title) }
            link rel="stylesheet" href=(STYLE_CSS);
            link rel="stylesheet" href=(CLEAN_CSS);
            @if let Some(href) = theme_stylesheet(theme) {
               link rel="stylesheet" href=(href);
            }
            link rel="stylesheet" href=(IMPORT_CSS);
            @if dashboard {
               script src="/js/htmx.min.js" defer="" {}
            } @else {
               meta name="robots" content="noindex, nofollow";
            }
         }
         body {
            main class="import-page" {
               a href="/" { "← Home" }
               h1 { (title) }
               (content)
            }
         }
      }
   }
}

pub(crate) fn dashboard(theme: &str, snapshot: Option<&Snapshot>, is_admin: bool) -> Markup {
   page(theme, "Account inventory", true, html! {
      p class="import-lead" {
         "Pool totals and recent import batches. No individual accounts are shown."
         a href=(PUBLIC_PATH) { "Refresh" }
         @if is_admin {
            a class="import-submit import-action" href=(ADMIN_PATH) { "Import accounts" }
         }
      }
      div hx-get=(REFRESH_PATH) hx-trigger="every 60s" hx-swap="innerHTML" aria-live="polite" {
         (figures(snapshot))
      }
   })
}

fn time_label(instant: OffsetDateTime) -> Markup {
   let datetime = instant.format(&Rfc3339).unwrap_or_default();
   let label = format!(
      "{} {:02}:{:02} UTC",
      instant.date(),
      instant.hour(),
      instant.minute(),
   );
   html! { time datetime=(datetime) { (label) } }
}

pub(crate) fn figures(snapshot: Option<&Snapshot>) -> Markup {
   html! {
      @if let Some(snapshot) = snapshot {
         dl class="import-stats" {
            div class="import-stat-card" { dt { "Total" } dd { (snapshot.stats.total) } }
            div class="import-stat-card" { dt { "Search" } dd { (snapshot.stats.search) } }
            div class="import-stat-card" { dt { "search-pass" } dd { (snapshot.stats.search_pass) } }
            div class="import-stat-card" { dt { "View" } dd { (snapshot.stats.view) } }
            @if snapshot.stats.ct0_dead > 0 {
               div class="import-stat-card" { dt { "Stale ct0" } dd { (snapshot.stats.ct0_dead) } }
            }
            @if snapshot.stats.rejected > 0 {
               div class="import-stat-card" { dt { "Rejected" } dd { (snapshot.stats.rejected) } }
            }
         }
         @if let Some(batches) = &snapshot.batches {
            @if !batches.is_empty() {
               section class="import-batches-section" {
                  h2 { "Recent imports" }
                  ol class="import-batches" {
                     @for batch in batches {
                        li {
                           @if let Ok(instant) = OffsetDateTime::from_unix_timestamp(batch.timestamp) {
                              (time_label(instant))
                           }
                           span class="import-batch-num" { (batch.counts.imported) }
                           span class="import-batch-label" { "added" }
                           @if batch.counts.duplicates > 0 {
                              span class="import-batch-num" { (batch.counts.duplicates) }
                              span class="import-batch-label" { "dupes" }
                           }
                           @if batch.counts.invalid > 0 {
                              span class="import-batch-num" { (batch.counts.invalid) }
                              span class="import-batch-label" { "invalid" }
                           }
                        }
                     }
                  }
               }
            }
         }
         p class="import-updated" {
            "Updated " (time_label(snapshot.updated))
         }
      } @else {
         p role="status" { "Statistics are temporarily unavailable." }
      }
   }
}

pub(crate) enum AdminNotice {
   Error(&'static str),
   Saved(ImportSummary),
}

fn summary_line(values: &ImportSummary) -> Markup {
   html! {
      (values.imported) " added"
      @if values.duplicates > 0 { ", " (values.duplicates) " duplicates" }
      @if values.invalid > 0 { ", " (values.invalid) " skipped (incomplete or invalid)" }
   }
}

pub(crate) fn admin(
   theme: &str,
   csrf: &str,
   selected_pool: &str,
   max_chars: usize,
   max_lines: usize,
   notice: Option<&AdminNotice>,
) -> Markup {
   let selected = AccountPool::parse(selected_pool).unwrap_or(AccountPool::View);
   page(theme, "Import accounts", false, html! {
      section class="import-form-section" {
         h2 { "New batch" }
         @if let Some(notice) = notice {
            @match notice {
               AdminNotice::Saved(values) => {
                  div class="import-notice" role="status" {
                     "Saved: " (summary_line(values)) "."
                  }
               }
               AdminNotice::Error(message) => {
                  div class="import-notice" role="alert" { (message) }
               }
            }
         }
         form method="post" action=(ADMIN_PATH) autocomplete="off" {
            input type="hidden" name="csrf" value=(csrf);
            div class="import-field" {
               label for="import-pool" { "Pool" }
               select id="import-pool" name="pool" required {
                  option value="view" selected[selected == AccountPool::View] { "View" }
                  option value="search" selected[selected == AccountPool::Search] { "Search" }
                  option value="search-pass" selected[selected == AccountPool::SearchPass] { "Search-pass" }
               }
            }
            div class="import-field" {
               label for="import-accounts" { "Account data" }
               p class="import-help" id="import-help" {
                  "Paste a colon-, pipe-, or dash-delimited account export, a Netscape cookie file, "
                  "a JSON cookie export, or one Cookie header per line. Records missing "
                  code { "auth_token" } " or " code { "ct0" } " are skipped. Up to "
                  (max_lines) " lines and 256 KiB. Not shown after submit."
               }
               textarea id="import-accounts" name="accounts" rows="12" maxlength=(max_chars)
                  aria-describedby="import-help" spellcheck="false" autocapitalize="off" required {}
            }
            button type="submit" class="import-submit" { "Import" }
         }
      }
      nav class="import-nav-row" aria-label="Import navigation" {
         a href=(PUBLIC_PATH) { "← Public inventory" }
         a href="/admin/stats" { "Admin stats →" }
      }
   })
}

pub(crate) fn forbidden(theme: &str) -> Markup {
   page(theme, "Import accounts", false, html! {
      div class="import-signin" {
         p { "An administrator session is required." }
         p { a href="/search-pass/login" { "Sign in" } " and return to this page." }
      }
   })
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn admin_form_preserves_selected_pool_and_dashboard_gates_admin_link() {
      let default_form = admin("Teapot", "token", "view", 1024, 10, None).into_string();
      assert!(default_form.contains("<option value=\"view\" selected>View</option>"));
      assert!(default_form.contains("<option value=\"search\">Search</option>"));
      assert!(default_form.contains("<option value=\"search-pass\">Search-pass</option>"));

      let search_form = admin("Teapot", "token", "search", 1024, 10, None).into_string();
      assert!(search_form.contains("<option value=\"view\">View</option>"));
      assert!(search_form.contains("<option value=\"search\" selected>Search</option>"));
      assert!(search_form.contains("<option value=\"search-pass\">Search-pass</option>"));

      let pass_form = admin("Teapot", "token", "search-pass", 1024, 10, None).into_string();
      assert!(pass_form.contains("<option value=\"view\">View</option>"));
      assert!(pass_form.contains("<option value=\"search\">Search</option>"));
      assert!(pass_form.contains("<option value=\"search-pass\" selected>Search-pass</option>"));

      let visitor_page = dashboard("Teapot", None, false).into_string();
      assert!(!visitor_page.contains(ADMIN_PATH));

      let admin_page = dashboard("Teapot", None, true).into_string();
      assert!(admin_page.contains(ADMIN_PATH));
   }
}
