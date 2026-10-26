//! The error envelope: `{"error": {code, message, details[], request_id}}`.
//!
//! Every non-2xx response the server sends carries this shape. The server's
//! internal error enum (`OrionError`) stays server-side — what is contract is
//! only what serializes, which is exactly what this module holds.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The stable machine-readable `error.code` registry.
///
/// A client branches on these, so renaming one is a breaking API change. The
/// server's wire-contract tests pin each variant's code with a string literal
/// on purpose — the literals there are the pin, these constants are what
/// production code (server mapping, client hints) is written against, and the
/// tests are what keep the two honest.
pub mod codes {
    pub const NOT_FOUND: &str = "NOT_FOUND";
    pub const VALIDATION_ERROR: &str = "VALIDATION_ERROR";
    pub const UNAUTHORIZED: &str = "UNAUTHORIZED";
    pub const FORBIDDEN: &str = "FORBIDDEN";
    pub const CONFLICT: &str = "CONFLICT";
    pub const UNSUPPORTED_MEDIA_TYPE: &str = "UNSUPPORTED_MEDIA_TYPE";
    pub const METHOD_NOT_ALLOWED: &str = "METHOD_NOT_ALLOWED";
    pub const SERVICE_UNAVAILABLE: &str = "SERVICE_UNAVAILABLE";
    pub const RATE_LIMITED: &str = "RATE_LIMITED";
    pub const TIMEOUT: &str = "TIMEOUT";
    /// The request body exceeded `ingest.max_payload_size` (data plane) or
    /// `server.max_admin_body_size` (admin plane). Distinct from
    /// [`RESPONSE_TOO_LARGE`], which is about what the server built.
    pub const PAYLOAD_TOO_LARGE: &str = "PAYLOAD_TOO_LARGE";
    pub const RESPONSE_TOO_LARGE: &str = "RESPONSE_TOO_LARGE";
    pub const INTERNAL_ERROR: &str = "INTERNAL_ERROR";
    pub const CONFIG_ERROR: &str = "CONFIG_ERROR";
    pub const STORAGE_ERROR: &str = "STORAGE_ERROR";
    pub const ENGINE_ERROR: &str = "ENGINE_ERROR";
    pub const SERIALIZATION_ERROR: &str = "SERIALIZATION_ERROR";
    /// An open circuit breaker shed the request before it reached a connector.
    pub const CIRCUIT_OPEN: &str = "CIRCUIT_OPEN";
    /// A `channel_call` chain came back to a channel already on it, or went
    /// deeper than the configured maximum (`508 Loop Detected`). The loop is
    /// in the deployed channels, not in the request.
    pub const LOOP_DETECTED: &str = "LOOP_DETECTED";
    /// V2 deferred-write incompatibility (`426 Upgrade Required`).
    pub const UPGRADE_REQUIRED: &str = "UPGRADE_REQUIRED";
    /// Enterprise tier package or feature rejected on Community Edition.
    pub const ENTERPRISE_TIER_NOT_ENABLED: &str = "ENTERPRISE_TIER_NOT_ENABLED";
    /// Edition transition blocked by active Enterprise dependencies.
    pub const EDITION_TRANSITION_BLOCKED: &str = "EDITION_TRANSITION_BLOCKED";
}

