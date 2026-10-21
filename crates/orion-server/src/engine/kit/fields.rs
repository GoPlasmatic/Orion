//! Orion's own config field types (I183): key material, a list of it, one
//! value of it read as written, an expression a function evaluates over a
//! root of its own, and a map that keeps its members in the order they were
//! written.
//!
//! Each implements [`ConfigField`], so a derived config holds one as it holds
//! an `Expr`: compiled once at build with every issue at its path, walked by
//! the engine's secret checks, and resolved for [`Ctx::input`], which writes
//! a secret-reading field `null`.

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{
    CompileCx, ConfigField, FieldKind, Interpolated, Resolver, ValueKind, Visit, secret_refs,
};
use dataflow_rs::program::IssueCode;
use dataflow_rs::{Ctx, Expr};
use serde_json::Value;

use crate::connector::secrets::resolve_secret_string;
use crate::engine::functions::secret_ref::secret_name;
use crate::engine::{ErrorClass, HandlerError};

/// A refusal of key material: the caller's to fix, as dataflow-rs 3's
/// `Validation` was.
fn refused(message: impl Into<String>) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, message.into())
}

/// `v` as the JSON value dataflow-rs 3 handed its reader for it.
fn json(v: &OwnedDataValue) -> Value {
    Value::from(v)
}

/// A key-material field: `crypto.key`, `jwt_sign.key`, `jwt_verify`'s
/// `keys[].key` (`secret_ref::key_material_field`'s rules).
///
/// The two spellings answer different questions and are told apart by what
/// the author wrote, not by what it produces:
///
/// * A **string** is [`Self::Text`]: a literal, or an `env://` / `vault://`
///   reference resolved at call through
///   [`resolve_secret_string`].
/// * **Anything else** is [`Self::Expr`], JSONLogic whose result *is* the
///   material: `{"secret": "name"}` reads the engine's store through the
///   reserved operator, and composes. Its result is never post-processed, so
///   a secret whose value begins with `env://` is not resolved a second time.
pub enum KeyMaterial {
    /// A literal, or a secret reference resolved at call.
    Text(String),
    /// An expression; the engine's secret checks see it, and
    /// [`Ctx::input`] writes it `null` when it reads a secret.
    Expr(Box<Expr<OwnedDataValue>>),
}

impl std::fmt::Debug for KeyMaterial {
    /// The spelling, never the material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(_) => f.write_str("KeyMaterial::Text(..)"),
            Self::Expr(expr) => f
                .debug_tuple("KeyMaterial::Expr")
                .field(expr.raw())
                .finish(),
        }
    }
}

impl ConfigField for KeyMaterial {
    /// `null` is an expression, refused at call as dataflow-rs 3 refused it
    /// ("must resolve to a string, got null"), not an unset field.
    const NULL_IS_VALUE: bool = true;

    fn compile(raw: &Value, cx: &mut CompileCx<'_>) -> Option<Self> {
        match raw {
            Value::String(text) => Some(Self::Text(text.clone())),
            _ => Expr::<OwnedDataValue>::compile(raw, cx)
                .map(Box::new)
                .map(Self::Expr),
        }
    }

    fn visit(&self, v: &mut Visit<'_>) {
        if let Self::Expr(expr) = self {
            v.expr(expr.raw());
        }
    }

    /// The text as written, never resolved, or the expression's value.
    fn resolve(&self, r: &mut dyn Resolver) -> dataflow_rs::Result<Option<OwnedDataValue>> {
        match self {
            Self::Text(text) => Ok(Some(OwnedDataValue::String(text.clone()))),
            Self::Expr(expr) => r.eval(expr.as_ref()).map(Some),
        }
    }

    fn kind() -> FieldKind {
        FieldKind::Expr(ValueKind::String)
    }
}

impl KeyMaterial {
    /// The material for this call, as `key_material_field` resolves it. The
    /// errors name the field as `handler.field`, and for a store miss the
    /// key — never a value.
    ///
    /// # Errors
    ///
    /// A [`ErrorClass::CallerInput`] error when a reference does not resolve
    /// or the expression's result is not a string; the evaluation's own
    /// error, kept whole, when the expression fails.
    pub async fn material(
        &self,
        ctx: &Ctx<'_>,
        field: &str,
        handler: &str,
    ) -> Result<String, HandlerError> {
        let label = format!("{handler}.{field}");
        match self {
            Self::Text(text) => resolve_secret_string(text, &label).await.map_err(refused),
            Self::Expr(expr) => {
                let value = ctx.eval(expr)?;
                if let OwnedDataValue::String(material) = &*value {
                    return Ok(material.clone());
                }
                match json(&value) {
                    Value::String(material) => Ok(material),
                    other => Err(refused(format!(
                        "'{label}' must resolve to a string, got {other}"
                    ))),
                }
            }
        }
    }
}

