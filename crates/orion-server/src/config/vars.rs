//! `[vars]` and `[secrets]` — the two ways an operator declares a value that
//! workflow expressions may read.
//!
//! They exist for the same reason: a definition is promoted between instances
//! unchanged, so anything that differs per environment — a topic prefix, a
//! partner's base URL, a signing key — cannot be written into the definition
//! itself. They differ in exactly one respect, and it is the one that decides
//! which section a value belongs in:
//!
//! | | Read as | Recorded |
//! |---|---|---|
//! | `[vars]` | `{"var": "metadata.vars.name"}` | **Yes** — stamped into every message's metadata, so it appears in traces |
//! | `[secrets]` | `{"secret": "name"}` | **No** — held by the engine, never part of a message |
//!
//! A var is deployment configuration, and an operator debugging "which topic
//! did this run publish to?" needs it in the trace. A secret is key material,
//! and the whole point is that it is nowhere a trace can reach.
//!
//! Values reach both sections the same way: `${VAR}` placeholders are
//! substituted into the config text before it is parsed (see
//! `super::env_substitute`), so both sections read from the process
//! environment without either one naming a resolver.
//!
//! The difference resurfaces in what a value may *be*. A secret must be a
//! `env://` / `vault://` reference resolved at startup, never a literal — a
//! key pasted into a config file is a key in the deployment's file tree. A var
//! must be the opposite: a literal, because nothing resolves a reference on
//! its way into metadata, so an `env://` there would reach a workflow as the
//! nine characters `env://` and its name.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::errors::OrionError;

/// Replace every `var://name` reference in `value` with the declared var's
/// **JSON value**, in place.
///
/// Typed substitution, unlike the `env://` / `vault://` pass in
/// [`crate::connector::secrets`], which always produces a string. That is not a
/// stylistic difference: the config fields worth parameterising per instance
/// are mostly numbers — a cache TTL, a rate limit, a concurrency cap — and a
/// string `"60"` where a `u64` belongs fails to deserialize. A var already has
/// a type, so it keeps it.
///
/// A reference is the *whole* string, as the secret schemes are: `"var://ttl"`
/// resolves, `"ttl is var://ttl"` is that text. `skip` is consulted for every
/// object key and stops the walk descending into it.
///
/// # Errors
///
/// Names a var the config does not declare, listing what it does. Failing is
/// the point: passing `var://ttl` through as its own nine-plus characters is
/// how a rate limit silently becomes unparseable, or worse, parses as
/// something else.
pub fn resolve_var_references(
    value: &mut serde_json::Value,
    vars: Option<&serde_json::Value>,
    skip: &dyn Fn(&str) -> bool,
) -> Result<(), String> {
    match value {
        serde_json::Value::String(s) => {
            let Some(name) = s.strip_prefix(VAR_SCHEME) else {
                return Ok(());
            };
            let declared = vars.and_then(|v| v.get(name)).ok_or_else(|| {
                let known: Vec<&str> = vars
                    .and_then(|v| v.as_object())
                    .map(|m| m.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                if known.is_empty() {
                    format!("'{s}' names a var, but this instance declares no [vars] section")
                } else {
                    format!(
                        "'{s}' names a var this instance does not declare — [vars] has: {}",
                        known.join(", ")
                    )
                }
            })?;
            *value = declared.clone();
            Ok(())
        }
        serde_json::Value::Object(map) => map
            .iter_mut()
            .filter(|(key, _)| !skip(key))
            .try_for_each(|(_, v)| resolve_var_references(v, vars, skip)),
        serde_json::Value::Array(items) => items
            .iter_mut()
            .try_for_each(|v| resolve_var_references(v, vars, skip)),
        _ => Ok(()),
    }
}

/// Type a definition whose `var://` references cannot be resolved here.
///
/// Authoring-time validation runs where the deployment's values are not:
/// `POST /channels`, `orion-server lint` and `package lint` all have to pass on
/// a CI runner that declares no vars and holds no secrets. A secret reference
/// survives that because it is a string sitting in a string field. A var can
/// stand in for anything — a `u64` TTL, a bool, a list — and the field it
/// stands in may be required, so neither keeping the reference nor dropping it
/// types every document: `"ttl_secs": "var://ttl"` where a `u64` belongs fails
/// to parse, and dropping `"client_id": "var://id"` reports a required field
/// as missing, which is how the book's own `oauth2_login` example came to be
/// refused at create.
///
/// So the deserializer is asked. The document is typed as written, and each
/// time the parse stops at a reference — naming its path — that one reference
/// is replaced by a placeholder of the kind the field wants (`1`, `true`, `[]`,
/// `{}`, and finally nothing at all, for an optional member) and the parse is
/// run again. A reference the shape wants as a string is kept as the string it
/// is, so a required string field holding one is present. Nothing else in the
/// document is touched, and an error anywhere else — a type the author got
/// wrong, a key the shape does not have (even one whose value is a reference)
/// — is theirs, reported with its path.
///
/// What comes back is a *shape*, not the configuration that will serve: a
/// placeholder stands wherever a var was not a string, so a check reading such
/// a field is reading `1`. The load path substitutes the real value into the
/// JSON before typing it ([`resolve_var_references`]) and refuses the row if
/// it does not fit, against the instance that declares it. `skip` is consulted
/// for every object key and stops the walk descending into it, as it does
/// there.
pub fn parse_with_unresolved_vars<T: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    skip: &dyn Fn(&str) -> bool,
) -> Result<T, String> {
    let mut sites = Vec::new();
    collect_var_sites(value, skip, &mut Vec::new(), &mut sites);
    let mut doc = value.clone();
    // How many placeholders each reference has been through.
    let mut tried: HashMap<Vec<Seg>, usize> = HashMap::new();
    // Members dropped after every placeholder was refused, so a "missing
    // field" that follows is told apart from an author's omission.
    let mut dropped: Vec<Vec<Seg>> = Vec::new();
    loop {
        let err = match serde_path_to_error::deserialize::<_, T>(doc.clone()) {
            Ok(typed) => return Ok(typed),
            Err(err) => err,
        };
        let at: Option<Vec<Seg>> = err.path().iter().map(Seg::from_segment).collect();
        let Some(at) = at else {
            return Err(err.to_string());
        };
        // A key the shape does not have is the author's whatever its value
        // holds; trying placeholders there would end by dropping the member
        // and hiding the typo.
        if !sites.contains(&at) || is_unknown_field(err.inner()) {
            if let Some(site) = dropped.iter().find(|site| {
                let (Some(Seg::Key(key)), parent) = (site.last(), &site[..site.len() - 1]) else {
                    return false;
                };
                parent == at.as_slice() && names_missing_field(err.inner(), key)
            }) {
                return Err(format!(
                    "{} holds '{}', and no value of any kind fits there without the value it \
                     stands for; use a literal here, or a var where the field takes a string, \
                     number, boolean, list or object",
                    display_path(site),
                    reference_at(value, site)
                ));
            }
            return Err(err.to_string());
        }
        let n = tried.entry(at.clone()).or_insert(0);
        match placeholder(*n) {
            Some(candidate) => set_at(&mut doc, &at, candidate),
            None => {
                // Every placeholder refused. Drop the member if it is one —
                // the field may be optional — and let the next parse say.
                if !matches!(at.last(), Some(Seg::Key(_))) || dropped.contains(&at) {
                    return Err(err.to_string());
                }
                remove_at(&mut doc, &at);
                dropped.push(at.clone());
            }
        }
        *n += 1;
    }
}

