//! A connector handler's freeform `input`, with its expression fields compiled
//! once at engine build.
//!
//! Twelve of the fourteen connector handlers take freeform JSON rather than a
//! typed config, and until dataflow-rs 3.9 that meant they could not have
//! expression fields at all. The engine hands a custom handler its `input`
//! exactly as authored — a config document, not a template — so a handler
//! wanting a value from the message had to fold one itself, per message, with
//! [`connector_helpers::resolve_value`]: a recursive walk that recognises
//! `{"var": …}` and treats every other node as a literal. That partiality was
//! not a simplification, it was the whole design: a literal object could not be
//! told apart from an operator call, so admitting operators would have made a
//! stored MongoDB document with a `map` or `in` field start evaluating.
//!
//! dataflow-rs 3.9's template-key escape removes that ambiguity, and with it
//! the reason to fold only `var`. A field can now be a real [`Template`]:
//! compiled once when the engine is built, constant-folded when it was written
//! as a literal, and evaluated on the worker's pooled arena when it was not.
//!
//! **Which fields, without a typed struct per handler.** The declaration
//! already exists — [`FieldSchema::template_at`] on the field table each
//! handler keeps next to itself — so this type reads it rather than asking
//! twelve handlers to spell a struct. That keeps one answer to "does this field
//! evaluate", which is the property `resolvable` kept failing at: it was a
//! second declaration of something each handler decided for itself, and three
//! surfaces read the wrong one when they disagreed.
//!
//! **What is deliberately still folded, not evaluated.** A field the table does
//! not mark stays on the `{"var": …}` walk. That is not an oversight: a
//! MongoDB `filter`/`update`/`document`/`pipeline`, a dialect `query`, a JSON
//! Schema — the document-shaped fields — are full of `$`-prefixed keys
//! (`$set`, `$oid`, `$ref`), and one `$` comes off every key in a template
//! position. Making those templates would require doubling every prefix in
//! every stored definition, silently rewriting the ones that were missed. The
//! scalar fields have no such keys, so they carry the change and the documents
//! do not.
//!
//! [`connector_helpers::resolve_value`]: super::connector_helpers::resolve_value
//! [`FieldSchema::template_at`]: super::schema::FieldSchema::template_at

use std::collections::HashMap;

use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::{Template, TemplateCompiler};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A task's `input` as authored, plus one compiled [`Template`] per field the
/// registry marks as an expression.
#[derive(Debug, Default)]
pub struct TemplatedInput {
    /// The authored JSON, untouched. Every field that is *not* an expression is
    /// read straight off this, which is what a connector name, an operation
    /// name and an output path have always been.
    raw: Value,
    /// Compiled templates by field name. Empty until [`Self::compile`] runs,
    /// and empty forever for a handler whose table marks nothing.
    templates: HashMap<String, Template>,
}

impl<'de> Deserialize<'de> for TemplatedInput {
    /// Accepts any JSON, like the `Value` this replaces: a handler's input is
    /// validated by its field table, not by serde, so that the author gets
    /// every problem at once with a field path rather than the parser's first.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self {
            raw: Value::deserialize(d)?,
            templates: HashMap::new(),
        })
    }
}

impl From<Value> for TemplatedInput {
    /// For tests and callers that build an input by hand. The result holds no
    /// compiled templates, so an expression field reads as the literal it was
    /// written as — see [`Self::value_of`].
    fn from(raw: Value) -> Self {
        Self {
            raw,
            templates: HashMap::new(),
        }
    }
}

impl TemplatedInput {
    /// Compile every field `handler`'s table declares an expression.
    ///
    /// Called by the [`Connector`](super::connector_handler::Connector) wrapper
    /// rather than by each handler, so a handler cannot forget it and silently
    /// fall back to reading its expression fields as literals.
    ///
    /// A malformed expression fails here — at engine build — which for Orion is
    /// a whole-instance failure unless something catches it first. `lint` and
    /// the create/update validators are that something, and `screen_workflow`
    /// turns what still gets through into a per-channel quarantine (F33/F41)
    /// rather than a reload that takes every channel down.
    ///
    /// # Errors
    ///
    /// [`DataflowError`] when a declared field does not compile as JSONLogic.
    pub fn compile(
        &mut self,
        handler: &'static str,
        c: &TemplateCompiler,
    ) -> dataflow_rs::Result<()> {
        let Some(object) = self.raw.as_object() else {
            return Ok(());
        };
        let mut compiled = HashMap::new();
        for (field, value) in object {
            let at = super::registry::FunctionRegistry::builtin().template_paths(handler, field);
            if at.contains(&"") {
                let mut template = Template::from(value.clone());
                template.compile(c, &format!("{handler}.{field}"))?;
                compiled.insert(field.clone(), template);
            } else if at.contains(&"*")
                && let Some(members) = value.as_object()
            {
                // The field itself is a map — an `http_call`'s or an email's
                // `headers` — whose *values* are the expressions. Keyed by the
                // dotted pair so one map can hold both kinds.
                for (member, member_value) in members {
                    let mut template = Template::from(member_value.clone());
                    template.compile(c, &format!("{handler}.{field}.{member}"))?;
                    compiled.insert(format!("{field}.{member}"), template);
                }
            }
        }
        self.templates = compiled;
        Ok(())
    }

