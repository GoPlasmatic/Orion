//! `jwt_sign` — mint a JWS in a workflow (#267 Part B).
//!
//! Issuance is workflow logic (login, refresh, RFC 7523 client assertions);
//! verification config lives on the channel. Same core as both verify
//! surfaces, driven in reverse. Self-contained like `crypto`: no connector,
//! and dry-run executes it for real. The signing key resolves through the
//! secret registry per call and never enters context, errors, or traces.

use std::borrow::Cow;

use async_trait::async_trait;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::{Ctx, Expr, Function, Issue, Outcome};
use dataflow_rs_v3::engine::error::DataflowError;
use dataflow_rs_v3::engine::functions::AsyncFunctionHandler;
use dataflow_rs_v3::engine::task_context::TaskContext;
use dataflow_rs_v3::engine::task_outcome::TaskOutcome;
use serde_json::{Map, Value};

use super::connector_helpers::{
    apply_output, parse_duration_secs, resolve_duration_secs, resolve_optional_str, resolve_value,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::TemplatedInput;
use crate::engine::kit::{self, KeyMaterial, read};
use crate::engine::{ErrorClass, HandlerError};

/// This handler's name in metrics, profiles and error messages (F48).
pub const NAME: &str = "jwt_sign";

/// Ten years: past this, "expiring" is a fiction and the author should say
/// what they mean with an explicit `exp` claim.
const MAX_EXPIRES_SECS: u64 = 315_360_000;

/// Workflow function handler that signs JWTs.
pub struct JwtSignHandler;

#[async_trait]
impl AsyncFunctionHandler for JwtSignHandler {
    type Input = TemplatedInput;

    /// Compile the expression fields this handler's table declares. The
    /// `Connector` wrapper does this for the connector handlers; these three
    /// are self-contained — no connector, no egress — so they implement
    /// `AsyncFunctionHandler` directly and say it themselves.
    fn compile_input(
        input: &mut Self::Input,
        c: &dataflow_rs_v3::engine::functions::TemplateCompiler,
    ) -> dataflow_rs_v3::Result<()> {
        input.compile("jwt_sign", c)
    }

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &TemplatedInput,
    ) -> dataflow_rs_v3::Result<TaskOutcome> {
        // Literal prologue (F58): the message-independent refusals first.
        let algorithm = match input.get("algorithm").and_then(Value::as_str) {
            Some(name) => crate::jwt::parse_algorithm(name).map_err(|e| validation(&e))?,
            None => return Err(validation("requires 'algorithm'")),
        };
        // Presence only — the value is read last, after everything the message
        // cannot change (F58). `key` is JSONLogic now, so what it resolves to is
        // not knowable here, but *whether the task named one* is.
        if matches!(input.get("key"), None | Some(Value::Null)) {
            return Err(validation(
                "requires 'key' ({\"secret\": \"name\"}, a secret reference like \
                 env://NAME, an expression, or a literal)",
            ));
        }
        let key_encoding = input.get("key_encoding").and_then(Value::as_str);
        let kid = resolve_optional_str(input, "kid", NAME, ctx)?;
        let output = input
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("data");

        // Claims: a resolvable object — {"var": ..} nodes fold, like every
        // connector function; computed claims are composed with `map` first.
        let mut claims = match input.get("claims") {
            None => serde_json::Map::new(),
            Some(raw) => match resolve_value(raw, ctx) {
                Value::Object(map) => map,
                _ => return Err(validation("'claims' must resolve to an object")),
            },
        };

        let now = chrono::Utc::now().timestamp();
        // Registered-claim conveniences; an explicit field wins over a claims
        // entry of the same name — it is the more specific statement.
        if let Some(iss) = resolve_optional_str(input, "issuer", NAME, ctx)?.as_deref() {
            claims.insert("iss".to_string(), Value::String(iss.to_string()));
        }
        if let Some(aud) = input.get("audience")
            && !aud.is_null()
        {
            claims.insert("aud".to_string(), resolve_value(aud, ctx));
        }
        if let Some(offset) = resolve_duration_secs(input, ctx, NAME, "not_before")? {
            claims.insert("nbf".to_string(), Value::from(now + offset as i64));
        }
        stamp_iat(&mut claims, now)?;
        stamp_exp(
            &mut claims,
            now,
            resolve_duration_secs(input, ctx, NAME, "expires_in")?,
        )?;

        // The key resolves last and lives only inside this call.
        let material = super::secret_ref::key_material_field(input, "key", NAME, ctx).await?;
        let token = mint(algorithm, &material, key_encoding, kid, claims)?;

        apply_output(ctx, output, Value::String(token));
        Ok(TaskOutcome::Success)
    }
}

