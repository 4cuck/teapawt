use maud::{
   Markup,
   html,
};

use super::{
   frame,
   kind_label,
   minute_label,
   pool_badge_class,
};
use crate::{
   routes::{
      OutAccountRow,
      OutAccountsPageData,
      OutAccountsQuery,
   },
   types::SessionKind,
};

const REASONS: [(&str, &str); 9] = [
   ("ct0_dead", "Stale ct0"),
   ("credentials_rejected", "Creds rejected"),
   ("unverified", "Unverified"),
   ("retired", "Retired"),
   ("suspended", "Suspended"),
   ("search_banned", "Search banned"),
   ("profile_banned", "Profile banned"),
   ("refused", "Op refused / 404"),
   ("globally_limited", "Global 429"),
];

const SORTS: [(&str, &str); 4] = [
   ("priority", "Fixable first"),
   ("id_desc", "Newest ID"),
   ("id_asc", "Oldest ID"),
   ("last_active", "Most recently active"),
];

fn scope_heading(query: &OutAccountsQuery) -> (&'static str, &'static str) {
   match query.op.as_deref() {
      Some("SearchTimeline") => {
         (
            "SearchTimeline — Out / unverified",
            "Search-pool accounts currently counted in the red Out / unverified bar for \
             SearchTimeline. Export cookies into your browser, replace stale ct0 tokens, or run a \
             live Search or Health probe.",
         )
      },
      Some("UserTweets") => {
         (
            "UserTweets — Out / unverified",
            "View-pool accounts currently counted in the red Out / unverified bar for UserTweets. \
             Inspect restriction history, export cookies, replace bad ct0 tokens, and test live \
             probes.",
         )
      },
      Some("UserByScreenName") => {
         (
            "UserByScreenName — Out / unverified",
            "View-pool accounts currently counted in the red Out / unverified bar for \
             UserByScreenName. Inspect restriction history, export cookies, replace bad ct0 \
             tokens, and test live probes.",
         )
      },
      Some("TweetDetail") => {
         (
            "TweetDetail — Out / unverified",
            "View-pool accounts currently counted in the red Out / unverified bar for \
             TweetDetail. Inspect restriction history, export cookies, replace bad ct0 tokens, \
             and test live probes.",
         )
      },
      _ => {
         match query.pool.as_deref() {
            Some("demoted") => {
               (
                  "Demoted from search (now in view pool)",
                  "Accounts originally assigned to search that were demoted into the view pool. \
                   Export cookies to inspect in a browser, fix stale ct0 tokens, run a Search \
                   probe, or reinstate them.",
               )
            },
            Some("ct0_dead") => {
               (
                  "Stale ct0 pool (ct0_dead)",
                  "Accounts held out of active search and view slot utilization because their \
                   CSRF token (ct0) expired. They are not brought back on their own. Resurrect \
                   CT0 asks account settings for a replacement, then turns search safety off. \
                   Paste a cookie to restore one by hand.",
               )
            },
            Some("rejected") => {
               (
                  "Rejected / locked pool",
                  "Accounts permanently out of rotation due to locked/rejected credentials (401 / \
                   code 326) or retirement. Excluded from active search and view slot utilization \
                   and never retried automatically.",
               )
            },
            Some("search") => {
               (
                  "Search pool — Out / unverified",
                  "Active search-pool accounts currently out of rotation or unverified for \
                   SearchTimeline (excludes ct0_dead and rejected pools).",
               )
            },
            Some("search-pass") => {
               (
                  "Search-pass pool — Out / unverified",
                  "Active search-pass pool accounts currently out of rotation or unverified for \
                   SearchTimeline (excludes ct0_dead and rejected pools).",
               )
            },
            Some("view") => {
               (
                  "View pool — Out / unverified",
                  "Active view-pool accounts currently out of rotation for UserTweets, \
                   UserByScreenName, or TweetDetail (excludes ct0_dead and rejected pools).",
               )
            },
            _ => {
               (
                  "Active pools — Out / unverified",
                  "Active search and view accounts currently counted in an operation's red Out / \
                   unverified bar. Permanently rejected and stale-ct0 accounts live in their own \
                   tabs.",
               )
            },
         }
      },
   }
}

