//! Cron channels: the authored `transport_config`, and the compiled schedule
//! the reconciler plans passes against.
//!
//! A cron channel is an ingress like any other — it binds a trigger to a
//! workflow and carries the same guards, timeout and trace policy. What is
//! different is that the trigger is a clock rather than a caller, so the
//! *schedule* has to be compiled once, at load, and be a pure function of the
//! stored definition from then on.
//!
//! This module is deliberately a leaf inside `channel`, not part of `cron/`.
//! [`ChannelLoader::build_runtime`](crate::channel::ChannelLoader) compiles the
//! descriptor so a schedule that no longer parses **quarantines its channel**
//! rather than failing at the first fire — the same N3 posture the rest of
//! `config_json` takes. `channel` may not name `cron`, `engine` or `queue`
//! (`module_layering_test`), so the compiler lives here and the scheduler
//! consumes it.
//!
//! # Wall clock in, UTC out
//!
//! A cron expression names *local calendar times*: "02:15 every day" means
//! 02:15 as a person reading a wall clock in the configured zone sees it.
//! Orion stores and compares UTC instants. The conversion between the two is
//! the only interesting thing this module does, and it is deliberately Orion's
//! code rather than a dependency's behaviour, because the two DST rules are a
//! documented product contract:
//!
//! - a local time that **does not exist** (the spring-forward gap) does not
//!   fire;
//! - a local time that happens **twice** (the fall-back repeat) fires twice,
//!   because the two instants have different UTC offsets and are therefore
//!   different `scheduled_for` values with different occurrence identities.
//!
//! [`chrono_tz`] answers both questions exactly — `LocalResult::None` and
//! `LocalResult::Ambiguous` — so the rules are a `match`, not an assumption.
//! The `cron` crate is used only for calendar matching, over `DateTime<Utc>`
//! values that stand in for wall-clock time: `Utc` has no offset and no
//! transitions, so iterating a schedule "in UTC" over naive local values is
//! pure field arithmetic with no zone semantics of its own.

use std::str::FromStr;

use chrono::{Duration, LocalResult, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::FieldError;

/// Fields a cron expression must have: second, minute, hour, day-of-month,
/// month, day-of-week.
///
/// Exactly six, and the count is checked before the parser runs. The `cron`
/// crate accepts five (no seconds) and seven (a trailing year) as well, and both
/// are refused here for the same reason: read as five fields, `0 15 2 * * *`
/// fires every minute for an hour instead of once a day, and an author has no
/// way to see which dialect they got. One dialect, stated in the reference,
/// checked at authoring.
const CRON_FIELDS: usize = 6;

/// How far ahead a compiled schedule must have at least one occurrence.
///
/// Catches the impossible-date class — `0 0 0 30 2 *` is a syntactically
/// perfect expression for February 30th, which never arrives. Without this the
/// channel activates, the reconciler searches to the parser's year ceiling on
/// every pass, and nothing ever fires. Five years is comfortably longer than
/// any real schedule's gap (a leap-day job is four).
const MAX_HORIZON_DAYS: i64 = 365 * 5;

/// Hard ceiling on a channel's `max_catch_up`, independent of
/// `cron.max_catch_up`.
///
/// The instance setting is an operator's *budget* and clamps this further at
/// run time (`min(channel, instance)`); this is the authoring bound, so a
/// definition promoted to an instance with a larger budget still cannot ask
/// for an unbounded replay.
const MAX_CATCH_UP_CEILING: u32 = 1000;

/// Ceiling on the serialized authored payload.
///
/// A fixed bound rather than `ingest.max_payload_size`: that setting caps what
/// a *caller* may send, and a cron payload has no caller — it is definition
/// content, versioned and content-hashed like a route pattern, so it is bounded
/// the way `route_pattern`'s 255 characters are. The value matches the default
/// request cap so the two do not surprise each other.
const MAX_PAYLOAD_BYTES: usize = 1_048_576;

/// Ceiling on a singleton key. Long enough for a reverse-domain name, short
/// enough to be a primary key on MySQL without an index-length prefix.
const MAX_SINGLETON_KEY_LEN: usize = 128;

/// Defensive bound on one [`CronDescriptor::fires_in`] walk.
///
/// The walk is over *matching* instants, so a sane window costs one iteration
/// per fire. The bound exists for the pathological shapes: a spring-forward gap
/// under a per-second schedule burns an hour of wall clock producing nothing,
/// and a zone with a historical multi-hour transition burns more. Reaching it
/// truncates the pass — the cursor still advances, so the next pass continues
/// rather than repeating.
const MAX_WALK_STEPS: usize = 200_000;

/// What a misfired occurrence — one whose scheduled time passed while no
/// healthy scheduler could start it — does when the scheduler comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MisfirePolicy {
    /// Record the miss and move on. Nothing runs late.
    Skip,
    /// Run the newest missed occurrence only. The default, and the right answer
    /// for the overwhelmingly common "rebuild yesterday's summary" shape: one
    /// run brings the world up to date and replaying the rest would repeat it.
    #[default]
    Latest,
    /// Run the missed occurrences oldest-first, bounded by `max_catch_up`. For
    /// schedules where each occurrence does distinct work.
    CatchUp,
}

impl MisfirePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Latest => "latest",
            Self::CatchUp => "catch_up",
        }
    }
}

/// What happens when an occurrence becomes due while its singleton key is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyPolicy {
    /// Occurrences may overlap. No singleton row is taken at all.
    #[default]
    Allow,
    /// At most one occurrence per key runs at a time; a contending occurrence
    /// is recorded `skipped_singleton` rather than deferred or dropped.
    Forbid,
    /// At most one running and at most one deferred. Not implemented yet —
    /// validation refuses it, and the storage model already supports it.
    QueueOne,
}

