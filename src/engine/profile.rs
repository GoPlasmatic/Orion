//! Per-request workflow profile mode.
//!
//! Lightweight, opt-in profiler that breaks a request down by phase
//! (engine lock wait, workflow logic, individual handler calls, trace
//! persistence) and ships the result back as `_orion.profile` on the
//! response (B3 shape lock — the `_orion` top-level namespace is
//! reserved for debug surfaces and never collides with workflow output).
//!
//! Activated by either an `X-Orion-Profile: 1` header or `?profile=1`
//! query parameter, gated globally by `tracing.debug_profile_enabled`.
//!
//! ## Design
//!
//! A [`ProfileCollector`] is carried through the request via a
//! `tokio::task_local!`. When the data route handler turns profiling on
//! it creates a collector and wraps the engine call in
//! `ORION_PROFILE.scope(...)`. Each Orion custom handler then calls
//! [`record`] to log the wall-clock cost of its body — `record` is a
//! single `try_with` no-op when no collector is in scope.
//!
//! `channel_call` recursion is depth-tracked so nested handler samples
//! don't double-count toward `handlers_total_ms`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::task_local;

/// One handler invocation sample.
#[derive(Debug, Clone)]
pub struct HandlerSample {
    pub function: &'static str,
    pub connector: Option<String>,
    pub duration: Duration,
    pub depth: u32,
}

/// Per-request profile state. Accessed via the [`ORION_PROFILE`] task-local.
pub struct ProfileCollector {
    /// Wall-clock at collector creation; used for `request_total_ms`.
    start: Instant,
    /// Time spent acquiring the engine read lock.
    engine_lock_wait: Mutex<Option<Duration>>,
    /// Total engine `process_message_for_channel` duration (set after the call).
    workflow_total: Mutex<Option<Duration>>,
    /// Time spent in `route_store_completed` (sync write OR queue submit).
    trace_store: Mutex<Option<Duration>>,
    /// One entry per custom-handler invocation, in invocation order.
    samples: Mutex<Vec<HandlerSample>>,
    /// `channel_call` recursion depth; `record` reads/increments this.
    depth: AtomicU32,
}

