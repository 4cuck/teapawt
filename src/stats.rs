//! Best-effort, aggregate telemetry for the operational dashboard.

use std::{
   collections::{
      BTreeMap,
      HashMap,
   },
   env,
   sync::{
      Arc,
      atomic::{
         AtomicU64,
         Ordering,
      },
   },
   time::{
      Duration,
      Instant,
      SystemTime,
      UNIX_EPOCH,
   },
};

use serde::Serialize;
use tokio::{
   sync::mpsc,
   time::{
      MissedTickBehavior,
      interval,
   },
};
use tokio_postgres::{
   Client,
   Error,
   NoTls,
};

const DEFAULT_URL: &str = "host=/var/run/postgresql user=teapot dbname=teapot";
const RETENTION_MINUTES: i32 = 30 * 24 * 60;
const QUEUE_CAPACITY: usize = 4096;
const BATCH_CAPACITY: usize = 512;
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(3600);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS stats_minute_buckets (
   minute_unix bigint PRIMARY KEY,
   received bigint NOT NULL,
   success bigint NOT NULL,
   empty bigint NOT NULL,
   rejected bigint NOT NULL,
   rate_limited bigint NOT NULL,
   failed bigint NOT NULL,
   other bigint NOT NULL,
   challenged bigint NOT NULL DEFAULT 0,
   latency_sum_ms bigint NOT NULL,
   latency_max_ms bigint NOT NULL
);
ALTER TABLE stats_minute_buckets ADD COLUMN IF NOT EXISTS challenged bigint NOT NULL DEFAULT 0;
CREATE TABLE IF NOT EXISTS stats_restrictions (
   session_id bigint NOT NULL,
   kind smallint NOT NULL,
   last_seen_unix bigint NOT NULL,
   PRIMARY KEY (session_id, kind)
);
CREATE INDEX IF NOT EXISTS stats_restrictions_last_seen_idx ON stats_restrictions (last_seen_unix);
CREATE TABLE IF NOT EXISTS stats_account_events (
   id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
   session_id bigint NOT NULL,
   kind smallint NOT NULL,
   observed_unix bigint NOT NULL,
   endpoint text NOT NULL,
   http_status integer,
   upstream_code integer,
   evidence text NOT NULL
);
CREATE INDEX IF NOT EXISTS stats_account_events_session_idx
   ON stats_account_events (session_id, observed_unix DESC, id DESC);
CREATE INDEX IF NOT EXISTS stats_account_events_recent_idx
   ON stats_account_events (observed_unix DESC, id DESC);
CREATE TABLE IF NOT EXISTS stats_search_account_minutes (
   minute_unix bigint NOT NULL,
   session_id bigint NOT NULL,
   non_empty bigint NOT NULL,
   empty bigint NOT NULL,
   rate_limited bigint NOT NULL,
   last_empty_unix bigint,
   last_non_empty_unix bigint,
   PRIMARY KEY (minute_unix, session_id)
);
CREATE INDEX IF NOT EXISTS stats_search_account_minutes_session_idx
   ON stats_search_account_minutes (session_id, minute_unix DESC);
";

// Both aggregates are applied atomically in a single round trip. The in-memory
// batch guarantees at most one input row for each conflict key.
const WRITE_BATCH: &str = "
WITH minutes AS (
   SELECT * FROM jsonb_to_recordset($1) AS m (
      minute_unix bigint, received bigint, success bigint, empty bigint, rejected bigint,
      rate_limited bigint, failed bigint, other bigint, challenged bigint, latency_sum_ms bigint,
      latency_max_ms bigint
   )
), written_minutes AS (
   INSERT INTO stats_minute_buckets
      (minute_unix, received, success, empty, rejected, rate_limited, failed, other, challenged,
       latency_sum_ms, latency_max_ms)
   SELECT * FROM minutes ORDER BY minute_unix
   ON CONFLICT (minute_unix) DO UPDATE SET
      received = stats_minute_buckets.received + EXCLUDED.received,
      success = stats_minute_buckets.success + EXCLUDED.success,
      empty = stats_minute_buckets.empty + EXCLUDED.empty,
      rejected = stats_minute_buckets.rejected + EXCLUDED.rejected,
      rate_limited = stats_minute_buckets.rate_limited + EXCLUDED.rate_limited,
      failed = stats_minute_buckets.failed + EXCLUDED.failed,
      other = stats_minute_buckets.other + EXCLUDED.other,
      challenged = stats_minute_buckets.challenged + EXCLUDED.challenged,
      latency_sum_ms = stats_minute_buckets.latency_sum_ms + EXCLUDED.latency_sum_ms,
      latency_max_ms = GREATEST(stats_minute_buckets.latency_max_ms, EXCLUDED.latency_max_ms)
)
INSERT INTO stats_restrictions (session_id, kind, last_seen_unix)
SELECT * FROM unnest($2::bigint[], $3::smallint[], $4::bigint[]) AS r (session_id, kind, \
                           last_seen_unix)
ORDER BY session_id, kind
ON CONFLICT (session_id, kind) DO UPDATE SET
   last_seen_unix = GREATEST(stats_restrictions.last_seen_unix, EXCLUDED.last_seen_unix)
";

