//! Package admission: the [`PackageVerifier`] an edition plugs in, and what
//! it is asked and answers.

use serde::{Deserialize, Serialize};

use crate::contract::ImplementationKind;
use crate::identity::ProviderRef;

/// Package admission request checked at install/deployment time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageAdmissionRequest {
    pub provider: ProviderRef,
    pub implementation: ImplementationKind,
    /// Driver identifier for `rust` providers, entry workflow for `workflow`, or component path for `wasm`.
    pub target_id: String,
    /// The distribution tier the package declares.
    pub tier: EditionTier,
    /// Optional detached signature payload verified by Enterprise admission policies.
    pub signature: Option<String>,
}

/// Distribution tier per `DEC-005`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditionTier {
    Community,
    Enterprise,
}

impl EditionTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Community => "community",
            Self::Enterprise => "enterprise",
        }
    }

    /// The tier named `name`, or `None` for a name that is not one.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "community" => Some(Self::Community),
            "enterprise" => Some(Self::Enterprise),
            _ => None,
        }
    }
}

/// Successful admission verdict returned by [`PackageVerifier::verify_package`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionVerdict {
    pub provider: ProviderRef,
    pub implementation: ImplementationKind,
    pub policy_id: String,
}

/// Package admission rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionRejection {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for AdmissionRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AdmissionRejection {}

/// The part of admission an edition plugs in: whether a provider package may
/// be admitted on this node.
///
/// Community composes its own; Enterprise supplies its (signed packages,
/// offline entitlements) through [`Registrar::package_verifier`](crate::Registrar::package_verifier).
/// Revocation and the security lease are not the verifier's: every node keeps
/// them in its revocation ledger and consults it before and after the
/// verifier, so a verifier cannot disable revocation by omission.
pub trait PackageVerifier: Send + Sync {
    /// Policy identifier for diagnostics, audit receipts and the admission
    /// fingerprint: an admission recorded under another policy is
    /// invalidated.
    fn policy_id(&self) -> &'static str;

    /// Validate a provider package before admission into a deployment generation.
    fn verify_package(
        &self,
        request: &PackageAdmissionRequest,
    ) -> Result<AdmissionVerdict, AdmissionRejection>;
}
