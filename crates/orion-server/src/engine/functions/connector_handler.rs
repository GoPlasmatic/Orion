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
//! All three were once enforced by tests that read handler `.rs` files as
//! strings and asserted on byte offsets, over a hand-kept list of handler
//! names. Each of those tests explained, carefully, that it was checking a
//! property a type could hold. This is that type, and they are gone: an
//! unwrapped handler is not an `AsyncFunctionHandler` and cannot be registered,
//! `parse` takes the `call` as an argument so it cannot run before the prologue
//! that built it, and `NAME` is an associated const with no second copy to
//! disagree with.
//!
//! One handler does not fit the five-step order and says so where the missing
//! step would be: `data_write`'s gate is the operation its envelope resolves
//! to, and resolving the envelope needs the connector's own schema guards, so
//! the answer does not exist until after the connector is in hand. It gates
//! inside `run`, before any pool is touched.
//!
//! **Why a wrapper and not a blanket impl.** The natural shape —
//! `impl<T: ConnectorHandler> AsyncFunctionHandler for T` — is rejected by the
//! orphan rule (E0210): `AsyncFunctionHandler` is dataflow-rs's, and a bare
//! type parameter is uncovered. [`Connector<H>`] is the local type that makes
//! the impl legal, and registration wraps each handler in it.

use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::TemplateCompiler;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::connector_helpers::{
    ConnectorCall, apply_output, require_connector, require_str_field, resolve_output_path,
};
use super::templated_input::TemplatedInput;
use crate::connector::{ConnectorRegistry, ConnectorTarget};
use crate::engine::HandlerError;

/// The literal prologue, read from a handler's own input type.
///
/// Two of the fourteen connector handlers do not take freeform JSON:
/// `http_call` and `publish_kafka` are dataflow-rs's own typed configs, and
/// they used to be the two that called `guarded_handler` by hand precisely
/// because [`ConnectorCall::begin`] could only read a `Value`. The prologue is
/// the same question in both cases — *which connector, and where does the
/// answer go* — so it is asked through a trait rather than through a shape.
///
/// Implementations must read **only literal keys**. Anything message-dependent
/// belongs in [`ConnectorHandler::parse`], which runs after this (F58).
pub trait ConnectorInput: DeserializeOwned + Send + Sync + 'static {
    /// The connector this task targets. `handler` is the name that appears in
    /// the "requires 'connector'" message when there is none.
    fn connector(&self, handler: &'static str) -> Result<&str, DataflowError>;

    /// Where the handler's result is written, defaulting to `data`.
    ///
    /// Takes the context because a destination is JSONLogic like every other
    /// parameter: `{"output": {"cat": ["data.by_tenant.", {"var": …}]}}` fans
    /// one task's results out by message content. A statically authored path
    /// folds at engine build and costs nothing here.
    ///
    /// # Errors
    ///
    /// [`DataflowError`] when the expression fails to evaluate.
    fn output(&self, handler: &'static str, ctx: &TaskContext<'_>)
    -> Result<String, DataflowError>;

    /// Compile this input's expression fields, once at engine build.
    ///
    /// On the trait rather than on each handler's `compile_input` because
    /// [`TemplatedInput`] needs the handler's *name* to know which fields its
    /// table declares, and the wrapper is the only thing that has both the
    /// input and `H::NAME`. A handler cannot forget it: the wrapper calls it
    /// for every input it parses.
    ///
    /// # Errors
    ///
    /// [`DataflowError`] when a declared expression field does not compile.
    fn compile(
        &mut self,
        _handler: &'static str,
        _c: &TemplateCompiler,
    ) -> dataflow_rs::Result<()> {
        Ok(())
    }
}

impl ConnectorInput for TemplatedInput {
    fn connector(&self, handler: &'static str) -> Result<&str, DataflowError> {
        require_str_field(self.raw(), "connector", handler)
    }

    fn output(
        &self,
        handler: &'static str,
        ctx: &TaskContext<'_>,
    ) -> Result<String, DataflowError> {
        resolve_output_path(self, handler, ctx)
    }

    fn compile(&mut self, handler: &'static str, c: &TemplateCompiler) -> dataflow_rs::Result<()> {
        TemplatedInput::compile(self, handler, c)
    }
}

impl ConnectorInput for dataflow_rs::engine::functions::HttpCallConfig {
    fn connector(&self, handler: &'static str) -> Result<&str, DataflowError> {
        // Typed: serde already refused a task without it, so the only refusal
        // left is the computed spelling dataflow-rs 3.9 made expressible.
        literal_connector(&self.connector, handler)
    }

    fn output(
        &self,
        _handler: &'static str,
        ctx: &TaskContext<'_>,
    ) -> Result<String, DataflowError> {
        // `response_path` is optional — omitting it discards the body — so the
        // default here is only ever consulted for a call that records nothing,
        // and the handler returns `Produced::nothing()` for those.
        Ok(self
            .resolve_response_path(ctx)?
            .unwrap_or_else(|| "data".to_string()))
    }
}

