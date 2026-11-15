//! Public import figures and the separate admin form.

use maud::{DOCTYPE, Markup, html};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::accounts::{ImportSummary, Stats};

use super::layout::{CLEAN_CSS, STYLE_CSS};

pub(crate) const PUBLIC_PATH: &str = "/import";
pub(crate) const ADMIN_PATH: &str = "/import/admin";
pub(crate) const REFRESH_PATH: &str = "/import/summary";

pub(crate) struct Batch {
   pub timestamp: i64,
   pub counts: ImportSummary,
}

pub(crate) struct Snapshot {
   pub stats: Stats,
   pub batches: Option<Vec<Batch>>,
   pub updated: OffsetDateTime,
}

fn page(title: &str, dashboard: bool, content: Markup) -> Markup {
   html! {
      (DOCTYPE)
      html lang="en" {
         head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            title { (title) }
            link rel="stylesheet" href=(STYLE_CSS);
            link rel="stylesheet" href=(CLEAN_CSS);
            link rel="stylesheet" href="/css/import.css?v=2";
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

pub(crate) fn dashboard(snapshot: Option<&Snapshot>) -> Markup {
   page(
      "Account inventory",
      true,
      html! {
         p class="import-lead" {
            "Pool totals and recent import batches. No individual accounts are shown."
            a href=(PUBLIC_PATH) { "Refresh" }
            a class="import-submit import-action" href=(ADMIN_PATH) { "Import accounts" }
         }
         div hx-get=(REFRESH_PATH) hx-trigger="every 60s" hx-swap="innerHTML" aria-live="polite" {
            (figures(snapshot))
         }
      },
   )
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
            div class="import-stat-card" { dt { "View" } dd { (snapshot.stats.view) } }
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
      @if values.invalid > 0 { ", " (values.invalid) " invalid" }
   }
}

pub(crate) fn admin(
   csrf: &str,
   max_chars: usize,
   max_lines: usize,
   notice: Option<&AdminNotice>,
) -> Markup {
   page(
      "Import accounts",
      false,
      html! {
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
                     option value="view" { "View" }
                     option value="search" { "Search" }
                  }
               }
               div class="import-field" {
                  label for="import-accounts" { "Account data" }
                  p class="import-help" id="import-help" {
                     "Paste an account export or a Netscape cookie file. Up to "
                     (max_lines) " lines and 256 KiB. Not shown after submit."
                  }
                  textarea id="import-accounts" name="accounts" rows="12" maxlength=(max_chars)
                     aria-describedby="import-help" spellcheck="false" autocapitalize="off" required {}
               }
               button type="submit" class="import-submit" { "Import" }
            }
         }
         p {
            a href=(PUBLIC_PATH) { "Public inventory →" }
         }
      },
   )
}

pub(crate) fn forbidden() -> Markup {
   page(
      "Import accounts",
      false,
      html! {
         div class="import-signin" {
            p { "An administrator session is required." }
            p { a href="/search-pass/login" { "Sign in" } " and return to this page." }
         }
      },
   )
}