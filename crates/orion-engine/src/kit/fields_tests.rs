use super::*;
use crate::ResolvedSecrets;
use crate::functions::twin;
use crate::operators::orion_exprs;
use dataflow_rs::expr::{ExprEngine, ExprVisitor};
use dataflow_rs::function::{FunctionConfig, testing};
use dataflow_rs::program::AuthoredPath;
use dataflow_rs::{Function, Message, Outcome};
use serde_json::{Map, json};

/// The store the probes read: a string, a non-string, and a client id.
fn secrets() -> ResolvedSecrets {
    let mut values = Map::new();
    values.insert("signing".into(), json!("s3cr3t-signing-key"));
    values.insert("count".into(), json!(7));
    values.insert("aud".into(), json!("partner-app"));
    ResolvedSecrets::from_values(values)
}

fn exprs() -> ExprEngine {
    orion_exprs(&secrets(), 0).expect("Orion's expressions build")
}

/// The message the probes run over.
fn data() -> Value {
    json!({
        "suffix": "eu",
        "n": 5,
        "aud1": "a1",
        "list": ["l1", "l2"],
        "one": "o1",
        "num": 3,
        "secname": "aud",
        "sec": {"secret": "aud"},
        "ref": "env://PATH",
    })
}

/// What a read returned: `{"ok": value}`, or `{"err": [class, message]}`.
fn seen(result: Result<Value, HandlerError>) -> Value {
    match result {
        Ok(value) => json!({"ok": value}),
        Err(e) => json!({"err": [format!("{:?}", e.class), e.msg]}),
    }
}

/// `input` (`{field: value}`, or `{}` when `value` is `None`).
fn input(field: &str, value: Option<&Value>) -> Option<Value> {
    Some(match value {
        Some(value) => json!({field: value}),
        None => json!({}),
    })
}

/// Runs `f` through `testing::call` over the message and store, and
/// returns what it wrote at `data.out` and `data.input`.
async fn run_v4<F: Function>(f: F, input: Option<Value>) -> (Value, Value) {
    let message = Message::from_parts(
        OwnedDataValue::Null,
        OwnedDataValue::from(&data()),
        OwnedDataValue::Null,
    );
    let called = testing::call(f, input.unwrap_or_else(|| json!({})))
        .exprs(exprs())
        .name("probe")
        .message(message)
        .await;
    assert!(called.issues.is_empty(), "{:?}", called.issues);
    assert_eq!(called.outcome.expect("the probe returns Ok"), Outcome::Ok);
    let read = |at: &str| {
        let path: dataflow_rs::Path = at.parse().expect("a path");
        called
            .message
            .context()
            .get(&path)
            .map_or(Value::Null, Value::from)
    };
    (read("data.out"), read("data.input"))
}

/// Holds a probe's `out` to 1.12's record `case` for `value`.
#[track_caller]
fn pinned(out: &Value, case: &str, value: Option<&Value>) {
    let key = match value {
        Some(value) => format!("{case}/{value}"),
        None => format!("{case}/unset"),
    };
    twin::pinned(&Ok(without_path(out)), key);
}

/// `out` with this process's `PATH` spelled `<PATH>`: the `env://PATH`
/// cases resolve it, and a record must not depend on the machine.
fn without_path(out: &Value) -> Value {
    let Ok(path) = std::env::var("PATH") else {
        return out.clone();
    };
    let text = out.to_string().replace(&path, "<PATH>");
    serde_json::from_str(&text).expect("still JSON")
}

/// Writes `out` and the call's input, as a layer reads it.
fn write(ctx: &mut Ctx<'_>, out: &Value) -> dataflow_rs::Result<Outcome> {
    let input = ctx.input()?;
    ctx.set(&"data.out".parse()?, OwnedDataValue::from(out))?;
    ctx.set(&"data.input".parse()?, input)?;
    Ok(Outcome::Ok)
}

// -- KeyMaterial ------------------------------------------------------