impl ConnectorInput for dataflow_rs::engine::functions::PublishKafkaConfig {
    fn connector(&self, handler: &'static str) -> Result<&str, DataflowError> {
        literal_connector(&self.connector, handler)
    }

    fn output(
        &self,
        _handler: &'static str,
        _ctx: &TaskContext<'_>,
    ) -> Result<String, DataflowError> {
        // A publish records nothing; this is never read.
        Ok("data".to_string())
    }
}

/// The literal spelling of a `Template` that names a connector.
///
/// dataflow-rs 3.9 made every built-in parameter JSONLogic, so `connector` can
/// now be an expression. Orion does not resolve one yet, and the refusal is
/// explicit rather than silent: the connector name is read in the literal
/// prologue, *before* the message is consulted (F58), and it is also what
/// `Workflow::connector_refs` reports to the dependency endpoint, the package
/// linter and the pool pre-warmer. Admitting a computed name means teaching all
/// four, not just this line.
fn literal_connector<'t>(
    template: &'t dataflow_rs::Template,
    handler: &'static str,
) -> Result<&'t str, DataflowError> {
    template.as_json().as_str().ok_or_else(|| {
        DataflowError::Validation(format!(
            "{handler} 'connector' must be a literal connector name — a computed connector              is not resolvable before the connector is looked up"
        ))
    })
}

/// What a handler's call produced.
///
/// Two things vary and both used to be expressed by each handler writing its
/// own tail: whether there is a value to record at `output`, and whether the
/// task succeeded outright. `cache_write` and `publish_kafka` are the calls
/// whose effect *is* the call, and they declare no `output` field; a partially
/// applied bulk write is the one non-`Success` outcome any connector handler
/// produces (F28 — 207, with applied/failed/never-attempted named).
///
/// `From<Value>` keeps the ordinary case one word, and the two exceptions have
/// to say so explicitly rather than inherit a default.
pub struct Produced {
    /// The value to record at the task's `output` path, or `None` when the
    /// call has no result to record.
    pub value: Option<Value>,
    /// The task's outcome. `Success` for everything but a partial bulk write.
    pub outcome: TaskOutcome,
}

impl From<Value> for Produced {
    fn from(value: Value) -> Self {
        Self {
            value: Some(value),
            outcome: TaskOutcome::Success,
        }
    }
}

impl Produced {
    /// A call whose effect is the call: nothing is written at `output`.
    pub fn nothing() -> Self {
        Self {
            value: None,
            outcome: TaskOutcome::Success,
        }
    }

    /// A value recorded at `output` under a task outcome the handler chose —
    /// today only [`TaskOutcome::Status`] for a partial bulk write.
    pub fn with_outcome(value: Value, outcome: TaskOutcome) -> Self {
        Self {
            value: Some(value),
            outcome,
        }
    }
}

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
    ///
    /// [`ConnectorTarget`] rather than `ConnectorKind` so that the portable
    /// dialect — valid against `db` or `es` — can name its two-variant target
    /// and get the same wrong-type refusal as everything else.
    type Kind: ConnectorTarget;

    /// The task input's shape. [`TemplatedInput`] for the twelve handlers
    /// taking freeform JSON; a typed config for `http_call` and
    /// `publish_kafka`.
    type Input: ConnectorInput;

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
        input: &Self::Input,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, HandlerError>;

    /// Check the connector's operation gates.
    ///
    /// Separate from [`run`](Self::run) because a gate refusal is the
    /// connector's answer, not the backend's: it must happen before anything is
    /// dialled. Default is "no gate of its own" — a connector type whose
    /// `operations` vocabulary is empty.
    ///
    /// Takes the parsed input because for two handlers the gate is not fixed:
    /// `mongo_write` gates on the operation its envelope names, and
    /// `data_write` on the one its command resolves to. A gate chosen after
    /// the backend is dialled would be no gate at all, so the parsed input has
    /// to reach it here.
    fn gate(
        _parsed: &Self::Parsed,
        _conn: &<Self::Kind as ConnectorTarget>::Config,
        _connector: &str,
    ) -> Result<(), HandlerError> {
        Ok(())
    }

    /// The call itself, against a resolved, type-checked, gated connector.
    ///
    /// This is the seam a fake backend plugs into. Everything above it is the
    /// wrapper's; everything below is the connector's.
    ///
    /// See [`Produced`] for what a handler says about its result: a bare
    /// `Value` converts, and the two exceptions — no output at all, a partial
    /// bulk write — are spelled out.
    ///
    /// The input is here as well as in [`parse`](Self::parse) because
    /// [`Parsed`](Self::Parsed) is not "everything the handler needs" — it is
    /// "everything that had to be read while `ctx` was still shared". A literal
    /// the call needs later is read from `input` here, rather than copied into
    /// `Parsed` to survive the borrow.
    async fn run(
        &self,
        parsed: Self::Parsed,
        conn: &<Self::Kind as ConnectorTarget>::Config,
        call: &ConnectorCall<'_>,
        input: &Self::Input,
        ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError>;

    /// Parse the raw task JSON into [`Input`](Self::Input).
    ///
    /// Forwarded to `AsyncFunctionHandler::parse_input` by the wrapper, with
    /// the same default, so wrapping a handler cannot silently drop a custom
    /// parse.
    fn parse_input(input: &Value) -> dataflow_rs::Result<Self::Input> {
        serde_json::from_value(input.clone()).map_err(DataflowError::from_serde)
    }

    /// Compile the `Template` fields of a just-parsed input, once at engine
    /// build. Forwarded by the wrapper for the same reason as
    /// [`parse_input`](Self::parse_input).
    fn compile_input(_input: &mut Self::Input, _c: &TemplateCompiler) -> dataflow_rs::Result<()> {
        Ok(())
    }
}