/// One step of a path into a JSON document.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Seg {
    Key(String),
    Index(usize),
}

impl Seg {
    /// `None` for an enum variant or an untracked step: neither addresses a
    /// value in the document, so an error there is nobody's reference.
    fn from_segment(segment: &serde_path_to_error::Segment) -> Option<Self> {
        match segment {
            serde_path_to_error::Segment::Map { key } => Some(Self::Key(key.clone())),
            serde_path_to_error::Segment::Seq { index } => Some(Self::Index(*index)),
            _ => None,
        }
    }
}

fn collect_var_sites(
    value: &serde_json::Value,
    skip: &dyn Fn(&str) -> bool,
    path: &mut Vec<Seg>,
    sites: &mut Vec<Vec<Seg>>,
) {
    match value {
        serde_json::Value::String(s) if s.starts_with(VAR_SCHEME) => sites.push(path.clone()),
        serde_json::Value::Object(map) => {
            for (key, v) in map {
                if skip(key) {
                    continue;
                }
                path.push(Seg::Key(key.clone()));
                collect_var_sites(v, skip, path, sites);
                path.pop();
            }
        }
        serde_json::Value::Array(items) => {
            for (index, v) in items.iter().enumerate() {
                path.push(Seg::Index(index));
                collect_var_sites(v, skip, path, sites);
                path.pop();
            }
        }
        _ => {}
    }
}

/// The stand-ins tried, in order, where the shape refused the reference as a
/// string. `1` rather than `0` because a count or a rate of zero is what a
/// domain check refuses.
fn placeholder(n: usize) -> Option<serde_json::Value> {
    match n {
        0 => Some(serde_json::json!(1)),
        1 => Some(serde_json::json!(true)),
        2 => Some(serde_json::json!([])),
        3 => Some(serde_json::json!({})),
        _ => None,
    }
}

