use serde_json::Value;

use super::{
    RESERVED_SCHEMES, SecretResolver, default_resolvers, is_resolvable_reference, parse_reference,
};
use crate::errors::OrionError;

/// Resolve one string that may be a secret reference (`env://VAR`, …); a
/// literal passes through unchanged.
///
/// The single-value convenience over [`resolve_in_place`] with the default
/// resolver registry — channel auth and the `crypto` task function both drive
/// it, so a workflow author has exactly one reference mechanism to learn and
/// production key material never has to sit in a stored definition.
/// `field` names the referencing field in error messages; the resolved value
/// itself never appears in one.
pub async fn resolve_secret_string(value: &str, field: &str) -> Result<String, String> {
    let mut json = Value::String(value.to_string());
    resolve_in_place(&mut json, default_resolvers(), field)
        .await
        .map_err(|e| e.to_string())?;
    json.as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{field} did not resolve to a string"))
}

/// Walk `value`, replacing each `scheme://reference` string with the value
/// from the matching resolver. Strings without a recognized scheme pass
/// through unchanged. Other JSON types (numbers, bools, null) are never
/// modified.
///
/// Three phases rather than one recursive walk (H3c): collect the distinct
/// references synchronously, resolve each **once** (a document repeating a
/// reference costs one lookup — which matters because a lookup can be an
/// HTTP round trip), then substitute synchronously. It also sidesteps async
/// recursion, which would otherwise need per-level boxing.
pub async fn resolve_in_place(
    value: &mut Value,
    resolvers: &[Box<dyn SecretResolver>],
    source_label: &str,
) -> Result<(), OrionError> {
    let mut wanted: Vec<String> = Vec::new();
    collect_references(value, resolvers, &mut wanted);
    if wanted.is_empty() {
        return Ok(());
    }

    let mut resolved: std::collections::HashMap<String, String> = Default::default();
    for reference_string in wanted {
        let (scheme, reference) =
            parse_reference(&reference_string).ok_or_else(|| OrionError::Config {
                message: format!("{source_label}: invalid secret reference '{reference_string}'"),
            })?;
        let resolver = resolvers
            .iter()
            .find(|r| r.scheme() == scheme)
            .ok_or_else(|| OrionError::Config {
                message: format!("{source_label}: unknown secret scheme '{scheme}'"),
            })?;
        let secret = resolver.resolve(reference).await.map_err(|e| match e {
            OrionError::Config { message } => OrionError::Config {
                message: format!("{source_label}: {message}"),
            },
            other => other,
        })?;
        resolved.insert(reference_string, secret);
    }

    substitute(value, &resolved);
    Ok(())
}

/// Phase 1 of [`resolve_in_place`]: every distinct string in the tree whose
/// scheme matches a registered resolver, in first-seen order.
fn collect_references(value: &Value, resolvers: &[Box<dyn SecretResolver>], out: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            if let Some((scheme, _)) = parse_reference(s)
                && resolvers.iter().any(|r| r.scheme() == scheme)
                && !out.iter().any(|seen| seen == s)
            {
                out.push(s.clone());
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_references(v, resolvers, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_references(v, resolvers, out);
            }
        }
        _ => {}
    }
}

/// Phase 3 of [`resolve_in_place`].
fn substitute(value: &mut Value, resolved: &std::collections::HashMap<String, String>) {
    match value {
        Value::String(s) => {
            if let Some(secret) = resolved.get(s.as_str()) {
                *s = secret.clone();
            }
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                substitute(v, resolved);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                substitute(v, resolved);
            }
        }
        _ => {}
    }
}