impl ConcurrencyPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Forbid => "forbid",
            Self::QueueOne => "queue_one",
        }
    }
}

/// The `concurrency` object of a cron channel's `transport_config`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcurrencyConfig {
    #[serde(default)]
    pub policy: ConcurrencyPolicy,
    /// The lock identity. Literal and validated, defaulting to the channel's
    /// `channel_id` — one singleton per scheduled channel.
    ///
    /// Deliberately not a JSONLogic expression: dynamic lock cardinality cannot
    /// be bounded at authoring time, and an expression that fails would fail at
    /// *firing* time, in the one code path whose whole job is to be predictable.
    /// Two channels naming the same key serialise with each other, which is the
    /// only cross-channel coordination this needs to express.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// A cron channel's authored `transport_config`.
///
/// `deny_unknown_fields` for the reason [`ChannelConfig`](super::ChannelConfig)
/// gives: every key here changes *when work runs*, and a key this struct does
/// not recognise is a scheduling decision that silently did not happen. A
/// `"misfire_polcy"` typo would leave the default in place forever, and the
/// stored document is the operator's own — nothing re-serializes it, so the
/// typo survives every reload. Refused at create time, quarantined at load.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CronTransportConfig {
    /// Six-field cron expression: second minute hour day-of-month month
    /// day-of-week.
    pub schedule: String,

    /// IANA time-zone name the expression's calendar times are read in.
    /// Defaults to `UTC`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,

    /// The `data` every occurrence of this schedule runs with. Defaults to `{}`.
    ///
    /// Definition content: versioned, content-hashed and promoted with the
    /// channel. Secrets are refused here — a workflow reaches the engine's
    /// secret store instead — because this value is recorded verbatim in every
    /// occurrence's trace input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,

    #[serde(default)]
    pub misfire_policy: MisfirePolicy,

    /// Bound on a `catch_up` replay. Required in spirit for `catch_up` and
    /// defaulted to 1 for the other policies, where it is unused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_catch_up: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<ConcurrencyConfig>,
}

/// The identity a descriptor is compiled for — the channel row's own fields,
/// passed in rather than re-read so compilation stays a pure function.
#[derive(Debug, Clone)]
pub struct CronIdentity {
    pub channel_id: String,
    pub channel_name: String,
    pub version: i64,
    pub workflow_id: Option<String>,
}

/// A compiled cron schedule: everything the reconciler and the worker need,
/// with nothing left to parse.
///
/// Carried on [`ChannelRuntimeConfig`](super::registry::ChannelRuntimeConfig)
/// and therefore reused by `Arc` across a reload whose channel row did not move
/// (N6/N17) — compiling a schedule is cheap, but the cursor identity below is
/// not something to recompute for no reason.
pub struct CronDescriptor {
    pub channel_id: String,
    pub channel_name: String,
    pub version: i64,
    pub workflow_id: Option<String>,
    schedule: cron::Schedule,
    /// The authored expression, kept for diagnostics and the status endpoint.
    pub expression: String,
    pub timezone: Tz,
    pub payload: Value,
    pub misfire: MisfirePolicy,
    pub max_catch_up: u32,
    pub concurrency: ConcurrencyPolicy,
    pub singleton_key: String,
    /// SHA-256 over the *scheduling* fields — expression, zone, misfire policy
    /// and catch-up bound — and nothing else.
    ///
    /// This is what decides whether a new channel version keeps its cursor.
    /// Payload and concurrency are deliberately excluded: editing what a job
    /// does, or how it locks, must not silently reset when it next runs.
    pub config_hash: String,
}

impl std::fmt::Debug for CronDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CronDescriptor")
            .field("channel_id", &self.channel_id)
            .field("expression", &self.expression)
            .field("timezone", &self.timezone.name())
            .field("misfire", &self.misfire)
            .field("concurrency", &self.concurrency)
            .field("singleton_key", &self.singleton_key)
            .field("config_hash", &self.config_hash)
            .finish_non_exhaustive()
    }
}

/// Parse a channel's stored `transport_config_json` and compile it.
///
/// The two failure classes are one type on purpose: an author sees the same
/// `FieldError` list whether their document had an unknown key or an
/// unresolvable time zone.
pub fn compile_from_json(
    transport_config_json: &str,
    identity: CronIdentity,
) -> Result<CronDescriptor, Vec<FieldError>> {
    let parsed: CronTransportConfig = serde_json::from_str(transport_config_json).map_err(|e| {
        vec![FieldError::new(
            "channel.transport_config",
            orion_api::error::field_codes::INVALID,
            format!("cron transport_config does not parse: {e}"),
        )]
    })?;
    parsed.compile(identity)
}

impl CronTransportConfig {
    /// Compile, collecting **every** problem rather than stopping at the first
    /// — the same B1 posture the protocol field matrix takes, so one round trip
    /// tells an author everything wrong with their schedule.
    pub fn compile(&self, identity: CronIdentity) -> Result<CronDescriptor, Vec<FieldError>> {
        let mut errors = Vec::new();

        let expression = self.schedule.trim().to_string();
        let schedule = compile_expression(&expression, &mut errors);
        let timezone = compile_timezone(self.timezone.as_deref(), &mut errors);
        let payload = compile_payload(self.payload.as_ref(), &mut errors);
        let max_catch_up = compile_catch_up(self.max_catch_up, self.misfire_policy, &mut errors);
        let (concurrency, singleton_key) =
            compile_concurrency(self.concurrency.as_ref(), &identity.channel_id, &mut errors);

        // The horizon check needs both halves, so it runs only once both
        // parsed. A schedule that did not parse has already reported why.
        if let (Some(schedule), Some(timezone)) = (schedule.as_ref(), timezone) {
            check_horizon(schedule, timezone, &expression, &mut errors);
        }

        if !errors.is_empty() {
            return Err(errors);
        }
        let schedule = schedule.expect("no errors means the expression compiled");
        let timezone = timezone.expect("no errors means the zone compiled");

        let config_hash = scheduling_hash(&expression, timezone, self.misfire_policy, max_catch_up);
        Ok(CronDescriptor {
            channel_id: identity.channel_id,
            channel_name: identity.channel_name,
            version: identity.version,
            workflow_id: identity.workflow_id,
            schedule,
            expression,
            timezone,
            payload,
            misfire: self.misfire_policy,
            max_catch_up,
            concurrency,
            singleton_key,
            config_hash,
        })
    }
}

