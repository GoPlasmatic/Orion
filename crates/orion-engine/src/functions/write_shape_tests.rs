use super::*;

/// The guard this whole field exists for: a 19th handler cannot reach the
/// authoring analysis with its output semantics unknown.
///
/// Both tables are checked, because `task_writes` reads both — a built-in
/// added upstream and mirrored here without a shape would be just as silent
/// as a new Orion handler without one.
#[test]
fn every_function_declares_where_it_writes() {
    for schema in schema::registry() {
        assert!(
            write_shape(schema.name).is_some(),
            "function '{}' has no WriteShape",
            schema.name
        );
    }
    for (name, ..) in schema::ENGINE_BUILTINS {
        assert!(
            write_shape(name).is_some(),
            "built-in '{name}' has no WriteShape"
        );
    }
}

/// A name neither table knows contributes no writes, rather than being run
/// through the generic `output` rule. See `task_writes`.
#[test]
fn an_unknown_function_has_no_write_shape() {
    assert!(write_shape("no_such_function").is_none());
}

/// The three shapes the analysis distinguishes, pinned to the functions
/// that motivated them.
#[test]
fn the_declared_shapes_match_the_handlers_they_describe() {
    assert_eq!(write_shape("map"), Some(WriteShape::Mappings));
    assert_eq!(write_shape("parse_json"), Some(WriteShape::Target));
    assert_eq!(write_shape("filter"), Some(WriteShape::Nothing));
    assert_eq!(
        write_shape("data_query"),
        Some(WriteShape::OutputPath {
            default_root: Some("data")
        }),
        "data_query defaults its output to the data root"
    );
    assert_eq!(
        write_shape("db_read"),
        Some(WriteShape::OutputPath {
            default_root: Some("data")
        })
    );
    assert_eq!(write_shape("cache_write"), Some(WriteShape::Nothing));
    assert_eq!(
        write_shape("http_call"),
        Some(WriteShape::OutputPath { default_root: None })
    );
    assert_eq!(
        write_shape("channel_call"),
        Some(WriteShape::OutputPath {
            default_root: Some("data")
        })
    );
}
