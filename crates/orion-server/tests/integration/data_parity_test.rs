//! The cross-backend **parity matrix** for the portable `data_query` dialect
//! (W21).
//!
//! The dialect's whole value proposition is that one envelope means one thing on
//! every backend. Before this file the only cross-backend coverage was a single
//! CRUD round-trip exercising `>`, `==`, one sort and one projection
//! (`data_roundtrip_test.rs`), and the per-backend unit tests are goldens that
//! assert *shape*, never agreement — so every silent divergence the 1.0 audit
//! found (null ordering, the `id` → `_id` rewrite, include grouping) was
//! invisible to the suite by construction.
//!
//! The shape here is a table:
//!
//! - **one fixture dataset** ([`seed_tasks`]) written through the portable write
//!   dialect, so it lands identically everywhere;
//! - **N envelopes** ([`cases`]), each with the row set it must return —
//!   asserted as the ordered `name` column, so the assertion is about *which
//!   rows and in what order*, not about per-backend result plumbing;
//! - an **`expected_error` column** naming the backends that must refuse the
//!   envelope instead of answering it, because the dialect's rule is "parity or
//!   a precise capability error, never a silent approximation".
//!
//! `expected_error` asserts a validation-class refusal, not *which* refusal. The
//! data plane replaces task-error messages with a generic string (G1) and every
//! `QueryError` variant maps to the same `DataflowError::Validation`, so the
//! response body carries no discriminator to assert on. Which refusal a backend
//! gives — `FeatureUnsupportedByTarget` rather than a parse error, say — is
//! pinned where the error value itself is in hand, in the per-backend renderer
//! tests (`query::backend::{mongo,es}::tests::test_include_is_capability_error`).
//!
//! [`cases`] is also the source of truth for the parity table in
//! `docs/src/reference/data-dialect.md`: anything not carrying an
//! `expected_error` is a promise that the two answers are the same.
//!
//! SQLite runs in-memory and is not `#[ignore]`d, so the matrix executes in the
//! normal (Docker-free) test job. Postgres / MySQL / MongoDB / Elasticsearch use
//! testcontainers and stay `#[ignore]`d behind the existing container gate.

use crate::common;
use crate::common::backends::{Backend, BackendHarness};
use crate::common::dsl;

use axum::http::StatusCode;
use serde_json::{Value, json};

/// The schema every case is translated through.
///
/// **Relations** are what the capability rows quantify over. `orders` is
/// declared `referenced` on MongoDB — it *is* a separate collection here, and a
/// `find` filter cannot join one, so Mongo refuses rather than answering a
/// different question with `$elemMatch` over an embedded array that does not
/// exist.
///
/// **One rename** (`handle` → the physical `nickname`) so that the matrix
/// exercises schema resolution and not just identity mode. Renaming is the
/// dialect's most load-bearing schema feature and it is applied in four
/// separate places — `filter`, `fields`, `sort` and `include` — by four
/// different code paths, on five backends. The `unmapped` policy is pinned to
/// `identity` explicitly — F24 made `reject` the default, and this matrix is
/// about cross-backend agreement rather than the allowlist, so every other
/// column keeps passing through by name.
fn parity_schema() -> Value {
    json!({ "unmapped": "identity", "entities": { "users": {
        "columns": { "handle": { "name": "nickname" } },
        "relations": {
            "orders": {
                "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id",
                "mongo": "referenced"
            },
            "tags": {
                "to": "tags", "kind": "many_to_many", "local": "id", "foreign": "id",
                "through": { "table": "user_tags", "local": "user_id", "foreign": "tag_id" }
            }
        }
    } } })
}

/// One row of the matrix.
struct Case {
    /// Stable identifier; names the channel and every assertion message.
    name: &'static str,
    query: Value,
    /// The `name` column of the expected result, in order. The same list is
    /// expected from every backend not listed in `expected_error`.
    rows: &'static [&'static str],
    /// Backends that must refuse this envelope. Empty means "answerable
    /// everywhere, identically".
    expected_error: &'static [Backend],
    /// When set, the exact key set every returned row must carry — projection
    /// shape is part of the contract, not an accident of the driver.
    fields_exactly: Option<&'static [&'static str]>,
}