fn invalid(path: &'static str, message: String) -> FieldError {
    FieldError::new(path, orion_api::error::field_codes::INVALID, message)
}

fn compile_expression(expression: &str, errors: &mut Vec<FieldError>) -> Option<cron::Schedule> {
    if expression.is_empty() {
        errors.push(FieldError::new(
            "channel.transport_config.schedule",
            orion_api::error::field_codes::REQUIRED,
            "a cron channel must declare a schedule".to_string(),
        ));
        return None;
    }
    let fields = expression.split_whitespace().count();
    if fields != CRON_FIELDS {
        errors.push(invalid(
            "channel.transport_config.schedule",
            format!(
                "schedule must have exactly {CRON_FIELDS} fields \
                 (second minute hour day-of-month month day-of-week), got {fields}: \
                 \"{expression}\". Orion's dialect always states seconds — \
                 a daily 02:15 is \"0 15 2 * * *\", not \"15 2 * * *\"."
            ),
        ));
        return None;
    }
    match cron::Schedule::from_str(expression) {
        Ok(schedule) => Some(schedule),
        Err(e) => {
            errors.push(invalid(
                "channel.transport_config.schedule",
                format!("schedule \"{expression}\" does not parse: {e}"),
            ));
            None
        }
    }
}

fn compile_timezone(timezone: Option<&str>, errors: &mut Vec<FieldError>) -> Option<Tz> {
    let name = timezone.map(str::trim).filter(|t| !t.is_empty());
    let Some(name) = name else {
        return Some(Tz::UTC);
    };
    match name.parse::<Tz>() {
        Ok(tz) => Some(tz),
        Err(_) => {
            errors.push(invalid(
                "channel.transport_config.timezone",
                format!(
                    "\"{name}\" is not an IANA time-zone name. Use a zone from the \
                     tz database, for example \"UTC\", \"Europe/London\" or \
                     \"Asia/Kolkata\" — abbreviations like \"IST\" or \"EST\" are \
                     ambiguous and are not accepted."
                ),
            ));
            None
        }
    }
}

fn compile_payload(payload: Option<&Value>, errors: &mut Vec<FieldError>) -> Value {
    let Some(payload) = payload else {
        return Value::Object(serde_json::Map::new());
    };
    if !payload.is_object() {
        errors.push(FieldError::new(
            "channel.transport_config.payload",
            orion_api::error::field_codes::TYPE_MISMATCH,
            format!(
                "payload must be a JSON object — it becomes the run's `data`, \
                 which every workflow reads by key (got {})",
                crate::engine::utils::json_kind(payload)
            ),
        ));
        return Value::Object(serde_json::Map::new());
    }
    let size = serde_json::to_string(payload).map(|s| s.len()).unwrap_or(0);
    if size > MAX_PAYLOAD_BYTES {
        errors.push(FieldError::new(
            "channel.transport_config.payload",
            orion_api::error::field_codes::TOO_LONG,
            format!("payload is {size} bytes; the limit is {MAX_PAYLOAD_BYTES}"),
        ));
    }
    payload.clone()
}

fn compile_catch_up(
    max_catch_up: Option<u32>,
    policy: MisfirePolicy,
    errors: &mut Vec<FieldError>,
) -> u32 {
    match max_catch_up {
        None if policy == MisfirePolicy::CatchUp => {
            errors.push(FieldError::new(
                "channel.transport_config.max_catch_up",
                orion_api::error::field_codes::REQUIRED,
                "misfire_policy \"catch_up\" must declare max_catch_up: without a \
                 bound, a schedule restored after a long outage floods the engine \
                 with every occurrence it missed"
                    .to_string(),
            ));
            1
        }
        None => 1,
        Some(0) => {
            errors.push(invalid(
                "channel.transport_config.max_catch_up",
                "max_catch_up must be at least 1 — use misfire_policy \"skip\" to \
                 run nothing at all"
                    .to_string(),
            ));
            1
        }
        Some(n) if n > MAX_CATCH_UP_CEILING => {
            errors.push(invalid(
                "channel.transport_config.max_catch_up",
                format!("max_catch_up must be at most {MAX_CATCH_UP_CEILING}, got {n}"),
            ));
            MAX_CATCH_UP_CEILING
        }
        Some(n) => n,
    }
}

fn compile_concurrency(
    concurrency: Option<&ConcurrencyConfig>,
    channel_id: &str,
    errors: &mut Vec<FieldError>,
) -> (ConcurrencyPolicy, String) {
    let Some(concurrency) = concurrency else {
        return (ConcurrencyPolicy::default(), channel_id.to_string());
    };
    if concurrency.policy == ConcurrencyPolicy::QueueOne {
        errors.push(invalid(
            "channel.transport_config.concurrency.policy",
            "\"queue_one\" is not implemented yet — use \"forbid\" to run at most \
             one at a time, or \"allow\" to let occurrences overlap"
                .to_string(),
        ));
    }
    let key = match concurrency.key.as_deref().map(str::trim) {
        None | Some("") => channel_id.to_string(),
        Some(key) => {
            if key.len() > MAX_SINGLETON_KEY_LEN {
                errors.push(FieldError::new(
                    "channel.transport_config.concurrency.key",
                    orion_api::error::field_codes::TOO_LONG,
                    format!("concurrency.key must be at most {MAX_SINGLETON_KEY_LEN} characters"),
                ));
            } else if !is_valid_singleton_key(key) {
                errors.push(invalid(
                    "channel.transport_config.concurrency.key",
                    format!(
                        "concurrency.key \"{key}\" must start with a letter or digit and \
                         contain only letters, digits, '_', '-' and '.' — it is a literal \
                         lock name, not an expression"
                    ),
                ));
            }
            key.to_string()
        }
    };
    (concurrency.policy, key)
}

