List the workflow task functions registered in the Orion engine, along with each function's input JSON Schema.

Use this to discover what built-in functions (e.g. `http_call`, `cache_write`, `publish_kafka`) are available when authoring a workflow's task sequence, and to learn the exact input shape each function expects. Returns a map of function name to its schema/description under the `data` key.