impl Case {
    fn new(name: &'static str, query: Value, rows: &'static [&'static str]) -> Self {
        Case {
            name,
            query,
            rows,
            expected_error: &[],
            fields_exactly: None,
        }
    }

    fn erroring_on(mut self, backends: &'static [Backend]) -> Self {
        self.expected_error = backends;
        self
    }

    fn projecting(mut self, fields: &'static [&'static str]) -> Self {
        self.fields_exactly = Some(fields);
        self
    }
}

/// Backends with no `include` / relation-join capability at all.
const DOC_STORES: &[Backend] = &[Backend::Mongo, Backend::Es];
const EVERY_BACKEND: &[Backend] = &[
    Backend::Sqlite,
    Backend::Postgres,
    Backend::Mysql,
    Backend::Mongo,
    Backend::Es,
];

/// A `sort` that makes every result set totally ordered, so "the same rows"
/// and "the same order" are one assertion.
fn by_name() -> Value {
    json!([{ "name": "asc" }])
}

/// The matrix. Fixture (see [`seed_tasks`]):
///
/// | id | name  | nickname | age | status   |
/// |----|-------|----------|-----|----------|
/// | u1 | Alice | *null*   | 30  | active   |
/// | u2 | Bob   | *null*   | 20  | active   |
/// | u3 | Carol | cee      | 40  | inactive |
/// | u4 | Dave  | dee      | 25  | active   |
fn cases() -> Vec<Case> {
    vec![
        // -- the operator vocabulary, row set + order ------------------------
        Case::new(
            "match_all",
            json!({ "source": "users", "sort": by_name() }),
            &["Alice", "Bob", "Carol", "Dave"],
        ),
        Case::new(
            "eq",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "==": [{ "field": "status" }, "inactive"] } }),
            &["Carol"],
        ),
        // W10: `id` is an ordinary field on every backend now. MongoDB used to
        // rewrite it to `_id`, so this envelope silently matched nothing there
        // while matching Carol everywhere else.
        Case::new(
            "eq_on_a_plain_id_field",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "==": [{ "field": "id" }, "u3"] } }),
            &["Carol"],
        ),
        Case::new(
            "ne",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "!=": [{ "field": "status" }, "active"] } }),
            &["Carol"],
        ),
        Case::new(
            "gt",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { ">": [{ "field": "age" }, 25] } }),
            &["Alice", "Carol"],
        ),
        Case::new(
            "gte",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { ">=": [{ "field": "age" }, 25] } }),
            &["Alice", "Carol", "Dave"],
        ),
        Case::new(
            "lt",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "<": [{ "field": "age" }, 25] } }),
            &["Bob"],
        ),
        Case::new(
            "lte",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "<=": [{ "field": "age" }, 25] } }),
            &["Bob", "Dave"],
        ),
        Case::new(
            "in_list",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "in": [{ "field": "status" }, ["inactive", "banned"]] } }),
            &["Carol"],
        ),
        Case::new(
            "in_empty_list_matches_nothing",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "in": [{ "field": "status" }, []] } }),
            &[],
        ),
        Case::new(
            "not",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "!": { "==": [{ "field": "status" }, "active"] } } }),
            &["Carol"],
        ),
        Case::new(
            "and",
            json!({ "source": "users", "sort": by_name(), "filter": { "and": [
                { ">": [{ "field": "age" }, 20] },
                { "==": [{ "field": "status" }, "active"] }
            ] } }),
            &["Alice", "Dave"],
        ),
        Case::new(
            "or",
            json!({ "source": "users", "sort": by_name(), "filter": { "or": [
                { "<": [{ "field": "age" }, 21] },
                { ">": [{ "field": "age" }, 39] }
            ] } }),
            &["Bob", "Carol"],
        ),
        Case::new(
            "range_inclusive",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "<=": [20, { "field": "age" }, 30] } }),
            &["Alice", "Bob", "Dave"],
        ),
        Case::new(
            "range_strict",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "<": [20, { "field": "age" }, 40] } }),
            &["Alice", "Dave"],
        ),
        Case::new(
            "is_null",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "==": [{ "field": "nickname" }, null] } }),
            &["Alice", "Bob"],
        ),
        Case::new(
            "is_not_null",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "!=": [{ "field": "nickname" }, null] } }),
            &["Carol", "Dave"],
        ),
        Case::new(
            "missing",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "missing": ["nickname"] } }),
            &["Alice", "Bob"],
        ),
        // Text anchors. The patterns are chosen so no backend's case-folding
        // rule can change the answer — case sensitivity is the one behaviour
        // the dialect documents as backend-defined rather than normalising
        // (W13), so asserting it here would be asserting a divergence.
        Case::new(
            "starts_with",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "starts_with": [{ "field": "name" }, "Ca"] } }),
            &["Carol"],
        ),
        Case::new(
            "ends_with",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "ends_with": [{ "field": "name" }, "ol"] } }),
            &["Carol"],
        ),
        Case::new(
            "contains",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "in": ["ar", { "field": "name" }] } }),
            &["Carol"],
        ),
        // -- W8: null ordering ------------------------------------------------
        // A null sorts as the smallest value: first on `asc`, last on `desc`,
        // with the secondary key breaking ties inside the null group. Mongo's
        // `find` used to disagree with SQL and ES on both directions, because
        // the old rule ("nulls last on asc") is not expressible there.
        Case::new(
            "null_ordering_asc_puts_nulls_first",
            json!({ "source": "users", "sort": [{ "nickname": "asc" }, { "name": "asc" }] }),
            &["Alice", "Bob", "Carol", "Dave"],
        ),
        Case::new(
            "null_ordering_desc_puts_nulls_last",
            json!({ "source": "users", "sort": [{ "nickname": "desc" }, { "name": "asc" }] }),
            &["Dave", "Carol", "Alice", "Bob"],
        ),
        // -- paging and projection shape --------------------------------------
        Case::new(
            "skip_and_limit",
            json!({ "source": "users", "sort": by_name(), "skip": 1, "limit": 2 }),
            &["Bob", "Carol"],
        ),
        Case::new(
            "projection_returns_exactly_the_requested_fields",
            json!({ "source": "users", "sort": by_name(), "fields": ["name"] }),
            &["Alice", "Bob", "Carol", "Dave"],
        )
        .projecting(&["name"]),
        // -- schema renames -----------------------------------------------
        // `handle` is the logical name for the physical `nickname` column (see
        // `parity_schema`). The rename has to be applied by `filter`, `fields`
        // and `sort` alike, on every backend. On SQL an unresolved `fields`
        // entry is a hard "no such column"; on the document stores it is
        // silent, which is why the `sort` carries the assertion — ordering on
        // a name that does not exist yields a different row order, and the row
        // order is what the matrix compares.
        Case::new(
            "a_rename_applies_to_filter",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "==": [{ "field": "handle" }, "cee"] } }),
            &["Carol"],
        ),
        Case::new(
            "a_rename_applies_to_fields_and_sort",
            json!({ "source": "users", "fields": ["name", "handle"],
                    "sort": [{ "handle": "desc" }, { "name": "asc" }] }),
            &["Dave", "Carol", "Alice", "Bob"],
        ),
        // -- capability-gated combinations ------------------------------------
        // `include` hydration is SQL-only; the document stores refuse it rather
        // than nesting silently empty children.
        Case::new(
            "include_is_sql_only",
            json!({ "source": "users", "sort": by_name(), "fields": ["name", "id"],
                    "include": { "orders": {
                        "fields": ["total"], "sort": [{ "total": "desc" }], "limit": 2
                    } } }),
            &["Alice", "Bob", "Carol", "Dave"],
        )
        .erroring_on(DOC_STORES),
        // The include's `sort` names a column its `fields` does not. The key has
        // to reach the *outer* select — the per-parent page is cut by ranking
        // rows in a sub-select and filtering the rank outside it, so the outer
        // `ORDER BY` can only name columns the sub-select emits. Projecting just
        // `fields` + the join key made this a hard failure on PostgreSQL
        // (`column "id" does not exist`) and MySQL (`Unknown column 'id' in
        // 'order clause'`) — which is why it is a matrix row and not a
        // SQLite-only one: SQLite accepts the same SQL and silently orders by a
        // string literal instead.
        Case::new(
            "include_can_sort_on_a_column_it_does_not_project",
            json!({ "source": "users", "sort": by_name(), "fields": ["name", "id"],
                    "include": { "orders": {
                        "fields": ["total"], "sort": [{ "id": "desc" }], "limit": 2
                    } } }),
            &["Alice", "Bob", "Carol", "Dave"],
        )
        .erroring_on(DOC_STORES),
        // A junction join is not expressible in either document store's filter
        // language. On SQL the (empty) junction correctly matches nobody.
        Case::new(
            "many_to_many_quantifier_is_sql_only",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "some": [{ "field": "tags" },
                                         { "==": [{ "field": "label" }, "vip"] }] } }),
            &[],
        )
        .erroring_on(DOC_STORES),
        // `all` is set-equivalent on SQL; ES cannot express it over nesting and
        // Mongo cannot join a referenced collection at all.
        Case::new(
            "all_over_a_referenced_relation",
            json!({ "source": "users", "sort": by_name(),
                    "filter": { "all": [{ "field": "orders" },
                                        { ">": [{ "field": "total" }, 40] }] } }),
            &["Alice"],
        )
        .erroring_on(DOC_STORES),
        // Deep pagination is bounded by ES's result window and by nothing else.
        Case::new(
            "deep_pagination_is_rejected_on_es_only",
            json!({ "source": "users", "sort": by_name(), "skip": 9990, "limit": 100 }),
            &[],
        )
        .erroring_on(&[Backend::Es]),
        // The page bounds are shared, so these refuse identically everywhere.
        Case::new(
            "limit_over_the_cap_is_rejected_everywhere",
            json!({ "source": "users", "sort": by_name(), "limit": 5000 }),
            &[],
        )
        .erroring_on(EVERY_BACKEND),
        Case::new(
            "skip_over_the_cap_is_rejected_everywhere",
            json!({ "source": "users", "sort": by_name(), "skip": 10001 }),
            &[],
        )
        .erroring_on(EVERY_BACKEND),
        Case::new(
            "an_unknown_envelope_key_is_rejected_everywhere",
            json!({ "source": "users", "sort": by_name(), "fileds": ["name"] }),
            &[],
        )
        .erroring_on(EVERY_BACKEND),
    ]
}

