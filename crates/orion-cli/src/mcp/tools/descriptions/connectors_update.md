Update an existing connector by ID. The connector_json contains only the fields to update — unspecified fields remain unchanged.

Updatable fields: `name`, `connector_type`, `config`, `enabled`.

See `connectors_create` for the full connector JSON format including HTTP auth types and Kafka config.