/// [`KeyMaterial::material`] for an optional field, refusing one the task
/// leaves unset with `key_material_field`'s message.
///
/// # Errors
///
/// As [`KeyMaterial::material`], and a [`ErrorClass::CallerInput`] error
/// when `key` is `None`.
pub async fn key_material(
    key: Option<&KeyMaterial>,
    ctx: &Ctx<'_>,
    field: &str,
    handler: &str,
) -> Result<String, HandlerError> {
    match key {
        Some(key) => key.material(ctx, field, handler).await,
        None => Err(refused(format!(
            "{handler} requires '{field}' ({{\"secret\": \"name\"}}, a secret reference \
             like env://NAME, an expression, or a literal)"
        ))),
    }
}

/// `jwt_verify`'s `issuer` and `audience`: a string, or an array of them,
/// each resolved like key material (`jwt_verify::string_or_vec`'s rules).
///
/// The field and each element are folded as `var` holes, not evaluated: a
/// `{"var": …}` is read from the message, and any other operator stays data.
/// A `{"secret": …}` node, as the field or as an element, is one item read
/// from the store; a string is a literal or an `env://` reference; a `null`
/// element is dropped. Anything else is refused at call, as it was. What a
/// hole yields is judged by the same rules at call: a string resolves as a
/// reference, a `{"secret": …}` object reads the store.
pub struct KeyMaterialList {
    shape: ListShape,
}

enum ListShape {
    /// The field is one string or one `{"secret": …}` node.
    One(Item),
    /// The field is an array, one item per element.
    Items(Vec<Item>),
    /// Anything else: folded at call, then read as a string, a secret node
    /// or an array.
    Folded(Interpolated),
}

enum Item {
    /// A literal, or a secret reference resolved at call.
    Text(String),
    /// A `{"secret": …}` node, boxed: its compiled expression is large and
    /// rare beside the strings.
    Secret(Box<SecretItem>),
    /// An element folded at call: a hole, or a value refused once folded.
    Folded(Interpolated),
    /// A literal `null` element, dropped.
    Null,
}

/// A `{"secret": …}` node of a [`KeyMaterialList`].
struct SecretItem {
    /// The node as authored, which `visit` reports, so the engine's secret
    /// checks see it and [`Ctx::input`] writes it `null`.
    raw: Value,
    /// The node compiled, for a resolve that reads secrets; `None` when it
    /// does not compile, which the field's own reading never asks of it.
    expr: Option<Expr<OwnedDataValue>>,
    /// The secret's name.
    name: SecretName,
}

enum SecretName {
    /// Written as a string, or a one-element array of one.
    Literal(String),
    /// Anything else, folded at call like the rest of the field.
    Folded(Interpolated),
}

/// Whether `raw` is the reserved operator's shape: one key, `secret`.
fn is_secret_node(raw: &Value) -> bool {
    raw.as_object()
        .is_some_and(|o| o.len() == 1 && o.contains_key("secret"))
}

impl Item {
    fn compile(raw: &Value, cx: &mut CompileCx<'_>) -> Option<Self> {
        match raw {
            Value::String(text) => Some(Self::Text(text.clone())),
            Value::Null => Some(Self::Null),
            _ if is_secret_node(raw) => {
                SecretItem::compile(raw, cx).map(|s| Self::Secret(Box::new(s)))
            }
            _ => Interpolated::compile(raw, cx).map(Self::Folded),
        }
    }

    fn visit(&self, v: &mut Visit<'_>) {
        match self {
            Self::Secret(secret) => v.expr(&secret.raw),
            Self::Folded(doc) => v.interpolated(doc),
            Self::Text(_) | Self::Null => {}
        }
    }

    fn resolve(&self, r: &mut dyn Resolver) -> dataflow_rs::Result<OwnedDataValue> {
        match self {
            Self::Text(text) => Ok(OwnedDataValue::String(text.clone())),
            Self::Secret(secret) => match &secret.expr {
                Some(expr) => r.eval(expr),
                None => Ok(OwnedDataValue::Null),
            },
            Self::Folded(doc) => r.render(doc),
            Self::Null => Ok(OwnedDataValue::Null),
        }
    }

