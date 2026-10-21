//! MongoDB aggregation pipelines (#263).
//!
//! The pipeline is an extended-JSON array of stages; **the stage allowlist is
//! data** — read-only stages in `READ_STAGES`, the two write stages
//! (`$out`/`$merge`) behind an explicit connector opt-in
//! (`aggregate_write_stages: true`, default **false** — the one deliberate
//! default-deny, because "aggregation" reads as a read and must not silently
//! write). A future stage is one allowlist row, not new API surface.
//!
//! Stages are validated **after** `{"var": ..}` folding, so a stage smuggled
//! in through message data meets the same refusal as an authored one —
//! fail-closed. The authoring-time validator runs the same check on literal
//! pipelines, so the usual mistakes never wait for a request.

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::{Ctx, Function, Issue, Outcome};
use dataflow_rs_v3::engine::error::DataflowError;
use dataflow_rs_v3::engine::task_context::TaskContext;
use mongodb::bson::{self, Bson, Document};
use serde_json::{Map, Value};

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, QueryBudget, require_op_allowed, resolve_value, to_connect_error,
};
use super::mongo_common::{
    docs_to_json, drain_capped, rendered_bson, report_static, require_mongo_backend,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::{Scope, TemplatedInput};
use crate::config::QueryConfig;
use crate::connector::mongo_pool::MongoPoolCache;
use crate::connector::{ConnectorRegistry, DbConnectorConfig};
use crate::engine::HandlerError;
use crate::engine::kit;

/// This handler's name, for the helpers that take it as an argument — a
/// reference to the one place it is written (F48).
const NAME: &str = <MongoAggregateHandler as ConnectorHandler>::NAME;

/// Read-only stages, allowed on any MongoDB connector whose `read` gate is on.
/// One row per stage — growth is data.
pub(super) const READ_STAGES: &[&str] = &[
    "$addFields",
    "$bucket",
    "$bucketAuto",
    "$count",
    "$densify",
    "$facet",
    "$fill",
    "$geoNear",
    "$graphLookup",
    "$group",
    "$limit",
    "$lookup",
    "$match",
    "$project",
    "$redact",
    "$replaceRoot",
    "$replaceWith",
    "$sample",
    "$search",
    "$searchMeta",
    "$set",
    "$setWindowFields",
    "$skip",
    "$sort",
    "$sortByCount",
    "$unionWith",
    "$unset",
    "$unwind",
    "$vectorSearch",
];

/// Stages that write to a collection; each requires the connector's explicit
/// `aggregate_write_stages: true` opt-in.
pub(super) const WRITE_STAGES: &[&str] = &["$out", "$merge"];

/// Whether `stage` is in either allowlist.
pub(super) fn is_known_stage(stage: &str) -> bool {
    READ_STAGES.contains(&stage) || WRITE_STAGES.contains(&stage)
}

/// Workflow function handler for running an aggregation pipeline.
pub struct MongoAggregateHandler {
    pub pool_cache: Arc<MongoPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// `[query] max_limit` caps the drained result size (F10) — a `$group`
    /// over a large collection must not OOM the process.
    pub limits: QueryConfig,
}

/// The pipeline as it will actually run: `{"var": ..}` folded at any depth,
/// every stage checked against the allowlist, and the answer to the one
/// question the gate needs — whether any stage writes.
///
/// Owned by the dataflow-rs 3 handler, which reads the names off the
/// authored input per call; the names borrowed from the compiled config by
/// the dataflow-rs 4 function.
#[derive(Debug)]
pub struct Aggregation<'a> {
    database: Cow<'a, str>,
    collection: Cow<'a, str>,
    allow_disk_use: bool,
    pipeline: Vec<Document>,
    wants_write_stage: bool,
}

/// The stages of a folded pipeline, as 1.12 read them: the shape and the
/// allowlist judged on the folded JSON, then each stage converted, and
/// whether any stage writes. Shared with the dataflow-rs 4 function, which
/// reaches it only for a pipeline it refuses.
fn stages_from_value(resolved: &Value) -> Result<(Vec<Document>, bool), DataflowError> {
    let stages = pipeline_stages(resolved)?;
    let wants_write_stage = stages
        .iter()
        .any(|(name, _)| WRITE_STAGES.contains(&name.as_str()));
    let pipeline = super::mongo_common::documents_from_values(
        stages.iter().map(|(_, stage)| *stage),
        "pipeline",
        NAME,
    )?;
    Ok((pipeline, wants_write_stage))
}

/// The stages of a rendered pipeline read straight from its BSON: every
/// stage a one-key document whose key is an allowlisted stage, and whether
/// any writes. `None` for anything [`stages_from_value`] would refuse, an
/// empty pipeline included, so the refusal is worded from the folded JSON.
fn stages_from_bson(pipeline: Bson) -> Option<(Vec<Document>, bool)> {
    let Bson::Array(items) = pipeline else {
        return None;
    };
    if items.is_empty() {
        return None;
    }
    let mut wants_write_stage = false;
    let mut stages = Vec::with_capacity(items.len());
    for item in items {
        let Bson::Document(stage) = item else {
            return None;
        };
        let mut keys = stage.keys();
        let (Some(name), None) = (keys.next(), keys.next()) else {
            return None;
        };
        if !name.starts_with('$') || !is_known_stage(name) {
            return None;
        }
        wants_write_stage |= WRITE_STAGES.contains(&name.as_str());
        stages.push(stage);
    }
    Some((stages, wants_write_stage))
}