/// The shell, applied to a [`ConnectorHandler`].
///
/// Registration wraps each handler in this, and that is the whole enforcement:
/// a handler that is not wrapped is not an `AsyncFunctionHandler` and cannot be
/// registered at all — where previously it registered fine and a test noticed.
pub struct Connector<H>(pub H);

#[async_trait]
impl<H: ConnectorHandler> AsyncFunctionHandler for Connector<H> {
    type Input = H::Input;

    fn parse_input(input: &Value) -> dataflow_rs::Result<Self::Input> {
        H::parse_input(input)
    }

    fn compile_input(input: &mut Self::Input, c: &TemplateCompiler) -> dataflow_rs::Result<()> {
        // The input's own expression fields first — the wrapper is the only
        // place holding both the input and `H::NAME` — then anything the
        // handler compiles for itself.
        input.compile(H::NAME, c)?;
        H::compile_input(input, c)
    }

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Self::Input,
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
            let config = call.resolve(registry).await?;
            let conn = require_connector::<H::Kind>(&config, call.connector)?;
            H::gate(&parsed, conn, call.connector).map_err(dataflow_rs::DataflowError::from)?;

            let produced = self
                .0
                .run(parsed, conn, &call, input, ctx)
                .await
                .map_err(dataflow_rs::DataflowError::from)?;

            if let Some(value) = produced.value {
                apply_output(ctx, &call.output, value);
            }
            Ok(produced.outcome)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::ConnectorRegistry;

    /// A handler whose `parse` needs a field the task does not set, so the two
    /// possible complaints — the literal `connector` and the message-dependent
    /// `key` — are both available and their order is the thing under test.
    struct Probe(Arc<ConnectorRegistry>);

    #[async_trait]
    impl ConnectorHandler for Probe {
        const NAME: &'static str = "probe";
        type Kind = crate::connector::kind::Cache;
        type Input = TemplatedInput;
        type Parsed = String;

        fn registry(&self) -> &Arc<ConnectorRegistry> {
            &self.0
        }

        fn parse(
            &self,
            call: &ConnectorCall<'_>,
            input: &TemplatedInput,
            _ctx: &TaskContext<'_>,
        ) -> Result<Self::Parsed, HandlerError> {
            Ok(call.require_str(input, "key")?.to_string())
        }

        async fn run(
            &self,
            _parsed: Self::Parsed,
            _conn: &crate::connector::CacheConnectorConfig,
            _call: &ConnectorCall<'_>,
            _input: &TemplatedInput,
            _ctx: &mut TaskContext<'_>,
        ) -> Result<Produced, HandlerError> {
            Ok(Produced::nothing())
        }
    }

    /// F58, as a property of the wrapper rather than of each handler's source.
    ///
    /// A task missing both `connector` and the handler's own field must be told
    /// about `connector`: the author fixes it, re-runs, and only then learns
    /// about `key` — which is the ordering that made this a defect. Every
    /// handler used to be checked for it by reading its `.rs` file and
    /// comparing byte offsets, one test per handler in a list that a new
    /// handler could simply not join. Here it is unconditional: `parse` takes
    /// the `call` as an argument, so it cannot run before the prologue that
    /// built it.
    #[tokio::test]
    async fn the_literal_prologue_is_reported_before_anything_message_dependent() {
        let handler = Connector(Probe(Arc::new(ConnectorRegistry::new(Default::default()))));
        let datalogic = Arc::new(dataflow_rs::datalogic_rs::Engine::new());
        let mut message = dataflow_rs::Message::from_value(&serde_json::json!({}));
        let mut ctx = TaskContext::new(&mut message, &datalogic);

        let err = handler
            .execute(&mut ctx, &TemplatedInput::from(serde_json::json!({})))
            .await
            .expect_err("a task naming no connector cannot run");
        let msg = err.to_string();
        assert!(
            msg.contains("connector"),
            "the literal prologue must be reported first: {msg}"
        );
        assert!(
            !msg.contains("'key'"),
            "the message-dependent field must not pre-empt it: {msg}"
        );
    }
}