impl ProfileCollector {
    /// Build a fresh collector. Cheap — caller wraps it in `Arc`.
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            start: Instant::now(),
            engine_lock_wait: Mutex::new(None),
            workflow_total: Mutex::new(None),
            trace_store: Mutex::new(None),
            samples: Mutex::new(Vec::new()),
            depth: AtomicU32::new(0),
        })
    }

    pub fn set_engine_lock_wait(&self, d: Duration) {
        let mut g = self.engine_lock_wait.lock().expect("profile mutex");
        *g = Some(g.map_or(d, |existing| existing + d));
    }

    pub fn set_workflow_total(&self, d: Duration) {
        *self.workflow_total.lock().expect("profile mutex") = Some(d);
    }

    pub fn set_trace_store(&self, d: Duration) {
        *self.trace_store.lock().expect("profile mutex") = Some(d);
    }

    /// Stable shape version for the response/trace `profile` JSON object.
    ///
    /// Bump this when the rendered structure changes in a way that would
    /// break existing consumers. Clients should branch on `version` to
    /// tolerate future shape changes. v0.2.0 ships with v1.
    pub const PROFILE_VERSION: u32 = 1;

    /// Render the collector as the response/trace `profile` JSON object.
    ///
    /// **Shape (v1):**
    /// ```text
    /// {
    ///   "version":            1,
    ///   "totals_ms":          1.2,           // wall-clock request duration
    ///   "phases":             [{name, ms, pct}, ...],
    ///   // detail fields (kept for richer drill-down):
    ///   "request_total_ms":   1.2,
    ///   "handlers_total_ms":  0.8,
    ///   "handlers":           [{function, duration_ms, pct_of_workflow, ...}],
    ///   "by_function":        {fn_name: {count, total_ms}, ...},
    ///   "by_connector":       {connector: {count, total_ms}, ...},
    ///   "breakdown_pct":      {external_io, workflow_overhead, trace_store, engine_lock_wait},
    ///   "workflow_total_ms":  optional,
    ///   "workflow_overhead_ms": optional,
    ///   "engine_lock_wait_ms": optional,
    ///   "trace_store_ms":      optional
    /// }
    /// ```
    pub fn to_json(&self) -> Value {
        let samples = self.samples.lock().expect("profile mutex").clone();
        let engine_lock_wait = self.engine_lock_wait.lock().expect("profile mutex").take();
        let workflow_total = self.workflow_total.lock().expect("profile mutex").take();
        let trace_store = self.trace_store.lock().expect("profile mutex").take();

        let request_total = self.start.elapsed();

        // Depth-0 samples = top-level handler calls. Anything nested
        // happened inside a depth-0 call (currently only via `channel_call`)
        // and is already accounted for in the parent's duration.
        let handlers_total_ms: f64 = samples
            .iter()
            .filter(|s| s.depth == 0)
            .map(|s| s.duration.as_secs_f64() * 1000.0)
            .sum();

        let workflow_total_ms = workflow_total.map(|d| d.as_secs_f64() * 1000.0);
        let engine_lock_wait_ms = engine_lock_wait.map(|d| d.as_secs_f64() * 1000.0);
        let trace_store_ms = trace_store.map(|d| d.as_secs_f64() * 1000.0);
        let request_total_ms = request_total.as_secs_f64() * 1000.0;

        let workflow_overhead_ms = match (workflow_total_ms, engine_lock_wait_ms) {
            (Some(w), Some(lw)) => Some((w - handlers_total_ms - lw).max(0.0)),
            (Some(w), None) => Some((w - handlers_total_ms).max(0.0)),
            _ => None,
        };

        // Per-sample JSON with pct of workflow_total.
        let workflow_basis = workflow_total_ms.unwrap_or(0.0);
        let handlers_json: Vec<Value> = samples
            .iter()
            .filter(|s| s.depth == 0)
            .map(|s| {
                let dur_ms = s.duration.as_secs_f64() * 1000.0;
                let pct = if workflow_basis > 0.0 {
                    (dur_ms / workflow_basis) * 100.0
                } else {
                    0.0
                };
                let mut obj = json!({
                    "function": s.function,
                    "duration_ms": round2(dur_ms),
                    "pct_of_workflow": round2(pct),
                });
                if let Some(ref c) = s.connector {
                    obj["connector"] = Value::String(c.clone());
                }
                if samples.iter().any(|x| x.depth > s.depth) {
                    // Attach nested samples that occurred during this top-level
                    // call. We don't have explicit parent links — for now, we
                    // attach all samples with depth > 0 as a flat list onto the
                    // first depth-0 channel_call sample. This is a coarse but
                    // useful approximation; precise grouping would require
                    // recording start/end instants and matching by interval.
                    if s.function == "channel_call" {
                        let nested: Vec<Value> = samples
                            .iter()
                            .filter(|x| x.depth > 0)
                            .map(|x| {
                                let dms = x.duration.as_secs_f64() * 1000.0;
                                let mut o = json!({
                                    "function": x.function,
                                    "duration_ms": round2(dms),
                                    "depth": x.depth,
                                });
                                if let Some(ref c) = x.connector {
                                    o["connector"] = Value::String(c.clone());
                                }
                                o
                            })
                            .collect();
                        obj["nested"] = Value::Array(nested);
                    }
                }
                obj
            })
            .collect();

        // by_function aggregation (depth-0 only).
        let mut by_function: std::collections::BTreeMap<&'static str, (u32, f64)> =
            std::collections::BTreeMap::new();
        for s in samples.iter().filter(|s| s.depth == 0) {
            let entry = by_function.entry(s.function).or_insert((0, 0.0));
            entry.0 += 1;
            entry.1 += s.duration.as_secs_f64() * 1000.0;
        }
        let by_function_json: serde_json::Map<String, Value> = by_function
            .into_iter()
            .map(|(k, (count, total))| {
                (
                    k.to_string(),
                    json!({ "count": count, "total_ms": round2(total) }),
                )
            })
            .collect();

        // by_connector aggregation (depth-0 only, connector present).
        let mut by_connector: std::collections::BTreeMap<String, (u32, f64)> =
            std::collections::BTreeMap::new();
        for s in samples.iter().filter(|s| s.depth == 0) {
            if let Some(ref c) = s.connector {
                let entry = by_connector.entry(c.clone()).or_insert((0, 0.0));
                entry.0 += 1;
                entry.1 += s.duration.as_secs_f64() * 1000.0;
            }
        }
        let by_connector_json: serde_json::Map<String, Value> = by_connector
            .into_iter()
            .map(|(k, (count, total))| (k, json!({ "count": count, "total_ms": round2(total) })))
            .collect();

        // Breakdown percentages relative to request_total_ms (the
        // wall-clock the developer actually waited).
        let basis = request_total_ms;
        let breakdown_pct = if basis > 0.0 {
            let ext = (handlers_total_ms / basis) * 100.0;
            let ov = workflow_overhead_ms
                .map(|v| (v / basis) * 100.0)
                .unwrap_or(0.0);
            let ts = trace_store_ms.map(|v| (v / basis) * 100.0).unwrap_or(0.0);
            let lw = engine_lock_wait_ms
                .map(|v| (v / basis) * 100.0)
                .unwrap_or(0.0);
            json!({
                "external_io": round2(ext),
                "workflow_overhead": round2(ov),
                "trace_store": round2(ts),
                "engine_lock_wait": round2(lw),
            })
        } else {
            json!({})
        };

        // Normalized `phases[]` view — same numbers as the per-phase
        // *_ms fields below, but iterable by clients that don't want to
        // hard-code each key. Each entry: { name, ms, pct } (pct
        // relative to `totals_ms` / `request_total_ms`).
        let basis_for_phase_pct = request_total_ms.max(0.0);
        let mut phases: Vec<Value> = Vec::with_capacity(4);
        let mut push_phase = |name: &'static str, ms: f64| {
            let pct = if basis_for_phase_pct > 0.0 {
                (ms / basis_for_phase_pct) * 100.0
            } else {
                0.0
            };
            phases.push(json!({
                "name": name,
                "ms": round2(ms),
                "pct": round2(pct),
            }));
        };
        if let Some(v) = engine_lock_wait_ms {
            push_phase("engine_lock_wait", v);
        }
        push_phase("handlers", handlers_total_ms);
        if let Some(v) = workflow_overhead_ms {
            push_phase("workflow_overhead", v);
        }
        if let Some(v) = trace_store_ms {
            push_phase("trace_store", v);
        }

        let mut out = json!({
            "version": Self::PROFILE_VERSION,
            "totals_ms": round2(request_total_ms),
            "phases": Value::Array(phases),
            "request_total_ms": round2(request_total_ms),
            "handlers_total_ms": round2(handlers_total_ms),
            "handlers": handlers_json,
            "by_function": Value::Object(by_function_json),
            "by_connector": Value::Object(by_connector_json),
            "breakdown_pct": breakdown_pct,
        });
        if let Some(v) = workflow_total_ms {
            out["workflow_total_ms"] = json!(round2(v));
        }
        if let Some(v) = workflow_overhead_ms {
            out["workflow_overhead_ms"] = json!(round2(v));
        }
        if let Some(v) = engine_lock_wait_ms {
            out["engine_lock_wait_ms"] = json!(round2(v));
        }
        if let Some(v) = trace_store_ms {
            out["trace_store_ms"] = json!(round2(v));
        }
        out
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