/// The connector's gates for an aggregation: its `read` operation, a MongoDB
/// backend, and the write stages' opt-in. Shared by the dataflow-rs 3 handler
/// and the dataflow-rs 4 function.
fn gate(
    wants_write_stage: bool,
    conn: &DbConnectorConfig,
    connector: &str,
) -> Result<(), HandlerError> {
    require_op_allowed(&conn.operations, "read", connector)?;
    require_mongo_backend(conn, NAME, connector)?;
    // The write stages are default-deny: `$out`/`$merge` run only on a
    // connector that explicitly says its aggregations may write. Decided
    // from the resolved pipeline, which is why the gate reads the parse.
    if wants_write_stage && !conn.aggregate_write_stages {
        return Err(crate::errors::connector_detail_error(format!(
            "pipeline contains a write stage ($out/$merge), which is disabled on \
             connector '{connector}' — set \"aggregate_write_stages\": true on the \
             connector to permit it"
        ))
        .into());
    }
    Ok(())
}

/// The documents an aggregation returns, as a JSON array, against a resolved
/// and gated connector, at most `cap` of them. Shared by the dataflow-rs 3
/// handler and the dataflow-rs 4 function. `run_deadline` is the deadline of
/// the run the call belongs to, which the dataflow-rs 4 function passes on
/// ([`QueryBudget::within`]).
async fn aggregate_documents(
    pool_cache: &MongoPoolCache,
    connector: &str,
    db_config: &DbConnectorConfig,
    cap: usize,
    aggregation: Aggregation<'_>,
    run_deadline: Option<dataflow_rs::run::Instant>,
) -> Result<Value, HandlerError> {
    let client = pool_cache
        .get_client(connector, db_config)
        .await
        .map_err(to_connect_error)?;
    let coll = client
        .database(&aggregation.database)
        .collection::<Document>(&aggregation.collection);
    let docs: Vec<Document> = QueryBudget::within(db_config.query_timeout_ms, run_deadline)
        .run(NAME, async {
            // F11: one wall-clock bound over connect + aggregate + drain.
            let mut agg = coll.aggregate(aggregation.pipeline);
            if aggregation.allow_disk_use {
                agg = agg.allow_disk_use(true);
            }
            let cursor = agg.await.map_err(|e| e.to_string())?;
            drain_capped(cursor, cap, NAME).await
        })
        .await?;

    Ok(docs_to_json(&docs, NAME)?)
}

#[async_trait]
impl ConnectorHandler for MongoAggregateHandler {
    const NAME: &'static str = "mongo_aggregate";
    type Kind = crate::connector::kind::Db;
    type Input = TemplatedInput;
    type Parsed = Aggregation<'static>;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &Scope<'_, '_>,
    ) -> Result<Self::Parsed, HandlerError> {
        let database = call.require_str(input, "database")?.to_string();
        let collection = call.require_str(input, "collection")?.to_string();
        let allow_disk_use = input
            .raw()
            .get("allow_disk_use")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Fold `{"var": ..}` at any depth, then validate the *resolved* stages
        // — the allowlist judges what will actually run, not what was typed.
        let raw = input.get("pipeline").ok_or_else(|| {
            DataflowError::Validation(format!("{} requires 'pipeline' field", call.name))
        })?;
        let (pipeline, wants_write_stage) = stages_from_value(&resolve_value(raw, ctx))?;

        Ok(Aggregation {
            database: Cow::Owned(database),
            collection: Cow::Owned(collection),
            allow_disk_use,
            pipeline,
            wants_write_stage,
        })
    }

    fn gate(
        aggregation: &Self::Parsed,
        conn: &DbConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        gate(aggregation.wants_write_stage, conn, connector)
    }

    async fn run(
        &self,
        aggregation: Self::Parsed,
        db_config: &DbConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        Ok(aggregate_documents(
            &self.pool_cache,
            call.connector,
            db_config,
            self.limits.max_limit as usize,
            aggregation,
            None,
        )
        .await?
        .into())
    }
}

/// Validate the resolved pipeline's shape: an array of single-key objects,
/// each key a stage in the allowlists. Returns `(stage_name, stage_json)`
/// pairs. Every violation names its index.
fn pipeline_stages(resolved: &Value) -> Result<Vec<(String, &Value)>, DataflowError> {
    let Value::Array(items) = resolved else {
        return Err(DataflowError::Validation(format!(
            "{NAME} 'pipeline' must resolve to an array of stage objects"
        )));
    };
    if items.is_empty() {
        return Err(DataflowError::Validation(format!(
            "{NAME} 'pipeline' must not be empty"
        )));
    }
    let mut stages = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let (name, _) = stage_entry(item).ok_or_else(|| {
            DataflowError::Validation(format!("{NAME} {}", stage_shape_message(i)))
        })?;
        if !is_known_stage(name) {
            return Err(DataflowError::Validation(format!(
                "{NAME} {}",
                unknown_stage_message(i, name)
            )));
        }
        stages.push((name.to_string(), item));
    }
    Ok(stages)
}

/// One wording per pipeline shape rule, shared verbatim by the runtime check
/// and authoring-time validation ([`validate_static_input`]) — the two
/// surfaces must state the same rule.
fn stage_shape_message(i: usize) -> String {
    format!(
        "pipeline[{i}] must be an object with exactly one $-prefixed stage \
         key (e.g. {{\"$match\": {{..}}}})"
    )
}

