//! A **definition set**: the channels, workflows and connectors that are
//! authored together and must be consistent with each other.
//!
//! One file at a time is not enough. `orion-cli lint wf.json` parses a
//! workflow, checks its shapes, and says "valid" — while the channel that
//! names it, the connector its tasks reach for, and the `channel_call` target
//! it invokes all sit in files the command never opens. A set can be green on
//! every per-file gate and still be missing a channel at runtime (#286).
//!
//! The cross-reference pass runs over a set, not over a container: a
//! directory of definitions and the one-file set `lint wf.json` builds around
//! a single workflow need the same checks, which is what this module
//! separates:
//!
//! ```text
//! from_directory() ─┐                          ┌─ lint / clippy
//!                   ├─ DefinitionSet ─┐        │
//! from_entries()   ─┘                 ├─ check() ─┤
//!                      Boundary ──────┘        └─ compile
//! ```
//!
//! Two loaders, one pass. A second validator beside the first is how two
//! forms of a set come to disagree about what a valid set is.
//!
//! ## What a [`Boundary`] is for
//!
//! Not every name a set references has to live inside it: `lint` and `clippy`
//! take `--requires-channel` / `--requires-connector` for channels and
//! connectors expected to exist elsewhere, so closure checking does not fail
//! on them.
//!
//! The default is **empty** — everything must resolve in-set, and `compile`
//! always uses it — because the gate exists to catch the missing channel, and
//! a permissive default would be a gate that passes the bug it was built for.

pub mod analysis;
mod check;
pub mod clippy;
pub mod compile;
pub mod deployment;
mod diagnostic;
pub mod documents;
mod expand;
pub mod fix;
pub mod fmt;
mod gate;
mod set;
mod shared;
pub mod statements;

pub use orion_definitions::{json, provenance};

pub use check::check;
pub use compile::{Cx, Pass, Residue};
pub use diagnostic::{Diagnostic, Severity};
pub use gate::{GateOpts, GateReport, gate_directory};
pub use json::{Document, Node, Span, Spanned};
pub use provenance::{SourceMap, SourceRef, Via};
pub use set::{
    Boundary, Definition, DefinitionSet, Entity, LoadReport, PluginDefinition, is_plugin_manifest,
    json_files,
};
pub use shared::{Fragment, FragmentBody, PackageDecl, SharedDefinitions, first_reference};
