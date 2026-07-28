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
///
/// Every schema struct rejects unknown keys (W5): a misspelled key here is
/// privileged security configuration silently not applying — the documented
/// example itself used `"table"` where the field is `physical`, and the entity
/// quietly fell back to identity mode.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct Entity {
    /// Physical table/collection/index; defaults to the entity's key.
    pub physical: Option<String>,
    pub columns: HashMap<String, Column>,
    pub relations: HashMap<String, Relation>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    pub fn physical_table(&self, entity: &str) -> Result<String, QueryError> {
        let table = self
            .entities
            .get(entity)
            .and_then(|e| e.physical.clone())
            .unwrap_or_else(|| entity.to_string());
        validate_identifier(&table, "source")?;
        Ok(table)
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
            let physical = col.name.clone().unwrap_or_else(|| name.to_string());
            // W4: a rename target is operator-supplied too.
            validate_identifier(&physical, at)?;
            return Ok(FieldRef {
                path: vec![name.to_string()],
                physical,
                ty: col.ty,
            });
        }
        match self.unmapped {
            UnmappedPolicy::Identity => {
                validate_identifier(name, at)?;
                Ok(FieldRef::identity(name))
            }
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
            let physical = col.name.clone().unwrap_or_else(|| name.to_string());
            validate_identifier(&physical, at)?;
            return Ok(physical);
        }
        match self.unmapped {
            UnmappedPolicy::Identity => {
                validate_identifier(name, at)?;
                Ok(name.to_string())
            }
            UnmappedPolicy::Reject => Err(QueryError::InvalidField {
                field: name.to_string(),
                at: at.to_string(),
            }),
        }
    }

    /// Physical name for a column named by the *schema itself* — a relation's
    /// join keys — rather than by a caller.
    ///
    /// Honours renames and [`validate_identifier`], deliberately **not** the
    /// `queryable` / `writable` allowlist: those gate what a caller may name,
    /// and a join key is structure the operator declared. This is the
    /// distinction `physical_column` was reaching for before W3 — legitimate
    /// here, illegitimate for a caller-supplied `returning`.
    fn structural_column(&self, entity: &str, name: &str, at: &str) -> Result<String, QueryError> {
        let physical = self
            .entities
            .get(entity)
            .and_then(|e| e.columns.get(name))
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| name.to_string());
        validate_identifier(&physical, at)?;
        Ok(physical)
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
        // F25: junction names are operator-declared structure that reaches the
        // SQL renderer as identifiers (`Alias::new`), exactly like the join
        // keys below — they were the one identifier channel that skipped
        // `validate_identifier`.
        let through = rel
            .through
            .as_ref()
            .map(|j| -> Result<JunctionRef, QueryError> {
                validate_identifier(&j.table, at)?;
                validate_identifier(&j.local, at)?;
                validate_identifier(&j.foreign, at)?;
                Ok(JunctionRef {
                    table: j.table.clone(),
                    local: j.local.clone(),
                    foreign: j.foreign.clone(),
                })
            })
            .transpose()?;
        Ok((
            RelRef {
                name: name.to_string(),
                target_table: self.physical_table(&rel.to)?,
                // Join keys are operator-declared structure, not caller input,
                // so they honour renames and identifier rules but not the
                // caller-facing `queryable` allowlist (W2). Before this they
                // were passed through raw, so a renamed key column silently
                // broke include grouping.
                local: self.structural_column(entity, &rel.local, at)?,
                foreign: self.structural_column(&rel.to, &rel.foreign, at)?,
                through,
                mongo: rel.mongo,
                es: rel.es,
            },
            rel.to.clone(),
        ))
    }
}