    /// The item's material, or `None` for one that is dropped. `whole` says
    /// the item is the whole field, whose secret node with a name that folds
    /// to nothing usable is refused as the field is.
    async fn material(
        &self,
        ctx: &Ctx<'_>,
        field: &str,
        handler: &str,
        whole: bool,
    ) -> Result<Option<String>, HandlerError> {
        match self {
            Self::Text(text) => resolve_secret_string(text, field)
                .await
                .map(Some)
                .map_err(refused),
            Self::Secret(secret) => match &secret.name {
                SecretName::Literal(name) => store(ctx, name, field).map(Some),
                SecretName::Folded(doc) => {
                    let arg = json(&ctx.render(doc).to_owned());
                    match name_in(&arg) {
                        Some(name) => store(ctx, name, field).map(Some),
                        None if whole => Err(not_a_list(field, handler)),
                        None => Err(not_material(field)),
                    }
                }
            },
            Self::Folded(doc) => {
                let value = json(&ctx.render(doc).to_owned());
                if value.is_null() {
                    return Ok(None);
                }
                folded_material(&value, ctx, field).await.map(Some)
            }
            Self::Null => Ok(None),
        }
    }
}

impl SecretItem {
    fn compile(raw: &Value, cx: &mut CompileCx<'_>) -> Option<Self> {
        let name = match secret_name(raw) {
            Some(name) => SecretName::Literal(name.to_string()),
            None => {
                let arg = raw.get("secret").unwrap_or(&Value::Null);
                SecretName::Folded(cx.at("secret", |cx| Interpolated::compile(arg, cx))?)
            }
        };
        Some(Self {
            raw: raw.clone(),
            expr: cx.exprs().compile(raw).ok(),
            name,
        })
    }
}

/// The name a folded `secret` argument gives: a string, or a one-element
/// array of one, as `secret_name` reads a node's.
fn name_in(arg: &Value) -> Option<&str> {
    match arg {
        Value::String(name) => Some(name),
        Value::Array(items) if items.len() == 1 => items[0].as_str(),
        _ => None,
    }
}

/// A secret from the engine's store, as `resolve_key_material` reads one.
fn store(ctx: &Ctx<'_>, name: &str, field: &str) -> Result<String, HandlerError> {
    let Some(secret) = ctx.secret(name) else {
        return Err(refused(format!(
            "'{field}' names secret '{name}', which this instance does not declare \
             — add it to the [secrets] section of the config file"
        )));
    };
    secret
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| refused(format!("'{field}': secret '{name}' is not a string")))
}

/// What a folded value yields as key material, as `resolve_key_material`
/// reads a value it is handed.
async fn folded_material(
    value: &Value,
    ctx: &Ctx<'_>,
    field: &str,
) -> Result<String, HandlerError> {
    if let Some(name) = secret_name(value) {
        return store(ctx, name, field);
    }
    match value.as_str() {
        Some(text) => resolve_secret_string(text, field).await.map_err(refused),
        None => Err(not_material(field)),
    }
}

/// An element that is not key material.
fn not_material(field: &str) -> HandlerError {
    refused(format!(
        "'{field}' must be a string (a literal or an env:// reference) or \
         {{\"secret\": \"name\"}}"
    ))
}

/// A field that is not a string, a secret node or an array.
fn not_a_list(field: &str, handler: &str) -> HandlerError {
    refused(format!(
        "{handler}: '{field}' must be a string or an array of strings"
    ))
}

impl std::fmt::Debug for KeyMaterialList {
    /// The shape, never the material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shape = match &self.shape {
            ListShape::One(_) => "one",
            ListShape::Items(_) => "items",
            ListShape::Folded(_) => "folded",
        };
        f.debug_struct("KeyMaterialList")
            .field("shape", &shape)
            .finish_non_exhaustive()
    }
}

impl ConfigField for KeyMaterialList {
    fn compile(raw: &Value, cx: &mut CompileCx<'_>) -> Option<Self> {
        let shape = match raw {
            Value::String(text) => ListShape::One(Item::Text(text.clone())),
            Value::Array(items) => {
                // Every element is compiled, so every issue is reported.
                let compiled: Vec<Option<Item>> = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| cx.at(i, |cx| Item::compile(item, cx)))
                    .collect();
                ListShape::Items(compiled.into_iter().collect::<Option<Vec<_>>>()?)
            }
            _ if is_secret_node(raw) => {
                ListShape::One(Item::Secret(Box::new(SecretItem::compile(raw, cx)?)))
            }
            _ => ListShape::Folded(Interpolated::compile(raw, cx)?),
        };
        Some(Self { shape })
    }

    fn visit(&self, v: &mut Visit<'_>) {
        match &self.shape {
            ListShape::One(item) => item.visit(v),
            ListShape::Items(items) => {
                for (i, item) in items.iter().enumerate() {
                    v.at(i, |v| item.visit(v));
                }
            }
            ListShape::Folded(doc) => v.interpolated(doc),
        }
    }

    /// The field as written, its holes filled: an element at its index, a
    /// secret node as its value, which [`Ctx::input`] writes `null`.
    fn resolve(&self, r: &mut dyn Resolver) -> dataflow_rs::Result<Option<OwnedDataValue>> {
        match &self.shape {
            ListShape::One(item) => item.resolve(r).map(Some),
            ListShape::Items(items) => items
                .iter()
                .map(|item| item.resolve(r))
                .collect::<dataflow_rs::Result<Vec<_>>>()
                .map(|items| Some(OwnedDataValue::Array(items))),
            ListShape::Folded(doc) => r.render(doc).map(Some),
        }
    }

    fn kind() -> FieldKind {
        FieldKind::Expr(ValueKind::Any)
    }
}