/// The one fixture dataset, written through the portable write dialect so the
/// same rows land on every backend. `orders` carries the relation rows the
/// capability cases quantify over; `tags` / `user_tags` stay empty (the m2m case
/// is about the capability, not about the join's contents).
fn seed_tasks(h: &BackendHarness) -> Vec<Value> {
    let mut tasks = Vec::new();
    if let Some(sql) = h.ddl_users() {
        tasks.push(dsl::ddl(&h.connector_name, "ddl_users", &sql));
    }
    for (i, sql) in h.ddl_relations().iter().enumerate() {
        tasks.push(dsl::ddl(&h.connector_name, &format!("ddl_rel{i}"), sql));
    }
    tasks.push(write_task(
        h,
        "seed_users",
        json!({
            "op": "insert", "target": "users",
            "values": [
                { "id": "u1", "name": "Alice", "nickname": null, "age": 30, "status": "active" },
                { "id": "u2", "name": "Bob",   "nickname": null, "age": 20, "status": "active" },
                { "id": "u3", "name": "Carol", "nickname": "cee", "age": 40, "status": "inactive" },
                { "id": "u4", "name": "Dave",  "nickname": "dee", "age": 25, "status": "active" }
            ]
        }),
    ));
    // Alice: every order over 40 (so `all` holds). Bob: one order under it (so
    // `all` fails). Carol/Dave: no orders (so `all` fails on the empty set).
    tasks.push(write_task(
        h,
        "seed_orders",
        json!({
            "op": "insert", "target": "orders",
            "values": [
                { "id": "o1", "user_id": "u1", "total": 150 },
                { "id": "o2", "user_id": "u1", "total": 50 },
                { "id": "o3", "user_id": "u2", "total": 10 }
            ]
        }),
    ));
    tasks
}

