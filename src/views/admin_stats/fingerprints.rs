use maud::{
   Markup,
   html,
};

use super::frame;

/// Interactive Castle fingerprint capture explorer inside `/admin/stats`.
/// All rendering stays CSP-clean: zero inline styles and zero inline scripts.
pub(crate) fn fingerprints(theme: &str) -> Markup {
   frame(theme, "Castle fingerprints", "fingerprints", None, html! {
      header class="admin-page-header" {
         p class="admin-eyebrow" { "Operations / Castle debugging" }
         div class="admin-title-row" {
            h1 { "Submitted browser fingerprints" }
            div class="admin-header-actions" {
               a class="admin-btn admin-btn--sm" href="/admin/fingerprint" target="_blank" rel="noopener noreferrer" {
                  "Open capture page ↗"
               }
               button id="fp-copy-capture-url" class="admin-btn admin-btn--sm" type="button" {
                  "Copy capture URL"
               }
               button id="fp-compare-toggle" class="admin-btn admin-btn--sm" type="button" aria-pressed="false" {
                  "Compare A / B"
               }
               button id="fp-refresh" class="admin-btn admin-btn--sm admin-btn--primary" type="button" {
                  "Refresh"
               }
            }
         }
         p {
            "Voluntary captures submitted from "
            code { "/admin/fingerprint" }
            ". Inspect decoded Castle envelopes, font-probe matrices, rendered canvas outputs, "
            "client hints vs wire headers, and side-by-side diffs."
         }
      }

      p id="fp-status" class="admin-notice" role="status" hidden {}

      section class="admin-kpi-grid fp-kpi-grid" aria-label="Capture overview" {
         article class="admin-kpi" {
            span class="admin-kpi-label" { "Submissions" }
            strong id="fp-kpi-total" class="admin-kpi-value" { "—" }
            span id="fp-kpi-shown" class="admin-kpi-sub" { "Loading captures…" }
         }
         article class="admin-kpi" {
            span class="admin-kpi-label" { "Browsers & OS" }
            strong id="fp-kpi-browsers" class="admin-kpi-value" { "—" }
            span id="fp-kpi-platforms" class="admin-kpi-sub" { "—" }
         }
         article class="admin-kpi" {
            span class="admin-kpi-label" { "Castle SDK builds" }
            strong id="fp-kpi-sdks" class="admin-kpi-value" { "—" }
            span id="fp-kpi-keyids" class="admin-kpi-sub" { "—" }
         }
         article class="admin-kpi" {
            span class="admin-kpi-label" { "Token size (avg)" }
            strong id="fp-kpi-token" class="admin-kpi-value" { "—" }
            span id="fp-kpi-latest" class="admin-kpi-sub" { "—" }
         }
      }

      section id="fp-compare-panel" class="admin-stats-panel fp-compare-panel" hidden {
         div class="admin-section-heading" {
            div {
               h2 { "Side-by-side capture diff" }
               p class="admin-muted" {
                  "Compare any two submissions across Castle envelope, environment, wire headers, font presence/widths, and canvas hashes."
               }
            }
            div class="fp-compare-controls" {
               label class="fp-inline-check" {
                  input id="fp-diff-only" type="checkbox" checked;
                  span { "Only differences" }
               }
               button id="fp-compare-swap" class="admin-btn admin-btn--sm" type="button" { "Swap A ↔ B" }
               button id="fp-compare-close" class="admin-btn admin-btn--sm" type="button" { "Close" }
            }
         }
         div class="fp-compare-pickers" {
            label class="admin-filter-field" {
               "Capture A (baseline)"
               select id="fp-select-a" {
                  option value="" { "Choose capture A…" }
               }
            }
            label class="admin-filter-field" {
               "Capture B (target)"
               select id="fp-select-b" {
                  option value="" { "Choose capture B…" }
               }
            }
         }
         div id="fp-compare-body" class="fp-compare-body" {
            p class="admin-empty" { "Select two captures above (or click A / B in the table) to diff them." }
         }
      }

      section class="admin-stats-panel" {
         form id="fp-filter-form" class="admin-filters" {
            label class="admin-filter-field admin-filter-field--search" {
               "Search captures"
               input
                  id="fp-filter-q"
                  type="search"
                  placeholder="ID, note, UA, GPU, timezone, IP, SDK, CUID…";
            }
            label class="admin-filter-field" {
               "Browser"
               select id="fp-filter-browser" {
                  option value="" { "All browsers" }
                  option value="Chrome" { "Chrome" }
                  option value="Firefox" { "Firefox" }
                  option value="Safari" { "Safari" }
                  option value="Edge" { "Edge" }
                  option value="Other" { "Other" }
               }
            }
            label class="admin-filter-field" {
               "Platform"
               select id="fp-filter-os" {
                  option value="" { "All platforms" }
                  option value="Windows" { "Windows" }
                  option value="macOS" { "macOS" }
                  option value="Linux" { "Linux" }
                  option value="Android" { "Android" }
                  option value="iOS" { "iOS" }
                  option value="Other" { "Other" }
               }
            }
            label class="admin-filter-field" {
               "SDK build"
               select id="fp-filter-sdk" {
                  option value="" { "All SDK builds" }
               }
            }
            label class="admin-filter-field" {
               "Sort by"
               select id="fp-filter-sort" {
                  option value="newest" { "Newest first" }
                  option value="oldest" { "Oldest first" }
                  option value="token_desc" { "Token length ↓" }
                  option value="token_asc" { "Token length ↑" }
                  option value="fonts_desc" { "Fonts detected ↓" }
               }
            }
            div class="admin-filters-actions" {
               button id="fp-filter-clear" class="admin-btn" type="button" { "Clear" }
            }
         }
      }

      section class="admin-stats-panel" aria-labelledby="fp-list-title" {
         div class="admin-section-heading" {
            h2 id="fp-list-title" { "Captured sessions" }
            span id="fp-table-count" class="admin-muted" {}
         }
         div class="admin-table-wrap" {
            table class="admin-table fp-list-table" {
               thead {
                  tr {
                     th scope="col" { "Captured (UTC)" }
                     th scope="col" { "Browser & OS" }
                     th scope="col" { "Display & GPU" }
                     th scope="col" { "Castle token" }
                     th scope="col" { "Fonts & canvas" }
                     th scope="col" { "Zone & IP" }
                     th scope="col" { "Diff" }
                  }
               }
               tbody id="fp-list-rows" {
                  tr {
                     td colspan="7" class="admin-empty" { "Loading submitted fingerprints…" }
                  }
               }
            }
         }
      }

      section id="fp-inspector" class="admin-stats-panel fp-inspector" aria-labelledby="fp-inspector-title" hidden {
         div class="fp-inspector-head" {
            div {
               p class="admin-eyebrow" { "Capture inspector" }
               h2 id="fp-inspector-title" { "—" }
               p id="fp-inspector-sub" class="admin-muted" {}
            }
            div class="fp-inspector-actions" {
               button id="fp-copy-token" class="admin-btn admin-btn--sm admin-btn--primary" type="button" {
                  "Copy Castle token"
               }
               button id="fp-copy-json" class="admin-btn admin-btn--sm" type="button" {
                  "Copy JSON"
               }
               button id="fp-download-json" class="admin-btn admin-btn--sm" type="button" {
                  "Download JSON"
               }
               button id="fp-set-a" class="admin-btn admin-btn--sm" type="button" { "Set A" }
               button id="fp-set-b" class="admin-btn admin-btn--sm" type="button" { "Set B" }
               button id="fp-delete" class="admin-btn admin-btn--sm admin-btn--danger" type="button" {
                  "Delete"
               }
            }
         }

         div class="fp-tabs" role="tablist" aria-label="Capture inspection sections" {
            button class="fp-tab is-active" type="button" role="tab" aria-selected="true" data-tab="overview" {
               "Overview & token"
            }
            button class="fp-tab" type="button" role="tab" aria-selected="false" data-tab="fonts" {
               "Font probe"
               span id="fp-tab-count-fonts" class="fp-tab-pill" {}
            }
            button class="fp-tab" type="button" role="tab" aria-selected="false" data-tab="canvas" {
               "Canvas & pixels"
               span id="fp-tab-count-canvas" class="fp-tab-pill" {}
            }
            button class="fp-tab" type="button" role="tab" aria-selected="false" data-tab="headers" {
               "Headers & hints"
            }
            button class="fp-tab" type="button" role="tab" aria-selected="false" data-tab="tape" {
               "Event tape"
               span id="fp-tab-count-tape" class="fp-tab-pill" {}
            }
            button class="fp-tab" type="button" role="tab" aria-selected="false" data-tab="raw" {
               "Raw JSON"
            }
         }

         div id="fp-tab-overview" class="fp-tab-panel" data-panel="overview" {}
         div id="fp-tab-fonts" class="fp-tab-panel" data-panel="fonts" hidden {}
         div id="fp-tab-canvas" class="fp-tab-panel" data-panel="canvas" hidden {}
         div id="fp-tab-headers" class="fp-tab-panel" data-panel="headers" hidden {}
         div id="fp-tab-tape" class="fp-tab-panel" data-panel="tape" hidden {}
         div id="fp-tab-raw" class="fp-tab-panel" data-panel="raw" hidden {}
      }
   })
}

#[cfg(test)]
mod tests {
   use super::fingerprints;

   #[test]
   fn renders_csp_clean_fingerprints_explorer() {
      let dark = fingerprints("Teapot").into_string();
      assert!(dark.contains("data-theme=\"dark\""));
      assert!(dark.contains("/css/admin-captures.css?v=1"));
      assert!(dark.contains("/js/admin-captures.js?v=2"));
      assert!(dark.contains("href=\"/admin/fingerprint\""));
      assert!(!dark.contains("style=\""));
      assert!(!dark.contains("<script>"));

      let light = fingerprints("Catppuccin Latte").into_string();
      assert!(light.contains("data-theme=\"light\""));
   }
}