/// A caller-fixable refusal, named: `jwt_sign`'s messages carry the
/// handler's name themselves.
fn invalid(msg: &str) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, format!("{NAME}: {msg}"))
}

fn validation(msg: &str) -> DataflowError {
    invalid(msg).into()
}

/// `iat`, stamped only when the claims object does not supply one: shared
/// by both engines' paths.
///
/// Unlike `iss`/`aud`/`nbf`/`exp`, `iat` has no dedicated input field — so
/// there is nothing "more specific" to beat a claims entry, and the entry
/// should simply win. Overwriting it with an ambient value made two things
/// impossible: a revocation-pivot scheme cannot forward-date a token minted
/// in the same second as the pivot it must survive, and no offline
/// `*.case.json` case can assert a minted token, because the bytes moved
/// every run.
///
/// `iat` is not security-bearing in Orion — nothing in the runtime makes a
/// trust decision on it — so author control costs no check. (`exp`, which
/// is, has been author-settable since #267 for the same reason: a
/// non-expiring token must be a deliberate, visible choice.)
fn stamp_iat(claims: &mut Map<String, Value>, now: i64) -> Result<(), HandlerError> {
    if let Some(iat) = claims.get("iat") {
        require_numeric_date(iat, "iat")
    } else {
        claims.insert("iat".to_string(), Value::from(now));
        Ok(())
    }
}

/// `exp` from `expires_in`'s seconds, bounded, or the claims' own: shared by
/// both engines' paths. A token without an expiry must be a stated
/// decision, not an omission: either `expires_in`, or an explicit `exp`
/// claim.
fn stamp_exp(
    claims: &mut Map<String, Value>,
    now: i64,
    expires_in: Option<u64>,
) -> Result<(), HandlerError> {
    match expires_in {
        Some(secs) => {
            if secs == 0 || secs > MAX_EXPIRES_SECS {
                return Err(invalid(&format!(
                    "'expires_in' must be between 1 second and {MAX_EXPIRES_SECS} \
                     seconds (10 years)"
                )));
            }
            claims.insert("exp".to_string(), Value::from(now + secs as i64));
            Ok(())
        }
        // Same rule as `iat`: a non-numeric registered date mints a token
        // every verifier rejects, so refuse it here rather than ship it.
        None if claims.contains_key("exp") => require_numeric_date(&claims["exp"], "exp"),
        None => Err(invalid(
            "requires 'expires_in' (or an explicit 'exp' claim — non-expiring \
             tokens must be deliberate)",
        )),
    }
}

/// The compact JWS of `claims`, signed with the key `material` makes: shared
/// by both engines' paths. The key lives only inside this call.
fn mint(
    algorithm: jsonwebtoken::Algorithm,
    material: &str,
    key_encoding: Option<&str>,
    kid: Option<String>,
    claims: Map<String, Value>,
) -> Result<String, HandlerError> {
    let key =
        crate::jwt::encoding_key(algorithm, material, key_encoding).map_err(|e| invalid(&e))?;
    crate::jwt::sign(algorithm, &key, kid, &Value::Object(claims))
        .map_err(|e| HandlerError::new(ErrorClass::Backend, format!("{NAME}: {e}")))
}

/// A registered date claim must be a JSON number — NumericDate, RFC 7519 §2.
///
/// A string or object here mints a token that every verifier rejects later,
/// which surfaces as an opaque failure at the far end of an integration. Refuse
/// it at sign time instead, where the message can name the claim.
fn require_numeric_date(value: &Value, claim: &'static str) -> Result<(), HandlerError> {
    if value.is_number() {
        return Ok(());
    }
    Err(invalid(&format!(
        "claims.{claim} must be a number of seconds since the Unix epoch \
         (NumericDate, RFC 7519 §2), got {}",
        crate::engine::utils::json_kind(value)
    )))
}