fn is_valid_singleton_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    key.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Refuse a schedule with no occurrence inside [`MAX_HORIZON_DAYS`].
fn check_horizon(
    schedule: &cron::Schedule,
    timezone: Tz,
    expression: &str,
    errors: &mut Vec<FieldError>,
) {
    let now = Utc::now().naive_utc();
    let horizon = now + Duration::days(MAX_HORIZON_DAYS);
    if walk(schedule, timezone, now, horizon, 1, MAX_WALK_STEPS).is_empty() {
        errors.push(invalid(
            "channel.transport_config.schedule",
            format!(
                "schedule \"{expression}\" has no occurrence in the next \
                 {} years — check the day-of-month and month fields \
                 (\"0 0 0 30 2 *\" is February 30th, which never arrives)",
                MAX_HORIZON_DAYS / 365
            ),
        ));
    }
}

fn scheduling_hash(
    expression: &str,
    timezone: Tz,
    misfire: MisfirePolicy,
    max_catch_up: u32,
) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    // Length-prefixed so no pair of fields can be confused for another
    // ("a" + "bc" and "ab" + "c" must not hash alike).
    for field in [
        expression,
        timezone.name(),
        misfire.as_str(),
        &max_catch_up.to_string(),
    ] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    hex::encode(hasher.finalize())
}

impl CronDescriptor {
    /// The first UTC instant this schedule fires **strictly after** `after`.
    ///
    /// `None` only when the schedule has no future occurrence at all, which
    /// compilation refuses (the horizon check) — so a `None` here means the
    /// horizon has been reached at run time, and the reconciler parks the
    /// cursor rather than looping.
    pub fn next_after(&self, after: NaiveDateTime) -> Option<NaiveDateTime> {
        let horizon = after + Duration::days(MAX_HORIZON_DAYS);
        walk(
            &self.schedule,
            self.timezone,
            after,
            horizon,
            1,
            MAX_WALK_STEPS,
        )
        .into_iter()
        .next()
    }

    /// Every UTC instant this schedule fires in `[from, to]`, ascending.
    ///
    /// `from` is **inclusive**: the cursor names the next instant to
    /// materialise, so a pass that starts exactly on it must see it.
    pub fn fires_in(
        &self,
        from: NaiveDateTime,
        to: NaiveDateTime,
        cap: usize,
    ) -> Vec<NaiveDateTime> {
        if to < from {
            return Vec::new();
        }
        // The walk is exclusive at its lower bound (it asks the parser for the
        // next match *after* a cursor), so step back the smallest representable
        // amount to make `from` itself eligible.
        let exclusive_from = from - Duration::nanoseconds(1);
        walk(
            &self.schedule,
            self.timezone,
            exclusive_from,
            to,
            cap,
            MAX_WALK_STEPS,
        )
    }
}

/// Walk a schedule from `after` (exclusive) to `until` (inclusive), resolving
/// each matching local calendar time to UTC instants.
///
/// The walk is in **local wall-clock space** and the bounds are **UTC**, which
/// is the whole subtlety. `Tz::from_utc_datetime` is total — every UTC instant
/// has exactly one local reading — so the cursor converts cleanly in that
/// direction; it is the reverse that is partial, and that is where the two DST
/// rules live.
///
/// The result is ascending in UTC even across a transition. Local times are
/// visited in ascending order, and a local time's UTC instant(s) are
/// non-decreasing in the local time, so the sequence cannot go backwards: at a
/// fall-back the ambiguous local time yields its two instants in offset order
/// (`Ambiguous(earliest, latest)`), and the next distinct local time is later
/// than both.
fn walk(
    schedule: &cron::Schedule,
    timezone: Tz,
    after: NaiveDateTime,
    until: NaiveDateTime,
    cap: usize,
    max_steps: usize,
) -> Vec<NaiveDateTime> {
    if cap == 0 || until < after {
        return Vec::new();
    }
    // The local reading of the lower bound. Everything from here on is wall
    // clock until a match is resolved back.
    let local_cursor = timezone.from_utc_datetime(&after).naive_local();
    let mut found = Vec::new();

    // `Utc` as a stand-in for wall clock: no offset, no transitions, so the
    // iterator performs pure calendar arithmetic on the naive fields. Feeding
    // it a real zone instead would make the parser apply its own DST policy,
    // which is exactly the decision this function exists to own.
    let as_wall_clock = Utc.from_utc_datetime(&local_cursor);
    for (step, local) in schedule.after(&as_wall_clock).enumerate() {
        if step >= max_steps {
            tracing::warn!(
                timezone = timezone.name(),
                max_steps,
                "cron walk hit its step bound; truncating this pass"
            );
            break;
        }
        let local = local.naive_utc();
        match timezone.from_local_datetime(&local) {
            // The spring-forward gap: this wall-clock reading never happens, so
            // the schedule does not fire. Keep walking — the next match is
            // usually on the far side of the gap.
            LocalResult::None => continue,
            LocalResult::Single(instant) => {
                let utc = instant.naive_utc();
                if utc > until {
                    break;
                }
                if utc > after {
                    found.push(utc);
                }
            }
            // The fall-back repeat: one wall-clock reading, two instants, an
            // hour apart. Both fire. They are different `scheduled_for` values,
            // so they are different occurrences with different identities and
            // the unique key stays unambiguous.
            LocalResult::Ambiguous(earliest, latest) => {
                let mut past_end = false;
                for instant in [earliest, latest] {
                    let utc = instant.naive_utc();
                    if utc > until {
                        past_end = true;
                        continue;
                    }
                    if utc > after {
                        found.push(utc);
                    }
                }
                if past_end && found.len() >= cap {
                    break;
                }
                if past_end && latest.naive_utc() > until {
                    break;
                }
            }
        }
        if found.len() >= cap {
            break;
        }
    }
    found.truncate(cap);
    found
}

