//! Package lifecycle logic shared by the `orion-server package` CLI and
//! anything else that applies a package: what an artifact carries, and
//! whether the target is serving it, and what `--prune` removes.
//!
//! A top layer, beside `bootstrap`: nothing below the HTTP layer may name
//! it (`module_layering_test`).

pub mod prune;
pub mod verify;

pub use prune::{
    Baseline, PruneMode, PrunePlan, References, Refusal, Removal, prune_plan, refusals,
};
pub use verify::{ChannelMember, PackageMembers, QuarantinedEntity, quarantined_members};
