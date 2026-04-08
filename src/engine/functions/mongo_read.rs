use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;
use futures::TryStreamExt;
use mongodb::bson::{self, Document};
use serde_json::Value;

use super::connector_helpers::{
    apply_output, extract_custom_input, require_db_connector, require_str_field, resolve_connector,
};
use crate::connector::ConnectorRegistry;
use crate::connector::mongo_pool::MongoPoolCache;

/// Workflow function handler for reading documents from MongoDB.
pub struct MongoReadHandler {
    pub pool_cache: Arc<MongoPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl AsyncFunctionHandler for MongoReadHandler {
    async fn execute(
        &self,
        message: &mut Message,
        config: &FunctionConfig,
        _datalogic: Arc<DataLogic>,
    ) -> dataflow_rs::Result<(usize, Vec<Change>)> {
        let input = extract_custom_input(config, "mongo_read")?;
        let connector_name = require_str_field(input, "connector", "mongo_read")?;
        let database = require_str_field(input, "database", "mongo_read")?;
        let collection = require_str_field(input, "collection", "mongo_read")?;

        // Optional filter document (default: {} = match all)
        let filter_val = input
            .get("filter")
            .cloned()
            .unwrap_or(Value::Object(serde_json::Map::new()));
        let filter_doc = bson::to_document(&filter_val)
            .map_err(|e| DataflowError::Validation(format!("Invalid MongoDB filter: {}", e)))?;

        let connector_config = resolve_connector(&self.registry, connector_name).await?;
        let db_config = require_db_connector(&connector_config, connector_name)?;

        let client = self
            .pool_cache
            .get_client(connector_name, db_config)
            .await
            .map_err(|e| DataflowError::function_execution(e.to_string(), None))?;

        let coll = client.database(database).collection::<Document>(collection);
        let cursor = coll.find(filter_doc).await.map_err(|e| {
            DataflowError::function_execution(format!("MongoDB query failed: {}", e), None)
        })?;
        let docs: Vec<Document> = cursor.try_collect().await.map_err(|e| {
            DataflowError::function_execution(format!("MongoDB cursor failed: {}", e), None)
        })?;

        // Convert BSON documents to JSON
        let result: Vec<Value> = docs
            .iter()
            .filter_map(|doc| bson::to_bson(doc).ok())
            .filter_map(|bson_val| serde_json::to_value(&bson_val).ok())
            .collect();

        let output_path = input
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or("data");

        let changes = apply_output(message, output_path, Value::Array(result));
        Ok((1, changes))
    }
}
