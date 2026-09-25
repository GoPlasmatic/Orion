//! `cache_invalidate`: bump response-cache namespaces from a workflow.
//!
//! The write-side half of `cache.namespaces` (see `channel::cache_namespace`).
//! A workflow that changes the data a namespace covers bumps it, and every
//! channel response cached under that namespace stops matching — on this node
//! and, through the shared Redis, on every node.
//!
//! It takes no connector, deliberately. A response-cache entry can live in any
//! store a channel resolves — the default one or any cache connector — and the
//! author invalidating "the ladder" should not have to know which. The handler
//! bumps the namespace in every store an entry could be in. It also cannot
//! write a response body: the response cache's keyspace stays one only Orion
//! writes to (S19), and a workflow can move a counter in it, nothing else.

use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::json;

use super::connector_helpers::{
    apply_output, output_declared, resolve_output_path, resolve_required_str_list, to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::TemplatedInput;
use crate::channel::cache_namespace;
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::CachePool;
use crate::engine::HandlerError;

const NAME: &str = "cache_invalidate";

/// Most namespaces one call may bump.
const MAX_PER_CALL: usize = 64;

pub struct CacheInvalidateHandler {
    pub channel_loader: Arc<crate::channel::ChannelLoader>,
    pub registry: Arc<ConnectorRegistry>,
    pub cache_pool: Arc<CachePool>,
}

#[async_trait]
impl AsyncFunctionHandler for CacheInvalidateHandler {
    type Input = TemplatedInput;

    fn compile_input(
        input: &mut Self::Input,
        c: &dataflow_rs::engine::functions::TemplateCompiler,
    ) -> dataflow_rs::Result<()> {
        input.compile(NAME, c)
    }

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &TemplatedInput,
    ) -> dataflow_rs::Result<TaskOutcome> {
        self.run(ctx, input)
            .await
            .map_err(|e| e.prefixed(NAME).into())
    }
}

impl CacheInvalidateHandler {
    async fn run(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &TemplatedInput,
    ) -> Result<TaskOutcome, HandlerError> {
        let namespaces = resolve_required_str_list(input, "namespaces", NAME, ctx, MAX_PER_CALL)?;
        if namespaces.is_empty() {
            return Err(DataflowError::Validation(
                "'namespaces' resolved to an empty list; name at least one".to_string(),
            )
            .into());
        }
        for ns in &namespaces {
            cache_namespace::check_name(ns).map_err(DataflowError::Validation)?;
        }
        // Resolved before anything is bumped, so a bad `output` does not leave
        // an invalidation behind a failed task.
        let output = if output_declared(input) {
            Some(resolve_output_path(input, NAME, ctx)?)
        } else {
            None
        };

        let stores = self
            .channel_loader
            .invalidate_namespaces(&self.registry, &self.cache_pool, &namespaces, "workflow")
            .await
            .map_err(to_exec_error)?;
        tracing::debug!(namespaces = ?namespaces, stores, "Invalidated response-cache namespaces");

        if let Some(path) = output {
            apply_output(
                ctx,
                &path,
                json!({ "namespaces": namespaces.len(), "stores": stores }),
            );
        }
        Ok(TaskOutcome::Success)
    }
}

// -- Input schema (F53) --

pub(super) const CACHE_INVALIDATE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "namespaces",
        description: "Response-cache namespaces to invalidate (at most 64), as declared in channels' `cache.namespaces`. JSONLogic: an array of literals or expressions, or an expression evaluating to one.",
        kind: FieldKind::Array,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where `{\"namespaces\": n, \"stores\": m}` is stored. Omit to record nothing.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];