fn write_task(h: &BackendHarness, id: &str, envelope: Value) -> Value {
    dsl::dw(&h.connector_name, id, h.with_db(envelope))
}

/// A single-task `data_query` workflow for one matrix row.
fn query_task(h: &BackendHarness, case: &Case) -> Value {
    query_task_as(h, "q", "data.result", case.query.clone())
}

/// [`query_task`] with an explicit task id and output path, so one workflow can
/// run several envelopes against one seeded fixture.
fn query_task_as(h: &BackendHarness, id: &str, output: &str, query: Value) -> Value {
    let input = h.with_db(json!({
        "connector": h.connector_name,
        "query": query,
        "schema": parity_schema(),
        "output": output
    }));
    json!({
        "id": id, "name": id,
        "function": { "name": "data_query", "input": input }
    })
}

/// The ordered `name` column of a result array.
fn names(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("a data_query result must be an array")
        .iter()
        .filter_map(|r| r["name"].as_str().map(str::to_string))
        .collect()
}

/// Run the whole matrix against one backend.
async fn assert_parity(backend: Backend) {
    let app = common::test_app().await;
    let label = backend.label();
    let h = common::backends::start(backend, &format!("pty_{label}")).await;
    h.prepare().await;
    common::create_connector(&app, h.connector_json()).await;

    // Seed once, in its own channel.
    let seed_channel = format!("ch-pty-{label}-seed");
    common::create_and_activate_channel(
        &app,
        &seed_channel,
        common::workflow_with_tasks("seed", json!(seed_tasks(&h))),
    )
    .await;
    let (status, body) = dsl::post(&app, &seed_channel, json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK, "seeding failed: {body}");
    assert_eq!(body["status"], "ok", "seeding failed: {body}");

    for (i, case) in cases().iter().enumerate() {
        let channel = format!("ch-pty-{label}-{i}");
        common::create_and_activate_channel(
            &app,
            &channel,
            common::workflow_with_tasks("parity", json!([query_task(&h, case)])),
        )
        .await;
        let (status, body) = dsl::post(&app, &channel, json!({ "data": {} })).await;

        if case.expected_error.contains(&backend) {
            assert!(
                dsl::is_gate_rejection(status, &body),
                "[{label}] case '{}' must be refused with a capability error, got {body}",
                case.name
            );
            continue;
        }

        assert_eq!(
            body["status"], "ok",
            "[{label}] case '{}' must run: {body}",
            case.name
        );
        let rows = &body["data"]["result"];
        assert_eq!(
            names(rows),
            case.rows,
            "[{label}] case '{}' returned a different row set: {body}",
            case.name
        );
        if let Some(expected) = case.fields_exactly {
            for row in rows.as_array().expect("array") {
                let mut keys: Vec<&str> = row
                    .as_object()
                    .expect("a row must be an object")
                    .keys()
                    .map(String::as_str)
                    .collect();
                keys.sort_unstable();
                let mut want: Vec<&str> = expected.to_vec();
                want.sort_unstable();
                assert_eq!(
                    keys, want,
                    "[{label}] case '{}' projected a different shape: {row}",
                    case.name
                );
            }
        }
    }
}

