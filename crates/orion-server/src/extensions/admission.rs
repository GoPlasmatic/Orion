//! Package admission and runtime security-lease policy (`V2-11.1`, `V2-05.5`, `CTX-09`).
//!
//! Split in two: the [`PackageVerifier`] an edition plugs in (Community's
//! [`CommunityPackageVerifier`], or Enterprise's license, signature and
//! entitlement checks), and the [`RevocationLedger`] every node keeps, which
//! enforces monotonic runtime revocation across all live generations
//! (`CTX-09`). [`Admission`] is the two together.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use serde::{Deserialize, Serialize};

use super::contract::{
    ConnectorError, ConnectorErrorClass, EffectStatus, ErrorOrigin, ImplementationKind,
    ProviderReference, RetryAdvice, codes,
};

/// Package admission request checked at install/deployment time (`V2-11.1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageAdmissionRequest {
    pub provider: ProviderReference,
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
    pub provider: ProviderReference,
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
/// be admitted on this node (`V2-11.1`).
///
/// Community composes [`CommunityPackageVerifier`]; Enterprise supplies its
/// own (signed packages, offline entitlements) through
/// [`super::CompositionBuilder::with_package_verifier`]. Revocation and the
/// security lease are not the verifier's: every node keeps them in its
/// [`RevocationLedger`], and [`Admission`] consults the ledger before and
/// after the verifier, so a verifier cannot disable revocation by omission.
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

/// Community Edition package verifier (`V2-11.1`).
///
/// - Verifies canonical `sha256:<64 hex>` package digests and non-empty targets.
/// - Rejects packages explicitly marked `tier: "enterprise"`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommunityPackageVerifier;

impl CommunityPackageVerifier {
    pub const POLICY_ID: &'static str = "orion.community.admission/1";
}

fn is_valid_sha256_digest(digest: &str) -> bool {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl PackageVerifier for CommunityPackageVerifier {
    fn policy_id(&self) -> &'static str {
        Self::POLICY_ID
    }

    fn verify_package(
        &self,
        request: &PackageAdmissionRequest,
    ) -> Result<AdmissionVerdict, AdmissionRejection> {
        if request.provider.provider_id.trim().is_empty() {
            return Err(AdmissionRejection {
                code: "PACKAGE_INVALID_PROVIDER_ID".to_string(),
                message: "Provider ID must not be empty".to_string(),
            });
        }
        if !is_valid_sha256_digest(&request.provider.package_digest) {
            return Err(AdmissionRejection {
                code: "PACKAGE_INVALID_DIGEST".to_string(),
                message: format!(
                    "Package digest '{}' must be canonical sha256:<64 lowercase hex>",
                    request.provider.package_digest
                ),
            });
        }
        if request.target_id.trim().is_empty() {
            return Err(AdmissionRejection {
                code: "PACKAGE_INVALID_TARGET".to_string(),
                message: "Implementation target_id must not be empty".to_string(),
            });
        }
        if request.tier == EditionTier::Enterprise {
            return Err(AdmissionRejection {
                code: "ENTERPRISE_TIER_NOT_ENABLED".to_string(),
                message: format!(
                    "Provider '{}' requires Enterprise admission policy",
                    request.provider.provider_id
                ),
            });
        }
        Ok(AdmissionVerdict {
            provider: request.provider.clone(),
            implementation: request.implementation,
            policy_id: self.policy_id().to_string(),
        })
    }
}

/// The node's monotonic runtime deny-set and security lease (`CTX-09`).
///
/// Core state every node keeps whatever its edition: revocations take effect
/// immediately across all retained generations, and an expired lease blocks
/// new external work. One per node, shared by [`Admission`] and the
/// [`super::ProviderRegistry`] that dispatches connector calls.
#[derive(Debug)]
pub struct RevocationLedger {
    revoked_digests: RwLock<BTreeSet<String>>,
    revoked_providers: RwLock<BTreeSet<String>>,
    revoked_connections: RwLock<BTreeSet<String>>,
    lease_valid: AtomicBool,
    watermark: AtomicU64,
}

