//! The trace-capture policy every transport shares.

/// A trace's snapshot budget, when a channel opted into per-task capture.
#[derive(Debug, Clone, Copy)]
pub struct TraceCapture {
    /// Approximate in-memory budget for the trace's changes. Pass
    /// `queue.max_result_size_bytes`: it is the exact limit the serialized row
    /// is checked against afterwards, so using it here makes the post-hoc cap a
    /// backstop that should essentially never fire. `0` is unbounded, matching
    /// the result cap's own semantics.
    pub max_snapshot_bytes: usize,
}
