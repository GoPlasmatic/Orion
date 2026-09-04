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
use crate::plugin::PluginSet;

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
    pub engine: Arc<dataflow_rs::Engine>,
    pub channels: Arc<ChannelSnapshot>,
    /// Every function the engine above dispatches, and what each declares —
    /// what create-time validation and `GET /admin/functions` read, so a
    /// workflow is accepted against exactly the set this generation runs.
    pub functions: Arc<FunctionRegistry>,
    /// The plugins this generation loaded, and the reasons any did not —
    /// `/health` and `GET /plugins/{id}` read it, and a reload compares its
    /// fingerprint to decide whether the engine must be rebuilt.
    pub plugins: Arc<PluginSet>,
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
    /// Created before the real engine exists, because the `channel_call`
    /// handler is registered *on* that engine and holds this handle: the
    /// placeholder engine bootstrap builds for its datalogic is the same one
    /// that stands in here until the first real publish.
    pub fn new(
        engine: Arc<dataflow_rs::Engine>,
        channels: Arc<ChannelSnapshot>,
        functions: Arc<FunctionRegistry>,
    ) -> Self {
        Self {
            current: ArcSwap::from_pointee(RuntimeGeneration {
                id: 0,
                engine,
                channels,
                functions,
                plugins: Arc::new(PluginSet::empty()),
            }),
            published: AtomicU64::new(0),
        }
    }

    /// A snapshot of the live generation. Wait-free; the returned `Arc` stays
    /// valid across a concurrent [`Self::publish`].
    ///
    /// **Load once per unit of work** — one HTTP request, one Kafka record,
    /// one queued trace, one `channel_call` — and pass the generation down.
    /// Two loads in one request can straddle a reload, which is the same class
    /// of bug as two published values, moved to the reader.
    pub fn load(&self) -> Arc<RuntimeGeneration> {
        self.current.load_full()
    }

    /// Publish the next generation. Returns its id.
    ///
    /// The engine, the channels and the function registry must be built from
    /// the same rows; taking them together is what makes that the only way to
    /// say it. Readers already holding a generation finish on it; every load
    /// after this returns the new one.
    pub fn publish(
        &self,
        engine: Arc<dataflow_rs::Engine>,
        channels: Arc<ChannelSnapshot>,
        functions: Arc<FunctionRegistry>,
        plugins: Arc<PluginSet>,
    ) -> u64 {
        let id = self.published.fetch_add(1, Ordering::Relaxed) + 1;
        self.current.store(Arc::new(RuntimeGeneration {
            id,
            engine,
            channels,
            functions,
            plugins,
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
/// with an engine that has no workflows and an estate with no channels.
#[cfg(test)]
pub(crate) fn test_handle() -> Arc<RuntimeHandle> {
    Arc::new(RuntimeHandle::new(
        Arc::new(
            dataflow_rs::Engine::builder()
                .build()
                .expect("an empty engine builds"),
        ),
        Arc::new(ChannelSnapshot::empty()),
        FunctionRegistry::builtin().clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Arc<dataflow_rs::Engine> {
        Arc::new(
            dataflow_rs::Engine::builder()
                .build()
                .expect("empty engine builds"),
        )
    }

    fn handle() -> (RuntimeHandle, Arc<dataflow_rs::Engine>) {
        let boot = engine();
        (
            RuntimeHandle::new(
                boot.clone(),
                Arc::new(ChannelSnapshot::empty()),
                FunctionRegistry::builtin().clone(),
            ),
            boot,
        )
    }

    /// The retention property, on **both** halves at once — which is the whole
    /// point. A holder of generation N sees N's engine and N's channels after
    /// N+1 is published, so no request can be admitted by one generation and
    /// executed by another.
    #[test]
    fn a_held_generation_survives_a_publish_whole() {
        let (handle, boot_engine) = handle();
        let held = handle.load();

        let next_engine = engine();
        handle.publish(
            next_engine.clone(),
            Arc::new(ChannelSnapshot::empty()),
            FunctionRegistry::builtin().clone(),
            Arc::new(PluginSet::empty()),
        );

        assert_eq!(held.id, 0);
        assert!(
            Arc::ptr_eq(&held.engine, &boot_engine),
            "a held generation must keep the engine it was loaded with"
        );
        assert!(
            !Arc::ptr_eq(&held.engine, &next_engine),
            "and must not see the engine published after it"
        );
        assert_eq!(handle.load().id, 1, "a later load sees the new generation");
        assert!(Arc::ptr_eq(&handle.load().engine, &next_engine));
    }

    /// Ids are the publish count, so "which generation is this node on?" and
    /// "how many times has it republished?" cannot disagree.
    #[test]
    fn ids_count_publications() {
        let (handle, _) = handle();
        assert_eq!(handle.published_count(), 0);
        assert_eq!(handle.load().id, 0);

        for expected in 1..=3 {
            let id = handle.publish(
                engine(),
                Arc::new(ChannelSnapshot::empty()),
                FunctionRegistry::builtin().clone(),
                Arc::new(PluginSet::empty()),
            );
            assert_eq!(id, expected);
            assert_eq!(handle.load().id, expected);
            assert_eq!(handle.published_count(), expected);
        }
    }
}