impl KeyMaterialList {
    /// Every accepted value for this call, in order, as `string_or_vec`
    /// reads them. `handler` names the function in the refusal of a field
    /// that is not a list; an element's refusal names `field` alone, as it
    /// did.
    ///
    /// # Errors
    ///
    /// A [`ErrorClass::CallerInput`] error for a field or an element that is
    /// not key material, or a reference that does not resolve.
    pub async fn materials(
        &self,
        ctx: &Ctx<'_>,
        field: &str,
        handler: &str,
    ) -> Result<Vec<String>, HandlerError> {
        match &self.shape {
            ListShape::One(item) => Ok(item
                .material(ctx, field, handler, true)
                .await?
                .into_iter()
                .collect()),
            ListShape::Items(items) => {
                let mut materials = Vec::with_capacity(items.len());
                for item in items {
                    if let Some(material) = item.material(ctx, field, handler, false).await? {
                        materials.push(material);
                    }
                }
                Ok(materials)
            }
            ListShape::Folded(doc) => {
                let folded = json(&ctx.render(doc).to_owned());
                // A `{"secret": …}` object is one item, not a map of one.
                let items = match folded {
                    value if secret_name(&value).is_some() => vec![value],
                    value @ Value::String(_) => vec![value],
                    Value::Array(items) => items,
                    _ => return Err(not_a_list(field, handler)),
                };
                let mut materials = Vec::with_capacity(items.len());
                for item in items.iter().filter(|item| !item.is_null()) {
                    materials.push(folded_material(item, ctx, field).await?);
                }
                Ok(materials)
            }
        }
    }
}

/// [`KeyMaterialList::materials`] for an optional field: unset is no values.
///
/// # Errors
///
/// As [`KeyMaterialList::materials`].
pub async fn key_material_list(
    list: Option<&KeyMaterialList>,
    ctx: &Ctx<'_>,
    field: &str,
    handler: &str,
) -> Result<Vec<String>, HandlerError> {
    match list {
        Some(list) => list.materials(ctx, field, handler).await,
        None => Ok(Vec::new()),
    }
}

/// One key-material value read as written, with no expression around it:
/// `jwt_verify`'s `keys[].key` (`secret_ref::key_material`'s rules, I183).
///
/// A string is [`Self::Text`], a literal or an `env://` / `vault://`
/// reference resolved at call. A `{"secret": "name"}` node, or
/// `{"secret": ["name"]}`, is [`Self::Secret`], read from the engine's
/// store, a miss and a non-string refused in 1.12's words. Anything else,
/// `null` and an expression included, is [`Self::Other`], refused at call as
/// 1.12 refused it: the value is never evaluated, so a `{"var": …}` cannot
/// hand the call key material from the message, where [`KeyMaterial`]
/// would evaluate it.
pub enum KeyRef {
    /// A literal, or a secret reference resolved at call.
    Text(String),
    /// A `{"secret": …}` node with a literal name, boxed: its compiled
    /// expression is large and rare beside the strings.
    Secret(Box<SecretRef>),
    /// Anything else, as authored, refused at call.
    Other(Value),
}

/// A [`KeyRef::Secret`]: the node as authored, which `visit` reports, so the
/// engine's secret checks see it and [`Ctx::input`] writes it `null`; the
/// node compiled, for a resolve that reads secrets; and the name the store
/// is read by.
pub struct SecretRef {
    raw: Value,
    expr: Option<Expr<OwnedDataValue>>,
    name: String,
}

impl std::fmt::Debug for KeyRef {
    /// The spelling, never the material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(_) => f.write_str("KeyRef::Text(..)"),
            Self::Secret(secret) => f.debug_tuple("KeyRef::Secret").field(&secret.raw).finish(),
            Self::Other(raw) => f.debug_tuple("KeyRef::Other").field(raw).finish(),
        }
    }
}