// ============================================================
// Misfire planning
// ============================================================

/// A run of occurrences that will not be materialised individually, recorded as
/// one row.
///
/// One row rather than one per instant, and the count is the point: a
/// per-second schedule down for a day missed 86 400 occurrences, and writing
/// 86 400 rows to say so turns an outage into a second outage. The newest
/// missed instant carries the row so it keeps a stable, unique
/// `(channel_id, scheduled_for)` identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipSummary {
    pub newest: NaiveDateTime,
    pub oldest: NaiveDateTime,
    pub count: u64,
    pub reason: String,
}

/// What one reconciliation pass decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassPlan {
    /// Instants to insert as `pending` occurrences, oldest first.
    pub materialise: Vec<NaiveDateTime>,
    /// The misses recorded as one `skipped_misfire` row, if any.
    pub skipped: Option<SkipSummary>,
    /// Where the cursor moves. `None` when the schedule has no further
    /// occurrence — the reconciler parks rather than re-planning forever.
    pub next_cursor: Option<NaiveDateTime>,
}

impl CronDescriptor {
    /// Plan one reconciliation pass over `[cursor, now]`.
    ///
    /// Pure: same inputs, same plan. Everything durable — the inserts, the
    /// cursor compare-and-set — is the caller's, so this is the part that can
    /// be tested exhaustively without a database.
    ///
    /// `grace` is the normal polling delay, not a misfire. An occurrence due
    /// half a second ago on a one-second poll is simply *late*, and treating it
    /// as a misfire would mean a schedule that never once ran on time still
    /// reported a miss on every pass.
    pub fn plan_pass(
        &self,
        cursor: NaiveDateTime,
        now: NaiveDateTime,
        grace: Duration,
        instance_max_catch_up: u32,
    ) -> PassPlan {
        // The window is bounded by what any policy could possibly keep, plus
        // headroom to count the rest. Without a cap here a multi-week outage on
        // a per-second schedule would enumerate millions of instants to throw
        // nearly all of them away.
        let cap = MAX_WALK_STEPS;
        let due = self.fires_in(cursor, now, cap);
        let next_cursor = self.next_after(now);
        if due.is_empty() {
            return PassPlan {
                materialise: Vec::new(),
                skipped: None,
                next_cursor,
            };
        }

        // Late enough to be a misfire, or merely behind the poll interval.
        let misfire_before = now - grace;
        let split = due.partition_point(|t| *t < misfire_before);
        let (late, fresh) = due.split_at(split);

        let keep = match self.misfire {
            MisfirePolicy::Skip => 0,
            MisfirePolicy::Latest => 1,
            MisfirePolicy::CatchUp => {
                Ord::min(self.max_catch_up, instance_max_catch_up).max(1) as usize
            }
        };

        // `latest` keeps the newest; `catch_up` keeps the oldest, because each
        // occurrence is distinct work and the order it happens in is the order
        // it was scheduled in.
        let (kept, dropped): (Vec<NaiveDateTime>, Vec<NaiveDateTime>) = if keep == 0 {
            (Vec::new(), late.to_vec())
        } else if late.len() <= keep {
            (late.to_vec(), Vec::new())
        } else if self.misfire == MisfirePolicy::Latest {
            let start = late.len() - keep;
            (late[start..].to_vec(), late[..start].to_vec())
        } else {
            (late[..keep].to_vec(), late[keep..].to_vec())
        };

        let skipped = summarise(&dropped, self.misfire);
        let mut materialise = kept;
        materialise.extend_from_slice(fresh);
        materialise.sort_unstable();

        PassPlan {
            materialise,
            skipped,
            next_cursor,
        }
    }
}

