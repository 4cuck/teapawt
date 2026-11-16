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
mod investigation;
pub(crate) use account::account;
pub(crate) use investigation::restrictions;

#[derive(Serialize)]
pub(crate) struct Dashboard {
   pub minutes:        i32,
   pub pool:           Option<(i64, i64)>,
   pub capacity:       Option<f64>,
   pub dropped_events: u64,
   pub buckets:        Vec<Bucket>,
   pub restrictions:   Vec<RestrictionCount>,
   #[serde(skip)]
   pub recent:         Vec<crate::routes::Observation>,
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
   /// Turnstile and scrape-gate challenges served. Not a fault: the gate
   /// working as designed.
   pub challenged:   i64,
   pub latency_ms:   i64,
   pub finished:     i64,
}

#[derive(Serialize)]
pub(crate) struct RestrictionCount {
   pub minute:      i64,
   pub search_only: i64,
   pub suspended:   i64,
}

const PAGE: &str = "/admin/stats";
const TABLE_LIMIT: usize = 120;
const WEEK_MINUTES: i32 = 7 * 24 * 60;
const RANGES: [(i32, &str); 4] = [
   (15, "15 minutes"),
   (60, "1 hour"),
   (1440, "24 hours"),
   (WEEK_MINUTES, "7 days"),
];

/// How the range looks at a glance, decided by error rate rather than by any
/// single count. Thresholds follow the RED method: the question is what share
/// of visitors got a usable answer, not how many requests there were.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
   /// Nothing recorded in the range.
   Unknown,
   /// Under 1% of responses were bad.
   Healthy,
   /// 1% to 5% bad.
   Degraded,
   /// Over 5% bad.
   Failing,
}

impl Health {
   const fn label(self) -> &'static str {
      match self {
         Self::Unknown => "No data",
         Self::Healthy => "Healthy",
         Self::Degraded => "Degraded",
         Self::Failing => "Failing",
      }
   }

   const fn modifier(self) -> &'static str {
      match self {
         Self::Unknown => "unknown",
         Self::Healthy => "healthy",
         Self::Degraded => "degraded",
         Self::Failing => "failing",
      }
   }

   /// The sentence under the status banner, phrased as the answer to "is
   /// anything wrong right now".
   const fn detail(self) -> &'static str {
      match self {
         Self::Unknown => "No traffic was recorded in this range.",
         Self::Healthy => "Under 1% of responses were unusable.",
         Self::Degraded => "Between 1% and 5% of responses were unusable.",
         Self::Failing => "More than 5% of responses were unusable.",
      }
   }
}

/// Totals for the selected range, split by what visitors actually received.
///
/// Every outcome is kept apart rather than lumped into "success" and "failed":
/// a rate limit and an upstream outage need different fixes, and the panel that
/// shows them should not make the reader subtract to find out which one moved.
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
   /// Responses that answered the query, including an empty result.
   const fn answered(&self) -> i128 {
      self.successful + self.empty
   }

   /// Responses that were our fault: we refused, we rate-limited, or the
   /// upstream failed and we could not answer from elsewhere.
   ///
   /// `other` is deliberately excluded. It is where a 404 for a page that never
   /// existed lands, and a visitor mistyping a URL is not a service fault.
   const fn bad(&self) -> i128 {
      self.rejected + self.rate_limited + self.failed
   }

   /// Share of responses that were not usable, as a percentage.
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
      let mut closed_successes = 0_i128;
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
            closed_successes += i128::from(bucket.success);
            closed_minutes += 1;
         }
      }
      summary.observed =
         (closed_minutes > 0).then(|| closed_successes as f64 / f64::from(closed_minutes));
      summary.latency =
         (summary.completed > 0).then(|| weighted_latency / summary.completed as f64);
      summary
   }
}

/// One recorded minute (or one hour, in the weekly view) reduced to the numbers
/// the panels need.
///
/// Every outcome is its own field rather than a total plus a remainder, so each
/// panel can plot exactly one of them against its own axis. That is the RED
/// method's whole point: a rate limit and an upstream outage are different
/// faults, and reading them off one shared scale is how the small one gets
/// hidden by the large one.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Sample {
   minute:       i64,
   incoming:     f64,
   answered:     f64,
   rejected:     f64,
   rate_limited: f64,
   failed:       f64,
   other:        f64,
   /// Challenges we served. Tracked, but neither good nor bad.
   challenged:   f64,
   latency:      f64,
   /// Share of responses that were not usable, in percent.
   error_rate:   f64,
}

