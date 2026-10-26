//! V2-03 & V2-04 immutable deployment storage: canonical manifests, bounded
//! archive ingestion, candidate planning, idempotent CAS apply, rollback by
//! new revision, and two-phase `ACTIVE -> DRAINING -> DELETED` artifact GC.
//!
//! The database is the store. Every mutation is one transaction that reads
//! what it judges with targeted SELECTs (`writes`), decides on values
//! (`rules`) and writes through sea-query (`sql`). What a node reads on
//! its serving path — the head, the committed revisions, and the packages,
//! admissions and connection revisions they name — is cached in the
//! `catalog`, refreshed incrementally; artifact bytes are never cached and
//! are read by digest when needed.

mod catalog;
mod error;
mod manifest;
mod rules;
mod sql;
mod writes;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use error::{DeploymentError, DeploymentErrorKind};
pub use manifest::{CanonicalManifest, EntityKind, EntityRef, ManifestMember};

use super::ImplementationKind;
use super::package_identity::{canonical_bytes, sha256_digest};
use super::schema_profile::{CompiledSchemaBundle, SchemaProfileFinding};
use super::{
    ConnectionId, ConnectionReference, Digest, EffectiveConnectionIdentity, ProviderId,
    ProviderRef, Revision,
};
use error::DeploymentErrorKind as Kind;

/// Which of an uploaded package's declared schemas an instance is checked
/// against ([`ImmutableDeploymentStore::check_package_instance`]).
#[derive(Debug, Clone, Copy)]
pub enum PackageSchema<'a> {
    /// The manifest's `configuration_schema`: a connection's configuration.
    Configuration,
    /// `operations.<name>.input_schema`: what `connector_call` sends.
    OperationInput(&'a str),
}

/// Whether `key` names a secret, so its value belongs in `secret_locators`.
///
/// A key that names an endpoint — ending `_url`, `_uri` or `_endpoint` — is
/// not one, whatever else it says: OAuth2's `token_url` is where a token is
/// fetched from, not a token, and `authorization_endpoint` is a page. Its
/// value is still screened like every public string (no userinfo, no
/// secret-named query parameter), which is where a credential in a URL is
/// caught. `token`, `access_token`, `refresh_token`, `api_token` and
/// `session_token` stay secret.
fn is_public_config_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    if ["_url", "_uri", "_endpoint"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
    {
        return false;
    }
    const SECRET_PATTERNS: &[&str] = &[
        "secret",
        "password",
        "passwd",
        "pwd",
        "token",
        "api_key",
        "apikey",
        "api-key",
        "private_key",
        "private-key",
        "access_key",
        "access-key",
        "authorization",
        "credential",
        "bearer",
        "passphrase",
        "signing_key",
    ];
    SECRET_PATTERNS.iter().any(|pat| lower.contains(pat))
}