#[derive(Clone, Copy)]
pub enum Outcome {
   Success,
   Empty,
   Rejected,
   RateLimited,
   Failed,
   Other,
   /// A gate we put in front of the request, not a fault.
   ///
   /// The Turnstile page answers 403 and the scrape gate answers 429, and both
   /// were being counted as failures. That made the error rate read 91% while
   /// the site was healthy: the guard working as designed is the opposite of an
   /// error. Kept as its own outcome so the volume of challenges is still
   /// visible without being scored against the visitors who never saw one.
   Challenged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[repr(i16)]
#[serde(rename_all = "snake_case")]
pub enum Restriction {
   SearchOnly          = 1,
   Suspended           = 2,
   CredentialsRejected = 3,
   CsrfRejected        = 4,
   SearchBanned        = 5,
   ProfileBanned       = 6,
   OperationRefused    = 7,
   RateLimited         = 8,
   SettingsRejected    = 9,
   SearchRested        = 10,
   /// A manual probe found the account alive, so a stored verdict was cleared.
   Reinstated          = 11,
   /// A supposedly valid (verified) search account repeatedly answered with no
   /// results.
   SearchEmptyStreak   = 12,
}

impl Restriction {
   pub fn from_i16(kind: i16) -> Option<Self> {
      Some(match kind {
         1 => Self::SearchOnly,
         2 => Self::Suspended,
         3 => Self::CredentialsRejected,
         4 => Self::CsrfRejected,
         5 => Self::SearchBanned,
         6 => Self::ProfileBanned,
         7 => Self::OperationRefused,
         8 => Self::RateLimited,
         9 => Self::SettingsRejected,
         10 => Self::SearchRested,
         11 => Self::Reinstated,
         12 => Self::SearchEmptyStreak,
         _ => return None,
      })
   }

   pub const fn label(self) -> &'static str {
      match self {
         Self::SearchOnly | Self::SearchBanned => "Search refused",
         Self::Suspended => "Suspended",
         Self::CredentialsRejected => "Credentials rejected",
         Self::CsrfRejected => "Stale CSRF token",
         Self::ProfileBanned => "Profile access refused",
         Self::OperationRefused => "Operation refused",
         Self::RateLimited => "Rate limited",
         Self::SettingsRejected => "Settings write refused",
         Self::SearchRested => "Search temporarily rested",
         Self::Reinstated => "Reinstated by probe",
         Self::SearchEmptyStreak => "Consistent empty search",
      }
   }
}

/// Outcome of a single upstream `SearchTimeline` request for one account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchAccountOutcome {
   NonEmpty,
   Empty,
   RateLimited,
}

/// Aggregated upstream `SearchTimeline` outcomes for one account over a time
/// window.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SearchAccountSummary {
   pub session_id:          i64,
   pub non_empty:           i64,
   pub empty:               i64,
   pub rate_limited:        i64,
   pub last_empty_unix:     Option<i64>,
   pub last_non_empty_unix: Option<i64>,
}

/// Operational evidence only: credentials and request payloads never belong
/// here.
#[derive(Clone, Debug, Serialize)]
pub struct AccountEvent {
   pub id:            i64,
   pub session_id:    i64,
   pub kind:          Restriction,
   pub observed_unix: i64,
   pub endpoint:      String,
   pub http_status:   Option<i32>,
   pub upstream_code: Option<i32>,
   pub evidence:      String,
}

impl AccountEvent {
   pub fn new(session_id: i64, kind: Restriction, endpoint: &str, evidence: &str) -> Self {
      Self {
         id: 0,
         session_id,
         kind,
         observed_unix: now_unix(),
         endpoint: endpoint.chars().take(160).collect(),
         http_status: None,
         upstream_code: None,
         evidence: evidence.chars().take(240).collect(),
      }
   }

   pub fn upstream(mut self, http_status: u16, upstream_code: Option<i64>) -> Self {
      self.http_status = Some(i32::from(http_status));
      self.upstream_code = upstream_code.and_then(|code| i32::try_from(code).ok());
      self
   }
}

#[derive(Clone)]
pub struct StatsStore {
   client:         Arc<Client>,
   tx:             mpsc::Sender<Event>,
   dropped_events: Arc<AtomicU64>,
}

/// Begins at `start_unix` (inclusive) and ends at `end_unix` (exclusive).
/// Includes the partial current minute. Absent buckets are not treated as zero.
#[derive(Serialize)]
pub struct Snapshot {
   pub minutes:        i32,
   pub start_unix:     i64,
   pub end_unix:       i64,
   pub buckets:        Vec<MinuteBucket>,
   pub totals:         RequestCounts,
   pub latency:        LatencySummary,
   pub restrictions:   Vec<RestrictionMinute>,
   /// Process-local total of events lost to queue pressure or failed writes.
   pub dropped_events: u64,
}

#[derive(Serialize)]
pub struct MinuteBucket {
   /// UTC Unix seconds at the start of this minute.
   pub minute_unix: i64,
   pub counts:      RequestCounts,
   pub latency:     LatencySummary,
}

#[derive(Default, Serialize)]
pub struct RequestCounts {
   pub received:     i64,
   pub success:      i64,
   pub empty:        i64,
   pub rejected:     i64,
   pub rate_limited: i64,
   pub failed:       i64,
   pub other:        i64,
   /// Turnstile and scrape-gate challenges served. Not a fault.
   pub challenged:   i64,
}