fn reason_count(data: &OutAccountsPageData, key: &str) -> usize {
   match key {
      "ct0_dead" => data.breakdown.ct0_dead,
      "credentials_rejected" => data.breakdown.credentials_rejected,
      "unverified" => data.breakdown.unverified_search,
      "retired" => data.breakdown.retired,
      "suspended" => data.breakdown.suspended,
      "search_banned" => data.breakdown.search_banned,
      "profile_banned" => data.breakdown.profile_banned,
      "refused" => data.breakdown.refused,
      "globally_limited" => data.breakdown.globally_limited,
      _ => data.breakdown.total,
   }
}

fn pagination_bar(data: &OutAccountsPageData, query: &OutAccountsQuery) -> Markup {
   let shown = data.accounts.len();
   let start = if shown == 0 {
      0
   } else {
      (data.page - 1) * data.page_size + 1
   };
   let end = if shown == 0 { 0 } else { start + shown - 1 };

   html! {
      div class="admin-pagination-bar" {
         div class="admin-result-summary" {
            @if data.total_filtered == 0 {
               span { "0 matching accounts" }
            } @else {
               span {
                  "Showing " (start) "–" (end) " of " (data.total_filtered) " account"
                  @if data.total_filtered != 1 { "s" }
               }
               span class="admin-cursor-tag" {
                  "Page " (data.page) " / " (data.total_pages)
               }
            }
         }
         @if data.total_pages > 1 {
            div class="admin-pagination-links" {
               @if data.page > 1 {
                  a class="admin-pagination" href=(query.to_uri("/admin/stats/out", Some(1))) {
                     "« First"
                  }
                  a class="admin-pagination" href=(query.to_uri("/admin/stats/out", Some(data.page - 1))) {
                     "← Previous"
                  }
               }
               @if data.page < data.total_pages {
                  a class="admin-pagination" href=(query.to_uri("/admin/stats/out", Some(data.page + 1))) {
                     "Next →"
                  }
                  a class="admin-pagination" href=(query.to_uri("/admin/stats/out", Some(data.total_pages))) {
                     "Last »"
                  }
               }
            }
         }
      }
   }
}