#[derive(Debug, FunctionConfig)]
struct KeyConfig {
    key: Option<KeyMaterial>,
}

/// `crypto`'s `key`, read through the kit.
struct KeyV4;

impl Function for KeyV4 {
    type Config = KeyConfig;

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &KeyConfig) -> dataflow_rs::Result<Outcome> {
        let read = key_material(cfg.key.as_ref(), ctx, "key", "crypto").await;
        write(ctx, &seen(read.map(Value::String)))
    }
}

/// Every spelling resolves as 1.12's `key_material_field` resolved it —
/// the material, or the class and message of the refusal — and `input()`
/// writes the field `null` exactly when it reads a secret.
#[tokio::test]
async fn key_material_resolves_as_1_12_did() {
    let cases = [
        (Some(json!("a-literal-key")), json!("a-literal-key")),
        (Some(json!("env://PATH")), json!("env://PATH")),
        (
            Some(json!("env://ORION_KIT_SURELY_UNSET_7Q2")),
            json!("env://ORION_KIT_SURELY_UNSET_7Q2"),
        ),
        (Some(json!({"secret": "signing"})), Value::Null),
        (Some(json!({"secret": ["signing"]})), Value::Null),
        (Some(json!({"secret": "count"})), Value::Null),
        (
            Some(json!({"cat": [{"secret": "signing"}, "-", {"var": "data.suffix"}]})),
            Value::Null,
        ),
        (Some(json!({"var": "data.suffix"})), json!("eu")),
        (Some(json!({"var": "data.n"})), json!(5)),
        (Some(Value::Null), Value::Null),
        (None, Value::Null),
    ];
    for (value, in_input) in cases {
        let (out, written) = run_v4(KeyV4, input("key", value.as_ref())).await;
        pinned(&out, "key_material", value.as_ref());
        assert_eq!(
            written.get("key").cloned().unwrap_or(Value::Null),
            in_input,
            "key = {value:?}"
        );
        if value.is_none() {
            assert_eq!(written, json!({}), "an unset key is absent from the input");
        }
    }
    // The material of the store's key.
    let (out, _) = run_v4(KeyV4, input("key", Some(&json!({"secret": "signing"})))).await;
    assert_eq!(out, json!({"ok": "s3cr3t-signing-key"}));
}

#[test]
fn key_material_compiles_a_string_as_text_and_anything_else_as_an_expression() {
    let compile = |raw: Value| {
        testing::config_with::<KeyConfig>(&exprs(), json!({"key": raw}))
            .expect("compiles")
            .key
            .expect("set")
    };
    assert!(matches!(compile(json!("plain")), KeyMaterial::Text(t) if t == "plain"));
    assert!(matches!(compile(json!("env://K")), KeyMaterial::Text(t) if t == "env://K"));
    assert!(matches!(
        compile(json!({"secret": "signing"})),
        KeyMaterial::Expr(_)
    ));
    assert!(matches!(
        compile(json!({"cat": [{"secret": "signing"}, {"var": "data.suffix"}]})),
        KeyMaterial::Expr(_)
    ));
    assert!(matches!(compile(Value::Null), KeyMaterial::Expr(_)));
    assert!(matches!(
        KeyMaterial::kind(),
        FieldKind::Expr(ValueKind::String)
    ));
    // An unset key is `None`, the "requires" refusal at call.
    let unset = testing::config_with::<KeyConfig>(&exprs(), json!({})).expect("compiles");
    assert!(unset.key.is_none());
}

// -- KeyMaterialList --------------------------------------------------

#[derive(Debug, FunctionConfig)]
struct ListConfig {
    audience: Option<KeyMaterialList>,
}

/// `jwt_verify`'s `audience`, read through the kit.
struct ListV4;