    /// The authored input, for the fields read as written.
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    /// The authored value of one field, as written. Never evaluated — use
    /// [`Self::value_of`] for a field that may be an expression.
    pub fn get(&self, field: &str) -> Option<&Value> {
        self.raw.get(field)
    }

    /// This field's value for this message, or `None` when the task does not
    /// set it.
    ///
    /// Three cases, in the order they are tried. A **compiled template** is
    /// resolved — from the constant cache when the expression folded, so a
    /// statically authored field costs no evaluation. A field the table marks
    /// `resolvable` but not an expression gets the **`{"var": …}` fold** it
    /// always got. Anything else is the **literal** it was authored as.
    ///
    /// The uncompiled-template case reads as a literal rather than failing:
    /// a `TemplatedInput` built by [`From<Value>`](Self::from) in a test has no
    /// compiled fields, and a literal is what the field would evaluate to
    /// anyway.
    ///
    /// # Errors
    ///
    /// [`DataflowError`] when evaluating the expression fails — a missing
    /// operand, a bad operator argument. A path that simply does not resolve is
    /// `null`, not an error, exactly as JSONLogic defines it.
    pub fn value_of(
        &self,
        field: &str,
        handler: &str,
        ctx: &TaskContext<'_>,
    ) -> Option<Result<Value, DataflowError>> {
        let raw = self.raw.get(field)?;
        Some(match self.templates.get(field) {
            Some(template) => template.resolve(ctx).map(|v| Value::from(&v)),
            None => Ok(super::connector_helpers::resolve_declared_field(
                handler, field, raw, ctx,
            )),
        })
    }
}

impl TemplatedInput {
    /// One member's value inside a field whose *values* are expressions — a
    /// `"*"` in [`super::schema::FieldSchema::template_at`].
    ///
    /// Separate from [`Self::value_of`] because the field itself is not an
    /// expression there: the map must still be a map, and only what is under
    /// each key evaluates.
    ///
    /// # Errors
    ///
    /// As [`Self::value_of`].
    pub fn member_value(
        &self,
        field: &str,
        member: &str,
        _handler: &str,
        ctx: &TaskContext<'_>,
    ) -> Option<Result<Value, DataflowError>> {
        let raw = self.raw.get(field)?.get(member)?;
        Some(match self.templates.get(&format!("{field}.{member}")) {
            Some(template) => template.resolve(ctx).map(|v| Value::from(&v)),
            // Never compiled — a hand-built input in a test. A literal is what
            // it would evaluate to.
            None => Ok(raw.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The engine hands a handler its input as authored, so an input that was
    /// never compiled must still read — as the literal it was written as.
    #[test]
    fn an_uncompiled_input_reads_its_fields_as_literals() {
        let input = TemplatedInput::from(json!({"key": "orders", "n": 3}));
        let datalogic = std::sync::Arc::new(dataflow_rs::datalogic_rs::Engine::new());
        let mut message = dataflow_rs::Message::from_value(&json!({}));
        let ctx = TaskContext::new(&mut message, &datalogic);

        assert_eq!(
            input
                .value_of("key", "cache_read", &ctx)
                .expect("set")
                .expect("reads"),
            json!("orders")
        );
        assert!(input.value_of("absent", "cache_read", &ctx).is_none());
    }

    /// `raw` is the authored document, so a field read as written — a
    /// connector name, an operation — is unaffected by any of this.
    #[test]
    fn the_authored_input_survives_verbatim() {
        let input = TemplatedInput::from(json!({"connector": "c", "op": "insert_one"}));
        assert_eq!(input.get("connector"), Some(&json!("c")));
        assert_eq!(input.raw()["op"], json!("insert_one"));
    }
}
