//! Host-owned cumulative run budget and occupancy accounting for Dataflow v4
//! runs (`OR-01`, `UP-03`, `V2-05.1`).
//!
//! `dataflow-rs` 4.0.1 separates per-expression operation ceilings
//! (`ExprEngineBuilder::ops_budget`) from run-wide cumulative accounting
//! ([`RunAccountant`]). [`SharedRunAccountant`] is Orion's thread-safe
//! host ledger that can be attached to a root workflow run via
//! [`RunOptions::accountant`] and automatically inherited by nested
//! `channel_call` executions via `ctx.accountant()`, ensuring:
//!
//! - Total task dispatches (built-ins, host functions, loop steps, and fan-out
//!   branches) share one monotonic counter and optional ceiling without
//!   resetting across child invocations.
//! - Total expression operations (including failed expressions metered via
//!   `datalogic-rs` 5.9.0's `evaluate_metered_report`) accumulate into one
//!   shared counter and optional ceiling.
//! - Custom host/connector charges (`charge_custom`) share a cumulative budget.
//! - Concurrent task occupancy is tracked and bounded via RAII
//!   [`OccupancyGuard`]s that release capacity even on error or cancellation.

use std::sync::atomic::{AtomicU64, Ordering};

use dataflow_rs::Error;
use dataflow_rs::run::{DispatchKind, OccupancyGuard, RunAccountant, RunOptions};

pub use orion_common::config::RunBudgetLimits;

/// Point-in-time usage snapshot read from a [`SharedRunAccountant`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunBudgetSnapshot {
    pub tasks_dispatched: u64,
    pub expr_ops_charged: u64,
    pub custom_units_charged: u64,
    pub in_flight: u64,
    pub peak_in_flight: u64,
}

/// Thread-safe, host-owned [`RunAccountant`] shared across a root run and its
/// nested child runs.
#[derive(Debug)]
pub struct SharedRunAccountant {
    limits: RunBudgetLimits,
    tasks_dispatched: AtomicU64,
    expr_ops_charged: AtomicU64,
    custom_units_charged: AtomicU64,
    in_flight: AtomicU64,
    peak_in_flight: AtomicU64,
}

impl SharedRunAccountant {
    /// Create a new shared accountant with the given ceilings.
    #[inline]
    pub fn new(limits: RunBudgetLimits) -> Self {
        Self {
            limits,
            tasks_dispatched: AtomicU64::new(0),
            expr_ops_charged: AtomicU64::new(0),
            custom_units_charged: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            peak_in_flight: AtomicU64::new(0),
        }
    }

    /// Create an unbounded accountant that records usage without rejecting.
    #[inline]
    pub fn unbounded() -> Self {
        Self::new(RunBudgetLimits::default())
    }

    /// Configured limits for this accountant.
    #[inline]
    pub fn limits(&self) -> RunBudgetLimits {
        self.limits
    }

    /// Read a point-in-time snapshot of cumulative and in-flight usage.
    #[inline]
    pub fn snapshot(&self) -> RunBudgetSnapshot {
        RunBudgetSnapshot {
            tasks_dispatched: self.tasks_dispatched.load(Ordering::Acquire),
            expr_ops_charged: self.expr_ops_charged.load(Ordering::Acquire),
            custom_units_charged: self.custom_units_charged.load(Ordering::Acquire),
            in_flight: self.in_flight.load(Ordering::Acquire),
            peak_in_flight: self.peak_in_flight.load(Ordering::Acquire),
        }
    }

    /// Attach this accountant to `options`.
    #[inline]
    pub fn apply<'a>(&'a self, options: RunOptions<'a>) -> RunOptions<'a> {
        options.accountant(self)
    }
}

impl RunAccountant for SharedRunAccountant {
    fn before_task_dispatch(
        &self,
        _kind: DispatchKind<'_>,
    ) -> dataflow_rs::Result<OccupancyGuard<'_>> {
        // Check cumulative expression ops first (since `on_expr_evaluated` is
        // observational, a prior expression that crossed `max_expr_ops` halts
        // before the next task dispatches).
        if let Some(limit) = self.limits.max_expr_ops {
            let spent = self.expr_ops_charged.load(Ordering::Acquire);
            if spent > limit {
                return Err(Error::budget(format!(
                    "run expression operations budget exceeded ({spent} > {limit} max ops)"
                )));
            }
        }

        let tasks = self
            .tasks_dispatched
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        if let Some(limit) = self.limits.max_tasks
            && tasks > limit
        {
            return Err(Error::budget(format!(
                "run task budget exceeded ({tasks} > {limit} max tasks)"
            )));
        }

        let current_in_flight = self
            .in_flight
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        self.peak_in_flight
            .fetch_max(current_in_flight, Ordering::AcqRel);

        if let Some(limit) = self.limits.max_in_flight
            && current_in_flight > limit
        {
            self.in_flight.fetch_sub(1, Ordering::Release);
            return Err(Error::budget(format!(
                "run in-flight occupancy budget exceeded ({current_in_flight} > {limit})"
            )));
        }

