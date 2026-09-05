//! The metadata a scheduled run carries.
//!
//! Every ingress builds this differently and every ingress obeys the same two
//! rules: engine-owned keys are cleared, and `metadata.vars` is stamped from
//! the instance's declared values rather than inherited. A cron occurrence has
//! no caller to inherit anything *from*, which makes the rules trivial here —
//! but it also makes this the one place a workflow can learn *why* it is
//! running, so the `trigger` object is the whole point of the module.

use chrono::NaiveDateTime;
use serde_json::{Value, json};

/// The reserved metadata key describing what started this run.
///
/// Declared in [`crate::engine`] with the rest of the reserved namespace, not
/// here: the offline metadata validator has to know the name, and it sits a
/// layer below this module. Re-exported so the builder below reads naturally.
///
/// Platform-owned, in the same sense as `channel` and `vars`: it is stamped by
/// the runtime, never supplied. A workflow reads `metadata.trigger.scheduled_for`
/// to know *which* occurrence it is — the instant the work is for, as distinct
/// from the instant it happens to be running.
pub use crate::engine::TRIGGER_KEY;

/// Everything a workflow can learn about its own occurrence.
pub struct TriggerFacts<'a> {
    pub channel_name: &'a str,
    /// `cron` or `manual`.
    pub trigger_type: &'a str,
    pub occurrence_id: &'a str,
    /// The instant this occurrence was *due*, in UTC. Immutable across retries,
    /// which is what makes it usable as an idempotency key: two attempts at the
    /// same occurrence agree on it, and no two occurrences of one channel share
    /// it.
    pub scheduled_for: NaiveDateTime,
    /// When this attempt actually began. Differs from `scheduled_for` by the
    /// polling delay, or by however long the scheduler was down.
    pub started_at: NaiveDateTime,
    /// The channel's IANA zone, so a workflow formatting a local date does not
    /// have to hard-code the one the schedule was written in.
    pub timezone: &'a str,
    /// 1 for a first run. A workflow that must not repeat a side effect can
    /// branch on this, though an idempotent destination is the better answer.
    pub attempt: i64,
    /// The lock this run holds, when its channel takes one.
    pub singleton_key: Option<&'a str>,
}

/// Build the message metadata for one attempt.
///
/// The `vars` argument follows [`crate::engine::stamp_vars`]'s contract: `None`
/// *removes* the key rather than writing an empty object, so a workflow reading
/// `metadata.vars.x` sees the same missing value whichever ingress reached it.
pub fn occurrence_metadata(facts: TriggerFacts<'_>, vars: Option<&Value>) -> Value {
    let mut metadata = json!({
        "channel": facts.channel_name,
        TRIGGER_KEY: {
            "type": facts.trigger_type,
            "occurrence_id": facts.occurrence_id,
            "scheduled_for": facts.scheduled_for.and_utc().to_rfc3339(),
            "started_at": facts.started_at.and_utc().to_rfc3339(),
            "timezone": facts.timezone,
            "attempt": facts.attempt,
        },
    });
    if let Some(key) = facts.singleton_key {
        metadata[TRIGGER_KEY]["singleton_key"] = json!(key);
    }
    // Nothing here was ever supplied by a caller, so there is no inherited
    // error context to clear — but the call stays, because "every ingress
    // clears it" is the invariant, and an ingress that skips it because its
    // *current* inputs happen to be clean is one refactor away from not being.
    crate::engine::clear_error_context(&mut metadata);
    crate::engine::stamp_vars(&mut metadata, vars);
    metadata
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(hh: u32, mm: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 9, 5)
            .expect("date")
            .and_hms_opt(hh, mm, 0)
            .expect("time")
    }

    fn facts() -> TriggerFacts<'static> {
        TriggerFacts {
            channel_name: "nightly-rollup",
            trigger_type: "cron",
            occurrence_id: "occ-1",
            scheduled_for: at(2, 15),
            started_at: at(2, 15),
            timezone: "Asia/Kolkata",
            attempt: 1,
            singleton_key: None,
        }
    }

    #[test]
    fn the_trigger_object_carries_the_occurrences_identity() {
        let metadata = occurrence_metadata(facts(), None);
        assert_eq!(metadata["channel"], "nightly-rollup");
        assert_eq!(metadata["trigger"]["type"], "cron");
        assert_eq!(metadata["trigger"]["occurrence_id"], "occ-1");
        assert_eq!(
            metadata["trigger"]["scheduled_for"],
            "2026-09-05T02:15:00+00:00"
        );
        assert_eq!(metadata["trigger"]["timezone"], "Asia/Kolkata");
        assert_eq!(metadata["trigger"]["attempt"], 1);
        assert!(metadata["trigger"].get("singleton_key").is_none());
    }

    /// The distinction the whole object exists for: what the work is *for*
    /// versus when it happened to run.
    #[test]
    fn a_late_run_reports_both_instants() {
        let metadata = occurrence_metadata(
            TriggerFacts {
                started_at: at(6, 30),
                attempt: 3,
                singleton_key: Some("order-pipeline"),
                ..facts()
            },
            None,
        );
        assert_eq!(
            metadata["trigger"]["scheduled_for"],
            "2026-09-05T02:15:00+00:00"
        );
        assert_eq!(
            metadata["trigger"]["started_at"],
            "2026-09-05T06:30:00+00:00"
        );
        assert_eq!(metadata["trigger"]["attempt"], 3);
        assert_eq!(metadata["trigger"]["singleton_key"], "order-pipeline");
    }

    #[test]
    fn vars_are_stamped_the_way_every_other_ingress_stamps_them() {
        let vars = json!({"region": "eu-west-1"});
        let metadata = occurrence_metadata(facts(), Some(&vars));
        assert_eq!(metadata["vars"]["region"], "eu-west-1");

        // And absent — not empty — when the instance declares none, so a
        // workflow sees the same missing value on every transport.
        assert!(occurrence_metadata(facts(), None).get("vars").is_none());
    }
}