fn unknown_stage_message(i: usize, name: &str) -> String {
    format!(
        "pipeline[{i}] stage '{name}' is not in the allowed stage set \
         (read stages: {}; write stages, connector-gated: {})",
        READ_STAGES.join(", "),
        WRITE_STAGES.join(", ")
    )
}

/// The `($stage, body)` of a stage object, if it has exactly that shape.
pub(super) fn stage_entry(item: &Value) -> Option<(&str, &Value)> {
    let map = item.as_object()?;
    if map.len() != 1 {
        return None;
    }
    let (k, v) = map.iter().next()?;
    k.starts_with('$').then_some((k.as_str(), v))
}

// -- Authoring-time validation (F53) --

/// Structural checks on a *literal* pipeline, shared with
/// `schema.rs::validate_input` — a misspelled stage fails at create, update,
/// import, `POST /admin/workflows/validate` and `orion-server lint`, not at
/// the first request. A `{"var": ..}` pipeline (or element) defers to the
/// runtime check, which judges the resolved value with the same rules.
pub(super) fn validate_static_input(
    input: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    let mut errs = Vec::new();
    let Some(pipeline) = input.get("pipeline") else {
        return errs; // presence is the generic field schema's job
    };
    if pipeline.as_object().is_some_and(|m| m.contains_key("var")) {
        return errs; // whole pipeline from the message: runtime's call
    }
    let Some(items) = pipeline.as_array() else {
        errs.push((
            "pipeline",
            "invalid_pipeline",
            "'pipeline' must be an array of stage objects".to_string(),
        ));
        return errs;
    };
    if items.is_empty() {
        errs.push((
            "pipeline",
            "invalid_pipeline",
            "'pipeline' must not be empty".to_string(),
        ));
        return errs;
    }
    for (i, item) in items.iter().enumerate() {
        if item.as_object().is_some_and(|m| m.contains_key("var")) {
            continue; // a stage substituted from the message: runtime's call
        }
        match stage_entry(item) {
            None => errs.push(("pipeline", "invalid_pipeline", stage_shape_message(i))),
            Some((name, _)) if !is_known_stage(name) => {
                errs.push(("pipeline", "unknown_stage", unknown_stage_message(i, name)))
            }
            Some(_) => {}
        }
    }
    errs
}

// -- dataflow-rs 4 --

/// I185's Defect: the documents are an array, which no context root takes
/// (break 21), so an `output` that is one fails every call.
const ROOT_OUTPUT: &str = "`mongo_aggregate` writes an array of documents, and a context \
    root takes only an object (break 21): set `output` to a path below it, such as \
    `data.documents`";

/// `mongo_aggregate`'s input on dataflow-rs 4, in its field table's order
/// (I179). The connector, the database and the collection are literals, and
/// the pipeline a document of `var` holes, `$` keys kept as data, as 1.12
/// folded it; all four are required at load (I183). `allow_disk_use` keeps
/// 1.12's reading (I183): a literal `true` spills, anything else does not,
/// an expression included, which 1.12 never evaluated. Its `check` holds
/// 1.12's authoring-time rules at load.
#[derive(Debug, FunctionConfig)]
#[config(check = check)]
pub struct MongoAggregateConfig {
    /// The MongoDB connector read through.
    connector: String,
    /// The database.
    database: String,
    /// The collection.
    collection: String,
    /// The aggregation stages, in extended JSON.
    pipeline: Interpolated,
    /// Lets the server spill large stages to disk, read only when it is a
    /// literal boolean.
    allow_disk_use: Option<Value>,
    /// Where the documents are written.
    #[output(default = "data")]
    output: Output,
}

/// 1.12's authoring-time rules for `mongo_aggregate`
/// ([`validate_static_input`], by which create, update, import, validate
/// and `lint` refused a task), on the config at load: each refusal at
/// `pipeline`, in 1.12's words. The pipeline judged is the config's, as
/// authored, so the two cannot drift.
fn check(cfg: &MongoAggregateConfig, cx: &mut CompileCx<'_>) {
    let mut input = Map::new();
    input.insert("pipeline".to_string(), cfg.pipeline.raw().clone());
    report_static(validate_static_input(&input), cx);
}

/// `mongo_aggregate` as a dataflow-rs 4 function: the pipeline
/// `MongoAggregateHandler` runs, read in the same order.
pub struct MongoAggregateFunction {
    pub pool_cache: Arc<MongoPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// `[query] max_limit` caps the drained result size (F10).
    pub limits: QueryConfig,
}

impl MongoAggregateFunction {
    /// The aggregation a config states, its pipeline rendered for this call
    /// and its stages judged after the holes are filled, so message data
    /// meets the allowlist an authored stage meets: `parse`'s reading, the
    /// names borrowed.
    fn aggregation<'c>(
        &self,
        ctx: &Ctx<'_>,
        cfg: &'c MongoAggregateConfig,
    ) -> Result<Aggregation<'c>, HandlerError> {
        let allow_disk_use = cfg
            .allow_disk_use
            .as_ref()
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let (pipeline, wants_write_stage) = rendered_bson(
            ctx,
            &cfg.pipeline,
            |rendered| stages_from_bson(bson::to_bson(rendered).ok()?),
            stages_from_value,
        )?;
        Ok(Aggregation {
            database: Cow::Borrowed(&cfg.database),
            collection: Cow::Borrowed(&cfg.collection),
            allow_disk_use,
            pipeline,
            wants_write_stage,
        })
    }
}

