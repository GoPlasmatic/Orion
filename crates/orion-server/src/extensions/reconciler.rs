//! V2-06 & V2-05.2 Node reconciler, candidate builder with whole-build vs
//! per-entity quarantine semantics, non-superseded atomic publication,
//! jittered backoff, convergence wait (`this-node` / `observed`), and
//! fail-closed DB-time security lease.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::deployment_store::{DeploymentError, EntityRef, ImmutableDeploymentStore};

pub const MIN_BACKOFF_MS: u64 = 250;
pub const MAX_BACKOFF_MS: u64 = 30_000;
pub const DEFAULT_CONVERGENCE_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_SECURITY_LEASE_MS: u64 = 5_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub node_id: String,
    pub boot_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CandidateBuildIssue {
    /// Whole-build integrity, admission, or workflow compilation failure:
    /// aborts publication and retains the last known-good serving generation.
    WholeBuildIntegrity { code: String, message: String },
    /// Per-channel or per-connection external secret/endpoint transient failure:
    /// quarantines only the affected entity and publishes unaffected members
    /// with `configuration_degraded: true`.
    PerEntityExternal { entity: EntityRef, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantinedEntity {
    pub entity: EntityRef,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeNodeStatus {
    pub identity: NodeIdentity,
    pub desired_revision: u64,
    pub applied_revision: u64,
    pub applied_manifest_digest: Option<String>,
    pub configuration_degraded: bool,
    pub quarantined_entities: Vec<QuarantinedEntity>,
    pub last_error: Option<String>,
    pub backoff_attempt: u32,
    pub backoff_delay_ms: u64,
    pub security_lease_expires_at_ms: u64,
    pub retained_revisions: BTreeSet<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConvergenceTarget {
    /// Wait for the specific node that accepted the write (`receiving_node_id`).
    ThisNode(String),
    /// Wait for the fixed set of active nodes observed when the wait began.
    ObservedNodes(BTreeSet<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvergenceReport {
    pub converged: bool,
    pub target_revision: u64,
    pub converged_nodes: Vec<String>,
    pub pending_nodes: Vec<String>,
    pub degraded_nodes: Vec<String>,
}

/// Compute exponential backoff in `[250ms, 30_000ms]` with deterministic jitter (`V2-06.4`).
pub fn compute_reconcile_backoff_ms(attempt: u32, jitter_seed: u64) -> u64 {
    if attempt == 0 {
        return 0;
    }
    let exp = attempt.saturating_sub(1).min(10);
    let base = (MIN_BACKOFF_MS.saturating_mul(1u64 << exp)).min(MAX_BACKOFF_MS);
    let jitter = (jitter_seed % 51).saturating_mul(base) / 1000; // up to +5% jitter
    (base + jitter).min(MAX_BACKOFF_MS)
}

#[derive(Clone)]
pub struct NodeReconciler {
    store: ImmutableDeploymentStore,
    nodes: Arc<Mutex<BTreeMap<String, RuntimeNodeStatus>>>,
    local_identity: NodeIdentity,
}

impl NodeReconciler {
    pub fn new(
        store: ImmutableDeploymentStore,
        node_id: impl Into<String>,
        boot_id: impl Into<String>,
    ) -> Self {
        let node_id_str = node_id.into();
        store.set_node_id(node_id_str.clone());
        let identity = NodeIdentity {
            node_id: node_id_str,
            boot_id: boot_id.into(),
        };
        let mut map = BTreeMap::new();
        map.insert(
            identity.node_id.clone(),
            RuntimeNodeStatus {
                identity: identity.clone(),
                desired_revision: 0,
                applied_revision: 0,
                applied_manifest_digest: None,
                configuration_degraded: false,
                quarantined_entities: Vec::new(),
                last_error: None,
                backoff_attempt: 0,
                backoff_delay_ms: 0,
                security_lease_expires_at_ms: DEFAULT_SECURITY_LEASE_MS,
                retained_revisions: BTreeSet::new(),
            },
        );
        Self {
            store,
            nodes: Arc::new(Mutex::new(map)),
            local_identity: identity,
        }
    }

    pub fn local_status(&self) -> RuntimeNodeStatus {
        let g = self.nodes.lock().expect("nodes lock");
        g[&self.local_identity.node_id].clone()
    }

    pub fn observed_node_ids(&self) -> BTreeSet<String> {
        let g = self.nodes.lock().expect("nodes lock");
        g.keys().cloned().collect()
    }

    pub fn register_peer_node(&self, node_id: &str, boot_id: &str) {
        let mut g = self.nodes.lock().expect("nodes lock");
        g.insert(
            node_id.to_string(),
            RuntimeNodeStatus {
                identity: NodeIdentity {
                    node_id: node_id.to_string(),
                    boot_id: boot_id.to_string(),
                },
                desired_revision: 0,
                applied_revision: 0,
                applied_manifest_digest: None,
                configuration_degraded: false,
                quarantined_entities: Vec::new(),
                last_error: None,
                backoff_attempt: 0,
                backoff_delay_ms: 0,
                security_lease_expires_at_ms: DEFAULT_SECURITY_LEASE_MS,
                retained_revisions: BTreeSet::new(),
            },
        );
    }

    /// V2-06.1: Poll the durable `deployment_head` from the store. When a new
    /// desired revision is observed, reset any prior failure backoff.
    pub fn observe_desired_head(&self) -> u64 {
        let (head_rev, _) = self.store.current_head();
        let mut g = self.nodes.lock().expect("nodes lock");
        if let Some(node) = g.get_mut(&self.local_identity.node_id)
            && head_rev > node.desired_revision
        {
            node.desired_revision = head_rev;
            node.backoff_attempt = 0;
            node.backoff_delay_ms = 0;
        }
        head_rev
    }

    /// V2-06.2 & V2-06.3: Build a candidate outside write locks and publish only
    /// if not superseded by a newer desired or applied revision.
    pub fn build_and_publish_candidate(
        &self,
        candidate_revision: u64,
        issues: Vec<CandidateBuildIssue>,
    ) -> Result<RuntimeNodeStatus, DeploymentError> {
        // 1. Read candidate manifest outside lock
        let rev_row = self.store.get_revision(candidate_revision).ok_or_else(|| {
            DeploymentError::new(
                "CANDIDATE_REVISION_MISSING",
                format!("revision {candidate_revision} not found"),
            )
        })?;

        // 2. Check for whole-build integrity failure vs per-entity quarantine
        let mut quarantined = Vec::new();
        for issue in issues {
            match issue {
                CandidateBuildIssue::WholeBuildIntegrity { code, message } => {
                    let mut g = self.nodes.lock().expect("nodes lock");
                    let node = g
                        .get_mut(&self.local_identity.node_id)
                        .expect("local node registered");
                    node.backoff_attempt += 1;
                    node.backoff_delay_ms =
                        compute_reconcile_backoff_ms(node.backoff_attempt, candidate_revision);
                    node.last_error = Some(format!("{code}: {message}"));
                    return Err(DeploymentError::new(code, message));
                }
                CandidateBuildIssue::PerEntityExternal { entity, reason } => {
                    quarantined.push(QuarantinedEntity { entity, reason });
                }
            }
        }

        // 3. Publication barrier: verify candidate is not superseded by desired or applied revision
        let (latest_head, _) = self.store.current_head();
        let mut g = self.nodes.lock().expect("nodes lock");
        let node = g
            .get_mut(&self.local_identity.node_id)
            .expect("local node registered");
        if latest_head > node.desired_revision {
            node.desired_revision = latest_head;
        }
        if candidate_revision < node.desired_revision || candidate_revision <= node.applied_revision
        {
            return Err(DeploymentError::new(
                "CANDIDATE_SUPERSEDED",
                format!(
                    "candidate revision {candidate_revision} is superseded (applied={}, desired={})",
                    node.applied_revision, node.desired_revision
                ),
            ));
        }

        // 4. Atomic swap & applied-state update
        node.applied_revision = candidate_revision;
        node.applied_manifest_digest = Some(rev_row.manifest_digest);
        node.configuration_degraded = !quarantined.is_empty();
        node.quarantined_entities = quarantined;
        node.last_error = None;
        node.backoff_attempt = 0;
        node.backoff_delay_ms = 0;
        node.retained_revisions.insert(candidate_revision);
        let status = node.clone();
        drop(g);

        self.store.update_node_retained_revisions(
            &self.local_identity.node_id,
            status.retained_revisions.clone(),
        );
        Ok(status)
    }

    /// Release an old drained generation from this node's retained set.
    pub fn release_retained_revision(&self, revision: u64) {
        let mut g = self.nodes.lock().expect("nodes lock");
        if let Some(node) = g.get_mut(&self.local_identity.node_id) {
            if node.applied_revision != revision {
                node.retained_revisions.remove(&revision);
            }
            let held = node.retained_revisions.clone();
            drop(g);
            self.store
                .update_node_retained_revisions(&self.local_identity.node_id, held);
        }
    }

    /// V2-06.4: Evaluate convergence against `ThisNode` or a fixed `ObservedNodes` set.
    pub fn evaluate_convergence(
        &self,
        target_revision: u64,
        target: &ConvergenceTarget,
    ) -> ConvergenceReport {
        let g = self.nodes.lock().expect("nodes lock");
        let target_ids: Vec<String> = match target {
            ConvergenceTarget::ThisNode(id) => vec![id.clone()],
            ConvergenceTarget::ObservedNodes(ids) => ids.iter().cloned().collect(),
        };

        let mut converged_nodes = Vec::new();
        let mut pending_nodes = Vec::new();
        let mut degraded_nodes = Vec::new();

        for id in target_ids {
            match g.get(&id) {
                Some(node) if node.applied_revision >= target_revision => {
                    converged_nodes.push(id.clone());
                    if node.configuration_degraded {
                        degraded_nodes.push(id);
                    }
                }
                _ => pending_nodes.push(id),
            }
        }

        ConvergenceReport {
            converged: pending_nodes.is_empty(),
            target_revision,
            converged_nodes,
            pending_nodes,
            degraded_nodes,
        }
    }

    /// V2-06.5: Renew the node's DB-time security lease and check validity.
    pub fn renew_security_lease(&self, now_ms: u64, lease_duration_ms: u64) {
        let mut g = self.nodes.lock().expect("nodes lock");
        if let Some(node) = g.get_mut(&self.local_identity.node_id) {
            node.security_lease_expires_at_ms = now_ms.saturating_add(lease_duration_ms);
        }
    }

    pub fn is_security_lease_valid(&self, now_ms: u64) -> bool {
        let g = self.nodes.lock().expect("nodes lock");
        g.get(&self.local_identity.node_id)
            .is_some_and(|node| now_ms <= node.security_lease_expires_at_ms)
    }

    /// Persist this node's [`RuntimeNodeStatus`] to the `runtime_nodes` table across
    /// any configured [`crate::storage::DbPool`] (`TD-18`).
    pub async fn persist_local_status_to_db_pool(
        &self,
        pool: &crate::storage::DbPool,
    ) -> Result<(), DeploymentError> {
        let status = self.local_status();
        let retained_vec: Vec<u64> = status.retained_revisions.iter().copied().collect();
        let retained_json =
            serde_json::to_string(&retained_vec).unwrap_or_else(|_| "[]".to_string());

        match pool {
            crate::storage::DbPool::Sqlite(p) => {
                sqlx::query(
                    "INSERT INTO runtime_nodes (node_id, boot_id, desired_revision, applied_revision, applied_manifest_digest, configuration_degraded, retained_revisions_json, security_lease_until, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) \
                     ON CONFLICT(node_id) DO UPDATE SET \
                       boot_id = excluded.boot_id, \
                       desired_revision = excluded.desired_revision, \
                       applied_revision = excluded.applied_revision, \
                       applied_manifest_digest = excluded.applied_manifest_digest, \
                       configuration_degraded = excluded.configuration_degraded, \
                       retained_revisions_json = excluded.retained_revisions_json, \
                       security_lease_until = CURRENT_TIMESTAMP, \
                       updated_at = CURRENT_TIMESTAMP",
                )
                .bind(&status.identity.node_id)
                .bind(&status.identity.boot_id)
                .bind(status.desired_revision as i64)
                .bind(status.applied_revision as i64)
                .bind(status.applied_manifest_digest.as_deref())
                .bind(status.configuration_degraded)
                .bind(&retained_json)
                .execute(p)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
            }
            crate::storage::DbPool::Postgres(p) => {
                sqlx::query(
                    "INSERT INTO runtime_nodes (node_id, boot_id, desired_revision, applied_revision, applied_manifest_digest, configuration_degraded, retained_revisions_json, security_lease_until, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) \
                     ON CONFLICT(node_id) DO UPDATE SET \
                       boot_id = EXCLUDED.boot_id, \
                       desired_revision = EXCLUDED.desired_revision, \
                       applied_revision = EXCLUDED.applied_revision, \
                       applied_manifest_digest = EXCLUDED.applied_manifest_digest, \
                       configuration_degraded = EXCLUDED.configuration_degraded, \
                       retained_revisions_json = EXCLUDED.retained_revisions_json, \
                       security_lease_until = CURRENT_TIMESTAMP, \
                       updated_at = CURRENT_TIMESTAMP",
                )
                .bind(&status.identity.node_id)
                .bind(&status.identity.boot_id)
                .bind(status.desired_revision as i64)
                .bind(status.applied_revision as i64)
                .bind(status.applied_manifest_digest.as_deref())
                .bind(status.configuration_degraded)
                .bind(&retained_json)
                .execute(p)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
            }
            crate::storage::DbPool::Mysql(p) => {
                sqlx::query(
                    "INSERT INTO runtime_nodes (node_id, boot_id, desired_revision, applied_revision, applied_manifest_digest, configuration_degraded, retained_revisions_json, security_lease_until, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) \
                     ON DUPLICATE KEY UPDATE \
                       boot_id = VALUES(boot_id), \
                       desired_revision = VALUES(desired_revision), \
                       applied_revision = VALUES(applied_revision), \
                       applied_manifest_digest = VALUES(applied_manifest_digest), \
                       configuration_degraded = VALUES(configuration_degraded), \
                       retained_revisions_json = VALUES(retained_revisions_json), \
                       security_lease_until = CURRENT_TIMESTAMP, \
                       updated_at = CURRENT_TIMESTAMP",
                )
                .bind(&status.identity.node_id)
                .bind(&status.identity.boot_id)
                .bind(status.desired_revision as i64)
                .bind(status.applied_revision as i64)
                .bind(status.applied_manifest_digest.as_deref())
                .bind(status.configuration_degraded)
                .bind(&retained_json)
                .execute(p)
                .await
                .map_err(|e| DeploymentError::new("DB_WRITE_ERROR", e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Load all observed cluster nodes from the `runtime_nodes` table into `self.nodes`
    /// and update `self.store`'s retained revision sets (`TD-18`).
    pub async fn refresh_observed_nodes_from_db_pool(
        &self,
        pool: &crate::storage::DbPool,
    ) -> Result<BTreeSet<String>, DeploymentError> {
        type PersistedNodeTuple = (String, String, i64, i64, Option<String>, bool, String);
        let query_sql = "SELECT node_id, boot_id, desired_revision, applied_revision, applied_manifest_digest, configuration_degraded, retained_revisions_json FROM runtime_nodes";
        let rows: Vec<PersistedNodeTuple> = match pool {
            crate::storage::DbPool::Sqlite(p) => match sqlx::query_as(query_sql).fetch_all(p).await
            {
                Ok(r) => r,
                Err(e) if e.to_string().contains("no such table") => {
                    return Ok(self.observed_node_ids());
                }
                Err(e) => return Err(DeploymentError::new("DB_READ_ERROR", e.to_string())),
            },
            crate::storage::DbPool::Postgres(p) => {
                match sqlx::query_as(query_sql).fetch_all(p).await {
                    Ok(r) => r,
                    Err(e) if e.to_string().contains("does not exist") => {
                        return Ok(self.observed_node_ids());
                    }
                    Err(e) => return Err(DeploymentError::new("DB_READ_ERROR", e.to_string())),
                }
            }
            crate::storage::DbPool::Mysql(p) => {
                match sqlx::query_as(query_sql).fetch_all(p).await {
                    Ok(r) => r,
                    Err(e) if e.to_string().contains("doesn't exist") => {
                        return Ok(self.observed_node_ids());
                    }
                    Err(e) => return Err(DeploymentError::new("DB_READ_ERROR", e.to_string())),
                }
            }
        };

        let mut retained_updates = Vec::new();
        {
            let mut g = self.nodes.lock().expect("nodes lock");
            for (
                node_id,
                boot_id,
                desired_rev,
                applied_rev,
                manifest_digest,
                degraded,
                retained_str,
            ) in rows
            {
                let retained: BTreeSet<u64> = serde_json::from_str::<Vec<u64>>(&retained_str)
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                retained_updates.push((node_id.clone(), retained.clone()));

                if node_id == self.local_identity.node_id {
                    continue;
                }
                g.insert(
                    node_id.clone(),
                    RuntimeNodeStatus {
                        identity: NodeIdentity { node_id, boot_id },
                        desired_revision: desired_rev.max(0) as u64,
                        applied_revision: applied_rev.max(0) as u64,
                        applied_manifest_digest: manifest_digest,
                        configuration_degraded: degraded,
                        quarantined_entities: Vec::new(),
                        last_error: None,
                        backoff_attempt: 0,
                        backoff_delay_ms: 0,
                        security_lease_expires_at_ms: DEFAULT_SECURITY_LEASE_MS,
                        retained_revisions: retained,
                    },
                );
            }
        }
        for (node_id, retained) in retained_updates {
            self.store
                .update_node_retained_revisions(&node_id, retained);
        }
        Ok(self.observed_node_ids())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::extensions::CommunityAdmissionPolicy;
    use crate::extensions::deployment_store::{CanonicalManifest, EntityKind, ManifestMember};
    use crate::extensions::package_identity::sha256_digest;

    #[test]
    fn v2_06_1_2_3_4_5_reconciler_quarantines_per_entity_discards_superseded_and_enforces_lease() {
        let store = ImmutableDeploymentStore::new("node-a");
        let policy = CommunityAdmissionPolicy::new();
        let reconciler = NodeReconciler::new(store.clone(), "node-a", "boot-1");

        // Commit revisions 1 and 2
        for rev in 1..=2 {
            let m = CanonicalManifest {
                schema_version: 2,
                providers: Vec::new(),
                provider_lock_digest: None,
                members: vec![ManifestMember {
                    entity: EntityRef {
                        kind: EntityKind::Channel,
                        id: "orders".to_string(),
                        revision: rev,
                    },
                    definition_digest: sha256_digest(format!("ch-{rev}").as_bytes()),
                    owning_package: None,
                    requires: Vec::new(),
                    rollouts: Vec::new(),
                }],
            };
            let plan = store.plan_candidate(rev - 1, m).unwrap();
            store
                .apply_candidate("admin", &format!("k-{rev}"), &plan, "deploy", &policy)
                .unwrap();
        }

        // Observe head = 2
        assert_eq!(reconciler.observe_desired_head(), 2);

        // Superseded candidate (revision 1 when desired is 2) is discarded
        let sup_err = reconciler
            .build_and_publish_candidate(1, Vec::new())
            .unwrap_err();
        assert_eq!(sup_err.code, "CANDIDATE_SUPERSEDED");
        assert_eq!(reconciler.local_status().applied_revision, 0);

        // Whole-build integrity failure retains applied_revision = 0 and sets backoff
        let int_err = reconciler
            .build_and_publish_candidate(
                2,
                vec![CandidateBuildIssue::WholeBuildIntegrity {
                    code: "WORKFLOW_COMPILE_ERROR".to_string(),
                    message: "bad task".to_string(),
                }],
            )
            .unwrap_err();
        assert_eq!(int_err.code, "WORKFLOW_COMPILE_ERROR");
        assert_eq!(reconciler.local_status().applied_revision, 0);
        assert!(reconciler.local_status().backoff_delay_ms >= MIN_BACKOFF_MS);

        // Per-entity external failure quarantines only that entity and publishes revision 2 as degraded
        let status = reconciler
            .build_and_publish_candidate(
                2,
                vec![CandidateBuildIssue::PerEntityExternal {
                    entity: EntityRef {
                        kind: EntityKind::Channel,
                        id: "orders".to_string(),
                        revision: 2,
                    },
                    reason: "vault secret timeout".to_string(),
                }],
            )
            .expect("published degraded");
        assert_eq!(status.applied_revision, 2);
        assert!(status.configuration_degraded);
        assert_eq!(status.quarantined_entities.len(), 1);

        // Convergence check for ThisNode("node-a")
        let conv =
            reconciler.evaluate_convergence(2, &ConvergenceTarget::ThisNode("node-a".to_string()));
        assert!(conv.converged);
        assert_eq!(conv.degraded_nodes, vec!["node-a".to_string()]);

        // Security lease check
        reconciler.renew_security_lease(1_000, 5_000);
        assert!(reconciler.is_security_lease_valid(5_500));
        assert!(!reconciler.is_security_lease_valid(6_500));
    }

    #[tokio::test]
    async fn td18_td34_runtime_nodes_sqlite_persistence_and_observed_convergence() {
        use std::str::FromStr;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
            .await
            .unwrap();
        crate::storage::migrator_for(crate::storage::DbBackend::Sqlite)
            .run(&pool)
            .await
            .unwrap();
        let db_pool = crate::storage::DbPool::Sqlite(pool);

        let root_a = crate::extensions::CompositionRoot::community()
            .with_node_identity("node-alpha", "boot-alpha");
        assert_eq!(
            root_a.deployments.node_id(),
            "node-alpha",
            "with_node_identity must synchronize store and reconciler node_id"
        );

        let policy = CommunityAdmissionPolicy::new();
        let m = CanonicalManifest {
            schema_version: 2,
            providers: Vec::new(),
            provider_lock_digest: None,
            members: Vec::new(),
        };
        let plan = root_a.deployments.plan_candidate(0, m).unwrap();
        let receipt = root_a
            .deployments
            .apply_candidate("admin", "idem-obs", &plan, "deploy", &policy)
            .unwrap();
        assert_eq!(receipt.receiving_node_id, "node-alpha");

        root_a.reconciler.observe_desired_head();
        root_a
            .reconciler
            .build_and_publish_candidate(1, Vec::new())
            .unwrap();
        root_a
            .reconciler
            .persist_local_status_to_db_pool(&db_pool)
            .await
            .unwrap();

        // Second node ("node-beta") has not yet applied revision 1
        let store_b = ImmutableDeploymentStore::new("node-beta");
        let rec_b = NodeReconciler::new(store_b, "node-beta", "boot-beta");
        rec_b
            .persist_local_status_to_db_pool(&db_pool)
            .await
            .unwrap();

        let observed = root_a
            .reconciler
            .refresh_observed_nodes_from_db_pool(&db_pool)
            .await
            .unwrap();
        assert!(observed.contains("node-alpha"));
        assert!(observed.contains("node-beta"));

        let conv_pending = root_a
            .reconciler
            .evaluate_convergence(1, &ConvergenceTarget::ObservedNodes(observed.clone()));
        assert!(!conv_pending.converged);
        assert_eq!(conv_pending.pending_nodes, vec!["node-beta".to_string()]);
    }
}
