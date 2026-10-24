use std::borrow::Cow;
use std::sync::Arc;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::{Ctx, Function, Issue, Outcome};
use mongodb::bson::{self, Document};
use serde_json::Value;

use super::connector_helpers::{QueryBudget, require_op_allowed, to_connect_error};
use super::mongo_common::{
    docs_to_json, drain_capped, rendered_bson, rendered_document, require_mongo_backend,
};
use super::schema::{FieldKind, FieldSchema};
use crate::config::QueryConfig;
use crate::connector::mongo_pool::MongoPoolCache;
use crate::connector::{ConnectorRegistry, DbConnectorConfig};
use crate::engine::kit::{self, read};
use crate::engine::{ErrorClass, HandlerError};
use crate::query::QueryError;

/// This handler's name, for the helpers that take it as an argument — a
/// reference to the one place it is written (F48).
pub(crate) const NAME: &str = "mongo_read";

/// The `find` the task describes, with every message-dependent part already
/// folded: `{"var": ..}` nodes may sit at any depth of the filter, so a
/// per-request query is expressible.
///
/// The names are borrowed from the function's compiled config.
#[derive(Debug)]
pub struct MongoFind<'a> {
    database: Cow<'a, str>,
    collection: Cow<'a, str>,
    filter: Document,
    projection: Option<Document>,
    sort: Option<Document>,
    limit: Option<u64>,
    skip: Option<u64>,
}

/// The filter as 1.12 read the JSON it folded: any document the bridge
/// takes, an empty one when the task sets none.
fn filter_from_value(filter: &Value) -> Result<Document, HandlerError> {
    bson::to_document(filter).map_err(|e| {
        HandlerError::new(
            ErrorClass::CallerInput,
            format!("Invalid MongoDB filter: {e}"),
        )
    })
}

/// The dialect's reject-never-clamp rule for both bounds. An absent `limit`
/// deliberately does NOT fall back to `default_limit`: the pre-#263 contract
/// is "everything the filter matches, capped", and a silent page-size
/// default would change existing tasks' results.
fn check_bounds(
    limit: Option<u64>,
    skip: Option<u64>,
    limits: &QueryConfig,
) -> Result<(), HandlerError> {
    if let Some(l) = limit
        && l > limits.max_limit
    {
        return Err(HandlerError::from(QueryError::LimitExceeded {
            requested: l,
            max: limits.max_limit,
        }));
    }
    if let Some(s) = skip
        && s > limits.max_skip
    {
        return Err(HandlerError::from(QueryError::SkipExceeded {
            requested: s,
            max: limits.max_skip,
        }));
    }
    Ok(())
}

/// The connector's gates for a read: its `read` operation, and a MongoDB
/// backend.
fn gate(conn: &DbConnectorConfig, connector: &str) -> Result<(), HandlerError> {
    require_op_allowed(&conn.operations, "read", connector)?;
    require_mongo_backend(conn, NAME, connector)
}

/// The documents a find returns, as a JSON array, against a resolved and
/// gated connector, at most `cap` of them. `run_deadline` is the deadline of
/// the run the call belongs to ([`QueryBudget::within`]).
async fn find_documents(
    pool_cache: &MongoPoolCache,
    connector: &str,
    db_config: &DbConnectorConfig,
    cap: usize,
    find: MongoFind<'_>,
    run_deadline: Option<dataflow_rs::run::Instant>,
) -> Result<Value, HandlerError> {
    let client = pool_cache
        .get_client(connector, db_config)
        .await
        .map_err(to_connect_error)?;

    let coll = client
        .database(&find.database)
        .collection::<Document>(&find.collection);
    let docs: Vec<Document> = QueryBudget::within(db_config.query_timeout_ms, run_deadline)
        .run(NAME, async {
            // F11: the Mongo driver has no per-query timeout of its own here —
            // the budget bounds connect + find + drain.
            let mut q = coll.find(find.filter);
            if let Some(p) = find.projection {
                q = q.projection(p);
            }
            if let Some(s) = find.sort {
                q = q.sort(s);
            }
            if let Some(sk) = find.skip {
                q = q.skip(sk);
            }
            if let Some(l) = find.limit {
                q = q.limit(l as i64);
            }
            let cursor = q.await.map_err(|e| e.to_string())?;
            drain_capped(cursor, cap, NAME).await
        })
        .await?;

    docs_to_json(&docs, NAME)
}