task_local! {
    /// Per-request profile collector. Set via `ORION_PROFILE.scope(...)`
    /// in the data route handler when profiling is on; read by [`record`]
    /// from every Orion custom-handler call.
    pub static ORION_PROFILE: std::sync::Arc<ProfileCollector>;
}

/// Wrap a handler body, recording its duration into the per-request
/// collector when one is in scope. Off-path is a single `try_with`
/// returning `Err` plus the original `fut.await` — no allocation.
pub async fn record<F, T>(function: &'static str, connector: Option<&str>, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let collector = match ORION_PROFILE.try_with(|c| c.clone()) {
        Ok(c) => c,
        Err(_) => return fut.await,
    };

    let depth = collector.depth.fetch_add(1, Ordering::Relaxed);
    let start = Instant::now();
    let result = fut.await;
    let elapsed = start.elapsed();
    collector.depth.fetch_sub(1, Ordering::Relaxed);

    let connector_owned = connector.map(str::to_owned);
    collector
        .samples
        .lock()
        .expect("profile mutex")
        .push(HandlerSample {
            function,
            connector: connector_owned,
            duration: elapsed,
            depth,
        });

    result
}

/// Convenience: if a collector is in scope, append the given lock-wait
/// duration to it. Used by `engine::acquire_engine_read`.
pub fn record_engine_lock_wait(d: Duration) {
    let _ = ORION_PROFILE.try_with(|c| c.set_engine_lock_wait(d));
}

