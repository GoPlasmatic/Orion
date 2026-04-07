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

use crate::connector::mongo_pool::MongoPoolCache;
use crate::connector::{ConnectorConfig, ConnectorRegistry};

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
        let input = match config {
            FunctionConfig::Custom { input, .. } => input,
            _ => {
                return Err(DataflowError::Validation(
                    "Expected Custom config for mongo_read".into(),
                ));
            }
        };

        let connector_name = input
            .get("connector")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DataflowError::Validation("mongo_read requires 'connector'".into()))?;
        let database = input
            .get("database")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DataflowError::Validation("mongo_read requires 'database'".into()))?;
        let collection = input
            .get("collection")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DataflowError::Validation("mongo_read requires 'collection'".into()))?;

        // Optional filter document (default: {} = match all)
        let filter_val = input
            .get("filter")
            .cloned()
            .unwrap_or(Value::Object(serde_json::Map::new()));
        let filter_doc = bson::to_document(&filter_val)
            .map_err(|e| DataflowError::Validation(format!("Invalid MongoDB filter: {}", e)))?;

        // Resolve connector
        let connector_config = self.registry.get(connector_name).await.ok_or_else(|| {
            DataflowError::function_execution(
                format!("Connector '{}' not found", connector_name),
                None,
            )
        })?;
        let db_config = match connector_config.as_ref() {
            ConnectorConfig::Db(c) => c,
            _ => {
                return Err(DataflowError::Validation(format!(
                    "Connector '{}' is not a database connector",
                    connector_name
                )));
            }
        };

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

        let old_value = super::http_common::get_nested(&message.context, output_path);
        let new_value = Value::Array(result);
        super::http_common::set_nested(&mut message.context, output_path, new_value.clone());
        message.invalidate_context_cache();

        let changes = vec![Change {
            path: Arc::from(output_path),
            old_value: Arc::new(old_value),
            new_value: Arc::new(new_value),
        }];
        Ok((1, changes))
    }
}