        Ok(OccupancyGuard::new(|| {
            self.in_flight.fetch_sub(1, Ordering::Release);
        }))
    }

    fn on_expr_evaluated(&self, ops: u64) {
        self.expr_ops_charged.fetch_add(ops, Ordering::AcqRel);
    }

    fn charge_custom(&self, kind: &str, units: u64) -> dataflow_rs::Result<()> {
        let spent = self
            .custom_units_charged
            .fetch_add(units, Ordering::AcqRel)
            .saturating_add(units);
        if let Some(limit) = self.limits.max_custom_units
            && spent > limit
        {
            return Err(Error::budget(format!(
                "run custom budget '{kind}' exceeded ({spent} > {limit})"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dataflow_rs::datavalue::OwnedDataValue;
    use dataflow_rs::run::{ConditionErrorPolicy, EngineConfig, RunStatus, SiteKind};
    use dataflow_rs::{Ctx, Engine, Function, Message, Outcome, Registry};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    struct CustomCharger {
        units: u64,
        calls: Arc<AtomicUsize>,
    }

    impl Function for CustomCharger {
        type Config = ();

        async fn call(&self, ctx: &mut Ctx<'_>, _: &()) -> dataflow_rs::Result<Outcome> {
            if let Some(acct) = ctx.accountant() {
                acct.charge_custom("provider_io", self.units)?;
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome::Ok)
        }
    }

    #[tokio::test]
    async fn shared_accountant_tracks_and_enforces_task_expr_and_custom_budgets() {
        let calls = Arc::new(AtomicUsize::new(0));
        let engine = Engine::builder()
            .register(
                "charge",
                CustomCharger {
                    units: 3,
                    calls: calls.clone(),
                },
            )
            .workflow_json(&json!({
                "id": "w",
                "tasks": [
                    {"id": "t1", "function": {"name": "map", "input": {
                        "mappings": [{"path": "data.a", "logic": {"+": [{"var": "data.x"}, 1]}}]
                    }}},
                    {"id": "t2", "function": {"name": "charge"}},
                    {"id": "t3", "function": {"name": "charge"}}
                ]
            }))
            .build()
            .expect("build test engine");

        let acct = SharedRunAccountant::new(RunBudgetLimits {
            max_tasks: Some(10),
            max_expr_ops: Some(100),
            max_custom_units: Some(5),
            max_in_flight: Some(1),
        });
        let mut msg = Message::from_parts(
            OwnedDataValue::Null,
            OwnedDataValue::from(&json!({"x": 41})),
            OwnedDataValue::Null,
        );
        let opts = acct.apply(RunOptions::default());
        let run = engine.run_with(&mut msg, &opts).await;

        // Second `charge` task crosses `max_custom_units = 5` (3 + 3 = 6 > 5).
        assert!(matches!(run.status(), RunStatus::Stopped(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let snap = acct.snapshot();
        assert_eq!(snap.tasks_dispatched, 3);
        assert!(snap.expr_ops_charged >= 2);
        assert_eq!(snap.custom_units_charged, 6);
        assert_eq!(snap.in_flight, 0, "OccupancyGuard must release on drop");
        assert_eq!(snap.peak_in_flight, 1);
    }

    #[tokio::test]
    async fn condition_error_policy_stop_halts_before_later_effects_with_site_and_cause() {
        let calls = Arc::new(AtomicUsize::new(0));
        let exprs = crate::operators::orion_exprs(&crate::ResolvedSecrets::empty(), 4)
            .expect("orion_exprs builds");
        let mut registry = Registry::new(exprs);
        registry.register(
            "effect",
            CustomCharger {
                units: 1,
                calls: calls.clone(),
            },
        );
        let engine = Engine::builder()
            .config(
                EngineConfig::default()
                    .error_context(crate::ERROR_CONTEXT.clone())
                    .with_condition_error_policy(ConditionErrorPolicy::Stop),
            )
            .registry(registry)
            .workflow_json(&json!({
                "id": "w",
                "tasks": [
                    {
                        "id": "guard",
                        "on_failure": "continue",
                        "function": {
                            "name": "filter",
                            "input": {
                                "condition": {"/": [1, {"var": "data.zero"}]},
                                "on_reject": "skip"
                            }
                        }
                    },
                    {"id": "after", "function": {"name": "effect"}}
                ]
            }))
            .build()
            .expect("build test engine");

        let acct = SharedRunAccountant::unbounded();
        let mut msg = Message::from_parts(
            OwnedDataValue::Null,
            OwnedDataValue::from(&json!({"zero": 0})),
            OwnedDataValue::Null,
        );
        let run = engine
            .run_with(&mut msg, &acct.apply(RunOptions::default()))
            .await;

        assert_eq!(calls.load(Ordering::SeqCst), 0, "later effect must not run");
        assert!(
            matches!(run.status(), RunStatus::Stopped(_)),
            "expected Stopped, got {:?}",
            run.status()
        );
        if let RunStatus::Stopped(err) = run.status() {
            let site = err.site().expect("ErrorSite attached");
            assert_eq!(site.kind, SiteKind::FilterCondition);
            assert_eq!(site.workflow_id.as_ref(), "w");
            assert_eq!(site.task_id.as_deref(), Some("guard"));
            let cause = err.expr_cause().expect("ExprCause attached");
            assert_eq!(cause.operator.as_deref(), Some("/"));
        }
        assert!(
            acct.snapshot().expr_ops_charged > 0,
            "failed condition evaluation ops are metered via evaluate_metered_report"
        );
    }
}
