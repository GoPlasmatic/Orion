//! The connector call contract: what a call answers, and how it fails.
//!
//! All three connector implementation kinds (`rust`, `workflow`, `wasm`) share
//! this semantic contract: a call answers `Result<`[`ConnectorOutput`]`,
//! `[`ConnectorError`]`>`. Host adapters translate backend-specific outputs
//! into a [`ConnectorOutput`] and normalize errors into a [`ConnectorError`]
//! while preserving host-observed [`EffectStatus`] and trusted call lineage.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The three supported connector implementation kinds in Orion v2.
///
/// Channels are intentionally excluded here: channels are native Rust ingress
/// and scheduler drivers ([`crate::ChannelDriver`]), whereas connectors may be
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
    /// The kind named `name` (`rust`, `workflow` or `wasm`), or `None`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "rust" => Some(Self::Rust),
            "workflow" => Some(Self::Workflow),
            "wasm" => Some(Self::Wasm),
            _ => None,
        }
    }

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

/// Structured error origin (`V2-10.1`). The stage is a fixed word in
/// practice (`dispatch`, `validation`, `transport`, …), so it is borrowed
/// rather than allocated; a deserialized one owns its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorOrigin {
    pub kind: OriginKind,
    pub stage: Cow<'static, str>,
}

impl ErrorOrigin {
    pub fn host(stage: impl Into<Cow<'static, str>>) -> Self {
        Self {
            kind: OriginKind::Host,
            stage: stage.into(),
        }
    }

    pub fn provider(stage: impl Into<Cow<'static, str>>) -> Self {
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
    pub const WORKFLOW_FAILED: &str = "PROVIDER_WORKFLOW_FAILED";
}

/// Normalized connector failure record (`V2-10.1`).
///
/// Kept small enough to travel in a `Result` by value: the code and the
/// stage are borrowed (every host code is a [`codes`] constant), and the
/// rarely-set `details` are boxed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorError {
    pub code: Cow<'static, str>,
    pub class: ConnectorErrorClass,
    pub message: String,
    pub effect_status: EffectStatus,
    pub retry_advice: RetryAdvice,
    pub origin: ErrorOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<Value>>,
}

impl ConnectorError {
    pub fn capability_invalid(message: impl Into<String>, prior_effect: EffectStatus) -> Self {
        Self {
            code: Cow::Borrowed(codes::CAPABILITY_INVALID),
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
            code: Cow::Borrowed(codes::BUDGET_EXHAUSTED),
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
            code: Cow::Borrowed(codes::DEADLINE_EXCEEDED),
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
            code: Cow::Borrowed(codes::CANCELLED),
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
        code: &'static str,
        stage: &'static str,
        message: impl Into<String>,
        prior_effect: EffectStatus,
    ) -> Self {
        Self {
            code: Cow::Borrowed(code),
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
            code: Cow::Borrowed(codes::INVALID_INPUT),
            class: ConnectorErrorClass::InvalidInput,
            message: message.into(),
            effect_status: EffectStatus::NotStarted,
            retry_advice: RetryAdvice::Never,
            origin: ErrorOrigin::host("validation"),
            retry_after_ms: None,
            details: details.map(Box::new),
        }
    }

    pub fn contract_violation(
        stage: &'static str,
        message: impl Into<String>,
        effect_status: EffectStatus,
    ) -> Self {
        Self {
            code: Cow::Borrowed(codes::CONTRACT_VIOLATION),
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
            code: Cow::Borrowed(codes::UNAVAILABLE),
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
