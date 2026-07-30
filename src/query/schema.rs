//! The entity/relation schema (`EntityRegistry`).
//!
//! Declared inline in the `data_query` input (a `schema` field). It is privileged
//! configuration authored alongside the query — never built from request input —
//! adding, only when wanted: renames (logical→physical), type hints, a field
//! allowlist, and the relation declarations that `some`/`all`/`none` require.
//!
//! Since 1.0 the default is `UnmappedPolicy::Reject` (F24): a task that
//! declares no schema reaches nothing, and identity mode — every name passing
//! through to the physical one — is an explicit `"unmapped": "identity"`
//! opt-in that a connector can refuse outright (`dialect.require_schema`).

use std::collections::{HashMap, HashSet};

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
    /// Physical names this connector permits, from the operator-owned
    /// `dialect.allowed_entities` (F24). `None` means unrestricted.
    ///
    /// `serde(skip)` is the point: this is connector configuration installed by
    /// the handler via [`EntityRegistry::restrict_to`], never something the
    /// task's own `schema` can set — a workflow author who could widen their
    /// own allowlist would not have one.
    #[serde(skip)]
    allowed_physical: Option<HashSet<String>>,
}

/// What to do with a field not declared on its entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnmappedPolicy {
    /// Reject any entity or field not declared (allowlist mode) — the default
    /// since 1.0 (F24).
    #[default]
    Reject,
    /// Treat the logical name as the physical name (identity mode). Through
    /// 0.x this was the default, so any workflow author reached every table
    /// the connector's database user could see, read *and* write; it is now
    /// an explicit per-task opt-in that `dialect.require_schema` can forbid.
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
        // Borrowed deserialisation: the schema is re-parsed per task execution,
        // and `from_value` would deep-clone the whole document to do it.
        let reg: Self = Self::deserialize(v)
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

    /// An explicitly identity-mode registry: every name resolves to itself.
    ///
    /// Test-only. F24 flipped the default policy to `reject`, so the tests that
    /// predate the schema registry — the ones asserting what *identity mode*
    /// does — have to ask for it by name. `default()` now means the opposite
    /// and would pass them for the wrong reason, every name rejected as
    /// undeclared rather than accepted and checked.
    #[cfg(test)]
    pub(crate) fn identity() -> Self {
        Self {
            unmapped: UnmappedPolicy::Identity,
            ..Self::default()
        }
    }

    /// Install the connector's `allowed_entities` allowlist (F24). An empty
    /// list means "no restriction", matching the config default.
    pub fn restrict_to(&mut self, allowed: &[String]) {
        if !allowed.is_empty() {
            self.allowed_physical = Some(allowed.iter().cloned().collect());
        }
    }

    /// Whether this registry would let an undeclared name through — i.e. it is
    /// in identity mode. `dialect.require_schema` refuses such a task (F24).
    pub fn is_identity_mode(&self) -> bool {
        self.unmapped == UnmappedPolicy::Identity
    }

    /// Whether the task declared any entity at all.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Physical table/collection/index for a **caller-named** entity — the
    /// envelope's `source` / `target`.
    ///
    /// F24: this used to fall back to the logical name unconditionally, so
    /// `unmapped: "reject"` bounded only *columns*. A query naming no fields at
    /// all (`{"source": "secrets"}` → `SELECT *`) resolved every table the
    /// connector's database user could see even in allowlist mode, because no
    /// field resolution ever ran. Under `reject` an undeclared entity is now
    /// refused before any backend sees it.
    pub fn physical_table(&self, entity: &str) -> Result<String, QueryError> {
        if self.unmapped == UnmappedPolicy::Reject && !self.entities.contains_key(entity) {
            return Err(QueryError::UndeclaredEntity {
                entity: entity.to_string(),
            });
        }
        self.structural_table(entity)
    }

    /// Physical table for an entity named by the *schema itself* — a relation's
    /// `to` target — rather than by a caller.
    ///
    /// Skips the undeclared-entity gate for the same reason
    /// [`structural_column`](Self::structural_column) skips the `queryable`
    /// allowlist: the operator wrote `"to": "orders"` in the schema, so the
    /// reference is declared structure, not caller input. The connector's
    /// `allowed_entities` still applies — that is operator config outranking
    /// the task's schema, not the schema checking itself.
    fn structural_table(&self, entity: &str) -> Result<String, QueryError> {
        let table = self
            .entities
            .get(entity)
            .and_then(|e| e.physical.clone())
            .unwrap_or_else(|| entity.to_string());
        validate_identifier(&table, "source")?;
        self.check_allowed(entity, &table)?;
        Ok(table)
    }

    /// Enforce the connector's `allowed_entities` against a resolved *physical*
    /// name. Physical rather than logical deliberately: the allowlist is
    /// operator config and the schema is authored per task, so a rename
    /// (`"orders" → "secrets"`) must not be able to step around it.
    fn check_allowed(&self, entity: &str, physical: &str) -> Result<(), QueryError> {
        match &self.allowed_physical {
            Some(allowed) if !allowed.contains(physical) => Err(QueryError::EntityNotAllowed {
                entity: entity.to_string(),
                physical: physical.to_string(),
            }),
            _ => Ok(()),
        }
    }

    /// The projection a read gets when it names no `fields`: every declared
    /// **queryable** column of `entity`, in a stable (logical-name) order.
    ///
    /// F24, the column half of the same hole. Bounding entities left the column
    /// allowlist bypassable, because a field-less read never resolves a field:
    /// `fields: []` renders `SELECT *` (and no `_source` on ES, no projection
    /// document on Mongo), and [`crate::query::spec::QuerySpec::resolve_names`]
    /// only walks the columns a caller actually named. So with
    /// `{"password_hash": {"queryable": false}}` declared, `{"source":
    /// "users"}` still returned it — `queryable` meant "you may not *name* this
    /// column", not "you may not read it". Under `reject` the wildcard is
    /// replaced by the declared column list, so the flag means the same thing
    /// whether or not the caller wrote a `fields` array.
    ///
    /// An empty result means *keep the wildcard*, and there are exactly two
    /// such cases, both of them "no column allowlist was written here":
    /// identity mode, and an entity declaring no columns at all (a
    /// relation-only or write-only declaration). An entity that declares
    /// columns and marks every one non-queryable is the third case and is an
    /// error, not a wildcard — silently widening back to every column is the
    /// defect this closes.
    pub fn default_projection(&self, entity: &str, at: &str) -> Result<Vec<String>, QueryError> {
        if self.unmapped != UnmappedPolicy::Reject {
            return Ok(Vec::new());
        }
        let Some(ent) = self.entities.get(entity) else {
            return Ok(Vec::new());
        };
        if ent.columns.is_empty() {
            return Ok(Vec::new());
        }
        // `columns` is a HashMap, so sort for a deterministic projection —
        // rendered SQL must not vary between processes.
        let mut declared: Vec<&String> = ent
            .columns
            .iter()
            .filter(|(_, c)| c.queryable)
            .map(|(name, _)| name)
            .collect();
        declared.sort();
        if declared.is_empty() {
            return Err(QueryError::NoQueryableColumns {
                entity: entity.to_string(),
            });
        }
        declared
            .into_iter()
            .map(|name| Ok(self.resolve_field(entity, name, at)?.physical))
            .collect()
    }

    /// Resolve a caller-named column on `entity` to its physical name and type,
    /// honouring renames, the unmapped policy, and whichever per-column flag
    /// (`queryable` / `writable`) `allowed` tests.
    ///
    /// One body for both directions on purpose (W4): the read and write paths
    /// used to validate differently, which is exactly how a name that is not a
    /// plain identifier reached a backend on one of them.
    fn resolve_column(
        &self,
        entity: &str,
        name: &str,
        at: &str,
        allowed: fn(&Column) -> bool,
    ) -> Result<(String, FieldType), QueryError> {
        if let Some(col) = self.entities.get(entity).and_then(|e| e.columns.get(name)) {
            if !allowed(col) {
                return Err(QueryError::InvalidField {
                    field: name.to_string(),
                    at: at.to_string(),
                });
            }
            let physical = col.name.clone().unwrap_or_else(|| name.to_string());
            // W4: a rename target is operator-supplied too.
            validate_identifier(&physical, at)?;
            return Ok((physical, col.ty));
        }
        match self.unmapped {
            UnmappedPolicy::Identity => {
                validate_identifier(name, at)?;
                Ok((name.to_string(), FieldType::Unknown))
            }
            UnmappedPolicy::Reject => Err(QueryError::InvalidField {
                field: name.to_string(),
                at: at.to_string(),
            }),
        }
    }

    /// Resolve a single-segment field on `entity` to a physical [`FieldRef`],
    /// honouring renames, type hints, and the allowlist.
    pub fn resolve_field(
        &self,
        entity: &str,
        name: &str,
        at: &str,
    ) -> Result<FieldRef, QueryError> {
        let (physical, ty) = self.resolve_column(entity, name, at, |c| c.queryable)?;
        Ok(FieldRef {
            path: vec![name.to_string()],
            physical,
            ty,
        })
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
        Ok(self.resolve_column(entity, name, at, |c| c.writable)?.0)
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
                // F24: a junction is a third table this call touches and it
                // never went through `physical_table`, so it was the one table
                // name the connector allowlist would otherwise miss.
                self.check_allowed(&rel.to, &j.table)?;
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
                target_table: self.structural_table(&rel.to)?,
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

    use crate::query::lower::Params;
    use EntityRegistry as Reg;

    fn identity() -> EntityRegistry {
        Reg::identity()
    }

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
        let reg = identity();
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
        let reg = identity();
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
            identity().physical_table("users").expect("plain name"),
            "users"
        );
    }

    // -----------------------------------------------------------------
    // F24: the safe mode is the default one
    // -----------------------------------------------------------------

    /// The flip itself. Through 0.x an absent `schema` meant identity mode, so
    /// a task with no schema reached every table the connector's database user
    /// could see. An empty registry must now reach nothing.
    #[test]
    fn the_default_registry_rejects_rather_than_passing_names_through() {
        assert_eq!(EntityRegistry::default().unmapped, UnmappedPolicy::Reject);

        let reg = EntityRegistry::default();
        assert!(
            reg.physical_table("users").is_err(),
            "an undeclared entity must not resolve under the default policy"
        );
        assert!(reg.resolve_field("users", "id", "filter").is_err());
        assert!(reg.resolve_write_column("users", "id", "values").is_err());
    }

    /// The specific hole the policy flip alone would have left open: `reject`
    /// used to bound only *columns*, and `{"source": "secrets"}` renders
    /// `SELECT *` without ever resolving a field — so no field check ran and
    /// the table resolved anyway.
    #[test]
    fn an_undeclared_entity_is_refused_even_when_no_field_is_named() {
        let reg = EntityRegistry::from_json(&json!({
            "entities": { "users": { "columns": { "id": {} } } }
        }))
        .expect("registry");

        assert_eq!(reg.physical_table("users").expect("declared"), "users");
        let err = reg
            .physical_table("secrets")
            .expect_err("an undeclared entity must not resolve");
        assert_eq!(
            err,
            QueryError::UndeclaredEntity {
                entity: "secrets".to_string()
            }
        );
    }

    /// The error a 0.x workflow hits after upgrading is the one place the fix
    /// can be explained, so it must name both ways forward.
    #[test]
    fn the_undeclared_entity_error_names_exactly_what_to_add() {
        let msg = QueryError::UndeclaredEntity {
            entity: "orders".to_string(),
        }
        .to_string();
        assert!(msg.contains("orders"), "{msg}");
        assert!(
            msg.contains("\"schema\""),
            "must name the key to add: {msg}"
        );
        assert!(msg.contains("\"entities\""), "{msg}");
        assert!(
            msg.contains("\"unmapped\": \"identity\""),
            "must name the opt-out too: {msg}"
        );
    }

    /// A relation's `to` is schema-declared structure, not caller input, so it
    /// resolves like `structural_column` does — otherwise the documented
    /// example, which declares a relation to an entity it does not itself
    /// declare, would stop working under the new default.
    ///
    /// The exemption is exactly that far and no further, which is what the
    /// second half pins: the relation itself resolves, but every *column* on
    /// the target is caller input again and goes through the ordinary
    /// allowlist. Combined with F27 — which requires each `include` to name a
    /// deterministic `sort` key — that means an `include` over an undeclared
    /// target cannot plan at all under the default policy: the sort key it is
    /// obliged to name is a column the allowlist refuses. Declaring the target
    /// entity is what makes an `include` usable; the relation exemption only
    /// keeps `resolve_relation` itself working.
    #[test]
    fn a_relation_target_need_not_be_a_declared_entity() {
        let schema = json!({ "entities": { "users": {
            "columns": { "id": {} },
            "relations": { "orders": {
                "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id"
            }}
        }}});
        let reg = EntityRegistry::from_json(&schema).expect("registry");

        let (rel, target) = reg
            .resolve_relation("users", "orders", "filter")
            .expect("a declared relation resolves even in reject mode");
        assert_eq!(rel.target_table, "orders");
        assert_eq!(target, "orders");

        // But an `include` must name a `sort` key (F27), and that key is a
        // column on the target — so under the default policy an include over an
        // undeclared target cannot plan, whichever way the caller writes it.
        let limits = crate::config::QueryConfig::default();
        let err = crate::query::plan_sql(
            &json!({ "source": "users", "include": { "orders": {} } }),
            &Params::new(),
            &reg,
            crate::query::SqlDialect::Sqlite,
            &limits,
        )
        .expect_err("an include with no sort is refused");
        assert!(
            err.to_string().contains("sort"),
            "expected the missing-sort error, got: {err}"
        );

        let err = crate::query::plan_sql(
            &json!({ "source": "users", "include": { "orders": {
                "sort": [{ "id": "asc" }]
            } } }),
            &Params::new(),
            &reg,
            crate::query::SqlDialect::Sqlite,
            &limits,
        )
        .expect_err("the sort key is a column on an undeclared target");
        assert!(matches!(err, QueryError::InvalidField { .. }), "{err}");

        // Naming one of its columns does not.
        let err = crate::query::plan_sql(
            &json!({ "source": "users", "include": { "orders": { "fields": ["id"] } } }),
            &Params::new(),
            &reg,
            crate::query::SqlDialect::Sqlite,
            &limits,
        )
        .expect_err("a column on an undeclared target is caller input");
        assert!(matches!(err, QueryError::InvalidField { .. }), "{err}");

        // Nor does a predicate over it.
        let err = crate::query::plan_sql(
            &json!({ "source": "users", "filter": {
                "some": [{ "field": "orders" }, { "==": [{ "field": "total" }, 1] }]
            }}),
            &Params::new(),
            &reg,
            crate::query::SqlDialect::Sqlite,
            &limits,
        )
        .expect_err("a `some` over an undeclared target names its columns");
        assert!(matches!(err, QueryError::InvalidField { .. }), "{err}");
    }

    // -----------------------------------------------------------------
    // F24: the column half — a field-less read is projected, not `SELECT *`
    // -----------------------------------------------------------------

    /// `queryable: false` used to mean "you may not *name* this column": a
    /// read that named no fields at all rendered `SELECT *` and returned it,
    /// because `resolve_names` only walks the columns a caller listed.
    #[test]
    fn a_field_less_read_projects_the_declared_queryable_columns() {
        let reg = EntityRegistry::from_json(&json!({ "entities": { "users": { "columns": {
            "id": {},
            "email": { "name": "email_addr" },
            "password_hash": { "queryable": false }
        }}}}))
        .expect("registry");

        assert_eq!(
            reg.default_projection("users", "fields")
                .expect("a declared entity projects its queryable columns"),
            vec!["email_addr".to_string(), "id".to_string()],
            "the non-queryable column must not be projected, and renames apply"
        );
    }

    /// The two cases that legitimately stay a wildcard: nobody wrote a column
    /// allowlist here.
    #[test]
    fn a_read_stays_a_wildcard_when_no_columns_were_declared() {
        assert!(
            identity()
                .default_projection("users", "fields")
                .expect("identity mode")
                .is_empty(),
            "identity mode declares nothing, so there is nothing to project"
        );

        let reg = EntityRegistry::from_json(&json!({ "entities": { "users": {
            "relations": { "orders": {
                "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id"
            }}
        }}}))
        .expect("registry");
        assert!(
            reg.default_projection("users", "fields")
                .expect("relation-only entity")
                .is_empty()
        );
    }

    /// An entity whose every column is `queryable: false` must not fall back
    /// to the wildcard it was declared to prevent.
    #[test]
    fn an_entity_with_no_queryable_column_is_an_error_not_a_wildcard() {
        let reg = EntityRegistry::from_json(&json!({ "entities": { "audit": { "columns": {
            "token": { "queryable": false },
            "secret": { "queryable": false }
        }}}}))
        .expect("registry");

        assert_eq!(
            reg.default_projection("audit", "fields")
                .expect_err("nothing is readable, so nothing may be returned"),
            QueryError::NoQueryableColumns {
                entity: "audit".to_string()
            }
        );
    }

    /// `allowed_entities` is operator config and the schema is authored per
    /// task, so the allowlist binds the *physical* name — a rename must not be
    /// able to step around it.
    #[test]
    fn the_connector_allowlist_binds_the_physical_name_not_the_logical_one() {
        let mut reg = EntityRegistry::from_json(&json!({ "entities": {
            "orders": { "physical": "secrets", "columns": { "id": {} } },
            "users":  { "columns": { "id": {} } }
        }}))
        .expect("registry");
        reg.restrict_to(&["users".to_string()]);

        assert_eq!(reg.physical_table("users").expect("allowed"), "users");
        let err = reg
            .physical_table("orders")
            .expect_err("a rename onto a disallowed table must be refused");
        assert_eq!(
            err,
            QueryError::EntityNotAllowed {
                entity: "orders".to_string(),
                physical: "secrets".to_string(),
            }
        );
    }

    /// The allowlist must cover every table the call touches, not just the
    /// envelope's `source` — relation targets and junctions are tables too,
    /// and the junction never went through `physical_table` at all.
    #[test]
    fn the_allowlist_covers_relation_targets_and_junctions() {
        let schema = json!({ "entities": { "users": {
            "columns": { "id": {} },
            "relations": {
                "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id" },
                "tags": {
                    "to": "tags", "kind": "many_to_many", "local": "id", "foreign": "id",
                    "through": { "table": "user_tags", "local": "user_id", "foreign": "tag_id" }
                }
            }
        }}});

        let mut reg = EntityRegistry::from_json(&schema).expect("registry");
        reg.restrict_to(&["users".to_string()]);
        assert!(
            reg.resolve_relation("users", "orders", "filter").is_err(),
            "a relation target outside the allowlist must be refused"
        );
        assert!(
            reg.resolve_relation("users", "tags", "filter").is_err(),
            "a junction table outside the allowlist must be refused"
        );

        let mut reg = EntityRegistry::from_json(&schema).expect("registry");
        reg.restrict_to(&[
            "users".to_string(),
            "orders".to_string(),
            "tags".to_string(),
            "user_tags".to_string(),
        ]);
        assert!(reg.resolve_relation("users", "orders", "filter").is_ok());
        assert!(reg.resolve_relation("users", "tags", "filter").is_ok());
    }

    /// An empty `allowed_entities` is the config default and must mean "no
    /// restriction", not "nothing is reachable".
    #[test]
    fn an_empty_allowlist_restricts_nothing() {
        let mut reg = identity();
        reg.restrict_to(&[]);
        assert_eq!(
            reg.physical_table("anything").expect("unrestricted"),
            "anything"
        );
    }

    /// The allowlist is connector configuration. A task that could name it in
    /// its own `schema` would simply widen it.
    #[test]
    fn a_task_schema_cannot_set_the_allowlist_itself() {
        let err = EntityRegistry::from_json(&json!({
            "entities": {}, "allowed_physical": ["users"]
        }))
        .expect_err("the allowlist is not a task-settable key");
        assert!(err.to_string().contains("invalid schema"), "{err}");
    }
}
