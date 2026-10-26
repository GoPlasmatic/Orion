//! Column identifiers for the v2 deployment store's tables: the head, its
//! revisions and their members, the idempotency receipts, the
//! content-addressed artifacts, and the provider packages, admissions and
//! connection revisions a manifest names.

use sea_query::Iden;

/// Content-addressed bytes: definitions, package files, connection
/// configurations. `state` is `active`, `draining` or `deleted`; a deleted
/// row keeps its digest as a tombstone and drops `payload_bytes`.
#[derive(Iden, Clone, Copy)]
pub enum ArtifactObjects {
    Table,
    Digest,
    SizeBytes,
    State,
    DrainEpoch,
    PayloadBytes,
    CreatedAt,
}

#[derive(Iden, Clone, Copy)]
pub enum ProviderPackages {
    Table,
    Digest,
    Publisher,
    ProviderId,
    Version,
    Implementation,
    Tier,
    ManifestJson,
    InventoryJson,
}

#[derive(Iden, Clone, Copy)]
pub enum ProviderAdmissions {
    Table,
    PackageDigest,
    PolicyId,
    AdmissionFingerprint,
    Revoked,
    UpdatedAt,
}

#[derive(Iden, Clone, Copy)]
pub enum ConnectionRevisions {
    Table,
    ConnectionId,
    Revision,
    ProviderDigest,
    PublicConfigJson,
    SecretLocatorsJson,
    BindingsJson,
}

#[derive(Iden, Clone, Copy)]
pub enum DeploymentRevisions {
    Table,
    Revision,
    ParentRevision,
    ManifestDigest,
    ManifestJson,
    Principal,
    Reason,
    MembersCompacted,
    CreatedAt,
}

#[derive(Iden, Clone, Copy)]
pub enum DeploymentMembers {
    Table,
    DeploymentRevision,
    EntityKind,
    EntityId,
    EntityRevision,
    OwningPackage,
    DefinitionDigest,
}

/// One row (`singleton_id = 1`): the committed head and the epoch every
/// mutation that peers must notice moves.
#[derive(Iden, Clone, Copy)]
pub enum DeploymentHead {
    Table,
    SingletonId,
    HeadRevision,
    Epoch,
    UpdatedAt,
}

/// One idempotency receipt per `(principal, idempotency_key)`.
#[derive(Iden, Clone, Copy)]
pub enum DeploymentOperations {
    Table,
    Principal,
    IdempotencyKey,
    RequestHash,
    CommittedRevision,
    ManifestDigest,
    OperationId,
    Epoch,
    ReceivingNodeId,
    CreatedAt,
}
