use super::*;

fn plugin_entry(name: &str) -> FunctionEntry {
    FunctionEntry {
        name: name.to_string(),
        description: "a plugin function".to_string(),
        category: "transform".to_string(),
        source: Source::Plugin,
        input_fields: Some(vec![FieldSpec {
            name: "message".to_string(),
            description: "the message".to_string(),
            kind: FieldKind::String,
            required: true,
            resolvable: true,
            secret_at: &[],
            template_at: &[],
        }]),
        writes: WriteShape::OutputPath {
            default_root: Some("data"),
        },
        retry_safety: RetrySafety::Pure,
        deny_unknown: true,
        validate_static: None,
        connector: None,
        plugin: Some(PluginBinding {
            id: "acme.codec".to_string(),
            version: 1,
            digest: "sha256:00".to_string(),
            abi: "orion:plugin@1.0.0".to_string(),
        }),
    }
}

/// Every dataflow-rs built-in is accepted.
///
/// The engine publishes the list (`def::BUILTIN_FUNCTIONS`); every name on
/// it runs inline with no registration.
#[test]
fn every_builtin_is_accepted() {
    let registry = FunctionRegistry::builtin();
    for name in dataflow_rs::def::BUILTIN_FUNCTIONS {
        assert!(
            registry.contains(name),
            "'{name}' runs with no registration, so rejecting it at create \
             refuses a workflow the engine would happily execute"
        );
    }
}

/// A function outside the engine's built-ins is accepted only if Orion
/// registered one — and `enrich` is the case that proves it matters.
///
/// A name accepted at create with no handler behind it activates cleanly
/// and then fails *every* request with `FunctionNotFound`. Nothing
/// registers a handler for `enrich`.
#[test]
fn a_function_without_a_handler_is_refused() {
    let registry = FunctionRegistry::builtin();
    assert!(registry.contains("http_call"));
    assert!(
        !registry.contains("enrich"),
        "Orion registers no `enrich` handler, so the name must be refused \
         at create rather than at every request"
    );
}

/// Every static row is an entry, with its schema, and an unknown name is
/// not.
#[test]
fn every_orion_handler_is_an_entry_with_its_schema() {
    let registry = FunctionRegistry::builtin();
    for schema in schema::registry() {
        let entry = registry.get(schema.name);
        assert!(entry.is_some(), "'{}' has no entry", schema.name);
        let entry = entry.expect("asserted above");
        assert_eq!(entry.source, Source::Orion);
        assert_eq!(
            entry.input_fields.as_ref().map(Vec::len),
            Some(schema.input_fields.len()),
            "'{}' lost fields in conversion",
            schema.name
        );
        assert_eq!(entry.writes, schema.writes);
        assert_eq!(entry.retry_safety, schema.retry_safety);
        assert_eq!(entry.connector, schema.connector);
    }
    assert!(!registry.contains("__not_a_function__"));
    assert!(registry.get("__not_a_function__").is_none());
}

/// `validate`, 1.12's spelling of `validation`, names nothing: v3
/// spellings are not read.
#[test]
fn the_v3_spelling_of_validation_names_nothing() {
    let registry = FunctionRegistry::builtin();
    assert!(registry.get("validate").is_none());
    assert!(!registry.contains("validate"));
    assert!(registry.contains("validation"));
}

#[test]
fn the_catalogue_is_sorted_by_name() {
    let names: Vec<String> = FunctionRegistry::builtin()
        .catalogue()
        .into_iter()
        .map(|e| e.name)
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}

/// The typos the `UNKNOWN_FUNCTION` message exists for: the suggestions
/// must be the real, registered names they misspell.
#[test]
fn suggestion_recovers_common_typos() {
    let registry = FunctionRegistry::builtin();
    for (typo, expected) in [
        ("data_writes", "data_write"),
        ("jwt_verifiy", "jwt_verify"),
        ("cache_readd", "cache_read"),
    ] {
        assert_eq!(
            registry.suggest(typo),
            Some(expected),
            "'{typo}' should point at '{expected}'"
        );
    }
}

/// The window scales with name length, so garbage that is far from every
/// registered name gets no suggestion rather than a wrong one —
/// `http_request` is a plausible typo but seven edits from `http_call`.
#[test]
fn suggestion_is_silent_when_nothing_is_close() {
    let registry = FunctionRegistry::builtin();
    assert_eq!(registry.suggest("http_request"), None);
    assert_eq!(registry.suggest("no_such_function_xyz"), None);
    assert_eq!(registry.suggest("totally_unrelated"), None);
    assert_eq!(registry.suggest("x"), None);
}

/// Suggestions only ever name something the engine can actually run.
#[test]
fn suggestion_never_names_an_unknown_function() {
    let registry = FunctionRegistry::builtin();
    for typo in ["data_writes", "jwt_verifiy", "cache_readd"] {
        if let Some(candidate) = registry.suggest(typo) {
            assert!(
                registry.contains(candidate),
                "'{candidate}' is suggested for '{typo}' but is not itself registered"
            );
        }
    }
}

