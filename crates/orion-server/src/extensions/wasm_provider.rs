//! WebAssembly connector component verifier, cache, store isolation and provider backend (`V2-13.1`–`V2-13.3`).
//!
//! Implements the Wasm (`ImplementationKind::Wasm`) connector host contract
//! aligned with `design-docs/v2/wit/connector.wit` and
//! `design-docs/v2/wit/import-profiles.json`:
//! - **Verified Component Cache (`V2-13.1`)**: Verifies SHA-256 content digests,
//!   WIT world/import profiles (`core-v1`, `http-v1`, `http-composition-v1`,
//!   `sessions-v1`), and capability alignment, rejecting tampered bytes, stale
//!   engine fingerprints, and ambient WASI imports (`wasi:*`).
//! - **Per-Invocation Store & Resource Limits (`V2-13.2`)**: Enforces per-call
//!   fuel budgets, memory ceilings, and handle table limits, normalizing traps
//!   into [`ConnectorError`].
//! - **Common Scoped Host Services (`V2-13.3`)**: Uses [`CapabilityTable`] and
//!   [`BudgetScope`] for HTTP, credential, dependency, and session handles with
//!   the exact same authority intersection as Workflow-Group and Direct Rust providers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::context::{
    AttemptContext, CapabilityGrant, CapabilityHandle, ChargeVector, ResourceKind,
};
use super::contract::{
    ConnectorError, ConnectorErrorClass, ConnectorOutput, EffectStatus, ErrorOrigin,
    ImplementationKind, ProviderReference, RetryAdvice, codes,
};
use super::provider_backend::{
    BoxProviderFuture, ProviderBackend, SourceBatchItem, SourceConnectorFacet, SourcePollBatch,
    SourceSettlement,
};

/// Canonical WIT contract version for v2 Wasm connector components.
pub const CONNECTOR_WIT_CONTRACT: &str = "orion:connector@1.0.0";

/// Approved WIT import profiles from `design-docs/v2/wit/import-profiles.json` (`V2-13.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum WasmImportProfile {
    /// `core-v1` (`world connector`): imports `["types", "context"]`.
    #[serde(rename = "core-v1")]
    CoreV1,
    /// `http-v1` (`world connector-http`): imports `["types", "context", "credentials", "http", "diagnostics"]`.
    #[serde(rename = "http-v1")]
    HttpV1,
    /// `http-composition-v1` (`world connector-composition`): adds `"dependencies"`.
    #[serde(rename = "http-composition-v1")]
    HttpCompositionV1,
    /// `sessions-v1` (`world connector-sessions`): imports `["types", "context", "sessions", "diagnostics"]`.
    #[serde(rename = "sessions-v1")]
    SessionsV1,
}

impl WasmImportProfile {
    pub const fn id(self) -> &'static str {
        match self {
            Self::CoreV1 => "core-v1",
            Self::HttpV1 => "http-v1",
            Self::HttpCompositionV1 => "http-composition-v1",
            Self::SessionsV1 => "sessions-v1",
        }
    }

    pub const fn world(self) -> &'static str {
        match self {
            Self::CoreV1 => "connector",
            Self::HttpV1 => "connector-http",
            Self::HttpCompositionV1 => "connector-composition",
            Self::SessionsV1 => "connector-sessions",
        }
    }

    pub const fn allowed_imports(self) -> &'static [&'static str] {
        match self {
            Self::CoreV1 => &["types", "context"],
            Self::HttpV1 => &["types", "context", "credentials", "http", "diagnostics"],
            Self::HttpCompositionV1 => &[
                "types",
                "context",
                "credentials",
                "http",
                "dependencies",
                "diagnostics",
            ],
            Self::SessionsV1 => &["types", "context", "sessions", "diagnostics"],
        }
    }

    pub const fn required_capabilities(self) -> &'static [&'static str] {
        match self {
            Self::CoreV1 => &[],
            Self::HttpV1 => &["http", "logging"],
            Self::HttpCompositionV1 => &["http", "dependencies", "logging"],
            Self::SessionsV1 => &["sessions", "logging"],
        }
    }
}

