use std::fmt::Write as _;

use maud::{
   DOCTYPE,
   Markup,
   html,
};
use serde::Serialize;
use time::{
   OffsetDateTime,
   format_description::well_known::Rfc3339,
};

mod account;
mod fingerprints;
mod investigation;
mod out;
pub(crate) use account::account;
pub(crate) use fingerprints::fingerprints;
pub(crate) use investigation::restrictions;
pub(crate) use out::out_accounts;

use crate::{
   api::{
      ClearanceStatus,
      auth::{
         OperationPool,
         PoolCensus,
      },
   },
   pressure::Cause,
   routes::anti_ddos::GateStatus,
};

#[derive(Serialize)]
pub(crate) struct Dashboard {
   pub minutes:         i32,
   pub pool:            Option<(i64, i64)>,
   pub capacity:        Option<f64>,
   pub census:          PoolCensus,
   pub gate:            GateStatus,
   pub clearance:       ClearanceStatus,
   pub dropped_events:  u64,
   pub buckets:         Vec<Bucket>,
   pub restrictions:    Vec<RestrictionCount>,
   pub search_accounts: Vec<SearchAccountRow>,
}

#[derive(Serialize)]
pub(crate) struct SearchAccountRow {
   pub session_id:           i64,
   pub username:             String,
   pub pool:                 String,
   pub verified_search:      bool,
   pub filters_cleared:      bool,
   pub filters_failed:       bool,
   pub suspended:            bool,
   pub credentials_rejected: bool,
   pub empty:                i64,
   pub non_empty:            i64,
   pub rate_limited:         i64,
   pub empty_streak:         u32,
   pub remaining:            Option<i32>,
   pub limit:                Option<i32>,
   pub last_empty_unix:      Option<i64>,
   pub last_non_empty_unix:  Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct Bucket {
   pub minute:       i64,
   pub incoming:     i64,
   pub success:      i64,
   pub empty:        i64,
   pub rejected:     i64,
   pub rate_limited: i64,
   pub failed:       i64,
   pub other:        i64,
   /// Turnstile and Anti-DDoS challenges served. Not a fault: the gate
   /// working as designed.
   pub challenged:   i64,
   /// Mean latency for completed requests in this minute, in milliseconds.
   pub latency_ms:   i64,
   pub finished:     i64,
}

#[derive(Serialize)]
pub(crate) struct RestrictionCount {
   pub minute:      i64,
   pub search_only: i64,
   pub suspended:   i64,
}

const TABLE_LIMIT: usize = 120;
const SIX_HOURS_MINUTES: i32 = 6 * 60;
const DAY_MINUTES: i32 = 24 * 60;
const WEEK_MINUTES: i32 = 7 * 24 * 60;
const RANGES: [(i32, &str); 5] = [
   (15, "15m"),
   (60, "1h"),
   (SIX_HOURS_MINUTES, "6h"),
   (DAY_MINUTES, "24h"),
   (WEEK_MINUTES, "7d"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
   Unknown,
   Healthy,
   Degraded,
   Failing,
}

impl Health {
   const fn label(self) -> &'static str {
      match self {
         Self::Unknown => "Idle / No samples",
         Self::Healthy => "Healthy",
         Self::Degraded => "Degraded",
         Self::Failing => "Failing",
      }
   }

   const fn modifier(self) -> &'static str {
      match self {
         Self::Unknown => "idle",
         Self::Healthy => "ok",
         Self::Degraded => "warn",
         Self::Failing => "bad",
      }
   }

   const fn detail(self) -> &'static str {
      match self {
         Self::Unknown => "No origin search/quote traffic was recorded in this window.",
         Self::Healthy => "Under 1% of origin responses were service faults.",
         Self::Degraded => "Between 1% and 5% of origin responses were service faults.",
         Self::Failing => "More than 5% of origin responses were service faults.",
      }
   }
}

#[derive(Default)]
struct TrafficSummary {
   incoming:     i128,
   successful:   i128,
   empty:        i128,
   rejected:     i128,
   rate_limited: i128,
   failed:       i128,
   other:        i128,
   challenged:   i128,
   completed:    i128,
   peak:         Option<i64>,
   observed:     Option<f64>,
   latency:      Option<f64>,
}

impl TrafficSummary {
   const fn answered(&self) -> i128 {
      self.successful + self.empty
   }

   const fn bad(&self) -> i128 {
      self.rejected + self.rate_limited + self.failed
   }

   fn error_rate(&self) -> Option<f64> {
      let total = self.answered() + self.bad();
      (total > 0).then(|| 100.0 * self.bad() as f64 / total as f64)
   }

   fn health(&self) -> Health {
      match self.error_rate() {
         None => Health::Unknown,
         Some(rate) if rate >= 5.0 => Health::Failing,
         Some(rate) if rate >= 1.0 => Health::Degraded,
         Some(_) => Health::Healthy,
      }
   }

   fn from_buckets(buckets: &[&Bucket], current_minute: i64) -> Self {
      let mut summary = Self::default();
      let mut weighted_latency = 0.0;
      let mut closed_answered = 0_i128;
      let mut closed_minutes = 0;
      for bucket in buckets {
         summary.incoming += i128::from(bucket.incoming);
         summary.successful += i128::from(bucket.success);
         summary.empty += i128::from(bucket.empty);
         summary.rejected += i128::from(bucket.rejected);
         summary.rate_limited += i128::from(bucket.rate_limited);
         summary.failed += i128::from(bucket.failed);
         summary.other += i128::from(bucket.other);
         summary.challenged += i128::from(bucket.challenged);
         summary.completed += i128::from(bucket.finished);
         summary.peak = Some(
            summary
               .peak
               .map_or(bucket.incoming, |peak| peak.max(bucket.incoming)),
         );
         weighted_latency += bucket.latency_ms as f64 * bucket.finished as f64;
         if bucket.minute < current_minute {
            closed_answered += i128::from(bucket.success + bucket.empty);
            closed_minutes += 1;
         }
      }
      summary.observed =
         (closed_minutes > 0).then(|| closed_answered as f64 / f64::from(closed_minutes));
      summary.latency =
         (summary.completed > 0).then(|| weighted_latency / summary.completed as f64);
      summary
   }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Sample {
   minute:       i64,
   incoming:     f64,
   answered:     f64,
   empty:        f64,
   rejected:     f64,
   rate_limited: f64,
   failed:       f64,
   other:        f64,
   challenged:   f64,
   latency:      f64,
   error_rate:   f64,
}

impl Sample {
   fn of(bucket: &Bucket) -> Self {
      let answered = (bucket.success + bucket.empty) as f64;
      let bad = (bucket.rejected + bucket.rate_limited + bucket.failed) as f64;
      let total = answered + bad;
      Self {
         minute: bucket.minute,
         incoming: bucket.incoming as f64,
         answered,
         empty: bucket.empty as f64,
         rejected: bucket.rejected as f64,
         rate_limited: bucket.rate_limited as f64,
         failed: bucket.failed as f64,
         other: bucket.other as f64,
         challenged: bucket.challenged as f64,
         // `bucket.latency_ms` is already the per-request average in ms.
         latency: if bucket.finished > 0 {
            bucket.latency_ms as f64
         } else {
            0.0
         },
         error_rate: if total > 0.0 {
            100.0 * bad / total
         } else {
            0.0
         },
      }
   }
}

#[derive(Default)]
struct HourAccumulator {
   hour_unix:        i64,
   count:            usize,
   incoming:         f64,
   answered:         f64,
   empty:            f64,
   rejected:         f64,
   rate_limited:     f64,
   failed:           f64,
   other:            f64,
   challenged:       f64,
   weighted_latency: f64,
   finished:         i64,
}

impl HourAccumulator {
   fn new(hour_unix: i64) -> Self {
      Self {
         hour_unix,
         ..Self::default()
      }
   }

