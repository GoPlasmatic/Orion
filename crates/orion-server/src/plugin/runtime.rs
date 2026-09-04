//! One Wasmtime engine per process, compiled components keyed by digest, and
//! a fresh store per invocation.
//!
//! - **Compile once per digest.** `Component::new` runs Cranelift over the
//!   whole component; it is the expensive step and it never runs on a request
//!   task — [`WasmRuntime::load`] moves it to the blocking pool. The result is
//!   an `InstancePre`: every linking decision made, nothing instantiated.
//! - **Fresh `Store` per invocation.** An instance lives inside its store and a
//!   store cannot be reset, so each call instantiates from the `InstancePre`
//!   into a new store carrying its own fuel, deadline and memory limiter. The
//!   pooling allocator makes that microseconds. A store that trapped is
//!   dropped, so no guest state crosses messages.
//! - **Two clocks.** The epoch deadline traps a guest when the ticker
//!   ([`super::ticker`]) has advanced the engine past it; the wall-clock
//!   timeout around the call is the belt to that brace, and it can only fire
//!   because the guest yields every [`FUEL_YIELD_INTERVAL`] units of fuel.
//!   Fuel itself is a backstop, not a contract: its cost moves between
//!   Wasmtime versions, so operators reason in `max_timeout_ms`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, Store};

use super::error::{Category, Failure};
use super::limits::{HostState, Limits};
use crate::config::PluginsConfig;

wasmtime::component::bindgen!({
    world: "plugin",
    path: "wit",
    exports: { default: async },
});

pub use exports::orion::plugin::functions::{ErrorClass as GuestErrorClass, PluginError};

/// How often the epoch ticker advances the engine. A deadline is measured in
/// these, so it is the granularity of every plugin timeout.
pub const EPOCH_TICK: Duration = Duration::from_millis(10);

/// Fuel between forced yields to the executor — what lets the wall-clock
/// timeout cancel a guest that never returns. Small enough that a spinning
/// guest yields many times a millisecond, large enough to be noise on a
/// real workload.
pub const FUEL_YIELD_INTERVAL: u64 = 1_000_000;

/// A component compiled and linked, ready to instantiate.
pub struct LoadedComponent {
    /// `sha256:<hex>` of the bytes — the identity everything names it by.
    pub digest: String,
    pub size: usize,
    pub compile_time: Duration,
    pre: PluginPre<HostState>,
}

impl std::fmt::Debug for LoadedComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedComponent")
            .field("digest", &self.digest)
            .field("size", &self.size)
            .field("compile_time", &self.compile_time)
            .finish()
    }
}

/// Why a component could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// Larger than `plugins.max_component_bytes`.
    TooLarge { size: usize, max: usize },
    /// Not a valid component, or one Cranelift refuses.
    Compile(String),
    /// A component that does not export the `orion:plugin` world, or imports
    /// something — which the world forbids.
    Link(String),
    /// A declared function did not answer a probe call.
    SelfTest { function: String, reason: String },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { size, max } => write!(
                f,
                "component is {size} bytes, over plugins.max_component_bytes ({max})"
            ),
            Self::Compile(reason) => write!(f, "component failed to compile: {reason}"),
            Self::Link(reason) => write!(
                f,
                "component does not implement the {} world: {reason}",
                super::ABI
            ),
            Self::SelfTest { function, reason } => {
                write!(f, "self-test of '{function}' failed: {reason}")
            }
        }
    }
}

impl std::error::Error for LoadError {}

/// The engine, its linker, and the digest-keyed cache of loaded components.
pub struct WasmRuntime {
    engine: Engine,
    linker: Linker<HostState>,
    max_component_bytes: usize,
    cache: Mutex<HashMap<String, Arc<LoadedComponent>>>,
    /// Instances alive right now, across every function.
    live: AtomicU64,
}