fn is_unknown_field(err: &serde_json::Error) -> bool {
    err.to_string().starts_with("unknown field")
}

fn names_missing_field(err: &serde_json::Error, key: &str) -> bool {
    let message = err.to_string();
    message.starts_with("missing field") && message.contains(&format!("`{key}`"))
}

fn slot_at<'a>(doc: &'a mut serde_json::Value, path: &[Seg]) -> Option<&'a mut serde_json::Value> {
    path.iter().try_fold(doc, |current, seg| match seg {
        Seg::Key(key) => current.get_mut(key.as_str()),
        Seg::Index(index) => current.get_mut(*index),
    })
}

fn set_at(doc: &mut serde_json::Value, path: &[Seg], candidate: serde_json::Value) {
    if let Some(slot) = slot_at(doc, path) {
        *slot = candidate;
    }
}

fn remove_at(doc: &mut serde_json::Value, path: &[Seg]) {
    let Some((Seg::Key(key), parent)) = path.split_last() else {
        return;
    };
    if let Some(serde_json::Value::Object(map)) = slot_at(doc, parent) {
        map.remove(key);
    }
}

fn reference_at(doc: &serde_json::Value, path: &[Seg]) -> String {
    path.iter()
        .try_fold(doc, |current, seg| match seg {
            Seg::Key(key) => current.get(key.as_str()),
            Seg::Index(index) => current.get(*index),
        })
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn display_path(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Seg::Index(index) => out.push_str(&format!("[{index}]")),
        }
    }
    out
}

/// The scheme prefix `[vars]` values are referenced by, alongside `env://` and
/// `vault://`. Spelled once so the docs, the resolver and the error text cannot
/// disagree.
pub const VAR_SCHEME: &str = "var://";

/// Deployment values stamped into `metadata.vars` on every message.
///
/// Free-form, so there is no `ORION_VARS__…` override: the values are named by
/// the operator, and `${VAR}` in the config text already covers reading them
/// from the environment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct VarsConfig(pub BTreeMap<String, toml::Value>);

/// Secret references resolved once at startup and published to the engine,
/// where `{"secret": "name"}` reaches them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct SecretsConfig(pub BTreeMap<String, String>);

impl VarsConfig {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The whole section as one JSON object — what gets stamped into
    /// `metadata.vars`. Built once at startup, cloned per message.
    ///
    /// Returns `None` for an empty section, which is the signal to stamp
    /// nothing at all rather than an empty object: a workflow reading
    /// `metadata.vars.x` on an instance that declares no vars should see the
    /// same missing value either way, and an empty object in every trace is
    /// noise.
    pub fn to_json(&self) -> Option<serde_json::Value> {
        if self.is_empty() {
            return None;
        }
        // Infallible for the value kinds `validate` admits — every `toml::Value`
        // has a JSON form, and the one whose form is a nonsense object
        // (`Datetime`) is refused before it can get here.
        serde_json::to_value(&self.0).ok()
    }

    pub(super) fn validate(&self) -> Result<(), OrionError> {
        for (name, value) in &self.0 {
            validate_name(name, "vars")?;
            check_var_value(name, value)?;
        }
        Ok(())
    }
}