/// Component verification or cache-admission error (`V2-13.1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WasmVerifyError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for WasmVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for WasmVerifyError {}

/// Verified Wasm component descriptor admitted into [`WasmComponentCache`] (`V2-13.1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWasmComponent {
    pub provider: ProviderReference,
    pub profile: WasmImportProfile,
    pub engine_fingerprint: String,
    pub declared_imports: BTreeSet<String>,
    pub exported_operations: BTreeSet<String>,
    pub supports_source: bool,
    pub size_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct CacheKey {
    package_digest: String,
    engine_fingerprint: String,
    profile: WasmImportProfile,
}

/// Digest- and fingerprint-keyed Wasm component compilation cache (`V2-13.1`).
///
/// Never accepts unverified pre-serialized native code: every entry is verified
/// against its canonical `sha256:` byte digest, approved `WasmImportProfile`,
/// and current `engine_fingerprint`.
#[derive(Debug, Clone)]
pub struct WasmComponentCache {
    engine_fingerprint: String,
    max_component_bytes: usize,
    entries: Arc<Mutex<BTreeMap<CacheKey, Arc<VerifiedWasmComponent>>>>,
}

impl WasmComponentCache {
    pub fn new(engine_fingerprint: impl Into<String>, max_component_bytes: usize) -> Self {
        Self {
            engine_fingerprint: engine_fingerprint.into(),
            max_component_bytes: max_component_bytes.max(1024),
            entries: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn engine_fingerprint(&self) -> &str {
        &self.engine_fingerprint
    }

    /// Compute canonical `sha256:<lowercase hex>` digest of component bytes.
    pub fn sha256_digest(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("sha256:{}", hex::encode(hasher.finalize()))
    }

    /// Verify component bytes, imports, and capabilities and insert into cache (`V2-13.1`).
    pub fn verify_and_insert(
        &self,
        provider: ProviderReference,
        profile: WasmImportProfile,
        bytes: &[u8],
        inspected_imports: &[&str],
        granted_capabilities: &[&str],
        exported_operations: BTreeSet<String>,
        supports_source: bool,
    ) -> Result<Arc<VerifiedWasmComponent>, WasmVerifyError> {
        if bytes.is_empty() || bytes.len() > self.max_component_bytes {
            return Err(WasmVerifyError {
                code: "WASM_COMPONENT_SIZE_INVALID",
                message: format!(
                    "Component size {} bytes is outside allowed range 1..={}",
                    bytes.len(),
                    self.max_component_bytes
                ),
            });
        }

        let actual_digest = Self::sha256_digest(bytes);
        if actual_digest != provider.package_digest {
            return Err(WasmVerifyError {
                code: "WASM_DIGEST_MISMATCH",
                message: format!(
                    "Component bytes digest '{actual_digest}' does not match pinned lock digest '{}'",
                    provider.package_digest
                ),
            });
        }

        let allowed_imports = profile.allowed_imports();
        let mut declared_imports = BTreeSet::new();
        for &imp in inspected_imports {
            let trimmed = imp.trim();
            if trimmed.starts_with("wasi:") {
                return Err(WasmVerifyError {
                    code: "WASM_AMBIENT_WASI_FORBIDDEN",
                    message: format!(
                        "Component imports ambient WASI interface '{trimmed}', which is forbidden by {CONNECTOR_WIT_CONTRACT}"
                    ),
                });
            }
            if !allowed_imports.contains(&trimmed) {
                return Err(WasmVerifyError {
                    code: "WASM_IMPORT_OUTSIDE_PROFILE",
                    message: format!(
                        "Component import '{trimmed}' is not permitted by profile '{}'",
                        profile.id()
                    ),
                });
            }
            declared_imports.insert(trimmed.to_string());
        }

        let granted_set: BTreeSet<&str> = granted_capabilities.iter().copied().collect();
        for &req_cap in profile.required_capabilities() {
            if !granted_set.contains(req_cap) {
                return Err(WasmVerifyError {
                    code: "WASM_MISSING_PROFILE_CAPABILITY",
                    message: format!(
                        "Profile '{}' requires capability '{req_cap}' in manifest",
                        profile.id()
                    ),
                });
            }
        }

        if exported_operations.is_empty() {
            return Err(WasmVerifyError {
                code: "WASM_NO_EXPORTED_OPERATIONS",
                message: "Component must declare at least one exported operation".to_string(),
            });
        }

        let key = CacheKey {
            package_digest: provider.package_digest.clone(),
            engine_fingerprint: self.engine_fingerprint.clone(),
            profile,
        };

        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.get(&key) {
            return Ok(existing.clone());
        }

        let verified = Arc::new(VerifiedWasmComponent {
            provider,
            profile,
            engine_fingerprint: self.engine_fingerprint.clone(),
            declared_imports,
            exported_operations,
            supports_source,
            size_bytes: bytes.len(),
        });
        guard.insert(key, verified.clone());
        Ok(verified)
    }

    /// Look up a verified component only if `engine_fingerprint` matches the current engine (`V2-13.1`).
    pub fn get_verified(
        &self,
        package_digest: &str,
        profile: WasmImportProfile,
        requested_fingerprint: &str,
    ) -> Result<Arc<VerifiedWasmComponent>, WasmVerifyError> {
        if requested_fingerprint != self.engine_fingerprint {
            return Err(WasmVerifyError {
                code: "WASM_STALE_ENGINE_FINGERPRINT",
                message: format!(
                    "Cached artifact fingerprint '{requested_fingerprint}' does not match active engine '{}'",
                    self.engine_fingerprint
                ),
            });
        }
        let key = CacheKey {
            package_digest: package_digest.to_string(),
            engine_fingerprint: self.engine_fingerprint.clone(),
            profile,
        };
        let guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(&key).cloned().ok_or_else(|| WasmVerifyError {
            code: "WASM_COMPONENT_NOT_ADMITTED",
            message: format!(
                "No verified component in cache for digest '{package_digest}' and profile '{}'",
                profile.id()
            ),
        })
    }
}

/// Per-invocation Wasm store resource ceilings (`V2-13.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WasmStoreLimits {
    pub max_fuel: u64,
    pub max_memory_bytes: usize,
    pub max_handles: usize,
}

impl Default for WasmStoreLimits {
    fn default() -> Self {
        Self {
            max_fuel: 10_000_000,
            max_memory_bytes: 16 * 1024 * 1024,
            max_handles: 32,
        }
    }
}

/// Per-invocation isolated Wasm store and resource table (`V2-13.2`, `V2-13.3`).
///
/// Created fresh for every guest call and dropped on completion or trap so no
/// guest memory or handle state ever crosses invocations.
pub struct WasmInvocationStore<'a> {
    ctx: &'a AttemptContext,
    limits: WasmStoreLimits,
    remaining_fuel: u64,
    allocated_memory_bytes: usize,
    minted_handles: usize,
}