impl Function for MongoAggregateFunction {
    type Config = MongoAggregateConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    /// I185: an `output` that is a literal context root, the default `data`
    /// included, is a Defect at `output`.
    fn prepare(&self, cfg: &mut MongoAggregateConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        kit::root_output_defect(&cfg.output, ROOT_OUTPUT)
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        cfg: &MongoAggregateConfig,
    ) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, then `parse`'s order.
        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
        let aggregation = self.aggregation(ctx, cfg)?;
        let run_deadline = ctx.deadline();
        let mut docs = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let db = kit::require_connector::<crate::connector::kind::Db>(&config, &cfg.connector)?;
            gate(aggregation.wants_write_stage, db, &cfg.connector)?;
            docs = Some(
                aggregate_documents(
                    &self.pool_cache,
                    &cfg.connector,
                    db,
                    self.limits.max_limit as usize,
                    aggregation,
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

pub(super) const MONGO_AGGREGATE_FIELDS: &[FieldSchema] = &[
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
        name: "pipeline",
        description: "Array of aggregation stages (extended JSON). Stage names are allowlisted: read-only stages always; $out/$merge only when the connector sets aggregate_write_stages. Accepts {\"var\": \"path\"} at any depth.",
        kind: FieldKind::Array,
        required: true,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "allow_disk_use",
        description: "Let the server spill large stages to disk. Defaults to false.",
        kind: FieldKind::Bool,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where result documents are written.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().expect("test input is an object").clone()
    }

    fn mongo_connector(aggregate_write_stages: bool) -> crate::connector::DbConnectorConfig {
        crate::connector::DbConnectorConfig {
            connection_string: "mongodb://localhost:27017".to_string(),
            max_connections: None,
            connect_timeout_ms: None,
            query_timeout_ms: None,
            allow_private_urls: true,
            operations: Default::default(),
            dialect: Default::default(),
            aggregate_write_stages,
        }
    }

    /// Parse a pipeline the way the shell does, so `gate` can be asked about
    /// it. No engine, no registry entry, no MongoDB.
    fn parse(pipeline: Value) -> Aggregation<'static> {
        let handler = MongoAggregateHandler {
            pool_cache: Arc::new(crate::connector::mongo_pool::MongoPoolCache::new(2)),
            registry: Arc::new(ConnectorRegistry::new(Default::default())),
            limits: Default::default(),
        };
        let datalogic = Arc::new(dataflow_rs_v3::datalogic_rs::Engine::new());
        let mut message = dataflow_rs_v3::Message::from_value(&json!({}));
        let ctx = TaskContext::new(&mut message, &datalogic);
        let call = ConnectorCall {
            name: MongoAggregateHandler::NAME,
            connector: "analytics",
            channel: "ch".to_string(),
            output: "data".to_string(),
        };
        let input = json!({
            "connector": "analytics",
            "database": "db",
            "collection": "events",
            "pipeline": pipeline,
        });
        let input = TemplatedInput::from(input);
        crate::engine::functions::templated_input::with_scope(&ctx, |scope| {
            ConnectorHandler::parse(&handler, &call, &input, scope)
        })
        .expect("the pipeline parses")
    }

    /// The default-deny that had no test: `$out` and `$merge` write to a
    /// collection, so they run only on a connector that says its aggregations
    /// may write.
    ///
    /// Reachable in-process because the gate is a function of the parse and the
    /// connector, not of a live server — which is the whole reason the handler
    /// shape is a trait. Before this it could only be exercised against a
    /// container.
    #[test]
    fn a_write_stage_needs_the_connectors_opt_in() {
        let parsed = parse(json!([
            { "$match": { "status": "active" } },
            { "$out": "rollup" },
        ]));

        let err = <MongoAggregateHandler as ConnectorHandler>::gate(
            &parsed,
            &mongo_connector(false),
            "analytics",
        )
        .expect_err("a write stage must be refused by default");
        let detail = err.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("$out/$merge"),
            "the refusal must name the stages it is about: {detail:?}"
        );

        assert!(
            <MongoAggregateHandler as ConnectorHandler>::gate(
                &parsed,
                &mongo_connector(true),
                "analytics",
            )
            .is_ok(),
            "the same pipeline runs on a connector that opted in"
        );
    }

    /// The other half: a read-only pipeline is unaffected by the opt-in, so
    /// leaving it off costs nothing for the ordinary case.
    #[test]
    fn a_read_only_pipeline_passes_the_gate_either_way() {
        let parsed = parse(json!([{ "$group": { "_id": "$q", "n": { "$sum": 1 } } }]));
        for opted_in in [false, true] {
            assert!(
                <MongoAggregateHandler as ConnectorHandler>::gate(
                    &parsed,
                    &mongo_connector(opted_in),
                    "analytics",
                )
                .is_ok(),
                "a read-only pipeline must not depend on aggregate_write_stages"
            );
        }
    }