/// A reference that sits *inside* a longer string — `"Bearer env://API_KEY"`
/// — and is therefore not a reference at all: a reference is the whole
/// value, so this text is sent literally. `Some(scheme)` names the scheme
/// that was meant.
///
/// A scheme only counts where it starts a word (`someenv://k` is not a hit)
/// and is followed by a name. A string that is one `${…}` placeholder is
/// skipped: `${X:-env://Y}` becomes a whole-string reference after
/// substitution.
pub fn embedded_reference(s: &str) -> Option<&'static str> {
    if s.starts_with("${") && s.ends_with('}') {
        return None;
    }
    std::iter::once("env")
        .chain(RESERVED_SCHEMES.iter().copied())
        .find(|scheme| {
            let needle = format!("{scheme}://");
            s.match_indices(&needle).any(|(at, _)| {
                at > 0
                    && !s[..at]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-'))
                    && !s[at + needle.len()..].trim().is_empty()
            })
        })
}

/// One string in a connector config with a reference inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedReference {
    /// Dotted path from the config root, e.g. `headers.Authorization`.
    pub path: String,
    /// The scheme that was meant, e.g. `env`.
    pub scheme: &'static str,
    /// The string sits under `headers` as an `Authorization` header, where
    /// `auth` is the field that takes a reference.
    pub authorization_header: bool,
}

impl EmbeddedReference {
    /// What an author should do instead.
    pub fn remedy(&self) -> &'static str {
        if self.authorization_header {
            "use \"auth\": {\"type\": \"bearer\", \"token\": \"env://API_KEY\"} instead of an \
             Authorization header"
        } else {
            "make the reference the whole value, or build the string in the deployment \
             environment"
        }
    }

    /// The finding, in one sentence.
    pub fn message(&self) -> String {
        format!(
            "`{}://…` sits inside a longer string, so it is not a reference: a reference must be \
             the whole value, and this text will be sent literally",
            self.scheme
        )
    }
}

