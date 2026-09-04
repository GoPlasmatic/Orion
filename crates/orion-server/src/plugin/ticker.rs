//! The epoch ticker: the clock every plugin deadline is measured in.
//!
//! Wasmtime's epoch interruption is a counter the host advances; a store
//! traps when the counter passes its deadline. Nothing advances it but this
//! task, so it runs under the supervisor as `Required`: a dead ticker would
//! silently disable every deadline, and `/readyz` should say so rather than
//! the next spinning guest.

use std::sync::Arc;

use super::runtime::{EPOCH_TICK, WasmRuntime};
use crate::runtime::{Criticality, TaskRegistry};

pub const TASK_NAME: &str = "plugin_epoch_ticker";

/// Start advancing `runtime`'s epoch every [`EPOCH_TICK`] until shutdown.
pub fn start(tasks: &TaskRegistry, runtime: Arc<WasmRuntime>) {
    tasks.supervise(TASK_NAME, Criticality::Required, move |mut shutdown| {
        let runtime = runtime.clone();
        async move {
            loop {
                if !shutdown.sleep(EPOCH_TICK).await {
                    return;
                }
                runtime.increment_epoch();
            }
        }
    });
}