impl Sample {
   fn of(bucket: &Bucket) -> Self {
      let answered = (bucket.success + bucket.empty) as f64;
      // `other` is a 404 for a page that never existed, not a service fault.
      let bad = (bucket.rejected + bucket.rate_limited + bucket.failed) as f64;
      // A challenge is not a response a visitor was denied: it is the gate
      // doing its job. Excluded from the denominator so a site where every
      // first-time visitor sees one does not read as 90% failed.
      let total = answered + bad;
      Self {
         minute: bucket.minute,
         incoming: bucket.incoming as f64,
         answered,
         rejected: bucket.rejected as f64,
         rate_limited: bucket.rate_limited as f64,
         failed: bucket.failed as f64,
         other: bucket.other as f64,
         challenged: bucket.challenged as f64,
         latency: if bucket.finished > 0 {
            bucket.latency_ms as f64 / bucket.finished as f64
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

   /// Fold two samples of the same hour. Every field keeps its peak, so a single
   /// bad minute survives the weekly roll-up instead of being averaged into the
   /// good minutes beside it.
   fn merge(self, other: Self) -> Self {
      Self {
         minute: self.minute,
         incoming: self.incoming.max(other.incoming),
         answered: self.answered.max(other.answered),
         rejected: self.rejected.max(other.rejected),
         rate_limited: self.rate_limited.max(other.rate_limited),
         failed: self.failed.max(other.failed),
         other: self.other.max(other.other),
         challenged: self.challenged.max(other.challenged),
         latency: self.latency.max(other.latency),
         error_rate: self.error_rate.max(other.error_rate),
      }
   }
}

/// Reduce buckets to one sample per plotted point.
fn samples(buckets: &[&Bucket], hourly: bool) -> Vec<Sample> {
   let mut out: Vec<Sample> = Vec::new();
   for bucket in buckets {
      let mut sample = Sample::of(bucket);
      if hourly {
         sample.minute = sample.minute.div_euclid(3600) * 3600;
      }
      match out.last_mut() {
         Some(last) if hourly && last.minute == sample.minute => *last = last.merge(sample),
         _ => out.push(sample),
      }
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

pub(crate) fn frame(title: &str, active: &str, content: Markup) -> Markup {
   html! {
      (DOCTYPE)
      html lang="en" {
         head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            meta name="robots" content="noindex, nofollow";
            title { (title) " · Teapot administration" }
            link rel="stylesheet" href="/css/admin-stats.css?v=5";
            script src="/js/admin-stats.js?v=5" defer="" {}
         }
         body {
            div class="admin-workspace" {
               nav class="admin-sidebar" aria-label="Investigation workspace" {
                  a class="admin-brand" href="/admin/stats" { "TEAPOT" span { "operations" } }
                  a href="/admin/stats" aria-current[active == "overview"] { "Traffic overview" }
                  a href="/admin/stats/restrictions" aria-current[active == "restrictions"] { "Restrictions" }
                  a href="/admin/import" { "Account import" }
                  a class="admin-home" href="/" { "← Public site" }
               }
               main class="admin-stats" data-page=(active) { (content) }
            }
         }
      }
   }
}

pub(crate) fn kind_label(kind: crate::stats::Restriction) -> &'static str {
   kind.label()
}

pub(crate) fn page(_theme: &str, data: Option<&Dashboard>, minutes: i32) -> Markup {
   frame("Traffic overview", "overview", html! {
      header class="admin-page-header" {
         p class="admin-eyebrow" { "Operations / overview" }
         h1 { "Traffic overview" }
         p { "Origin request health and recent account signals. Select an account to inspect its evidence and session state." }
      }
      (overview_content(data, minutes))
   })
}

fn overview_content(data: Option<&Dashboard>, minutes: i32) -> Markup {
   let buckets = data.map_or_else(Vec::new, |data| {
      let mut buckets: Vec<_> = data.buckets.iter().collect();
      buckets.sort_unstable_by_key(|bucket| bucket.minute);
      buckets
   });
   let now = OffsetDateTime::now_utc();
   let current_minute = now.unix_timestamp().div_euclid(60) * 60;
   let summary = TrafficSummary::from_buckets(&buckets, current_minute);
   let health = summary.health();
   let hourly = minutes == WEEK_MINUTES;
   let series = samples(&buckets, hourly);
   let estimate = data
      .and_then(|data| data.capacity)
      .filter(|value| value.is_finite());
   let pool = data.and_then(|data| data.pool);
   let mut restrictions: Vec<_> = data
      .into_iter()
      .flat_map(|data| &data.restrictions)
      .filter(|row| row.search_only > 0 || row.suspended > 0)
      .collect();
   restrictions.sort_unstable_by_key(|row| std::cmp::Reverse(row.minute));

   let error_rate = summary
      .error_rate()
      .map_or_else(|| "—".to_string(), |rate| format!("{rate:.2}%"));

   html! {
      div class="admin-stats-content" data-minutes=(minutes) data-refresh-interval="30" {
         div class="admin-stats-toolbar" {
            form action=(PAGE) method="get" {
               label for="stats-minutes" { "Range" }
               select name="minutes" id="stats-minutes" {
                  @if !RANGES.iter().any(|(value, _)| *value == minutes) {
                     option value=(minutes) selected { (minutes) " minutes" }
                  }
                  @for (value, label) in RANGES {
                     option value=(value) selected[value == minutes] { (label) }
                  }
               }
               button type="submit" { "Apply" }
            }
            div class="admin-stats-toolbar-meta" {
               p class="admin-stats-updated" {
                  span id="stats-updated" {
                     @if data.is_some() { (timestamp(now)) }
                     @else { "Not yet available" }
                  }
               }
               span class="admin-stats-refresh-badge" {
                  span class="admin-stats-live-dot" {}
                  span id="stats-countdown" { "30s" }
               }
            }
         }

         // The one-line answer to "is anything wrong". Everything below it is
         // evidence for this verdict.
         div class=(format!("admin-verdict admin-verdict--{}", health.modifier())) {
            div class="admin-verdict-main" {
               span class="admin-verdict-dot" {}
               strong { (health.label()) }
               span class="admin-verdict-rate" { (error_rate) " unusable" }
            }
            p class="admin-verdict-detail" { (health.detail()) }
            dl class="admin-verdict-facts" {
               div { dt { "Responses" } dd { (summary.answered() + summary.bad()) } }
               div { dt { "Answered" } dd { (summary.answered()) } }
               div { dt { "Bad" } dd { (summary.bad()) } }
               div { dt { "Challenged" } dd { (summary.challenged) } }
               div { dt { "Mean latency" } dd { (summary.latency.map_or_else(|| "—".to_string(), |value| format!("{value:.0} ms"))) } }
            }
         }

         p id="stats-status" role="status" hidden[data.is_some()] {
            @if data.is_none() { "Statistics are temporarily unavailable. Reload this page to try again." }
         }

         section class="admin-section" aria-labelledby="stats-red-title" {
            div class="admin-section-heading" {
               div {
                  h2 id="stats-red-title" { "Request health" }
                  p {
                     "Rate, errors, and duration, each on its own axis. "
                     @if hourly {
                        "For 7 days, each point is the worst recorded minute of its UTC hour."
                     } @else {
                        "Each point is one recorded minute (UTC)."
                     }
                     " Minutes with no samples are not plotted."
                  }
               }
            }
            p id="stats-traffic-empty" class="admin-empty" hidden[!series.is_empty()] {
               @if data.is_some() { "No traffic samples were recorded in this range." }
               @else { "Traffic samples are unavailable." }
            }
            div id="stats-red" class="admin-panel-grid" hidden[series.is_empty()] {
               @for spec in &PANELS {
                  (panel(&series, spec))
               }
            }
         }

         section class="admin-section" aria-labelledby="stats-breakdown-title" {
            div class="admin-section-heading" {
               div {
                  h2 id="stats-breakdown-title" { "What the bad responses were" }
                  p { "Each outcome on its own axis, so a handful of 429s is as readable as an outage." }
               }
            }
            div id="stats-breakdown" class="admin-panel-grid admin-panel-grid--compact" hidden[series.is_empty()] {
               @for spec in &BREAKDOWN {
                  (panel(&series, spec))
               }
            }
         }

         section class="admin-section" aria-labelledby="stats-pool-title" {
            div class="admin-section-heading" {
               div {
                  h2 id="stats-pool-title" { "Account pools" }
                  p { "Current runtime counts, not historical. Pool size is what caps throughput." }
               }
            }
            dl class="admin-stats-metrics" {
               (metric("stats-pool", "Search / view", pool.map(|(search, view)| format!("{search} / {view}")), "Accounts reserved for search, then for profile reads", ""))
               (metric("stats-estimate", "Upstream search budget", estimate.map(|value| format!("{value:.1} calls/min")), "Theoretical ceiling from live account windows", ""))
               (metric("stats-peak", "Peak rate", summary.peak.map(|count| format!("{count} req/min")), "Busiest recorded minute in this range", ""))
               (metric("stats-dropped", "Dropped samples", data.map(|data| data.dropped_events.to_string()), "Since this process started; a non-zero value means the totals above undercount", if data.is_some_and(|data| data.dropped_events > 0) { "warn" } else { "" }))
            }
         }

         section class="admin-section" aria-labelledby="stats-table-title" {
            div class="admin-section-heading" {
               div {
                  h2 id="stats-table-title" { "Recorded minutes" }
                  p { "The raw rows the panels are drawn from." }
               }
            }
            details id="stats-traffic-table" hidden[buckets.is_empty()] {
               summary { "Show table" }
               div class="admin-stats-table-scroll" role="region" aria-label="Traffic data table" tabindex="0" {
                  table {
                     caption {
                        "Outcomes by minute (UTC). "
                        span id="stats-table-limit" hidden[buckets.len() <= TABLE_LIMIT] {
                           "Showing the latest " (TABLE_LIMIT) " recorded minutes."
                        }
                     }
                     thead {
                        tr {
                           @for label in ["Minute", "Rate", "Answered", "Denied", "429", "Failed", "Other", "Challenged", "Error rate", "Latency"] {
                              th scope="col" { (label) }
                           }
                        }
                     }
                     tbody id="stats-rows" {
                        @for bucket in buckets.iter().rev().take(TABLE_LIMIT) {
                           @let sample = Sample::of(bucket);
                           tr {
                              th scope="row" { (minute_label(bucket.minute)) }
                              td { (bucket.incoming) }
                              td { (bucket.success + bucket.empty) }
                              td { (bucket.rejected) }
                              td { (bucket.rate_limited) }
                              td { (bucket.failed) }
                              td { (bucket.other) }
                              td { (bucket.challenged) }
                              td { (format!("{:.1}%", sample.error_rate)) }
                              td { (if bucket.finished > 0 { format!("{:.0} ms", sample.latency) } else { "—".to_string() }) }
                           }
                        }
                     }
                  }
               }
            }
         }

         section class="admin-section" aria-labelledby="stats-restrictions-title" {
            div class="admin-section-heading" {
               div {
                  h2 id="stats-restrictions-title" { "Recent account restrictions" }
                  p { "Recorded observations over the last 30 days." }
               }
               a class="admin-text-link" href="/admin/stats/restrictions" { "Investigate all →" }
            }
            p id="stats-restrictions-empty" class="admin-empty" hidden[restrictions.is_empty()] { "No restriction observations were recorded in this range." }
            div id="stats-restrictions-list" hidden[restrictions.is_empty()] {
               @for row in restrictions.into_iter().take(10) {
                  div class="stats-restriction-row" {
                     (minute_label(row.minute))
                     @if row.search_only > 0 { span class="stats-restriction-badge stats-restriction-badge--search" { (row.search_only) " search-only" } }
                     @if row.suspended > 0 { span class="stats-restriction-badge stats-restriction-badge--suspended" { (row.suspended) " suspended" } }
                  }
               }
            }
         }

         aside class="admin-section admin-reading" {
            h2 { "Reading this page" }
            ul {
               li { "Only origin traffic is counted. Missing minutes are not zero traffic." }
               li { "An empty search result counts as answered: it was a correct response to the query." }
               li { "Restriction observations are kept for 30 days; live account health is read from the pool, not from here." }
               li { "Panels never share a scale. Compare shapes across panels, not heights." }
            }
         }
      }
   }
}

fn metric(id: &str, title: &str, value: Option<String>, note: &str, modifier: &str) -> Markup {
   let class = if modifier.is_empty() {
      "admin-stats-metric".to_string()
   } else {
      format!("admin-stats-metric admin-stats-metric--{modifier}")
   };
   html! {
      div class=(class) {
         dt { (title) }
         dd id=(id) { (value.as_deref().unwrap_or("Unavailable")) }
         dd class="admin-stats-note" { (note) }
      }
   }
}

/// What one panel plots: a title, the unit, and how to read a sample.
struct Panel {
   id:       &'static str,
   title:    &'static str,
   unit:     &'static str,
   /// One line under the title, so a reader never has to guess what the line is.
   note:     &'static str,
   /// CSS modifier for the stroke colour.
   tone:     &'static str,
   value:    fn(&Sample) -> f64,
   /// Draw a threshold line at this value, when the panel has a meaningful one.
   threshold: Option<f64>,
}

/// The panels, in reading order: how much traffic, how much of it failed, then
/// what the failures were, then how slow it was.
///
/// This is the RED method. Rate and errors are separate panels on separate
/// axes, which is the only way a handful of 429s stays visible next to
/// thousands of successes — Grafana's guidance is explicit that stacking hides
/// exactly that.
const PANELS: [Panel; 4] = [
   Panel {
      id:        "rate",
      title:     "Request rate",
      unit:      "req/min",
      note:      "Every request that reached the origin, per recorded minute.",
      tone:      "rate",
      value:     |sample| sample.incoming,
      threshold: None,
   },
   Panel {
      id:        "errors",
      title:     "Error rate",
      unit:      "%",
      note:      "Share of responses that were not a usable answer. Challenges are excluded; the line is the 5% threshold.",
      tone:      "errors",
      value:     |sample| sample.error_rate,
      threshold: Some(5.0),
   },
   Panel {
      id:        "latency",
      title:     "Mean response time",
      unit:      "ms",
      note:      "Average time to produce a response, weighted by completed requests.",
      tone:      "latency",
      value:     |sample| sample.latency,
      threshold: None,
   },
   Panel {
      id:        "challenges",
      title:     "Challenges served",
      unit:      "resp/min",
      note:      "Turnstile pages handed to visitors. Not an error: the gate is doing its job.",
      tone:      "challenged",
      value:     |sample| sample.challenged,
      threshold: None,
   },
];

/// The outcome panels under the RED row. Same rendering, split out so the two
/// groups can be read as different questions: "how is it going" and "what went
/// wrong".
const BREAKDOWN: [Panel; 4] = [
   Panel {
      id:        "denied",
      title:     "Access denied",
      unit:      "resp/min",
      note:      "401, 402, and 403: authentication, pass, and access failures.",
      tone:      "rejected",
      value:     |sample| sample.rejected,
      threshold: None,
   },
   Panel {
      id:        "limited",
      title:     "Rate limited",
      unit:      "resp/min",
      note:      "429s we sent because no account had budget left.",
      tone:      "rate-limited",
      value:     |sample| sample.rate_limited,
      threshold: None,
   },
   Panel {
      id:        "upstream",
      title:     "Upstream failed",
      unit:      "resp/min",
      note:      "X failed and we could not answer from another account.",
      tone:      "failed",
      value:     |sample| sample.failed,
      threshold: None,
   },
   Panel {
      id:        "other",
      title:     "Other",
      unit:      "resp/min",
      note:      "Timeouts and responses outside the categories above.",
      tone:      "other",
      value:     |sample| sample.other,
      threshold: None,
   },
];

/// A single-series time chart.
///
/// One line, one axis, scaled to its own maximum. Nothing is stacked: the point
/// of a separate panel per outcome is that three rate limits fill the panel the
/// same way three thousand successes fill theirs.
fn panel(samples: &[Sample], panel: &Panel) -> Markup {
   let values: Vec<(i64, f64)> = samples
      .iter()
      .map(|sample| (sample.minute, (panel.value)(sample)))
      .collect();
   let peak = values.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);
   // A panel whose only value is zero should still show a flat line at the
   // bottom rather than a division by zero.
   let maximum = peak.max(panel.threshold.unwrap_or(0.0)).max(1.0);
   let first = values.first().map_or(0, |(minute, _)| *minute);
   let last = values.last().map_or(0, |(minute, _)| *minute);
   let span = last.saturating_sub(first);

   let x_at = |minute: i64| -> f64 {
      if span == 0 {
         320.0
      } else {
         18.0 + 604.0 * (minute.saturating_sub(first) as f64 / span as f64)
      }
   };
   let y_at = |value: f64| -> f64 { 152.0 - 136.0 * (value.max(0.0) / maximum) };

   let line = {
      let mut path = String::new();
      use std::fmt::Write as _;
      for (index, (minute, value)) in values.iter().enumerate() {
         let x = x_at(*minute);
         let y = y_at(*value);
         let _ = if index == 0 {
            write!(path, "M{x:.2} {y:.2}")
         } else {
            write!(path, " L{x:.2} {y:.2}")
         };
      }
      path
   };

   let area = if line.is_empty() {
      String::new()
   } else {
      let first_x = x_at(first);
      let last_x = x_at(last);
      format!("{line} L{last_x:.2} 152 L{first_x:.2} 152 Z")
   };

   let threshold = panel.threshold.map(|value| {
      let y = y_at(value);
      format!("M14 {y:.2} H626")
   });

   let peak_label = if panel.unit == "%" {
      format!("{peak:.1}%")
   } else if panel.unit == "ms" {
      format!("{peak:.0} ms")
   } else {
      format!("{peak:.0}")
   };

   html! {
      figure class=(format!("admin-panel admin-panel--{}", panel.tone)) {
         figcaption class="admin-panel-head" {
            div {
               h3 { (panel.title) }
               p class="admin-panel-note" { (panel.note) }
            }
            dl class="admin-panel-stat" {
               dt { "peak" }
               dd { (peak_label) }
            }
         }
         div class="admin-panel-plot" {
            svg viewBox="0 0 640 168" preserveAspectRatio="none" role="img"
               aria-label=(format!("{} per recorded minute", panel.title)) {
               path class="stats-grid" d="M14 16 H626 M14 50 H626 M14 84 H626 M14 118 H626 M14 152 H626" {}
               @if let Some(d) = threshold { path class="stats-threshold" d=(d) {} }
               path class="stats-area" d=(area) {}
               path class="stats-line" d=(line) {}
            }
            div class="admin-panel-axis" aria-hidden="true" {
               span { (format!("{maximum:.0}")) }
               span { (panel.unit) }
               span { "0" }
            }
         }
      }
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   fn bucket(minute: i64, incoming: i64, success: i64, finished: i64, latency_ms: i64) -> Bucket {
      Bucket {
         minute,
         incoming,
         success,
         empty: 1,
         rejected: 0,
         rate_limited: 0,
         failed: finished - success,
         other: 0,
         challenged: 0,
         latency_ms,
         finished,
      }
   }

   #[test]
   fn summary_weights_latency_and_excludes_current_minute_only_from_rate() {
      let first = bucket(0, 2, 1, 1, 20);
      let next = bucket(60, 4, 2, 3, 100);
      let current = bucket(120, 5, 4, 4, 120);
      let summary = TrafficSummary::from_buckets(&[&first, &next, &current], 120);
      assert_eq!(summary.incoming, 11);
      assert_eq!(summary.successful, 7);
      assert_eq!(summary.completed, 8);
      assert_eq!(summary.peak, Some(5));
      assert_eq!(summary.observed, Some(1.5));
      assert_eq!(summary.latency, Some(100.0));
      assert_eq!(
         TrafficSummary::from_buckets(&[&current], 120).observed,
         None
      );
      assert_eq!(TrafficSummary::from_buckets(&[], 120).latency, None);
   }

   #[test]
   fn hourly_rollup_keeps_each_outcome_peak_across_the_range() {
      let first = bucket(0, 9, 1, 26, 0);
      let second = bucket(60, 4, 7, 7, 0);
      let later = bucket(7200, 6, 5, 5, 0);
      let buckets = [&first, &second, &later];

      let hourly = samples(&buckets, true);
      // The busiest minute of each hour wins per field, so a single bad minute
      // survives the roll-up instead of being averaged into its neighbours.
      assert_eq!(hourly.len(), 2);
      assert_eq!(hourly[0].minute, 0);
      assert_eq!(hourly[0].answered, 8.0); // max(1 + 1, 7 + 1)
      assert_eq!(hourly[0].failed, 25.0);
      assert_eq!(hourly[1].minute, 7200);

      let minute = samples(&buckets, false);
      assert_eq!(minute.len(), 3);
      assert_eq!(minute[0].answered, 2.0);
      assert_eq!(minute[1].answered, 8.0);
      assert_eq!(minute[2].answered, 6.0);
   }

   #[test]
   fn a_sample_separates_answers_from_each_kind_of_bad_response() {
      let bucket = Bucket {
         minute:       0,
         incoming:     21,
         success:      9,
         empty:        1,
         rejected:     3,
         rate_limited: 5,
         failed:       2,
         other:        1,
         challenged:   7,
         latency_ms:   210,
         finished:     10,
      };
      let sample = Sample::of(&bucket);
      assert_eq!(sample.answered, 10.0);
      assert_eq!(sample.rejected, 3.0);
      assert_eq!(sample.rate_limited, 5.0);
      assert_eq!(sample.failed, 2.0);
      assert_eq!(sample.other, 1.0);
      // 10 bad of 20 that were not challenges: 3 + 5 + 2.
      // `other` is excluded because a 404 is not a service fault.
      assert!((sample.error_rate - 50.0).abs() < 0.01);
      // Per-request latency, not the bucket total.
      assert_eq!(sample.latency, 21.0);
   }

   #[test]
   fn a_challenge_is_not_counted_against_the_error_rate() {
      // A healthy site where every first-time visitor gets a Turnstile page.
      // Before this was its own outcome the challenge was scored as a 403, and
      // the panel read 91% while nothing was wrong.
      let bucket = Bucket {
         minute:       0,
         incoming:     110,
         success:      9,
         empty:        0,
         rejected:     0,
         rate_limited: 0,
         failed:       1,
         other:        0,
         challenged:   100,
         latency_ms:   1000,
         finished:     10,
      };
      let sample = Sample::of(&bucket);
      // One bad response out of ten that were not challenges.
      assert!((sample.error_rate - 10.0).abs() < 0.01);
      assert_eq!(sample.challenged, 100.0);

      let summary = TrafficSummary::from_buckets(&[&bucket], 60);
      // The challenge is visible but not scored.
      assert_eq!(summary.challenged, 100);
      assert_eq!(summary.bad(), 1);
      assert_eq!(summary.health(), Health::Failing);
   }

   #[test]
   fn health_is_decided_by_the_share_of_bad_responses_not_the_count() {
      let quiet = TrafficSummary {
         successful: 1000,
         rejected:   4,
         ..TrafficSummary::default()
      };
      let loud = TrafficSummary {
         successful:   100,
         rate_limited: 9,
         ..TrafficSummary::default()
      };
      // Four failures out of a thousand is healthy; nine out of a hundred is
      // not, even though the first number is larger.
      assert_eq!(quiet.health(), Health::Healthy);
      assert_eq!(loud.health(), Health::Failing);
      assert_eq!(TrafficSummary::default().health(), Health::Unknown);

      let degraded = TrafficSummary {
         successful: 98,
         failed:     2,
         ..TrafficSummary::default()
      };
      assert_eq!(degraded.health(), Health::Degraded);
   }

   #[test]
   fn weekly_view_discloses_chart_and_table_limits_without_truncating_totals() {
      let dashboard = Dashboard {
         minutes:        WEEK_MINUTES,
         pool:           Some((1, 2)),
         capacity:       Some(20.0),
         dropped_events: 3,
         buckets:        (0..=120)
            .map(|index| bucket(index * 60, 2, 1, 2, 42))
            .collect(),
         restrictions:   Vec::new(),
         recent:         vec![],
      };
      let markup = page("dark", Some(&dashboard), WEEK_MINUTES).into_string();
      // The table is still capped, and says so.
      assert_eq!(markup.matches("scope=\"row\"").count(), TABLE_LIMIT);
      assert!(markup.contains("Showing the latest 120 recorded minutes."));
      // The weekly view explains that each point is the worst minute of an hour.
      assert!(markup.contains("worst recorded minute of its UTC hour"));
      // Panels are separate, never stacked: each has its own axis label.
      assert!(markup.contains("Request rate"));
      assert!(markup.contains("Error rate"));
      assert!(!markup.contains("stats-band"));
      // The summary line reads the health, not a raw count.
      assert!(markup.contains("admin-verdict--"));
      assert!(markup.contains("id=\"stats-pool\">1 / 2</dd>"));
      assert!(markup.contains("id=\"stats-estimate\">20.0 calls/min</dd>"));
      assert!(markup.contains("id=\"stats-dropped\">3</dd>"));
   }
}