/// True when a collector is in scope for the current task.
pub fn is_active() -> bool {
    ORION_PROFILE.try_with(|_| ()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_noop_when_disabled() {
        let v = record("test", None, async { 42 }).await;
        assert_eq!(v, 42);
    }

    #[tokio::test]
    async fn record_captures_sample_when_active() {
        let collector = ProfileCollector::new();
        ORION_PROFILE
            .scope(collector.clone(), async {
                record("http_call", Some("svc_a"), async {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                })
                .await;
            })
            .await;
        let samples = collector.samples.lock().expect("test").clone();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].function, "http_call");
        assert_eq!(samples[0].connector.as_deref(), Some("svc_a"));
        assert_eq!(samples[0].depth, 0);
    }

    #[tokio::test]
    async fn nested_record_increments_depth() {
        let collector = ProfileCollector::new();
        ORION_PROFILE
            .scope(collector.clone(), async {
                record("channel_call", None, async {
                    record("db_read", Some("db1"), async {}).await;
                })
                .await;
            })
            .await;
        let samples = collector.samples.lock().expect("test").clone();
        assert_eq!(samples.len(), 2);
        // db_read finished first (inner), pushed first; depth=1
        assert_eq!(samples[0].function, "db_read");
        assert_eq!(samples[0].depth, 1);
        // channel_call finished after, pushed second; depth=0
        assert_eq!(samples[1].function, "channel_call");
        assert_eq!(samples[1].depth, 0);
    }

    #[tokio::test]
    async fn to_json_shape() {
        let collector = ProfileCollector::new();
        ORION_PROFILE
            .scope(collector.clone(), async {
                record("http_call", Some("svc_a"), async {}).await;
                record("db_read", Some("db1"), async {}).await;
            })
            .await;
        collector.set_workflow_total(Duration::from_millis(10));
        collector.set_engine_lock_wait(Duration::from_micros(50));
        collector.set_trace_store(Duration::from_millis(1));

        let v = collector.to_json();
        // B3 shape lock: top-level version + totals_ms + iterable phases[].
        assert_eq!(v["version"], 1);
        assert!(v["totals_ms"].as_f64().expect("test") > 0.0);
        let phases = v["phases"].as_array().expect("phases must be an array");
        let phase_names: Vec<&str> = phases.iter().filter_map(|p| p["name"].as_str()).collect();
        // All four phases (set above) should appear in order.
        assert!(phase_names.contains(&"engine_lock_wait"));
        assert!(phase_names.contains(&"handlers"));
        assert!(phase_names.contains(&"workflow_overhead"));
        assert!(phase_names.contains(&"trace_store"));
        // Detail fields preserved.
        assert!(v["handlers"].is_array());
        assert_eq!(v["handlers"].as_array().expect("test").len(), 2);
        assert!(
            v["by_function"]["http_call"]["count"]
                .as_u64()
                .expect("test")
                >= 1
        );
        assert!(v["by_connector"]["svc_a"]["count"].as_u64().expect("test") >= 1);
        assert!(v["workflow_total_ms"].as_f64().expect("test") > 0.0);
        assert!(v["workflow_overhead_ms"].as_f64().expect("test") >= 0.0);
    }
}