    #[test]
    fn a_read_only_pipeline_passes_authoring_validation() {
        let errs = validate_static_input(&obj(json!({ "pipeline": [
            { "$match": { "status": "active" } },
            { "$unwind": "$videos" },
            { "$group": { "_id": "$videos.quality", "n": { "$sum": 1 } } },
            { "$sort": { "n": -1 } }
        ] })));
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn an_unknown_stage_is_named_with_its_index() {
        let errs = validate_static_input(&obj(json!({ "pipeline": [
            { "$match": {} },
            { "$currentOp": {} }
        ] })));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].1, "unknown_stage");
        assert!(errs[0].2.contains("pipeline[1]"), "{}", errs[0].2);
        assert!(errs[0].2.contains("$currentOp"), "{}", errs[0].2);
    }

    #[test]
    fn a_stage_that_is_not_a_single_key_object_is_refused() {
        for bad in [json!("not an object"), json!({ "$match": {}, "$sort": {} })] {
            let errs = validate_static_input(&obj(json!({ "pipeline": [bad] })));
            assert_eq!(errs.len(), 1, "{errs:?}");
            assert_eq!(errs[0].1, "invalid_pipeline");
        }
    }

    #[test]
    fn an_empty_pipeline_is_refused() {
        let errs = validate_static_input(&obj(json!({ "pipeline": [] })));
        assert_eq!(errs.len(), 1, "{errs:?}");
    }

    /// `$out`/`$merge` are *known* — authoring passes them; whether they run
    /// is the connector's `aggregate_write_stages` call, judged at runtime.
    #[test]
    fn write_stages_are_known_at_authoring_time() {
        let errs = validate_static_input(&obj(json!({ "pipeline": [
            { "$match": {} },
            { "$merge": { "into": "summary" } }
        ] })));
        assert!(errs.is_empty(), "{errs:?}");
    }

    /// A pipeline (or stage) substituted from the message defers to the
    /// runtime check — which the execution path applies to the resolved value.
    #[test]
    fn var_pipelines_and_stages_defer_to_runtime() {
        let errs = validate_static_input(&obj(json!({
            "pipeline": { "var": "temp_data.pipeline" }
        })));
        assert!(errs.is_empty(), "{errs:?}");
        let errs = validate_static_input(&obj(json!({
            "pipeline": [{ "var": "temp_data.stage" }]
        })));
        assert!(errs.is_empty(), "{errs:?}");
    }

    /// The runtime validator refuses what the allowlist refuses — after var
    /// folding, so message data cannot smuggle a stage past it.
    #[test]
    fn the_runtime_stage_check_matches_the_allowlist() {
        let good = json!([{ "$match": { "x": 1 } }]);
        assert!(pipeline_stages(&good).is_ok());
        let err = pipeline_stages(&json!([{ "$collStats": {} }]))
            .expect_err("diagnostic stages are not allowlisted");
        assert!(err.to_string().contains("$collStats"), "{err}");
        let err = pipeline_stages(&json!([])).expect_err("empty pipeline");
        assert!(err.to_string().contains("must not be empty"), "{err}");
    }

    // -- dataflow-rs 4: the config and its check, I185's Defect, the seams,
    // and twins of the handler --
    //
    // No MongoDB is reached. A probe of each engine writes the aggregation
    // it parsed, past the connector and its gate, where the driver would be
    // called; the functions themselves run against `testkit`'s connector to
    // a closed port, where a call that passes every gate waits in the
    // driver's server selection until the connector's `query_timeout_ms`.

    use crate::engine::functions::connector_handler::Connector;
    use crate::engine::functions::mongo_common::testkit;
    use crate::engine::functions::twin::{self, Seen};
    use crate::errors::kind::CONNECTOR_DETAIL;
    use dataflow_rs::ErrorKind;
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use std::time::Duration;

    fn limits() -> QueryConfig {
        QueryConfig::default()
    }

    fn handler(registry: Arc<ConnectorRegistry>) -> MongoAggregateHandler {
        MongoAggregateHandler {
            pool_cache: testkit::pools(),
            registry,
            limits: limits(),
        }
    }

    fn function(registry: Arc<ConnectorRegistry>) -> MongoAggregateFunction {
        MongoAggregateFunction {
            pool_cache: testkit::pools(),
            registry,
            limits: limits(),
        }
    }

    /// An aggregation on the `docs` connector's `db.events`, with `rest`.
    fn task(rest: Value) -> Value {
        let mut input = json!({"connector": "docs", "database": "db", "collection": "events"});
        if let (Some(input), Some(rest)) = (input.as_object_mut(), rest.as_object()) {
            input.extend(rest.clone());
        }
        input
    }

    /// One task through each engine's real function, each over a registry
    /// of its own, `docs` configured with `extra`.
    async fn both(input: Value, data: Value, extra: Value) -> (Seen, Seen) {
        let v3 = twin::v3(
            NAME,
            Box::new(Connector(handler(testkit::connectors(extra.clone()).await))),
            input.clone(),
            data.clone(),
        )
        .await;
        let v4 = twin::v4(
            NAME,
            function(testkit::connectors(extra).await),
            input,
            data,
        )
        .await;
        (v3, v4)
    }

    /// Writes the aggregation 1.12's handler parsed, at `output`: its
    /// prologue, its parse, the connector and its gate, and no driver call.
    struct AggregationV3(MongoAggregateHandler);

    #[async_trait]
    impl ConnectorHandler for AggregationV3 {
        const NAME: &'static str = "mongo_aggregate";
        type Kind = crate::connector::kind::Db;
        type Input = TemplatedInput;
        type Parsed = Aggregation<'static>;

        fn registry(&self) -> &Arc<ConnectorRegistry> {
            &self.0.registry
        }

        fn parse(
            &self,
            call: &ConnectorCall<'_>,
            input: &TemplatedInput,
            ctx: &Scope<'_, '_>,
        ) -> Result<Self::Parsed, HandlerError> {
            self.0.parse(call, input, ctx)
        }

        fn gate(
            parsed: &Self::Parsed,
            conn: &DbConnectorConfig,
            connector: &str,
        ) -> Result<(), HandlerError> {
            MongoAggregateHandler::gate(parsed, conn, connector)
        }

        async fn run(
            &self,
            aggregation: Self::Parsed,
            _db_config: &DbConnectorConfig,
            _call: &ConnectorCall<'_>,
            _input: &TemplatedInput,
            _ctx: &mut TaskContext<'_>,
        ) -> Result<Produced, HandlerError> {
            Ok(Value::String(format!("{aggregation:?}")).into())
        }
    }

    /// The same, through the function's own reading.
    struct AggregationV4(MongoAggregateFunction);

    impl Function for AggregationV4 {
        type Config = MongoAggregateConfig;

        async fn call(
            &self,
            ctx: &mut Ctx<'_>,
            cfg: &MongoAggregateConfig,
        ) -> dataflow_rs::Result<Outcome> {
            let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
            let aggregation = self.0.aggregation(ctx, cfg)?;
            let config = kit::resolve_connector(&self.0.registry, &cfg.connector).await?;
            let db = kit::require_connector::<crate::connector::kind::Db>(&config, &cfg.connector)?;
            gate(aggregation.wants_write_stage, db, &cfg.connector)?;
            let seen = OwnedDataValue::String(format!("{aggregation:?}"));
            ctx.set(&output, seen)?;
            Ok(Outcome::Ok)
        }
    }

    /// The aggregation each engine parses from `input` over `data`, the
    /// `docs` connector configured with `extra`.
    async fn parsed(input: Value, data: Value, extra: Value) -> (Seen, Seen) {
        let v3 = twin::v3(
            NAME,
            Box::new(Connector(AggregationV3(handler(
                testkit::connectors(extra.clone()).await,
            )))),
            input.clone(),
            data.clone(),
        )
        .await;
        let v4 = twin::v4(
            NAME,
            AggregationV4(function(testkit::connectors(extra).await)),
            input,
            data,
        )
        .await;
        (v3, v4)
    }

    fn rejected(input: Value) -> Vec<(String, IssueCode)> {
        testing::config_with::<MongoAggregateConfig>(&kit::test_exprs(), input)
            .expect_err("refused")
            .into_iter()
            .filter(|i| i.severity == Severity::Rejected)
            .map(|i| (i.path.to_string(), i.code))
            .collect()
    }

    /// Declaration order is the field table's (I179). The pipeline is a
    /// document of `var` holes, `$` keys kept as data; the connector, the
    /// database, the collection and the pipeline are required (I183);
    /// `allow_disk_use` takes any value, as 1.12 did; an unknown key is
    /// refused (break 2a); `output` defaults to `data`.
    #[test]
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = MongoAggregateConfig::fields()
            .iter()
            .map(|f| &*f.name)
            .collect();
        let table: Vec<&str> = MONGO_AGGREGATE_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

        let pipeline = json!([
            {"$match": {"tenant": {"var": "data.tenant"}, "_id": {"$oid": "665f1f77bcf86cd799439011"}}},
            {"var": "temp_data.stage"}
        ]);
        let cfg = testing::config_with::<MongoAggregateConfig>(
            &kit::test_exprs(),
            task(json!({"pipeline": pipeline.clone(), "allow_disk_use": true})),
        )
        .expect("compiles");
        assert_eq!(
            cfg.pipeline.raw(),
            &pipeline,
            "the document as authored, `$` keys kept"
        );
        let holes: Vec<String> = cfg.pipeline.holes().map(ToString::to_string).collect();
        assert_eq!(holes, ["data.tenant", "temp_data.stage"]);
        assert_eq!(cfg.allow_disk_use, Some(json!(true)));
        let output = cfg.output.constant().map(ToString::to_string);
        assert_eq!(output.as_deref(), Some("data"));
        // 1.12 read anything but a literal `true` as `false`, and ran.
        for allow_disk_use in [
            json!("yes"),
            json!({"var": "data.spill"}),
            json!(null),
            json!(1),
        ] {
            testing::config_with::<MongoAggregateConfig>(
                &kit::test_exprs(),
                task(json!({"pipeline": [{"$match": {}}], "allow_disk_use": allow_disk_use})),
            )
            .expect("compiles");
        }

        let named = task(json!({"pipeline": [{"$match": {}}]}));
        for field in ["connector", "database", "collection", "pipeline"] {
            let mut input = named.clone();
            input.as_object_mut().expect("an object").remove(field);
            assert_eq!(
                rejected(input),
                [(field.to_string(), IssueCode::InputParse)]
            );
        }
        let mut input = named;
        input["limit"] = json!(5);
        let issues = rejected(input);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].0, "limit");
    }

    /// The config's issues: each Rejected one's path and message, in order.
    fn check_issues(input: &Value) -> Vec<(String, String)> {
        match testing::config_with::<MongoAggregateConfig>(&kit::test_exprs(), input.clone()) {
            Ok(_) => Vec::new(),
            Err(issues) => issues
                .into_iter()
                .filter(|i| i.severity == Severity::Rejected)
                .map(|i| {
                    assert_eq!(i.code, IssueCode::InputParse, "{i:?}");
                    (i.path.to_string(), i.message)
                })
                .collect(),
        }
    }

    /// The six `validate_static_input` cases above, and the pipeline's other
    /// literal shapes, through the config's check: 1.12's refusals, in its
    /// order, each at `pipeline`, and none where it had none. A refusal is a
    /// load refusal now, a named break: 1.12 refused each at create, update,
    /// import, validate and `lint`, and failed every call on it.
    #[test]
    fn the_check_is_validate_static_input() {
        let cases = [
            // a_read_only_pipeline_passes_authoring_validation
            json!([
                {"$match": {"status": "active"}}, {"$unwind": "$videos"},
                {"$group": {"_id": "$videos.quality", "n": {"$sum": 1}}}, {"$sort": {"n": -1}}
            ]),
            // an_unknown_stage_is_named_with_its_index
            json!([{"$match": {}}, {"$currentOp": {}}]),
            // a_stage_that_is_not_a_single_key_object_is_refused
            json!(["not an object"]),
            json!([{"$match": {}, "$sort": {}}]),
            // an_empty_pipeline_is_refused
            json!([]),
            // write_stages_are_known_at_authoring_time
            json!([{"$match": {}}, {"$merge": {"into": "summary"}}]),
            // var_pipelines_and_stages_defer_to_runtime
            json!({"var": "temp_data.pipeline"}),
            json!([{"var": "temp_data.stage"}]),
            // And the other literal shapes: not an array, `null`, a stage
            // without its `$`, two refusals in one pipeline.
            json!({"$match": {}}),
            json!("text"),
            json!(null),
            json!([{"match": {}}]),
            json!([{"$nope": {}}, 7, {"$limit": 5}]),
        ];
        for pipeline in cases {
            let input = task(json!({"pipeline": pipeline.clone()}));
            let refusals: Vec<(String, String)> =
                validate_static_input(input.as_object().expect("an object"))
                    .into_iter()
                    .map(|(field, _code, message)| (field.to_string(), message))
                    .collect();
            assert_eq!(check_issues(&input), refusals, "{pipeline}");
        }
        let unknown = check_issues(&task(
            json!({"pipeline": [{"$match": {}}, {"$currentOp": {}}]}),
        ));
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].0, "pipeline");
        assert!(unknown[0].1.contains("pipeline[1]"), "{}", unknown[0].1);
    }

    /// I185: the documents are never an object, so a literal root `output`,
    /// the default `data` included, is a Defect at `output`, reported by
    /// `prepare`; a path below a root is not. The call still runs, as far as
    /// its connector.
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
            let mut input = task(json!({"pipeline": [{"$match": {}}]}));
            input["connector"] = json!("absent");
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
        }
    }

    /// `a_write_stage_needs_the_connectors_opt_in`, on the function: `$out`
    /// and `$merge` run only on a connector that says its aggregations may
    /// write, whether the stage is authored or arrives in the message; the
    /// same pipeline passes the gate on a connector that opted in.
    #[tokio::test]
    async fn on_v4_a_write_stage_needs_the_connectors_opt_in() {
        let data = json!({"stage": {"$out": "rollup"}});
        for pipeline in [
            json!([{"$match": {"status": "active"}}, {"$out": "rollup"}]),
            json!([{"$match": {"status": "active"}}, {"var": "data.stage"}]),
        ] {
            let input = task(json!({"pipeline": pipeline.clone(), "output": "data.documents"}));
            testing::config_with::<MongoAggregateConfig>(&kit::test_exprs(), input.clone())
                .expect("compiles");
            let (v3, v4) = both(input.clone(), data.clone(), json!({})).await;
            assert_eq!(v4, v3, "{pipeline}");
            let refused = v4.expect_err("a write stage is refused by default");
            assert_eq!(refused.code, CONNECTOR_DETAIL, "{pipeline}");
            let detail = refused.detail.as_deref().unwrap_or_default();
            assert!(
                detail.contains("$out/$merge"),
                "the stages it is about: {detail:?}"
            );

            let opted_in = json!({"aggregate_write_stages": true});
            let (v3, v4) = both(input, data.clone(), opted_in).await;
            assert_eq!(v4, v3, "{pipeline}");
            assert_eq!(v4.expect_err("the driver times out").code, "TIMEOUT_ERROR");
        }
    }

    /// `a_read_only_pipeline_passes_the_gate_either_way`, on the function: a
    /// read-only pipeline does not depend on `aggregate_write_stages`.
    #[tokio::test]
    async fn on_v4_a_read_only_pipeline_passes_the_gate_either_way() {
        let input = task(json!({
            "pipeline": [{"$group": {"_id": "$q", "n": {"$sum": 1}}}],
            "output": "data.documents"
        }));
        testing::config_with::<MongoAggregateConfig>(&kit::test_exprs(), input.clone())
            .expect("compiles");
        for opted_in in [false, true] {
            let (v3, v4) = both(
                input.clone(),
                Value::Null,
                json!({"aggregate_write_stages": opted_in}),
            )
            .await;
            assert_eq!(v4, v3, "{opted_in}");
            assert_eq!(
                v4.expect_err("past the gate, the driver times out").code,
                "TIMEOUT_ERROR",
                "{opted_in}"
            );
        }
    }

    /// The aggregation each engine would run: the pipeline rendered into
    /// BSON with its holes filled and its extended JSON read, its stages
    /// judged after the holes are filled, `allow_disk_use` read as 1.12 read
    /// it; and every refusal the check leaves to the call, with its code and
    /// text.
    #[tokio::test]
    async fn the_aggregation_is_read_as_on_v3() {
        let data = json!({
            "tenant": "acme", "since": "2026-01-02T03:04:05Z", "id": "665f1f77bcf86cd799439011",
            "stage": {"$limit": 5}, "out": {"$out": "rollup"}, "bad": {"$currentOp": {}},
            "pipeline": [{"$match": {"a": 1}}, {"$count": "n"}], "empty": [], "object": {"a": 1},
            "two": {"$match": {}, "$sort": {}}, "big": u64::MAX, "spill": true
        });
        for (rest, extra, ok) in [
            (
                json!({"pipeline": [
                    {"$match": {"tenant": {"var": "data.tenant"},
                                "at": {"$gte": {"$date": {"var": "data.since"}}},
                                "_id": {"$oid": {"var": "data.id"}}}},
                    {"$group": {"_id": "$kind", "n": {"$sum": 1}}},
                    {"var": "data.stage"}
                ]}),
                json!({}),
                true,
            ),
            (
                json!({"pipeline": {"var": "data.pipeline"}, "allow_disk_use": true}),
                json!({}),
                true,
            ),
            (
                json!({"pipeline": [{"$match": {}}], "allow_disk_use": "yes"}),
                json!({}),
                true,
            ),
            (
                json!({"pipeline": [{"$match": {}}], "allow_disk_use": {"var": "data.spill"}}),
                json!({}),
                true,
            ),
            (
                json!({"pipeline": [{"$match": {}}, {"var": "data.out"}]}),
                json!({"aggregate_write_stages": true}),
                true,
            ),
            (
                json!({"pipeline": [{"$match": {}}, {"var": "data.out"}]}),
                json!({}),
                false,
            ),
            (json!({"pipeline": [{"var": "data.bad"}]}), json!({}), false),
            (json!({"pipeline": [{"var": "data.two"}]}), json!({}), false),
            (
                json!({"pipeline": [{"var": "data.missing"}]}),
                json!({}),
                false,
            ),
            (json!({"pipeline": {"var": "data.empty"}}), json!({}), false),
            (
                json!({"pipeline": {"var": "data.object"}}),
                json!({}),
                false,
            ),
            (
                json!({"pipeline": {"var": "data.missing"}}),
                json!({}),
                false,
            ),
            (
                json!({"pipeline": [{"$match": {"n": {"var": "data.big"}}}]}),
                json!({}),
                true,
            ),
            (
                json!({"pipeline": [{"$match": {"n": u64::MAX}}]}),
                json!({}),
                false,
            ),
            (
                json!({"pipeline": [{"var": "data.bad"}, {"$match": {"n": u64::MAX}}]}),
                json!({}),
                false,
            ),
            (
                json!({"pipeline": [{"$match": {"n": u64::MAX}}, {"var": "data.bad"}]}),
                json!({}),
                false,
            ),
            (
                json!({"pipeline": [{"$match": {"$oid": {"var": "data.id"}}}]}),
                json!({}),
                true,
            ),
            (
                json!({"pipeline": [{"$match": {}}], "output": {"var": "data.spill"}}),
                json!({}),
                false,
            ),
        ] {
            let mut input = task(rest.clone());
            if input.get("output").is_none() {
                input["output"] = json!("data.aggregation");
            }
            let (v3, v4) = parsed(input, data.clone(), extra).await;
            assert_eq!(v4, v3, "{rest}");
            assert_eq!(v4.is_ok(), ok, "{rest}: {v4:?}");
        }
    }

    /// The function itself, refused at each step as 1.12's handler was, and
    /// past every gate reaching the driver, 1.12's `TIMEOUT_ERROR`.
    #[tokio::test]
    async fn the_call_refuses_and_reaches_the_driver_as_on_v3() {
        let aggregate = task(json!({
            "pipeline": [{"$match": {"n": {"var": "data.n"}}}], "output": "data.documents"
        }));
        let with = |key: &str, value: Value| {
            let mut input = aggregate.clone();
            input[key] = value;
            input
        };
        for (input, extra, code) in [
            (aggregate.clone(), json!({}), "TIMEOUT_ERROR"),
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
                aggregate.clone(),
                json!({"operations": {"read": false}}),
                CONNECTOR_DETAIL,
            ),
            (
                with("pipeline", json!({"var": "data.n"})),
                json!({}),
                "VALIDATION_ERROR",
            ),
            (
                with("output", json!({"var": "data.n"})),
                json!({}),
                "VALIDATION_ERROR",
            ),
        ] {
            let (v3, v4) = both(input.clone(), json!({"n": 3}), extra).await;
            assert_eq!(v4, v3, "{input}");
            assert_eq!(v4.expect_err("refused").code, code, "{input}");
        }
    }

    /// `QueryBudget` within the run's deadline (E10): a run deadline before
    /// the connector's `query_timeout_ms` ends the call `TimedOut`, writing
    /// nothing.
    #[tokio::test]
    async fn a_run_deadline_before_the_query_timeout_ends_the_call_timed_out() {
        let registry = testkit::connectors(json!({"query_timeout_ms": 30_000})).await;
        let input = task(json!({"pipeline": [{"$match": {}}], "output": "data.documents"}));
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
            "the run's deadline passed during `mongo_aggregate`"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(called.writes.is_empty());
    }
}