impl RequestCounts {
   fn finish(&mut self, outcome: Outcome) {
      let count = match outcome {
         Outcome::Success => &mut self.success,
         Outcome::Empty => &mut self.empty,
         Outcome::Rejected => &mut self.rejected,
         Outcome::RateLimited => &mut self.rate_limited,
         Outcome::Failed => &mut self.failed,
         Outcome::Other => &mut self.other,
         Outcome::Challenged => &mut self.challenged,
      };
      *count += 1;
   }

   fn add(&mut self, other: &Self) {
      self.received += other.received;
      self.success += other.success;
      self.empty += other.empty;
      self.rejected += other.rejected;
      self.rate_limited += other.rate_limited;
      self.failed += other.failed;
      self.other += other.other;
      self.challenged += other.challenged;
   }

   fn finished(&self) -> i64 {
      self.success
         + self.empty
         + self.rejected
         + self.rate_limited
         + self.failed
         + self.other
         + self.challenged
   }
}

#[derive(Serialize)]
pub struct LatencySummary {
   pub samples: i64,
   pub avg_ms:  Option<f64>,
   pub max_ms:  Option<i64>,
}

impl LatencySummary {
   fn new(samples: i64, sum: i64, max: i64) -> Self {
      Self {
         samples,
         avg_ms: (samples > 0).then(|| sum as f64 / samples as f64),
         max_ms: (samples > 0).then_some(max),
      }
   }
}

#[derive(Serialize)]
pub struct RestrictionMinute {
   pub minute_unix: i64,
   pub search_only: i64,
   pub suspended:   i64,
}

impl StatsStore {
   pub async fn connect() -> Result<Self, Error> {
      let url = env::var("TEAPOT_STATS_DATABASE_URL")
         .or_else(|_| env::var("TEAPOT_LIMITS_DATABASE_URL"))
         .unwrap_or_else(|_| DEFAULT_URL.to_owned());
      let (client, connection) = tokio_postgres::connect(&url, NoTls).await?;
      tokio::spawn(async move {
         if let Err(error) = connection.await {
            tracing::warn!(%error, "postgres stats connection closed");
         }
      });
      client.batch_execute(SCHEMA).await?;
      let client = Arc::new(client);
      let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
      let dropped_events = Arc::new(AtomicU64::new(0));
      tokio::spawn(run_writer(
         Arc::clone(&client),
         rx,
         Arc::clone(&dropped_events),
      ));
      Ok(Self {
         client,
         tx,
         dropped_events,
      })
   }

   /// A receive is recorded at start time, independently of its eventual
   /// outcome.
   pub fn request_started(&self) {
      self.submit(Event::Started(minute(now_unix())));
   }

   /// Outcomes and latencies are recorded in the minute the request finished.
   pub fn request_finished(&self, outcome: Outcome, elapsed: Duration) {
      let elapsed_ms = elapsed.as_millis().min(i32::MAX as u128) as i64;
      self.submit(Event::Finished(minute(now_unix()), outcome, elapsed_ms));
   }

   pub fn account_event(&self, event: AccountEvent) {
      if let Some(kind) = restricted_bucket_kind(event.kind) {
         self.submit(Event::Restricted(
            event.session_id,
            kind,
            event.observed_unix,
         ));
      }
      self.submit(Event::Account(event));
   }

   pub fn search_account_outcome(&self, session_id: i64, outcome: SearchAccountOutcome) {
      let now = now_unix();
      self.submit(Event::SearchAccount(minute(now), session_id, outcome, now));
   }

   pub async fn search_account_summaries(
      &self,
      minutes: i32,
      limit: i64,
   ) -> Result<Vec<SearchAccountSummary>, Error> {
      let minutes = minutes.clamp(1, RETENTION_MINUTES);
      let limit = limit.clamp(1, 500);
      let end_unix = minute(now_unix()) + 60;
      let start_unix = end_unix - i64::from(minutes) * 60;
      self
         .client
         .query(
            "SELECT
                session_id,
                COALESCE(SUM(non_empty), 0)::bigint AS non_empty,
                COALESCE(SUM(empty), 0)::bigint AS empty,
                COALESCE(SUM(rate_limited), 0)::bigint AS rate_limited,
                MAX(last_empty_unix) AS last_empty_unix,
                MAX(last_non_empty_unix) AS last_non_empty_unix
             FROM stats_search_account_minutes
             WHERE minute_unix >= $1 AND minute_unix < $2
             GROUP BY session_id
             HAVING COALESCE(SUM(empty), 0) > 0 OR COALESCE(SUM(rate_limited), 0) > 0
             ORDER BY COALESCE(SUM(empty), 0) DESC, COALESCE(SUM(rate_limited), 0) DESC, \
             session_id ASC
             LIMIT $3",
            &[&start_unix, &end_unix, &limit],
         )
         .await?
         .into_iter()
         .map(|row| {
            Ok(SearchAccountSummary {
               session_id:          row.try_get(0)?,
               non_empty:           row.try_get(1)?,
               empty:               row.try_get(2)?,
               rate_limited:        row.try_get(3)?,
               last_empty_unix:     row.try_get(4)?,
               last_non_empty_unix: row.try_get(5)?,
            })
         })
         .collect()
   }

