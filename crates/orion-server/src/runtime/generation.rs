//! The node's serving generation: the engine and the channel estate built
//! from the same rows, published as one value.
//!
//! A reload rebuilds two things — the channel snapshot (routes, guards, rate
//! limiters, compiled validation) and the engine (the workflows themselves).
//! They used to be two independently published `ArcSwap`s, stored one after
//! the other. Each store was atomic; the *pair* was not. Between them the node
//! served the new channel estate against the old engine, and the ordering
//! comment that justified it ("guards before reachability") could only ever
//! pick which mismatch you got, never remove it:
//!
//! - a just-activated channel was routable while the old engine had no
//!   workflow for it, so `process_message_for_channel` matched zero workflows,
//!   returned `Ok`, and the caller was handed back its own input with a `200`
//!   — a success that did nothing;
//! - a channel repointed to a different `workflow_id` was admitted under its
//!   new guards and executed by its old workflow.
//!
//! One value, one store. A request loads a [`RuntimeGeneration`] once and
//! holds it for its whole life, so every answer it gives comes from a single
//! build — and because the `Arc` keeps that generation alive, a reload
//! midway through changes nothing underneath it.
//!
//! Publication is the only mutation, and [`RuntimeHandle::publish`] is the
//! only way to perform it: there is no way to store one half. That is the
//! whole guarantee, and `runtime_generation_test` pins it by counting stores.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;

use crate::channel::ChannelSnapshot;
use crate::engine::FunctionRegistry;
use crate::model::ModelSet;
use crate::plugin::PluginSet;

tokio::task_local! {
    /// Pinned [`RuntimeGeneration`] for the current root run (`V2-05.2`, `V2-05.3`).
    ///
    /// When set via `PINNED_GENERATION.scope(Arc::clone(&generation), fut)`,
    /// nested `channel_call`, guard stores, and model inference inside `fut`
    /// resolve against the parent's admitted generation rather than observing
    /// a newer generation published mid-run.
    pub static PINNED_GENERATION: Arc<RuntimeGeneration>;
}

/// One complete, self-consistent generation of everything a request is served
/// from.
///
/// Cheap to hold: the channels of an unchanged row are carried over by `Arc`
/// between generations (N6/N17), so retaining one for the length of a request
/// retains a `HashMap` and a route table, not a copy of the estate.
pub struct RuntimeGeneration {
    /// Monotonic, starting at 0 for the empty generation a node boots on.
    /// Assigned by [`RuntimeHandle::publish`] — a generation cannot be built
    /// with an id of its own choosing, so the number always means "how many
    /// times this node has republished".
    pub id: u64,
    /// The engine built from the same rows: every row compiled on its
    /// `Registry`, whose verdict decided which channels load (I182).
    /// `channel_call` runs its child on it, and a reload that keeps the
    /// plugins compiles on its compiler. `None` only on the boot generation,
    /// before the first publish, which serves nothing.
    pub engine: Option<dataflow_rs::Engine>,
    pub channels: Arc<ChannelSnapshot>,
    /// Every function the engine above dispatches, and what each declares —
    /// what create-time validation and `GET /admin/functions` read, so a
    /// workflow is accepted against exactly the set this generation runs.
    pub functions: Arc<FunctionRegistry>,
    /// The plugins this generation loaded, and the reasons any did not —
    /// `/health` and `GET /plugins/{id}` read it, and a reload compares its
    /// fingerprint to decide whether the engine must be rebuilt.
    pub plugins: Arc<PluginSet>,
    /// The models this generation can run — each with its adapters compiled
    /// on the process's expression engine, which the set holds and every
    /// reload keeps, so a model whose manifest is unchanged carries its
    /// compiled adapters into the next generation (I188) — and the reasons
    /// any active row did not load. `model_infer` resolves through it;
    /// `/health` and `GET /models/{id}` read it.
    pub models: Arc<ModelSet>,
    /// Immutable snapshot of resolved connector configurations published with
    /// this generation (`TD-07`). When `Some`, connector tasks executing under
    /// [`PINNED_GENERATION`] resolve against this snapshot rather than reading
    /// a concurrently reloaded [`crate::connector::ConnectorRegistry`].
    pub connectors:
        Option<Arc<std::collections::HashMap<String, Arc<crate::connector::ConnectorConfig>>>>,
    /// Run-wide resource budget ceilings configured for this generation (`TD-03`).
    pub run_budget_limits: crate::engine::budget::RunBudgetLimits,
    /// Which connections each workflow may call through `connector_call`.
    /// Empty unless the generation was built from a deployment manifest.
    pub connector_authority: Arc<super::authority::ConnectorAuthority>,
}