// -- dataflow-rs 4 --

/// I185's Defect for a literal root: the token is a string, which no
/// context root takes (break 21), so it is refused on every call.
const ROOT_OUTPUT: &str = "`jwt_sign` writes a token string, and a context root takes only \
    an object (break 21): set `output` to a path below it, such as `data.token`";

/// The same Defect for a computed `output`, which `jwt_sign` writes at
/// `data` as 1.12 did ([`kit::written_output`]).
const COMPUTED_OUTPUT: &str = "`jwt_sign` reads `output` as written, so a computed one is \
    written at `data`, as 1.12 wrote it, and a context root takes only an object (break 21): \
    set `output` to a literal path below a root, such as `data.token`";

/// `jwt_sign`'s input on dataflow-rs 4, in its field table's order (I179),
/// each field in the position 1.12 executed it in (I183). The algorithm and
/// the key encoding are literals, read as written; the key is key
/// material; `claims` and `audience` are documents of `var` holes, as 1.12
/// folded them (`audience`'s table marks it a template, which 1.12 compiled
/// and never evaluated); `expires_in`, `issuer`, `not_before` and `kid` are
/// expressions, read by `kit::read`'s twins of 1.12's readers. `output` is
/// read as written ([`kit::written_output`]). A task missing `algorithm` or
/// `key` is refused at load (I183), and its `check` is 1.12's
/// authoring-time rules.
#[derive(Debug, FunctionConfig)]
#[config(check = check)]
pub struct JwtSignConfig {
    /// The signing algorithm: HS/RS/PS 256-512, ES256/384 or EdDSA.
    algorithm: String,
    /// The HS secret or the private-key PEM.
    key: KeyMaterial,
    /// How an HS secret becomes bytes: utf8, base64 or hex.
    key_encoding: Option<String>,
    /// The claims, `var` holes filled from the message.
    claims: Option<Interpolated>,
    /// The token's lifetime: seconds or `"<n>s|m|h|d"`, setting `exp`.
    expires_in: Option<Expr<OwnedDataValue>>,
    /// The `iss` claim.
    issuer: Option<Expr<OwnedDataValue>>,
    /// The `aud` claim, `var` holes filled from the message.
    audience: Option<Interpolated>,
    /// The offset from now for the `nbf` claim.
    not_before: Option<Expr<OwnedDataValue>>,
    /// The key id stamped into the token's header.
    kid: Option<Expr<OwnedDataValue>>,
    /// Where the token is written.
    #[output(default = "data")]
    output: Output,
}

/// 1.12's authoring-time rules for `jwt_sign` ([`validate_static_input`], by
/// which create, update, import, validate and `lint` refused a task), on the
/// config at load: a supported algorithm, and an `expires_in` that is a
/// duration, or an explicit `exp` claim, each refused at the field 1.12
/// named, in its words.
fn check(cfg: &JwtSignConfig, cx: &mut CompileCx<'_>) {
    let mut input = Map::new();
    input.insert(
        "algorithm".to_string(),
        Value::String(cfg.algorithm.clone()),
    );
    if let Some(claims) = &cfg.claims {
        input.insert("claims".to_string(), claims.raw().clone());
    }
    if let Some(expires_in) = &cfg.expires_in {
        input.insert("expires_in".to_string(), expires_in.raw().clone());
    }
    super::mongo_common::report_static(validate_static_input(&input), cx);
}

/// `jwt_sign` as a dataflow-rs 4 function: the token `JwtSignHandler`
/// mints, each field read in the same order, the key last.
pub struct JwtSignFunction;

impl Function for JwtSignFunction {
    type Config = JwtSignConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    /// I185: the token is never an object, so an `output` that is a literal
    /// context root, the default `data` included, is a Defect at `output`,
    /// and so is a computed one, which is written at `data`.
    fn prepare(&self, cfg: &mut JwtSignConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        kit::written_root_output_defect(&cfg.output, ROOT_OUTPUT, COMPUTED_OUTPUT)
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &JwtSignConfig) -> dataflow_rs::Result<Outcome> {
        let token = token(ctx, cfg).await?;
        ctx.set(
            kit::written_output(&cfg.output),
            OwnedDataValue::String(token),
        )?;
        Ok(Outcome::Ok)
    }
}