   pub async fn account_search_summary(
      &self,
      session_id: i64,
      minutes: i32,
   ) -> Result<Option<SearchAccountSummary>, Error> {
      let minutes = minutes.clamp(1, RETENTION_MINUTES);
      let end_unix = minute(now_unix()) + 60;
      let start_unix = end_unix - i64::from(minutes) * 60;
      let row = self
         .client
         .query_opt(
            "SELECT
                COALESCE(SUM(non_empty), 0)::bigint AS non_empty,
                COALESCE(SUM(empty), 0)::bigint AS empty,
                COALESCE(SUM(rate_limited), 0)::bigint AS rate_limited,
                MAX(last_empty_unix) AS last_empty_unix,
                MAX(last_non_empty_unix) AS last_non_empty_unix
             FROM stats_search_account_minutes
             WHERE session_id = $1 AND minute_unix >= $2 AND minute_unix < $3
             HAVING COUNT(*) > 0",
            &[&session_id, &start_unix, &end_unix],
         )
         .await?;
      row.map(|row| {
         Ok(SearchAccountSummary {
            session_id,
            non_empty: row.try_get(0)?,
            empty: row.try_get(1)?,
            rate_limited: row.try_get(2)?,
            last_empty_unix: row.try_get(3)?,
            last_non_empty_unix: row.try_get(4)?,
         })
      })
      .transpose()
   }

   pub async fn recent_account_events(
      &self,
      kind: Option<Restriction>,
      before: Option<i64>,
      limit: i64,
   ) -> Result<Vec<AccountEvent>, Error> {
      self
         .recent_account_events_filtered(kind, None, before, limit)
         .await
   }

   /// One page of observations. `before` is the last event ID from the previous
   /// page; identity IDs come from the account database, which may be separate.
   pub async fn recent_account_events_filtered(
      &self,
      kind: Option<Restriction>,
      session_ids: Option<&[i64]>,
      before: Option<i64>,
      limit: i64,
   ) -> Result<Vec<AccountEvent>, Error> {
      let limit = limit.clamp(1, 200);
      let kind = kind.map(|kind| kind as i16);
      let before = before.unwrap_or(i64::MAX);
      self
         .account_events(
            "SELECT id, session_id, kind, observed_unix, endpoint, http_status, upstream_code, \
             evidence
          FROM stats_account_events
          WHERE ($1::smallint IS NULL OR kind = $1) AND id < $2
            AND ($3::bigint[] IS NULL OR session_id = ANY($3))
          ORDER BY id DESC LIMIT $4",
            &[&kind, &before, &session_ids, &limit],
         )
         .await
   }

   pub async fn account_timeline(&self, session_id: i64) -> Result<Vec<AccountEvent>, Error> {
      self
         .account_events(
            "SELECT id, session_id, kind, observed_unix, endpoint, http_status, upstream_code, \
             evidence
          FROM stats_account_events WHERE session_id = $1
          ORDER BY observed_unix DESC, id DESC LIMIT 500",
            &[&session_id],
         )
         .await
   }

   /// Latest recorded operational event for each supplied session ID.
   pub async fn latest_account_events(
      &self,
      session_ids: &[i64],
   ) -> Result<HashMap<i64, AccountEvent>, Error> {
      if session_ids.is_empty() {
         return Ok(HashMap::new());
      }
      let events = self
         .account_events(
            "SELECT e.id, e.session_id, e.kind, e.observed_unix, e.endpoint, e.http_status, \
             e.upstream_code, e.evidence
          FROM unnest($1::bigint[]) AS s(id)
          CROSS JOIN LATERAL (
             SELECT id, session_id, kind, observed_unix, endpoint, http_status, upstream_code, \
             evidence
             FROM stats_account_events
             WHERE session_id = s.id
             ORDER BY observed_unix DESC, id DESC
             LIMIT 1
          ) AS e",
            &[&session_ids],
         )
         .await?;
      Ok(events
         .into_iter()
         .map(|event| (event.session_id, event))
         .collect())
   }

   async fn account_events(
      &self,
      sql: &str,
      params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
   ) -> Result<Vec<AccountEvent>, Error> {
      self
         .client
         .query(sql, params)
         .await?
         .into_iter()
         .map(|row| {
            let kind = Restriction::from_i16(row.try_get(2)?)
               .ok_or_else(|| row.try_get::<_, Option<i16>>(2).unwrap_err())?;
            Ok(AccountEvent {
               id: row.try_get(0)?,
               session_id: row.try_get(1)?,
               kind,
               observed_unix: row.try_get(3)?,
               endpoint: row.try_get(4)?,
               http_status: row.try_get(5)?,
               upstream_code: row.try_get(6)?,
               evidence: row.try_get(7)?,
            })
         })
         .collect()
   }

   fn submit(&self, event: Event) {
      if self.tx.try_send(event).is_err() {
         self.dropped_events.fetch_add(1, Ordering::Relaxed);
      }
   }