/// I185's Defect: the documents are an array, which no context root takes
/// (break 21), so an `output` that is one fails every call.
const ROOT_OUTPUT: &str = "`mongo_read` writes an array of documents, and a context root \
    takes only an object (break 21): set `output` to a path below it, such as `data.documents`";

/// `mongo_read`'s input on dataflow-rs 4, in its field table's order (I179).
/// The connector, the database and the collection are literals, required at
/// load (I183); the filter, the projection and the sort are documents of
/// `var` holes, `$` keys kept as data, as 1.12 folded them; `limit` and
/// `skip` are documents too, as 1.12 executed them (I183), read by
/// `kit::read::mongo_u64`.
#[derive(Debug, FunctionConfig)]
pub struct MongoReadConfig {
    /// The MongoDB connector read through.
    connector: String,
    /// The database.
    database: String,
    /// The collection.
    collection: String,
    /// The `find` filter, in extended JSON; unset matches every document.
    filter: Option<Interpolated>,
    /// The projection document.
    projection: Option<Interpolated>,
    /// The sort document.
    sort: Option<Interpolated>,
    /// At most this many documents; no more than `query.max_limit`.
    limit: Option<Interpolated>,
    /// Documents skipped first; no more than `query.max_skip`.
    skip: Option<Interpolated>,
    /// Where the documents are written.
    #[output(default = "data")]
    output: Output,
}

/// `mongo_read` as a dataflow-rs 4 function: the find `MongoReadHandler`
/// runs, read in the same order.
pub struct MongoReadFunction {
    pub pool_cache: Arc<MongoPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// The `[query]` bounds (F10, W12).
    pub limits: QueryConfig,
}

/// A `limit` or `skip` rendered for this call, as `resolve_u64` reads it: a
/// literal `null` is unset, a rendered one is refused.
fn rendered_u64(
    ctx: &Ctx<'_>,
    doc: Option<&Interpolated>,
    field: &str,
) -> Result<Option<u64>, HandlerError> {
    let value = doc
        .filter(|doc| !doc.raw().is_null())
        .map(|doc| ctx.render(doc).to_owned());
    read::mongo_u64(value.as_ref(), field, NAME)
}

impl MongoReadFunction {
    /// The find a config states, its documents rendered for this call:
    /// `MongoReadHandler::parse`'s reading, in its order, the names
    /// borrowed.
    fn find<'c>(
        &self,
        ctx: &Ctx<'_>,
        cfg: &'c MongoReadConfig,
    ) -> Result<MongoFind<'c>, HandlerError> {
        let filter = match &cfg.filter {
            None => Document::new(),
            Some(doc) => rendered_bson(
                ctx,
                doc,
                |rendered| bson::to_document(rendered).ok(),
                filter_from_value,
            )?,
        };
        let limit = rendered_u64(ctx, cfg.limit.as_ref(), "limit")?;
        let skip = rendered_u64(ctx, cfg.skip.as_ref(), "skip")?;
        check_bounds(limit, skip, &self.limits)?;
        Ok(MongoFind {
            database: Cow::Borrowed(&cfg.database),
            collection: Cow::Borrowed(&cfg.collection),
            filter,
            projection: rendered_document(ctx, cfg.projection.as_ref(), "projection", NAME)?,
            sort: rendered_document(ctx, cfg.sort.as_ref(), "sort", NAME)?,
            limit,
            skip,
        })
    }
}

