use super::*;

/// The vocabulary the admin door validates against must be exactly the
/// fields the gate structs carry. A gate added to a struct but forgotten
/// here would be refused as an unknown key; one left here after being
/// removed would be accepted and ignored — the silent no-op the key check
/// exists to prevent.
#[test]
fn operation_gate_keys_match_the_gate_structs() {
    fn fields<T: Serialize>(value: &T) -> std::collections::BTreeSet<String> {
        serde_json::to_value(value)
            .expect("gates serialize")
            .as_object()
            .expect("gates are a JSON object")
            .keys()
            .cloned()
            .collect()
    }
    fn declared(t: ConnectorType) -> std::collections::BTreeSet<String> {
        t.operation_gate_keys()
            .iter()
            .map(|k| (*k).to_string())
            .collect()
    }

    // The `match` is the point, and it has no wildcard arm: a new
    // connector type does not compile until someone says which gate
    // struct backs it. The hand-written list this replaces enumerated
    // five types and stopped, so a later type's gate struct was never
    // checked at all.
    for &connector_type in ConnectorType::ALL {
        let struct_fields = match connector_type {
            ConnectorType::Db => fields(&OperationGates::default()),
            ConnectorType::Cache => fields(&CacheOperationGates::default()),
            ConnectorType::Http => fields(&HttpOperationGates::default()),
        };
        assert_eq!(
            struct_fields,
            declared(connector_type),
            "{connector_type}: the gate keys the admin door validates \
             against must be exactly the fields the gate struct carries"
        );
    }
}

/// `VALID_CONNECTOR_TYPES` is what a rejected `type` value is reported
/// against, and it is spelled out by hand because `unknown_variant` needs
/// a `&'static` list. This is what keeps that hand-written copy honest.
#[test]
fn the_valid_type_list_is_the_enum() {
    let from_enum: Vec<&str> = ConnectorType::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(from_enum, VALID_CONNECTOR_TYPES);
}

/// Every type parses back from the spelling it serializes to, including
/// the case-insensitivity the wire contract promises.
#[test]
fn every_type_round_trips_through_its_wire_spelling() {
    for &connector_type in ConnectorType::ALL {
        let wire = connector_type.as_str();
        for spelling in [wire.to_string(), wire.to_ascii_uppercase()] {
            let parsed = serde_json::from_value::<ConnectorType>(serde_json::Value::String(
                spelling.clone(),
            ));
            assert_eq!(
                parsed.ok(),
                Some(connector_type),
                "'{spelling}' must parse back to {connector_type}"
            );
        }
    }
}

#[test]
fn test_valid_connector_types() {
    assert!(VALID_CONNECTOR_TYPES.contains(&"http"));
    assert!(!VALID_CONNECTOR_TYPES.contains(&"grpc"));
}

#[test]
fn test_retry_config_default() {
    let config = RetryConfig::default();
    assert_eq!(config.max_retries, 3);
    assert_eq!(config.retry_delay_ms, 1000);
}

#[test]
fn test_connector_config_deserialization_http() {
    let json = r#"{"type":"http","url":"https://api.example.com","headers":{},"retry":{"max_retries":2,"retry_delay_ms":500}}"#;
    let config: ConnectorConfig = serde_json::from_str(json).expect("test");
    match config {
        ConnectorConfig::Http(http) => {
            assert_eq!(http.url, "https://api.example.com");
            assert_eq!(http.retry.max_retries, 2);
            assert_eq!(http.retry.retry_delay_ms, 500);
            assert_eq!(http.max_response_size, 10 * 1024 * 1024);
        }
        _ => unreachable!("Expected Http config"),
    }
}