impl ConfigField for KeyRef {
    /// `null` is a value, refused at call ("is required") as 1.12 refused
    /// it, not an unset field.
    const NULL_IS_VALUE: bool = true;

    fn compile(raw: &Value, cx: &mut CompileCx<'_>) -> Option<Self> {
        Some(match raw {
            Value::String(text) => Self::Text(text.clone()),
            _ => match secret_name(raw) {
                Some(name) => Self::Secret(Box::new(SecretRef {
                    raw: raw.clone(),
                    expr: cx.exprs().compile(raw).ok(),
                    name: name.to_string(),
                })),
                None => Self::Other(raw.clone()),
            },
        })
    }

    fn visit(&self, v: &mut Visit<'_>) {
        if let Self::Secret(secret) = self {
            v.expr(&secret.raw);
        }
    }

    /// The text as written, never resolved; a secret node's value, which
    /// [`Ctx::input`] writes `null`; anything else as authored.
    fn resolve(&self, r: &mut dyn Resolver) -> dataflow_rs::Result<Option<OwnedDataValue>> {
        Ok(Some(match self {
            Self::Text(text) => OwnedDataValue::String(text.clone()),
            Self::Secret(secret) => match &secret.expr {
                Some(expr) => r.eval(expr)?,
                None => OwnedDataValue::Null,
            },
            Self::Other(raw) => OwnedDataValue::from(raw),
        }))
    }

    /// A literal to the engine: a string, or a `{"secret": …}` node.
    fn kind() -> FieldKind {
        FieldKind::Literal(ValueKind::String)
    }
}

impl KeyRef {
    /// Whether the value is `null`, which its caller refuses in its own
    /// words, as 1.12's did (`'keys[0].key' is required`).
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Other(Value::Null))
    }

    /// The material for this call, as `secret_ref::key_material` resolves
    /// it, its errors naming `field`, and for a store miss the key, never a
    /// value.
    ///
    /// # Errors
    ///
    /// A [`ErrorClass::CallerInput`] error when a reference does not
    /// resolve, the store's value is not a string, or the value is neither a
    /// string nor a `{"secret": …}` node.
    pub async fn material(&self, ctx: &Ctx<'_>, field: &str) -> Result<String, HandlerError> {
        match self {
            Self::Text(text) => resolve_secret_string(text, field).await.map_err(refused),
            Self::Secret(secret) => store(ctx, &secret.name, field),
            Self::Other(_) => Err(not_material(field)),
        }
    }
}

/// An expression the function evaluates over a root of its own, never the
/// message: `model_infer.select`, whose `var` reads address the model's
/// outputs by name.
///
/// Compiled on the engine's expressions, so it means what any expression
/// means; walked by nothing, since its reads are not the message's; resolved
/// to its raw JSON. Its result is written, so a secret read is refused.
pub struct DetachedExpr(Expr<OwnedDataValue>);

impl DetachedExpr {
    /// The compiled expression, for `ctx.exprs().eval(expr, &root)`.
    pub fn expr(&self) -> &Expr<OwnedDataValue> {
        &self.0
    }

    /// The expression as authored.
    pub fn raw(&self) -> &Value {
        self.0.raw()
    }
}

impl std::fmt::Debug for DetachedExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DetachedExpr").field(self.raw()).finish()
    }
}

impl ConfigField for DetachedExpr {
    fn compile(raw: &Value, cx: &mut CompileCx<'_>) -> Option<Self> {
        if secret_refs(raw).next().is_some() {
            cx.issue(
                IssueCode::SecretInMessageWrite,
                "reads a secret, and this expression's result is written to the message; \
                 compute a derived value in a host function instead",
            );
            return None;
        }
        Expr::<OwnedDataValue>::compile(raw, cx).map(Self)
    }

    // `visit` reports nothing: the engine's checks read an expression over
    // the message, and this one reads the function's own root.

    fn resolve(&self, _r: &mut dyn Resolver) -> dataflow_rs::Result<Option<OwnedDataValue>> {
        Ok(Some(OwnedDataValue::from(self.raw())))
    }

    /// A literal to the engine, which never evaluates it: the function does.
    fn kind() -> FieldKind {
        FieldKind::Literal(ValueKind::Any)
    }
}

/// An object field whose members are kept in the order the task wrote them,
/// each value a `T`: `send_email.headers`, whose members 1.12 judged and
/// rendered in that order (I183), where a `BTreeMap` sorts them. The input
/// object's order is the authored one, since Orion's graph builds
/// `serde_json` with `preserve_order`.
///
/// Compiled, walked and resolved as a `BTreeMap<String, T>` is, each member
/// at its key; only the order differs. `null` is not a value, so an
/// `Option<Members<T>>` written as `null` is unset.
#[derive(Debug)]
pub struct Members<T>(Vec<(String, T)>);

