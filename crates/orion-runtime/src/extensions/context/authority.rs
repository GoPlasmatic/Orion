use std::collections::{BTreeMap, BTreeSet};

use orion_extension_api::ConnectionReference;

/// Authority projection carried by a `RunContext`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthorityGrant {
    /// Allowed `(connection_id, operation)` pairs in this context.
    pub allowed_operations: BTreeMap<String, BTreeSet<String>>,
    /// Declared dependency aliases mapped to `(connection_id, allowed_operations)`.
    pub declared_dependencies: BTreeMap<String, (ConnectionReference, BTreeSet<String>)>,
    /// Allowed channel targets for nested `channel_call` or workflow sub-channels.
    pub allowed_channels: BTreeSet<String>,
}

impl AuthorityGrant {
    /// Whether `operation` on `connection_id` is granted.
    pub fn allows(&self, connection_id: &str, operation: &str) -> bool {
        self.allowed_operations
            .get(connection_id)
            .is_some_and(|ops| ops.contains(operation))
    }

    /// Add every operation `other` grants.
    pub fn extend(&mut self, other: &AuthorityGrant) {
        for (conn, ops) in &other.allowed_operations {
            self.allowed_operations
                .entry(conn.clone())
                .or_default()
                .extend(ops.iter().cloned());
        }
        for (alias, dep) in &other.declared_dependencies {
            self.declared_dependencies
                .entry(alias.clone())
                .or_insert_with(|| dep.clone());
        }
        self.allowed_channels
            .extend(other.allowed_channels.iter().cloned());
    }

    pub fn intersect(&self, requested: &AuthorityGrant) -> Self {
        let mut allowed_operations = BTreeMap::new();
        for (conn_id, req_ops) in &requested.allowed_operations {
            if let Some(parent_ops) = self.allowed_operations.get(conn_id) {
                let intersected: BTreeSet<String> =
                    parent_ops.intersection(req_ops).cloned().collect();
                if !intersected.is_empty() {
                    allowed_operations.insert(conn_id.clone(), intersected);
                }
            }
        }
        let mut declared_dependencies = BTreeMap::new();
        for (alias, (conn_ref, req_ops)) in &requested.declared_dependencies {
            if let Some((parent_ref, parent_ops)) = self.declared_dependencies.get(alias)
                && parent_ref == conn_ref
            {
                let intersected: BTreeSet<String> =
                    parent_ops.intersection(req_ops).cloned().collect();
                if !intersected.is_empty() {
                    declared_dependencies.insert(alias.clone(), (conn_ref.clone(), intersected));
                }
            }
        }
        let allowed_channels = self
            .allowed_channels
            .intersection(&requested.allowed_channels)
            .cloned()
            .collect();

        Self {
            allowed_operations,
            declared_dependencies,
            allowed_channels,
        }
    }
}