/// An optional string field for this call, by `resolve_optional_str`'s
/// reading.
fn optional_str(
    ctx: &Ctx<'_>,
    field: &Option<Expr<OwnedDataValue>>,
    name: &str,
) -> Result<Option<String>, HandlerError> {
    let value = field.as_ref().map(|expr| ctx.eval(expr)).transpose()?;
    Ok(read::optional_str(value.as_deref(), name, NAME)?.map(Cow::into_owned))
}

/// A duration field for this call, by `resolve_duration_secs`'s reading.
fn duration_secs(
    ctx: &Ctx<'_>,
    field: &Option<Expr<OwnedDataValue>>,
    name: &str,
) -> Result<Option<u64>, HandlerError> {
    let value = field.as_ref().map(|expr| ctx.eval(expr)).transpose()?;
    read::duration_secs(value.as_deref(), name, NAME)
}

/// The token `JwtSignHandler::execute` mints for this call, read in its
/// order: the literal prologue, the key's presence, the kid, the claims and
/// their conveniences, the dates, then the key's material.
async fn token(ctx: &Ctx<'_>, cfg: &JwtSignConfig) -> Result<String, HandlerError> {
    let algorithm = crate::jwt::parse_algorithm(&cfg.algorithm).map_err(|e| invalid(&e))?;
    // Presence only: a `null` key, which 1.12 refused before reading
    // anything the message can change (F58).
    if matches!(&cfg.key, KeyMaterial::Expr(expr) if expr.raw().is_null()) {
        return Err(invalid(
            "requires 'key' ({\"secret\": \"name\"}, a secret reference like \
             env://NAME, an expression, or a literal)",
        ));
    }
    let key_encoding = cfg.key_encoding.as_deref();
    let kid = optional_str(ctx, &cfg.kid, "kid")?;

    let mut claims = match &cfg.claims {
        None => Map::new(),
        Some(doc) => match kit::rendered(ctx, doc)? {
            Value::Object(map) => map,
            _ => return Err(invalid("'claims' must resolve to an object")),
        },
    };

    let now = chrono::Utc::now().timestamp();
    if let Some(iss) = optional_str(ctx, &cfg.issuer, "issuer")? {
        claims.insert("iss".to_string(), Value::String(iss));
    }
    // An authored `null` is no audience, as 1.12 skipped it.
    if let Some(aud) = cfg.audience.as_ref().filter(|doc| !doc.raw().is_null()) {
        claims.insert("aud".to_string(), kit::rendered(ctx, aud)?);
    }
    if let Some(offset) = duration_secs(ctx, &cfg.not_before, "not_before")? {
        claims.insert("nbf".to_string(), Value::from(now + offset as i64));
    }
    stamp_iat(&mut claims, now)?;
    stamp_exp(
        &mut claims,
        now,
        duration_secs(ctx, &cfg.expires_in, "expires_in")?,
    )?;

    let material = cfg.key.material(ctx, "key", NAME).await?;
    mint(algorithm, &material, key_encoding, kid, claims)
}

// -- Authoring-time validation (shared with schema::validate_input) --

pub(super) fn validate_static_input(
    obj: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    let mut errors: Vec<(&'static str, &'static str, String)> = Vec::new();

    // A missing or non-string `algorithm` is the field loop's REQUIRED /
    // TYPE_MISMATCH; only a present string is judged here.
    if let Some(name) = obj.get("algorithm").and_then(Value::as_str)
        && let Err(e) = crate::jwt::parse_algorithm(name)
    {
        errors.push(("algorithm", "INVALID", e));
    }
    if obj.get("expires_in").is_none_or(Value::is_null) {
        let has_exp = obj
            .get("claims")
            .and_then(Value::as_object)
            .is_some_and(|c| c.contains_key("exp"));
        if !has_exp {
            errors.push((
                "expires_in",
                "REQUIRED",
                "jwt_sign requires 'expires_in' (or an explicit 'exp' claim — \
                 non-expiring tokens must be deliberate)"
                    .to_string(),
            ));
        }
    } else if let Some(Value::String(s)) = obj.get("expires_in")
        && let Err(e) = parse_duration_secs(s)
    {
        errors.push(("expires_in", "INVALID", e));
    }
    errors
}