impl Function for ListV4 {
    type Config = ListConfig;

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &ListConfig) -> dataflow_rs::Result<Outcome> {
        let read = key_material_list(cfg.audience.as_ref(), ctx, "audience", "jwt_verify").await;
        let read = read.map(|values| Value::Array(values.into_iter().map(Value::String).collect()));
        write(ctx, &seen(read))
    }
}

/// 1.12's `string_or_vec`'s cases, each read as it read them: a string,
/// an array, holes in either, a secret node as the field or an element,
/// a `null` element dropped, and every refusal.
#[tokio::test]
async fn key_material_list_reads_as_1_12_did() {
    let cases = [
        Some(json!("aud-literal")),
        Some(json!(["a", "b"])),
        Some(json!(["a", null, "b"])),
        Some(json!([{"var": "data.aud1"}, {"var": "data.aud2"}])),
        Some(json!({"var": "data.list"})),
        Some(json!({"var": "data.one"})),
        Some(json!({"var": "data.missing"})),
        Some(json!({"secret": "aud"})),
        Some(json!({"secret": ["aud"]})),
        Some(json!(["x", {"secret": "aud"}])),
        Some(json!({"secret": "count"})),
        Some(json!([{"secret": "count"}])),
        Some(json!({"secret": {"var": "data.secname"}})),
        Some(json!([{"secret": {"var": "data.secname"}}])),
        Some(json!({"secret": 7})),
        Some(json!([{"secret": 7}])),
        Some(json!(["env://PATH"])),
        Some(json!(["env://ORION_KIT_SURELY_UNSET_7Q2"])),
        Some(json!([7])),
        Some(json!([{"var": "data.num"}])),
        Some(json!([["nested"]])),
        Some(json!(7)),
        Some(json!({"a": 1})),
        Some(Value::Null),
        None,
        // 1.12's reading of a hole's value, kept by I183: a string it
        // yields resolves as a reference, and a secret node it yields
        // reads the store.
        Some(json!({"var": "data.ref"})),
        Some(json!({"var": "data.sec"})),
    ];
    for value in cases {
        let (out, _) = run_v4(ListV4, input("audience", value.as_ref())).await;
        pinned(&out, "key_material_list", value.as_ref());
    }
    // A `null` element is dropped; a hole that finds nothing is too.
    let (out, _) = run_v4(
        ListV4,
        input("audience", Some(&json!(["a", null, {"var": "data.aud2"}]))),
    )
    .await;
    assert_eq!(out, json!({"ok": ["a"]}));
}

/// `input()` writes each secret node `null`, as the field or at its
/// index, and fills every hole.
#[tokio::test]
async fn key_material_list_input_redacts_its_secret_nodes() {
    for (value, written) in [
        (json!({"secret": "aud"}), Value::Null),
        (
            json!(["x", {"secret": "aud"}, {"var": "data.aud1"}]),
            json!(["x", null, "a1"]),
        ),
        (json!({"var": "data.list"}), json!(["l1", "l2"])),
        (json!("env://PATH"), json!("env://PATH")),
    ] {
        let (_, input) = run_v4(ListV4, input("audience", Some(&value))).await;
        assert_eq!(input["audience"], written, "audience = {value}");
    }
}

/// The engine's secret checks see a list's secret nodes: a name the
/// store lacks is refused at the node, as for any expression.
#[test]
fn key_material_lists_secret_nodes_are_visited() {
    /// Every reported expression's path.
    struct Paths(Vec<String>);
    impl ExprVisitor for Paths {
        fn expr(&mut self, at: &AuthoredPath, _raw: &Value, _records: bool) {
            self.0.push(at.to_string());
        }
    }
    let cfg = testing::config_with::<ListConfig>(
        &exprs(),
        json!({"audience": ["x", {"secret": "aud"}, {"var": "data.aud1"}]}),
    )
    .expect("compiles");
    let list = cfg.audience.expect("set");
    let mut paths = Paths(Vec::new());
    list.visit(&mut Visit::new(&mut paths, AuthoredPath::root()));
    assert_eq!(paths.0, ["[1]"]);
    assert!(matches!(
        KeyMaterialList::kind(),
        FieldKind::Expr(ValueKind::Any)
    ));
}

