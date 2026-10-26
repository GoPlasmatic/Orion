//! What a deployment store operation refuses with: a [`DeploymentErrorKind`]
//! the code matches on, and the wire `code` the admin API answers.

/// How a refusal is answered: the HTTP class the admin API maps it to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// `404`: a package, admission, revision or connection that is not stored.
    NotFound,
    /// `409`: an immutable row stored differently, a reused idempotency key,
    /// or a head another writer moved first.
    Conflict,
    /// `403`: a provider that is not admitted, or no longer is.
    Forbidden,
    /// `503`: the state database could not be read or written.
    Unavailable,
    /// `500`: a stored row the store cannot read back.
    Corrupt,
    /// `400`: everything else — the request itself is refused.
    Invalid,
}

macro_rules! kinds {
    ($( $(#[$meta:meta])* $variant:ident = $code:literal => $class:ident, )*) => {
        /// Every refusal the deployment store, its planner and the node
        /// reconciler raise, each with its stable wire code.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum DeploymentErrorKind {
            $( $(#[$meta])* $variant, )*
            /// A code raised by another layer (a package or schema finding,
            /// an admission policy refusal, a build issue) and passed
            /// through as it was raised.
            Other,
        }

        impl DeploymentErrorKind {
            /// The wire code; empty for [`Self::Other`], whose code is the
            /// one the error carries.
            pub const fn code(self) -> &'static str {
                match self {
                    $( Self::$variant => $code, )*
                    Self::Other => "",
                }
            }

            /// The HTTP class this kind is answered with.
            pub const fn class(self) -> ErrorClass {
                match self {
                    $( Self::$variant => ErrorClass::$class, )*
                    Self::Other => ErrorClass::Invalid,
                }
            }

            /// The kind `code` names, when it is one of the store's own.
            /// A code from another layer can coincide with one (an admission
            /// policy answers `PROVIDER_REVOKED`), and is then answered the
            /// same way.
            pub fn from_code(code: &str) -> Option<Self> {
                match code {
                    $( $code => Some(Self::$variant), )*
                    _ => None,
                }
            }
        }
    };
}

kinds! {
    PackageNotFound = "PACKAGE_NOT_FOUND" => NotFound,
    AdmissionNotFound = "ADMISSION_NOT_FOUND" => NotFound,
    BaseRevisionNotFound = "BASE_REVISION_NOT_FOUND" => NotFound,
    RollbackTargetNotFound = "ROLLBACK_TARGET_NOT_FOUND" => NotFound,
    ConnectionRevisionNotFound = "CONNECTION_REVISION_NOT_FOUND" => NotFound,

    PublisherVersionConflict = "PUBLISHER_VERSION_CONFLICT" => Conflict,
    ImmutableContentConflict = "IMMUTABLE_CONTENT_CONFLICT" => Conflict,
    ImmutableConnectionConflict = "IMMUTABLE_CONNECTION_CONFLICT" => Conflict,
    IdempotencyKeyConflict = "IDEMPOTENCY_KEY_CONFLICT" => Conflict,
    /// The head moved since the caller's base: another writer committed first.
    DeploymentHeadConflict = "DEPLOYMENT_HEAD_CONFLICT" => Conflict,
    ReviewedDigestMismatch = "REVIEWED_DIGEST_MISMATCH" => Conflict,

    ProviderNotAdmitted = "PROVIDER_NOT_ADMITTED" => Forbidden,
    ProviderRevoked = "PROVIDER_REVOKED" => Forbidden,
    AdmissionInvalidated = "ADMISSION_INVALIDATED" => Forbidden,

    DbTransaction = "DB_TRANSACTION_ERROR" => Unavailable,
    DbWrite = "DB_WRITE_ERROR" => Unavailable,
    DbCommit = "DB_COMMIT_ERROR" => Unavailable,
    DbRead = "DB_READ_ERROR" => Unavailable,

    CorruptPersistedState = "CORRUPT_PERSISTED_STATE" => Corrupt,

    ArchiveEntryCountExceeded = "ARCHIVE_ENTRY_COUNT_EXCEEDED" => Invalid,
    ArchiveSymlinkDenied = "ARCHIVE_SYMLINK_DENIED" => Invalid,
    ArchiveDuplicateEntry = "ARCHIVE_DUPLICATE_ENTRY" => Invalid,
    ArchiveCompressionRatioExceeded = "ARCHIVE_COMPRESSION_RATIO_EXCEEDED" => Invalid,
    ArchiveSizeLimitExceeded = "ARCHIVE_SIZE_LIMIT_EXCEEDED" => Invalid,
    ArchiveInventoryMissing = "ARCHIVE_INVENTORY_MISSING" => Invalid,
    ArchiveInventoryInvalid = "ARCHIVE_INVENTORY_INVALID" => Invalid,
    ManifestInvalidJson = "MANIFEST_INVALID_JSON" => Invalid,
    ManifestShapeInvalid = "MANIFEST_SHAPE_INVALID" => Invalid,
    DistributionInvalidJson = "DISTRIBUTION_INVALID_JSON" => Invalid,
    DistributionShapeInvalid = "DISTRIBUTION_SHAPE_INVALID" => Invalid,
    SchemaInvalidJson = "SCHEMA_INVALID_JSON" => Invalid,
    PackageInvalidTier = "PACKAGE_INVALID_TIER" => Invalid,
    ArtifactNotActive = "ARTIFACT_NOT_ACTIVE" => Invalid,
    ArtifactDrainingOrDeleted = "ARTIFACT_DRAINING_OR_DELETED" => Invalid,

    ConnectionRevisionInvalid = "CONNECTION_REVISION_INVALID" => Invalid,
    ConnectionPublicSecretForbidden = "CONNECTION_PUBLIC_SECRET_FORBIDDEN" => Invalid,
    ConnectionSecretLocatorInvalid = "CONNECTION_SECRET_LOCATOR_INVALID" => Invalid,
    ConnectionDigestMismatch = "CONNECTION_DIGEST_MISMATCH" => Invalid,
    ConnectionProviderNotDeclared = "CONNECTION_PROVIDER_NOT_DECLARED" => Invalid,

    ProviderMetadataMismatch = "PROVIDER_METADATA_MISMATCH" => Invalid,
    MemberDefinitionNotFound = "MEMBER_DEFINITION_NOT_FOUND" => Invalid,
    OwningPackageNotDeclared = "OWNING_PACKAGE_NOT_DECLARED" => Invalid,
    MemberArtifactNotInPackage = "MEMBER_ARTIFACT_NOT_IN_PACKAGE" => Invalid,
    RollbackTargetCompacted = "ROLLBACK_TARGET_COMPACTED" => Invalid,
    RequestHashInvalid = "REQUEST_HASH_INVALID" => Invalid,
    SerializeError = "SERIALIZE_ERROR" => Invalid,

    EntityRefInvalid = "ENTITY_REF_INVALID" => Invalid,
    ProviderRefInvalid = "PROVIDER_REF_INVALID" => Invalid,
    RolloutPartitionInvalid = "ROLLOUT_PARTITION_INVALID" => Invalid,
    ManifestVersionUnsupported = "MANIFEST_VERSION_UNSUPPORTED" => Invalid,
    ManifestDuplicateProvider = "MANIFEST_DUPLICATE_PROVIDER" => Invalid,
    ManifestOwnerMissing = "MANIFEST_OWNER_MISSING" => Invalid,
    ManifestOwnershipConflict = "MANIFEST_OWNERSHIP_CONFLICT" => Invalid,
    ManifestDuplicateSingleton = "MANIFEST_DUPLICATE_SINGLETON" => Invalid,
    ManifestDuplicateMember = "MANIFEST_DUPLICATE_MEMBER" => Invalid,
    ManifestMissingRequirement = "MANIFEST_MISSING_REQUIREMENT" => Invalid,
    ManifestCanonicalError = "MANIFEST_CANONICAL_ERROR" => Invalid,
    InvalidManifestJson = "INVALID_MANIFEST_JSON" => Invalid,
    DeploymentWouldNotBuild = "DEPLOYMENT_WOULD_NOT_BUILD" => Invalid,

    CandidateRevisionMissing = "CANDIDATE_REVISION_MISSING" => Invalid,
    CandidateSuperseded = "CANDIDATE_SUPERSEDED" => Invalid,
}

/// A refusal: its [`kind`](Self::kind), and the wire `code` and `message`
/// the admin API answers with. For every kind but
/// [`DeploymentErrorKind::Other`] the code is the kind's own.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct DeploymentError {
    pub kind: DeploymentErrorKind,
    pub code: String,
    pub message: String,
}

impl DeploymentError {
    pub fn new(kind: DeploymentErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            code: kind.code().to_string(),
            message: message.into(),
        }
    }

    /// A refusal raised by another layer under its own `code`, kept as it
    /// was raised; one that coincides with a store code takes that kind.
    pub fn from_code(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        Self {
            kind: DeploymentErrorKind::from_code(&code).unwrap_or(DeploymentErrorKind::Other),
            code,
            message: message.into(),
        }
    }

    /// The HTTP class the admin API answers this refusal with.
    pub fn class(&self) -> ErrorClass {
        self.kind.class()
    }

    pub(super) fn read(e: sqlx::Error) -> Self {
        Self::new(DeploymentErrorKind::DbRead, e.to_string())
    }

    pub(super) fn write(e: sqlx::Error) -> Self {
        Self::new(DeploymentErrorKind::DbWrite, e.to_string())
    }

    pub(super) fn corrupt(message: impl Into<String>) -> Self {
        Self::new(DeploymentErrorKind::CorruptPersistedState, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kind's code names it back, so a code from another layer that
    /// coincides with one is answered as the store's own.
    #[test]
    fn every_code_names_its_kind() {
        for kind in [
            DeploymentErrorKind::DeploymentHeadConflict,
            DeploymentErrorKind::ProviderRevoked,
            DeploymentErrorKind::CorruptPersistedState,
            DeploymentErrorKind::MemberDefinitionNotFound,
        ] {
            assert_eq!(DeploymentErrorKind::from_code(kind.code()), Some(kind));
        }
        let passed = DeploymentError::from_code("PROVIDER_REVOKED", "policy");
        assert_eq!(passed.kind, DeploymentErrorKind::ProviderRevoked);
        assert_eq!(passed.class(), ErrorClass::Forbidden);
        let other = DeploymentError::from_code("ENTERPRISE_TIER_NOT_ENABLED", "x");
        assert_eq!(other.kind, DeploymentErrorKind::Other);
        assert_eq!(other.code, "ENTERPRISE_TIER_NOT_ENABLED");
        assert_eq!(other.class(), ErrorClass::Invalid);
    }
}
