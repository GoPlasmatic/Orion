//! Package admission: the [`PackageVerifier`] an edition plugs in, and what
//! it is asked and answers.

use serde::{Deserialize, Serialize};

pub use orion_api::v2::EditionTier;

use crate::contract::ImplementationKind;
use crate::identity::ProviderRef;

/// Rejection codes the host itself uses. A verifier may add codes of its own.
pub mod admission_codes {
    pub const INVALID_PROVIDER_ID: &str = "PACKAGE_INVALID_PROVIDER_ID";
    pub const INVALID_TARGET: &str = "PACKAGE_INVALID_TARGET";
    pub const TIER_NOT_ENABLED: &str = "ENTERPRISE_TIER_NOT_ENABLED";
    pub const PROVIDER_REVOKED: &str = "PROVIDER_REVOKED";
}

/// Package admission request checked at install/deployment time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PackageAdmissionRequest {
    pub provider: ProviderRef,
    pub implementation: ImplementationKind,
    /// Driver identifier for `rust` providers, entry workflow for `workflow`, or component path for `wasm`.
    pub target_id: String,
    /// The distribution tier the package declares.
    pub tier: EditionTier,
    /// Optional detached signature payload, for a verifier that checks one.
    pub signature: Option<String>,
}

impl PackageAdmissionRequest {
    /// A request to admit `provider`, unsigned.
    pub fn new(
        provider: ProviderRef,
        implementation: ImplementationKind,
        target_id: impl Into<String>,
        tier: EditionTier,
    ) -> Self {
        Self {
            provider,
            implementation,
            target_id: target_id.into(),
            tier,
            signature: None,
        }
    }

    /// Carry a detached signature.
    #[must_use]
    pub fn with_signature(mut self, signature: impl Into<String>) -> Self {
        self.signature = Some(signature.into());
        self
    }
}

/// Successful admission verdict returned by [`PackageVerifier::verify_package`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AdmissionVerdict {
    pub provider: ProviderRef,
    pub implementation: ImplementationKind,
    pub policy_id: String,
}

impl AdmissionVerdict {
    /// Admit `provider` under the policy `policy_id`.
    pub fn new(
        provider: ProviderRef,
        implementation: ImplementationKind,
        policy_id: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            implementation,
            policy_id: policy_id.into(),
        }
    }
}

/// Package admission rejection. `code` is one of [`admission_codes`] or a code
/// the verifier's own policy defines; the message is for a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AdmissionRejection {
    pub code: String,
    pub message: String,
}

impl AdmissionRejection {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
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
    fn policy_id(&self) -> &str;

    /// Validate a provider package before admission into a deployment generation.
    fn verify_package(
        &self,
        request: &PackageAdmissionRequest,
    ) -> Result<AdmissionVerdict, AdmissionRejection>;
}