#[test]
fn test_connector_config_deserialization_db() {
    let json =
        r#"{"type":"db","connection_string":"postgres://localhost/mydb","max_connections":5}"#;
    let config: ConnectorConfig = serde_json::from_str(json).expect("test");
    match config {
        ConnectorConfig::Db(db) => {
            assert_eq!(db.connection_string, "postgres://localhost/mydb");
            assert_eq!(db.max_connections, Some(5));
        }
        _ => unreachable!("Expected Db config"),
    }
}

#[test]
fn test_connector_config_deserialization_cache_memory() {
    let json = r#"{"type":"cache","backend":"memory","default_ttl_secs":60}"#;
    let config: ConnectorConfig = serde_json::from_str(json).expect("test");
    match config {
        ConnectorConfig::Cache(cache) => {
            assert_eq!(cache.backend, "memory");
        }
        _ => unreachable!("Expected Cache config"),
    }
}

/// F22/F7: connector configs deliberately do **not** use
/// `deny_unknown_fields`, so a config carrying a key no connector reads
/// (a db `retry`, an `auth` block) loads and the key is ignored, rather
/// than the connector failing to deserialize and dropping out of the
/// registry.
#[test]
fn legacy_configs_with_removed_fields_still_load() {
    let db = r#"{"type":"db","connection_string":"postgres://localhost/mydb",
                 "driver":"mysql","retry":{"max_retries":5,"retry_delay_ms":250},
                 "auth":{"type":"basic","username":"u","password":"p"}}"#;
    match serde_json::from_str::<ConnectorConfig>(db).expect("0.3.x db config must still load") {
        ConnectorConfig::Db(db) => {
            assert_eq!(db.connection_string, "postgres://localhost/mydb");
        }
        _ => unreachable!("Expected Db config"),
    }

    let cache = r#"{"type":"cache","backend":"memory","default_ttl_secs":300,
                    "max_connections":10,"retry":{"max_retries":3}}"#;
    match serde_json::from_str::<ConnectorConfig>(cache)
        .expect("0.3.x cache config must still load")
    {
        ConnectorConfig::Cache(c) => assert_eq!(c.backend, "memory"),
        _ => unreachable!("Expected Cache config"),
    }
}