impl Default for RevocationLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl RevocationLedger {
    pub fn new() -> Self {
        Self {
            revoked_digests: RwLock::new(BTreeSet::new()),
            revoked_providers: RwLock::new(BTreeSet::new()),
            revoked_connections: RwLock::new(BTreeSet::new()),
            lease_valid: AtomicBool::new(true),
            watermark: AtomicU64::new(0),
        }
    }

    /// Add `value` to `set`. A poisoned lock still takes the write: a
    /// revocation that was dropped would fail open.
    fn revoke_into(&self, set: &RwLock<BTreeSet<String>>, value: &str) -> u64 {
        set.write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(value.to_string());
        self.watermark.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Whether `set` holds `value`, reading through a poisoned lock.
    fn revoked_in(set: &RwLock<BTreeSet<String>>, value: &str) -> bool {
        set.read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(value)
    }

    pub fn watermark(&self) -> u64 {
        self.watermark.load(Ordering::SeqCst)
    }

    /// Monotonically revoke a package digest. Returns the new watermark.
    pub fn revoke_digest(&self, digest: &str) -> u64 {
        self.revoke_into(&self.revoked_digests, digest)
    }

    /// Whether a package digest has been revoked.
    pub fn is_digest_revoked(&self, digest: &str) -> bool {
        Self::revoked_in(&self.revoked_digests, digest)
    }

    /// Monotonically revoke a provider ID. Returns the new watermark.
    pub fn revoke_provider(&self, provider_id: &str) -> u64 {
        self.revoke_into(&self.revoked_providers, provider_id)
    }

    /// Monotonically revoke a connection ID. Returns the new watermark.
    pub fn revoke_connection(&self, connection_id: &str) -> u64 {
        self.revoke_into(&self.revoked_connections, connection_id)
    }

    /// Update the node's runtime security lease validity. Returns the new
    /// watermark.
    pub fn set_lease_valid(&self, valid: bool) -> u64 {
        self.lease_valid.store(valid, Ordering::SeqCst);
        self.watermark.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Check the deny-set and the security lease before child dispatch or an
    /// external connector operation (`V2-05.5`, `CTX-09`).
    ///
    /// Generation pinning does not freeze an emergency revocation or an
    /// expired lease: an old generation consults this before starting new
    /// external work.
    pub fn check_runtime_lease(
        &self,
        _generation_id: u64,
        provider_id: &str,
        connection_id: &str,
        prior_effect: EffectStatus,
    ) -> Result<(), ConnectorError> {
        if !self.lease_valid.load(Ordering::SeqCst) {
            return Err(ConnectorError {
                code: codes::CONNECTION_FORBIDDEN.to_string(),
                class: ConnectorErrorClass::Forbidden,
                message: "Security lease has expired; new external operations are blocked"
                    .to_string(),
                effect_status: prior_effect,
                retry_advice: RetryAdvice::Never,
                origin: ErrorOrigin::host("admission"),
                retry_after_ms: None,
                details: None,
            });
        }
        if Self::revoked_in(&self.revoked_providers, provider_id) {
            return Err(ConnectorError::forbidden(
                codes::CONNECTION_FORBIDDEN,
                "admission",
                format!("Provider '{provider_id}' has been revoked"),
                prior_effect,
            ));
        }
        if Self::revoked_in(&self.revoked_connections, connection_id) {
            return Err(ConnectorError::forbidden(
                codes::CONNECTION_FORBIDDEN,
                "admission",
                format!("Connection '{connection_id}' has been revoked"),
                prior_effect,
            ));
        }
        Ok(())
    }
}

/// The node's admission: the edition's [`PackageVerifier`] over the node's
/// [`RevocationLedger`]. What the deployment store and the admin routes
/// admit, roll out and roll back under.
#[derive(Clone)]
pub struct Admission {
    verifier: Arc<dyn PackageVerifier>,
    ledger: Arc<RevocationLedger>,
}

impl std::fmt::Debug for Admission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admission")
            .field("policy_id", &self.policy_id())
            .field("watermark", &self.ledger.watermark())
            .finish()
    }
}

