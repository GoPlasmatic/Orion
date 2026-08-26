//! A **definition set**: the channels, workflows and connectors that are
//! authored together and must be consistent with each other.
//!
//! Orion validated one file at a time offline. `orion-server lint wf.json`
//! parses a workflow, checks its shapes, and says "valid" — while the channel
//! that names it, the connector its tasks reach for, and the `channel_call`
//! target it invokes all sit in files the command never opened. A set can be
//! green on every per-file gate and still be missing a channel at runtime
//! (#286).
//!
//! The cross-reference pass that closes that already existed, in
//! `package_cli::run_lint`, over a *promotion artifact*. It was the only
//! consumer, so it owned the walk. A directory of definitions needs the same
//! checks against a different container, which is what this module separates:
//!
//! ```text
//! from_artifact()  ─┐                          ┌─ package lint
//!                   ├─ DefinitionSet ─┐        │
//! from_directory() ─┘                 ├─ check() ─┤
//!                      Boundary ──────┘        └─ lint <dir>
//! ```
//!
//! Two loaders, one pass. The alternative — a second validator beside the
//! first — is how the artifact form and the directory form come to disagree
//! about what a valid set is, which is worse than having only one of them.
//!
//! ## What a [`Boundary`] is for
//!
//! Not every name a set references has to live inside it. A promotion artifact
//! says so explicitly in `requires`: these channels and connectors are expected
//! to exist on the target already, so closure checking must not fail on them. A
//! directory needs the same escape hatch for the same reason, so the concept is
//! lifted out of the artifact and given to both.
//!
//! The *default* differs, and deliberately. An artifact's boundary is whatever
//! it declares. A directory's is **empty** — everything must resolve in-set —
//! because the gate exists to catch the missing channel, and a permissive
//! default would be a gate that passes the bug it was built for.

mod check;
pub mod compile;
mod finding;
mod set;
mod shared;

pub use check::check;
pub use compile::{Cx, Pass, Residue};
pub use finding::{Finding, Severity};
pub use set::{Boundary, DefinitionSet, Entity, LoadReport};
pub use shared::{Fragment, SharedDefinitions, first_reference};