fn summarise(dropped: &[NaiveDateTime], policy: MisfirePolicy) -> Option<SkipSummary> {
    let oldest = *dropped.first()?;
    let newest = *dropped.last()?;
    let count = dropped.len() as u64;
    let reason = format!(
        "skipped {count} missed occurrence{} between {} and {} (misfire_policy \"{}\")",
        if count == 1 { "" } else { "s" },
        oldest.and_utc().to_rfc3339(),
        newest.and_utc().to_rfc3339(),
        policy.as_str()
    );
    Some(SkipSummary {
        newest,
        oldest,
        count,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn identity() -> CronIdentity {
        CronIdentity {
            channel_id: "nightly-rollup".to_string(),
            channel_name: "Nightly rollup".to_string(),
            version: 1,
            workflow_id: Some("order-rollup".to_string()),
        }
    }

    fn config(schedule: &str, timezone: Option<&str>) -> CronTransportConfig {
        CronTransportConfig {
            schedule: schedule.to_string(),
            timezone: timezone.map(str::to_string),
            payload: None,
            misfire_policy: MisfirePolicy::Latest,
            max_catch_up: None,
            concurrency: None,
        }
    }

    fn descriptor(schedule: &str, timezone: Option<&str>) -> CronDescriptor {
        config(schedule, timezone)
            .compile(identity())
            .expect("compiles")
    }

    fn utc(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .expect("date")
            .and_hms_opt(hh, mm, ss)
            .expect("time")
    }

    // -- the dialect --

    #[test]
    fn a_six_field_expression_compiles() {
        let d = descriptor("0 15 2 * * *", Some("UTC"));
        assert_eq!(d.timezone, Tz::UTC);
        assert_eq!(d.misfire, MisfirePolicy::Latest);
        assert_eq!(d.concurrency, ConcurrencyPolicy::Allow);
        assert_eq!(d.singleton_key, "nightly-rollup");
    }

    /// The ambiguity the six-field rule exists to remove: five fields is a
    /// perfectly good cron expression somewhere else, and here it would mean
    /// something else entirely.
    #[test]
    fn five_and_seven_field_expressions_are_refused() {
        for expression in ["15 2 * * *", "0 15 2 * * * 2027"] {
            let errors = config(expression, None)
                .compile(identity())
                .expect_err("must refuse");
            assert_eq!(errors.len(), 1, "{expression}: {errors:?}");
            assert_eq!(errors[0].path, "channel.transport_config.schedule");
            assert!(
                errors[0].message.contains("exactly 6 fields"),
                "{}",
                errors[0].message
            );
        }
    }

    #[test]
    fn an_unparseable_expression_is_refused() {
        let errors = config("0 0 25 * * *", None)
            .compile(identity())
            .expect_err("hour 25 does not exist");
        assert_eq!(errors[0].path, "channel.transport_config.schedule");
    }

    /// A syntactically perfect expression for a date that never arrives.
    /// Without the horizon check this activates and never fires.
    #[test]
    fn a_schedule_that_never_fires_is_refused() {
        let errors = config("0 0 0 30 2 *", None)
            .compile(identity())
            .expect_err("February 30th");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("no occurrence in the next")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_zone_abbreviation_is_refused_and_an_iana_name_is_not() {
        let errors = config("0 15 2 * * *", Some("IST"))
            .compile(identity())
            .expect_err("abbreviations are ambiguous");
        assert_eq!(errors[0].path, "channel.transport_config.timezone");
        assert_eq!(
            descriptor("0 15 2 * * *", Some("Asia/Kolkata")).timezone,
            Tz::Asia__Kolkata
        );
    }

    #[test]
    fn an_absent_zone_is_utc() {
        assert_eq!(descriptor("0 15 2 * * *", None).timezone, Tz::UTC);
    }

    /// B1: an author with three problems learns about three problems.
    #[test]
    fn every_problem_is_reported_at_once() {
        let cfg = CronTransportConfig {
            schedule: "nonsense".to_string(),
            timezone: Some("Mars/Olympus".to_string()),
            payload: Some(serde_json::json!([1, 2, 3])),
            misfire_policy: MisfirePolicy::CatchUp,
            max_catch_up: None,
            concurrency: Some(ConcurrencyConfig {
                policy: ConcurrencyPolicy::Forbid,
                key: Some("not a key!".to_string()),
            }),
        };
        let errors = cfg.compile(identity()).expect_err("five problems");
        let fields: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        for expected in [
            "channel.transport_config.schedule",
            "channel.transport_config.timezone",
            "channel.transport_config.payload",
            "channel.transport_config.max_catch_up",
            "channel.transport_config.concurrency.key",
        ] {
            assert!(
                fields.contains(&expected),
                "{expected} missing from {fields:?}"
            );
        }
    }

    #[test]
    fn queue_one_is_refused_until_it_is_implemented() {
        let cfg = CronTransportConfig {
            concurrency: Some(ConcurrencyConfig {
                policy: ConcurrencyPolicy::QueueOne,
                key: None,
            }),
            ..config("0 15 2 * * *", None)
        };
        let errors = cfg.compile(identity()).expect_err("not implemented");
        assert_eq!(
            errors[0].path,
            "channel.transport_config.concurrency.policy"
        );
    }

    #[test]
    fn an_unknown_transport_key_is_refused_by_the_parser() {
        let err = serde_json::from_str::<CronTransportConfig>(
            r#"{"schedule": "0 15 2 * * *", "misfire_polcy": "skip"}"#,
        )
        .expect_err("deny_unknown_fields");
        assert!(err.to_string().contains("misfire_polcy"), "{err}");
    }

    #[test]
    fn a_singleton_key_defaults_to_the_channel_id_and_may_be_shared() {
        assert_eq!(
            descriptor("0 15 2 * * *", None).singleton_key,
            "nightly-rollup"
        );
        let cfg = CronTransportConfig {
            concurrency: Some(ConcurrencyConfig {
                policy: ConcurrencyPolicy::Forbid,
                key: Some("order-pipeline".to_string()),
            }),
            ..config("0 15 2 * * *", None)
        };
        assert_eq!(
            cfg.compile(identity()).expect("compiles").singleton_key,
            "order-pipeline"
        );
    }

    // -- the config hash decides whether a cursor survives --

    #[test]
    fn the_hash_covers_scheduling_fields_and_nothing_else() {
        let base = descriptor("0 15 2 * * *", Some("UTC"));

        // Payload and concurrency are not scheduling: editing what a job does
        // must not silently reset when it next runs.
        let same = CronTransportConfig {
            payload: Some(serde_json::json!({"window": "previous_day"})),
            concurrency: Some(ConcurrencyConfig {
                policy: ConcurrencyPolicy::Forbid,
                key: None,
            }),
            ..config("0 15 2 * * *", Some("UTC"))
        };
        assert_eq!(
            same.compile(identity()).expect("compiles").config_hash,
            base.config_hash
        );

        // The zone is: the same expression in another zone fires at other
        // instants, so the cursor is meaningless across the change.
        assert_ne!(
            descriptor("0 15 2 * * *", Some("Asia/Kolkata")).config_hash,
            base.config_hash
        );
        assert_ne!(
            descriptor("0 30 2 * * *", Some("UTC")).config_hash,
            base.config_hash
        );

        let other_policy = CronTransportConfig {
            misfire_policy: MisfirePolicy::Skip,
            ..config("0 15 2 * * *", Some("UTC"))
        };
        assert_ne!(
            other_policy
                .compile(identity())
                .expect("compiles")
                .config_hash,
            base.config_hash
        );
    }

    // -- wall clock to UTC --

    #[test]
    fn a_daily_schedule_tracks_its_zone_not_utc() {
        // 02:15 in Kolkata is 20:45 UTC the previous day, all year: the zone has
        // no DST, so the offset never moves.
        let d = descriptor("0 15 2 * * *", Some("Asia/Kolkata"));
        let fires = d.fires_in(utc(2026, 9, 1, 0, 0, 0), utc(2026, 9, 3, 0, 0, 0), 10);
        assert_eq!(
            fires,
            vec![utc(2026, 9, 1, 20, 45, 0), utc(2026, 9, 2, 20, 45, 0)]
        );
    }

    /// Spring forward: 01:00 GMT becomes 02:00 BST, so the local hour 01:00–01:59
    /// does not exist on 2026-03-29. A schedule pinned inside it does not fire.
    #[test]
    fn a_nonexistent_local_time_does_not_fire() {
        let d = descriptor("0 30 1 * * *", Some("Europe/London"));
        let fires = d.fires_in(utc(2026, 3, 27, 0, 0, 0), utc(2026, 3, 31, 0, 0, 0), 10);
        assert_eq!(
            fires,
            vec![
                // 01:30 GMT on the 27th and 28th…
                utc(2026, 3, 27, 1, 30, 0),
                utc(2026, 3, 28, 1, 30, 0),
                // …nothing on the 29th: 01:30 never happened…
                // …and 01:30 BST on the 30th, which is 00:30 UTC.
                utc(2026, 3, 30, 0, 30, 0),
            ],
            "the spring-forward gap must be skipped, not shifted"
        );
    }

    /// Fall back: 02:00 BST becomes 01:00 GMT, so the local hour 01:00–01:59
    /// happens twice on 2026-10-25. Both readings are real instants an hour
    /// apart, so a schedule pinned inside it fires twice.
    #[test]
    fn a_repeated_local_time_fires_twice() {
        let d = descriptor("0 30 1 * * *", Some("Europe/London"));
        let fires = d.fires_in(utc(2026, 10, 25, 0, 0, 0), utc(2026, 10, 25, 6, 0, 0), 10);
        assert_eq!(
            fires,
            vec![
                utc(2026, 10, 25, 0, 30, 0), // 01:30 BST
                utc(2026, 10, 25, 1, 30, 0), // 01:30 GMT, one hour later
            ],
            "both readings of an ambiguous local time are distinct instants"
        );
    }

    /// The same rules in the other hemisphere's direction, so the behaviour is
    /// not an accident of one zone's transition sign.
    #[test]
    fn the_dst_rules_hold_in_new_york_too() {
        let d = descriptor("0 30 2 * * *", Some("America/New_York"));
        // 2026-03-08: 02:00 EST becomes 03:00 EDT — 02:30 does not exist.
        let spring = d.fires_in(utc(2026, 3, 8, 0, 0, 0), utc(2026, 3, 9, 0, 0, 0), 10);
        assert!(
            spring.is_empty(),
            "02:30 does not exist on a spring-forward day: {spring:?}"
        );
        // 2026-11-01: the repeat is 01:00–01:59, so 02:30 is unambiguous.
        let fall = d.fires_in(utc(2026, 11, 1, 0, 0, 0), utc(2026, 11, 2, 0, 0, 0), 10);
        assert_eq!(fall, vec![utc(2026, 11, 1, 7, 30, 0)]);
    }

    #[test]
    fn results_are_ascending_across_a_transition() {
        let d = descriptor("0 0 * * * *", Some("Europe/London"));
        let fires = d.fires_in(utc(2026, 10, 24, 22, 0, 0), utc(2026, 10, 25, 6, 0, 0), 50);
        let mut sorted = fires.clone();
        sorted.sort_unstable();
        assert_eq!(fires, sorted, "UTC instants must come out ascending");
        assert_eq!(
            fires.len(),
            sorted
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            "no instant may be reported twice"
        );
    }

    #[test]
    fn a_per_second_schedule_enumerates_without_drift() {
        let d = descriptor("* * * * * *", Some("UTC"));
        let fires = d.fires_in(utc(2026, 9, 1, 0, 0, 1), utc(2026, 9, 1, 0, 0, 5), 100);
        assert_eq!(
            fires,
            vec![
                utc(2026, 9, 1, 0, 0, 1),
                utc(2026, 9, 1, 0, 0, 2),
                utc(2026, 9, 1, 0, 0, 3),
                utc(2026, 9, 1, 0, 0, 4),
                utc(2026, 9, 1, 0, 0, 5),
            ],
            "`from` is inclusive and every second in the window is present"
        );
    }

    #[test]
    fn the_window_is_inclusive_at_both_ends_and_the_cap_is_honoured() {
        let d = descriptor("* * * * * *", Some("UTC"));
        assert_eq!(
            d.fires_in(utc(2026, 9, 1, 0, 0, 0), utc(2026, 9, 1, 0, 0, 0), 10),
            vec![utc(2026, 9, 1, 0, 0, 0)]
        );
        assert_eq!(
            d.fires_in(utc(2026, 9, 1, 0, 0, 0), utc(2026, 9, 1, 1, 0, 0), 3)
                .len(),
            3
        );
        assert!(
            d.fires_in(utc(2026, 9, 1, 0, 0, 5), utc(2026, 9, 1, 0, 0, 4), 10)
                .is_empty(),
            "an inverted window yields nothing"
        );
    }

    #[test]
    fn next_after_is_strict() {
        let d = descriptor("0 15 2 * * *", Some("UTC"));
        let on_the_dot = utc(2026, 9, 1, 2, 15, 0);
        assert_eq!(d.next_after(on_the_dot), Some(utc(2026, 9, 2, 2, 15, 0)));
        assert_eq!(
            d.next_after(on_the_dot - Duration::seconds(1)),
            Some(on_the_dot)
        );
    }

    // -- misfire planning --

    fn plan(
        policy: MisfirePolicy,
        max_catch_up: Option<u32>,
        cursor: NaiveDateTime,
        now: NaiveDateTime,
    ) -> PassPlan {
        let cfg = CronTransportConfig {
            misfire_policy: policy,
            max_catch_up,
            ..config("0 0 * * * *", Some("UTC")) // hourly, on the hour
        };
        cfg.compile(identity())
            .expect("compiles")
            .plan_pass(cursor, now, Duration::seconds(5), 100)
    }

    /// A pass that is merely behind its poll interval is not a misfire. A
    /// schedule that never once ran in the same second it was due would
    /// otherwise report a miss every time.
    #[test]
    fn work_inside_the_grace_window_is_late_not_misfired() {
        let due = utc(2026, 9, 1, 3, 0, 0);
        let p = plan(
            MisfirePolicy::Skip,
            None,
            due,
            due + Duration::milliseconds(1200),
        );
        assert_eq!(p.materialise, vec![due], "still runs under `skip`");
        assert_eq!(p.skipped, None);
        assert_eq!(p.next_cursor, Some(utc(2026, 9, 1, 4, 0, 0)));
    }

    #[test]
    fn skip_runs_nothing_and_records_one_summary_row() {
        // Down from 00:00 to 05:30: five missed hourly occurrences.
        let p = plan(
            MisfirePolicy::Skip,
            None,
            utc(2026, 9, 1, 0, 0, 0),
            utc(2026, 9, 1, 5, 30, 0),
        );
        assert!(p.materialise.is_empty());
        let skipped = p.skipped.expect("a summary");
        assert_eq!(skipped.count, 6);
        assert_eq!(skipped.oldest, utc(2026, 9, 1, 0, 0, 0));
        assert_eq!(skipped.newest, utc(2026, 9, 1, 5, 0, 0));
        assert!(skipped.reason.contains("skipped 6 missed occurrences"));
    }

    #[test]
    fn latest_runs_the_newest_and_summarises_the_rest() {
        let p = plan(
            MisfirePolicy::Latest,
            None,
            utc(2026, 9, 1, 0, 0, 0),
            utc(2026, 9, 1, 5, 30, 0),
        );
        assert_eq!(p.materialise, vec![utc(2026, 9, 1, 5, 0, 0)]);
        let skipped = p.skipped.expect("a summary");
        assert_eq!(skipped.count, 5);
        assert_eq!(skipped.newest, utc(2026, 9, 1, 4, 0, 0));
    }

    #[test]
    fn catch_up_runs_oldest_first_and_is_bounded() {
        let p = plan(
            MisfirePolicy::CatchUp,
            Some(3),
            utc(2026, 9, 1, 0, 0, 0),
            utc(2026, 9, 1, 5, 30, 0),
        );
        assert_eq!(
            p.materialise,
            vec![
                utc(2026, 9, 1, 0, 0, 0),
                utc(2026, 9, 1, 1, 0, 0),
                utc(2026, 9, 1, 2, 0, 0),
            ],
            "oldest first, because each occurrence is distinct work"
        );
        assert_eq!(p.skipped.expect("a summary").count, 3);
    }

    /// The instance's budget is a ceiling on the channel's ask, not a default
    /// it can raise.
    #[test]
    fn the_instance_budget_clamps_the_channels_catch_up() {
        let cfg = CronTransportConfig {
            misfire_policy: MisfirePolicy::CatchUp,
            max_catch_up: Some(500),
            ..config("0 0 * * * *", Some("UTC"))
        };
        let p = cfg.compile(identity()).expect("compiles").plan_pass(
            utc(2026, 9, 1, 0, 0, 0),
            utc(2026, 9, 1, 20, 30, 0),
            Duration::seconds(5),
            2,
        );
        assert_eq!(p.materialise.len(), 2);
    }

    #[test]
    fn a_pass_with_nothing_due_plans_nothing_and_still_advances() {
        let p = plan(
            MisfirePolicy::Latest,
            None,
            utc(2026, 9, 1, 4, 0, 0),
            utc(2026, 9, 1, 3, 30, 0),
        );
        assert!(p.materialise.is_empty());
        assert_eq!(p.skipped, None);
        assert_eq!(p.next_cursor, Some(utc(2026, 9, 1, 4, 0, 0)));
    }

    /// The cursor is inclusive, so the instant it names is materialised exactly
    /// once and the next pass starts past it.
    #[test]
    fn the_cursor_never_replays_an_instant() {
        let cursor = utc(2026, 9, 1, 3, 0, 0);
        let first = plan(
            MisfirePolicy::Latest,
            None,
            cursor,
            cursor + Duration::seconds(1),
        );
        assert_eq!(first.materialise, vec![cursor]);
        let next = first.next_cursor.expect("advances");
        let second = plan(
            MisfirePolicy::Latest,
            None,
            next,
            cursor + Duration::seconds(2),
        );
        assert!(second.materialise.is_empty());
    }
}
