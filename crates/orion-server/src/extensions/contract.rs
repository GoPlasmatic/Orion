//! Normalized connector invocation, identity, result and error contract (`V2-10.1`).
//!
//! All three connector implementation kinds (`rust`, `workflow`, `wasm`) share
//! this semantic contract. Host adapters translate backend-specific outputs into
//! [`ConnectorResult`] and normalize errors into [`ConnectorError`] while
//! preserving host-observed [`EffectStatus`] and trusted call lineage.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The three supported connector implementation kinds in Orion v2.
///
/// Channels are intentionally excluded here: channels are native Rust ingress
/// and scheduler drivers ([`super::ChannelDriver`]), whereas connectors may be
/// packaged as Direct Rust, Workflow-Group composites, or Wasm components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImplementationKind {
    /// Compiled native Rust provider registered via `ProviderBackend` (`orion-rust-provider/1`).
    Rust,
    /// Composite workflow-group package combining private workflows, sub-channels, and connector slots.
    Workflow,
    /// Sandboxed WebAssembly component (`orion:connector@2.0.0`).
    Wasm,
}

impl ImplementationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Workflow => "workflow",
            Self::Wasm => "wasm",
        }
    }
}

impl fmt::Display for ImplementationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Immutable provider package identity pinned in a deployment lock.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProviderReference {
    /// Namespaced provider identifier (e.g. `"orion.http"`, `"orion.postgresql"`).
    pub provider_id: String,
    /// Exact semantic version string.
    pub version: String,
    /// Canonical package digest (`sha256:<64 lowercase hex>`).
    pub package_digest: String,
}

/// Pinned connection reference within a compiled runtime generation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ConnectionReference {
    /// Stable logical connection identifier.
    pub connection_id: String,
    /// Positive immutable revision number.
    pub revision: u64,
    /// Pinned provider package artifact.
    pub provider: ProviderReference,
}

/// Effective identity used to partition stateful sessions and connection pools (`V2-05.4`, `CTX-08`).
///
/// Two invocations share a session pool entry only when their provider artifact,
/// connection ID, connection revision, and resolved credential hash all match.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EffectiveConnectionIdentity {
    pub provider_digest: String,
    pub connection_id: String,
    pub revision: u64,
    /// SHA-256 digest of resolved secret material (never the raw secret).
    pub secret_hash: String,
}

impl EffectiveConnectionIdentity {
    /// Stable capacity key shared across revisions of the same logical connection
    /// so rotating credentials or updating config does not multiply max pool ceilings (`CTX-08`).
    pub fn stable_capacity_key(&self) -> (&str, &str) {
        (&self.provider_digest, &self.connection_id)
    }
}

/// Deployment revision identity (`V2-10.1`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeploymentIdentity {
    pub revision: u64,
    pub manifest_hash: String,
}

/// Normalized error class vocabulary (`V2-10.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorErrorClass {
    InvalidInput,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    RateLimited,
    Unavailable,
    Deadline,
    Cancelled,
    ProviderFailure,
    ContractViolation,
}

impl ConnectorErrorClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::RateLimited => "rate_limited",
            Self::Unavailable => "unavailable",
            Self::Deadline => "deadline",
            Self::Cancelled => "cancelled",
            Self::ProviderFailure => "provider_failure",
            Self::ContractViolation => "contract_violation",
        }
    }
}

/// Host-observed external effect settlement state (`V2-10.1`).
///
/// Ordered by uncertainty lattice rules: once an attempt or child records
/// [`EffectStatus::Unknown`], a subsequent local pre-send rejection cannot
/// downgrade the logical invocation back to [`EffectStatus::NotStarted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    /// No relevant external operation started across this logical invocation and descendants.
    NotStarted,
    /// Relevant external operations settled (including an observed downstream rejection).
    Completed,
    /// At least one external operation may have taken effect without conclusive settlement evidence.
    Unknown,
}

impl EffectStatus {
    /// Combine prior invocation effect evidence with a newly observed attempt/child status.
    ///
    /// - `Unknown` dominates everything unless an explicit idempotent settlement overrides it.
    /// - `NotStarted` before any prior activity remains `NotStarted`; after `Completed` or
    ///   `Unknown`, a pre-send failure preserves the earlier evidence (`CTX-05`).
    pub const fn merge(self, next: Self) -> Self {
        match (self, next) {
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Completed, _) | (_, Self::Completed) => Self::Completed,
            (Self::NotStarted, Self::NotStarted) => Self::NotStarted,
        }
    }
}