// -- Input schema (F53) --

pub(super) const JWT_SIGN_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "algorithm",
        description: "Signing algorithm: HS/RS/PS 256-512, ES256/384, or EdDSA.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key",
        description: "HS secret or RS/ES/Ed private-key PEM: {\"secret\": \"name\"} \
                      reads the engine's [secrets] store; a string is a literal or a \
                      reference (env://NAME). Never appears in traces or errors.",
        kind: FieldKind::String,
        required: true,
        secret_at: &[""],
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key_encoding",
        description: "How an HS secret becomes bytes: utf8 (default), base64, hex.",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "claims",
        description: "The claims object; values fold {\"var\": ..} nodes (compose \
                      computed claims with a map task first). iat is stamped \
                      automatically.",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "expires_in",
        description: "Token lifetime: integer seconds or \"<n>s|m|h|d\"; sets exp from \
                      now. Required unless claims carries an explicit exp.",
        kind: FieldKind::Any,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "issuer",
        description: "Convenience for the iss claim.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "audience",
        description: "Convenience for the aud claim (string or array; resolvable).",
        kind: FieldKind::Any,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "not_before",
        description: "Offset from now for the nbf claim: integer seconds or \
                      \"<n>s|m|h|d\".",
        kind: FieldKind::Any,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "kid",
        description: "Key id stamped into the token header, for rotation-aware \
                      verifiers.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the compact JWS (string) is stored. Defaults \
                      to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