/// Every string in `config` with [`embedded_reference`] in it.
pub fn embedded_references(config: &Value) -> Vec<EmbeddedReference> {
    fn walk(value: &Value, path: &mut Vec<String>, out: &mut Vec<EmbeddedReference>) {
        match value {
            Value::String(s) => {
                if let Some(scheme) = embedded_reference(s) {
                    out.push(EmbeddedReference {
                        path: path.join("."),
                        scheme,
                        authorization_header: matches!(
                            path.as_slice(),
                            [headers, name] if headers == "headers"
                                && name.eq_ignore_ascii_case("authorization")
                        ),
                    });
                }
            }
            Value::Object(map) => {
                for (key, v) in map {
                    path.push(key.clone());
                    walk(v, path, out);
                    path.pop();
                }
            }
            Value::Array(items) => {
                for (index, v) in items.iter().enumerate() {
                    path.push(index.to_string());
                    walk(v, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(config, &mut Vec::new(), &mut out);
    out
}

/// Whether `s` stands for a value resolved at load — a `var://` or a
/// resolvable secret reference — and so may sit in a field of any type in an
/// authored connector config.
pub fn is_load_time_reference(s: &str) -> bool {
    s.starts_with(crate::config::vars::VAR_SCHEME) || is_resolvable_reference(s)
}

/// The authoring parse of a connector config: a reference standing in a
/// field that is not a string is replaced by a placeholder of the kind the
/// field wants, so a document the load path can type types here too — on a
/// host that holds none of the deployment's values. A check that reads such
/// a field reads the placeholder; see
/// [`crate::config::vars::parse_with_unresolved_vars`].
pub struct UnresolvedReferences;

impl super::super::VariantParse for UnresolvedReferences {
    type Error = String;
    fn parse<T: serde::de::DeserializeOwned>(&self, value: &Value) -> Result<T, String> {
        crate::config::vars::parse_with_unresolved_refs(value, &|_| false, &is_load_time_reference)
    }
}

/// Paths of every string [`resolve_in_place`] will replace, collected before
/// it runs — so the typed parse after it knows which values came from a
/// reference.
pub(crate) fn reference_sites(
    value: &Value,
    resolvers: &[Box<dyn SecretResolver>],
) -> Vec<Vec<crate::config::vars::Seg>> {
    use crate::config::vars::Seg;
    fn walk(
        value: &Value,
        resolvers: &[Box<dyn SecretResolver>],
        path: &mut Vec<Seg>,
        out: &mut Vec<Vec<Seg>>,
    ) {
        match value {
            Value::String(s) => {
                if let Some((scheme, _)) = parse_reference(s)
                    && resolvers.iter().any(|r| r.scheme() == scheme)
                {
                    out.push(path.clone());
                }
            }
            Value::Object(map) => {
                for (key, v) in map {
                    path.push(Seg::Key(key.clone()));
                    walk(v, resolvers, path, out);
                    path.pop();
                }
            }
            Value::Array(items) => {
                for (index, v) in items.iter().enumerate() {
                    path.push(Seg::Index(index));
                    walk(v, resolvers, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(value, resolvers, &mut Vec::new(), &mut out);
    out
}

/// Why a resolved connector config did not type. None of these quotes a
/// resolved value: a mis-pointed reference may hold a credential (S21).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedParseError {
    /// A boolean field's reference resolved to text other than `true` or
    /// `false`. `field` is the dotted path.
    NotABoolean { field: String },
    /// Any other refusal at a reference site; the value is withheld.
    ShapeAtSite { field: String },
    /// A refusal away from every reference site — the author's own value,
    /// safe to show.
    Shape(String),
}

impl std::fmt::Display for ResolvedParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotABoolean { field } => write!(
                f,
                "{field}: the reference resolved to something other than true or false"
            ),
            Self::ShapeAtSite { field } => write!(
                f,
                "{field}: the reference resolved to a value this field cannot take (the value \
                 is withheld: it may be a secret)"
            ),
            Self::Shape(message) => f.write_str(message),
        }
    }
}

/// The load parse of a connector config whose references [`resolve_in_place`]
/// has replaced. A reference resolves to a string, always; where the shape
/// wants a boolean, exactly `true` or `false` (trimmed, any case) at a
/// reference site becomes that boolean. Nothing else is coerced, and nothing
/// away from a site is — a literal `"true"` the author wrote in a boolean
/// field is still their error.
pub struct ResolvedReferences<'a> {
    pub(crate) sites: &'a [Vec<crate::config::vars::Seg>],
}

impl super::super::VariantParse for ResolvedReferences<'_> {
    type Error = ResolvedParseError;
    fn parse<T: serde::de::DeserializeOwned>(
        &self,
        value: &Value,
    ) -> Result<T, ResolvedParseError> {
        use crate::config::vars::{Seg, display_path, slot_at};
        let mut doc = value.clone();
        // Each site is coerced at most once, so the loop is bounded.
        let mut coerced: Vec<Vec<Seg>> = Vec::new();
        loop {
            let err = match serde_path_to_error::deserialize::<_, T>(doc.clone()) {
                Ok(typed) => return Ok(typed),
                Err(err) => err,
            };
            let at: Option<Vec<Seg>> = err.path().iter().map(Seg::from_segment).collect();
            let Some(at) = at.filter(|at| self.sites.contains(at)) else {
                return Err(ResolvedParseError::Shape(err.to_string()));
            };
            let field = display_path(&at);
            let text = slot_at(&mut doc, &at)
                .and_then(|slot| slot.as_str())
                .map(|s| s.trim().to_ascii_lowercase());
            let wants_bool = err.inner().to_string().contains("expected a boolean");
            match text.as_deref() {
                Some(b @ ("true" | "false")) if !coerced.contains(&at) => {
                    let b = b == "true";
                    if let Some(slot) = slot_at(&mut doc, &at) {
                        *slot = Value::Bool(b);
                    }
                    coerced.push(at);
                }
                _ if wants_bool => return Err(ResolvedParseError::NotABoolean { field }),
                _ => return Err(ResolvedParseError::ShapeAtSite { field }),
            }
        }
    }
}