/// `retry` on a db connector was declared, validated and documented
/// while the only reader of a `RetryConfig` is `http_call`. An operator who
/// set `max_retries: 5` on their Postgres connector got exactly nothing.
/// It now belongs to the HTTP connector alone, and the round-trip is where
/// that shows: `GET /admin/connectors` renders these structs, so a field
/// re-added without a consumer would start advertising itself again.
#[test]
fn only_the_http_connector_advertises_a_retry_policy() {
    let rendered = |json: &str| {
        let config: ConnectorConfig = serde_json::from_str(json).expect("config parses");
        serde_json::to_value(&config).expect("config serializes")
    };

    assert!(
        !rendered(r#"{"type":"db","connection_string":"sqlite::memory:"}"#)["retry"].is_object(),
        "a db connector must not advertise a retry policy nothing applies"
    );
    assert!(
        !rendered(r#"{"type":"cache","backend":"memory"}"#)["retry"].is_object(),
        "a cache connector must not advertise a retry policy nothing applies"
    );
    // http_call is the one reader, so this one stays.
    assert_eq!(
        rendered(r#"{"type":"http","url":"https://example.com"}"#)["retry"]["max_retries"],
        3
    );
}

#[test]
fn test_connector_config_deserialization_cache_missing_backend() {
    // backend is required — deserialization should fail
    let json = r#"{"type":"cache","operations":{"read":true}}"#;
    let result = serde_json::from_str::<ConnectorConfig>(json);
    assert!(result.is_err());
}

#[test]
fn test_valid_connector_types_expanded() {
    assert!(VALID_CONNECTOR_TYPES.contains(&"http"));
    assert!(VALID_CONNECTOR_TYPES.contains(&"db"));
    assert!(VALID_CONNECTOR_TYPES.contains(&"cache"));
    assert!(!VALID_CONNECTOR_TYPES.contains(&"es"));
    assert!(!VALID_CONNECTOR_TYPES.contains(&"storage"));
    assert!(!VALID_CONNECTOR_TYPES.contains(&"kafka"));
    assert!(!VALID_CONNECTOR_TYPES.contains(&"grpc"));
}

#[test]
fn test_operation_gates_default_all_allowed() {
    let json = r#"{"type":"db","connection_string":"sqlite::memory:"}"#;
    let config: ConnectorConfig = serde_json::from_str(json).expect("test");
    let gates = config.operation_gates().expect("db has gates");
    for op in ["read", "insert", "update", "delete", "upsert", "raw_write"] {
        assert!(gates.allows(op), "{op} should default to allowed");
    }
    assert!(!gates.allows("unknown"), "unknown ops are denied");
}

#[test]
fn test_operation_gates_partial_override() {
    let json = r#"{"type":"db","connection_string":"sqlite::memory:",
        "operations":{"delete":false,"raw_write":false}}"#;
    let config: ConnectorConfig = serde_json::from_str(json).expect("test");
    let gates = config.operation_gates().expect("db has gates");
    assert!(!gates.allows("delete"));
    assert!(!gates.allows("raw_write"));
    assert!(gates.allows("read"));
    assert!(gates.allows("insert"));
    assert!(gates.allows("update"));
    assert!(gates.allows("upsert"));
}

// -----------------------------------------------------------------
// F24: connector-level dialect guards
// -----------------------------------------------------------------

/// Both guards are opt-in: the 1.0 `unmapped: reject` default is what makes
/// the safe mode the default one, and these only add restrictions on top.
#[test]
fn dialect_guards_default_to_off_on_db() {
    let json = r#"{"type":"db","connection_string":"sqlite::memory:"}"#;
    let config: ConnectorConfig = serde_json::from_str(json).expect("test");
    let guards = config.dialect_guards().expect("db carries dialect guards");
    assert!(!guards.require_schema, "{json}");
    assert!(guards.allowed_entities.is_empty(), "{json}");
    let http: ConnectorConfig =
        serde_json::from_str(r#"{"type":"http","url":"https://example.com"}"#).expect("test");
    assert!(http.dialect_guards().is_none(), "http has no dialect");
}

#[test]
fn dialect_guards_parse_from_connector_config() {
    let json = r#"{"type":"db","connection_string":"sqlite::memory:",
        "dialect":{"require_schema":true,"allowed_entities":["users","orders"]}}"#;
    let config: ConnectorConfig = serde_json::from_str(json).expect("test");
    let guards = config.dialect_guards().expect("db has guards");
    assert!(guards.require_schema);
    assert_eq!(guards.allowed_entities, vec!["users", "orders"]);
}

/// `require_schema` has to reject *both* ways of having no effective
/// schema. An entity list under `"unmapped": "identity"` bounds nothing —
/// every undeclared name still passes through — so it is not a schema.
#[test]
fn require_schema_refuses_identity_mode_and_empty_schemas() {
    let off = DialectGuards::default();
    assert!(
        off.schema_is_sufficient(false, true),
        "off permits anything"
    );

    let on = DialectGuards {
        require_schema: true,
        allowed_entities: vec![],
    };
    assert!(
        on.schema_is_sufficient(true, false),
        "entities + reject mode is a real schema"
    );
    assert!(
        !on.schema_is_sufficient(false, false),
        "no entities declared is not a schema"
    );
    assert!(
        !on.schema_is_sufficient(true, true),
        "entities under identity mode still let every other name through"
    );
    assert!(!on.schema_is_sufficient(false, true));
}

/// A misspelled guard key is the guard not applying at all, on a connector
/// whose whole purpose is to bound what workflows reach — so it is a
/// refused config, not an ignored key.
#[test]
fn a_misspelled_dialect_guard_is_rejected() {
    for bad in [
        r#"{"type":"db","connection_string":"sqlite::memory:",
            "dialect":{"requireSchema":true}}"#,
        r#"{"type":"db","connection_string":"sqlite::memory:",
            "dialect":{"allowed_entites":["users"]}}"#,
        r#"{"type":"db","connection_string":"sqlite::memory:",
            "dialect":{"require_schemas":true}}"#,
    ] {
        let err = serde_json::from_str::<ConnectorConfig>(bad)
            .expect_err("a misspelled dialect key must not parse as no guard");
        assert!(err.to_string().contains("unknown field"), "{err}");
    }
}