impl Admission {
    pub fn new(verifier: Arc<dyn PackageVerifier>, ledger: Arc<RevocationLedger>) -> Self {
        Self { verifier, ledger }
    }

    /// Community's verifier over a fresh ledger.
    pub fn community() -> Self {
        Self::new(
            Arc::new(CommunityPackageVerifier),
            Arc::new(RevocationLedger::new()),
        )
    }

    /// The verifier's policy identifier.
    pub fn policy_id(&self) -> &'static str {
        self.verifier.policy_id()
    }

    /// The node's revocation ledger.
    pub fn ledger(&self) -> &Arc<RevocationLedger> {
        &self.ledger
    }

    /// The verifier's verdict on `request`, refused when its digest is
    /// revoked on this node whatever the verifier says.
    pub fn verify_package(
        &self,
        request: &PackageAdmissionRequest,
    ) -> Result<AdmissionVerdict, AdmissionRejection> {
        let verdict = self.verifier.verify_package(request)?;
        if self
            .ledger
            .is_digest_revoked(&request.provider.package_digest)
        {
            return Err(AdmissionRejection {
                code: "PROVIDER_REVOKED".to_string(),
                message: format!(
                    "Provider package '{}' ({}) has been revoked",
                    request.provider.provider_id, request.provider.package_digest
                ),
            });
        }
        Ok(verdict)
    }

    /// [`RevocationLedger::check_runtime_lease`].
    pub fn check_runtime_lease(
        &self,
        generation_id: u64,
        provider_id: &str,
        connection_id: &str,
        prior_effect: EffectStatus,
    ) -> Result<(), ConnectorError> {
        self.ledger
            .check_runtime_lease(generation_id, provider_id, connection_id, prior_effect)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// A revocation lands, and is seen, even through a lock a panicking
    /// writer poisoned: dropping it would fail open.
    #[test]
    fn revocation_holds_through_a_poisoned_lock() {
        let policy = std::sync::Arc::new(RevocationLedger::new());
        let poisoner = std::sync::Arc::clone(&policy);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.revoked_digests.write().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(policy.revoked_digests.is_poisoned());
        policy.revoke_digest("sha256:dead");
        assert!(policy.is_digest_revoked("sha256:dead"));
    }

    #[test]
    fn a_tier_is_one_of_two_names() {
        assert_eq!(
            EditionTier::parse("community"),
            Some(EditionTier::Community)
        );
        assert_eq!(
            EditionTier::parse("enterprise"),
            Some(EditionTier::Enterprise)
        );
        assert_eq!(EditionTier::parse("enterprize"), None);
    }

    #[test]
    fn ctx_09_revocation_and_lease_expiry_block_live_old_generation() {
        let policy = RevocationLedger::new();

        // Generation 41 is live and initially allowed.
        assert!(
            policy
                .check_runtime_lease(41, "orion.http", "conn-1", EffectStatus::NotStarted)
                .is_ok()
        );

        // Revoking a connection blocks generation 41 immediately.
        policy.revoke_connection("conn-1");
        let err = policy
            .check_runtime_lease(41, "orion.http", "conn-1", EffectStatus::Unknown)
            .expect_err("revoked connection must fail");
        assert_eq!(err.code, codes::CONNECTION_FORBIDDEN);
        assert_eq!(err.effect_status, EffectStatus::Unknown);

        // Expiring the security lease blocks even unrevoked connections on generation 41.
        policy.set_lease_valid(false);
        let err = policy
            .check_runtime_lease(41, "orion.http", "conn-2", EffectStatus::NotStarted)
            .expect_err("expired lease must block new work");
        assert_eq!(err.code, codes::CONNECTION_FORBIDDEN);
    }
}