/// Everything one generation is built from, published together by
/// [`RuntimeHandle::publish_parts`]. The first five are what every
/// generation needs; the rest default.
pub struct GenerationParts {
    pub engine: dataflow_rs::Engine,
    pub channels: Arc<ChannelSnapshot>,
    pub functions: Arc<FunctionRegistry>,
    pub plugins: Arc<PluginSet>,
    pub models: Arc<ModelSet>,
    pub connectors:
        Option<Arc<std::collections::HashMap<String, Arc<crate::connector::ConnectorConfig>>>>,
    pub run_budget_limits: crate::engine::budget::RunBudgetLimits,
    pub connector_authority: Arc<super::authority::ConnectorAuthority>,
}

impl GenerationParts {
    pub fn new(
        engine: dataflow_rs::Engine,
        channels: Arc<ChannelSnapshot>,
        functions: Arc<FunctionRegistry>,
        plugins: Arc<PluginSet>,
        models: Arc<ModelSet>,
    ) -> Self {
        Self {
            engine,
            channels,
            functions,
            plugins,
            models,
            connectors: None,
            run_budget_limits: Default::default(),
            connector_authority: Default::default(),
        }
    }
}

impl RuntimeGeneration {
    /// The workflows this generation serves: its program's. Empty on the boot
    /// generation, which has no program yet.
    pub fn workflows(&self) -> &[dataflow_rs::program::WorkflowInfo] {
        self.engine
            .as_ref()
            .map_or(&[], |engine| engine.program().workflows())
    }
}

/// The live generation, swapped wholesale on reload.
///
/// `ArcSwap` rather than a lock because every ingress reads it and the only
/// writer stores a finished value: readers never block, never wait, and never
/// need an `.await`. A reader mid-request keeps the generation it started with
/// until it drops the `Arc`, so publication needs no timeout to bound how long
/// it might hold readers off.
///
/// This was two handles — `EngineHandle` around the engine and an `ArcSwap`
/// inside `ChannelRegistry` — and the argument above was written out for each
/// of them separately. It was true of each and false of the pair, which is the
/// bug this type exists to make unrepresentable.
///
/// Serialising *reloads* is a separate concern and not what this type is for:
/// two concurrent reloads would each build from a possibly stale read, and the
/// loser's publish would win. `AppStateInner::reload_lock` is what prevents
/// that; this type only guarantees that whatever is published is published
/// atomically.
pub struct RuntimeHandle {
    current: ArcSwap<RuntimeGeneration>,
    /// How many generations have been published. The id of the live one, but
    /// readable without loading it — which is what lets a test assert that one
    /// reload performs exactly one publish.
    published: AtomicU64,
}

impl RuntimeHandle {
    /// The handle a node boots with, around generation 0.
    ///
    /// Created before the engine exists, because the `channel_call` function
    /// is registered *on* that engine's registry and holds this handle: the
    /// boot generation has no engine until the first publish, which happens
    /// before anything is served.
    pub fn new(channels: Arc<ChannelSnapshot>, functions: Arc<FunctionRegistry>) -> Self {
        Self {
            current: ArcSwap::from_pointee(RuntimeGeneration {
                id: 0,
                engine: None,
                channels,
                functions,
                plugins: Arc::new(PluginSet::empty()),
                models: Arc::new(ModelSet::empty()),
                connectors: None,
                run_budget_limits: Default::default(),
                connector_authority: Default::default(),
            }),
            published: AtomicU64::new(0),
        }
    }

    /// A snapshot of the live generation. Wait-free; the returned `Arc` stays
    /// valid across a concurrent [`Self::publish`].
    ///
    /// **Load once per unit of work** — one HTTP request, one cron occurrence,
    /// one queued trace, one `channel_call` — and pass the generation down.
    /// Two loads in one request can straddle a reload, which is the same class
    /// of bug as two published values, moved to the reader.
    pub fn load(&self) -> Arc<RuntimeGeneration> {
        self.current.load_full()
    }