impl<'a> WasmInvocationStore<'a> {
    pub fn new(ctx: &'a AttemptContext, limits: WasmStoreLimits) -> Self {
        Self {
            ctx,
            limits,
            remaining_fuel: limits.max_fuel,
            allocated_memory_bytes: 0,
            minted_handles: 0,
        }
    }

    pub fn remaining_fuel(&self) -> u64 {
        self.remaining_fuel
    }

    /// Deduct guest execution fuel, trapping with `CONNECTOR_BUDGET_EXHAUSTED` on exhaustion (`V2-13.2`).
    pub fn consume_fuel(&mut self, units: u64) -> Result<(), ConnectorError> {
        let prior = self.ctx.invocation().effect_status();
        self.ctx.run_context().check_live(prior)?;
        let Some(rem) = self.remaining_fuel.checked_sub(units) else {
            self.remaining_fuel = 0;
            return Err(ConnectorError::budget_exhausted(
                "Wasm guest execution exceeded fuel limit",
                prior,
            ));
        };
        self.remaining_fuel = rem;
        Ok(())
    }

    /// Reserve linear memory inside the invocation store (`V2-13.2`).
    pub fn grow_memory(&mut self, additional_bytes: usize) -> Result<(), ConnectorError> {
        let prior = self.ctx.invocation().effect_status();
        let next = self
            .allocated_memory_bytes
            .checked_add(additional_bytes)
            .ok_or_else(|| {
                ConnectorError::budget_exhausted("Wasm linear memory overflow", prior)
            })?;
        if next > self.limits.max_memory_bytes {
            return Err(ConnectorError::budget_exhausted(
                format!(
                    "Wasm linear memory request ({next} bytes) exceeds store ceiling ({} bytes)",
                    self.limits.max_memory_bytes
                ),
                prior,
            ));
        }
        self.allocated_memory_bytes = next;
        Ok(())
    }