// -- KeyRef -----------------------------------------------------------

#[derive(Debug, FunctionConfig)]
struct RefConfig {
    key: KeyRef,
}

/// `jwt_verify`'s `keys[].key`, read through the kit.
struct RefV4;

impl Function for RefV4 {
    type Config = RefConfig;

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &RefConfig) -> dataflow_rs::Result<Outcome> {
        let read = cfg.key.material(ctx, "jwt_verify.keys").await;
        write(ctx, &seen(read.map(Value::String)))
    }
}

/// Every spelling resolves as 1.12's `key_material` resolved it: a string as a
/// literal or a reference, a secret node from the store, and anything
/// else refused, an expression included, which is never evaluated. The
/// input writes the field `null` exactly when it reads a secret, and
/// anything else as written.
#[tokio::test]
async fn key_ref_resolves_as_1_12_did() {
    let cases = [
        (json!("a-literal-key"), json!("a-literal-key")),
        (json!("env://PATH"), json!("env://PATH")),
        (
            json!("env://ORION_KIT_SURELY_UNSET_7Q2"),
            json!("env://ORION_KIT_SURELY_UNSET_7Q2"),
        ),
        (json!({"secret": "signing"}), Value::Null),
        (json!({"secret": ["signing"]}), Value::Null),
        (json!({"secret": "count"}), Value::Null),
        // Not a literal name: refused, never folded.
        (
            json!({"secret": {"var": "data.secname"}}),
            json!({"secret": {"var": "data.secname"}}),
        ),
        (
            json!({"secret": "signing", "x": 1}),
            json!({"secret": "signing", "x": 1}),
        ),
        // An expression is refused, not evaluated: no key from the
        // message, where `KeyMaterial` would read one.
        (json!({"var": "data.suffix"}), json!({"var": "data.suffix"})),
        (
            json!({"cat": ["k-", {"var": "data.suffix"}]}),
            json!({"cat": ["k-", {"var": "data.suffix"}]}),
        ),
        (json!(7), json!(7)),
        (json!(["k"]), json!(["k"])),
        (Value::Null, Value::Null),
    ];
    for (value, in_input) in cases {
        let (out, written) = run_v4(RefV4, input("key", Some(&value))).await;
        pinned(&out, "key_ref", Some(&value));
        assert_eq!(written["key"], in_input, "key = {value}");
    }
    // The material of the store's key.
    let (out, _) = run_v4(RefV4, input("key", Some(&json!({"secret": "signing"})))).await;
    assert_eq!(out, json!({"ok": "s3cr3t-signing-key"}));
}

/// A string is text and a secret node a secret, visited so the engine's
/// checks see it; anything else is held as written, visited by nothing.
#[test]
fn key_ref_compiles_a_secret_node_and_visits_only_it() {
    /// Every reported expression's path.
    struct Paths(Vec<String>);
    impl ExprVisitor for Paths {
        fn expr(&mut self, at: &AuthoredPath, _raw: &Value, _records: bool) {
            self.0.push(at.to_string());
        }
    }
    let compile = |raw: Value| {
        testing::config_with::<RefConfig>(&exprs(), json!({"key": raw}))
            .expect("compiles")
            .key
    };
    assert!(matches!(compile(json!("plain")), KeyRef::Text(t) if t == "plain"));
    assert!(matches!(
        compile(json!({"secret": ["signing"]})),
        KeyRef::Secret(_)
    ));
    assert!(matches!(
        compile(json!({"var": "data.k"})),
        KeyRef::Other(_)
    ));
    assert!(compile(Value::Null).is_null());
    assert!(!compile(json!("")).is_null());
    for (raw, visited) in [
        (json!({"secret": "signing"}), vec!["key"]),
        (json!({"var": "data.k"}), vec![]),
        (json!({"cat": [{"secret": "signing"}]}), vec![]),
        (json!("plain"), vec![]),
    ] {
        let cfg =
            testing::config_with::<RefConfig>(&exprs(), json!({"key": raw})).expect("compiles");
        let mut paths = Paths(Vec::new());
        FunctionConfig::visit(&cfg, &mut paths);
        assert_eq!(paths.0, visited, "{raw}");
    }
    assert!(matches!(
        KeyRef::kind(),
        FieldKind::Literal(ValueKind::String)
    ));
    let issues =
        testing::config_with::<RefConfig>(&exprs(), json!({})).expect_err("a key is required");
    assert_eq!(issues[0].path.to_string(), "key", "{issues:?}");
}