fn validate_no_secrets_in_public_config(value: &Value) -> Result<(), DeploymentError> {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if is_public_config_secret_key(k) {
                    return Err(DeploymentError::new(
                        Kind::ConnectionPublicSecretForbidden,
                        format!("secret key '{k}' must use secret_locators, not public_config"),
                    ));
                }
                validate_no_secrets_in_public_config(v)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                validate_no_secrets_in_public_config(item)?;
            }
            Ok(())
        }
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.starts_with("Bearer ") || trimmed.starts_with("Basic ") {
                return Err(DeploymentError::new(
                    Kind::ConnectionPublicSecretForbidden,
                    "authorization credentials must use secret_locators, not public_config",
                ));
            }
            if let Some(after_scheme) = trimmed.split("://").nth(1) {
                let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
                if authority.contains('@') {
                    return Err(DeploymentError::new(
                        Kind::ConnectionPublicSecretForbidden,
                        "credential-bearing URL userinfo must use secret_locators, not public_config",
                    ));
                }
            }
            if let Ok(parsed) = url::Url::parse(trimmed) {
                if !parsed.username().is_empty() || parsed.password().is_some() {
                    return Err(DeploymentError::new(
                        Kind::ConnectionPublicSecretForbidden,
                        "credential-bearing URL userinfo must use secret_locators, not public_config",
                    ));
                }
                for (qk, _) in parsed.query_pairs() {
                    if is_public_config_secret_key(&qk) {
                        return Err(DeploymentError::new(
                            Kind::ConnectionPublicSecretForbidden,
                            format!(
                                "secret URL query parameter '{qk}' must use secret_locators, not public_config"
                            ),
                        ));
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn extract_inventory_file_digests(inventory_json: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(files) = inventory_json.get("files").and_then(Value::as_array) {
        for entry in files {
            if let Some(d) = entry
                .get("digest")
                .or_else(|| entry.get("sha256"))
                .and_then(Value::as_str)
            {
                if d.starts_with(Digest::PREFIX) {
                    out.push(d.to_string());
                } else {
                    out.push(format!("{}{d}", Digest::PREFIX));
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ArtifactState {
    Active,
    Draining,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactObjectRow {
    pub digest: String,
    pub size_bytes: usize,
    pub state: ArtifactState,
    pub drain_epoch: Option<u64>,
    pub bytes: Option<Vec<u8>>,
}

/// A stored provider package. Its identity is a [`ProviderRef`]
/// ([`Self::provider_ref`]); the row adds what the upload carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPackageRow {
    pub package_digest: Digest,
    pub publisher: String,
    pub provider_id: ProviderId,
    pub version: String,
    pub implementation: ImplementationKind,
    pub manifest_json: Value,
    pub distribution_json: Value,
    pub inventory_json: Value,
}

impl ProviderPackageRow {
    /// The id the package's backend registers under on the
    /// [`ProviderRegistry`](crate::extensions::ProviderRegistry): its digest
    /// for a `workflow` or `wasm` package, which only that version's bytes
    /// can name, and its provider id for a `rust` package, whose backend is
    /// built into the binary.
    pub fn backend_id(&self) -> &str {
        match self.implementation {
            ImplementationKind::Rust => &self.provider_id,
            ImplementationKind::Workflow | ImplementationKind::Wasm => &self.package_digest,
        }
    }

    /// The package's identity, as a manifest pins it.
    pub fn provider_ref(&self) -> ProviderRef {
        ProviderRef {
            publisher: self.publisher.clone(),
            provider_id: self.provider_id.clone(),
            version: self.version.clone(),
            digest: self.package_digest.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAdmissionRow {
    pub package_digest: Digest,
    pub admission_fingerprint: String,
    pub policy_id: String,
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionRevisionRow {
    pub connection_id: ConnectionId,
    pub revision: Revision,
    pub provider_digest: Digest,
    pub public_config: Value,
    /// Where each credential comes from: a config path (`connection_string`,
    /// `auth.token`, `auth.client_secret`) to a secret reference (`env://`,
    /// `vault://`, …). Never a secret itself, and never a path
    /// `public_config` already holds.
    pub secret_locators: std::collections::BTreeMap<String, String>,
    /// Over a `workflow` provider package: which connection each of the
    /// package's declared dependency aliases calls (`alias -> connection_id`).
    /// The deployment member for this connection requires each bound
    /// connection, so a build deploys them together. Empty otherwise.
    #[serde(default)]
    pub bindings: std::collections::BTreeMap<String, String>,
    pub configuration_digest: Digest,
}

impl ConnectionRevisionRow {
    /// The `(connection_id, revision)` key a revision is stored and cached
    /// under.
    pub fn key(&self) -> (String, u64) {
        (self.connection_id.to_string(), self.revision.get())
    }

    /// This revision, running on `provider` (the reference
    /// [`ImmutableDeploymentStore::provider_ref`] resolves its
    /// `provider_digest` to).
    pub fn reference(&self, provider: ProviderRef) -> ConnectionReference {
        ConnectionReference {
            connection_id: self.connection_id.clone(),
            revision: self.revision,
            provider,
        }
    }

    /// The identity a call over this revision runs under: its provider, the
    /// connection and revision, and a hash of where its secrets come from —
    /// never of a secret, and a revision reading another vault path is
    /// another identity.
    pub fn effective_identity(&self) -> EffectiveConnectionIdentity {
        EffectiveConnectionIdentity {
            provider_digest: self.provider_digest.clone(),
            connection_id: self.connection_id.clone(),
            revision: self.revision,
            secret_hash: if self.secret_locators.is_empty() {
                String::new()
            } else {
                sha256_digest(
                    crate::canonical::canonical_json(&json!(self.secret_locators)).as_bytes(),
                )
                .into_string()
            },
        }
    }

    /// The connection's configuration as a provider reads it: `public_config`
    /// with every secret reference placed at its path. The references are
    /// resolved where they are used, never stored resolved.
    pub fn config_with_locators(&self) -> Value {
        place_locators(&self.public_config, &self.secret_locators)
    }
}

/// A connection's configuration as a provider reads it: `public_config` with
/// each secret reference placed at its locator path. What
/// [`ConnectionRevisionRow::config_with_locators`] answers, for a caller
/// judging a connection before it is stored.
pub fn place_locators(
    public_config: &Value,
    secret_locators: &std::collections::BTreeMap<String, String>,
) -> Value {
    let mut config = public_config.clone();
    if !config.is_object() {
        config = Value::Object(serde_json::Map::new());
    }
    for (path, reference) in secret_locators {
        let mut at = &mut config;
        let mut segments = path.split('.').peekable();
        while let Some(segment) = segments.next() {
            let Some(object) = at.as_object_mut() else {
                break;
            };
            if segments.peek().is_none() {
                object.insert(segment.to_string(), Value::String(reference.clone()));
                break;
            }
            at = object
                .entry(segment.to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
        }
    }
    config
}

/// The bytes a connection revision's `configuration_digest` is taken over:
/// its public settings and where each secret comes from, in the general
/// canonical form (a connector's settings hold booleans and fractions the
/// package encoder refuses).
///
/// `bindings` are covered only when there are some, so a revision without any
/// keeps the digest it had before bindings existed.
pub(super) fn connection_config_bytes(
    public_config: &Value,
    secret_locators: &std::collections::BTreeMap<String, String>,
    bindings: &std::collections::BTreeMap<String, String>,
) -> Vec<u8> {
    let mut doc = serde_json::json!({
        "public_config": public_config,
        "secret_locators": secret_locators,
    });
    if !bindings.is_empty() {
        doc["bindings"] = serde_json::json!(bindings);
    }
    crate::canonical::canonical_json(&doc).into_bytes()
}

/// A binding map as the `bindings_json` column holds it: canonical JSON, or
/// NULL when there is none.
pub(super) fn bindings_column(
    bindings: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    (!bindings.is_empty()).then(|| crate::canonical::canonical_json(&serde_json::json!(bindings)))
}

/// The map a `bindings_json` column holds, read strictly.
pub(super) fn bindings_from_column(
    column: Option<String>,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    match column {
        None => Ok(Default::default()),
        Some(text) => serde_json::from_str(&text)
            .map_err(|e| format!("bindings_json is not an alias map: {e}")),
    }
}

/// A secret locator map as the `secret_locators_json` column holds it: canonical
/// JSON, or NULL when there is none.
pub(super) fn locators_column(
    secret_locators: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    (!secret_locators.is_empty())
        .then(|| crate::canonical::canonical_json(&serde_json::json!(secret_locators)))
}

/// The map a `secret_locators_json` column holds, read strictly.
pub(super) fn locators_from_column(
    column: Option<String>,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    match column {
        None => Ok(Default::default()),
        Some(text) => serde_json::from_str(&text)
            .map_err(|e| format!("secret_locators_json is not a locator map: {e}")),
    }
}

/// Refuse a connection's settings as a revision create refuses them: a
/// secret-shaped key or a credential in `public_config`, or a locator that
/// is not a config path to a secret reference. The check behind
/// `POST /api/v2/admin/connections`, for a caller that builds a connection
/// before sending it.
pub fn check_connection_config(
    public_config: &Value,
    secret_locators: &std::collections::BTreeMap<String, String>,
) -> Result<(), DeploymentError> {
    validate_no_secrets_in_public_config(public_config)?;
    validate_secret_locators(public_config, secret_locators)
}

/// Refuse a secret locator map whose paths are not config paths, whose
/// values are not secret references, or whose paths `public_config` already
/// sets.
fn validate_secret_locators(
    public_config: &Value,
    locators: &std::collections::BTreeMap<String, String>,
) -> Result<(), DeploymentError> {
    for (path, reference) in locators {
        let well_formed = !path.is_empty()
            && path.split('.').all(|seg| {
                !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            });
        if !well_formed {
            return Err(DeploymentError::new(
                Kind::ConnectionSecretLocatorInvalid,
                format!("secret locator path '{path}' is not a dotted config path"),
            ));
        }
        if !crate::connector::secrets::is_secret_reference(reference) {
            return Err(DeploymentError::new(
                Kind::ConnectionSecretLocatorInvalid,
                format!(
                    "secret locator '{path}' must be a secret reference (such as env://VAR), \
                     never the secret itself"
                ),
            ));
        }
        let mut at = Some(public_config);
        for segment in path.split('.') {
            at = at.and_then(|v| v.get(segment));
        }
        if at.is_some() {
            return Err(DeploymentError::new(
                Kind::ConnectionSecretLocatorInvalid,
                format!("'{path}' is set in public_config and by a secret locator; keep one"),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentRevisionRow {
    pub revision: u64,
    pub parent_revision: Option<u64>,
    pub manifest_digest: String,
    pub manifest_bytes: Vec<u8>,
    pub manifest: CanonicalManifest,
    pub reason: String,
    pub created_by: String,
    pub members_compacted: bool,
    /// When the revision was committed, on the database clock.
    #[serde(default)]
    pub created_at: Option<chrono::NaiveDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentReceipt {
    pub operation_id: String,
    pub principal: String,
    pub idempotency_key: String,
    pub request_hash: String,
    pub committed_revision: u64,
    pub manifest_digest: String,
    pub epoch: u64,
    pub receiving_node_id: String,
    /// When the receipt was committed, on the database clock.
    #[serde(default)]
    pub created_at: Option<chrono::NaiveDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidatePlan {
    pub base_revision: u64,
    pub candidate_manifest: CanonicalManifest,
    pub candidate_digest: String,
    pub candidate_bytes: Vec<u8>,
    pub added_entities: Vec<EntityRef>,
    pub removed_entities: Vec<EntityRef>,
    pub external_effect_warnings: Vec<String>,
}

impl CandidatePlan {
    /// Compute the deterministic request hash over `(operation, base_revision,
    /// candidate_digest, removals, reason)` (`V2-03.3`, `V2-04.3`).
    pub fn compute_request_hash(
        &self,
        operation: &str,
        reason: &str,
    ) -> Result<String, DeploymentError> {
        let removals: Vec<Value> = self
            .removed_entities
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "kind": r.kind.as_str(),
                    "revision": r.revision,
                })
            })
            .collect();
        let doc = json!({
            "base_revision": self.base_revision,
            "candidate_digest": self.candidate_digest,
            "operation": operation,
            "reason": reason,
            "removals": removals,
        });
        let bytes = canonical_bytes(&doc)
            .map_err(|e| DeploymentError::new(Kind::RequestHashInvalid, e.code))?;
        Ok(sha256_digest(&bytes).into_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntryInput {
    pub path: String,
    pub bytes: Vec<u8>,
    pub compressed_size: usize,
    pub is_symlink: bool,
}

/// An apply request: commit `plan` as the next revision, under `principal`'s
/// idempotency key. The apply's counterpart of [`Rollback`].
#[derive(Debug, Clone, Copy)]
pub struct Apply<'a> {
    pub principal: &'a str,
    pub idempotency_key: &'a str,
    pub plan: &'a CandidatePlan,
    pub reason: &'a str,
}

/// A new immutable connection revision: its public configuration, the
/// secret locators it resolves at load, and — for a connection over a
/// `workflow` provider package — the dependency `bindings`.
#[derive(Debug, Clone)]
pub struct NewConnectionRevision<'a> {
    pub connection_id: &'a str,
    pub revision: u64,
    pub provider_digest: &'a str,
    pub public_config: serde_json::Value,
    pub secret_locators: std::collections::BTreeMap<String, String>,
    pub bindings: std::collections::BTreeMap<String, String>,
}

/// A rollback request: re-deploy `target_revision`'s manifest as a new
/// revision on top of `expected_head_revision`.
#[derive(Debug, Clone, Copy)]
pub struct Rollback<'a> {
    pub principal: &'a str,
    pub idempotency_key: &'a str,
    pub expected_head_revision: u64,
    pub target_revision: u64,
    pub reason: &'a str,
}

/// What one garbage-collection pass changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcOutcome {
    /// Marked `DRAINING` this pass: unreferenced, bytes still held.
    pub draining: Vec<String>,
    /// Moved to `DELETED` this pass: their bytes are gone.
    pub deleted: Vec<String>,
    /// The bytes the deleted artifacts held.
    pub deleted_bytes: u64,
    /// `DRAINING` artifacts something references again, put back to `ACTIVE`.
    pub revived: Vec<String>,
    /// Revisions whose member rows were compacted.
    pub compacted: Vec<u64>,
}

impl GcOutcome {
    /// Whether the pass changed nothing (and so moved no epoch).
    pub fn is_empty(&self) -> bool {
        self.draining.is_empty()
            && self.deleted.is_empty()
            && self.revived.is_empty()
            && self.compacted.is_empty()
    }
}

/// The deployment store (`V2-03`, `V2-04`): the database, through the
/// transactions in `writes`, and the `catalog` a node reads its serving
/// path from. Cloning shares one catalog.
#[derive(Clone)]
pub struct ImmutableDeploymentStore {
    node_id: Arc<RwLock<String>>,
    catalog: Arc<RwLock<catalog::Catalog>>,
    /// Held by every mutation and refresh on this node, so the catalog is
    /// refreshed in commit order. Planning does not take it.
    write_lock: Arc<tokio::sync::Mutex<()>>,
    /// The revisions each node reports holding (applied, or retained while a
    /// generation drains): the collector keeps them whole.
    held: Arc<Mutex<BTreeMap<String, BTreeSet<u64>>>>,
}

impl Default for ImmutableDeploymentStore {
    fn default() -> Self {
        Self::new("node-local-1")
    }
}

impl ImmutableDeploymentStore {
    pub fn new(node_id: impl Into<String>) -> Self {
        Self {
            node_id: Arc::new(RwLock::new(node_id.into())),
            catalog: Arc::default(),
            write_lock: Arc::default(),
            held: Arc::default(),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, catalog::Catalog> {
        self.catalog.read().expect("catalog lock")
    }

    pub fn node_id(&self) -> String {
        self.node_id.read().expect("node id lock").clone()
    }

    pub fn set_node_id(&self, node_id: impl Into<String>) {
        *self.node_id.write().expect("node id lock") = node_id.into();
    }

    /// `(head_revision, epoch)` as the catalog last read them.
    pub fn current_head(&self) -> (u64, u64) {
        self.read().head
    }

    pub fn get_revision(&self, revision: u64) -> Option<Arc<DeploymentRevisionRow>> {
        self.read().revisions.get(&revision).cloned()
    }

    pub fn list_revisions(&self) -> Vec<Arc<DeploymentRevisionRow>> {
        self.read().revisions.values().cloned().collect()
    }

    pub fn list_packages(&self) -> Vec<Arc<ProviderPackageRow>> {
        self.read().packages.values().cloned().collect()
    }

    /// The package stored under `package_digest`.
    pub fn get_package(&self, package_digest: &str) -> Option<Arc<ProviderPackageRow>> {
        self.read().packages.get(package_digest).cloned()
    }

    pub fn list_admissions(&self) -> Vec<Arc<ProviderAdmissionRow>> {
        self.read().admissions.values().cloned().collect()
    }

    pub fn get_admission(&self, package_digest: &str) -> Option<Arc<ProviderAdmissionRow>> {
        self.read().admissions.get(package_digest).cloned()
    }

    pub fn list_connections(&self) -> Vec<Arc<ConnectionRevisionRow>> {
        self.read().connections.values().cloned().collect()
    }

    pub fn get_connection(
        &self,
        connection_id: &str,
        revision: Revision,
    ) -> Option<Arc<ConnectionRevisionRow>> {
        self.read()
            .connections
            .get(&(connection_id.to_string(), revision.get()))
            .cloned()
    }

    /// The pinned identity of the provider `digest` names: a built-in
    /// driver's synthetic one, or the stored package's. `None` when it is
    /// neither.
    pub fn provider_ref(&self, digest: &Digest) -> Option<ProviderRef> {
        super::foundation::builtin_provider_ref(digest)
            .or_else(|| self.get_package(digest).map(|pkg| pkg.provider_ref()))
    }

    /// Resolve the active connection revision for `connection_id`: when a
    /// deployment revision is active (`head_revision > 0`) and declares a
    /// `Connection` member for `connection_id`, return that exact revision;
    /// otherwise return the highest stored revision for `connection_id`.
    pub fn get_active_connection(&self, connection_id: &str) -> Option<Arc<ConnectionRevisionRow>> {
        let c = self.read();
        let (head, _) = c.head;
        if head > 0
            && let Some(rev) = c.revisions.get(&head)
            && let Some(member) =
                rev.manifest.members.iter().find(|m| {
                    m.entity.kind == EntityKind::Connection && m.entity.id == connection_id
                })
        {
            return c
                .connections
                .get(&(connection_id.to_string(), member.entity.revision.get()))
                .cloned();
        }
        c.connections
            .range((connection_id.to_string(), 0)..=(connection_id.to_string(), u64::MAX))
            .next_back()
            .map(|(_, row)| Arc::clone(row))
    }

    /// The schemas `package_digest`'s package declares, compiled as its
    /// upload checked them. `None` when no package is stored under that
    /// digest (a built-in driver's, or none at all); an error when a schema
    /// file the package lists is no longer stored.
    pub fn package_schemas(
        &self,
        package_digest: &str,
    ) -> Option<Result<Arc<CompiledSchemaBundle>, DeploymentError>> {
        self.read().bundles.get(package_digest).cloned()
    }

    /// Check `instance` against the schema `package_digest`'s manifest
    /// declares for `slot`: its `configuration_schema`, or an operation's
    /// `input_schema`. `None` when no package is stored under that digest;
    /// `Ok` when the manifest declares no schema there.
    pub fn check_package_instance(
        &self,
        package_digest: &str,
        slot: PackageSchema<'_>,
        instance: &Value,
    ) -> Option<Result<(), SchemaProfileFinding>> {
        let package = self.get_package(package_digest)?;
        let declared = match slot {
            PackageSchema::Configuration => package.manifest_json.get("configuration_schema"),
            PackageSchema::OperationInput(operation) => package
                .manifest_json
                .get("operations")
                .and_then(|ops| ops.get(operation))
                .and_then(|op| op.get("input_schema")),
        };
        let Some(path) = declared.and_then(Value::as_str) else {
            return Some(Ok(()));
        };
        Some(match self.package_schemas(package_digest)? {
            Ok(bundle) => bundle.validate_instance(path, instance),
            Err(e) => Err(SchemaProfileFinding::new(e.code(), e.message)),
        })
    }

    /// Record the revisions currently held (applied + retained in-flight) by a node.
    pub fn update_node_retained_revisions(&self, node_id: &str, revisions: BTreeSet<u64>) {
        self.held
            .lock()
            .expect("held revisions lock")
            .insert(node_id.to_string(), revisions);
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::panic)]
mod tests;
