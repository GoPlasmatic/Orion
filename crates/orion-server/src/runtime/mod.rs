//! Process-level runtime concerns: the things that own a *node* rather than a
//! request.
//!
//! The first resident is [`tasks`], the supervisor for the long-lived
//! background tasks. It lives here rather than in `queue/` because it is not
//! about traces: the trace dispatcher, the persistence workers, the audit
//! writer, the retention jobs and the cluster epoch watcher are owned by four
//! different modules, and "is every one of them still alive?" is a question
//! about the node.

pub mod tasks;

pub use tasks::{Criticality, Shutdown, TaskGuard, TaskRegistry, TaskReport, TaskState};