/// Run the count cases against one backend.
///
/// Kept out of the row matrix because they answer a different shape:
/// `{"count": true}` returns `{"count": n}`, not a row set, so [`Case`] — whose
/// whole assertion is the ordered `name` column — cannot carry one. The claim
/// is the same, though: one envelope, one number, on every backend.
///
/// The dialect renders these three completely differently — `SELECT COUNT(*)`,
/// `count_documents`, `POST _count` — which is exactly why the answer has to be
/// asserted equal rather than assumed.
async fn assert_count_parity(backend: Backend) {
    let app = common::test_app().await;
    let label = backend.label();
    let h = common::backends::start(backend, &format!("cnt_{label}")).await;
    h.prepare().await;
    common::create_connector(&app, h.connector_json()).await;

    let seed_channel = format!("ch-cnt-{label}-seed");
    common::create_and_activate_channel(
        &app,
        &seed_channel,
        common::workflow_with_tasks("seed", json!(seed_tasks(&h))),
    )
    .await;
    let (status, body) = dsl::post(&app, &seed_channel, json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK, "seeding failed: {body}");

    // Four users are seeded; three are `active`, one is not.
    let cases: [(&str, Value, u64); 3] = [
        ("count_all", json!({ "source": "users", "count": true }), 4),
        (
            "count_filtered",
            json!({ "source": "users", "count": true,
                    "filter": { "==": [{ "field": "status" }, "active"] } }),
            3,
        ),
        (
            "count_matching_nothing",
            json!({ "source": "users", "count": true,
                    "filter": { "==": [{ "field": "status" }, "nobody"] } }),
            0,
        ),
    ];

    for (i, (name, query, expected)) in cases.iter().enumerate() {
        let channel = format!("ch-cnt-{label}-{i}");
        // Through the matrix's own builder, so the Mongo `database` key the
        // activation gate requires is set exactly as it is for every row case.
        let task = query_task_as(&h, "t_count", "data.result", query.clone());
        common::create_and_activate_channel(
            &app,
            &channel,
            common::workflow_with_tasks("count", json!([task])),
        )
        .await;
        let (status, body) = dsl::post(&app, &channel, json!({ "data": {} })).await;
        assert_eq!(status, StatusCode::OK, "[{label}] {name}: {body}");
        assert_eq!(
            body["data"]["result"],
            json!({ "count": expected }),
            "[{label}] case '{name}' counted differently: {body}"
        );
    }
}