/// A config holding an ordered map of expressions.
#[derive(Debug, dataflow_rs::function::FunctionConfig)]
struct MembersConfig {
    m: Option<Members<Expr<OwnedDataValue>>>,
}

/// A config holding an ordered map of literals.
#[derive(Debug, dataflow_rs::function::FunctionConfig)]
struct LiteralMembers {
    m: Members<String>,
}

/// Writes its resolved input, as a layer reads it, at `data.input`.
struct Echo;

impl Function for Echo {
    type Config = MembersConfig;

    async fn call(&self, ctx: &mut Ctx<'_>, _cfg: &MembersConfig) -> dataflow_rs::Result<Outcome> {
        let input = ctx.input()?;
        ctx.set(&"data.input".parse()?, input)?;
        Ok(Outcome::Ok)
    }
}

/// `Members` keeps the authored order through compile, walk and resolve,
/// where a `BTreeMap` would sort; `null` is unset, a non-object refused,
/// and each member's issue sits at its key.
#[tokio::test]
async fn members_keep_the_order_written() {
    let input = json!({"m": {"z": "last-written-first", "a": {"var": "data.n"}, "m": 7}});
    let cfg = testing::config_with::<MembersConfig>(&exprs(), input.clone()).expect("compiles");
    let members = cfg.m.as_ref().expect("set");
    let keys: Vec<&str> = members.iter().map(|(key, _)| key).collect();
    assert_eq!(keys, ["z", "a", "m"]);
    assert_eq!(members.len(), 3);

    /// The paths a walk reports, in order.
    struct Paths(Vec<String>);
    impl ExprVisitor for Paths {
        fn expr(&mut self, at: &AuthoredPath, _raw: &Value, _records: bool) {
            self.0.push(at.to_string());
        }
    }
    let mut paths = Paths(Vec::new());
    FunctionConfig::visit(&cfg, &mut paths);
    assert_eq!(paths.0, ["m.z", "m.a", "m.m"]);

    let data = OwnedDataValue::from(&data());
    let called = testing::call(Echo, input)
        .exprs(exprs())
        .message(Message::from_parts(
            OwnedDataValue::Null,
            data,
            OwnedDataValue::Null,
        ))
        .await;
    assert_eq!(called.outcome.expect("echoes"), Outcome::Ok);
    let written = called
        .message
        .context()
        .get(&"data.input".parse().expect("a path"))
        .expect("written");
    assert_eq!(
        serde_json::to_string(written).expect("json"),
        r#"{"m":{"z":"last-written-first","a":5,"m":7}}"#
    );

    let unset =
        testing::config_with::<MembersConfig>(&exprs(), json!({"m": null})).expect("compiles");
    assert!(unset.m.is_none());
    let issues = testing::config_with::<MembersConfig>(&exprs(), json!({"m": [1]}))
        .expect_err("not an object");
    assert_eq!(issues[0].path.to_string(), "m", "{issues:?}");
    let issues =
        testing::config_with::<LiteralMembers>(&exprs(), json!({"m": {"ok": "a", "bad": 7}}))
            .expect_err("a member is not a string");
    assert_eq!(issues[0].path.to_string(), "m.bad", "{issues:?}");
}