impl std::fmt::Debug for WasmRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmRuntime")
            .field("max_component_bytes", &self.max_component_bytes)
            .field("live", &self.live.load(Ordering::Relaxed))
            .finish()
    }
}

impl WasmRuntime {
    /// Build the engine the design specifies: component model, async, fuel,
    /// epoch interruption, pooling allocator sized from the ceilings, no WASI.
    pub fn new(config: &PluginsConfig) -> Result<Arc<Self>, String> {
        let mut c = Config::new();
        c.wasm_component_model(true);
        // Async is always on in Wasmtime 48; the `async` feature is what enables it.
        c.consume_fuel(true);
        c.epoch_interruption(true);

        let instances = config.max_live_instances;
        let mut pool = PoolingAllocationConfig::default();
        pool.total_component_instances(instances);
        pool.total_core_instances(instances.saturating_mul(4));
        pool.total_memories(instances.saturating_mul(2));
        pool.total_tables(instances.saturating_mul(4));
        pool.total_stacks(instances);
        pool.max_memory_size(config.max_memory_bytes);
        c.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));

        let engine = Engine::new(&c).map_err(|e| format!("wasmtime engine: {e}"))?;
        // Nothing is added to the linker: the world has no imports, and a
        // component that needs one fails `instantiate_pre` below.
        let linker = Linker::new(&engine);
        Ok(Arc::new(Self {
            engine,
            linker,
            max_component_bytes: config.max_component_bytes,
            cache: Mutex::new(HashMap::new()),
            live: AtomicU64::new(0),
        }))
    }

    /// The identity of a component: `sha256:<hex>` of its bytes.
    pub fn digest(bytes: &[u8]) -> String {
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Advance every store's clock by one tick. Called by the ticker.
    pub fn increment_epoch(&self) {
        self.engine.increment_epoch();
    }

    /// Instances alive right now.
    pub fn live_instances(&self) -> u64 {
        self.live.load(Ordering::Relaxed)
    }

    /// A component already compiled under `digest`, if any.
    pub fn cached(&self, digest: &str) -> Option<Arc<LoadedComponent>> {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(digest)
            .cloned()
    }

    /// Compile and link `bytes`, or return the cached result for the same
    /// digest. Blocks for the length of a Cranelift run — call it from the
    /// blocking pool ([`Self::load`]) on a request path.
    pub fn load_blocking(&self, bytes: &[u8]) -> Result<Arc<LoadedComponent>, LoadError> {
        if bytes.len() > self.max_component_bytes {
            return Err(LoadError::TooLarge {
                size: bytes.len(),
                max: self.max_component_bytes,
            });
        }
        let digest = Self::digest(bytes);
        if let Some(loaded) = self.cached(&digest) {
            return Ok(loaded);
        }
        let started = Instant::now();
        let component = Component::new(&self.engine, bytes)
            .map_err(|e| LoadError::Compile(first_line(&e.to_string())))?;
        let pre = self
            .linker
            .instantiate_pre(&component)
            .and_then(PluginPre::new)
            .map_err(|e| LoadError::Link(first_line(&e.to_string())))?;
        let loaded = Arc::new(LoadedComponent {
            digest: digest.clone(),
            size: bytes.len(),
            compile_time: started.elapsed(),
            pre,
        });
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(digest)
            .or_insert_with(|| loaded.clone())
            .clone()
            .pipe(Ok)
    }

    /// [`Self::load_blocking`] on the blocking pool.
    pub async fn load(self: &Arc<Self>, bytes: Vec<u8>) -> Result<Arc<LoadedComponent>, LoadError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.load_blocking(&bytes))
            .await
            .map_err(|e| LoadError::Compile(format!("compile task failed: {e}")))?
    }

    /// Prove every declared function answers: instantiate once and call each
    /// with an empty object. Any return — a value or a guest error — passes;
    /// a trap, a limit or a missing export fails, which is what a draft that
    /// "is already known to load" means.
    pub async fn self_test(
        &self,
        loaded: &LoadedComponent,
        limits: &Limits,
        functions: &[&str],
    ) -> Result<(), LoadError> {
        for function in functions {
            match self.invoke(loaded, limits, function, "{}").await {
                Ok(_) | Err(Invocation::Guest(_)) => {}
                Err(Invocation::Host(failure)) => {
                    return Err(LoadError::SelfTest {
                        function: (*function).to_string(),
                        reason: failure
                            .detail
                            .map(|d| format!("{} ({d})", failure.message))
                            .unwrap_or(failure.message),
                    });
                }
            }
        }
        Ok(())
    }

    /// One invocation: a fresh store under `limits`, instantiate, call, and
    /// classify whatever came back. The returned string is the guest's JSON,
    /// already checked against `max_response_bytes` but not yet parsed.
    pub async fn invoke(
        &self,
        loaded: &LoadedComponent,
        limits: &Limits,
        function: &str,
        input: &str,
    ) -> Result<String, Invocation> {
        let mut store = Store::new(&self.engine, HostState::new(limits));
        store.limiter(|state| &mut state.limiter);
        store.set_fuel(limits.fuel).map_err(|e| {
            Invocation::Host(Failure::host(Category::Trap, "store setup").with_detail(e))
        })?;
        store
            .fuel_async_yield_interval(Some(FUEL_YIELD_INTERVAL))
            .map_err(|e| {
                Invocation::Host(Failure::host(Category::Trap, "store setup").with_detail(e))
            })?;
        let ticks = u64::try_from(limits.timeout.as_millis() / EPOCH_TICK.as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
            + 1;
        store.set_epoch_deadline(ticks);
        store.epoch_deadline_trap();

        self.live.fetch_add(1, Ordering::Relaxed);
        let call = async {
            let plugin = loaded.pre.instantiate_async(&mut store).await?;
            plugin
                .orion_plugin_functions()
                .call_invoke(&mut store, function, input)
                .await
        };
        // A little past the epoch deadline, so the epoch trap — which names
        // the cause precisely — wins when the ticker is running, and this
        // still fires when it is not.
        let outcome = tokio::time::timeout(limits.timeout + EPOCH_TICK * 2, call).await;
        self.live.fetch_sub(1, Ordering::Relaxed);

        let memory_refused = store.data().limiter.refused;
        match outcome {
            Err(_elapsed) => Err(Invocation::Host(Failure::host(
                Category::Timeout,
                "the invocation exceeded its deadline",
            ))),
            Ok(Err(e)) => Err(Invocation::Host(Failure::from_wasmtime(&e, memory_refused))),
            Ok(Ok(Err(guest))) => Err(Invocation::Guest(Failure::guest(
                &guest.code,
                matches!(guest.class, GuestErrorClass::CallerInput),
                &guest.message,
            ))),
            Ok(Ok(Ok(json))) if json.len() > limits.max_response_bytes => {
                Err(Invocation::Host(Failure::host(
                    Category::ResponseSize,
                    format!(
                        "the plugin returned {} bytes, over the {} byte limit",
                        json.len(),
                        limits.max_response_bytes
                    ),
                )))
            }
            Ok(Ok(Ok(json))) => Ok(json),
        }
    }
}

/// A failed invocation, by who failed it. The distinction matters to the
/// self-test — a guest that refuses `{}` still proves its export works — and
/// to nothing else, so the handler folds both into one [`Failure`].
#[derive(Debug)]
pub enum Invocation {
    Guest(Failure),
    Host(Failure),
}

impl Invocation {
    pub fn into_failure(self) -> Failure {
        match self {
            Self::Guest(f) | Self::Host(f) => f,
        }
    }
}

/// Wasmtime's errors are multi-line reports; the first line is the message.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").to_string()
}

/// `Result::Ok` as a method, for the one place the borrow checker wants the
/// cache lock released before the value is returned.
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}