   fn push(&mut self, bucket: &Bucket) {
      let sample = Sample::of(bucket);
      self.count += 1;
      self.incoming += sample.incoming;
      self.answered += sample.answered;
      self.empty += sample.empty;
      self.rejected += sample.rejected;
      self.rate_limited += sample.rate_limited;
      self.failed += sample.failed;
      self.other += sample.other;
      self.challenged += sample.challenged;
      self.weighted_latency += bucket.latency_ms as f64 * bucket.finished as f64;
      self.finished += bucket.finished;
   }

   fn finish(self) -> Sample {
      let denom = (self.count.max(1)) as f64;
      let bad = self.rejected + self.rate_limited + self.failed;
      let total = self.answered + bad;
      Sample {
         minute:       self.hour_unix,
         incoming:     self.incoming / denom,
         answered:     self.answered / denom,
         empty:        self.empty / denom,
         rejected:     self.rejected / denom,
         rate_limited: self.rate_limited / denom,
         failed:       self.failed / denom,
         other:        self.other / denom,
         challenged:   self.challenged / denom,
         latency:      if self.finished > 0 {
            self.weighted_latency / self.finished as f64
         } else {
            0.0
         },
         error_rate:   if total > 0.0 {
            100.0 * bad / total
         } else {
            0.0
         },
      }
   }
}

fn samples(buckets: &[&Bucket], hourly: bool) -> Vec<Sample> {
   if !hourly {
      return buckets.iter().map(|bucket| Sample::of(bucket)).collect();
   }
   let mut out = Vec::new();
   let mut current: Option<HourAccumulator> = None;
   for bucket in buckets {
      let hour_unix = bucket.minute.div_euclid(3600) * 3600;
      match current.as_mut() {
         Some(acc) if acc.hour_unix == hour_unix => acc.push(bucket),
         _ => {
            if let Some(done) = current.replace(HourAccumulator::new(hour_unix)) {
               out.push(done.finish());
            }
            if let Some(acc) = current.as_mut() {
               acc.push(bucket);
            }
         },
      }
   }
   if let Some(done) = current {
      out.push(done.finish());
   }
   out
}

pub(crate) fn timestamp(instant: OffsetDateTime) -> Markup {
   let datetime = instant.format(&Rfc3339).unwrap_or_default();
   let label = format!(
      "{} {:02}:{:02}:{:02} UTC",
      instant.date(),
      instant.hour(),
      instant.minute(),
      instant.second()
   );
   html! { time datetime=(datetime) { (label) } }
}

pub(crate) fn minute_label(minute: i64) -> Markup {
   OffsetDateTime::from_unix_timestamp(minute).map_or_else(|_| html! { "Unknown time" }, timestamp)
}

fn axis_time_label(unix: i64, include_date: bool) -> String {
   let Ok(instant) = OffsetDateTime::from_unix_timestamp(unix) else {
      return String::new();
   };
   if include_date {
      format!(
         "{:02}-{:02} {:02}:{:02}",
         u8::from(instant.month()),
         instant.day(),
         instant.hour(),
         instant.minute()
      )
   } else {
      format!("{:02}:{:02} UTC", instant.hour(), instant.minute())
   }
}

fn theme_mode(theme: &str) -> &'static str {
   let lower = theme.to_ascii_lowercase();
   if lower.contains("light")
      || lower.contains("latte")
      || lower.contains("dawn")
      || lower == "twitter"
   {
      "light"
   } else {
      "dark"
   }
}

pub(crate) fn frame(
   theme: &str,
   title: &str,
   active: &str,
   session_id: Option<i64>,
   content: Markup,
) -> Markup {
   let mode = theme_mode(theme);
   let nav_overview = active == "dashboard" || active == "overview";
   let nav_out = active == "out";
   let nav_restrictions = active == "restrictions" || active == "account";
   let nav_fingerprints = active == "fingerprints";
   html! {
      (DOCTYPE)
      html lang="en" data-theme=(mode) {
         head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            meta name="robots" content="noindex, nofollow";
            title { (title) " · Teapot administration" }
            link rel="stylesheet" href="/css/admin-stats.css?v=9";
            script src="/js/admin-stats.js?v=9" defer="" {}
            @if nav_fingerprints {
               link rel="stylesheet" href="/css/admin-captures.css?v=1";
               script src="/js/admin-captures.js?v=2" defer="" {}
            }
         }
         body data-theme=(mode) {
            div class="admin-layout admin-workspace" {
               nav class="admin-sidebar" aria-label="Administration workspace" {
                  a class="admin-brand" href="/admin/stats" {
                     "TEAPOT"
                     span { "operations" }
                  }
                  a href="/admin/stats" aria-current=[nav_overview.then_some("page")] { "Overview & pools" }
                  a href="/admin/stats/out" aria-current=[nav_out.then_some("page")] { "Out / unverified" }
                  a href="/admin/stats/restrictions" aria-current=[nav_restrictions.then_some("page")] { "Restrictions" }
                  a href="/admin/stats/fingerprints" aria-current=[nav_fingerprints.then_some("page")] { "Fingerprints" }
                  a href="/admin/import" { "Account import" }
                  a class="admin-home" href="/" { "← Back to site" }
               }
               main
                  id="admin-stats-main"
                  class="admin-stats"
                  data-page=(active)
                  data-session-id=[session_id]
               {
                  (content)
               }
            }
         }
      }
   }
}

pub(crate) fn forbidden(theme: &str) -> Markup {
   frame(
      theme,
      "Administrator sign-in required",
      "forbidden",
      None,
      html! {
         header class="admin-page-header" {
            p class="admin-eyebrow" { "Operations / access" }
            h1 { "Administrator sign-in required" }
            p { "Sign in with the administrator pass account to inspect pool telemetry, restriction history, and session probes." }
         }
         section class="admin-stats-panel" {
            p class="admin-notice admin-notice--warn" {
               "Your current browser session does not hold an active administrator pass cookie."
            }
            div class="admin-action-buttons" {
               a class="admin-btn admin-btn--primary" href="/search-pass/login" { "Sign in with pass" }
               a class="admin-btn" href="/" { "Return to home page" }
            }
         }
      },
   )
}

pub(crate) fn kind_label(kind: crate::stats::Restriction) -> &'static str {
   kind.label()
}

pub(crate) fn page(theme: &str, data: Option<&Dashboard>, minutes: i32) -> Markup {
   frame(theme, "Operations overview", "dashboard", None, html! {
      header class="admin-page-header" {
         p class="admin-eyebrow" { "Operations / overview" }
         h1 { "Operations & pool telemetry" }
         p { "Live account-pool capacity, Anti-DDoS pressure state, and origin search/quote request health." }
      }
      (overview_content(data, minutes))
   })
}