pub(crate) fn out_accounts(
   theme: &str,
   data: &OutAccountsPageData,
   query: &OutAccountsQuery,
   canonical_uri: &str,
) -> Markup {
   let (heading, subtitle) = scope_heading(query);
   let op = query.op.as_deref();
   let pool = query.pool.as_deref();
   let active_reason = query.reason.as_deref();
   let active_sort = query.sort.as_deref().unwrap_or("priority");

   let tabs: [(&str, Option<&str>, Option<&str>, usize); 9] = [
      ("Active out", None, None, data.counts.all_out),
      (
         "SearchTimeline",
         Some("SearchTimeline"),
         None,
         data.counts.search_timeline,
      ),
      (
         "Search-pass",
         None,
         Some("search-pass"),
         data.counts.search_pass,
      ),
      (
         "UserTweets (view)",
         Some("UserTweets"),
         None,
         data.counts.user_tweets,
      ),
      (
         "UserByScreenName (view)",
         Some("UserByScreenName"),
         None,
         data.counts.user_by_screen_name,
      ),
      (
         "TweetDetail (view)",
         Some("TweetDetail"),
         None,
         data.counts.tweet_detail,
      ),
      (
         "Demoted from search",
         None,
         Some("demoted"),
         data.counts.demoted_from_search,
      ),
      (
         "Stale ct0 pool",
         None,
         Some("ct0_dead"),
         data.counts.ct0_dead_pool,
      ),
      (
         "Rejected pool",
         None,
         Some("rejected"),
         data.counts.rejected_pool,
      ),
   ];

   let search_mode_default =
      op == Some("SearchTimeline") || matches!(pool, Some("demoted" | "search" | "search-pass"));

   frame(theme, heading, "out", None, html! {
      header class="admin-page-header" {
         a class="admin-text-link" href="/admin/stats" { "← Overview & pools" }
         p class="admin-eyebrow" { "Operations / out & unverified" }
         h1 { (heading) }
         p { (subtitle) }
      }

      div class="admin-stats-toolbar admin-out-scope-bar" {
         nav class="admin-range-tabs" aria-label="Operation and pool scope" {
            @for (label, tab_op, tab_pool, count) in tabs {
               @let active = op == tab_op && (tab_op.is_some() || pool == tab_pool);
               a
                  class=(if active { "admin-range-tab is-active" } else { "admin-range-tab" })
                  href=(query.with_scope(tab_op, tab_pool))
                  aria-current=[active.then_some("page")]
               {
                  (label)
                  " "
                  span class="admin-tab-count" { (count) }
               }
            }
         }
         div class="admin-stats-toolbar-meta" {
            button
               id="out-refresh"
               class="admin-btn"
               type="button"
               data-uri=(canonical_uri)
            {
               "Refresh"
            }
         }
      }

      @if op == Some("SearchTimeline") && data.counts.demoted_from_search > 0 {
         p class="admin-notice admin-notice--warn" {
            (data.counts.demoted_from_search)
            " additional search account"
            @if data.counts.demoted_from_search != 1 { "s were" } @else { " was" }
            " demoted into the view pool. "
            a class="admin-text-link" href=(query.with_scope(None, Some("demoted"))) {
               "Inspect demoted accounts →"
            }
         }
      }

      section class="admin-stats-panel" aria-label="Filter out accounts" {
         div class="admin-reason-chips" role="navigation" aria-label="Filter by reason" {
            @let all_active = active_reason.is_none();
            a
               class=(if all_active { "admin-chip is-active" } else { "admin-chip" })
               href=(query.with_reason(None))
               aria-current=[all_active.then_some("page")]
            {
               "All reasons"
               span class="admin-chip-count" { (data.breakdown.total) }
            }
            @for (key, label) in REASONS {
               @let count = reason_count(data, key);
               @let active = active_reason == Some(key);
               @if count > 0 || active {
                  a
                     class=(if active { "admin-chip is-active" } else { "admin-chip" })
                     href=(query.with_reason(Some(key)))
                     aria-current=[active.then_some("page")]
                  {
                     (label)
                     span class="admin-chip-count" { (count) }
                  }
               }
            }
         }

         form class="admin-filters" action="/admin/stats/out" method="get" {
            @if let Some(selected_op) = op {
               input type="hidden" name="op" value=(selected_op);
            }
            @if let Some(selected_pool) = pool {
               input type="hidden" name="pool" value=(selected_pool);
            }
            @if let Some(selected_reason) = active_reason {
               input type="hidden" name="reason" value=(selected_reason);
            }
            label class="admin-filter-field admin-filter-field--search" {
               "Account or ID"
               input
                  type="search"
                  name="q"
                  value=(query.q.as_deref().unwrap_or(""))
                  placeholder="Username or numeric ID";
            }
            label class="admin-filter-field" {
               "Sort order"
               select name="sort" {
                  @for (value, label) in SORTS {
                     option value=(value) selected[active_sort == value] { (label) }
                  }
               }
            }
            div class="admin-filters-actions" {
               button class="admin-btn admin-btn--primary" type="submit" { "Filter" }
               @if query.has_filters() {
                  a class="admin-btn" href="/admin/stats/out" { "Clear" }
               }
            }
         }
      }

      section class="admin-stats-panel" aria-labelledby="out-accounts-title" {
         div class="admin-section-heading" {
            div {
               h2 id="out-accounts-title" { "Accounts requiring investigation" }
               p { "Use Copy cookies or JSON to import into Cookie-Editor in your browser, Fix ct0 to insert a replacement CSRF token, or Probe to test live against X." }
            }
            @if query.pool.as_deref() == Some("ct0_dead") && data.counts.ct0_dead_pool > 0 {
               button
                  id="resurrect-all-ct0"
                  class="admin-btn admin-resurrect-all-ct0"
                  type="button"
               {
                  "Resurrect all dead CT0"
               }
            }
         }
         p id="out-status" class="admin-notice" role="status" hidden {}

         (pagination_bar(data, query))

         @if data.accounts.is_empty() {
            p class="admin-empty" { "No accounts match the selected operation, pool, and reason filters." }
         } @else {
            div class="admin-table-wrap" {
               table class="admin-table admin-out-table" {
                  thead {
                     tr {
                        th scope="col" { "Account" }
                        th scope="col" { "Pool & ct0" }
                        th scope="col" { "Why out / flags" }
                        th scope="col" class="admin-col-evidence" { "Latest restriction & activity" }
                        th scope="col" { "Actions" }
                     }
                  }
                  tbody {
                     @for row in &data.accounts {
                        (account_row(row, search_mode_default))
                     }
                  }
               }
            }
         }

         (pagination_bar(data, query))
      }
   })
}