/// F52: which functions take a connector is one fact on the function's
/// row now, so "declares a connector but no type" cannot be written. What
/// can still go wrong is the row itself being dropped, so the set is
/// pinned by name.
#[test]
fn the_connector_bearing_functions_are_exactly_these() {
    let registry = FunctionRegistry::builtin();
    let mut takes: Vec<&str> = registry
        .entries()
        .filter(|e| e.takes_connector())
        .map(|e| e.name.as_str())
        .collect();
    takes.sort_unstable();
    assert_eq!(
        takes,
        [
            "cache_delete",
            "cache_incr",
            "cache_read",
            "cache_write",
            "data_query",
            "data_write",
            "db_read",
            "db_write",
            "http_call",
        ]
    );
    for entry in registry.entries() {
        if let Some(rule) = entry.connector {
            assert!(
                !rule.types.is_empty(),
                "'{}' takes a connector but names no connector type, so activation \
                 cannot check it",
                entry.name
            );
            assert!(
                entry
                    .input_fields
                    .as_deref()
                    .is_some_and(|fields| { fields.iter().any(|f| f.name == "connector") }),
                "'{}' declares a connector rule but no `connector` field",
                entry.name
            );
        }
    }
}

/// An engine built-in declares nothing, so nothing folds and nothing is a
/// secret path — treating it as permissive would make the authoring checks
/// vacuous exactly where they cannot see.
#[test]
fn an_engine_builtin_declares_no_field_facts() {
    let registry = FunctionRegistry::builtin();
    assert!(!registry.is_resolvable_field("map", "mappings"));
    assert!(registry.secret_paths("map", "mappings").is_empty());
    assert!(registry.template_paths("map", "mappings").is_empty());
    assert!(
        registry
            .validate_input("map", &serde_json::json!(7), "tasks[0]")
            .is_empty()
    );
}

/// A name two entries claim is refused — every reader assumes a name means
/// one function, and last-wins would have validation check one schema
/// while the engine dispatches another.
#[test]
fn a_name_claimed_twice_is_refused() {
    let registry = FunctionRegistry::builtin();
    let err = registry
        .with_entries(vec![plugin_entry("crypto")])
        .expect_err("a plugin may not shadow an Orion handler");
    assert!(err.contains("'crypto'"), "{err}");
    assert!(err.contains("orion") && err.contains("plugin"), "{err}");

    let err = registry
        .with_entries(vec![plugin_entry("acme.a"), plugin_entry("acme.a")])
        .expect_err("two plugin entries of one name");
    assert!(err.contains("'acme.a'"), "{err}");
}

/// The generation's registry: the built-ins plus what was loaded, with
/// the plugin entry answering every question a built-in does.
#[test]
fn a_plugin_entry_is_an_ordinary_entry() {
    let registry = FunctionRegistry::builtin()
        .with_entries(vec![plugin_entry("acme.codec.parse")])
        .expect("extends");
    assert!(registry.contains("acme.codec.parse"));
    assert!(registry.contains("map") && registry.contains("crypto"));
    assert_eq!(
        registry.write_shape("acme.codec.parse"),
        Some(WriteShape::OutputPath {
            default_root: Some("data")
        })
    );
    assert!(registry.is_resolvable_field("acme.codec.parse", "message"));
    assert!(!registry.takes_connector("acme.codec.parse"));

    let errors = registry.validate_input(
        "acme.codec.parse",
        &serde_json::json!({"messag": "x"}),
        "tasks[0]",
    );
    let codes: Vec<&str> = errors.iter().map(|e| e.code.as_str()).collect();
    assert!(codes.contains(&"REQUIRED"), "{errors:?}");
    assert!(codes.contains(&"UNKNOWN_FIELD"), "{errors:?}");

    let catalogue = registry.catalogue();
    let entry = catalogue
        .iter()
        .find(|e| e.name == "acme.codec.parse")
        .expect("catalogued");
    assert_eq!(entry.source, Source::Plugin);
    let json = serde_json::to_value(entry).expect("serialises");
    assert_eq!(json["source"], "plugin");
    assert_eq!(json["plugin"]["id"], "acme.codec");
    assert_eq!(json["plugin"]["digest"], "sha256:00");
    assert_eq!(json["retry_safety"]["kind"], "pure");

    // And a built-in's wire shape is untouched: no `plugin` key appears.
    let map = catalogue.iter().find(|e| e.name == "map").expect("map");
    let json = serde_json::to_value(map).expect("serialises");
    assert!(json.get("plugin").is_none(), "{json}");
    assert!(json.get("input_fields").is_none(), "{json}");
}

/// The owned field serialises exactly as the static row does — the wire
/// shape the OpenAPI document pins must not depend on where an entry came
/// from.
#[test]
fn an_owned_field_serialises_like_the_static_row() {
    for schema in schema::registry() {
        for field in &schema.input_fields {
            let from_row = serde_json::to_value(field).expect("row");
            let from_spec = serde_json::to_value(FieldSpec::from(field)).expect("spec");
            assert_eq!(from_row, from_spec, "{}.{}", schema.name, field.name);
        }
    }
}

/// Names are interned once, and an unregistered name
/// interns to nothing — the observer's cardinality bound.
#[test]
fn registered_names_intern_and_unregistered_ones_do_not() {
    let registry = FunctionRegistry::builtin();
    for name in registry.names() {
        assert_eq!(interned(name), Some(name), "'{name}' not interned");
    }
    assert_eq!(interned("__never_registered__"), None);
    assert_eq!(registry.get("map").expect("map").label(), "map");
}