    /// Return the task-local [`PINNED_GENERATION`] when executing inside an
    /// admitted root run (`V2-05.3`), or load the current generation otherwise.
    pub fn load_pinned_or_current(&self) -> Arc<RuntimeGeneration> {
        PINNED_GENERATION
            .try_with(Arc::clone)
            .unwrap_or_else(|_| self.load())
    }

    /// Publish the next generation. Returns its id.
    ///
    /// The engines, the channels, the function registry, the plugin set and
    /// the model set must be built from the same rows; taking them together
    /// is what makes that the only way to say it. Readers already holding a
    /// generation finish on it; every load after this returns the new one.
    pub fn publish(
        &self,
        engine: dataflow_rs::Engine,
        channels: Arc<ChannelSnapshot>,
        functions: Arc<FunctionRegistry>,
        plugins: Arc<PluginSet>,
        models: Arc<ModelSet>,
    ) -> u64 {
        self.publish_parts(GenerationParts::new(
            engine, channels, functions, plugins, models,
        ))
    }

    /// [`Self::publish`], with every part of the generation stated.
    pub fn publish_parts(&self, parts: GenerationParts) -> u64 {
        let GenerationParts {
            engine,
            channels,
            functions,
            plugins,
            models,
            connectors,
            run_budget_limits,
            connector_authority,
        } = parts;
        let id = self.published.fetch_add(1, Ordering::Relaxed) + 1;
        self.current.store(Arc::new(RuntimeGeneration {
            id,
            engine: Some(engine),
            channels,
            functions,
            plugins,
            models,
            connectors,
            run_budget_limits,
            connector_authority,
        }));
        id
    }

    /// How many generations this node has published. `0` means it is still on
    /// the empty boot generation.
    pub fn published_count(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }
}

/// An empty handle for unit tests elsewhere in the crate: the boot generation,
/// with no engine and an estate with no channels.
#[cfg(test)]
pub(crate) fn test_handle() -> Arc<RuntimeHandle> {
    Arc::new(RuntimeHandle::new(
        Arc::new(ChannelSnapshot::empty()),
        FunctionRegistry::builtin().clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(id: &str) -> dataflow_rs::Engine {
        dataflow_rs::Engine::builder()
            .workflow_json(&serde_json::json!({"id": id, "tasks": [
                {"id": "t", "function": {"name": "map", "input": {"mappings": []}}}
            ]}))
            .build()
            .expect("an engine builds")
    }

    fn handle() -> RuntimeHandle {
        RuntimeHandle::new(
            Arc::new(ChannelSnapshot::empty()),
            FunctionRegistry::builtin().clone(),
        )
    }

    fn publish(handle: &RuntimeHandle, engine: dataflow_rs::Engine) -> u64 {
        handle.publish(
            engine,
            Arc::new(ChannelSnapshot::empty()),
            FunctionRegistry::builtin().clone(),
            Arc::new(PluginSet::empty()),
            Arc::new(ModelSet::empty()),
        )
    }

    fn ids(generation: &RuntimeGeneration) -> Vec<String> {
        generation
            .workflows()
            .iter()
            .map(|w| w.id.to_string())
            .collect()
    }

    /// The retention property, on every half at once — which is the whole
    /// point. A holder of generation N sees N's engine and N's channels after
    /// N+1 is published, so no request can be admitted by one generation and
    /// executed by another. An `Engine` is an `Arc` inside, so the check is
    /// that the held one runs the program it was published with.
    #[test]
    fn a_held_generation_survives_a_publish_whole() {
        let handle = handle();
        assert!(
            handle.load().engine.is_none(),
            "the boot generation has none"
        );

        publish(&handle, engine("first"));
        let held = handle.load();
        publish(&handle, engine("second"));

        assert_eq!(held.id, 1);
        assert_eq!(
            ids(&held),
            ["first"],
            "a held generation must keep the engine it was loaded with"
        );
        assert_eq!(handle.load().id, 2, "a later load sees the new generation");
        assert_eq!(ids(&handle.load()), ["second"]);
    }

    /// Ids are the publish count, so "which generation is this node on?" and
    /// "how many times has it republished?" cannot disagree.
    #[test]
    fn ids_count_publications() {
        let handle = handle();
        assert_eq!(handle.published_count(), 0);
        assert_eq!(handle.load().id, 0);

        for expected in 1..=3 {
            let id = publish(&handle, engine("w"));
            assert_eq!(id, expected);
            assert_eq!(handle.load().id, expected);
            assert_eq!(handle.published_count(), expected);
        }
    }
}