   pub async fn snapshot(&self, minutes: i32) -> Result<Snapshot, Error> {
      let minutes = minutes.clamp(1, RETENTION_MINUTES);
      let end_unix = minute(now_unix()) + 60;
      let start_unix = end_unix - i64::from(minutes) * 60;
      let rows = self
         .client
         .query(
            "SELECT minute_unix, received, success, empty, rejected, rate_limited, failed, other,
                 challenged, latency_sum_ms, latency_max_ms
          FROM stats_minute_buckets WHERE minute_unix >= $1 AND minute_unix < $2 ORDER BY \
             minute_unix",
            &[&start_unix, &end_unix],
         )
         .await?;
      let mut buckets = Vec::with_capacity(rows.len());
      let mut totals = RequestCounts::default();
      let mut sum_ms = 0_i64;
      let mut max_ms = 0_i64;
      for row in rows {
         let counts = RequestCounts {
            received:     row.try_get(1)?,
            success:      row.try_get(2)?,
            empty:        row.try_get(3)?,
            rejected:     row.try_get(4)?,
            rate_limited: row.try_get(5)?,
            failed:       row.try_get(6)?,
            other:        row.try_get(7)?,
            challenged:   row.try_get(8)?,
         };
         let sum: i64 = row.try_get(9)?;
         let max = row.try_get(10)?;
         let latency = LatencySummary::new(counts.finished(), sum, max);
         sum_ms += sum;
         max_ms = max_ms.max(max);
         totals.add(&counts);
         buckets.push(MinuteBucket {
            minute_unix: row.try_get(0)?,
            counts,
            latency,
         });
      }
      let search_only = Restriction::SearchOnly as i16;
      let suspended = Restriction::Suspended as i16;
      // One latest observation per account and restriction kind is retained.
      let restrictions = self
         .client
         .query(
            "SELECT last_seen_unix / 60 * 60 AS minute_unix,
                 count(*) FILTER (WHERE kind = $3), count(*) FILTER (WHERE kind = $4)
          FROM stats_restrictions WHERE last_seen_unix >= $1 AND last_seen_unix < $2
          GROUP BY 1 ORDER BY 1 DESC",
            &[&start_unix, &end_unix, &search_only, &suspended],
         )
         .await?
         .into_iter()
         .map(|row| {
            Ok(RestrictionMinute {
               minute_unix: row.try_get(0)?,
               search_only: row.try_get(1)?,
               suspended:   row.try_get(2)?,
            })
         })
         .collect::<Result<Vec<_>, Error>>()?;
      Ok(Snapshot {
         minutes,
         start_unix,
         end_unix,
         buckets,
         latency: LatencySummary::new(totals.finished(), sum_ms, max_ms),
         totals,
         restrictions,
         dropped_events: self.dropped_events.load(Ordering::Relaxed),
      })
   }
}

fn now_unix() -> i64 {
   SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap_or_default()
      .as_secs() as i64
}

const fn minute(unix: i64) -> i64 {
   unix - unix.rem_euclid(60)
}

const fn restricted_bucket_kind(kind: Restriction) -> Option<Restriction> {
   match kind {
      Restriction::SearchOnly | Restriction::SearchBanned => Some(Restriction::SearchOnly),
      Restriction::Suspended => Some(Restriction::Suspended),
      _ => None,
   }
}

enum Event {
   Started(i64),
   Finished(i64, Outcome, i64),
   Restricted(i64, Restriction, i64),
   Account(AccountEvent),
   SearchAccount(i64, i64, SearchAccountOutcome, i64),
}

#[derive(Default, Serialize)]
struct MinuteUpdate {
   minute_unix:    i64,
   #[serde(flatten)]
   counts:         RequestCounts,
   latency_sum_ms: i64,
   latency_max_ms: i64,
}

#[derive(Default)]
struct SearchAccountMinuteUpdate {
   non_empty:           i64,
   empty:               i64,
   rate_limited:        i64,
   last_empty_unix:     Option<i64>,
   last_non_empty_unix: Option<i64>,
}

#[derive(Default)]
struct Batch {
   minutes:         BTreeMap<i64, MinuteUpdate>,
   restrictions:    HashMap<(i64, i16), i64>,
   account_events:  Vec<AccountEvent>,
   search_accounts: BTreeMap<(i64, i64), SearchAccountMinuteUpdate>,
   events:          usize,
}

impl Batch {
   fn minute(&mut self, key: i64) -> &mut MinuteUpdate {
      self.minutes.entry(key).or_insert_with(|| {
         MinuteUpdate {
            minute_unix: key,
            ..Default::default()
         }
      })
   }

   fn record(&mut self, event: Event) {
      self.events += 1;
      match event {
         Event::Started(key) => self.minute(key).counts.received += 1,
         Event::Finished(key, outcome, elapsed_ms) => {
            let update = self.minute(key);
            update.counts.finish(outcome);
            update.latency_sum_ms += elapsed_ms;
            update.latency_max_ms = update.latency_max_ms.max(elapsed_ms);
         },
         Event::Restricted(session_id, kind, when) => {
            let latest = self
               .restrictions
               .entry((session_id, kind as i16))
               .or_insert(when);
            *latest = (*latest).max(when);
         },
         Event::Account(event) => self.account_events.push(event),
         Event::SearchAccount(minute_unix, session_id, outcome, when) => {
            let entry = self
               .search_accounts
               .entry((minute_unix, session_id))
               .or_default();
            match outcome {
               SearchAccountOutcome::NonEmpty => {
                  entry.non_empty += 1;
                  entry.last_non_empty_unix = Some(
                     entry
                        .last_non_empty_unix
                        .map_or(when, |prev| prev.max(when)),
                  );
               },
               SearchAccountOutcome::Empty => {
                  entry.empty += 1;
                  entry.last_empty_unix =
                     Some(entry.last_empty_unix.map_or(when, |prev| prev.max(when)));
               },
               SearchAccountOutcome::RateLimited => {
                  entry.rate_limited += 1;
               },
            }
         },
      }
   }