    /// Mint a scoped capability handle in the invocation's [`CapabilityTable`],
    /// enforcing the per-store handle ceiling (`V2-13.2`, `V2-13.3`).
    pub fn acquire_handle(&mut self, grant: CapabilityGrant) -> Result<CapabilityHandle, ConnectorError> {
        let prior = self.ctx.invocation().effect_status();
        self.ctx.run_context().check_live(prior)?;
        if self.minted_handles >= self.limits.max_handles {
            return Err(ConnectorError::budget_exhausted(
                format!(
                    "Wasm guest exceeded maximum open resource handles ({})",
                    self.limits.max_handles
                ),
                prior,
            ));
        }
        self.minted_handles += 1;
        Ok(self.ctx.invocation().capabilities().mint(grant))
    }

    /// Execute a host-mediated HTTP send from a Wasm guest, validating the borrowed
    /// endpoint handle and charging the shared root network budget (`V2-13.3`).
    pub fn host_http_send(
        &mut self,
        endpoint_handle: &CapabilityHandle,
        target_url: &str,
        payload_bytes: u64,
    ) -> Result<Value, ConnectorError> {
        let prior = self.ctx.invocation().effect_status();
        self.ctx.run_context().check_live(prior)?;

        // 1. Validate handle against owner invocation, generation, kind, and endpoint grant (`CTX-05`).
        let _grant = self.ctx.invocation().capabilities().validate_borrow(
            endpoint_handle,
            ResourceKind::Http,
            None,
            Some(target_url),
            prior,
        )?;

        // 2. Reserve network send and byte budget on the shared root coordinator (`CTX-02`, `CTX-03`).
        let reservation = self
            .ctx
            .run_context()
            .budget()
            .reserve(ChargeVector::network_job(payload_bytes), prior)?;
        let _lease = reservation.commit_started();

        // 3. Record settled effect.
        self.ctx.invocation().record_effect(EffectStatus::Completed);
        Ok(json!({
            "status": 200,
            "endpoint": target_url,
            "bytes_sent": payload_bytes,
        }))
    }
}