fn overview_content(data: Option<&Dashboard>, minutes: i32) -> Markup {
   let buckets = data.map_or_else(Vec::new, |dashboard| {
      let mut sorted: Vec<_> = dashboard.buckets.iter().collect();
      sorted.sort_unstable_by_key(|bucket| bucket.minute);
      sorted
   });
   let now = OffsetDateTime::now_utc();
   let now_unix = now.unix_timestamp();
   let current_minute = now_unix.div_euclid(60) * 60;
   let summary = TrafficSummary::from_buckets(&buckets, current_minute);
   let health = summary.health();
   let hourly = minutes == WEEK_MINUTES;
   let include_date = minutes >= DAY_MINUTES;
   let series = samples(&buckets, hourly);
   let window_end = series.last().map_or(current_minute, |last| {
      if (current_minute - last.minute).abs() > i64::from(minutes) * 120 {
         last.minute
      } else {
         current_minute.max(last.minute)
      }
   });
   let window_start = window_end - i64::from(minutes) * 60;
   let series_rows: Vec<_> = series
      .iter()
      .map(|sample| {
         (
            sample.minute,
            sample.incoming,
            sample.answered,
            sample.empty,
            sample.rejected,
            sample.rate_limited,
            sample.failed,
            sample.other,
            sample.challenged,
            sample.error_rate,
            sample.latency,
         )
      })
      .collect();
   let series_json = serde_json::to_string(&series_rows).unwrap_or_else(|_| "[]".to_owned());
   let estimate = data
      .and_then(|dashboard| dashboard.capacity)
      .filter(|value| value.is_finite());
   let mut restrictions: Vec<_> = data
      .into_iter()
      .flat_map(|dashboard| &dashboard.restrictions)
      .filter(|row| row.search_only > 0 || row.suspended > 0)
      .collect();
   restrictions.sort_unstable_by_key(|row| std::cmp::Reverse(row.minute));

   let error_rate = summary
      .error_rate()
      .map_or_else(|| "—".to_string(), |rate| format!("{rate:.2}%"));

   html! {
      div
         class="admin-stats-content"
         data-minutes=(minutes)
         data-refresh-interval="30"
         data-series=(series_json)
         data-window-start=(window_start)
         data-window-end=(window_end)
         data-hourly=(hourly)
         data-include-date=(include_date)
      {
         div class="admin-stats-toolbar" {
            nav class="admin-range-tabs" aria-label="Time window" {
               @for (value, label) in RANGES {
                  @let active = value == minutes;
                  a
                     class=(if active { "admin-range-tab is-active" } else { "admin-range-tab" })
                     href=(format!("/admin/stats?minutes={value}"))
                     aria-current=[active.then_some("page")]
                  {
                     (label)
                  }
               }
            }
            div class="admin-stats-toolbar-meta" {
               p class="admin-stats-updated" {
                  span id="stats-updated" {
                     @if data.is_some() { (timestamp(now)) }
                     @else { "Telemetry unavailable" }
                  }
               }
               span class="admin-stats-refresh-badge" title="Auto-refreshes every 30 seconds" {
                  span class="admin-stats-live-dot" {}
                  span { "Auto " }
                  span id="stats-countdown" { "30s" }
               }
               button id="stats-refresh-now" class="admin-btn" type="button" { "Refresh" }
            }
         }

         p id="stats-status" class="admin-notice admin-notice--error" role="status" hidden[data.is_some()] {
            @if data.is_none() {
               "Statistics are temporarily unavailable. Use Refresh to retry."
            }
         }

         div class="admin-hero-grid" {
            div class=(format!("admin-verdict admin-verdict--{}", health.modifier())) {
               div class="admin-verdict-main" {
                  span class="admin-verdict-dot" {}
                  strong { "Origin search health: " (health.label()) }
                  span class="admin-verdict-rate" { (error_rate) " fault share" }
               }
               p class="admin-verdict-detail" { (health.detail()) }
               dl class="admin-verdict-facts" {
                  div { dt { "Incoming" } dd { (summary.incoming) } }
                  div { dt { "Answered" } dd { (summary.answered()) } }
                  div { dt { "Faults" } dd { (summary.bad()) } }
                  div { dt { "Challenged" } dd { (summary.challenged) } }
                  div {
                     dt { "Mean latency" }
                     dd {
                        (summary.latency.map_or_else(|| "—".to_string(), |value| format!("{value:.0} ms")))
                     }
                  }
                  div {
                     dt { "Peak rate" }
                     dd {
                        (summary.peak.map_or_else(|| "—".to_string(), |peak| format!("{peak}/m")))
                     }
                  }
               }
            }

            @if let Some(dashboard) = data {
               (gate_card(&dashboard.gate, now_unix))
               (clearance_card(&dashboard.clearance))
            }
         }

         @if let Some(dashboard) = data {
            (pools_section(&dashboard.census, dashboard.pool, estimate, dashboard.dropped_events))
         }

         section class="admin-section" aria-labelledby="stats-red-title" {
            div class="admin-section-heading" {
               div {
                  h2 id="stats-red-title" { "Search & quote origin traffic" }
                  p {
                     "Tracks /search, /hashtag/*, /*/search, and /*/status/*/quotes reaching origin. "
                     @if hourly {
                        "For 7d, each point is the hourly mean rate and weighted latency (UTC)."
                     } @else {
                        "Each point is one recorded minute (UTC); unrecorded gaps are left unbridged."
                     }
                  }
               }
               div class="admin-chart-controls" hidden[series.is_empty()] {
                  button id="chart-mode-raw" class="admin-range-tab is-active" type="button" data-smooth="1" { "Raw" }
                  button id="chart-mode-smooth" class="admin-range-tab" type="button" data-smooth="3" { "3-pt smooth" }
                  button id="chart-unpin" class="admin-btn" type="button" hidden { "Unpin" }
                  button id="chart-reset-zoom" class="admin-btn" type="button" hidden { "Reset zoom" }
               }
            }
            div id="stats-inspector" class="admin-inspector" hidden[series.is_empty()] {
               span id="inspector-hint" class="admin-muted" {
                  "Hover any chart to inspect synchronized metrics · Click to pin · Drag horizontally to zoom"
               }
               div id="inspector-values" class="admin-inspector-grid" hidden {}
            }
            p id="stats-traffic-empty" class="admin-empty" hidden[!series.is_empty()] {
               @if data.is_some() { "No origin search/quote samples were recorded in this window." }
               @else { "Traffic samples are unavailable." }
            }
            div id="stats-red" class="admin-panel-grid" hidden[series.is_empty()] {
               @for spec in &PANELS {
                  (panel(&series, spec, (window_start, window_end), hourly, include_date))
               }
            }
         }

         section class="admin-section" aria-labelledby="stats-breakdown-title" hidden[series.is_empty()] {
            div class="admin-section-heading" {
               div {
                  h2 id="stats-breakdown-title" { "Fault & non-answer breakdown" }
                  p { "Separated per HTTP/pool failure class so small 429 bursts remain visible." }
               }
            }
            @if summary.bad() == 0 && summary.other == 0 {
               p class="admin-zero-faults" {
                  "0 origin faults, rate-limits, or unclassified errors recorded in this window."
               }
            } @else {
               div id="stats-breakdown" class="admin-panel-grid admin-panel-grid--compact" {
                  @for spec in &BREAKDOWN {
                     (panel(&series, spec, (window_start, window_end), hourly, include_date))
                  }
               }
            }
         }

         section class="admin-section" aria-labelledby="stats-table-title" hidden[buckets.is_empty()] {
            details id="stats-traffic-table" class="admin-details" {
               summary {
                  "Raw minute buckets ("
                  (buckets.len().min(TABLE_LIMIT))
                  @if buckets.len() > TABLE_LIMIT { " of " (buckets.len()) }
                  " rows)"
               }
               div class="admin-table-wrap admin-stats-table-scroll" role="region" aria-label="Traffic data table" tabindex="0" {
                  table class="admin-table" {
                     caption {
                        "Origin outcomes by minute (UTC). "
                        span id="stats-table-limit" hidden[buckets.len() <= TABLE_LIMIT] {
                           "Showing the latest " (TABLE_LIMIT) " recorded minutes."
                        }
                     }
                     thead {
                        tr {
                           @for label in ["Minute (UTC)", "Incoming", "Answered", "Empty", "Denied", "429", "Failed", "Other", "Challenged", "Fault %", "Latency"] {
                              th scope="col" { (label) }
                           }
                        }
                     }
                     tbody id="stats-rows" {
                        @for bucket in buckets.iter().rev().take(TABLE_LIMIT) {
                           @let sample = Sample::of(bucket);
                           tr data-minute=(bucket.minute) {
                              th scope="row" class="admin-col-time" { (minute_label(bucket.minute)) }
                              td { (bucket.incoming) }
                              td { (bucket.success + bucket.empty) }
                              td { (bucket.empty) }
                              td { (bucket.rejected) }
                              td { (bucket.rate_limited) }
                              td { (bucket.failed) }
                              td { (bucket.other) }
                              td { (bucket.challenged) }
                              td { (format!("{:.1}%", sample.error_rate)) }
                              td {
                                 @if bucket.finished > 0 { (format!("{:.0} ms", sample.latency)) }
                                 @else { "—" }
                              }
                           }
                        }
                     }
                  }
               }
            }
         }

         @if let Some(dashboard) = data {
            (search_empty_accounts_section(&dashboard.search_accounts))
         }

         section class="admin-section" aria-labelledby="stats-restrictions-title" {
            div class="admin-section-heading" {
               div {
                  h2 id="stats-restrictions-title" { "Restriction activity in selected window" }
                  p { "Per-minute restriction events logged during this range. Full 30-day history is available under Restrictions." }
               }
               a class="admin-btn" href="/admin/stats/restrictions" { "Investigate restrictions →" }
            }
            p id="stats-restrictions-empty" class="admin-empty" hidden[!restrictions.is_empty()] {
               "No account restriction events were recorded in this window."
            }
            div id="stats-restrictions-list" class="stats-restrictions-list" hidden[restrictions.is_empty()] {
               @for row in restrictions.into_iter().take(12) {
                  div class="stats-restriction-row" {
                     (minute_label(row.minute))
                     div class="admin-badge-row" {
                        @if row.search_only > 0 {
                           a class="stats-restriction-badge stats-restriction-badge--search" href="/admin/stats/restrictions?kind=1" {
                              (row.search_only) " search-only / banned"
                           }
                        }
                        @if row.suspended > 0 {
                           a class="stats-restriction-badge stats-restriction-badge--suspended" href="/admin/stats/restrictions?kind=2" {
                              (row.suspended) " suspended"
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

fn gate_card(gate: &GateStatus, now_unix: i64) -> Markup {
   let pressure = &gate.pressure;
   let (tone, badge_tone, state_label, summary_line) = if !gate.armed {
      (
         "idle",
         "neutral",
         "Unarmed",
         "Anti-DDoS is disabled or Turnstile keys are not configured.",
      )
   } else if gate.everyone {
      (
         "warn",
         "warn",
         "Active — Everyone",
         "Manual override is challenging every unverified visitor across gated pages.",
      )
   } else if gate.checking_everyone {
      let reason_text = match pressure.cause {
         Some(Cause::Sustained) => "Sustained profile-timeline fallback crossed threshold.",
         Some(Cause::Spike) | None => "Profile-timeline fallback spike crossed flood threshold.",
      };
      ("bad", "bad", "Active — Flood", reason_text)
   } else if pressure.streak_minutes > 0 {
      (
         "warn",
         "warn",
         "Armed — Watching streak",
         "Fallback is above the sustained threshold; gate activates if streak reaches target.",
      )
   } else {
      (
         "ok",
         "ok",
         "Armed — Calm",
         "UserTweets pool is keeping up with demand; automatic gate is standing by.",
      )
   };

   let hold_remaining = pressure
      .until
      .map(|until| (until - now_unix).max(0))
      .filter(|&secs| secs > 0);

   let sustained_label = if pressure.sustained_percent == 0 {
      "Off".to_string()
   } else {
      format!(
         "{}/{}m @ ≥{}%",
         pressure.streak_minutes, pressure.sustained_minutes, pressure.sustained_percent
      )
   };

   html! {
      div class=(format!("admin-gate-card admin-gate-card--{tone}")) {
         div class="admin-gate-header" {
            strong { "Anti-DDoS & timeline pressure" }
            span class=(format!("admin-badge admin-badge--{badge_tone}")) { (state_label) }
         }
         p class="admin-gate-detail" {
            (summary_line)
            @if let Some(secs) = hold_remaining {
               " Hold remaining: " ((secs + 59) / 60) "m."
            }
         }
         dl class="admin-gate-facts" {
            div {
               dt { "Fallback share" }
               dd {
                  (pressure.share) "%"
                  @if pressure.sample > 0 {
                     " (" (pressure.sample) "/m)"
                  }
               }
            }
            div {
               dt { "Sustained trigger" }
               dd { (sustained_label) }
            }
            div {
               dt { "Spike / hold" }
               dd { "≥" (pressure.flood_percent) "% · " (pressure.hold_minutes) "m" }
            }
            div {
               dt { "Pass / edges" }
               dd {
                  (gate.pass_minutes) "m · "
                  (gate.blocked_networks) " ASN · "
                  (gate.watched_countries) " CC"
               }
            }
         }
      }
   }
}

/// The search `cf_clearance` (Cloudflare JSD) flow and its circuit breaker.
fn clearance_card(status: &ClearanceStatus) -> Markup {
   let (tone, badge, label, detail): (&str, &str, &str, &str) = if !status.enabled {
      (
         "idle",
         "neutral",
         "Disabled",
         "searchClearance is off. Search runs without a cf_clearance cookie.",
      )
   } else if status.tripped {
      (
         "bad",
         "bad",
         "Tripped — disabled",
         "A JSD solve failed, so the whole clearance flow is off until restart. No more x.com \
          clearance requests are made; search continues without the cookie.",
      )
   } else if status.successes == 0 {
      (
         "warn",
         "warn",
         "Armed — no mint yet",
         "Waiting for the first search to visit x.com and mint a cf_clearance.",
      )
   } else {
      (
         "ok",
         "ok",
         "Active",
         "Solving Cloudflare's JSD and attaching cf_clearance to search calls.",
      )
   };

   html! {
      div class=(format!("admin-gate-card admin-gate-card--{tone}")) {
         div class="admin-gate-header" {
            strong { "Search cf_clearance (JSD)" }
            span class=(format!("admin-badge admin-badge--{badge}")) { (label) }
         }
         p class="admin-gate-detail" {
            (detail)
            @if status.tripped {
               @if let Some(reason) = &status.reason {
                  " Reason: " (reason) "."
               }
            }
         }
         dl class="admin-gate-facts" {
            div {
               dt { "Sessions cleared" }
               dd { (status.active_sessions) }
            }
            div {
               dt { "Mints" }
               dd { (status.successes) " / " (status.attempts) }
            }
            div {
               dt { "Failures" }
               dd { (status.failures) }
            }
            @if let Some(at) = &status.last_ok {
               div {
                  dt { "Last mint" }
                  dd { (at) }
               }
            }
            @if let Some(at) = &status.tripped_at {
               div {
                  dt { "Tripped at" }
                  dd { (at) }
               }
            }
            @if !status.tripped {
               @if let Some(err) = &status.last_error {
                  div {
                     dt { "Last error" }
                     dd { (err) }
                  }
               }
            }
         }
      }
   }
}

pub(super) fn pool_badge_class(pool: &str) -> &'static str {
   match pool {
      "search" => "admin-badge admin-badge--search",
      "ct0_dead" => "admin-badge admin-badge--warn",
      "rejected" => "admin-badge admin-badge--bad",
      _ => "admin-badge admin-badge--view",
   }
}

fn pools_section(
   census: &PoolCensus,
   stored_pool: Option<(i64, i64)>,
   estimate: Option<f64>,
   dropped_events: u64,
) -> Markup {
   let pool_value = stored_pool
      .map(|(search, view)| format!("{search} / {view}"))
      .unwrap_or_else(|| format!("{} / {}", census.search, census.view));

   html! {
      section id="stats-pools" class="admin-section" aria-labelledby="stats-pool-title" {
         div class="admin-section-heading" {
            div {
               h2 id="stats-pool-title" { "Live account pools & operation capacity" }
               p { "Real-time slot states across active search and view pools. Permanently rejected/locked and stale-ct0 accounts are separated into their own pools so active utilization stays accurate." }
            }
            a class="admin-btn" href="/admin/import" { "Import accounts" }
         }

         dl class="admin-kpi-grid admin-stats-metrics" {
            (metric(
               "stats-pool",
               "Active search / view",
               Some(pool_value),
               &format!(
                  "{} loaded ({} search · {} view · {} ct0_dead · {} rejected)",
                  census.total,
                  census.search,
                  census.view,
                  census.ct0_dead_pool,
                  census.rejected_pool
               ),
               "",
            ))
            (metric(
               "stats-estimate",
               "Search budget ceiling",
               estimate.map(|val| format!("{val:.1} calls/min")),
               "50 calls / 15m per verified search account",
               "",
            ))
            div class=(if census.ct0_dead_pool > 0 { "admin-kpi admin-stats-metric admin-kpi--warn admin-stats-metric--warn" } else { "admin-kpi admin-stats-metric" }) {
               dt { "Stale ct0 pool (ct0_dead)" }
               dd id="stats-ct0-dead-pool" {
                  @if census.ct0_dead_pool > 0 {
                     a href="/admin/stats/out?pool=ct0_dead" { (census.ct0_dead_pool) }
                  } @else {
                     "0"
                  }
               }
               dd class="admin-stats-note" { "Excluded from active slots · replace ct0 to restore" }
            }
            div class=(if census.rejected_pool > 0 { "admin-kpi admin-stats-metric admin-kpi--bad admin-stats-metric--bad" } else { "admin-kpi admin-stats-metric" }) {
               dt { "Rejected / locked pool" }
               dd id="stats-rejected-pool" {
                  @if census.rejected_pool > 0 {
                     a href="/admin/stats/out?pool=rejected" { (census.rejected_pool) }
                  } @else {
                     "0"
                  }
               }
               dd class="admin-stats-note" {
                  @if census.credentials_rejected > 0 {
                     a href="/admin/stats/out?pool=rejected&reason=credentials_rejected" { (census.credentials_rejected) " bad auth" }
                  } @else {
                     (census.credentials_rejected) " bad auth"
                  }
                  " · "
                  @if census.retired > 0 {
                     a href="/admin/stats/out?pool=rejected&reason=retired" { (census.retired) " retired" }
                  } @else {
                     (census.retired) " retired"
                  }
                  " · never retried"
               }
            }
            div class=(if census.demoted_from_search > 0 { "admin-kpi admin-stats-metric admin-kpi--warn admin-stats-metric--warn" } else { "admin-kpi admin-stats-metric" }) {
               dt { "Demoted from search" }
               dd id="stats-demoted" {
                  @if census.demoted_from_search > 0 {
                     a href="/admin/stats/out?pool=demoted" { (census.demoted_from_search) }
                  } @else {
                     "0"
                  }
               }
               dd class="admin-stats-note" { "Queued for trickle SearchTimeline recovery" }
            }
            (metric(
               "stats-bans",
               "Bans & suspensions",
               Some(format!("{} / {}", census.suspended, census.profile_banned)),
               &format!(
                  "{} suspended · {} search-banned · {} profile-banned",
                  census.suspended, census.search_banned, census.profile_banned
               ),
               "",
            ))
            (metric(
               "stats-filters",
               "NSFW filter sync",
               Some(format!("{} cleared", census.filters_cleared)),
               &format!("{} settings-write refused", census.settings_refused),
               "",
            ))
            (metric(
               "stats-parked",
               "Parked calls / global 429",
               Some(format!("{} / {}", census.parked_calls, census.globally_limited)),
               &format!("In-flight waits / cooled · {} dropped stat events", dropped_events),
               if census.parked_calls > 0 || dropped_events > 0 { "warn" } else { "" },
            ))
         }

         @if !census.operations.is_empty() {
            div class="admin-table-wrap" {
               table class="admin-table admin-ops-table" {
                  caption {
                     div class="admin-bar-legend" {
                        span { i class="admin-legend-swatch admin-legend-swatch--free" {} "Free" }
                        span { i class="admin-legend-swatch admin-legend-swatch--busy" {} "Mid-call" }
                        span { i class="admin-legend-swatch admin-legend-swatch--held" {} "Held" }
                        span { i class="admin-legend-swatch admin-legend-swatch--resting" {} "Resting (429)" }
                        span { i class="admin-legend-swatch admin-legend-swatch--spent" {} "Window spent" }
                        a class="admin-legend-link" href="/admin/stats/out" title="Inspect all Out / unverified accounts" {
                           i class="admin-legend-swatch admin-legend-swatch--out" {}
                           "Out / unverified"
                        }
                     }
                  }
                  thead {
                     tr {
                        th scope="col" { "GraphQL operation" }
                        th scope="col" { "Pool" }
                        th scope="col" { "Slot utilization" }
                        th scope="col" { "Free" }
                        th scope="col" { "Mid-call" }
                        th scope="col" { "Resting" }
                        th scope="col" { "Spent" }
                        th scope="col" { "Out" }
                        th scope="col" { "Pool total" }
                     }
                  }
                  tbody {
                     @for op in &census.operations {
                        (operation_row(op))
                     }
                  }
               }
            }
         }
      }
   }
}

fn operation_row(op: &OperationPool) -> Markup {
   let total = op.accounts.max(1) as f64;
   let non_out_segments = [
      (op.free, "admin-bar-fill--free"),
      (op.busy, "admin-bar-fill--busy"),
      (op.held, "admin-bar-fill--held"),
      (op.resting, "admin-bar-fill--resting"),
      (op.spent, "admin-bar-fill--spent"),
   ];
   let mut cursor_x = 0.0_f64;
   let mut rects = Vec::new();
   for (count, class_name) in non_out_segments {
      if count == 0 {
         continue;
      }
      let width = (count as f64 * 100.0 / total).clamp(0.0, 100.0 - cursor_x);
      rects.push((format!("{cursor_x:.2}"), format!("{width:.2}"), class_name));
      cursor_x = (cursor_x + width).min(100.0);
   }

   let out_href = format!("/admin/stats/out?op={}", op.operation);
   let out_rect = (op.out > 0).then(|| {
      let raw_width = (op.out as f64 * 100.0 / total).clamp(0.0, 100.0);
      let width = raw_width.max(3.0).min(100.0);
      let x = (100.0 - width).min(cursor_x);
      (format!("{x:.2}"), format!("{width:.2}"))
   });

   let pool_badge = if op.pool == "search" {
      "admin-badge admin-badge--search"
   } else {
      "admin-badge admin-badge--view"
   };

   html! {
      tr {
         th scope="row" { (op.operation) }
         td { span class=(pool_badge) { (op.pool) } }
         td {
            svg
               class="admin-bar-svg"
               viewBox="0 0 100 10"
               preserveAspectRatio="none"
               role="group"
               aria-label=(format!("{} slot utilization", op.operation))
            {
               @for (x, width, class_name) in &rects {
                  rect x=(x) y="0" width=(width) height="10" class=(class_name) {}
               }
               @if let Some((x, width)) = &out_rect {
                  a
                     href=(out_href)
                     class="admin-bar-link"
                     aria-label=(format!("Inspect {} out or unverified {} accounts", op.out, op.operation))
                  {
                     rect x=(x) y="0" width=(width) height="10" class="admin-bar-fill--out" {
                        title { (op.out) " out / unverified — click to inspect" }
                     }
                  }
               }
            }
         }
         td { (op.free) }
         td { (op.busy + op.held) }
         td { (op.resting) }
         td { (op.spent) }
         td {
            @if op.out > 0 {
               a
                  class="admin-out-link"
                  href=(out_href)
                  title=(format!("Inspect {} out / unverified {} accounts", op.out, op.operation))
               {
                  (op.out)
               }
            } @else {
               "0"
            }
         }
         td { (op.accounts) }
      }
   }
}

fn metric(id: &str, title: &str, value: Option<String>, note: &str, modifier: &str) -> Markup {
   let class = if modifier.is_empty() {
      "admin-kpi admin-stats-metric".to_string()
   } else {
      format!("admin-kpi admin-stats-metric admin-kpi--{modifier} admin-stats-metric--{modifier}")
   };
   html! {
      div class=(class) {
         dt { (title) }
         dd id=(id) { (value.as_deref().unwrap_or("Unavailable")) }
         dd class="admin-stats-note" { (note) }
      }
   }
}

fn search_empty_accounts_section(rows: &[SearchAccountRow]) -> Markup {
   let suspect_count = rows
      .iter()
      .filter(|row| {
         let total = row.empty + row.non_empty;
         row.verified_search
            && ((row.empty >= 5 && total > 0 && row.empty * 2 >= total)
               || row.empty_streak >= crate::api::auth::SEARCH_EMPTY_STREAK_ALERT)
      })
      .count();

   html! {
      section id="stats-search-empty-accounts" class="admin-section" aria-labelledby="stats-search-empty-title" {
         div class="admin-section-heading" {
            div {
               h2 id="stats-search-empty-title" { "Search pool empty-response tracker" }
               p {
                  "Per-account SearchTimeline empty vs non-empty responses recorded in PostgreSQL for this window. "
                  "Supposedly valid (verified) accounts with high empty ratios or active streaks are sorted first."
               }
            }
            div class="admin-badge-row" {
               @if suspect_count > 0 {
                  span class="admin-badge admin-badge--bad" {
                     (suspect_count) " verified account" @if suspect_count != 1 { "s" } " ≥50% empty"
                  }
               }
               a class="admin-btn" href="/admin/stats/restrictions?kind=12" { "Empty-streak alerts →" }
            }
         }

         @if rows.is_empty() {
            p class="admin-empty" {
               "No empty or rate-limited SearchTimeline account responses were recorded in this window."
            }
         } @else {
            div class="admin-table-wrap admin-stats-table-scroll" role="region" aria-label="Search account empty responses" tabindex="0" {
               table class="admin-table" {
                  thead {
                     tr {
                        th scope="col" { "Account" }
                        th scope="col" { "Pool & status" }
                        th scope="col" { "Empty" }
                        th scope="col" { "With results" }
                        th scope="col" { "Empty %" }
                        th scope="col" { "Live streak" }
                        th scope="col" { "RL switches" }
                        th scope="col" { "Quota" }
                        th scope="col" { "Last empty (UTC)" }
                     }
                  }
                  tbody {
                     @for row in rows {
                        @let total = row.empty + row.non_empty;
                        @let empty_pct = if total > 0 { 100.0 * row.empty as f64 / total as f64 } else { 0.0 };
                        @let pct_class = if row.empty >= 5 && empty_pct >= 80.0 {
                           "admin-badge admin-badge--bad"
                        } else if row.empty >= 5 && empty_pct >= 40.0 {
                           "admin-badge admin-badge--warn"
                        } else {
                           "admin-badge admin-badge--neutral"
                        };
                        @let streak_class = if row.empty_streak >= crate::api::auth::SEARCH_EMPTY_STREAK_ALERT {
                           "admin-badge admin-badge--bad"
                        } else if row.empty_streak >= 5 {
                           "admin-badge admin-badge--warn"
                        } else {
                           "admin-badge admin-badge--neutral"
                        };
                        tr {
                           th scope="row" class="admin-nowrap" {
                              a href=(format!("/admin/stats/sessions/{}", row.session_id)) {
                                 @if row.username.is_empty() {
                                    strong { "Account #" (row.session_id) }
                                 } @else {
                                    strong { "@" (row.username) }
                                    " "
                                    small class="admin-muted" { "#" (row.session_id) }
                                 }
                              }
                           }
                           td {
                              div class="admin-badge-row" {
                                 span class=(pool_badge_class(&row.pool)) {
                                    (row.pool)
                                 }
                                 @if row.verified_search {
                                    span class="admin-badge admin-badge--ok" { "verified" }
                                 } @else {
                                    span class="admin-badge admin-badge--neutral" { "unverified" }
                                 }
                                 @if row.filters_failed {
                                    span class="admin-badge admin-badge--warn" { "filters refused" }
                                 } @else if !row.filters_cleared {
                                    span class="admin-badge admin-badge--neutral" { "filters pending" }
                                 }
                                 @if row.credentials_rejected {
                                    span class="admin-badge admin-badge--bad" { "creds rejected" }
                                 }
                                 @if row.suspended {
                                    span class="admin-badge admin-badge--bad" { "suspended" }
                                 }
                              }
                           }
                           td { (row.empty) }
                           td { (row.non_empty) }
                           td class="admin-nowrap" {
                              @if total > 0 {
                                 span class=(pct_class) { (format!("{empty_pct:.1}%")) }
                              } @else {
                                 "—"
                              }
                           }
                           td class="admin-nowrap" {
                              @if row.empty_streak > 0 {
                                 span class=(streak_class) { (row.empty_streak) }
                              } @else {
                                 "0"
                              }
                           }
                           td { (row.rate_limited) }
                           td class="admin-nowrap" {
                              @if let (Some(rem), Some(lim)) = (row.remaining, row.limit) {
                                 (rem) "/" (lim)
                              } @else {
                                 "—"
                              }
                           }
                           td class="admin-col-time" {
                              @if let Some(ts) = row.last_empty_unix {
                                 (minute_label(ts))
                              } @else {
                                 "—"
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
}

struct Panel {
   key:       &'static str,
   title:     &'static str,
   unit:      &'static str,
   note:      &'static str,
   tone:      &'static str,
   value:     fn(&Sample) -> f64,
   threshold: Option<f64>,
}

const PANELS: [Panel; 4] = [
   Panel {
      key:       "incoming",
      title:     "Request rate",
      unit:      "req/min",
      note:      "Origin search & quote requests per recorded minute.",
      tone:      "rate",
      value:     |sample| sample.incoming,
      threshold: None,
   },
   Panel {
      key:       "error_rate",
      title:     "Fault rate",
      unit:      "%",
      note:      "Share of non-challenge responses that failed (dashed line at 5%).",
      tone:      "errors",
      value:     |sample| sample.error_rate,
      threshold: Some(5.0),
   },
   Panel {
      key:       "latency",
      title:     "Mean response time",
      unit:      "ms",
      note:      "Average origin latency weighted by completed requests.",
      tone:      "latency",
      value:     |sample| sample.latency,
      threshold: None,
   },
   Panel {
      key:       "challenged",
      title:     "Challenges served",
      unit:      "resp/min",
      note:      "Turnstile checks served to visitors (excluded from fault rate).",
      tone:      "challenged",
      value:     |sample| sample.challenged,
      threshold: None,
   },
];

const BREAKDOWN: [Panel; 4] = [
   Panel {
      key:       "rejected",
      title:     "Access denied",
      unit:      "resp/min",
      note:      "HTTP 401/402/403 responses.",
      tone:      "rejected",
      value:     |sample| sample.rejected,
      threshold: None,
   },
   Panel {
      key:       "rate_limited",
      title:     "Rate limited",
      unit:      "resp/min",
      note:      "HTTP 429s returned when search budget was exhausted.",
      tone:      "rate-limited",
      value:     |sample| sample.rate_limited,
      threshold: None,
   },
   Panel {
      key:       "failed",
      title:     "Upstream failed",
      unit:      "resp/min",
      note:      "Requests that failed upstream after retry.",
      tone:      "failed",
      value:     |sample| sample.failed,
      threshold: None,
   },
   Panel {
      key:       "other",
      title:     "Other non-2xx",
      unit:      "resp/min",
      note:      "Other non-fault status codes (such as 404).",
      tone:      "other",
      value:     |sample| sample.other,
      threshold: None,
   },
];

fn format_panel_val(value: f64, unit: &str) -> String {
   if unit == "%" {
      format!("{value:.1}%")
   } else if unit == "ms" {
      format!("{value:.0} ms")
   } else if value >= 100.0 || (value - value.round()).abs() < 0.05 {
      format!("{value:.0}")
   } else {
      format!("{value:.1}")
   }
}

fn panel(
   samples: &[Sample],
   panel: &Panel,
   (window_start, window_end): (i64, i64),
   hourly: bool,
   include_date: bool,
) -> Markup {
   let values: Vec<(i64, f64)> = samples
      .iter()
      .map(|sample| (sample.minute, (panel.value)(sample)))
      .collect();
   let peak = values.iter().map(|(_, val)| *val).fold(0.0_f64, f64::max);
   let latest = values.last().map_or(0.0, |(_, val)| *val);
   let mean = if values.is_empty() {
      0.0
   } else {
      values.iter().map(|(_, val)| *val).sum::<f64>() / values.len() as f64
   };
   let maximum = peak.max(panel.threshold.unwrap_or(0.0)).max(1.0);
   let span = (window_end - window_start).max(0);
   let span_denom = span.max(60) as f64;
   let max_gap = if hourly { 7200_i64 } else { 180_i64 };

   let x_at = |minute: i64| -> f64 {
      16.0 + 728.0 * ((minute - window_start).clamp(0, span) as f64 / span_denom)
   };
   let y_at = |value: f64| -> f64 { 240.0 - 224.0 * (value.clamp(0.0, maximum) / maximum) };

   // Split into contiguous runs so missing minutes are not connected by fake
   // lines.
   let mut segments: Vec<Vec<(f64, f64)>> = Vec::new();
   let mut prev_minute: Option<i64> = None;
   for &(minute, value) in &values {
      let point = (x_at(minute), y_at(value));
      if let Some(prev) = prev_minute
         && minute.saturating_sub(prev) <= max_gap
         && let Some(seg) = segments.last_mut()
      {
         seg.push(point);
      } else {
         segments.push(vec![point]);
      }
      prev_minute = Some(minute);
   }

   let mut line = String::new();
   let mut area = String::new();
   let mut dots: Vec<(String, String)> = Vec::new();

   for seg in &segments {
      if let [(pt_x, pt_y)] = seg.as_slice() {
         dots.push((format!("{pt_x:.2}"), format!("{pt_y:.2}")));
         continue;
      }
      if let Some(&(first_x, first_y)) = seg.first()
         && let Some(&(last_x, _)) = seg.last()
      {
         if !line.is_empty() {
            line.push(' ');
         }
         let _ = write!(line, "M{first_x:.2} {first_y:.2}");
         if !area.is_empty() {
            area.push(' ');
         }
         let _ = write!(area, "M{first_x:.2} {first_y:.2}");
         for &(pt_x, pt_y) in &seg[1..] {
            let _ = write!(line, " L{pt_x:.2} {pt_y:.2}");
            let _ = write!(area, " L{pt_x:.2} {pt_y:.2}");
         }
         let _ = write!(area, " L{last_x:.2} 240 L{first_x:.2} 240 Z");
      }
   }

   let threshold = panel.threshold.map(|value| {
      let pt_y = y_at(value);
      format!("M16 {pt_y:.2} H744")
   });

   let mid_unix = window_start + span / 2;

   html! {
      figure
         class=(format!("admin-panel admin-panel--{}", panel.tone))
         data-panel-key=(panel.key)
         data-unit=(panel.unit)
         data-threshold=[panel.threshold.map(|val| val.to_string())]
      {
         figcaption class="admin-panel-head admin-panel-header" {
            div {
               h3 { (panel.title) }
               p class="admin-panel-note" { (panel.note) }
            }
            span class="admin-badge admin-badge--neutral admin-panel-hover-val" data-hover-val="" hidden {}
            dl class="admin-panel-stats" {
               div {
                  dt { "last" }
                  dd data-stat="last" { (format_panel_val(latest, panel.unit)) }
               }
               div {
                  dt { "avg" }
                  dd data-stat="avg" { (format_panel_val(mean, panel.unit)) }
               }
               div {
                  dt { "peak" }
                  dd data-stat="peak" { (format_panel_val(peak, panel.unit)) }
               }
            }
         }
         div class="admin-chart" {
            div class="admin-panel-axis" aria-hidden="true" {
               span data-axis="max" { (format_panel_val(maximum, panel.unit)) }
               span data-axis="mid" { (format_panel_val(maximum / 2.0, panel.unit)) }
               span { "0" }
            }
            svg
               class="admin-panel-svg"
               viewBox="0 0 760 260"
               role="img"
               aria-label=(format!("{} over selected window", panel.title))
            {
               path
                  class="stats-grid"
                  vector-effect="non-scaling-stroke"
                  d="M16 16 H744 M16 72 H744 M16 128 H744 M16 184 H744 M16 240 H744"
               {}
               @if let Some(path_d) = threshold {
                  path class="stats-threshold" vector-effect="non-scaling-stroke" d=(path_d) {}
               }
               path class="stats-area" d=(area) {}
               path class="stats-line" vector-effect="non-scaling-stroke" d=(line) {}
               g class="stats-dots" {
                  @for (cx, cy) in &dots {
                     circle class="stats-dot" cx=(cx) cy=(cy) r="4" {}
                  }
               }
               rect class="stats-brush" x="16" y="16" width="0" height="224" hidden {}
               line class="stats-crosshair" x1="16" y1="16" x2="16" y2="240" hidden {}
               circle class="stats-hover-ring" cx="0" cy="0" r="8" hidden {}
               circle class="stats-hover-dot" cx="0" cy="0" r="4.2" hidden {}
               rect class="stats-hit-area" x="16" y="12" width="728" height="236" {}
            }
         }
         div class="admin-panel-xaxis" aria-hidden="true" {
            span data-xaxis="start" { (axis_time_label(window_start, include_date)) }
            span data-xaxis="mid" { (axis_time_label(mid_unix, include_date)) }
            span data-xaxis="end" { (axis_time_label(window_end, include_date)) }
         }
      }
   }
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::pressure::Status as PressureStatus;

   fn sample_gate() -> GateStatus {
      GateStatus {
         armed:             true,
         automatic:         true,
         everyone:          false,
         checking_everyone: false,
         blocked_networks:  1,
         watched_countries: 1,
         surge_per_minute:  300,
         pass_minutes:      60,
         pressure:          PressureStatus {
            flooding:          false,
            cause:             None,
            since:             None,
            until:             None,
            share:             0,
            sample:            120,
            observed_at:       Some(1_700_000_000),
            streak_minutes:    0,
            flood_percent:     50,
            sustained_percent: 3,
            sustained_minutes: 5,
            hold_minutes:      15,
         },
      }
   }

   fn sample_clearance() -> ClearanceStatus {
      ClearanceStatus {
         enabled:         true,
         armed:           true,
         tripped:         false,
         reason:          None,
         tripped_at:      None,
         attempts:        3,
         successes:       3,
         failures:        0,
         active_sessions: 3,
         last_ok:         None,
         last_error:      None,
      }
   }

   fn bucket(minute: i64, incoming: i64, success: i64, finished: i64, latency_ms: i64) -> Bucket {
      Bucket {
         minute,
         incoming,
         success,
         empty: 1,
         rejected: 0,
         rate_limited: 0,
         failed: (finished - success).max(0),
         other: 0,
         challenged: 0,
         latency_ms,
         finished,
      }
   }

   #[test]
   fn summary_weights_latency_and_counts_answered_without_double_counting_empty() {
      let first = bucket(0, 2, 1, 1, 20);
      let next = bucket(60, 4, 2, 3, 100);
      let current = bucket(120, 5, 4, 4, 120);
      let summary = TrafficSummary::from_buckets(&[&first, &next, &current], 120);
      assert_eq!(summary.incoming, 11);
      assert_eq!(summary.successful, 7);
      assert_eq!(summary.empty, 3);
      assert_eq!(summary.answered(), 10);
      assert_eq!(summary.completed, 8);
      assert_eq!(summary.peak, Some(5));
      assert_eq!(summary.latency, Some(100.0));
   }

   #[test]
   fn sample_preserves_already_averaged_latency_without_redividing() {
      let row = Bucket {
         minute:       0,
         incoming:     21,
         success:      9,
         empty:        1,
         rejected:     3,
         rate_limited: 5,
         failed:       2,
         other:        1,
         challenged:   7,
         latency_ms:   245,
         finished:     10,
      };
      let sample = Sample::of(&row);
      assert_eq!(sample.answered, 10.0);
      assert_eq!(sample.empty, 1.0);
      assert!((sample.error_rate - 50.0).abs() < 0.01);
      assert_eq!(sample.latency, 245.0);
   }

   #[test]
   fn hourly_rollup_computes_true_hourly_averages_and_weighted_latency() {
      let first = bucket(0, 10, 4, 4, 100);
      let mut second = bucket(60, 20, 8, 6, 200);
      second.empty = 3;
      let later = bucket(7200, 6, 5, 5, 150);
      let buckets = [&first, &second, &later];

      let hourly = samples(&buckets, true);
      assert_eq!(hourly.len(), 2);
      assert_eq!(hourly[0].minute, 0);
      assert!((hourly[0].incoming - 15.0).abs() < 0.01);
      assert!((hourly[0].answered - 8.0).abs() < 0.01);
      assert!((hourly[0].empty - 2.0).abs() < 0.01);
      // (4 * 100 + 6 * 200) / 10 = 160 ms
      assert!((hourly[0].latency - 160.0).abs() < 0.01);
      assert_eq!(hourly[1].minute, 7200);
   }

   #[test]
   fn dashboard_markup_is_csp_clean_and_renders_live_pools_and_gate() {
      let mut census = PoolCensus {
         total: 5440,
         search: 536,
         view: 4600,
         ct0_dead_pool: 204,
         rejected_pool: 100,
         credentials_rejected: 100,
         ct0_dead: 204,
         demoted_from_search: 12,
         ..PoolCensus::default()
      };
      census.operations.push(OperationPool {
         operation: "UserTweets".to_owned(),
         pool:      "view",
         accounts:  4904,
         free:      4100,
         busy:      80,
         held:      0,
         resting:   0,
         spent:     10,
         out:       714,
      });

      let dashboard = Dashboard {
         minutes: WEEK_MINUTES,
         pool: Some((536, 4904)),
         capacity: Some(1786.6),
         census,
         gate: sample_gate(),
         clearance: sample_clearance(),
         dropped_events: 0,
         buckets: (0..=120)
            .map(|index| bucket(index * 60, 12, 10, 10, 240))
            .collect(),
         restrictions: Vec::new(),
         search_accounts: vec![SearchAccountRow {
            session_id:           1235,
            username:             "amy82si6".to_owned(),
            pool:                 "search".to_owned(),
            verified_search:      true,
            filters_cleared:      true,
            filters_failed:       false,
            suspended:            false,
            credentials_rejected: false,
            empty:                187,
            non_empty:            0,
            rate_limited:         1,
            empty_streak:         187,
            remaining:            Some(0),
            limit:                Some(187),
            last_empty_unix:      Some(7200),
            last_non_empty_unix:  None,
         }],
      };

      let markup = page("light", Some(&dashboard), WEEK_MINUTES).into_string();
      assert!(markup.contains("data-theme=\"light\""));
      assert!(markup.contains("/css/admin-stats.css?v=9"));
      assert!(markup.contains("/js/admin-stats.js?v=9"));
      assert!(markup.contains("href=\"/admin/stats/out?op=UserTweets\""));
      assert!(!markup.contains("style=\""));
      assert!(!markup.contains("<script>"));
      assert!(markup.contains("Anti-DDoS &amp; timeline pressure"));
      assert!(markup.contains("UserTweets"));
      assert!(markup.contains("id=\"stats-search-empty-accounts\""));
      assert!(markup.contains("@amy82si6"));
      assert!(markup.contains("100.0%"));
      assert!(markup.contains("1 verified account ≥50% empty"));
      assert!(markup.contains("id=\"stats-pool\">536 / 4904</dd>"));
      assert!(markup.contains("id=\"stats-ct0-dead-pool\""));
      assert!(markup.contains("href=\"/admin/stats/out?pool=ct0_dead\""));
      assert!(markup.contains("id=\"stats-rejected-pool\""));
      assert!(markup.contains("href=\"/admin/stats/out?pool=rejected\""));
      assert!(markup.contains("data-series=\"[[0,"));
      assert!(markup.contains("data-window-start=\"-597600\""));
      assert!(markup.contains("data-window-end=\"7200\""));
      assert!(markup.contains("data-hourly=\"true\""));
      assert!(markup.contains("data-include-date=\"true\""));
      assert!(markup.contains("id=\"chart-mode-raw\""));
      assert!(markup.contains("id=\"chart-mode-smooth\""));
      assert!(markup.contains("id=\"chart-unpin\""));
      assert!(markup.contains("id=\"chart-reset-zoom\""));
      assert!(markup.contains("id=\"stats-inspector\""));
      assert!(markup.contains("data-panel-key=\"incoming\""));
      assert!(markup.contains("data-panel-key=\"error_rate\""));
      assert!(markup.contains("data-threshold=\"5\""));
      assert!(markup.contains("data-hover-val=\"\""));
      assert!(markup.contains("data-stat=\"last\""));
      assert!(markup.contains("data-stat=\"avg\""));
      assert!(markup.contains("data-stat=\"peak\""));
      assert!(markup.contains("data-axis=\"max\""));
      assert!(markup.contains("data-axis=\"mid\""));
      assert!(markup.contains("data-xaxis=\"start\""));
      assert!(markup.contains("data-xaxis=\"mid\""));
      assert!(markup.contains("data-xaxis=\"end\""));
      assert!(
         markup.contains("<svg class=\"admin-panel-svg\" viewBox=\"0 0 760 260\" role=\"img\"")
      );
      assert!(markup.contains("class=\"stats-dots\""));
      assert!(markup.contains("class=\"stats-brush\""));
      assert!(markup.contains("class=\"stats-crosshair\""));
      assert!(markup.contains("class=\"stats-hover-ring\""));
      assert!(markup.contains("class=\"stats-hover-dot\""));
      assert!(markup.contains("class=\"stats-hit-area\""));
      assert!(markup.contains("<tr data-minute=\"7200\">"));
      assert_eq!(
         markup
            .matches("scope=\"row\" class=\"admin-col-time\"")
            .count(),
         TABLE_LIMIT
      );
   }

   #[test]
   fn single_sample_anchors_to_window_end_and_empty_hides_chart_controls() {
      let single = Dashboard {
         minutes:         15,
         pool:            Some((10, 20)),
         capacity:        Some(33.3),
         census:          PoolCensus::default(),
         gate:            sample_gate(),
         clearance:       sample_clearance(),
         dropped_events:  0,
         buckets:         vec![bucket(3600, 10, 10, 10, 120)],
         restrictions:    Vec::new(),
         search_accounts: Vec::new(),
      };
      let markup = page("dark", Some(&single), 15).into_string();
      assert!(!markup.contains("style=\""));
      assert!(!markup.contains("<script>"));
      assert!(markup.contains("data-window-start=\"2700\""));
      assert!(markup.contains("data-window-end=\"3600\""));
      assert!(markup.contains("data-hourly=\"false\""));
      assert!(markup.contains("data-include-date=\"false\""));
      assert!(markup.contains("class=\"stats-dot\" cx=\"744.00\" cy=\"16.00\" r=\"4\""));

      let empty_markup = page("dark", None, 15).into_string();
      assert!(empty_markup.contains("class=\"admin-chart-controls\" hidden"));
      assert!(empty_markup.contains("id=\"stats-inspector\" class=\"admin-inspector\" hidden"));
      assert!(empty_markup.contains("data-series=\"[]\""));
   }
}