/// Normalized retry advice (`V2-10.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RetryAdvice {
    Never,
    Transient,
}

/// Whether an error originated in the host policy/transport boundary or inside the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OriginKind {
    Host,
    Provider,
}

/// Structured error origin (`V2-10.1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorOrigin {
    pub kind: OriginKind,
    pub stage: String,
}

impl ErrorOrigin {
    pub fn host(stage: impl Into<String>) -> Self {
        Self {
            kind: OriginKind::Host,
            stage: stage.into(),
        }
    }

    pub fn provider(stage: impl Into<String>) -> Self {
        Self {
            kind: OriginKind::Provider,
            stage: stage.into(),
        }
    }
}

/// Standard v2 connector host error codes (`V2-10.1`).
pub mod codes {
    pub const INVALID_INPUT: &str = "CONNECTOR_INVALID_INPUT";
    pub const CONNECTION_FORBIDDEN: &str = "CONNECTOR_CONNECTION_FORBIDDEN";
    pub const OPERATION_FORBIDDEN: &str = "CONNECTOR_OPERATION_FORBIDDEN";
    pub const EGRESS_DENIED: &str = "CONNECTOR_EGRESS_DENIED";
    pub const CAPABILITY_INVALID: &str = "CONNECTOR_CAPABILITY_INVALID";
    pub const DEPENDENCY_UNDECLARED: &str = "CONNECTOR_DEPENDENCY_UNDECLARED";
    pub const DOWNSTREAM_UNAUTHORIZED: &str = "CONNECTOR_DOWNSTREAM_UNAUTHORIZED";
    pub const UNAVAILABLE: &str = "CONNECTOR_UNAVAILABLE";
    pub const DEADLINE_EXCEEDED: &str = "CONNECTOR_DEADLINE_EXCEEDED";
    pub const CANCELLED: &str = "CONNECTOR_CANCELLED";
    pub const BUDGET_EXHAUSTED: &str = "CONNECTOR_BUDGET_EXHAUSTED";
    pub const CONTRACT_VIOLATION: &str = "PROVIDER_CONTRACT_VIOLATION";
}