/// One rule for every logical name that becomes a physical one (W4).
///
/// The read and write paths validated differently — `lower.rs` rejected empty
/// and dotted names, `resolve_write_column` checked nothing — and **neither
/// rejected a leading `$`**. Three concrete consequences, all silent:
///
/// - `{"field": "$where"}` in identity mode reached MongoDB as a raw document
///   key, where `$`-prefixed keys are operators, not field names.
/// - `values: {"a.b": 1}` wrote a *nested path* on MongoDB and a *literal
///   column named `a.b`* on SQL — the same envelope, two different meanings.
/// - `values: {"": 1}` emitted `INSERT INTO "users" ("")`.
///
/// Applied in `resolve_field`, `resolve_write_column` and `physical_table`, so
/// no backend receives a name that has not been through it. This also closes
/// the residual half of F25: quoting still happens in sea-query's
/// `Iden::quoted`, but a name carrying a quote character no longer reaches it.
pub(crate) fn validate_identifier(name: &str, at: &str) -> Result<(), QueryError> {
    let reject = || QueryError::InvalidField {
        field: name.to_string(),
        at: at.to_string(),
    };
    if name.is_empty() {
        return Err(reject());
    }
    // MongoDB reads a leading `$` as an operator sigil.
    if name.starts_with('$') {
        return Err(reject());
    }
    // A dot is a nested path to MongoDB and a literal character to SQL.
    if name.contains('.') {
        return Err(reject());
    }
    // Quote and escape characters, NUL, and control characters. Defence in
    // depth: escaping lives in a transitive dependency (F25), so nothing that
    // would need escaping gets that far.
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, '"' | '\'' | '`' | '\\' | '\0'))
    {
        return Err(reject());
    }
    Ok(())
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

    // -----------------------------------------------------------------
    // W4: one identifier rule across the read and write paths
    // -----------------------------------------------------------------

    /// Identity mode is the permissive setting — it still must not hand a
    /// backend a name that means something other than "a column".
    #[test]
    fn identity_mode_rejects_names_that_are_not_plain_identifiers() {
        let reg = EntityRegistry::default();
        for bad in [
            // MongoDB reads a leading `$` as an operator sigil, so this
            // reached `mongo.rs` as a raw document key.
            "$where", "$ne",
            // Nested path on Mongo, literal column on SQL — one envelope,
            // two meanings.
            "a.b", // `INSERT INTO "users" ("")`.
            "",
            // Quoting lives in a transitive dependency (F25); nothing that
            // would need escaping should reach it.
            "a\"b", "a`b", "a'b", "a\\b", "a\nb",
        ] {
            assert!(
                reg.resolve_field("users", bad, "filter").is_err(),
                "read path accepted {bad:?}"
            );
            assert!(
                reg.resolve_write_column("users", bad, "values").is_err(),
                "write path accepted {bad:?}"
            );
        }
    }

    #[test]
    fn ordinary_identifiers_still_resolve_on_both_paths() {
        let reg = EntityRegistry::default();
        for good in ["id", "user_id", "createdAt", "col2", "_private"] {
            assert!(reg.resolve_field("users", good, "filter").is_ok(), "{good}");
            assert!(
                reg.resolve_write_column("users", good, "values").is_ok(),
                "{good}"
            );
        }
    }

    /// A rename target is operator-supplied config, so it goes through the
    /// same rule as a caller-supplied name.
    #[test]
    fn a_rename_target_is_validated_too() {
        let reg = EntityRegistry::from_json(&json!({ "entities": { "users": {
            "columns": { "key": { "name": "$id" } }
        }}}))
        .expect("registry");
        assert!(reg.resolve_field("users", "key", "filter").is_err());
    }

    /// F25: the junction's table and columns render as SQL identifiers via the
    /// M:M join, and used to be the one identifier channel that skipped
    /// `validate_identifier`.
    #[test]
    fn junction_identifiers_are_validated() {
        for (field, value) in [
            ("table", "user\"tags"),
            ("local", "$uid"),
            ("foreign", "tag.id"),
        ] {
            let mut junction = json!({
                "table": "user_tags", "local": "user_id", "foreign": "tag_id"
            });
            junction[field] = json!(value);
            let reg = EntityRegistry::from_json(&schema_with_relation(json!({
                "to": "tags", "kind": "many_to_many", "local": "id", "foreign": "id",
                "through": junction
            })))
            .expect("shape is valid; names are checked at resolution");
            assert!(
                reg.resolve_relation("users", "orders", "filter").is_err(),
                "junction {field} {value:?} must be rejected"
            );
        }
    }

    // -----------------------------------------------------------------
    // W5: the documented example must parse and mean what it says
    // -----------------------------------------------------------------

    /// The first fenced ```json block after "The schema registry" heading in
    /// `docs/src/reference/data-dialect.md`, exactly as a reader would copy it.
    fn documented_schema_example() -> serde_json::Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/src/reference/data-dialect.md"
        );
        let source = std::fs::read_to_string(path)
            .expect("docs/src/reference/data-dialect.md must be readable");
        let section = source
            .split("## The schema registry")
            .nth(1)
            .expect("data-dialect.md must have a 'The schema registry' section");
        let fence_start = section
            .find("```json")
            .expect("the schema registry section must carry a ```json example")
            + "```json".len();
        let body = &section[fence_start..];
        let fence_end = body
            .find("```")
            .expect("the ```json example must be closed");
        // The doc shows a `"schema": { … }` task fragment; wrap it in braces
        // to make it a standalone document, changing nothing else.
        let wrapped: serde_json::Value =
            serde_json::from_str(&format!("{{{}}}", body[..fence_end].trim()))
                .expect("the documented schema example must be valid JSON");
        wrapped
            .get("schema")
            .expect("the example must be a 'schema' fragment")
            .clone()
    }

    /// The single canonical example of the feature the dialect's security
    /// model hangs off was copy-paste broken in two directions: `"table"`
    /// where the field is `physical` (silently dropped, so the rename never
    /// applied) and `"type": "string"`, which is not a `FieldType` variant
    /// (hard parse error). Parse the doc's example verbatim and assert it
    /// *means* what the surrounding prose says, so it cannot drift again.
    #[test]
    fn the_documented_schema_example_parses_and_means_what_it_says() {
        let reg = EntityRegistry::from_json(&documented_schema_example())
            .expect("the documented schema example must parse as an EntityRegistry");

        // The prose promises a physical rename (users → app_users)…
        assert_eq!(
            reg.physical_table("users").expect("users is declared"),
            "app_users",
            "the entity rename in the example must actually apply"
        );
        // …a column rename with a type hint (id → user_id)…
        assert_eq!(
            reg.resolve_field("users", "id", "filter")
                .expect("id is declared")
                .physical,
            "user_id",
            "the column rename in the example must actually apply"
        );
        // …a column hidden from reads and writes…
        assert!(
            reg.resolve_field("users", "secret", "filter").is_err(),
            "queryable: false must hide the column from reads"
        );
        assert!(
            reg.resolve_write_column("users", "secret", "values")
                .is_err(),
            "writable: false must protect the column from writes"
        );
        // …allowlist mode, and a declared relation for some/all/none.
        assert_eq!(reg.unmapped, UnmappedPolicy::Reject);
        assert!(
            reg.resolve_relation("users", "orders", "filter").is_ok(),
            "the example's relation must resolve"
        );
    }

    // -----------------------------------------------------------------
    // W5: unknown schema keys are configuration that silently no-ops
    // -----------------------------------------------------------------

    /// The documented example used `"table"` where the field is `physical`;
    /// with unknown keys ignored, the rename silently did not apply and the
    /// entity fell back to identity mode. Every level must reject them.
    #[test]
    fn unknown_schema_keys_are_rejected_at_every_level() {
        for (what, schema) in [
            ("registry", json!({ "entitys": {} })),
            (
                "entity",
                json!({ "entities": { "users": { "table": "app_users" } } }),
            ),
            (
                "column",
                json!({ "entities": { "users": {
                    "columns": { "id": { "readable": false } }
                } } }),
            ),
            (
                "relation",
                json!({ "entities": { "users": { "relations": { "orders": {
                    "to": "orders", "local": "id", "foreign": "user_id", "cardinality": "has_many"
                } } } } }),
            ),
            (
                "junction",
                json!({ "entities": { "users": { "relations": { "tags": {
                    "to": "tags", "local": "id", "foreign": "id",
                    "through": { "table": "user_tags", "local": "user_id", "foreign": "tag_id", "junction": true }
                } } } } }),
            ),
        ] {
            let err = EntityRegistry::from_json(&schema)
                .expect_err(&format!("unknown key on {what} must be rejected"));
            assert!(err.to_string().contains("invalid schema"), "{what}: {err}");
        }
    }

    #[test]
    fn a_physical_table_name_is_validated_too() {
        let reg = EntityRegistry::from_json(&json!({ "entities": { "users": {
            "physical": "bad.table", "columns": {}
        }}}))
        .expect("registry");
        assert!(reg.physical_table("users").is_err());
        assert_eq!(
            EntityRegistry::default()
                .physical_table("users")
                .expect("plain name"),
            "users"
        );
    }
}