impl Function for MongoReadFunction {
    type Config = MongoReadConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    /// I185: an `output` that is a literal context root, the default `data`
    /// included, is a Defect at `output`.
    fn prepare(&self, cfg: &mut MongoReadConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        kit::root_output_defect(&cfg.output, ROOT_OUTPUT)
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &MongoReadConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, then `parse`'s order.
        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
        let find = self.find(ctx, cfg)?;
        let run_deadline = ctx.deadline();
        let mut docs = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let db = kit::require_connector::<crate::connector::kind::Db>(&config, &cfg.connector)?;
            gate(db, &cfg.connector)?;
            docs = Some(
                find_documents(
                    &self.pool_cache,
                    &cfg.connector,
                    db,
                    self.limits.max_limit as usize,
                    find,
                    run_deadline,
                )
                .await?,
            );
            Ok(Outcome::Ok)
        })
        .await?;
        if let Some(docs) = docs {
            ctx.set(&output, OwnedDataValue::from(&docs))?;
        }
        Ok(Outcome::Ok)
    }
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const MONGO_READ_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the MongoDB connector.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "database",
        description: "Mongo database name.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "collection",
        description: "Mongo collection name.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "filter",
        description: "Mongo find() filter document (extended JSON: $oid, $date, ... work). Defaults to {}. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "projection",
        description: "Mongo projection document (e.g. {\"name\": 1, \"_id\": 0}). Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "sort",
        description: "Mongo sort document (e.g. {\"created_at\": -1}). Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "limit",
        description: "Maximum documents to return; must not exceed query.max_limit. JSONLogic: a literal, or an expression over the message.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "skip",
        description: "Documents to skip before returning; must not exceed query.max_skip. JSONLogic: a literal, or an expression over the message.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where matched documents are written.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