/// `jwt_sign` on dataflow-rs 4 against its dataflow-rs 3 handler over the
/// same input (the round trips through `jwt_verify` live beside it, in
/// `jwt_verify.rs`): the expectation is computed from the handler, never
/// written out.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::json;

    const HS_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn config_v4(input: Value) -> Result<JwtSignConfig, Vec<dataflow_rs::Issue>> {
        testing::config_with(&kit::test_exprs(), input)
    }

    /// The Rejected issues of a config that does not load, as
    /// `(path, message)`.
    fn refusals(input: Value) -> Vec<(String, String)> {
        config_v4(input.clone())
            .map(|_| ())
            .expect_err(&format!("{input} is refused at load"))
            .into_iter()
            .filter(|i| i.severity == Severity::Rejected)
            .map(|i| (i.path.to_string(), i.message))
            .collect()
    }

    /// One task through each engine over the same input and `data`; the
    /// two must agree, and dataflow-rs 4's answer is returned.
    async fn agreed(input: Value, data: Value) -> Seen {
        let v3 = twin::v3(NAME, Box::new(JwtSignHandler), input.clone(), data.clone()).await;
        let v4 = twin::v4(NAME, JwtSignFunction, input.clone(), data).await;
        assert_eq!(v4, v3, "{input}");
        v4
    }

    /// A token's claims, read off its payload segment, unverified.
    fn claims_of(token: &Value) -> Value {
        use base64::Engine as _;
        let token = token.as_str().expect("a token");
        let payload = token.split('.').nth(1).expect("a payload segment");
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .expect("base64url");
        serde_json::from_slice(&bytes).expect("a claims object")
    }

    /// Declaration order is the field table's (I179).
    #[test]
    fn the_config_declares_the_field_table_in_its_order() {
        let fields: Vec<&str> = JwtSignConfig::fields().iter().map(|f| &*f.name).collect();
        let table: Vec<&str> = JWT_SIGN_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);
    }

    /// Each field compiles in the position 1.12 executed it in (I183): the
    /// algorithm and the key required, the key as text or an expression, a
    /// `null` key a value (refused at call, as 1.12 refused it), the
    /// documents of holes kept as authored.
    #[test]
    fn the_fields_compile_in_1_12s_positions() {
        let cfg = config_v4(json!({
            "algorithm": "HS256", "key": {"secret": "k"}, "key_encoding": "hex",
            "claims": {"sub": {"var": "data.user"}, "$role": "admin"},
            "audience": ["a", {"var": "data.aud"}], "expires_in": "1h",
            "issuer": {"var": "data.iss"}, "kid": "k-1", "output": "data.token"
        }))
        .expect("compiles");
        assert!(matches!(&cfg.key, KeyMaterial::Expr(_)));
        assert_eq!(cfg.key_encoding.as_deref(), Some("hex"));
        assert_eq!(
            cfg.claims.as_ref().map(Interpolated::raw),
            Some(&json!({"sub": {"var": "data.user"}, "$role": "admin"}))
        );
        assert_eq!(kit::written_output(&cfg.output).to_string(), "data.token");

        let cfg = config_v4(json!({"algorithm": "HS256", "key": null, "claims": null,
                                   "audience": null, "expires_in": 60}))
        .expect("compiles");
        assert!(matches!(&cfg.key, KeyMaterial::Expr(e) if e.raw().is_null()));
        assert!(cfg.claims.as_ref().is_some_and(|c| c.raw().is_null()));
        assert!(cfg.audience.as_ref().is_some_and(|a| a.raw().is_null()));

        // The two required fields, missing, and a non-string literal, which
        // 1.12's field loop refused at authoring, are refused at load.
        let full = json!({"algorithm": "HS256", "key": HS_SECRET, "expires_in": 60});
        for field in ["algorithm", "key"] {
            let mut input = full.clone();
            input.as_object_mut().expect("an object").remove(field);
            assert_eq!(refusals(input)[0].0, field);
        }
        for field in ["algorithm", "key_encoding"] {
            let mut input = full.clone();
            input[field] = json!(7);
            assert_eq!(refusals(input)[0].0, field);
        }
        // `jwt_sign` was `deny_unknown` in 1.12, so an unknown key stays
        // refused.
        let mut unknown = full;
        unknown["subject"] = json!("u1");
        let issues = config_v4(unknown).map(|_| ()).expect_err("an unknown key");
        assert_eq!(issues[0].path.to_string(), "subject");
        assert_eq!(issues[0].code, IssueCode::UnknownKey);
    }

    /// The check is 1.12's authoring-time rules: each refusal at the field
    /// and in the words `validate_static_input` gives, computed from it; an
    /// input it accepts loads.
    #[test]
    fn the_check_is_validate_static_input() {
        let authoring = |input: &Value| -> Vec<(String, String)> {
            validate_static_input(input.as_object().expect("an object"))
                .into_iter()
                .map(|(field, _code, message)| (field.to_string(), message))
                .collect()
        };
        for input in [
            json!({"algorithm": "HS999", "key": "k", "expires_in": 300}),
            json!({"algorithm": "none", "key": "k", "expires_in": 300}),
            json!({"algorithm": "HS256", "key": "k"}),
            json!({"algorithm": "HS256", "key": "k", "expires_in": null}),
            json!({"algorithm": "HS256", "key": "k", "expires_in": "5x"}),
            json!({"algorithm": "HS256", "key": "k", "claims": {"var": "data.claims"}}),
        ] {
            let expected = authoring(&input);
            assert!(!expected.is_empty(), "{input} is a refusal");
            assert_eq!(refusals(input.clone()), expected, "{input}");
        }
        for input in [
            json!({"algorithm": "HS256", "key": "k", "claims": {"exp": 1}}),
            json!({"algorithm": "HS256", "key": "k", "expires_in": "24h"}),
            json!({"algorithm": "HS256", "key": "k", "expires_in": {"var": "data.ttl"}}),
        ] {
            assert!(authoring(&input).is_empty(), "{input}");
            assert!(config_v4(input.clone()).is_ok(), "{input}");
        }
    }

    /// I185: the token is never an object, so a literal root `output` (the
    /// default `data` included) is a Defect at `output`, and so is a
    /// computed one, which `jwt_sign` writes at `data` as 1.12 did. A path
    /// below a root is not.
    #[tokio::test]
    async fn a_root_or_computed_output_is_i185s_defect() {
        for (output, defect) in [
            (None, Some(ROOT_OUTPUT)),
            (Some(json!("data")), Some(ROOT_OUTPUT)),
            (Some(json!("temp_data")), Some(ROOT_OUTPUT)),
            (Some(Value::Null), Some(ROOT_OUTPUT)),
            (Some(json!({"var": "data.where"})), Some(COMPUTED_OUTPUT)),
            (Some(json!("data.token")), None),
            (Some(json!("temp_data.jws")), None),
        ] {
            let mut input = json!({"algorithm": "HS256", "key": HS_SECRET, "expires_in": 60});
            if let Some(output) = &output {
                input["output"] = output.clone();
            }
            let called = testing::call(JwtSignFunction, input)
                .exprs(kit::test_exprs())
                .name(NAME)
                .await;
            let defects: Vec<_> = called
                .issues
                .iter()
                .filter(|i| i.severity == Severity::Defect)
                .map(|i| (i.path.to_string(), i.code, i.message.as_str()))
                .collect();
            match defect {
                Some(message) => {
                    assert_eq!(
                        defects,
                        [("output".to_string(), IssueCode::InputParse, message)],
                        "{output:?}"
                    );
                    let error = called.outcome.expect_err("no root takes a token");
                    assert_eq!(error.code(), "CONFIG_ERROR", "{output:?}");
                }
                None => {
                    assert!(defects.is_empty(), "{output:?}: {defects:?}");
                    assert_eq!(called.outcome.expect("signed"), Outcome::Ok);
                }
            }
        }
    }

    /// With every clock-dependent claim pinned, both engines mint the same
    /// bytes: the claims folded from the message in the same order, the
    /// conveniences added after them, the kid in the header.
    #[tokio::test]
    async fn a_fully_pinned_token_is_the_same_bytes_on_both_engines() {
        let out = agreed(
            json!({"algorithm": "HS256", "key": HS_SECRET, "key_encoding": "utf8",
                   "claims": {"sub": {"var": "data.user"}, "iat": 1_700_000_000i64,
                              "exp": 1_900_000_000i64, "scope": ["read", {"var": "data.scope"}],
                              "$role": "admin"},
                   "issuer": {"var": "data.iss"}, "audience": ["app", {"var": "data.aud"}],
                   "kid": {"cat": ["k-", {"var": "data.n"}]}, "output": "data.token"}),
            json!({"user": "u1", "scope": "write", "iss": "orion", "aud": "partner", "n": 2}),
        )
        .await
        .expect("signed");
        let claims = claims_of(&out["token"]);
        assert_eq!(
            serde_json::to_string(&claims).expect("json"),
            r#"{"sub":"u1","iat":1700000000,"exp":1900000000,"scope":["read","write"],"$role":"admin","iss":"orion","aud":["app","partner"]}"#
        );
    }

    /// `nbf`, `iat` and `exp` are stamped from one clock read, as 1.12
    /// stamped them.
    #[tokio::test]
    async fn the_dates_are_stamped_from_one_clock_read() {
        let input = json!({"algorithm": "HS256", "key": HS_SECRET, "claims": {},
                           "not_before": "1h", "expires_in": {"var": "data.ttl"},
                           "output": "data.token"});
        let data = json!({"ttl": "2h"});
        let v3 = twin::v3(NAME, Box::new(JwtSignHandler), input.clone(), data.clone())
            .await
            .expect("signed on v3");
        let v4 = twin::v4(NAME, JwtSignFunction, input, data)
            .await
            .expect("signed on v4");
        for out in [v3, v4] {
            let claims = claims_of(&out["token"]);
            let iat = claims["iat"].as_i64().expect("iat");
            assert_eq!(claims["nbf"].as_i64(), Some(iat + 3_600), "{claims}");
            assert_eq!(claims["exp"].as_i64(), Some(iat + 7_200), "{claims}");
        }
    }

    /// What reaches a call and is refused there keeps 1.12's code and text,
    /// in 1.12's order of refusal.
    #[tokio::test]
    async fn call_time_refusals_keep_1_12s_text() {
        let data = json!({"n": 5, "bad": "5x", "obj": {"a": 1}});
        let base = json!({"algorithm": "HS256", "key": HS_SECRET, "expires_in": 300,
                          "output": "data.token"});
        let with = |extra: Value| {
            let mut input = base.clone();
            for (key, value) in extra.as_object().expect("an object") {
                input[key] = value.clone();
            }
            input
        };
        for input in [
            with(json!({"key": null})),
            with(json!({"key": {"var": "data.n"}})),
            with(json!({"key": "env://ORION_JWT_SIGN_SURELY_UNSET_3Q"})),
            with(json!({"key": "short"})),
            with(json!({"algorithm": "RS256", "key": "not a pem"})),
            with(json!({"key_encoding": "base64", "key": "%%%"})),
            with(json!({"claims": {"var": "data.n"}})),
            with(json!({"claims": null})),
            with(json!({"claims": [1]})),
            with(json!({"kid": {"var": "data.n"}})),
            // The kid is judged before the claims, as 1.12 judged it.
            with(json!({"kid": {"var": "data.n"}, "claims": {"var": "data.n"}})),
            with(json!({"issuer": {"var": "data.obj"}})),
            with(json!({"expires_in": {"var": "data.bad"}})),
            with(json!({"expires_in": 0})),
            with(json!({"expires_in": 315_360_001u64})),
            with(json!({"expires_in": -5})),
            with(json!({"expires_in": 1.5})),
            with(json!({"expires_in": {"var": "data.obj"}})),
            with(json!({"not_before": "abc"})),
            with(json!({"claims": {"iat": "2026-08-20T00:00:00Z"}})),
            json!({"algorithm": "HS256", "key": HS_SECRET,
                   "claims": {"exp": "2026-08-20T00:00:00Z"}, "output": "data.token"}),
            json!({"algorithm": "HS256", "key": HS_SECRET, "claims": {"sub": "u"},
                   "expires_in": {"var": "data.missing"}, "output": "data.token"}),
        ] {
            let failure = agreed(input.clone(), data.clone())
                .await
                .expect_err(&format!("{input} is refused"));
            assert!(
                failure.code == "VALIDATION_ERROR" || failure.code == "LOGIC_ERROR",
                "{input}: {failure:?}"
            );
        }
    }

    /// A `{"secret": …}` node in `audience`. 1.12's authoring check let it
    /// through, taking the template-marked field to resolve the node, and
    /// the call folded `var` holes only, so the token carried the node
    /// itself as its `aud`. dataflow-rs 4 refuses it at load as it refuses
    /// a secret in any document (`SECRET_IN_DOCUMENT`). A difference the
    /// plan does not name, pinned for its ruling.
    #[tokio::test]
    async fn a_secret_node_in_audience_is_refused_at_load_where_1_12_minted_it_as_data() {
        let input = json!({"algorithm": "HS256", "key": HS_SECRET, "expires_in": 60,
                           "audience": {"secret": "aud"}, "output": "data.token"});
        let mut values = Map::new();
        values.insert("aud".into(), json!("partner-app"));
        let secrets = crate::engine::ResolvedSecrets::from_values(values);
        let workflow: dataflow_rs_v3::Workflow = serde_json::from_value(json!({
            "id": "w", "name": "w", "condition": true,
            "tasks": [{"id": "t", "name": "t", "function": {"name": NAME, "input": input}}]
        }))
        .expect("a workflow");
        let engine = crate::engine::operators::with_orion_engine_defaults(
            dataflow_rs_v3::Engine::builder(),
            &secrets,
        )
        .register(NAME, JwtSignHandler)
        .with_workflow(workflow)
        .build()
        .expect("1.12 built it");
        let mut message = dataflow_rs_v3::Message::from_value(&json!({}));
        engine
            .process_message(&mut message)
            .await
            .expect("1.12 minted it");
        let data: Value = message.data().into();
        assert_eq!(claims_of(&data["token"])["aud"], json!({"secret": "aud"}));

        let issues = config_v4(input).map(|_| ()).expect_err("refused at load");
        let issues: Vec<(String, IssueCode)> = issues
            .iter()
            .map(|i| (i.path.to_string(), i.code))
            .collect();
        assert_eq!(
            issues,
            [("audience".to_string(), IssueCode::SecretInDocument)]
        );
    }
}