fn account_row(row: &OutAccountRow, search_mode_default: bool) -> Markup {
   let entry = &row.entry;
   let id = entry.id;
   let is_cookie = entry.kind == SessionKind::Cookie;
   let can_export = is_cookie && entry.has_auth_token;
   let quarantined = matches!(entry.pool, "ct0_dead" | "rejected");
   let quick_probe_mode = if search_mode_default
      || matches!(entry.home_pool, "search" | "search-pass")
      || entry.demoted_from_search
   {
      "search"
   } else {
      "health"
   };
   let quick_probe_label = if quick_probe_mode == "search" {
      "Search Probe"
   } else {
      "Health Probe"
   };

   html! {
      tr data-account-row=(id) {
         th scope="row" class="admin-nowrap" {
            a href=(format!("/admin/stats/sessions/{id}")) {
               @if entry.username.is_empty() {
                  strong { "Account #" (id) }
               } @else {
                  strong { "@" (entry.username) }
                  small class="admin-muted" { "#" (id) }
               }
            }
            @if !is_cookie {
               small class="admin-muted" { (format!("{:?}", entry.kind)) }
            }
         }
         td class="admin-nowrap" {
            div class="admin-badge-row" {
               span class=(pool_badge_class(entry.pool)) {
                  (entry.pool)
               }
               @if quarantined {
                  span class="admin-badge admin-badge--neutral" title="Active pool this account returns to if repaired or reinstated" {
                     "home: " (entry.home_pool)
                  }
               }
               @if entry.demoted_from_search {
                  span class="admin-badge admin-badge--warn" title="Originally in search pool; demoted to view" {
                     "demoted"
                  }
               }
            }
            small class="admin-muted" {
               @if !is_cookie {
                  "OAuth session"
               } @else if entry.reasons.ct0_dead {
                  "ct0: stale (" (entry.ct0_len) " chars)"
               } @else if entry.ct0_present {
                  "ct0: " (entry.ct0_len) " chars"
               } @else {
                  "ct0: missing"
               }
            }
         }
         td {
            div class="admin-badge-row" {
               @if entry.reasons.ct0_dead {
                  span class="admin-badge admin-badge--bad" { "stale ct0" }
               }
               @if entry.reasons.credentials_rejected {
                  span class="admin-badge admin-badge--bad" { "creds rejected" }
               }
               @if entry.reasons.retired {
                  span class="admin-badge admin-badge--bad" { "retired" }
               }
               @if entry.reasons.unverified_search {
                  span class="admin-badge admin-badge--warn" { "unverified" }
               }
               @if entry.suspended {
                  span class="admin-badge admin-badge--bad" { "suspended" }
               }
               @if entry.reasons.search_banned {
                  span class="admin-badge admin-badge--warn" { "search banned" }
               }
               @if entry.reasons.profile_banned {
                  span class="admin-badge admin-badge--warn" { "profile banned" }
               }
               @if entry.reasons.search_404_cooldown {
                  span class="admin-badge admin-badge--warn" { "404 cooldown" }
               }
               @if entry.reasons.globally_limited {
                  span class="admin-badge admin-badge--neutral" { "global 429" }
               }
               @for op_name in &entry.reasons.refused_operations {
                  span class="admin-badge admin-badge--warn" { "refused: " (op_name) }
               }
               @if entry.filters_failed {
                  span class="admin-badge admin-badge--neutral" { "filters refused" }
               }
               @if entry.search_empty_streak >= 5 {
                  span class="admin-badge admin-badge--warn" { (entry.search_empty_streak) " empty streak" }
               }
            }
         }
         td class="admin-col-evidence" {
            @if let Some(event) = &row.latest_event {
               strong { (kind_label(event.kind)) }
               " · "
               (event.endpoint)
               @if let Some(status) = event.http_status {
                  " · HTTP " (status)
               }
               @if let Some(code) = event.upstream_code {
                  " · code " (code)
               }
               div class="admin-muted" { (event.evidence) }
               small class="admin-muted" {
                  "Observed " (minute_label(event.observed_unix))
                  @if let Some(last) = entry.last_active {
                     " · Active " (minute_label(last))
                  }
               }
            } @else {
               span class="admin-muted" {
                  @if let Some(last) = entry.last_active {
                     "Last active " (minute_label(last))
                  } @else {
                     "No upstream activity or restriction events recorded"
                  }
               }
            }
         }
         td class="admin-nowrap" {
            div class="admin-row-actions" {
               a class="admin-btn admin-btn--sm" href=(format!("/admin/stats/sessions/{id}")) {
                  "Inspect"
               }
               @if can_export {
                  button
                     class="admin-btn admin-btn--sm admin-out-copy-cookies"
                     type="button"
                     data-session=(id)
                     data-username=(entry.username)
                     title="Fetch and copy Cookie-Editor JSON to clipboard"
                  {
                     "Copy cookies"
                  }
                  a
                     class="admin-btn admin-btn--sm"
                     href=(format!("/admin/stats/sessions/{id}/cookies"))
                     download=(format!("account-{id}-cookies.json"))
                     title="Download Cookie-Editor JSON file"
                  {
                     "JSON"
                  }
               }
               @if is_cookie && entry.reasons.ct0_dead {
                  button
                     class="admin-btn admin-btn--sm admin-resurrect-ct0"
                     type="button"
                     data-session=(id)
                  {
                     "Resurrect CT0"
                  }
               }
               @if is_cookie {
                  button
                     class="admin-btn admin-btn--sm admin-out-toggle-ct0"
                     type="button"
                     data-session=(id)
                     aria-expanded="false"
                  {
                     "Fix ct0"
                  }
               }
               button
                  class="admin-btn admin-btn--sm admin-out-probe"
                  type="button"
                  data-session=(id)
                  data-probe-mode=(quick_probe_mode)
               {
                  (quick_probe_label)
               }
            }
         }
      }
      @if is_cookie {
         tr class="admin-inline-editor" data-ct0-row=(id) hidden {
            td colspan="5" {
               div class="admin-inline-ct0-box" data-session=(id) {
                  div class="admin-inline-ct0-head" {
                     strong {
                        "Update ct0 & test probe for "
                        @if entry.username.is_empty() {
                           "Account #" (id)
                        } @else {
                           "@" (entry.username) " (#" (id) ")"
                        }
                     }
                     span class="admin-muted" {
                        "Paste raw ct0 token, ct0=... cookie pair, Cookie header, or Cookie-Editor JSON."
                     }
                  }
                  div class="admin-ct0-form-row" {
                     input
                        class="admin-ct0-input"
                        type="text"
                        name="ct0"
                        autocomplete="off"
                        spellcheck="false"
                        placeholder="Paste new ct0 value (e.g. 160-char hex token or Cookie-Editor JSON)"
                        aria-label=(format!("New ct0 cookie for account {id}"));
                     button
                        class="admin-btn admin-btn--sm admin-ct0-submit"
                        type="button"
                        data-session=(id)
                        data-probe=""
                     {
                        "Save ct0"
                     }
                     button
                        class="admin-btn admin-btn--sm admin-btn--primary admin-ct0-submit"
                        type="button"
                        data-session=(id)
                        data-probe="health"
                     {
                        "Save & Health Probe"
                     }
                     button
                        class="admin-btn admin-btn--sm admin-ct0-submit"
                        type="button"
                        data-session=(id)
                        data-probe="search"
                     {
                        "Save & Search Probe"
                     }
                     button
                        class="admin-btn admin-btn--sm admin-out-probe"
                        type="button"
                        data-session=(id)
                        data-probe-mode="health"
                     {
                        "Health Probe"
                     }
                     button
                        class="admin-btn admin-btn--sm admin-out-probe"
                        type="button"
                        data-session=(id)
                        data-probe-mode="search"
                     {
                        "Search Probe"
                     }
                     button
                        class="admin-btn admin-btn--sm admin-out-close-ct0"
                        type="button"
                        data-session=(id)
                     {
                        "Close"
                     }
                  }
                  p class="admin-notice admin-inline-ct0-status" role="status" hidden {}
               }
            }
         }
      }
   }
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::api::auth::{
      OutAccountEntry,
      OutBreakdown,
      OutOperationCounts,
      OutReasons,
   };

   #[test]
   fn out_page_markup_is_csp_clean_and_renders_actions() {
      let query = OutAccountsQuery {
         op: Some("SearchTimeline".to_owned()),
         ..OutAccountsQuery::default()
      };
      let data = OutAccountsPageData {
         counts:         OutOperationCounts {
            all_out:             15,
            search_timeline:     3,
            search_pass:         0,
            user_tweets:         12,
            user_by_screen_name: 12,
            tweet_detail:        12,
            demoted_from_search: 2,
            ct0_dead_pool:       4,
            rejected_pool:       5,
         },
         breakdown:      OutBreakdown {
            total:                3,
            ct0_dead:             1,
            credentials_rejected: 0,
            unverified_search:    2,
            retired:              0,
            suspended:            0,
            search_banned:        0,
            profile_banned:       0,
            refused:              0,
            globally_limited:     0,
         },
         total_filtered: 1,
         page:           1,
         total_pages:    1,
         page_size:      50,
         accounts:       vec![OutAccountRow {
            entry:        OutAccountEntry {
               id:                  42,
               username:            "cookie_user".to_owned(),
               kind:                SessionKind::Cookie,
               pool:                "search",
               home_pool:           "search",
               demoted_from_search: false,
               has_auth_token:      true,
               ct0_present:         true,
               ct0_len:             160,
               suspended:           false,
               filters_cleared:     false,
               filters_failed:      false,
               verified_search:     false,
               last_active:         Some(1_700_000_000),
               search_empty_streak: 0,
               profile_strikes:     0,
               reasons:             OutReasons {
                  ct0_dead: true,
                  unverified_search: true,
                  ..OutReasons::default()
               },
            },
            latest_event: None,
         }],
         now:            1_700_000_100,
      };
      let markup =
         out_accounts("dark", &data, &query, "/admin/stats/out?op=SearchTimeline").into_string();
      assert!(!markup.contains("style=\""));
      assert!(!markup.contains("<script>"));
      assert!(markup.contains("SearchTimeline — Out / unverified"));
      assert!(markup.contains("@cookie_user"));
      assert!(markup.contains("stale ct0"));
      assert!(markup.contains("Copy cookies"));
      assert!(markup.contains("Fix ct0"));
      assert!(markup.contains("Save &amp; Health Probe"));
      assert!(markup.contains("Save &amp; Search Probe"));
      assert!(markup.contains("Inspect demoted accounts"));
   }
}