   async fn write(&self, client: &Client) -> Result<(), Error> {
      let minutes = serde_json::json!(self.minutes.values().collect::<Vec<_>>());
      // Pass IDs directly as typed PostgreSQL parameters, never through Serde.
      let mut ids = Vec::with_capacity(self.restrictions.len());
      let mut kinds = Vec::with_capacity(self.restrictions.len());
      let mut times = Vec::with_capacity(self.restrictions.len());
      for (&(session_id, kind), &last_seen_unix) in &self.restrictions {
         ids.push(session_id);
         kinds.push(kind);
         times.push(last_seen_unix);
      }
      client
         .execute(WRITE_BATCH, &[&minutes, &ids, &kinds, &times])
         .await?;
      if !self.search_accounts.is_empty() {
         let cap = self.search_accounts.len();
         let mut sa_minutes = Vec::with_capacity(cap);
         let mut sa_ids = Vec::with_capacity(cap);
         let mut sa_non_empty = Vec::with_capacity(cap);
         let mut sa_empty = Vec::with_capacity(cap);
         let mut sa_rate_limited = Vec::with_capacity(cap);
         let mut sa_last_empty: Vec<Option<i64>> = Vec::with_capacity(cap);
         let mut sa_last_non_empty: Vec<Option<i64>> = Vec::with_capacity(cap);
         for (&(minute_unix, session_id), update) in &self.search_accounts {
            sa_minutes.push(minute_unix);
            sa_ids.push(session_id);
            sa_non_empty.push(update.non_empty);
            sa_empty.push(update.empty);
            sa_rate_limited.push(update.rate_limited);
            sa_last_empty.push(update.last_empty_unix);
            sa_last_non_empty.push(update.last_non_empty_unix);
         }
         client
            .execute(
               "INSERT INTO stats_search_account_minutes
                (minute_unix, session_id, non_empty, empty, rate_limited, last_empty_unix, \
                last_non_empty_unix)
                SELECT * FROM unnest(
                   $1::bigint[], $2::bigint[], $3::bigint[], $4::bigint[], $5::bigint[], \
                $6::bigint[], $7::bigint[]
                ) AS s (
                   minute_unix, session_id, non_empty, empty, rate_limited, last_empty_unix, \
                last_non_empty_unix
                )
                ORDER BY minute_unix, session_id
                ON CONFLICT (minute_unix, session_id) DO UPDATE SET
                   non_empty = stats_search_account_minutes.non_empty + EXCLUDED.non_empty,
                   empty = stats_search_account_minutes.empty + EXCLUDED.empty,
                   rate_limited = stats_search_account_minutes.rate_limited + \
                EXCLUDED.rate_limited,
                   last_empty_unix = GREATEST(
                      stats_search_account_minutes.last_empty_unix,
                      EXCLUDED.last_empty_unix
                   ),
                   last_non_empty_unix = GREATEST(
                      stats_search_account_minutes.last_non_empty_unix,
                      EXCLUDED.last_non_empty_unix
                   )",
               &[
                  &sa_minutes,
                  &sa_ids,
                  &sa_non_empty,
                  &sa_empty,
                  &sa_rate_limited,
                  &sa_last_empty,
                  &sa_last_non_empty,
               ],
            )
            .await?;
      }
      if !self.account_events.is_empty() {
         let events = self
            .account_events
            .iter()
            .map(|event| {
               serde_json::json!({
                  "session_id": event.session_id,
                  "kind": event.kind as i16,
                  "observed_unix": event.observed_unix,
                  "endpoint": event.endpoint,
                  "http_status": event.http_status,
                  "upstream_code": event.upstream_code,
                  "evidence": event.evidence,
               })
            })
            .collect::<Vec<_>>();
         client
            .execute(
               "INSERT INTO stats_account_events
             (session_id, kind, observed_unix, endpoint, http_status, upstream_code, evidence)
             SELECT * FROM jsonb_to_recordset($1) AS e (
               session_id bigint, kind smallint, observed_unix bigint, endpoint text,
               http_status integer, upstream_code integer, evidence text)",
               &[&serde_json::Value::Array(events)],
            )
            .await?;
      }
      Ok(())
   }
}

