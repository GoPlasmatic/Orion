//! The connector-handler shape, as a type rather than a convention.
//!
//! Every connector handler does the same five things in the same order:
//! read the literal prologue, resolve the message-dependent fields, look up and
//! type-check the connector, check its operation gates, and run the call inside
//! the observability + circuit-breaker shell. That order is not incidental —
//! each step of it was a defect once:
//!
//! * **F6/F40** — the shell is the only thing emitting
//!   `connector_requests_total` and the only place the circuit breaker is
//!   applied, so a handler outside it is invisible and unprotected.
//! * **F58** — the literal prologue must precede message-dependent resolution,
//!   or a task missing both `connector` and `key` reports the `key` error, the
//!   author fixes that, re-runs, and only then learns about `connector`.
//! * **F48** — a handler names itself once, in one place, because that name
//!   reaches metric labels, profile samples and every error message.
//!
//! All three were enforced by tests that read handler `.rs` files as strings
//! and asserted on byte offsets. Each of those tests explains, carefully, that
//! it is checking a property a type could hold. This is that type: implement
//! [`ConnectorHandler`], and the order is the wrapper's to get right.
//!
//! **Why a wrapper and not a blanket impl.** The natural shape —
//! `impl<T: ConnectorHandler> AsyncFunctionHandler for T` — is rejected by the
//! orphan rule (E0210): `AsyncFunctionHandler` is dataflow-rs's, and a bare
//! type parameter is uncovered. [`Connector<H>`] is the local type that makes
//! the impl legal, and registration wraps each handler in it.

use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::connector_helpers::{ConnectorCall, apply_output, require_connector};
use crate::connector::{ConnectorKind, ConnectorRegistry};
use crate::engine::HandlerError;

/// One connector handler's own logic, with the shell factored out.
///
/// The associated [`Kind`](Self::Kind) is what makes the typed config arrive
/// already checked: a handler asks for the connector type it speaks, and
/// `require_connector` produces the message when a workflow points it at
/// another. It cannot bind the wrong variant, because it never names one.
#[async_trait]
pub trait ConnectorHandler: Send + Sync + 'static {
    /// This handler's name in metrics, profiles and error messages.
    ///
    /// An associated const, so it is written once and cannot be repeated —
    /// which is what `a_handler_names_itself_exactly_once` was asserting by
    /// counting string literals in the source.
    const NAME: &'static str;

    /// The connector type this handler speaks.
    type Kind: ConnectorKind;

    /// Everything read from the task input and the message before the
    /// connector is looked up.
    type Parsed: Send;

    /// The registry the connector is resolved through.
    fn registry(&self) -> &Arc<ConnectorRegistry>;

    /// Read the input, resolving anything message-dependent.
    ///
    /// Runs with `&TaskContext` — before the body takes it mutably — and
    /// **after** [`ConnectorCall::begin`] has checked that a connector was
    /// named at all. F58's ordering is structural here: there is no way to
    /// express "resolve `key` first", because `call` is already built.
    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &Value,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, HandlerError>;

    /// Check the connector's operation gates.
    ///
    /// Separate from [`run`](Self::run) because a gate refusal is the
    /// connector's answer, not the backend's: it must happen before anything is
    /// dialled. Default is "no gate of its own" — a connector type whose
    /// `operations` vocabulary is empty.
    fn gate(
        _conn: &<Self::Kind as ConnectorKind>::Config,
        _connector: &str,
    ) -> Result<(), HandlerError> {
        Ok(())
    }

    /// The call itself, against a resolved, type-checked, gated connector.
    ///
    /// This is the seam a fake backend plugs into. Everything above it is the
    /// wrapper's; everything below is the connector's.
    async fn run(
        &self,
        parsed: Self::Parsed,
        conn: &<Self::Kind as ConnectorKind>::Config,
        call: &ConnectorCall<'_>,
        ctx: &mut TaskContext<'_>,
    ) -> Result<Value, HandlerError>;
}

/// The shell, applied to a [`ConnectorHandler`].
///
/// Registration wraps each handler in this, and that is the whole enforcement:
/// a handler that is not wrapped is not an `AsyncFunctionHandler` and cannot be
/// registered at all — where previously it registered fine and a test noticed.
pub struct Connector<H>(pub H);

#[async_trait]
impl<H: ConnectorHandler> AsyncFunctionHandler for Connector<H> {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // The literal prologue: a task naming no connector says so before
        // anything about the message is consulted (F58).
        let call = ConnectorCall::begin(H::NAME, input, ctx)?;

        // Message-dependent resolution, still with `&ctx`.
        let parsed = self.0.parse(&call, input, ctx).map_err(|e| {
            let e: dataflow_rs::DataflowError = e.into();
            e
        })?;

        let registry = self.0.registry();
        call.run(registry, async {
            let config = call.resolve(registry, None).await?;
            let conn = require_connector::<H::Kind>(&config, call.connector)?;
            H::gate(conn, call.connector).map_err(dataflow_rs::DataflowError::from)?;

            let value = self
                .0
                .run(parsed, conn, &call, ctx)
                .await
                .map_err(dataflow_rs::DataflowError::from)?;

            apply_output(ctx, call.output, value);
            Ok(TaskOutcome::Success)
        })
        .await
    }
}
