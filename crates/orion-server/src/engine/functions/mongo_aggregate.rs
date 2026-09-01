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

use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use mongodb::bson::Document;
use serde_json::Value;

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, require_op_allowed, resolve_value, timed_query, to_connect_error,
};
use super::mongo_common::{docs_to_json, drain_capped, require_mongo_backend};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::TemplatedInput;
use crate::config::QueryConfig;
use crate::connector::ConnectorRegistry;
use crate::connector::mongo_pool::MongoPoolCache;
use crate::engine::HandlerError;

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
pub struct Aggregation {
    database: String,
    collection: String,
    allow_disk_use: bool,
    pipeline: Vec<Document>,
    wants_write_stage: bool,
}

#[async_trait]
impl ConnectorHandler for MongoAggregateHandler {
    const NAME: &'static str = "mongo_aggregate";
    type Kind = crate::connector::kind::Db;
    type Input = TemplatedInput;
    type Parsed = Aggregation;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &TaskContext<'_>,
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
        let resolved = resolve_value(raw, ctx);
        let stages = pipeline_stages(&resolved)?;
        let wants_write_stage = stages
            .iter()
            .any(|(name, _)| WRITE_STAGES.contains(&name.as_str()));
        let pipeline = super::mongo_common::documents_from_values(
            stages.iter().map(|(_, stage)| *stage),
            "pipeline",
            call.name,
        )?;

        Ok(Aggregation {
            database,
            collection,
            allow_disk_use,
            pipeline,
            wants_write_stage,
        })
    }

    fn gate(
        aggregation: &Self::Parsed,
        conn: &crate::connector::DbConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        require_op_allowed(&conn.operations, "read", connector)?;
        require_mongo_backend(conn, <Self as ConnectorHandler>::NAME, connector)?;
        // The write stages are default-deny: `$out`/`$merge` run only on a
        // connector that explicitly says its aggregations may write. Decided
        // from the resolved pipeline, which is why the gate reads the parse.
        if aggregation.wants_write_stage && !conn.aggregate_write_stages {
            return Err(crate::errors::connector_detail_error(format!(
                "pipeline contains a write stage ($out/$merge), which is disabled on \
                 connector '{connector}' — set \"aggregate_write_stages\": true on the \
                 connector to permit it"
            ))
            .into());
        }
        Ok(())
    }

    async fn run(
        &self,
        aggregation: Self::Parsed,
        db_config: &crate::connector::DbConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        let client = self
            .pool_cache
            .get_client(call.connector, db_config)
            .await
            .map_err(to_connect_error)?;
        let coll = client
            .database(&aggregation.database)
            .collection::<Document>(&aggregation.collection);
        let cap = self.limits.max_limit as usize;
        let docs: Vec<Document> = timed_query(db_config.query_timeout_ms, call.name, async {
            // F11: one wall-clock bound over connect + aggregate + drain.
            let mut agg = coll.aggregate(aggregation.pipeline);
            if aggregation.allow_disk_use {
                agg = agg.allow_disk_use(true);
            }
            let cursor = agg.await.map_err(|e| e.to_string())?;
            drain_capped(cursor, cap, NAME).await
        })
        .await?;

        Ok(docs_to_json(&docs).into())
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
    fn parse(pipeline: Value) -> Aggregation {
        let handler = MongoAggregateHandler {
            pool_cache: Arc::new(crate::connector::mongo_pool::MongoPoolCache::new(2)),
            registry: Arc::new(ConnectorRegistry::new(Default::default())),
            limits: Default::default(),
        };
        let datalogic = Arc::new(dataflow_rs::datalogic_rs::Engine::new());
        let mut message = dataflow_rs::Message::from_value(&json!({}));
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
        ConnectorHandler::parse(&handler, &call, &TemplatedInput::from(input), &ctx)
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
}