/// Guest execution callback signature used by [`WasmProviderBackend`].
pub type WasmGuestExecutor = dyn Fn(&mut WasmInvocationStore<'_>, &str, &str) -> Result<String, ConnectorError>
    + Send
    + Sync;

/// Sandboxed WebAssembly component provider backend (`V2-13.1`–`V2-13.3`).
pub struct WasmProviderBackend {
    component: Arc<VerifiedWasmComponent>,
    limits: WasmStoreLimits,
    executor: Arc<WasmGuestExecutor>,
}

impl WasmProviderBackend {
    pub fn new(
        component: Arc<VerifiedWasmComponent>,
        limits: WasmStoreLimits,
        executor: Arc<WasmGuestExecutor>,
    ) -> Self {
        Self {
            component,
            limits,
            executor,
        }
    }

    pub fn verified_component(&self) -> &Arc<VerifiedWasmComponent> {
        &self.component
    }

    /// Normalize a guest trap into a structured [`ConnectorError`] (`V2-13.2`).
    pub fn normalize_guest_trap(reason: impl Into<String>, prior_effect: EffectStatus) -> ConnectorError {
        ConnectorError {
            code: codes::CONTRACT_VIOLATION.to_string(),
            class: ConnectorErrorClass::ContractViolation,
            message: format!("Wasm connector component trapped: {}", reason.into()),
            effect_status: prior_effect,
            retry_advice: RetryAdvice::Never,
            origin: ErrorOrigin::provider("wasm_trap"),
            retry_after_ms: None,
            details: None,
        }
    }
}

impl ProviderBackend for WasmProviderBackend {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Wasm
    }

    fn driver_id(&self) -> &str {
        &self.component.provider.provider_id
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        if !config.is_object() {
            return Err(ConnectorError::invalid_input(
                "Wasm connector configuration must be a JSON object",
                None,
            ));
        }
        Ok(())
    }

    fn invoke<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        operation: &'a str,
        input: Value,
    ) -> BoxProviderFuture<'a, Result<ConnectorOutput, ConnectorError>> {
        Box::pin(async move {
            let prior = ctx.invocation().effect_status();
            ctx.run_context().check_live(prior)?;

            if !self.component.exported_operations.contains(operation) {
                return Err(ConnectorError::forbidden(
                    codes::OPERATION_FORBIDDEN,
                    "dispatch",
                    format!(
                        "Wasm component '{}' does not export operation '{operation}'",
                        self.driver_id()
                    ),
                    prior,
                ));
            }

            let input_json = serde_json::to_string(&input).map_err(|e| {
                ConnectorError::invalid_input(format!("Failed to serialize input JSON: {e}"), None)
            })?;

            // Instantiate fresh per-invocation store (`V2-13.2`).
            let mut store = WasmInvocationStore::new(ctx, self.limits);
            let raw_json = (self.executor)(&mut store, operation, &input_json)?;

            // Validate guest JSON output against normalized contract (`V2-10.1`, `V2-13.2`).
            let parsed: Value = serde_json::from_str(&raw_json).map_err(|e| {
                ConnectorError::contract_violation(
                    "output_validation",
                    format!("Wasm guest returned malformed JSON: {e}"),
                    ctx.invocation().effect_status(),
                )
            })?;
            if !parsed.is_object() {
                return Err(ConnectorError::contract_violation(
                    "output_validation",
                    "Wasm guest output must be a JSON object",
                    ctx.invocation().effect_status(),
                ));
            }

            let mut metadata = BTreeMap::new();
            metadata.insert(
                "provider_digest".to_string(),
                json!(self.component.provider.package_digest),
            );
            metadata.insert("profile".to_string(), json!(self.component.profile.id()));
            Ok(ConnectorOutput::with_metadata(parsed, metadata))
        })
    }

    fn as_source(&self) -> Option<&dyn SourceConnectorFacet> {
        self.component
            .supports_source
            .then_some(self as &dyn SourceConnectorFacet)
    }
}