/// The closed vocabulary of [`FieldError::code`].
///
/// These are the codes the server actually emits — the registry equivalent of
/// [`codes`] one level down. Client code should branch on these constants
/// rather than on string literals, and a server change that needs a new code
/// adds it here first: `field_code_literals_are_all_registered` in
/// orion-server fails on any literal that is not one of these.
pub mod field_codes {
    /// A required field was absent.
    pub const REQUIRED: &str = "REQUIRED";
    /// The field is required for this protocol/type, though optional in general.
    pub const REQUIRED_FOR_PROTOCOL: &str = "REQUIRED_FOR_PROTOCOL";
    /// Present and well-typed, but not an acceptable value.
    pub const INVALID: &str = "INVALID";
    /// Present but the wrong JSON type.
    pub const TYPE_MISMATCH: &str = "TYPE_MISMATCH";
    /// Longer than the column or protocol allows.
    pub const TOO_LONG: &str = "TOO_LONG";
    /// A key the strict parser does not accept — usually a typo or a pre-1.0
    /// spelling.
    pub const UNKNOWN_FIELD: &str = "UNKNOWN_FIELD";
    /// The same key appeared twice in one object.
    pub const DUPLICATE_FIELD: &str = "DUPLICATE_FIELD";
    /// Two steps in one workflow declare the same `id`. Tasks and task
    /// groups share one id namespace, so a group may collide with a task.
    pub const DUPLICATE_TASK_ID: &str = "DUPLICATE_TASK_ID";
    /// A task names a function the engine does not register — the workflow
    /// would be accepted and then fail at its first request.
    pub const UNKNOWN_FUNCTION: &str = "UNKNOWN_FUNCTION";
    /// The document still carries an authoring convenience that is resolved
    /// when a definition set is compiled — a `$from` shared value, a `use`
    /// task fragment. The admin API takes one document and has no set to
    /// resolve it against, so the compiled form is what it accepts:
    /// `orion-cli compile <dir>` produces it.
    pub const UNCOMPILED_SOURCE: &str = "UNCOMPILED_SOURCE";
    /// A secret reference (`env://NAME`, `vault://…`) sits in a field that
    /// does not resolve one. Only five fields do — `crypto.key`,
    /// `jwt_sign.key`, and `jwt_verify`'s `keys`, `issuer` and `audience` —
    /// and everywhere else the string is sent on as itself, so a URL spelled
    /// `env://API_BASE` is requested verbatim. Move the value to a connector,
    /// or declare it in the config file: a deployment value under `[vars]`,
    /// read as `{"var": "metadata.vars.<name>"}`, and key material under
    /// `[secrets]`, read as `{"secret": "<name>"}` in one of the five fields.
    pub const UNRESOLVED_SECRET_REF: &str = "UNRESOLVED_SECRET_REF";
    /// Every code above, for exhaustiveness checks.
    pub const ALL: &[&str] = &[
        REQUIRED,
        REQUIRED_FOR_PROTOCOL,
        INVALID,
        TYPE_MISMATCH,
        TOO_LONG,
        UNKNOWN_FIELD,
        DUPLICATE_FIELD,
        DUPLICATE_TASK_ID,
        UNKNOWN_FUNCTION,
        UNCOMPILED_SOURCE,
        UNRESOLVED_SECRET_REF,
    ];
}

/// How a v2 admin refusal is answered: the HTTP status class of a [`V2Code`].
///
/// Every code in the registry has exactly one kind, so a client can branch on
/// the kind without knowing every code, and the server maps a refusal to its
/// status from the kind alone — never from the code's spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// `400`: the request itself is refused.
    Invalid,
    /// `403`: a provider that is not admitted, or no longer is, or an
    /// admission policy that refuses it.
    Forbidden,
    /// `404`: a package, admission, revision or connection that is not stored.
    NotFound,
    /// `409`: an immutable row stored differently, a reused idempotency key,
    /// a head another writer moved first, or a reviewed digest that no longer
    /// matches.
    Conflict,
    /// `503`: the state database could not be read or written.
    Unavailable,
    /// `500`: a fault on the server's side — a stored row it cannot read
    /// back, a value it could not serialize. Never the caller's input.
    Internal,
}

impl ErrorKind {
    /// The HTTP status this kind is answered with.
    pub const fn status(self) -> u16 {
        match self {
            Self::Invalid => 400,
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::Conflict => 409,
            Self::Unavailable => 503,
            Self::Internal => 500,
        }
    }
}