#[tokio::test]
async fn count_parity_sqlite() {
    assert_count_parity(Backend::Sqlite).await;
}

#[tokio::test]
#[ignore]
async fn count_parity_postgres() {
    assert_count_parity(Backend::Postgres).await;
}

#[tokio::test]
#[ignore]
async fn count_parity_mysql() {
    assert_count_parity(Backend::Mysql).await;
}

#[tokio::test]
#[ignore]
async fn count_parity_mongo() {
    assert_count_parity(Backend::Mongo).await;
}

#[tokio::test]
#[ignore]
async fn count_parity_es() {
    assert_count_parity(Backend::Es).await;
}

/// The `include` rows of the matrix only assert *which parents* come back,
/// because the doc stores refuse it outright. The nesting itself — the
/// per-parent page, cut in SQL and ordered by the requested key (F27) — is
/// asserted here, on the backend that has it: three selections over one seeded
/// fixture, covering a projected sort key, a sort key the selection does not
/// project, and no projection at all.
#[tokio::test]
async fn include_hydration_is_bounded_and_ordered() {
    let app = common::test_app().await;
    let h = common::backends::start(Backend::Sqlite, "pty_inc").await;
    common::create_connector(&app, h.connector_json()).await;

    let mut tasks = seed_tasks(&h);
    // Give Alice a third order so a `limit` of 2 actually cuts something.
    tasks.push(write_task(
        &h,
        "more_orders",
        json!({ "op": "insert", "target": "orders",
                "values": [{ "id": "o4", "user_id": "u1", "total": 90 }] }),
    ));
    tasks.push(query_task_as(
        &h,
        "q_paged",
        "data.result",
        json!({ "source": "users", "sort": by_name(), "fields": ["name"],
                "include": { "orders": {
                    "fields": ["total"], "sort": [{ "total": "desc" }], "limit": 2
                } } }),
    ));
    // Same shape, but ordered by a column the selection does not project.
    tasks.push(query_task_as(
        &h,
        "q_sort_only",
        "data.sort_only",
        json!({ "source": "users", "sort": by_name(), "fields": ["name"],
                "include": { "orders": {
                    "fields": ["total"], "sort": [{ "id": "desc" }], "limit": 2
                } } }),
    ));
    // And one with no projection at all.
    tasks.push(query_task_as(
        &h,
        "q_no_fields",
        "data.no_fields",
        json!({ "source": "users", "sort": by_name(), "fields": ["name"],
                "include": { "orders": { "sort": [{ "id": "asc" }] } } }),
    ));

    common::create_and_activate_channel(
        &app,
        "ch-pty-include",
        common::workflow_with_tasks("include", json!(tasks)),
    )
    .await;
    let (status, body) = dsl::post(&app, "ch-pty-include", json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok", "{body}");

    let rows = body["data"]["result"].as_array().expect("array");
    assert_eq!(
        names(&body["data"]["result"]),
        ["Alice", "Bob", "Carol", "Dave"]
    );

    // Alice has three orders (150, 90, 50); `limit: 2` keeps the top two *by the
    // requested order*, not an arbitrary two. This is the F27 regression: with
    // no `ORDER BY` in the child query the truncation picked whichever two rows
    // the plan emitted first.
    let alice: Vec<i64> = rows[0]["orders"]
        .as_array()
        .expect("orders array")
        .iter()
        .map(|o| o["total"].as_i64().expect("total"))
        .collect();
    assert_eq!(alice, vec![150, 90], "body = {body}");

    // The grouping key and the window's rank column are plumbing, not output.
    assert_eq!(child_keys(&rows[0]), vec!["total"], "body = {body}");
    assert!(rows[0].get("id").is_none(), "body = {body}");

    // Bob has one order; Carol and Dave none — an empty array, never a missing key.
    assert_eq!(rows[1]["orders"].as_array().expect("array").len(), 1);
    assert_eq!(rows[2]["orders"], json!([]));
    assert_eq!(rows[3]["orders"], json!([]));

    // Ordering on a column the selection does not project: `id` has to be
    // projected into the sub-select for the outer `ORDER BY` to name it, then
    // dropped again. Alice's orders by id desc are o4 (90) and o2 (50).
    let sort_only = body["data"]["sort_only"].as_array().expect("array");
    let alice: Vec<i64> = sort_only[0]["orders"]
        .as_array()
        .expect("orders array")
        .iter()
        .map(|o| o["total"].as_i64().expect("total"))
        .collect();
    assert_eq!(alice, vec![90, 50], "body = {body}");
    assert_eq!(
        child_keys(&sort_only[0]),
        vec!["total"],
        "the sort key must not leak into the response: {body}"
    );

    // No `fields`: the child's own columns, and *only* those — the window's
    // rank column rides along in `SELECT *` and has to be removed.
    let no_fields = body["data"]["no_fields"].as_array().expect("array");
    assert_eq!(
        child_keys(&no_fields[0]),
        vec!["id", "total", "user_id"],
        "body = {body}"
    );
}

/// The sorted key set of a parent's first nested `orders` child.
fn child_keys(parent: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = parent["orders"][0]
        .as_object()
        .expect("a nested child must be an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

#[tokio::test]
async fn parity_sqlite() {
    assert_parity(Backend::Sqlite).await;
}

#[tokio::test]
#[ignore]
async fn parity_postgres() {
    assert_parity(Backend::Postgres).await;
}

#[tokio::test]
#[ignore]
async fn parity_mysql() {
    assert_parity(Backend::Mysql).await;
}

#[tokio::test]
#[ignore]
async fn parity_mongo() {
    assert_parity(Backend::Mongo).await;
}

#[tokio::test]
#[ignore]
async fn parity_es() {
    assert_parity(Backend::Es).await;
}