impl<T> Members<T> {
    /// The members, in the order written.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &T)> + Clone {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }

    /// How many members there are.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T: ConfigField> ConfigField for Members<T> {
    fn compile(raw: &Value, cx: &mut CompileCx<'_>) -> Option<Self> {
        let Some(entries) = raw.as_object() else {
            cx.issue(IssueCode::InputParse, "expected an object");
            return None;
        };
        // Every member is compiled, so every issue is reported.
        let compiled: Vec<Option<(String, T)>> = entries
            .iter()
            .map(|(key, value)| {
                cx.at(key.as_str(), |cx| T::compile(value, cx))
                    .map(|value| (key.clone(), value))
            })
            .collect();
        compiled.into_iter().collect::<Option<_>>().map(Self)
    }

    fn visit(&self, v: &mut Visit<'_>) {
        for (key, value) in &self.0 {
            v.at(key.as_str(), |v| value.visit(v));
        }
    }

    fn resolve(&self, r: &mut dyn Resolver) -> dataflow_rs::Result<Option<OwnedDataValue>> {
        let entries = self
            .0
            .iter()
            .map(|(key, value)| {
                let value = value.resolve(r)?.unwrap_or(OwnedDataValue::Null);
                Ok((key.clone(), value))
            })
            .collect::<dataflow_rs::Result<Vec<_>>>()?;
        Ok(Some(OwnedDataValue::Object(entries)))
    }

    fn kind() -> FieldKind {
        FieldKind::Map(T::kind)
    }
}