async fn run_writer(
   client: Arc<Client>,
   mut rx: mpsc::Receiver<Event>,
   dropped_events: Arc<AtomicU64>,
) {
   let mut ticker = interval(FLUSH_INTERVAL);
   ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
   let mut batch = Batch::default();
   let mut last_heartbeat = None;
   let mut last_maintenance = None;
   let mut last_warning: Option<Instant> = None;
   loop {
      let closed = tokio::select! {
         _ = ticker.tick() => false,
         event = rx.recv() => match event {
            Some(event) => {
               batch.record(event);
               if batch.events < BATCH_CAPACITY {
                  continue;
               }
               false
            },
            None => true,
         },
      };
      let now = now_unix();
      let current_minute = minute(now);
      if !closed && last_heartbeat != Some(current_minute) {
         batch.minute(current_minute);
      }
      if !batch.minutes.is_empty()
         || !batch.restrictions.is_empty()
         || !batch.account_events.is_empty()
         || !batch.search_accounts.is_empty()
      {
         match batch.write(&client).await {
            Ok(()) => last_heartbeat = Some(current_minute),
            Err(error) => {
               dropped_events.fetch_add(batch.events as u64, Ordering::Relaxed);
               if last_warning.is_none_or(|time| time.elapsed() >= Duration::from_secs(60)) {
                  tracing::warn!(%error, "could not write postgres stats batch");
                  last_warning = Some(Instant::now());
               }
            },
         }
         // Retrying an ambiguous write can double-count and holding it can stall
         // subsequent telemetry indefinitely. Drop the failed batch instead.
         batch = Batch::default();
      }
      if closed {
         break;
      }
      if last_maintenance.is_none_or(|time: Instant| time.elapsed() >= MAINTENANCE_INTERVAL) {
         last_maintenance = Some(Instant::now());
         if let Err(error) = prune(&client, now).await {
            tracing::warn!(%error, "could not prune postgres stats");
         }
      }
   }
}