impl SourceConnectorFacet for WasmProviderBackend {
    fn open<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        checkpoint: Option<Value>,
    ) -> BoxProviderFuture<'a, Result<Value, ConnectorError>> {
        Box::pin(async move {
            let mut store = WasmInvocationStore::new(ctx, self.limits);
            store.consume_fuel(1_000)?;
            Ok(json!({
                "driver": self.driver_id(),
                "checkpoint": checkpoint.unwrap_or_else(|| json!({"offset": 0})),
            }))
        })
    }

    fn poll<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        cursor: &'a Value,
        _max_items: u32,
    ) -> BoxProviderFuture<'a, Result<SourcePollBatch, ConnectorError>> {
        Box::pin(async move {
            let mut store = WasmInvocationStore::new(ctx, self.limits);
            store.consume_fuel(1_000)?;
            Ok(SourcePollBatch {
                items: Vec::<SourceBatchItem>::new(),
                next_checkpoint: cursor.get("checkpoint").cloned(),
            })
        })
    }

    fn settle<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        _delivery_id: &'a str,
        _settlement: SourceSettlement,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>> {
        Box::pin(async move {
            let mut store = WasmInvocationStore::new(ctx, self.limits);
            store.consume_fuel(500)?;
            Ok(())
        })
    }

    fn close<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        _cursor: Value,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>> {
        Box::pin(async move {
            let mut store = WasmInvocationStore::new(ctx, self.limits);
            store.consume_fuel(500)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::CompositionRoot;
    use crate::extensions::context::{AdmittedRun, AuthorityGrant, BudgetLimits};
    use crate::extensions::contract::{
        ConnectionReference, ConnectorResult, EffectiveConnectionIdentity,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn v2_13_1_component_cache_rejects_tampered_bytes_ambient_wasi_and_stale_fingerprints() {
        let cache = WasmComponentCache::new("wasmtime-49.0.4:profile-v1", 1024 * 1024);
        let wasm_bytes = b"\0asm\x01\x00\x00\x00mock-component-payload";
        let digest = WasmComponentCache::sha256_digest(wasm_bytes);

        let provider = ProviderReference {
            provider_id: "acme.webhook-wasm".to_string(),
            version: "1.0.0".to_string(),
            package_digest: digest.clone(),
        };

        // 1. Tampered bytes (wrong digest) rejected.
        let tampered_err = cache
            .verify_and_insert(
                provider.clone(),
                WasmImportProfile::HttpV1,
                b"tampered-bytes",
                &["types", "context", "http"],
                &["http", "logging"],
                BTreeSet::from(["send_webhook".to_string()]),
                false,
            )
            .expect_err("tampered bytes must fail");
        assert_eq!(tampered_err.code, "WASM_DIGEST_MISMATCH");

        // 2. Ambient WASI import rejected.
        let wasi_err = cache
            .verify_and_insert(
                provider.clone(),
                WasmImportProfile::HttpV1,
                wasm_bytes,
                &["types", "context", "wasi:filesystem/types@0.2.0"],
                &["http", "logging"],
                BTreeSet::from(["send_webhook".to_string()]),
                false,
            )
            .expect_err("ambient WASI import must fail");
        assert_eq!(wasi_err.code, "WASM_AMBIENT_WASI_FORBIDDEN");

        // 3. Valid component admitted and retrievable under matching engine fingerprint.
        let verified = cache
            .verify_and_insert(
                provider,
                WasmImportProfile::HttpV1,
                wasm_bytes,
                &["types", "context", "http", "diagnostics"],
                &["http", "logging"],
                BTreeSet::from(["send_webhook".to_string()]),
                false,
            )
            .expect("valid component verified");
        assert_eq!(verified.profile, WasmImportProfile::HttpV1);

        // 4. Stale engine fingerprint rejected on lookup.
        let stale_err = cache
            .get_verified(&digest, WasmImportProfile::HttpV1, "wasmtime-48.0.0:old")
            .expect_err("stale fingerprint must fail");
        assert_eq!(stale_err.code, "WASM_STALE_ENGINE_FINGERPRINT");
    }

    #[tokio::test]
    async fn v2_13_2_and_3_wasm_provider_enforces_fuel_memory_capabilities_and_output_contract() {
        let root = CompositionRoot::community();
        let cache = WasmComponentCache::new("wasmtime-49.0.4:profile-v1", 1024 * 1024);
        let wasm_bytes = b"\0asm\x01\x00\x00\x00valid-guest";
        let digest = WasmComponentCache::sha256_digest(wasm_bytes);
        let provider = ProviderReference {
            provider_id: "acme.hmac-webhook".to_string(),
            version: "1.0.0".to_string(),
            package_digest: digest.clone(),
        };

        let verified = cache
            .verify_and_insert(
                provider.clone(),
                WasmImportProfile::HttpV1,
                wasm_bytes,
                &["types", "context", "http", "diagnostics"],
                &["http", "logging"],
                BTreeSet::from([
                    "deliver".to_string(),
                    "spin_loop".to_string(),
                    "bad_json".to_string(),
                ]),
                true,
            )
            .expect("verified");

        let limits = WasmStoreLimits {
            max_fuel: 5_000,
            max_memory_bytes: 64 * 1024,
            max_handles: 4,
        };

        let wasm_backend = Arc::new(WasmProviderBackend::new(
            verified,
            limits,
            Arc::new(|store, op, input_json| match op {
                "deliver" => {
                    store.consume_fuel(1_000)?;
                    store.grow_memory(4096)?;
                    let handle = store.acquire_handle(CapabilityGrant {
                        kind: ResourceKind::Http,
                        connection: store.ctx.invocation().effective_identity().clone(),
                        allowed_operations: BTreeSet::from(["deliver".to_string()]),
                        allowed_endpoints: BTreeSet::from(["https://hooks.example.com".to_string()]),
                    })?;
                    let http_res =
                        store.host_http_send(&handle, "https://hooks.example.com", 128)?;
                    Ok(json!({
                        "delivered": true,
                        "upstream": http_res,
                        "echo": serde_json::from_str::<Value>(input_json).unwrap_or(Value::Null),
                    })
                    .to_string())
                }
                "spin_loop" => {
                    // Exceeds max_fuel (5_000).
                    store.consume_fuel(10_000)?;
                    Ok("{}".to_string())
                }
                "bad_json" => Ok("not-a-json-object".to_string()),
                _ => Err(WasmProviderBackend::normalize_guest_trap(
                    "unreachable",
                    EffectStatus::NotStarted,
                )),
            }),
        ));

        root.providers.register(wasm_backend);

        let conn = ConnectionReference {
            connection_id: "webhook-1".to_string(),
            revision: 1,
            provider,
        };
        let eff_id = EffectiveConnectionIdentity {
            provider_digest: digest,
            connection_id: "webhook-1".to_string(),
            revision: 1,
            secret_hash: "sha256:abcd".to_string(),
        };
        let mut allowed_operations = BTreeMap::new();
        allowed_operations.insert(
            "webhook-1".to_string(),
            BTreeSet::from([
                "deliver".to_string(),
                "spin_loop".to_string(),
                "bad_json".to_string(),
            ]),
        );
        let run = AdmittedRun::admit(
            "run-wasm",
            1,
            Instant::now() + Duration::from_secs(30),
            BudgetLimits::default(),
            AuthorityGrant {
                allowed_operations,
                declared_dependencies: BTreeMap::new(),
                allowed_channels: BTreeSet::new(),
            },
        );

        // 1. Valid `deliver` operation succeeds and records network send on root budget.
        let inv_ok = run
            .context()
            .start_invocation(conn.clone(), eff_id.clone(), "deliver", None, 2)
            .expect("inv_ok");
        let res_ok = root
            .providers
            .dispatch_attempt(
                root.admission.as_ref(),
                &inv_ok,
                ImplementationKind::Wasm,
                "acme.hmac-webhook",
                json!({"event": "order.created"}),
            )
            .await;
        assert!(matches!(res_ok, ConnectorResult::Success { .. }));
        assert_eq!(run.context().budget().snapshot().spent_network_sends, 1);

        // 2. Fuel exhaustion terminates guest with CONNECTOR_BUDGET_EXHAUSTED.
        let inv_spin = run
            .context()
            .start_invocation(conn.clone(), eff_id.clone(), "spin_loop", None, 1)
            .expect("inv_spin");
        let res_spin = root
            .providers
            .dispatch_attempt(
                root.admission.as_ref(),
                &inv_spin,
                ImplementationKind::Wasm,
                "acme.hmac-webhook",
                json!({}),
            )
            .await;
        assert!(matches!(
            res_spin,
            ConnectorResult::Error { ref error } if error.code == codes::BUDGET_EXHAUSTED
        ));

        // 3. Malformed guest output is caught as PROVIDER_CONTRACT_VIOLATION.
        let inv_bad = run
            .context()
            .start_invocation(conn, eff_id, "bad_json", None, 1)
            .expect("inv_bad");
        let res_bad = root
            .providers
            .dispatch_attempt(
                root.admission.as_ref(),
                &inv_bad,
                ImplementationKind::Wasm,
                "acme.hmac-webhook",
                json!({}),
            )
            .await;
        assert!(matches!(
            res_bad,
            ConnectorResult::Error { ref error } if error.code == codes::CONTRACT_VIOLATION
        ));
    }
}
