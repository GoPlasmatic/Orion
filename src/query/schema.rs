//! The entity/relation schema (`EntityRegistry`).
//!
//! Declared inline in the `data_query` input (a `schema` field). It is privileged
//! configuration authored alongside the query — never built from request input —
//! adding, only when wanted: renames (logical→physical), type hints, a field
//! allowlist, and the relation declarations that `some`/`all`/`none` require.
//!
//! With no schema the registry is empty and `UnmappedPolicy::Identity` applies:
//! every field resolves to itself with `FieldType::Unknown` (identity mode).

use std::collections::HashMap;

use serde::Deserialize;

use crate::query::error::QueryError;
use crate::query::ir::{EsStorage, FieldRef, FieldType, JunctionRef, MongoStorage, RelRef};

/// The set of entities queryable through the dialect, plus the unmapped policy.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct EntityRegistry {
    pub entities: HashMap<String, Entity>,
    pub unmapped: UnmappedPolicy,
}

/// What to do with a field not declared on its entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnmappedPolicy {
    /// Reject any field not declared (allowlist mode).
    Reject,
    /// Treat the field name as the physical name (identity mode) — the default.
    #[default]
    Identity,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Entity {
    /// Physical table/collection/index; defaults to the entity's key.
    pub physical: Option<String>,
    pub columns: HashMap<String, Column>,
    pub relations: HashMap<String, Relation>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Column {
    /// Physical column name; defaults to the logical key.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type", default)]
    pub ty: FieldType,
    #[serde(default = "default_true")]
    pub queryable: bool,
    /// Whether `data_write` may assign this column. Marks read-only / generated
    /// columns (identity/serial, computed) as non-writable. Defaults to true.
    #[serde(default = "default_true")]
    pub writable: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct Relation {
    /// Target entity name.
    pub to: String,
    /// Declared cardinality. Optional and assertion-only: query planning keys
    /// off `through` (present = many-to-many), so a declared `kind` is checked
    /// against the relation's shape at parse time rather than trusted.
    #[serde(default)]
    pub kind: Option<Cardinality>,
    /// Column on the current entity (the join's local side).
    pub local: String,
    /// Column on the target (or junction) referencing the current entity.
    pub foreign: String,
    /// Junction table for a many-to-many relation.
    #[serde(default)]
    pub through: Option<Junction>,
    /// Mongo storage hint (used by the Mongo renderer in later phases).
    #[serde(default)]
    pub mongo: MongoStorage,
    /// ES storage hint (used by the ES renderer in later phases).
    #[serde(default)]
    pub es: EsStorage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    HasOne,
    HasMany,
    ManyToMany,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Junction {
    pub table: String,
    /// Junction column joining to the current entity's `local`.
    pub local: String,
    /// Junction column joining to the target's `foreign`.
    pub foreign: String,
}

impl EntityRegistry {
    /// Parse an inline schema JSON value into a registry.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, QueryError> {
        let reg: Self = serde_json::from_value(v.clone())
            .map_err(|e| QueryError::InvalidEnvelope(format!("invalid schema: {e}")))?;
        for (entity, ent) in &reg.entities {
            for (name, rel) in &ent.relations {
                let mismatch = match rel.kind {
                    Some(Cardinality::ManyToMany) if rel.through.is_none() => {
                        Some("kind 'many_to_many' requires a 'through' junction")
                    }
                    Some(Cardinality::HasOne | Cardinality::HasMany) if rel.through.is_some() => {
                        Some("a 'through' junction requires kind 'many_to_many'")
                    }
                    _ => None,
                };
                if let Some(why) = mismatch {
                    return Err(QueryError::InvalidEnvelope(format!(
                        "invalid schema: relation '{entity}.{name}': {why}"
                    )));
                }
            }
        }
        Ok(reg)
    }

    /// Physical table/collection/index for an entity (declared, else the name).
    pub fn physical_table(&self, entity: &str) -> String {
        self.entities
            .get(entity)
            .and_then(|e| e.physical.clone())
            .unwrap_or_else(|| entity.to_string())
    }

    /// Resolve a single-segment field on `entity` to a physical [`FieldRef`],
    /// honouring renames, type hints, and the allowlist.
    pub fn resolve_field(
        &self,
        entity: &str,
        name: &str,
        at: &str,
    ) -> Result<FieldRef, QueryError> {
        if let Some(col) = self.entities.get(entity).and_then(|e| e.columns.get(name)) {
            if !col.queryable {
                return Err(QueryError::InvalidField {
                    field: name.to_string(),
                    at: at.to_string(),
                });
            }
            return Ok(FieldRef {
                path: vec![name.to_string()],
                physical: col.name.clone().unwrap_or_else(|| name.to_string()),
                ty: col.ty,
            });
        }
        match self.unmapped {
            UnmappedPolicy::Identity => Ok(FieldRef::identity(name)),
            UnmappedPolicy::Reject => Err(QueryError::InvalidField {
                field: name.to_string(),
                at: at.to_string(),
            }),
        }
    }

    /// Resolve a column being written on `entity` to its physical name, honouring
    /// renames, the `writable` flag, and the unmapped policy. Unlike
    /// [`resolve_field`](Self::resolve_field) this checks `writable` (not
    /// `queryable`) and returns only the physical name.
    pub fn resolve_write_column(
        &self,
        entity: &str,
        name: &str,
        at: &str,
    ) -> Result<String, QueryError> {
        if let Some(col) = self.entities.get(entity).and_then(|e| e.columns.get(name)) {
            if !col.writable {
                return Err(QueryError::InvalidField {
                    field: name.to_string(),
                    at: at.to_string(),
                });
            }
            return Ok(col.name.clone().unwrap_or_else(|| name.to_string()));
        }
        match self.unmapped {
            UnmappedPolicy::Identity => Ok(name.to_string()),
            UnmappedPolicy::Reject => Err(QueryError::InvalidField {
                field: name.to_string(),
                at: at.to_string(),
            }),
        }
    }

    /// Resolve a relation on `entity` to a physical [`RelRef`] plus the target
    /// entity name (the scope for the inner predicate). Undeclared relations are
    /// a clear error — relations are never inferred.
    pub fn resolve_relation(
        &self,
        entity: &str,
        name: &str,
        at: &str,
    ) -> Result<(RelRef, String), QueryError> {
        let rel = self
            .entities
            .get(entity)
            .and_then(|e| e.relations.get(name))
            .ok_or_else(|| QueryError::UnknownRelation {
                relation: name.to_string(),
                at: at.to_string(),
            })?;
        let through = rel.through.as_ref().map(|j| JunctionRef {
            table: j.table.clone(),
            local: j.local.clone(),
            foreign: j.foreign.clone(),
        });
        Ok((
            RelRef {
                name: name.to_string(),
                target_table: self.physical_table(&rel.to),
                local: rel.local.clone(),
                foreign: rel.foreign.clone(),
                through,
                mongo: rel.mongo,
                es: rel.es,
            },
            rel.to.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema_with_relation(rel: serde_json::Value) -> serde_json::Value {
        json!({ "entities": { "users": {
            "columns": {},
            "relations": { "orders": rel }
        }}})
    }

    #[test]
    fn kind_matching_shape_is_accepted() {
        let reg = EntityRegistry::from_json(&schema_with_relation(json!({
            "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id"
        })))
        .expect("has_many without through is valid");
        assert!(reg.entities.contains_key("users"));

        EntityRegistry::from_json(&schema_with_relation(json!({
            "to": "tags", "kind": "many_to_many", "local": "id", "foreign": "id",
            "through": { "table": "user_tags", "local": "user_id", "foreign": "tag_id" }
        })))
        .expect("many_to_many with through is valid");
    }

    #[test]
    fn many_to_many_without_through_is_rejected() {
        let err = EntityRegistry::from_json(&schema_with_relation(json!({
            "to": "tags", "kind": "many_to_many", "local": "id", "foreign": "tag_id"
        })))
        .expect_err("many_to_many needs a junction");
        assert!(err.to_string().contains("users.orders"), "{err}");
        assert!(err.to_string().contains("requires a 'through'"), "{err}");
    }

    #[test]
    fn through_with_single_valued_kind_is_rejected() {
        let err = EntityRegistry::from_json(&schema_with_relation(json!({
            "to": "tags", "kind": "has_many", "local": "id", "foreign": "id",
            "through": { "table": "user_tags", "local": "user_id", "foreign": "tag_id" }
        })))
        .expect_err("through implies many_to_many");
        assert!(
            err.to_string().contains("requires kind 'many_to_many'"),
            "{err}"
        );
    }
}