macro_rules! v2_codes {
    ($( $(#[$meta:meta])* $variant:ident = $code:literal => $kind:ident, )*) => {
        /// The closed vocabulary of `error.code` on the v2 admin API's own
        /// refusals — packages, connections, plans and deployments.
        ///
        /// The general codes in [`codes`] (`VALIDATION_ERROR` with field
        /// details, `NOT_FOUND`, `UNAUTHORIZED`, …) still answer what every
        /// endpoint shares: a body that does not parse, a missing key, a
        /// definition that would not serve. These are the rest, one per
        /// refusal a client can act on, each with the [`ErrorKind`] that
        /// decides its status. `v2_error_codes_drift_test` in orion-server
        /// keeps this list, what the server raises and
        /// `docs/src/reference/errors.md` in agreement.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum V2Code {
            $( $(#[$meta])* $variant, )*
        }

        impl V2Code {
            /// Every code, in registry order.
            pub const ALL: &'static [V2Code] = &[ $( V2Code::$variant, )* ];

            /// The wire spelling.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $code, )*
                }
            }

            /// The kind that decides the HTTP status.
            pub const fn kind(self) -> ErrorKind {
                match self {
                    $( Self::$variant => ErrorKind::$kind, )*
                }
            }

            /// The code `code` spells, when it is a registered one.
            pub fn parse(code: &str) -> Option<Self> {
                match code {
                    $( $code => Some(Self::$variant), )*
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for V2Code {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

v2_codes! {
    // -- Not found --
    PackageNotFound = "PACKAGE_NOT_FOUND" => NotFound,
    AdmissionNotFound = "ADMISSION_NOT_FOUND" => NotFound,
    BaseRevisionNotFound = "BASE_REVISION_NOT_FOUND" => NotFound,
    RollbackTargetNotFound = "ROLLBACK_TARGET_NOT_FOUND" => NotFound,
    ConnectionRevisionNotFound = "CONNECTION_REVISION_NOT_FOUND" => NotFound,
    /// No committed deployment revision has that number.
    RevisionNotFound = "REVISION_NOT_FOUND" => NotFound,
    /// The connection's provider package has no backend on this node.
    ProviderNotFound = "PROVIDER_NOT_FOUND" => NotFound,

    // -- Conflict --
    PublisherVersionConflict = "PUBLISHER_VERSION_CONFLICT" => Conflict,
    ImmutableContentConflict = "IMMUTABLE_CONTENT_CONFLICT" => Conflict,
    ImmutableConnectionConflict = "IMMUTABLE_CONNECTION_CONFLICT" => Conflict,
    IdempotencyKeyConflict = "IDEMPOTENCY_KEY_CONFLICT" => Conflict,
    /// The head moved since the caller's base: another writer committed first.
    DeploymentHeadConflict = "DEPLOYMENT_HEAD_CONFLICT" => Conflict,
    /// The manifest planned now is not the one the caller reviewed.
    ReviewedDigestMismatch = "REVIEWED_DIGEST_MISMATCH" => Conflict,
    /// A newer revision superseded the one a node was building.
    CandidateSuperseded = "CANDIDATE_SUPERSEDED" => Conflict,

    // -- Forbidden --
    ProviderNotAdmitted = "PROVIDER_NOT_ADMITTED" => Forbidden,
    ProviderRevoked = "PROVIDER_REVOKED" => Forbidden,
    AdmissionInvalidated = "ADMISSION_INVALIDATED" => Forbidden,
    /// An Enterprise-tier package on a Community node.
    EnterpriseTierNotEnabled = "ENTERPRISE_TIER_NOT_ENABLED" => Forbidden,
    PackageInvalidProviderId = "PACKAGE_INVALID_PROVIDER_ID" => Forbidden,
    PackageInvalidTarget = "PACKAGE_INVALID_TARGET" => Forbidden,
    /// The edition's admission policy refused the package for a reason of
    /// its own; the message carries the policy's code.
    AdmissionRejected = "ADMISSION_REJECTED" => Forbidden,

    // -- Unavailable --
    DbTransaction = "DB_TRANSACTION_ERROR" => Unavailable,
    DbWrite = "DB_WRITE_ERROR" => Unavailable,
    DbCommit = "DB_COMMIT_ERROR" => Unavailable,
    DbRead = "DB_READ_ERROR" => Unavailable,

    // -- Internal --
    CorruptPersistedState = "CORRUPT_PERSISTED_STATE" => Internal,
    /// A row the store wrote could not be serialized.
    SerializeError = "SERIALIZE_ERROR" => Internal,
    RequestHashInvalid = "REQUEST_HASH_INVALID" => Internal,
    /// A node was asked to build a revision the store does not hold.
    CandidateRevisionMissing = "CANDIDATE_REVISION_MISSING" => Internal,
    /// A health probe's run could not be started.
    ProbeFailed = "PROBE_FAILED" => Internal,

    // -- Invalid: the package archive --
    ArchiveEntryCountExceeded = "ARCHIVE_ENTRY_COUNT_EXCEEDED" => Invalid,
    ArchiveSymlinkDenied = "ARCHIVE_SYMLINK_DENIED" => Invalid,
    ArchiveDuplicateEntry = "ARCHIVE_DUPLICATE_ENTRY" => Invalid,
    ArchiveCompressionRatioExceeded = "ARCHIVE_COMPRESSION_RATIO_EXCEEDED" => Invalid,
    ArchiveSizeLimitExceeded = "ARCHIVE_SIZE_LIMIT_EXCEEDED" => Invalid,
    ArchiveInventoryMissing = "ARCHIVE_INVENTORY_MISSING" => Invalid,
    ArchiveInventoryInvalid = "ARCHIVE_INVENTORY_INVALID" => Invalid,
    ArchivePathInvalid = "ARCHIVE_PATH_INVALID" => Invalid,
    /// A file in an upload is neither UTF-8 text nor valid `base64:`.
    InvalidPackageEncoding = "INVALID_PACKAGE_ENCODING" => Invalid,
    /// `POST /authoring/pack` could not pack the files.
    PackValidationFailed = "PACK_VALIDATION_FAILED" => Invalid,
    /// A package finding with no code of its own here; the message carries
    /// the finding's.
    PackageInvalid = "PACKAGE_INVALID" => Invalid,

    // -- Invalid: the package inventory --
    InventoryContentMismatch = "INVENTORY_CONTENT_MISMATCH" => Invalid,
    InventoryLimitExceeded = "INVENTORY_LIMIT_EXCEEDED" => Invalid,
    InventoryNotCanonical = "INVENTORY_NOT_CANONICAL" => Invalid,
    InventoryOrderInvalid = "INVENTORY_ORDER_INVALID" => Invalid,
    InventoryPathCollision = "INVENTORY_PATH_COLLISION" => Invalid,
    InventoryPathInvalid = "INVENTORY_PATH_INVALID" => Invalid,
    InventoryPayloadMismatch = "INVENTORY_PAYLOAD_MISMATCH" => Invalid,
    InventoryRequiredFileMissing = "INVENTORY_REQUIRED_FILE_MISSING" => Invalid,
    InventorySelfReference = "INVENTORY_SELF_REFERENCE" => Invalid,
    InventoryShapeInvalid = "INVENTORY_SHAPE_INVALID" => Invalid,
    CanonicalValueUnsupported = "CANONICAL_VALUE_UNSUPPORTED" => Invalid,

    // -- Invalid: the package manifest and its schemas --
    ManifestInvalidJson = "MANIFEST_INVALID_JSON" => Invalid,
    ManifestShapeInvalid = "MANIFEST_SHAPE_INVALID" => Invalid,
    ManifestArtifactMissing = "MANIFEST_ARTIFACT_MISSING" => Invalid,
    ManifestDependencyAliasDuplicate = "MANIFEST_DEPENDENCY_ALIAS_DUPLICATE" => Invalid,
    ManifestEntryMismatch = "MANIFEST_ENTRY_MISMATCH" => Invalid,
    ManifestImportProfileCapabilityMissing = "MANIFEST_IMPORT_PROFILE_CAPABILITY_MISSING" => Invalid,
    ManifestLimitExceedsProfile = "MANIFEST_LIMIT_EXCEEDS_PROFILE" => Invalid,
    ManifestOperationReserved = "MANIFEST_OPERATION_RESERVED" => Invalid,
    ManifestPathInvalid = "MANIFEST_PATH_INVALID" => Invalid,
    ManifestRetryUnsafe = "MANIFEST_RETRY_UNSAFE" => Invalid,
    ManifestSchemaVersionUnsupported = "MANIFEST_SCHEMA_VERSION_UNSUPPORTED" => Invalid,
    DistributionInvalidJson = "DISTRIBUTION_INVALID_JSON" => Invalid,
    DistributionShapeInvalid = "DISTRIBUTION_SHAPE_INVALID" => Invalid,
    DocumentInvalidJson = "DOCUMENT_INVALID_JSON" => Invalid,
    DocumentLimitExceeded = "DOCUMENT_LIMIT_EXCEEDED" => Invalid,
    SchemaInvalidJson = "SCHEMA_INVALID_JSON" => Invalid,
    SchemaInvalid = "SCHEMA_INVALID" => Invalid,
    SchemaProfileLimit = "SCHEMA_PROFILE_LIMIT" => Invalid,
    SchemaReferenceCycle = "SCHEMA_REFERENCE_CYCLE" => Invalid,
    SchemaReferenceDenied = "SCHEMA_REFERENCE_DENIED" => Invalid,
    SchemaReferenceMissing = "SCHEMA_REFERENCE_MISSING" => Invalid,
    SchemaReferenceNotSchema = "SCHEMA_REFERENCE_NOT_SCHEMA" => Invalid,
    SchemaUnsupportedKeyword = "SCHEMA_UNSUPPORTED_KEYWORD" => Invalid,
    PackageInvalidTier = "PACKAGE_INVALID_TIER" => Invalid,
    ArtifactNotActive = "ARTIFACT_NOT_ACTIVE" => Invalid,
    ArtifactDrainingOrDeleted = "ARTIFACT_DRAINING_OR_DELETED" => Invalid,

    // -- Invalid: a dependency lock --
    LockAliasDuplicate = "LOCK_ALIAS_DUPLICATE" => Invalid,
    LockArtifactUnavailable = "LOCK_ARTIFACT_UNAVAILABLE" => Invalid,
    LockBindingSlotMismatch = "LOCK_BINDING_SLOT_MISMATCH" => Invalid,
    LockClosureMissing = "LOCK_CLOSURE_MISSING" => Invalid,
    LockCycle = "LOCK_CYCLE" => Invalid,
    LockDeclarationMismatch = "LOCK_DECLARATION_MISMATCH" => Invalid,
    LockDuplicateNode = "LOCK_DUPLICATE_NODE" => Invalid,
    LockGrantMismatch = "LOCK_GRANT_MISMATCH" => Invalid,
    LockIdentityConflict = "LOCK_IDENTITY_CONFLICT" => Invalid,
    LockMetadataMismatch = "LOCK_METADATA_MISMATCH" => Invalid,
    LockOrderInvalid = "LOCK_ORDER_INVALID" => Invalid,
    LockShapeInvalid = "LOCK_SHAPE_INVALID" => Invalid,
    LockUnreachableNode = "LOCK_UNREACHABLE_NODE" => Invalid,
    LockVersionRequirementUnqualified = "LOCK_VERSION_REQUIREMENT_UNQUALIFIED" => Invalid,

    // -- Invalid: a connection revision --
    ConnectionRevisionInvalid = "CONNECTION_REVISION_INVALID" => Invalid,
    ConnectionPublicSecretForbidden = "CONNECTION_PUBLIC_SECRET_FORBIDDEN" => Invalid,
    ConnectionSecretLocatorInvalid = "CONNECTION_SECRET_LOCATOR_INVALID" => Invalid,
    ConnectionDigestMismatch = "CONNECTION_DIGEST_MISMATCH" => Invalid,
    ConnectionProviderNotDeclared = "CONNECTION_PROVIDER_NOT_DECLARED" => Invalid,

    // -- Invalid: a deployment manifest --
    ProviderMetadataMismatch = "PROVIDER_METADATA_MISMATCH" => Invalid,
    MemberDefinitionNotFound = "MEMBER_DEFINITION_NOT_FOUND" => Invalid,
    OwningPackageNotDeclared = "OWNING_PACKAGE_NOT_DECLARED" => Invalid,
    MemberArtifactNotInPackage = "MEMBER_ARTIFACT_NOT_IN_PACKAGE" => Invalid,
    RollbackTargetCompacted = "ROLLBACK_TARGET_COMPACTED" => Invalid,
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
    /// This node could not build the candidate; nothing was committed.
    DeploymentWouldNotBuild = "DEPLOYMENT_WOULD_NOT_BUILD" => Invalid,
    /// The node could not rebuild its generation from the candidate.
    EngineReloadFailed = "ENGINE_RELOAD_FAILED" => Invalid,
}

/// Per-field validation detail returned in the `error.details[]` array.
///
/// `code` is a stable machine-readable identifier from [`field_codes`].
///
/// `path` is a pointer to the offending field, and is rooted one of two ways
/// depending on how far the request got:
///
/// - **Validation reached** — the path is resource-rooted and may be indexed:
///   `channel.protocol`, `tasks[2].function.input.connector`.
/// - **The body failed to deserialize** — validation never ran, so the layer
///   that reports it knows the field name but not which resource was being
///   parsed. The path is `body.<field>`, or bare `body` when even the field
///   cannot be recovered from the parser's message.
///
/// Match on the trailing segment rather than the whole path if you need to
/// treat both the same way.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema), schema(as = ErrorFieldDetail))]
pub struct FieldError {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub path: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub code: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub got: Option<Value>,
}

impl FieldError {
    /// `code` is `&'static str` on purpose: field codes are a closed, stable
    /// vocabulary, and a computed string here is a code no client can rely on.
    pub fn new(path: impl Into<String>, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            code: code.to_string(),
            message: message.into(),
            expected: None,
            got: None,
        }
    }

    pub fn with_expected(mut self, expected: impl Into<Value>) -> Self {
        self.expected = Some(expected.into());
        self
    }

    pub fn with_got(mut self, got: impl Into<Value>) -> Self {
        self.got = Some(got.into());
        self
    }
}

/// The object under the `error` key.
///
/// Serialization matches the server's v1.0 behaviour exactly: `details` is
/// omitted when empty (clients expecting only the v0.1 `{code, message}`
/// envelope still parse), `request_id` is omitted when the request had none.
/// Deserialization defaults every field so a pre-1.0 body still reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema), schema(as = ErrorDetail))]
pub struct ErrorBody {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub code: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<FieldError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// The full error response body: `{"error": {...}}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema), schema(as = ErrorResponse))]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each code is spelled once and names itself back.
    #[test]
    fn v2_codes_are_unique_and_parse_back() {
        let mut seen = std::collections::BTreeSet::new();
        for code in V2Code::ALL {
            assert!(seen.insert(code.as_str()), "{code} is registered twice");
            assert_eq!(V2Code::parse(code.as_str()), Some(*code));
        }
        assert_eq!(V2Code::parse("NOT_A_CODE"), None);
        assert_eq!(V2Code::DeploymentHeadConflict.kind().status(), 409);
        assert_eq!(V2Code::CorruptPersistedState.kind().status(), 500);
    }

    #[test]
    fn empty_details_and_request_id_are_omitted() {
        let env = ErrorEnvelope {
            error: ErrorBody {
                code: codes::NOT_FOUND.to_string(),
                message: "no such workflow".to_string(),
                details: Vec::new(),
                request_id: None,
            },
        };
        let value = serde_json::to_value(&env).expect("test");
        assert_eq!(value["error"]["code"], "NOT_FOUND");
        assert_eq!(value["error"]["message"], "no such workflow");
        assert!(value["error"].get("details").is_none());
        assert!(value["error"].get("request_id").is_none());
    }

    #[test]
    fn field_errors_serialize_with_optional_expected_got() {
        let f = FieldError::new("channel.protocol", "ENUM_MISMATCH", "unknown protocol")
            .with_expected(serde_json::json!(["rest", "http", "cron"]))
            .with_got("REST");
        let value = serde_json::to_value(&f).expect("test");
        assert_eq!(value["path"], "channel.protocol");
        assert_eq!(value["code"], "ENUM_MISMATCH");
        assert_eq!(
            value["expected"],
            serde_json::json!(["rest", "http", "cron"])
        );
        assert_eq!(value["got"], "REST");

        let bare = FieldError::new("x", "REQUIRED", "missing");
        let value = serde_json::to_value(&bare).expect("test");
        assert!(value.get("expected").is_none());
        assert!(value.get("got").is_none());
    }

    #[test]
    fn a_v01_body_still_parses() {
        // Pre-1.0 servers sent only {code, message}.
        let env: ErrorEnvelope =
            serde_json::from_str(r#"{"error": {"code": "CONFLICT", "message": "dup"}}"#)
                .expect("test");
        assert_eq!(env.error.code, codes::CONFLICT);
        assert_eq!(env.error.message, "dup");
        assert!(env.error.details.is_empty());
        assert!(env.error.request_id.is_none());
    }

    #[test]
    fn a_degenerate_body_parses_to_defaults() {
        let env: ErrorEnvelope = serde_json::from_str(r#"{"error": {}}"#).expect("test");
        assert!(env.error.code.is_empty());
        assert!(env.error.message.is_empty());
    }

    #[test]
    fn roundtrip_preserves_details_and_request_id() {
        let env = ErrorEnvelope {
            error: ErrorBody {
                code: codes::VALIDATION_ERROR.to_string(),
                message: "bad".to_string(),
                details: vec![FieldError::new("a.b", "REQUIRED", "missing")],
                request_id: Some("req-1".to_string()),
            },
        };
        let json = serde_json::to_string(&env).expect("test");
        let back: ErrorEnvelope = serde_json::from_str(&json).expect("test");
        assert_eq!(back.error.details.len(), 1);
        assert_eq!(back.error.details[0].path, "a.b");
        assert_eq!(back.error.request_id.as_deref(), Some("req-1"));
    }
}
