//! The dialect IR — a backend-neutral condition tree (`Cond`) plus its operands.
//!
//! A workflow's `filter` (JSONLogic-shaped) is lowered into this tree by
//! [`crate::query::lower`], and each backend renderer walks it into native query
//! form. The IR is deliberately small and evaluation-free: it exists only to be
//! translated. The token model and its normative semantics are documented in
//! `docs/src/reference/data-dialect.md`.

/// A backend-neutral boolean condition over a single logical entity.
#[derive(Debug, Clone, PartialEq)]
pub enum Cond {
    /// Always true (empty `and`). Renders to an empty `WHERE`.
    True,
    /// Always false (empty `or`, empty `in`). Renders to `1 = 0`.
    False,
    And(Vec<Cond>),
    Or(Vec<Cond>),
    Not(Box<Cond>),
    /// `field <op> value` scalar comparison.
    Compare {
        field: FieldRef,
        op: CmpOp,
        value: Value,
    },
    /// `field IN (values)` / `field NOT IN (values)`.
    In {
        field: FieldRef,
        values: Vec<Value>,
        negated: bool,
    },
    /// `field IS NULL` / `field IS NOT NULL` ("no meaningful value").
    IsNull {
        field: FieldRef,
        negated: bool,
    },
    /// Range with per-bound inclusivity so a chained `<` (strict) and `<=`
    /// (inclusive) render faithfully; native `BETWEEN` is used only when both
    /// bounds are inclusive (the chained-range rule in
    /// `docs/src/reference/data-dialect.md`).
    Between {
        field: FieldRef,
        low: Value,
        high: Value,
        low_incl: bool,
        high_incl: bool,
        negated: bool,
    },
    /// Substring / prefix / suffix text match (`LIKE`). Case-sensitivity is
    /// whatever the backend natively does (W13) — the dialect does not
    /// normalise it; the per-backend truth is in the parity table of
    /// `docs/src/reference/data-dialect.md`.
    Text {
        field: FieldRef,
        op: TextOp,
        pattern: String,
    },
    /// A relation predicate: `some` / `all` / `none` over a declared relation.
    /// Always an EXISTS-style semi/anti join — the root row count never changes.
    Rel {
        quant: Quant,
        rel: RelRef,
        cond: Box<Cond>,
    },
}

impl Cond {
    /// Whether this condition is satisfied by *every* row, i.e. it excludes
    /// nothing.
    ///
    /// This is the predicate the unfiltered-mutation guard keys on. It must be
    /// semantic rather than structural: `{"and": []}` lowers to [`Cond::True`],
    /// but `{"!": {"or": []}}` lowers to `Not(False)` and `{"and": [{"and": []}]}`
    /// to `And([True])` — all three match every row, and only the first is
    /// literally `Cond::True`. Treating just the literal node as unfiltered let
    /// the other shapes drive an unbounded `DELETE`/`UPDATE`.
    ///
    /// Deliberately conservative: anything not provably total returns `false`,
    /// so an unrecognised shape is treated as a real filter rather than waved
    /// through as vacuous.
    pub fn is_always_true(&self) -> bool {
        match self {
            Cond::True => true,
            Cond::False => false,
            // An empty `And` is vacuously true — `all` over an empty set.
            Cond::And(items) => items.iter().all(Cond::is_always_true),
            Cond::Or(items) => items.iter().any(Cond::is_always_true),
            Cond::Not(inner) => inner.is_always_false(),
            _ => false,
        }
    }

    /// Dual of [`Cond::is_always_true`]: satisfied by no row at all.
    pub fn is_always_false(&self) -> bool {
        match self {
            Cond::False => true,
            Cond::True => false,
            Cond::And(items) => items.iter().any(Cond::is_always_false),
            // An empty `Or` is vacuously false — `any` over an empty set.
            Cond::Or(items) => items.iter().all(Cond::is_always_false),
            Cond::Not(inner) => inner.is_always_true(),
            _ => false,
        }
    }
}

/// Relation quantifier. `Any` = `some`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quant {
    Any,
    All,
    None,
}

/// A relation resolved to physical join keys, ready to render. Populated during
/// lowering from the schema so the renderer never needs the registry.
#[derive(Debug, Clone, PartialEq)]
pub struct RelRef {
    /// Logical relation name (Mongo embedded-array field / `$lookup` alias).
    pub name: String,
    /// Physical table of the related entity.
    pub target_table: String,
    /// Physical column on the current entity (the join's local side).
    pub local: String,
    /// Physical column on the target that references the current entity
    /// (for a direct relation), or on the junction (for M:M).
    pub foreign: String,
    /// Junction table for a many-to-many relation.
    pub through: Option<JunctionRef>,
    /// How the relation is stored in MongoDB (find `$elemMatch` vs `$lookup`).
    pub mongo: MongoStorage,
    /// How the relation is stored in Elasticsearch (nested vs child).
    pub es: EsStorage,
}

/// How a relation is stored in MongoDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MongoStorage {
    #[default]
    Embedded,
    Referenced,
}

/// How a relation is stored in Elasticsearch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EsStorage {
    #[default]
    Nested,
    Child,
}

/// The junction table of a many-to-many relation, resolved to physical names.
#[derive(Debug, Clone, PartialEq)]
pub struct JunctionRef {
    /// Physical junction table.
    pub table: String,
    /// Junction column referencing the current entity (joins to `RelRef::local`).
    pub local: String,
    /// Junction column referencing the target (joins to `RelRef::foreign`).
    pub foreign: String,
}

/// Scalar comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    /// The operator that holds the same relation with operands swapped
    /// (`a < b` ⇔ `b > a`). Used when a filter writes `value <op> field`.
    pub fn flipped(self) -> Self {
        match self {
            CmpOp::Eq => CmpOp::Eq,
            CmpOp::Ne => CmpOp::Ne,
            CmpOp::Lt => CmpOp::Gt,
            CmpOp::Le => CmpOp::Ge,
            CmpOp::Gt => CmpOp::Lt,
            CmpOp::Ge => CmpOp::Le,
        }
    }
}

/// Text-match shapes; all render to `LIKE` on SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextOp {
    StartsWith,
    EndsWith,
    Contains,
}

/// A resolved reference to a column/field.
///
/// In identity mode the physical name is the name as written; a schema rename
/// maps the logical name to a different physical one during lowering.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldRef {
    /// The resolved physical column name.
    pub physical: String,
}

impl FieldRef {
    /// Build an identity-mode field reference for a single-segment column name.
    pub fn identity(name: impl Into<String>) -> Self {
        FieldRef {
            physical: name.into(),
        }
    }
}

/// A literal operand value. Only these variants ever become bound parameters,
/// and each maps to an `AnyPool`-safe `sea_query::Value` (never Decimal/Json/Uuid,
/// which panic under the `sqlx-any` binder).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
}