impl SecretsConfig {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }

    pub(super) fn validate(&self) -> Result<(), OrionError> {
        for (name, reference) in &self.0 {
            validate_name(name, "secrets")?;
            if !crate::connector::secrets::is_resolvable_reference(reference) {
                return Err(OrionError::Config {
                    message: format!(
                        "secrets.{name} must be a secret reference such as \
                         \"env://SOME_VAR\" or \"vault://path#key\", not a literal value \
                         (a key written into a config file is a key in the deployment's \
                         file tree)"
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Names are the path a workflow types, so they have to be typable: an
/// identifier, and nothing else.
///
/// A dot is refused for a reason beyond tidiness — `{"secret": "a.b"}` walks
/// into a nested object, so a flat key literally named `a.b` would be
/// unreachable, and `{"var": "metadata.vars.a.b"}` has the same problem.
fn validate_name(name: &str, section: &str) -> Result<(), OrionError> {
    let ok = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(OrionError::Config {
            message: format!(
                "[{section}] name '{name}' is not an identifier — names may hold \
                 ASCII letters, digits and underscores, and may not start with a digit"
            ),
        });
    }
    Ok(())
}

/// A var value must be JSON-representable and must not be a secret reference.
fn check_var_value(name: &str, value: &toml::Value) -> Result<(), OrionError> {
    match value {
        toml::Value::String(s) => {
            if crate::connector::secrets::is_resolvable_reference(s) {
                return Err(OrionError::Config {
                    message: format!(
                        "vars.{name} is a secret reference, and nothing resolves one on its \
                         way into metadata — a workflow would read the literal text '{s}'. \
                         Declare it under [secrets] and read it with \
                         {{\"secret\": \"{name}\"}}, or inline the value here"
                    ),
                });
            }
            Ok(())
        }
        toml::Value::Integer(_) | toml::Value::Float(_) | toml::Value::Boolean(_) => Ok(()),
        toml::Value::Array(items) => items
            .iter()
            .try_for_each(|item| check_var_value(name, item)),
        toml::Value::Table(table) => table
            .values()
            .try_for_each(|item| check_var_value(name, item)),
        toml::Value::Datetime(_) => Err(OrionError::Config {
            message: format!(
                "vars.{name} is a TOML datetime, which has no JSON form — write it as a \
                 quoted string"
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    /// A var keeps the type it was declared with.
    ///
    /// This is the whole reason `var://` resolves separately from `env://`:
    /// the config knobs worth varying per instance are numbers, and a string
    /// `"60"` where a `u64` belongs fails to deserialize.
    #[test]
    fn a_var_reference_substitutes_the_declared_type() {
        let vars = serde_json::json!({ "ttl": 60, "region": "eu", "on": true });
        let mut config = serde_json::json!({
            "cache": { "enabled": "var://on", "ttl_secs": "var://ttl" },
            "note": "var://region",
            "list": ["var://ttl", "literal"],
            "untouched": "ttl is var://ttl",
        });
        resolve_var_references(&mut config, Some(&vars), &|_| false).expect("resolves");
        assert_eq!(config["cache"]["ttl_secs"], serde_json::json!(60));
        assert_eq!(config["cache"]["enabled"], serde_json::json!(true));
        assert_eq!(config["note"], serde_json::json!("eu"));
        assert_eq!(config["list"][0], serde_json::json!(60));
        // A reference is the whole string, as the secret schemes are.
        assert_eq!(config["untouched"], serde_json::json!("ttl is var://ttl"));
    }

    /// An undeclared name fails and says what is declared. Passing the
    /// reference through as its own text is how a rate limit silently stops
    /// being a number.
    #[test]
    fn an_undeclared_var_is_refused_and_names_the_alternatives() {
        let vars = serde_json::json!({ "ttl": 60 });
        let mut config = serde_json::json!({ "x": "var://nope" });
        let err =
            resolve_var_references(&mut config, Some(&vars), &|_| false).expect_err("refused");
        assert!(err.contains("nope"), "{err}");
        assert!(err.contains("ttl"), "must list what is declared: {err}");

        let mut config = serde_json::json!({ "x": "var://nope" });
        let err = resolve_var_references(&mut config, None, &|_| false).expect_err("refused");
        assert!(err.contains("no [vars] section"), "{err}");
    }

    /// The skip predicate stops the walk descending into a field the caller
    /// owns — a channel's `*_logic`, which is evaluated per message and where a
    /// literal `"var://x"` is a string the author wrote to compare against.
    #[test]
    fn a_skipped_field_is_left_alone() {
        let vars = serde_json::json!({ "ttl": 60 });
        let mut config = serde_json::json!({
            "ttl_secs": "var://ttl",
            "validation_logic": { "==": [{ "var": "data.x" }, "var://ttl"] },
        });
        resolve_var_references(&mut config, Some(&vars), &|k| k.ends_with("_logic"))
            .expect("resolves");
        assert_eq!(config["ttl_secs"], serde_json::json!(60));
        assert_eq!(
            config["validation_logic"]["=="][1],
            serde_json::json!("var://ttl")
        );
    }

    use super::*;

    fn vars(toml_text: &str) -> VarsConfig {
        VarsConfig(toml::from_str(toml_text).expect("test fixture parses"))
    }

    fn secrets(toml_text: &str) -> SecretsConfig {
        SecretsConfig(toml::from_str(toml_text).expect("test fixture parses"))
    }

    #[test]
    fn a_var_keeps_the_type_it_was_written_as() {
        let json = vars("prefix = \"eu\"\nretries = 3\nverbose = true")
            .to_json()
            .expect("non-empty");
        assert_eq!(json["prefix"], serde_json::json!("eu"));
        assert_eq!(json["retries"], serde_json::json!(3));
        assert_eq!(json["verbose"], serde_json::json!(true));
    }

    #[test]
    fn an_empty_section_stamps_nothing() {
        assert!(vars("").to_json().is_none());
    }

    #[test]
    fn a_secret_reference_in_vars_is_refused() {
        let err = vars("token = \"env://PARTNER_TOKEN\"")
            .validate()
            .expect_err("a reference in vars reaches the workflow as literal text");
        assert!(err.to_string().contains("[secrets]"), "{err}");
    }

    #[test]
    fn a_literal_in_secrets_is_refused() {
        let err = secrets("token = \"sk-live-abc\"")
            .validate()
            .expect_err("a literal key in a config file is a key on disk");
        assert!(err.to_string().contains("env://"), "{err}");
        secrets("token = \"env://PARTNER_TOKEN\"")
            .validate()
            .expect("a reference is the whole point");
    }

    #[test]
    fn names_must_be_identifiers() {
        for bad in ["", "a.b", "2fast", "with space", "dash-ed"] {
            let mut map = BTreeMap::new();
            map.insert(bad.to_string(), toml::Value::String("x".into()));
            VarsConfig(map)
                .validate()
                .expect_err("'{bad}' is not a typable path segment");
        }
        vars("ok_name_2 = \"x\"")
            .validate()
            .expect("an identifier is fine");
    }

    #[test]
    fn a_datetime_var_is_refused_rather_than_silently_reshaped() {
        vars("cutover = 1979-05-27T07:32:00Z")
            .validate()
            .expect_err("TOML datetimes have no JSON form");
    }
}

#[cfg(test)]
mod parse_tests {
    use super::parse_with_unresolved_vars;
    use serde_json::json;

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Shape {
        name: String,
        count: u32,
        #[serde(default)]
        on: Option<bool>,
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default)]
        inner: Option<Inner>,
        #[serde(default)]
        select_logic: Option<serde_json::Value>,
        #[serde(default)]
        list: Vec<u32>,
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Inner {
        #[allow(dead_code)]
        needed: String,
    }

    /// A required string field keeps its reference; a field of any other type
    /// gets a stand-in of that type. Neither is reported missing, and an
    /// expression is not walked.
    #[test]
    fn a_reference_is_kept_as_a_string_or_stood_in_for_by_type() {
        let doc = json!({
            "name": "var://name",
            "count": "var://count",
            "on": "var://on",
            "tags": "var://tags",
            "list": [1, "var://two", 3],
            "select_logic": { "==": ["var://x", 1] },
        });
        let shape: Shape =
            parse_with_unresolved_vars(&doc, &|key| key.ends_with("_logic")).expect("types");
        assert_eq!(shape.name, "var://name");
        assert_eq!(shape.count, 1);
        assert_eq!(shape.on, Some(true));
        assert_eq!(shape.tags, Some(Vec::new()));
        assert_eq!(shape.list, vec![1, 1, 3]);
        assert_eq!(shape.select_logic, Some(json!({ "==": ["var://x", 1] })));
    }

    /// A member no stand-in fits is dropped when the shape lets it be.
    #[test]
    fn an_optional_block_reference_is_dropped() {
        let doc = json!({ "name": "n", "count": 2, "inner": "var://inner" });
        let shape: Shape = parse_with_unresolved_vars(&doc, &|_| false).expect("types");
        assert!(shape.inner.is_none());
    }

    /// Errors that are the author's stay the author's, path and all — a key
    /// the shape does not have included, whatever its value holds.
    #[test]
    fn the_authors_errors_are_reported_with_their_path() {
        let doc = json!({ "name": "n", "count": "var://count", "cuont": "var://typo" });
        let err = parse_with_unresolved_vars::<Shape>(&doc, &|_| false).expect_err("unknown key");
        assert!(err.contains("cuont"), "{err}");

        let doc = json!({ "name": "n", "count": "twelve" });
        let err = parse_with_unresolved_vars::<Shape>(&doc, &|_| false).expect_err("wrong type");
        assert!(err.contains("count"), "{err}");

        let doc = json!({ "count": 1 });
        let err = parse_with_unresolved_vars::<Shape>(&doc, &|_| false).expect_err("omitted");
        assert!(err.contains("name"), "{err}");
    }

    /// A required member no stand-in fits names itself and the reference,
    /// rather than reading as an omission.
    #[test]
    fn a_required_member_nothing_fits_is_named() {
        #[derive(Debug, serde::Deserialize)]
        struct Needs {
            #[allow(dead_code)]
            inner: Inner,
        }
        let doc = json!({ "inner": "var://inner" });
        let err = parse_with_unresolved_vars::<Needs>(&doc, &|_| false).expect_err("nothing fits");
        assert!(
            err.contains("inner") && err.contains("var://inner"),
            "{err}"
        );
    }
}