/// The db gate struct stays db-only: the other types carry
/// gates of their own shape, so `operation_gates()` must not start
/// answering for them (a `raw_write` flag on a cache reads as meaningful
/// and is not — the F22 defect).
#[test]
fn test_operation_gates_absent_on_http() {
    let json = r#"{"type":"http","url":"https://example.com"}"#;
    let config: ConnectorConfig = serde_json::from_str(json).expect("test");
    assert!(config.operation_gates().is_none());
}

// ---- F22e: gates on the other connector types ----

/// Additive with an all-allowed default: every connector authored before
/// the gates existed keeps behaving exactly as it did.
#[test]
fn every_connector_type_defaults_to_fully_open() {
    let cache: ConnectorConfig =
        serde_json::from_str(r#"{"type":"cache","backend":"memory"}"#).expect("test");
    match cache {
        ConnectorConfig::Cache(c) => {
            assert!(c.operations.read);
            assert!(c.operations.write);
        }
        _ => unreachable!("Expected Cache config"),
    }

    let http: ConnectorConfig =
        serde_json::from_str(r#"{"type":"http","url":"https://example.com"}"#).expect("test");
    match http {
        ConnectorConfig::Http(h) => {
            assert!(h.operations.methods.is_empty());
            for method in VALID_HTTP_METHODS {
                assert!(h.operations.allows_method(method), "{method} must be open");
            }
        }
        _ => unreachable!("Expected Http config"),
    }
}

/// A read-only cache connector: `cache_write` is refused, `cache_read` is
/// not. Partial overrides leave the other gate alone.
#[test]
fn a_cache_connector_can_be_made_read_only() {
    let json = r#"{"type":"cache","backend":"memory","operations":{"write":false}}"#;
    let config: ConnectorConfig = serde_json::from_str(json).expect("test");
    match config {
        ConnectorConfig::Cache(c) => {
            assert!(!c.operations.write);
            assert!(c.operations.read);
        }
        _ => unreachable!("Expected Cache config"),
    }
}

/// Naming one method makes the list exhaustive, and matching ignores case
/// so `["get"]` is not a connector that refuses every call.
#[test]
fn an_http_method_allow_list_is_exhaustive_and_case_insensitive() {
    let json = r#"{"type":"http","url":"https://example.com",
                   "operations":{"methods":["get","POST"]}}"#;
    let config: ConnectorConfig = serde_json::from_str(json).expect("test");
    match config {
        ConnectorConfig::Http(h) => {
            assert!(h.operations.allows_method("GET"));
            assert!(h.operations.allows_method("post"));
            assert!(!h.operations.allows_method("DELETE"));
            assert!(!h.operations.allows_method("PUT"));
        }
        _ => unreachable!("Expected Http config"),
    }
}

#[test]
fn test_http_connector_config_defaults() {
    let json = r#"{"type":"http","url":"https://example.com"}"#;
    let config: ConnectorConfig = serde_json::from_str(json).expect("test");
    match config {
        ConnectorConfig::Http(http) => {
            assert!(http.headers.is_empty());
            assert!(http.auth.is_none());
            assert_eq!(http.retry.max_retries, 3);
            assert_eq!(http.retry.retry_delay_ms, 1000);
            assert_eq!(http.max_response_size, 10 * 1024 * 1024);
        }
        _ => unreachable!("Expected Http config"),
    }
}
