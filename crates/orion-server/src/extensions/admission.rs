//! Package admission and runtime security-lease policy (`V2-11.1`, `V2-05.5`, `CTX-09`).
//!
//! Separates the public Community trust policy ([`CommunityAdmissionPolicy`])
//! from Enterprise license, signature, and entitlement verification while
//! enforcing monotonic runtime revocation across all live generations (`CTX-09`).

use std::collections::BTreeSet;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
    /// Optional distribution tier tag (`"community"` or `"enterprise"`).
    pub tier: Option<String>,
    /// Optional detached signature payload verified by Enterprise admission policies.
    pub signature: Option<String>,
}

/// Successful admission verdict returned by [`AdmissionPolicy::verify_package`].
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

/// Open policy interface for package admission and runtime revocation/lease checks (`V2-11.1`, `CTX-09`).
///
/// Community uses [`CommunityAdmissionPolicy`]; Enterprise supplies its own
/// implementation at the composition root to verify signed packages, entitlements,
/// and fleet license leases without altering core engine dispatch.
pub trait AdmissionPolicy: Send + Sync {
    /// Policy identifier for diagnostics and audit receipts.
    fn policy_id(&self) -> &'static str;

    /// Validate a provider package before admission into a deployment generation.
    fn verify_package(
        &self,
        request: &PackageAdmissionRequest,
    ) -> Result<AdmissionVerdict, AdmissionRejection>;

    /// Check the monotonic runtime deny-set and security lease before child dispatch
    /// or external connector operations (`V2-05.5`, `CTX-09`).
    ///
    /// Normal generation pinning does not freeze emergency revocations or expired
    /// security leases: an old generation must consult this operational check
    /// before starting new external work.
    fn check_runtime_lease(
        &self,
        generation_id: u64,
        provider_id: &str,
        connection_id: &str,
        prior_effect: EffectStatus,
    ) -> Result<(), ConnectorError>;

    /// Monotonically revoke a package digest across all active and retained generations (`CTX-09`).
    fn revoke_digest(&self, _digest: &str) -> u64 {
        0
    }

    /// Check whether a package digest has been monotonically revoked (`CTX-09`).
    fn is_digest_revoked(&self, _digest: &str) -> bool {
        false
    }

    /// Monotonically revoke a provider ID across all active and retained generations (`CTX-09`).
    fn revoke_provider(&self, _provider_id: &str) -> u64 {
        0
    }

    /// Monotonically revoke a connection ID across all active and retained generations (`CTX-09`).
    fn revoke_connection(&self, _connection_id: &str) -> u64 {
        0
    }

    /// Update the node's runtime security lease validity (`CTX-09`).
    fn set_lease_valid(&self, _valid: bool) -> u64 {
        0
    }
}

/// Community Edition admission and runtime revocation policy (`V2-11.1`, `CTX-09`).
///
/// - Verifies canonical `sha256:<64 hex>` package digests and non-empty targets.
/// - Rejects packages explicitly marked `tier: "enterprise"` on a Community node.
/// - Maintains a monotonic runtime deny-set and lease validity state so `CTX-09`
///   revocations take effect immediately across all retained generations.
pub struct CommunityAdmissionPolicy {
    revoked_digests: RwLock<BTreeSet<String>>,
    revoked_providers: RwLock<BTreeSet<String>>,
    revoked_connections: RwLock<BTreeSet<String>>,
    lease_valid: AtomicBool,
    watermark: AtomicU64,
}

impl Default for CommunityAdmissionPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl CommunityAdmissionPolicy {
    pub fn new() -> Self {
        Self {
            revoked_digests: RwLock::new(BTreeSet::new()),
            revoked_providers: RwLock::new(BTreeSet::new()),
            revoked_connections: RwLock::new(BTreeSet::new()),
            lease_valid: AtomicBool::new(true),
            watermark: AtomicU64::new(0),
        }
    }

    /// Monotonically revoke a package digest across all active and retained generations (`CTX-09`).
    pub fn revoke_digest(&self, digest: impl Into<String>) -> u64 {
        if let Ok(mut set) = self.revoked_digests.write() {
            set.insert(digest.into());
        }
        self.watermark.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Check whether a package digest has been monotonically revoked (`CTX-09`).
    pub fn is_digest_revoked(&self, digest: &str) -> bool {
        self.revoked_digests
            .read()
            .is_ok_and(|set| set.contains(digest))
    }

    /// Monotonically revoke a provider ID across all active and retained generations (`CTX-09`).
    pub fn revoke_provider(&self, provider_id: impl Into<String>) -> u64 {
        if let Ok(mut set) = self.revoked_providers.write() {
            set.insert(provider_id.into());
        }
        self.watermark.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Monotonically revoke a connection ID across all active and retained generations (`CTX-09`).
    pub fn revoke_connection(&self, connection_id: impl Into<String>) -> u64 {
        if let Ok(mut set) = self.revoked_connections.write() {
            set.insert(connection_id.into());
        }
        self.watermark.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Mark the node's security lease as expired or restored (`CTX-09`).
    pub fn set_lease_valid(&self, valid: bool) -> u64 {
        self.lease_valid.store(valid, Ordering::SeqCst);
        self.watermark.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn watermark(&self) -> u64 {
        self.watermark.load(Ordering::SeqCst)
    }
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

impl AdmissionPolicy for CommunityAdmissionPolicy {
    fn policy_id(&self) -> &'static str {
        "orion.community.admission/1"
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
        if request
            .tier
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("enterprise"))
        {
            return Err(AdmissionRejection {
                code: "ENTERPRISE_TIER_NOT_ENABLED".to_string(),
                message: format!(
                    "Provider '{}' requires Enterprise admission policy",
                    request.provider.provider_id
                ),
            });
        }

        if self.is_digest_revoked(&request.provider.package_digest) {
            return Err(AdmissionRejection {
                code: "PROVIDER_REVOKED".to_string(),
                message: format!(
                    "Provider package '{}' ({}) has been revoked",
                    request.provider.provider_id, request.provider.package_digest
                ),
            });
        }

        Ok(AdmissionVerdict {
            provider: request.provider.clone(),
            implementation: request.implementation,
            policy_id: self.policy_id().to_string(),
        })
    }

    fn revoke_digest(&self, digest: &str) -> u64 {
        CommunityAdmissionPolicy::revoke_digest(self, digest)
    }

    fn is_digest_revoked(&self, digest: &str) -> bool {
        CommunityAdmissionPolicy::is_digest_revoked(self, digest)
    }

    fn revoke_provider(&self, provider_id: &str) -> u64 {
        CommunityAdmissionPolicy::revoke_provider(self, provider_id)
    }

    fn revoke_connection(&self, connection_id: &str) -> u64 {
        CommunityAdmissionPolicy::revoke_connection(self, connection_id)
    }

    fn set_lease_valid(&self, valid: bool) -> u64 {
        CommunityAdmissionPolicy::set_lease_valid(self, valid)
    }

    fn check_runtime_lease(
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
        if self
            .revoked_providers
            .read()
            .is_ok_and(|set| set.contains(provider_id))
        {
            return Err(ConnectorError::forbidden(
                codes::CONNECTION_FORBIDDEN,
                "admission",
                format!("Provider '{provider_id}' has been revoked"),
                prior_effect,
            ));
        }
        if self
            .revoked_connections
            .read()
            .is_ok_and(|set| set.contains(connection_id))
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctx_09_revocation_and_lease_expiry_block_live_old_generation() {
        let policy = CommunityAdmissionPolicy::new();

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
