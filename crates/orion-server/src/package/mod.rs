//! Package lifecycle logic shared by the `orion-server package` CLI and
//! the server's own boot-time apply: what an artifact carries, how it is
//! linted, signed, applied and verified serving, and what `--prune`
//! removes.
//!
//! One apply engine over one transport trait, [`AdminApi`]: the CLI drives
//! it over HTTP with an `OrionClient`, the server through its own admin
//! router in-process ([`InProcessAdmin`]) — same requests, same handlers,
//! same gates and audit rows either way.
//!
//! A top layer, beside `bootstrap`: nothing below the HTTP layer may name
//! it (`module_layering_test`).

pub mod apply;
pub mod artifact;
pub mod lint;
pub mod prune;
pub mod sign;
pub mod transport;
pub mod verify;

pub use apply::{ApplyOptions, ApplyOutcome, apply};
pub use artifact::{
    ModelRequirement, PackageArtifact, PackageMeta, PluginRequirement, Requires,
    artifact_content_hash, read_artifact, verify_hash,
};
pub use lint::{LintReport, lint_artifact};
pub use prune::{
    Baseline, PruneMode, PrunePlan, References, Refusal, Removal, prune_plan, refusals,
};
pub use transport::{AdminApi, InProcessAdmin};
pub use verify::{ChannelMember, PackageMembers, QuarantinedEntity, quarantined_members};

/// A package operation's failure: the line an operator reads, with the
/// cause's own source chain kept for a caller that prints it.
#[derive(Debug)]
pub struct Error(Box<dyn std::error::Error + Send + Sync>);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self(message.into())
    }
}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self(message.into())
    }
}

macro_rules! error_from {
    ($($t:ty),+ $(,)?) => {$(
        impl From<$t> for Error {
            fn from(e: $t) -> Self {
                Self(Box::new(e))
            }
        }
    )+};
}

error_from!(orion_client::ClientError, serde_json::Error, std::io::Error,);

/// Where an operation's progress goes. The CLI prints `out` to stdout and
/// `err` to stderr; the server's boot-time apply logs them. `err` lines
/// carry their own `error: ` or `warning: ` prefix.
pub trait Reporter: Send + Sync {
    fn out(&self, line: &str);
    fn err(&self, line: &str);
}

/// The CLI's reporter: stdout and stderr, line by line.
pub struct Console;

impl Reporter for Console {
    fn out(&self, line: &str) {
        println!("{line}");
    }

    fn err(&self, line: &str) {
        eprintln!("{line}");
    }
}