/// Each field type through `testing::config_with` and `testing::call` on a
/// probe function, against its dataflow-rs 3 twin run on a dataflow-rs 3
/// engine over the same input, message and secret store: the expectation is
/// computed from the twin, never written out.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ResolvedSecrets;
    use crate::engine::functions::templated_input::TemplatedInput;
    use crate::engine::operators::{orion_exprs, with_orion_engine_defaults};
    use async_trait::async_trait;
    use dataflow_rs::expr::{ExprEngine, ExprVisitor};
    use dataflow_rs::function::{FunctionConfig, testing};
    use dataflow_rs::program::AuthoredPath;
    use dataflow_rs::{Function, Message, Outcome};
    use dataflow_rs_v3::engine::functions::{AsyncFunctionHandler, TemplateCompiler};
    use dataflow_rs_v3::engine::task_context::TaskContext;
    use dataflow_rs_v3::engine::task_outcome::TaskOutcome;
    use serde_json::{Map, json};

    /// The store both engines read: a string, a non-string, and a client id.
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

    /// The message both engines run over.
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

    /// The task `probe`, with `input`, alone in a workflow.
    fn workflow(input: Option<Value>) -> Value {
        let mut function = json!({"name": "probe"});
        function["input"] = input.unwrap_or_else(|| json!({}));
        json!({
            "id": "w", "name": "w", "condition": true,
            "tasks": [{"id": "t", "name": "t", "function": function}]
        })
    }

    /// `input` (`{field: value}`, or `{}` when `value` is `None`).
    fn input(field: &str, value: Option<&Value>) -> Option<Value> {
        Some(match value {
            Some(value) => json!({field: value}),
            None => json!({}),
        })
    }

    /// Runs `handler` as task `probe` on a dataflow-rs 3 engine built as 1.12
    /// built one, and returns what it wrote at `data.out`.
    async fn run_v3<H: AsyncFunctionHandler>(handler: H, input: Option<Value>) -> Value {
        let workflow: dataflow_rs_v3::Workflow =
            serde_json::from_value(workflow(input)).expect("a workflow");
        let engine = with_orion_engine_defaults(dataflow_rs_v3::Engine::builder(), &secrets())
            .register("probe", handler)
            .with_workflow(workflow)
            .build()
            .expect("the engine builds");
        let mut message = dataflow_rs_v3::Message::from_value(&json!({}));
        dataflow_rs_v3::engine::utils::set_nested_value(
            &mut message.context,
            "data",
            OwnedDataValue::from(&data()),
        );
        engine
            .process_message(&mut message)
            .await
            .expect("the probe never fails its task");
        let data: Value = message.data().into();
        data["out"].clone()
    }

    /// Runs `f` through `testing::call` over the same message and store, and
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

    /// Writes `out` and the call's input, as a layer reads it.
    fn write(ctx: &mut Ctx<'_>, out: &Value) -> dataflow_rs::Result<Outcome> {
        let input = ctx.input()?;
        ctx.set(&"data.out".parse()?, OwnedDataValue::from(out))?;
        ctx.set(&"data.input".parse()?, input)?;
        Ok(Outcome::Ok)
    }

    // -- KeyMaterial ------------------------------------------------------

    /// `crypto`'s `key`, read as 1.12 reads it.
    struct KeyV3;

    #[async_trait]
    impl AsyncFunctionHandler for KeyV3 {
        type Input = TemplatedInput;

        fn compile_input(
            input: &mut Self::Input,
            c: &TemplateCompiler,
        ) -> dataflow_rs_v3::Result<()> {
            // `crypto`'s table compiles `key` as an expression, as in 1.12.
            input.compile("crypto", c)
        }

        async fn execute(
            &self,
            ctx: &mut TaskContext<'_>,
            input: &TemplatedInput,
        ) -> dataflow_rs_v3::Result<TaskOutcome> {
            let read = crate::engine::functions::secret_ref::key_material_field(
                input, "key", "crypto", ctx,
            )
            .await;
            let out = seen(read.map(Value::String).map_err(HandlerError::from));
            ctx.set_json("data.out", &out);
            Ok(TaskOutcome::Success)
        }
    }

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

    /// Every spelling resolves as `key_material_field` resolves it — the
    /// material, or the class and message of the refusal — and `input()`
    /// writes the field `null` exactly when it reads a secret.
    #[tokio::test]
    async fn key_material_resolves_as_key_material_field_does() {
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
            let expected = run_v3(KeyV3, input("key", value.as_ref())).await;
            let (out, written) = run_v4(KeyV4, input("key", value.as_ref())).await;
            assert_eq!(out, expected, "key = {value:?}");
            assert_eq!(
                written.get("key").cloned().unwrap_or(Value::Null),
                in_input,
                "key = {value:?}"
            );
            if value.is_none() {
                assert_eq!(written, json!({}), "an unset key is absent from the input");
            }
        }
        // The material of the store's key, not merely the same as 1.12's.
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

    /// `jwt_verify`'s `audience`, read as 1.12 reads it.
    struct ListV3;

    #[async_trait]
    impl AsyncFunctionHandler for ListV3 {
        type Input = TemplatedInput;

        fn compile_input(
            input: &mut Self::Input,
            c: &TemplateCompiler,
        ) -> dataflow_rs_v3::Result<()> {
            input.compile("jwt_verify", c)
        }

        async fn execute(
            &self,
            ctx: &mut TaskContext<'_>,
            input: &TemplatedInput,
        ) -> dataflow_rs_v3::Result<TaskOutcome> {
            let read =
                crate::engine::functions::jwt_verify::string_or_vec(input, "audience", ctx).await;
            let out = seen(
                read.map(|values| Value::Array(values.into_iter().map(Value::String).collect()))
                    .map_err(HandlerError::from),
            );
            ctx.set_json("data.out", &out);
            Ok(TaskOutcome::Success)
        }
    }

    #[derive(Debug, FunctionConfig)]
    struct ListConfig {
        audience: Option<KeyMaterialList>,
    }

    /// `jwt_verify`'s `audience`, read through the kit.
    struct ListV4;

    impl Function for ListV4 {
        type Config = ListConfig;

        async fn call(&self, ctx: &mut Ctx<'_>, cfg: &ListConfig) -> dataflow_rs::Result<Outcome> {
            let read =
                key_material_list(cfg.audience.as_ref(), ctx, "audience", "jwt_verify").await;
            let read =
                read.map(|values| Value::Array(values.into_iter().map(Value::String).collect()));
            write(ctx, &seen(read))
        }
    }

    /// `string_or_vec`'s cases, each read as it reads them: a string, an
    /// array, holes in either, a secret node as the field or an element, a
    /// `null` element dropped, and every refusal.
    #[tokio::test]
    async fn key_material_list_reads_as_string_or_vec_does() {
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
            let expected = run_v3(ListV3, input("audience", value.as_ref())).await;
            let (out, _) = run_v4(ListV4, input("audience", value.as_ref())).await;
            assert_eq!(out, expected, "audience = {value:?}");
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

    /// `jwt_verify`'s `keys[].key`, read as 1.12 reads it: as written, with
    /// `secret_ref::key_material`.
    struct RefV3;

    #[async_trait]
    impl AsyncFunctionHandler for RefV3 {
        type Input = TemplatedInput;

        fn compile_input(
            input: &mut Self::Input,
            c: &TemplateCompiler,
        ) -> dataflow_rs_v3::Result<()> {
            input.compile("jwt_verify", c)
        }

        async fn execute(
            &self,
            ctx: &mut TaskContext<'_>,
            input: &TemplatedInput,
        ) -> dataflow_rs_v3::Result<TaskOutcome> {
            let raw = input.get("key").cloned().unwrap_or(Value::Null);
            let read =
                crate::engine::functions::secret_ref::key_material(&raw, "jwt_verify.keys", ctx)
                    .await;
            let out = seen(read.map(Value::String).map_err(HandlerError::from));
            ctx.set_json("data.out", &out);
            Ok(TaskOutcome::Success)
        }
    }

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

    /// Every spelling resolves as `key_material` resolves it: a string as a
    /// literal or a reference, a secret node from the store, and anything
    /// else refused, an expression included, which is never evaluated. The
    /// input writes the field `null` exactly when it reads a secret, and
    /// anything else as written.
    #[tokio::test]
    async fn key_ref_resolves_as_key_material_does() {
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
            let expected = run_v3(RefV3, input("key", Some(&value))).await;
            let (out, written) = run_v4(RefV4, input("key", Some(&value))).await;
            assert_eq!(out, expected, "key = {value}");
            assert_eq!(written["key"], in_input, "key = {value}");
        }
        // The material of the store's key, not merely the same as 1.12's.
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

    // -- DetachedExpr -----------------------------------------------------

    #[derive(Debug, FunctionConfig)]
    struct SelectConfig {
        select: Option<DetachedExpr>,
    }

    /// Evaluates `select` over the outputs it is given, not the message.
    struct SelectV4;

    impl Function for SelectV4 {
        type Config = SelectConfig;

        async fn call(
            &self,
            ctx: &mut Ctx<'_>,
            cfg: &SelectConfig,
        ) -> dataflow_rs::Result<Outcome> {
            let Some(select) = &cfg.select else {
                return write(ctx, &Value::Null);
            };
            let root = json!({"policy": [0.1, 0.7, 0.2], "input": {"n": 2}});
            let value = ctx.exprs().eval(select.expr(), &root)?.value.into_owned();
            write(ctx, &Value::from(&value))
        }
    }

    #[test]
    fn detached_expr_refuses_a_secret_read() {
        for select in [
            json!({"secret": "signing"}),
            json!({"cat": ["x", {"secret": "signing"}]}),
            json!({"secret": {"var": "policy"}}),
        ] {
            let issues = testing::config_with::<SelectConfig>(&exprs(), json!({"select": select}))
                .expect_err("a secret read is refused");
            let codes: Vec<(IssueCode, String)> = issues
                .iter()
                .map(|i| (i.code, i.path.to_string()))
                .collect();
            assert_eq!(
                codes,
                [(IssueCode::SecretInMessageWrite, "select".to_string())],
                "{select}"
            );
        }
    }

    /// Nothing is visited — a `var` here reads the outputs, so the message
    /// checks would misread it (a `payload` read is no `PAYLOAD_VAR`) — and
    /// the expression is evaluated over the function's own root.
    #[tokio::test]
    async fn detached_expr_visits_nothing_and_evaluates_over_its_own_root() {
        struct Count(usize);
        impl ExprVisitor for Count {
            fn expr(&mut self, _at: &AuthoredPath, _raw: &Value, _records: bool) {
                self.0 += 1;
            }
            fn interpolated(&mut self, _at: &AuthoredPath, _doc: &Interpolated) {
                self.0 += 1;
            }
        }
        let cfg =
            testing::config_with::<SelectConfig>(&exprs(), json!({"select": {"var": "payload.x"}}))
                .expect("compiles, and no check runs over it");
        let mut count = Count(0);
        cfg.select
            .expect("set")
            .visit(&mut Visit::new(&mut count, AuthoredPath::root()));
        assert_eq!(count.0, 0);

        let select = json!({"var": "policy.1"});
        let (out, input) = run_v4(SelectV4, Some(json!({"select": select}))).await;
        assert_eq!(
            out,
            json!(0.7),
            "read from the function's root, not the message"
        );
        assert_eq!(input["select"], select, "resolved to its raw JSON");

        let (out, _) = run_v4(
            SelectV4,
            Some(json!({"select": {"*": [{"var": "input.n"}, 3]}})),
        )
        .await;
        assert_eq!(out, json!(6));
        let (out, _) = run_v4(SelectV4, Some(json!({"select": null}))).await;
        assert_eq!(out, Value::Null, "a null select is unset");
        assert!(matches!(
            DetachedExpr::kind(),
            FieldKind::Literal(ValueKind::Any)
        ));
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

        async fn call(
            &self,
            ctx: &mut Ctx<'_>,
            _cfg: &MembersConfig,
        ) -> dataflow_rs::Result<Outcome> {
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
}