/// Normalized connector failure record (`V2-10.1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorError {
    pub code: String,
    pub class: ConnectorErrorClass,
    pub message: String,
    pub effect_status: EffectStatus,
    pub retry_advice: RetryAdvice,
    pub origin: ErrorOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ConnectorError {
    pub fn capability_invalid(message: impl Into<String>, prior_effect: EffectStatus) -> Self {
        Self {
            code: codes::CAPABILITY_INVALID.to_string(),
            class: ConnectorErrorClass::Forbidden,
            message: message.into(),
            effect_status: prior_effect,
            retry_advice: RetryAdvice::Never,
            origin: ErrorOrigin::host("dispatch"),
            retry_after_ms: None,
            details: None,
        }
    }

    pub fn budget_exhausted(message: impl Into<String>, prior_effect: EffectStatus) -> Self {
        Self {
            code: codes::BUDGET_EXHAUSTED.to_string(),
            class: ConnectorErrorClass::ProviderFailure,
            message: message.into(),
            effect_status: prior_effect,
            retry_advice: RetryAdvice::Never,
            origin: ErrorOrigin::host("budget"),
            retry_after_ms: None,
            details: None,
        }
    }

    pub fn deadline_exceeded(prior_effect: EffectStatus) -> Self {
        Self {
            code: codes::DEADLINE_EXCEEDED.to_string(),
            class: ConnectorErrorClass::Deadline,
            message: "Connector deadline exceeded".to_string(),
            effect_status: prior_effect,
            retry_advice: RetryAdvice::Never,
            origin: ErrorOrigin::host("deadline"),
            retry_after_ms: None,
            details: None,
        }
    }

    pub fn cancelled(prior_effect: EffectStatus) -> Self {
        Self {
            code: codes::CANCELLED.to_string(),
            class: ConnectorErrorClass::Cancelled,
            message: "Connector invocation cancelled".to_string(),
            effect_status: prior_effect,
            retry_advice: RetryAdvice::Never,
            origin: ErrorOrigin::host("cancellation"),
            retry_after_ms: None,
            details: None,
        }
    }

    pub fn forbidden(
        code: &str,
        stage: &str,
        message: impl Into<String>,
        prior_effect: EffectStatus,
    ) -> Self {
        Self {
            code: code.to_string(),
            class: ConnectorErrorClass::Forbidden,
            message: message.into(),
            effect_status: prior_effect,
            retry_advice: RetryAdvice::Never,
            origin: ErrorOrigin::host(stage),
            retry_after_ms: None,
            details: None,
        }
    }

    pub fn invalid_input(message: impl Into<String>, details: Option<Value>) -> Self {
        Self {
            code: codes::INVALID_INPUT.to_string(),
            class: ConnectorErrorClass::InvalidInput,
            message: message.into(),
            effect_status: EffectStatus::NotStarted,
            retry_advice: RetryAdvice::Never,
            origin: ErrorOrigin::host("validation"),
            retry_after_ms: None,
            details,
        }
    }

    pub fn contract_violation(
        stage: &str,
        message: impl Into<String>,
        effect_status: EffectStatus,
    ) -> Self {
        Self {
            code: codes::CONTRACT_VIOLATION.to_string(),
            class: ConnectorErrorClass::ContractViolation,
            message: message.into(),
            effect_status,
            retry_advice: RetryAdvice::Never,
            origin: ErrorOrigin::host(stage),
            retry_after_ms: None,
            details: None,
        }
    }

    pub fn unavailable(message: impl Into<String>, effect_status: EffectStatus) -> Self {
        Self {
            code: codes::UNAVAILABLE.to_string(),
            class: ConnectorErrorClass::Unavailable,
            message: message.into(),
            effect_status,
            retry_advice: RetryAdvice::Transient,
            origin: ErrorOrigin::host("transport"),
            retry_after_ms: None,
            details: None,
        }
    }
}

impl fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ConnectorError {}

/// Successful connector output payload and bounded metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorOutput {
    pub data: Value,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

impl ConnectorOutput {
    pub fn new(data: Value) -> Self {
        Self {
            data,
            metadata: BTreeMap::new(),
        }
    }

    pub fn with_metadata(data: Value, metadata: BTreeMap<String, Value>) -> Self {
        Self { data, metadata }
    }
}

/// Host-normalized tagged union result for SDKs and conformance comparison (`V2-10.1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ConnectorResult {
    Success {
        data: Value,
        #[serde(default)]
        metadata: BTreeMap<String, Value>,
    },
    Error {
        error: ConnectorError,
    },
}

impl From<Result<ConnectorOutput, ConnectorError>> for ConnectorResult {
    fn from(res: Result<ConnectorOutput, ConnectorError>) -> Self {
        match res {
            Ok(out) => Self::Success {
                data: out.data,
                metadata: out.metadata,
            },
            Err(error) => Self::Error { error },
        }
    }
}

/// Declared operation side-effect classification (`V2-10.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationEffect {
    Read,
    Write,
    Mixed,
}

/// Declared operation retry policy (`V2-10.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationRetryPolicy {
    Safe,
    RequiresKey,
    Never,
}

impl OperationRetryPolicy {
    /// Evaluate whether the host retry scheduler may start another attempt after `error`
    /// under the retry ownership table in `connector-invocation.md`.
    pub fn can_retry(
        self,
        effect: OperationEffect,
        error: &ConnectorError,
        has_idempotency_key: bool,
        has_downstream_idempotency_support: bool,
    ) -> bool {
        if error.retry_advice != RetryAdvice::Transient {
            return false;
        }
        match (effect, self) {
            (_, Self::Never) => false,
            (OperationEffect::Read, Self::Safe) => true,
            (OperationEffect::Read, Self::RequiresKey) => has_idempotency_key,
            (OperationEffect::Write | OperationEffect::Mixed, Self::RequiresKey | Self::Safe) => {
                if !has_idempotency_key || !has_downstream_idempotency_support {
                    return false;
                }
                true
            }
        }
    }
}