// -- the config, I185's Defect, and calls against 1.12's records --
//
// No MongoDB is reached. A probe writes the find the function parsed, past
// the connector and its gate, where the driver would be called; the
// function itself runs against `testkit`'s connector to a closed port,
// where a call that passes every gate waits in the driver's server selection
// until its connector's `query_timeout_ms` ends it.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::functions::mongo_common::testkit;
    use crate::engine::functions::twin::{self, Seen};
    use crate::errors::kind::CONNECTOR_DETAIL;
    use dataflow_rs::ErrorKind;
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::json;
    use std::time::Duration;

    fn limits() -> QueryConfig {
        QueryConfig {
            default_limit: 10,
            max_limit: 50,
            max_skip: 100,
            ..QueryConfig::default()
        }
    }

    fn function(registry: Arc<ConnectorRegistry>) -> MongoReadFunction {
        MongoReadFunction {
            pool_cache: testkit::pools(),
            registry,
            limits: limits(),
        }
    }

    /// One task through the real function, over a registry of its own,
    /// `docs` configured with `extra`.
    async fn run(input: Value, data: Value, extra: Value) -> Seen {
        twin::v4(
            NAME,
            function(testkit::connectors(extra).await),
            input,
            data,
        )
        .await
    }

    /// Writes the find the function parsed, at `output`: its prologue, its
    /// parse, the connector and its gate, and no driver call.
    struct FindV4(MongoReadFunction);

    impl Function for FindV4 {
        type Config = MongoReadConfig;

        async fn call(
            &self,
            ctx: &mut Ctx<'_>,
            cfg: &MongoReadConfig,
        ) -> dataflow_rs::Result<Outcome> {
            let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
            let find = self.0.find(ctx, cfg)?;
            let config = kit::resolve_connector(&self.0.registry, &cfg.connector).await?;
            let db = kit::require_connector::<crate::connector::kind::Db>(&config, &cfg.connector)?;
            gate(db, &cfg.connector)?;
            let seen = OwnedDataValue::String(format!("{find:?}"));
            ctx.set(&output, seen)?;
            Ok(Outcome::Ok)
        }
    }

    /// The find the function parses from `input` over `data`.
    async fn parsed(input: Value, data: Value) -> Seen {
        twin::v4(
            NAME,
            FindV4(function(testkit::connectors(json!({})).await)),
            input,
            data,
        )
        .await
    }

    fn rejected(input: Value) -> Vec<(String, IssueCode)> {
        testing::config_with::<MongoReadConfig>(&kit::test_exprs(), input)
            .expect_err("refused")
            .into_iter()
            .filter(|i| i.severity == Severity::Rejected)
            .map(|i| (i.path.to_string(), i.code))
            .collect()
    }

    /// Declaration order is the field table's (I179). The documents are
    /// documents of `var` holes, `$` keys kept as data, and so are `limit`
    /// and `skip`, as 1.12 executed them (I183); the connector, the database
    /// and the collection are literals, required (I183); an unknown key is
    /// refused (break 2a); `output` defaults to `data`.
    #[test]
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = MongoReadConfig::fields().iter().map(|f| &*f.name).collect();
        let table: Vec<&str> = MONGO_READ_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

        let filter = json!({
            "_id": {"$oid": {"var": "data.id"}},
            "n": {"$gte": {"var": ["data.min", 0]}}
        });
        let cfg = testing::config_with::<MongoReadConfig>(
            &kit::test_exprs(),
            json!({
                "connector": "docs", "database": "db", "collection": "c",
                "filter": filter.clone(), "sort": {"n": -1},
                "limit": {"var": "data.limit"}, "skip": 5
            }),
        )
        .expect("compiles");
        let doc = cfg.filter.as_ref().expect("set");
        assert_eq!(
            doc.raw(),
            &filter,
            "the document as authored, `$` keys kept"
        );
        let holes: Vec<String> = doc.holes().map(ToString::to_string).collect();
        assert_eq!(holes, ["data.id", "data.min"]);
        let limit: Vec<String> = cfg
            .limit
            .as_ref()
            .expect("set")
            .holes()
            .map(ToString::to_string)
            .collect();
        assert_eq!(limit, ["data.limit"]);
        assert_eq!(cfg.database, "db");
        let output = cfg.output.constant().map(ToString::to_string);
        assert_eq!(output.as_deref(), Some("data"));

        let named = json!({"connector": "docs", "database": "db", "collection": "c"});
        for field in ["connector", "database", "collection"] {
            let mut input = named.clone();
            input.as_object_mut().expect("an object").remove(field);
            assert_eq!(
                rejected(input.clone()),
                [(field.to_string(), IssueCode::InputParse)]
            );
            input[field] = json!({"var": "data.name"});
            assert_eq!(
                rejected(input),
                [(field.to_string(), IssueCode::InputParse)]
            );
        }
        let mut input = named;
        input["pipeline"] = json!([]);
        let issues = rejected(input);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].0, "pipeline");
    }

    /// I185: the documents are never an object, so a literal root `output`,
    /// the default `data` included, is a Defect at `output`, reported by
    /// `prepare`, which only a compile through the registry runs; a path
    /// below a root is not. The call still runs, as far as its connector.
    #[tokio::test]
    async fn a_root_output_is_i185s_defect() {
        for (output, defect) in [
            (None, true),
            (Some(json!("data")), true),
            (Some(json!("temp_data")), true),
            (Some(json!(null)), true),
            (Some(json!("data.documents")), false),
            (Some(json!({"cat": ["data", ".documents"]})), false),
        ] {
            let mut input = json!({"connector": "absent", "database": "db", "collection": "c"});
            if let Some(output) = &output {
                input["output"] = output.clone();
            }
            let called = testing::call(function(testkit::connectors(json!({})).await), input)
                .exprs(kit::test_exprs())
                .name(NAME)
                .await;
            let defects: Vec<_> = called
                .issues
                .iter()
                .filter(|i| i.severity == Severity::Defect)
                .map(|i| (i.path.to_string(), i.code, i.message.as_str()))
                .collect();
            if defect {
                assert_eq!(
                    defects,
                    [("output".to_string(), IssueCode::InputParse, ROOT_OUTPUT)],
                    "{output:?}"
                );
            } else {
                assert!(defects.is_empty(), "{output:?}: {defects:?}");
            }
            let error = called.outcome.expect_err("there is no connector `absent`");
            assert_eq!(error.code(), "FUNCTION_ERROR", "{output:?}");
            assert!(
                error.message().contains("Connector 'absent' not found"),
                "{output:?}: {error:?}"
            );
        }
    }

    /// The find the function would send: the filter, the projection and the
    /// sort rendered into BSON with their holes filled and their extended
    /// JSON read, `limit` and `skip` read and bounded; and every refusal on
    /// the way, in 1.12's order, with its code and text.
    #[tokio::test]
    async fn the_find_is_read_as_1_12_read_it() {
        let data = json!({
            "id": "665f1f77bcf86cd799439011", "min": 3, "when": "2026-01-02T03:04:05Z",
            "tags": ["a", "b"], "doc": {"k": 1, "$exists": true}, "limit": 5, "skip": 2,
            "neg": -1, "frac": 2.5, "whole": 5.0, "big": 9_007_199_254_740_993_i64,
            "text": "x", "nested": {"a": [1, {"b": null}]}
        });
        let base = json!({
            "connector": "docs", "database": "db", "collection": "c", "output": "data.find"
        });
        for (case, ok) in [
            (json!({}), true),
            (
                json!({"filter": {"_id": {"$oid": {"var": "data.id"}}}}),
                true,
            ),
            (
                json!({
                    "filter": {"n": {"$gte": {"var": ["data.min", 0]}}, "t": {"$in": {"var": "data.tags"}}},
                    "projection": {"_id": 0, "n": 1}, "sort": {"n": -1, "t": 1}
                }),
                true,
            ),
            (
                json!({
                    "filter": {"at": {"$date": {"var": "data.when"}}},
                    "limit": {"var": "data.limit"}, "skip": {"var": "data.skip"}
                }),
                true,
            ),
            (
                json!({"filter": {"var": "data.doc"}, "limit": 50, "skip": 100}),
                true,
            ),
            (
                json!({
                    "filter": {"x": {"var": "data.missing"}, "y": {"var": ["payload.y", "fallback"]}},
                    "limit": null, "skip": null, "projection": null, "sort": null
                }),
                true,
            ),
            (
                json!({"filter": {"n": {"var": "data.big"}, "deep": {"var": "data.nested"},
                                  "w": {"var": "data.whole"}, "f": 1.5}}),
                true,
            ),
            (json!({"filter": null}), false),
            (json!({"filter": "text"}), false),
            (json!({"filter": {"var": "data.text"}}), false),
            (json!({"filter": {"var": "data.tags"}}), false),
            // An `$oid` that names no id is a plain key to the bridge; one
            // that does is an id, not a document.
            (json!({"filter": {"$oid": "not-an-object-id"}}), true),
            (
                json!({"filter": {"$oid": "665f1f77bcf86cd799439011"}}),
                false,
            ),
            (json!({"filter": {"$oid": {"var": "data.id"}}}), false),
            (json!({"filter": {"n": u64::MAX}}), false),
            (json!({"projection": "x"}), false),
            (json!({"projection": {"var": "data.tags"}}), false),
            (json!({"sort": {"var": "data.missing"}}), false),
            (json!({"sort": {"$oid": "665f1f77bcf86cd799439011"}}), false),
            (json!({"limit": {"var": "data.neg"}}), false),
            (json!({"limit": {"var": "data.frac"}}), false),
            (json!({"limit": {"var": "data.whole"}}), false),
            (json!({"limit": {"var": "data.missing"}}), false),
            (json!({"limit": "5"}), false),
            (json!({"skip": true}), false),
            (json!({"limit": 51}), false),
            (json!({"skip": 101}), false),
            // The filter is read before the bounds, the bounds before the
            // projection, and the output before everything.
            (json!({"filter": "text", "limit": 51}), false),
            (json!({"limit": 51, "projection": "x"}), false),
            (
                json!({"output": {"var": "data.limit"}, "filter": "text"}),
                false,
            ),
        ] {
            let mut input = base.clone();
            if let (Some(input), Some(case)) = (input.as_object_mut(), case.as_object()) {
                input.extend(case.clone());
            }
            let v4 = parsed(input, data.clone()).await;
            twin::pinned(&v4, format!("the_find/{case}"));
            assert_eq!(v4.is_ok(), ok, "{case}: {v4:?}");
        }
    }

    /// The function itself, refused at each step as 1.12's handler was, and
    /// past every gate reaching the driver, which waits for a server until
    /// the connector's `query_timeout_ms`, 1.12's `TIMEOUT_ERROR`.
    #[tokio::test]
    async fn the_call_refuses_and_reaches_the_driver() {
        let read = json!({
            "connector": "docs", "database": "db", "collection": "c",
            "filter": {"n": {"var": "data.n"}}, "output": "data.documents"
        });
        let with = |key: &str, value: Value| {
            let mut input = read.clone();
            input[key] = value;
            input
        };
        for (input, extra, code) in [
            (read.clone(), json!({}), "TIMEOUT_ERROR"),
            (
                with("connector", json!("absent")),
                json!({}),
                "FUNCTION_ERROR",
            ),
            (with("connector", json!("api")), json!({}), CONNECTOR_DETAIL),
            (
                with("connector", json!("sql")),
                json!({}),
                "VALIDATION_ERROR",
            ),
            (
                read.clone(),
                json!({"operations": {"read": false}}),
                CONNECTOR_DETAIL,
            ),
            (with("filter", json!("x")), json!({}), "VALIDATION_ERROR"),
            (with("limit", json!(51)), json!({}), "VALIDATION_ERROR"),
            (
                with("output", json!({"var": "data.n"})),
                json!({}),
                "VALIDATION_ERROR",
            ),
        ] {
            let key = format!("the_call/{input}/{extra}");
            let v4 = run(input.clone(), json!({"n": 3}), extra).await;
            twin::pinned(&v4, key);
            assert_eq!(v4.expect_err("refused").code, code, "{input}");
        }
        let v4 = run(read, json!({"n": 3}), json!({})).await;
        assert_eq!(
            v4.expect_err("timed out").message,
            "Timeout error: mongo_read query timed out after 100ms"
        );
    }

    /// `QueryBudget` within the run's deadline (E10): a run deadline before
    /// the connector's `query_timeout_ms` is the run's, and ends the call
    /// `TimedOut` at the deadline, as 1.12's channel timeout ended the run,
    /// writing nothing.
    #[tokio::test]
    async fn a_run_deadline_before_the_query_timeout_ends_the_call_timed_out() {
        let registry = testkit::connectors(json!({"query_timeout_ms": 30_000})).await;
        let input = json!({
            "connector": "docs", "database": "db", "collection": "c", "output": "data.documents"
        });
        let started = std::time::Instant::now();
        let called = testing::call(function(registry), input)
            .exprs(kit::test_exprs())
            .name(NAME)
            .deadline(dataflow_rs::run::Instant::now() + Duration::from_millis(150))
            .await;
        let error = called.outcome.expect_err("the run's deadline passed");
        assert_eq!(*error.kind(), ErrorKind::TimedOut);
        assert_eq!(
            error.message(),
            "the run's deadline passed during `mongo_read`"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(called.writes.is_empty());
    }
}