async fn prune(client: &Client, now: i64) -> Result<(), Error> {
   let retention = i64::from(RETENTION_MINUTES) * 60;
   let bucket_cutoff = minute(now) - retention;
   let restriction_cutoff = now - retention;
   client
      .execute(
         "WITH pruned_minutes AS (DELETE FROM stats_minute_buckets WHERE minute_unix < $1),
          pruned_search_accounts AS (DELETE FROM stats_search_account_minutes WHERE minute_unix < \
          $1),
          pruned_restrictions AS (DELETE FROM stats_restrictions WHERE last_seen_unix < $2)
          DELETE FROM stats_account_events WHERE observed_unix < $2",
         &[&bucket_cutoff, &restriction_cutoff],
      )
      .await?;
   Ok(())
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn separates_start_and_finish_minutes_and_tracks_all_outcomes() {
      let mut batch = Batch::default();
      batch.record(Event::Started(minute(119)));
      for outcome in [
         Outcome::Success,
         Outcome::Empty,
         Outcome::Rejected,
         Outcome::RateLimited,
         Outcome::Failed,
         Outcome::Other,
      ] {
         batch.record(Event::Finished(minute(120), outcome, 25));
      }
      let started = &batch.minutes[&60];
      let finished = &batch.minutes[&120];
      assert_eq!(started.counts.received, 1);
      assert_eq!(started.counts.finished(), 0);
      assert_eq!(finished.counts.received, 0);
      assert_eq!(finished.counts.finished(), 6);
      assert_eq!(finished.counts.success, 1);
      assert_eq!(finished.counts.empty, 1);
      assert_eq!(finished.counts.rejected, 1);
      assert_eq!(finished.counts.rate_limited, 1);
      assert_eq!(finished.counts.failed, 1);
      assert_eq!(finished.counts.other, 1);
      assert_eq!(finished.latency_sum_ms, 150);
      assert_eq!(finished.latency_max_ms, 25);
   }

   #[test]
   fn heartbeat_does_not_backfill_missing_minutes() {
      let mut batch = Batch::default();
      batch.minute(60);
      batch.minute(180);
      assert_eq!(batch.minutes.keys().copied().collect::<Vec<_>>(), [60, 180]);
      assert_eq!(batch.minutes[&180].counts.received, 0);
      assert_eq!(batch.events, 0);
   }

   #[test]
   fn deduplicates_restrictions_by_kind_and_keeps_the_latest_time() {
      let mut batch = Batch::default();
      batch.record(Event::Restricted(1, Restriction::SearchOnly, 40));
      batch.record(Event::Restricted(1, Restriction::SearchOnly, 30));
      batch.record(Event::Restricted(1, Restriction::Suspended, 45));
      batch.record(Event::Restricted(2, Restriction::SearchOnly, 50));
      assert_eq!(batch.restrictions.len(), 3);
      assert_eq!(batch.restrictions[&(1, Restriction::SearchOnly as i16)], 40);
      assert_eq!(batch.restrictions[&(1, Restriction::Suspended as i16)], 45);
   }

   #[tokio::test]
   #[ignore = "requires a local PostgreSQL account that can create temporary tables"]
   async fn postgres_stats_round_trip() {
      let url = env::var("TEAPOT_STATS_TEST_DATABASE_URL").unwrap();
      let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
      tokio::spawn(connection);
      client
         .batch_execute(&SCHEMA.replace("CREATE TABLE", "CREATE TEMP TABLE"))
         .await
         .unwrap();
      let client = Arc::new(client);
      let (tx, _) = mpsc::channel(1);
      let store = StatsStore {
         client: Arc::clone(&client),
         tx,
         dropped_events: Arc::default(),
      };
      let timestamp = now_unix();
      let key = minute(timestamp);
      let mut batch = Batch::default();
      batch.record(Event::Started(key));
      batch.record(Event::Finished(key, Outcome::Empty, 35));
      batch.record(Event::Restricted(17, Restriction::Suspended, timestamp));
      batch.record(Event::SearchAccount(
         key,
         1235,
         SearchAccountOutcome::Empty,
         timestamp,
      ));
      batch.record(Event::SearchAccount(
         key,
         1235,
         SearchAccountOutcome::RateLimited,
         timestamp,
      ));
      batch.write(&client).await.unwrap();
      batch.write(&client).await.unwrap();
      let mut batch2 = Batch::default();
      batch2.record(Event::SearchAccount(
         key,
         1235,
         SearchAccountOutcome::NonEmpty,
         timestamp + 5,
      ));
      batch2.write(&client).await.unwrap();
      let snapshot = store.snapshot(15).await.unwrap();
      assert_eq!(snapshot.totals.received, 2);
      assert_eq!(snapshot.totals.empty, 2);
      assert_eq!(snapshot.latency.avg_ms, Some(35.0));
      assert_eq!(snapshot.restrictions.len(), 1);
      assert_eq!(snapshot.restrictions[0].suspended, 1);

      let summaries = store.search_account_summaries(15, 10).await.unwrap();
      assert_eq!(summaries.len(), 1);
      assert_eq!(summaries[0].session_id, 1235);
      assert_eq!(summaries[0].empty, 2);
      assert_eq!(summaries[0].non_empty, 1);
      assert_eq!(summaries[0].rate_limited, 2);
      assert_eq!(summaries[0].last_empty_unix, Some(timestamp));
      assert_eq!(summaries[0].last_non_empty_unix, Some(timestamp + 5));

      let single = store
         .account_search_summary(1235, 15)
         .await
         .unwrap()
         .unwrap();
      assert_eq!(single, summaries[0]);
      assert_eq!(store.account_search_summary(9999, 15).await.unwrap(), None);

      prune(&client, timestamp).await.unwrap();
   }

   #[test]
   fn snapshot_exposes_aggregates_without_session_ids() {
      let snapshot = Snapshot {
         minutes:        1,
         start_unix:     60,
         end_unix:       120,
         buckets:        vec![],
         totals:         RequestCounts::default(),
         latency:        LatencySummary::new(0, 0, 0),
         restrictions:   vec![RestrictionMinute {
            minute_unix: 60,
            search_only: 1,
            suspended:   0,
         }],
         dropped_events: 0,
      };
      let json = serde_json::to_value(snapshot).unwrap();
      assert!(!json.to_string().contains("session_id"));
      assert_eq!(json["restrictions"][0]["search_only"], 1);
      assert!(json["latency"]["avg_ms"].is_null());
      let latency = LatencySummary::new(2, 31, 20);
      assert_eq!(latency.avg_ms, Some(15.5));
      assert_eq!(latency.max_ms, Some(20));
   }

   #[test]
   fn maps_account_restrictions_to_separate_search_and_suspended_buckets() {
      assert_eq!(
         restricted_bucket_kind(Restriction::SearchOnly),
         Some(Restriction::SearchOnly)
      );
      assert_eq!(
         restricted_bucket_kind(Restriction::SearchBanned),
         Some(Restriction::SearchOnly)
      );
      assert_eq!(
         restricted_bucket_kind(Restriction::Suspended),
         Some(Restriction::Suspended)
      );
      assert_eq!(restricted_bucket_kind(Restriction::Reinstated), None);
      assert_eq!(restricted_bucket_kind(Restriction::RateLimited), None);
      assert_eq!(restricted_bucket_kind(Restriction::SearchEmptyStreak), None);
      assert_eq!(
         Restriction::from_i16(12),
         Some(Restriction::SearchEmptyStreak)
      );
   }

   #[test]
   fn aggregates_search_account_outcomes_per_minute_and_session() {
      let mut batch = Batch::default();
      batch.record(Event::SearchAccount(
         60,
         1235,
         SearchAccountOutcome::Empty,
         70,
      ));
      batch.record(Event::SearchAccount(
         60,
         1235,
         SearchAccountOutcome::Empty,
         85,
      ));
      batch.record(Event::SearchAccount(
         60,
         1235,
         SearchAccountOutcome::RateLimited,
         90,
      ));
      batch.record(Event::SearchAccount(
         60,
         5183,
         SearchAccountOutcome::NonEmpty,
         75,
      ));
      batch.record(Event::SearchAccount(
         60,
         5183,
         SearchAccountOutcome::Empty,
         80,
      ));

      let broken = &batch.search_accounts[&(60, 1235)];
      assert_eq!(broken.non_empty, 0);
      assert_eq!(broken.empty, 2);
      assert_eq!(broken.rate_limited, 1);
      assert_eq!(broken.last_empty_unix, Some(85));
      assert_eq!(broken.last_non_empty_unix, None);

      let healthy = &batch.search_accounts[&(60, 5183)];
      assert_eq!(healthy.non_empty, 1);
      assert_eq!(healthy.empty, 1);
      assert_eq!(healthy.rate_limited, 0);
      assert_eq!(healthy.last_empty_unix, Some(80));
      assert_eq!(healthy.last_non_empty_unix, Some(75));
   }
}
