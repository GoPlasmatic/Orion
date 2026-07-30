//! Secret masking for connector configs returned by the admin API.
//!
//! Two independent rules, applied to the whole config tree rather than to a
//! fixed list of top-level keys:
//!
//!   1. **By key name** — a scalar under a secret-looking key is replaced
//!      wholesale. When the key itself names a credential bundle
//!      (`credentials`, `auth_token`, …) everything beneath it is masked too,
//!      whatever the child keys are called.
//!   2. **By value shape** — any string carrying URL userinfo has its password
//!      component replaced, and any query parameter whose *name* looks secret
//!      under rule 1 has its value replaced, at any depth. This is what keeps
//!      `redis://:PASSWORD@host`, `https://user:pass@es:9200` and
//!      `https://api.example.com/v1?api_key=SECRET` out of
//!      `GET /api/v1/admin/connectors` while leaving the endpoint itself
//!      readable, which the admin UI needs.
//!
//! Known limitation (S18): a credential embedded in a URL *path* under a
//! non-secret key — `{"url": "https://hooks.slack.com/services/T00/B00/XX"}`
//! — is not redacted. A path segment carries no name to judge, and masking
//! every path would blank the endpoint identity the admin UI exists to show.
//! Store such URLs under a secret-looking key (`webhook`, `webhook_url`, …);
//! rule 1 then masks the whole value.

use serde_json::Value;
use std::collections::HashMap;

use crate::storage::models::ConnectorResponse;

pub(crate) const MASK: &str = "******";

/// Substrings that make a key a secret wherever it appears in the tree.
/// Deliberately narrow: `key` alone is not here because it matches innocuous
/// names like `cache_key_fields` — it is handled as an exact match below.
const SECRET_KEY_SUBSTRINGS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "credential",
    "api_key",
    "apikey",
    "private_key",
    "privatekey",
    "access_key",
    "accesskey",
    "signature",
    "bearer",
    // A DSN carries credentials by definition (`dsn`, `sentry_dsn`, …).
    "dsn",
    // Webhook URLs are capability tokens in the common idiom (Slack, Teams,
    // Discord embed the credential in the path), so the whole value is secret.
    "webhook",
];

/// Keys that are secrets only as an exact match.
const SECRET_KEY_EXACT: &[&str] = &[
    "key",
    "pwd",
    "cert",
    "certificate",
    "ssl_key",
    "sasl_key",
    "keyfile",
    "keytab",
    "authorization",
    "connection_string",
    // Exact rather than substring: "pat" appears in `path`/`pattern`, and
    // "sig" in `design`/`signal`. `sig` is the Azure SAS query parameter;
    // longer forms are caught by the `signature` substring.
    "pat",
    "sig",
];

/// Whether a scalar stored under `key` should be replaced with [`MASK`].
fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEY_EXACT.contains(&lower.as_str())
        || SECRET_KEY_SUBSTRINGS.iter().any(|p| lower.contains(p))
}

/// A URL-shaped string split into the segments masking cares about. One
/// parse shared by redaction ([`redact_url_secrets`]), positional
/// restoration ([`restore_url_secrets`]) and detection
/// ([`url_carries_mask`]), so the three can never disagree about what a
/// maskable position is. `None` from [`split_url`] means the string is not
/// URL-shaped and none of them apply.
///
/// Hand-rolled rather than parsed with the `url` crate: schemes such as
/// `SASL_SSL://` are not valid URL schemes but do appear in Kafka broker
/// lists, and re-serialising a parsed URL rewrites strings that had nothing
/// to hide (`https://h` → `https://h/`).
struct SplitUrl<'a> {
    scheme: &'a str,
    /// Userinfo username — present exactly when the authority carries a
    /// `…@`. A username alone is an identity, not a secret, and stays
    /// readable.
    user: Option<&'a str>,
    /// Userinfo password: what follows the first ':' of the userinfo. The
    /// first maskable position.
    password: Option<&'a str>,
    /// Host, port and path — everything between the userinfo (or scheme)
    /// and the query or fragment. Never masked.
    host_path: &'a str,
    /// Query pairs, split on '&'. The value of every secret-named pair is
    /// the second maskable position. `None` when the URL carries no '?'.
    query: Option<Vec<QueryPair<'a>>>,
    fragment: Option<&'a str>,
}

struct QueryPair<'a> {
    name: &'a str,
    /// `None` for a bare token with no '=' — never masked.
    value: Option<&'a str>,
}

fn split_url(s: &str) -> Option<SplitUrl<'_>> {
    let (scheme, rest) = s.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.' | '_'))
    {
        return None;
    }

    // The userinfo section lives before the first '/', '?' or '#' that ends
    // the authority; an '@' anywhere later is path, query or fragment data.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (user, password, after_userinfo) = match rest[..authority_end].rfind('@') {
        Some(at) => {
            let (user, password) = match rest[..at].split_once(':') {
                Some((user, password)) => (user, Some(password)),
                None => (&rest[..at], None),
            };
            (Some(user), password, &rest[at + 1..])
        }
        None => (None, None, rest),
    };

    // The query lives between the first '?' and the fragment, which starts
    // at the first '#'.
    let (main, fragment) = match after_userinfo.split_once('#') {
        Some((main, fragment)) => (main, Some(fragment)),
        None => (after_userinfo, None),
    };
    let (host_path, query) = match main.split_once('?') {
        Some((host_path, query)) => {
            let pairs = query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((name, value)) => QueryPair {
                        name,
                        value: Some(value),
                    },
                    None => QueryPair {
                        name: pair,
                        value: None,
                    },
                })
                .collect();
            (host_path, Some(pairs))
        }
        None => (main, None),
    };

    Some(SplitUrl {
        scheme,
        user,
        password,
        host_path,
        query,
        fragment,
    })
}

/// Reassemble a [`SplitUrl`], byte-for-byte for every segment the caller did
/// not replace. `password` is the (possibly replaced) userinfo password and
/// `query` the (possibly replaced) rendered pair list.
fn assemble_url(url: &SplitUrl<'_>, password: Option<&str>, query: Option<&[String]>) -> String {
    let mut out = format!("{}://", url.scheme);
    if let Some(user) = url.user {
        out.push_str(user);
        if let Some(password) = password {
            out.push(':');
            out.push_str(password);
        }
        out.push('@');
    }
    out.push_str(url.host_path);
    if let Some(pairs) = query {
        out.push('?');
        out.push_str(&pairs.join("&"));
    }
    if let Some(fragment) = url.fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// Render a query pair back to its wire form.
fn render_pair(name: &str, value: Option<&str>) -> String {
    match value {
        Some(value) => format!("{name}={value}"),
        None => name.to_string(),
    }
}

/// Replace the secrets a URL-shaped string carries in-band, preserving
/// everything else byte-for-byte: the userinfo password
/// (`scheme://user:password@host…`) and the value of every query parameter
/// whose name satisfies [`is_secret_key`] (`?api_key=…`, `?sig=…`) — one
/// predicate, two positions (S18). Returns `None` when the string carries
/// neither, so credential-free URLs are left exactly as authored.
///
/// Public because `validate-config` reuses it for the URL-shaped values its
/// summary prints verbatim, such as `storage.url` (O15).
pub fn redact_url_secrets(s: &str) -> Option<String> {
    let url = split_url(s)?;
    let mut changed = false;

    let password = url.password.map(|_| {
        changed = true;
        MASK
    });

    let query: Option<Vec<String>> = url.query.as_ref().map(|pairs| {
        pairs
            .iter()
            .map(|pair| match pair.value {
                Some(_) if is_secret_key(pair.name) => {
                    changed = true;
                    render_pair(pair.name, Some(MASK))
                }
                value => render_pair(pair.name, value),
            })
            .collect()
    });

    changed.then(|| assemble_url(&url, password, query.as_deref()))
}

/// Positional inverse of [`redact_url_secrets`] for the F34 round-trip: each
/// position of `incoming` that still reads as the mask sentinel gets its
/// value back from the *same position* of `stored`, independently. S18 gave
/// one string up to two kinds of maskable position (userinfo password +
/// secret-named query values), so a client can rotate one secret while
/// round-tripping the others still masked — whole-string comparison alone
/// cannot match such a URL and would leave the sentinel in place.
///
/// Returns `Some` when at least one position was restored. A masked position
/// with no stored counterpart — no stored password, no stored query pair of
/// that name, or a non-secret parameter name masking could never have
/// produced — is left carrying the sentinel, exactly like an unmatched
/// whole-value mask, for [`find_masked_value`] to reject.
fn restore_url_secrets(incoming: &str, stored: &str) -> Option<String> {
    let inc = split_url(incoming)?;
    let st = split_url(stored)?;
    let mut restored = false;

    let password = match inc.password {
        Some(p) if p == MASK => match st.password {
            Some(stored_password) => {
                restored = true;
                Some(stored_password)
            }
            None => Some(p),
        },
        other => other,
    };

    // Pairs match by name and occurrence, so duplicate-named parameters
    // restore in order and an edit elsewhere in the query cannot shift a
    // secret onto the wrong counterpart.
    let stored_pairs = st.query.unwrap_or_default();
    let mut occurrences: HashMap<&str, usize> = HashMap::new();
    let query: Option<Vec<String>> = inc.query.as_ref().map(|pairs| {
        pairs
            .iter()
            .map(|pair| {
                let occurrence = occurrences.entry(pair.name).or_insert(0);
                let counterpart = stored_pairs
                    .iter()
                    .filter(|sp| sp.name == pair.name)
                    .nth(*occurrence)
                    .and_then(|sp| sp.value);
                *occurrence += 1;
                match pair.value {
                    Some(v) if v == MASK && is_secret_key(pair.name) && counterpart.is_some() => {
                        restored = true;
                        render_pair(pair.name, counterpart)
                    }
                    value => render_pair(pair.name, value),
                }
            })
            .collect()
    });

    restored.then(|| assemble_url(&inc, password, query.as_deref()))
}

/// Whether a URL-shaped string still carries the mask sentinel in a maskable
/// position: the userinfo password, or *any* query value — even under a
/// non-secret name, because a sentinel there has no stored counterpart by
/// construction and must be rejected, never persisted. Positional on purpose
/// (S18): one freshly rotated secret must not launder the other position's
/// sentinel past the guard, which is exactly what a whole-string identity
/// check allowed.
fn url_carries_mask(s: &str) -> bool {
    let Some(url) = split_url(s) else {
        return false;
    };
    url.password == Some(MASK)
        || url
            .query
            .is_some_and(|pairs| pairs.iter().any(|pair| pair.value == Some(MASK)))
}

/// Walk the config tree masking secrets. `force` propagates down from a key
/// that names a credential bundle, so its children are masked regardless of
/// what they are called.
fn mask_in_place(value: &mut Value, key: Option<&str>, force: bool) {
    let secret = force || key.is_some_and(is_secret_key);
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                mask_in_place(v, Some(k.as_str()), secret);
            }
        }
        // Arrays inherit the parent key so `brokers: [...]` is still evaluated
        // against the name `brokers`.
        Value::Array(items) => {
            for item in items.iter_mut() {
                mask_in_place(item, key, secret);
            }
        }
        Value::Null => {}
        Value::String(s) => {
            if secret {
                *s = MASK.to_string();
            } else if let Some(redacted) = redact_url_secrets(s) {
                *s = redacted;
            }
        }
        // Numbers and booleans can still be secrets (a numeric PIN, say).
        other => {
            if secret {
                *other = Value::String(MASK.to_string());
            }
        }
    }
}

/// Mask every secret in a JSON-shaped tree, in place: scalars under
/// secret-looking keys are replaced wholesale, and URL userinfo passwords are
/// redacted at any depth.
///
/// This is the exact policy `GET /api/v1/admin/connectors` applies, exposed
/// so `validate-config` can dump the effective server config through the same
/// single list of secret-key patterns rather than a drifting copy (O15) —
/// `kafka.auth.sasl_password` and `admin_auth.api_keys` mask by key,
/// `storage.url` and `cluster.redis_url` by URL shape.
pub fn mask_secrets(value: &mut Value) {
    mask_in_place(value, None, false);
}

/// Mask sensitive fields in a connector's config_json for API responses.
pub fn mask_connector_secrets(config_json: &str) -> String {
    let Ok(mut val) = serde_json::from_str::<Value>(config_json) else {
        return config_json.to_string();
    };

    mask_secrets(&mut val);

    serde_json::to_string(&val).unwrap_or_else(|_| config_json.to_string())
}

/// Convert a stored connector row into the shape the admin API serves, with
/// every secret in `config_json` replaced by `******`.
///
/// This is the only constructor of [`ConnectorResponse`], and the row it reads
/// from cannot be serialized (D27) — so masking is not a step a handler can
/// skip, it is the only way to get a connector onto the wire at all.
pub fn mask_connector(connector: &crate::storage::models::Connector) -> ConnectorResponse {
    let mut masked = ConnectorResponse::from(connector);
    masked.config_json = mask_connector_secrets(&masked.config_json);
    masked
}

/// Restore values that a client round-tripped through the masked read API
/// (F34).
///
/// A `GET` → edit-one-field → `PUT` cycle sends `"******"` back for every
/// secret the reader never saw, and a wholesale `config_json` write would
/// persist the mask *as* the credential. A field is therefore treated as
/// unchanged when the incoming value is exactly what a `GET` would have shown
/// for the stored value.
///
/// Defined by re-masking the stored config and comparing, rather than by
/// re-testing key names: that covers whole-value masks
/// (`"password": "******"`), the URL form (`"url": "redis://u:******@host"`),
/// and any masking rule added later, with no second copy of the logic to keep
/// in sync. A URL that matches neither the stored value nor its masked form
/// is restored *per position* instead ([`restore_url_secrets`]) — S18 gave
/// one string several maskable positions, and a client may rotate one secret
/// while round-tripping the others still masked.
///
/// Anything still carrying a mask afterwards had no stored counterpart to
/// restore from — a genuinely new field, or a config whose shape changed — and
/// is left alone for [`find_masked_value`] to reject.
pub fn unmask_config(incoming: &mut Value, stored: &Value) {
    let mut masked_stored = stored.clone();
    mask_in_place(&mut masked_stored, None, false);
    restore_in_place(incoming, stored, &masked_stored);
}

fn restore_in_place(incoming: &mut Value, stored: &Value, masked: &Value) {
    match (incoming, stored, masked) {
        (Value::Object(inc), Value::Object(st), Value::Object(mk)) => {
            for (key, value) in inc.iter_mut() {
                if let (Some(stored_value), Some(masked_value)) = (st.get(key), mk.get(key)) {
                    restore_in_place(value, stored_value, masked_value);
                }
            }
        }
        // Masking is index-preserving, so positions line up. A reordered or
        // resized array simply stops matching and falls through to rejection.
        (Value::Array(inc), Value::Array(st), Value::Array(mk)) => {
            for ((value, stored_value), masked_value) in inc.iter_mut().zip(st).zip(mk) {
                restore_in_place(value, stored_value, masked_value);
            }
        }
        (inc, stored_value, masked_value) => {
            // `inc != stored_value` keeps this a no-op for unmasked fields,
            // and for the pathological case of a secret whose real value is
            // the mask string.
            if inc == masked_value && inc != stored_value {
                *inc = stored_value.clone();
            } else if let (Value::String(inc_s), Value::String(stored_s)) = (inc, stored_value)
                && let Some(restored) = restore_url_secrets(inc_s, stored_s)
            {
                // A URL equal to neither the stored value nor its fully
                // masked form was partially edited — restore each position
                // still carrying the mask independently, keeping the
                // caller's edits.
                *inc_s = restored;
            }
        }
    }
}

/// The dotted path of the first value that still carries the mask sentinel, or
/// `None` when the config is clean.
///
/// Used to reject writes that would persist `"******"` as a real credential —
/// on create, where there is nothing to restore from, and on update for
/// anything [`unmask_config`] could not match to a stored value.
pub fn find_masked_value(value: &Value) -> Option<String> {
    fn walk(value: &Value, path: &str) -> Option<String> {
        match value {
            Value::Object(map) => map.iter().find_map(|(key, child)| {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                walk(child, &child_path)
            }),
            Value::Array(items) => items
                .iter()
                .enumerate()
                .find_map(|(i, item)| walk(item, &format!("{path}[{i}]"))),
            // Either the whole value is the mask, or a maskable URL
            // position still reads as the mask after restoration — checked
            // per position, so one rotated secret cannot smuggle a second,
            // unrestorable sentinel through alongside it.
            Value::String(s) if s == MASK || url_carries_mask(s) => Some(path.to_string()),
            _ => None,
        }
    }

    walk(value, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_connector_secrets_bearer_token() {
        let config = r#"{"type":"http","url":"https://api.example.com","auth":{"type":"bearer","token":"secret123"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["auth"]["token"], "******");
    }

    #[test]
    fn test_mask_connector_secrets_basic_password() {
        let config = r#"{"type":"http","url":"https://api.example.com","auth":{"type":"basic","username":"user","password":"secret"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["auth"]["password"], "******");
        // Username should NOT be masked
        assert_eq!(val["auth"]["username"], "user");
    }

    #[test]
    fn test_mask_connector_secrets_api_key() {
        let config = r#"{"type":"http","url":"https://api.example.com","auth":{"type":"apikey","key":"mysecretkey"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["auth"]["key"], "******");
    }

    #[test]
    fn test_mask_connector_secrets_top_level_fields() {
        let config = r#"{"type":"http","url":"https://api.example.com","password":"top_secret","api_key":"ak123","token":"tk456","secret":"shhh"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["password"], "******");
        assert_eq!(val["api_key"], "******");
        assert_eq!(val["token"], "******");
        assert_eq!(val["secret"], "******");
        // A URL carrying no credentials stays readable — the admin UI needs to
        // show which endpoint a connector points at.
        assert_eq!(val["url"], "https://api.example.com");
    }

    #[test]
    fn url_userinfo_password_is_redacted() {
        let config = r#"{"type":"es","url":"https://elastic:s3cret@es.internal:9200"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["url"], "https://elastic:******@es.internal:9200");
    }

    #[test]
    fn url_userinfo_password_is_redacted_without_a_username() {
        let config =
            r#"{"type":"cache","backend":"redis","url":"redis://:hunter2@cache.internal:6379/0"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["url"], "redis://:******@cache.internal:6379/0");
        assert_eq!(val["backend"], "redis");
    }

    #[test]
    fn broker_list_entries_are_redacted() {
        let config = r#"{"type":"kafka","brokers":["sasl_ssl://svc:tok3n@b1.example:9093","b2.example:9092"],"topic":"orders"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["brokers"][0], "sasl_ssl://svc:******@b1.example:9093");
        // A plain host:port entry has nothing to hide.
        assert_eq!(val["brokers"][1], "b2.example:9092");
        assert_eq!(val["topic"], "orders");
    }

    #[test]
    fn nested_secrets_below_the_first_level_are_masked() {
        let config = r#"{"type":"http","url":"https://api.example.com","headers":{"authorization":"Bearer abc","x-tenant":"acme"},"extra":{"deep":{"client_secret":"cs123"}}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["headers"]["authorization"], "******");
        assert_eq!(val["headers"]["x-tenant"], "acme");
        assert_eq!(val["extra"]["deep"]["client_secret"], "******");
    }

    #[test]
    fn credential_bundles_mask_every_child() {
        // The bundle's children are not individually secret-looking, so only
        // the propagated flag saves them.
        let config = r#"{"type":"db","credentials":{"id":"AKIAEXAMPLE","value":"wJalrXUtn"},"database":"assets"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["credentials"]["id"], "******");
        assert_eq!(val["credentials"]["value"], "******");
        assert_eq!(val["database"], "assets");
    }

    #[test]
    fn kafka_sasl_password_shape_is_masked() {
        // Mirrors the `[kafka.auth]` shape added in Phase 4.
        let config = r#"{"type":"kafka","brokers":["b:9092"],"topic":"t","auth":{"sasl_mechanism":"PLAIN","sasl_username":"svc","sasl_password":"p@ss"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["auth"]["sasl_password"], "******");
        assert_eq!(val["auth"]["sasl_username"], "svc");
        assert_eq!(val["auth"]["sasl_mechanism"], "PLAIN");
    }

    #[test]
    fn innocuous_keys_are_left_alone() {
        let config = r#"{"type":"db","driver":"postgres","max_connections":10,"query_timeout_ms":5000,"cache_key_fields":["tenant"],"operations":{"read":true,"delete":false},"retry":{"max_retries":3}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["driver"], "postgres");
        assert_eq!(val["max_connections"], 10);
        assert_eq!(val["query_timeout_ms"], 5000);
        assert_eq!(val["cache_key_fields"][0], "tenant");
        assert_eq!(val["operations"]["read"], true);
        assert_eq!(val["operations"]["delete"], false);
        assert_eq!(val["retry"]["max_retries"], 3);
    }

    #[test]
    fn non_string_secret_values_are_masked() {
        let config = r#"{"type":"http","token":12345}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["token"], "******");
    }

    /// Rewritten for S18: the original pinned "only userinfo is redacted",
    /// which is exactly the gap — query-string secrets round-tripped in the
    /// clear. What must still be left alone: non-URLs, and URLs whose
    /// userinfo and query carry nothing secret-looking.
    #[test]
    fn redact_url_secrets_ignores_non_urls_and_credential_free_urls() {
        assert_eq!(redact_url_secrets("plain text"), None);
        // An innocuously-named query parameter is not a credential.
        assert_eq!(redact_url_secrets("https://example.com/a?b=c"), None);
        assert_eq!(
            redact_url_secrets("https://example.com/search?q=orion&page=2"),
            None
        );
        // A username with no password is an identity, not a secret.
        assert_eq!(redact_url_secrets("postgres://user@host/db"), None);
        // A '@' in the path must not be mistaken for userinfo.
        assert_eq!(redact_url_secrets("https://host/path@here"), None);
    }

    // ---------------------------------------------------------------
    // S18: query-string and path-embedded secrets
    // ---------------------------------------------------------------

    /// The motivating case: `?api_key=SECRET` used to round-trip through
    /// `GET /admin/connectors` in the clear.
    #[test]
    fn query_string_secret_is_redacted() {
        let config = r#"{"type":"http","url":"https://api.example.com/v1?api_key=SECRET&page=2"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(
            val["url"],
            "https://api.example.com/v1?api_key=******&page=2"
        );
    }

    /// Same predicate, second position: every name `is_secret_key` catches
    /// for object keys is caught as a query-parameter name too.
    #[test]
    fn query_parameter_names_use_the_same_predicate_as_keys() {
        for (url, expected) in [
            // Azure SAS-style `sig` (exact match).
            (
                "https://acct.blob.example.com/c/b?se=2026-01-01&sig=fzo7Ax8u",
                "https://acct.blob.example.com/c/b?se=2026-01-01&sig=******",
            ),
            // AWS presigned-style `X-Amz-Signature` (substring, case-insensitive).
            (
                "https://s3.example.com/k?X-Amz-Signature=abc123",
                "https://s3.example.com/k?X-Amz-Signature=******",
            ),
            (
                "https://api.example.com/cb?access_token=tok&state=xyz",
                "https://api.example.com/cb?access_token=******&state=xyz",
            ),
        ] {
            assert_eq!(redact_url_secrets(url).as_deref(), Some(expected));
        }
    }

    #[test]
    fn userinfo_and_query_secrets_are_both_redacted_in_one_url() {
        assert_eq!(
            redact_url_secrets("https://svc:hunter2@api.example.com/v1?token=t0k3n#frag")
                .as_deref(),
            Some("https://svc:******@api.example.com/v1?token=******#frag")
        );
    }

    /// A secret-looking name inside the *fragment* must not be redacted as a
    /// query parameter — the query ends at the first '#'.
    #[test]
    fn fragment_is_not_treated_as_query() {
        assert_eq!(
            redact_url_secrets("https://example.com/docs?page=2#api_key=example"),
            None
        );
    }

    /// The documented S18 limitation, pinned so a change here is a conscious
    /// one: a path-embedded capability token under a non-secret key is NOT
    /// redacted (a path segment carries no name to judge). The mitigation is
    /// the key-name rule: the `webhook` denylist term masks the whole value.
    #[test]
    fn path_embedded_tokens_rely_on_the_key_name_rule() {
        // Under a generic key the path token stays readable — the limitation.
        assert_eq!(
            redact_url_secrets("https://hooks.slack.com/services/T00/B00/XXXX"),
            None
        );
        // Under a webhook-named key the whole URL is masked — the mitigation.
        let config = r#"{"type":"http","webhook_url":"https://hooks.slack.com/services/T00/B00/XXXX","url":"https://api.example.com"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["webhook_url"], "******");
        assert_eq!(val["url"], "https://api.example.com");
    }

    /// S18 widened the key denylist; each addition in its intended shape.
    #[test]
    fn extended_denylist_terms_are_masked() {
        let config = r#"{"type":"http","bearer":"b","sentry_dsn":"https://k@sentry.example/1","webhook":"https://hooks.example/T/B/X","pat":"ghp_abc","sig":"xyz","pattern":"/orders/{id}","path":"/v1"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["bearer"], "******");
        assert_eq!(val["sentry_dsn"], "******");
        assert_eq!(val["webhook"], "******");
        assert_eq!(val["pat"], "******");
        assert_eq!(val["sig"], "******");
        // "pat"/"sig" are exact matches precisely so these stay readable.
        assert_eq!(val["pattern"], "/orders/{id}");
        assert_eq!(val["path"], "/v1");
    }

    /// F34 round-trip for the query form: a GET → edit → PUT cycle must
    /// restore the real query secret, not persist the mask.
    #[test]
    fn test_unmask_restores_query_secret() {
        let stored: Value = serde_json::from_str(
            r#"{"type":"http","url":"https://api.example.com/v1?api_key=real-key&page=2"}"#,
        )
        .expect("test");
        let mut incoming: Value =
            serde_json::from_str(&mask_connector_secrets(&stored.to_string())).expect("test");
        assert_eq!(
            incoming["url"],
            "https://api.example.com/v1?api_key=******&page=2"
        );

        unmask_config(&mut incoming, &stored);

        assert_eq!(
            incoming["url"],
            "https://api.example.com/v1?api_key=real-key&page=2"
        );
        assert_eq!(find_masked_value(&incoming), None);
    }

    #[test]
    fn test_find_masked_value_detects_the_query_form() {
        let config: Value =
            serde_json::from_str(r#"{"url":"https://api.example.com/v1?api_key=******"}"#)
                .expect("test");
        assert_eq!(find_masked_value(&config).as_deref(), Some("url"));
    }

    #[test]
    fn test_mask_connector_secrets_no_auth() {
        let config = r#"{"type":"http","url":"https://api.example.com"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["url"], "https://api.example.com");
    }

    #[test]
    fn test_mask_connector_secrets_invalid_json() {
        let config = "not valid json";
        let masked = mask_connector_secrets(config);
        assert_eq!(masked, config);
    }

    #[test]
    fn test_mask_connector_model() {
        use chrono::NaiveDate;
        let connector = crate::storage::models::Connector {
            id: "c1".to_string(),
            name: "test".to_string(),
            connector_type: "http".to_string(),
            config_json: r#"{"type":"http","url":"https://api.example.com","auth":{"type":"bearer","token":"secret"}}"#.to_string(),
            enabled: true,
            created_at: NaiveDate::from_ymd_opt(2025, 1, 1)
                .expect("test")
                .and_hms_opt(0, 0, 0)
                .expect("test"),
            updated_at: NaiveDate::from_ymd_opt(2025, 1, 1)
                .expect("test")
                .and_hms_opt(0, 0, 0)
                .expect("test"),
        };
        let masked = mask_connector(&connector);
        assert_eq!(masked.id, "c1");
        let val: serde_json::Value = serde_json::from_str(&masked.config_json).expect("test");
        assert_eq!(val["auth"]["token"], "******");
    }

    #[test]
    fn test_mask_connector_secrets_connection_string() {
        // Masked whole rather than userinfo-redacted: a DSN carries more than
        // the password (options, sslkey paths) and the UI never needed it.
        let config = r#"{"type":"db","connection_string":"postgres://user:pass@host/db","driver":"postgres"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).expect("test");
        assert_eq!(val["connection_string"], "******");
        assert_eq!(val["driver"], "postgres");
    }

    // ---------------------------------------------------------------
    // F34: mask round-trip on write
    // ---------------------------------------------------------------

    /// The exact GET → edit-one-field → PUT cycle an admin UI performs.
    #[test]
    fn test_unmask_restores_round_tripped_secret() {
        let stored: Value = serde_json::from_str(
            r#"{"type":"http","url":"https://api.example.com","timeout_secs":5,
                "auth":{"type":"bearer","token":"real-secret"}}"#,
        )
        .expect("test");
        // What GET returned, with one non-secret field edited.
        let mut incoming: Value =
            serde_json::from_str(&mask_connector_secrets(&stored.to_string())).expect("test");
        incoming["timeout_secs"] = serde_json::json!(30);

        unmask_config(&mut incoming, &stored);

        assert_eq!(incoming["auth"]["token"], "real-secret");
        assert_eq!(incoming["timeout_secs"], 30);
        assert_eq!(find_masked_value(&incoming), None);
    }

    /// F3 widened masking to `url`, so the userinfo form has to round-trip too.
    #[test]
    fn test_unmask_restores_url_password() {
        let stored: Value =
            serde_json::from_str(r#"{"type":"cache","url":"redis://admin:hunter2@redis:6379"}"#)
                .expect("test");
        let mut incoming: Value =
            serde_json::from_str(&mask_connector_secrets(&stored.to_string())).expect("test");
        assert_eq!(incoming["url"], "redis://admin:******@redis:6379");

        unmask_config(&mut incoming, &stored);

        assert_eq!(incoming["url"], "redis://admin:hunter2@redis:6379");
    }

    /// Masking is index-preserving, so a broker list round-trips per element.
    #[test]
    fn test_unmask_restores_array_elements() {
        let stored: Value = serde_json::from_str(
            r#"{"type":"kafka","brokers":["SASL_SSL://u:p1@b1:9093","plaintext://b2:9092"]}"#,
        )
        .expect("test");
        let mut incoming: Value =
            serde_json::from_str(&mask_connector_secrets(&stored.to_string())).expect("test");

        unmask_config(&mut incoming, &stored);

        assert_eq!(incoming["brokers"][0], "SASL_SSL://u:p1@b1:9093");
        assert_eq!(incoming["brokers"][1], "plaintext://b2:9092");
    }

    /// A real edit must survive: only values that still equal the mask are
    /// restored, never a value the caller actually changed.
    #[test]
    fn test_unmask_leaves_a_genuine_new_secret_alone() {
        let stored: Value =
            serde_json::from_str(r#"{"auth":{"token":"old-secret"}}"#).expect("test");
        let mut incoming: Value =
            serde_json::from_str(r#"{"auth":{"token":"new-secret"}}"#).expect("test");

        unmask_config(&mut incoming, &stored);

        assert_eq!(incoming["auth"]["token"], "new-secret");
    }

    /// A mask with no stored counterpart cannot be restored, so it must remain
    /// findable — the handler turns that into a 400 rather than persisting it.
    #[test]
    fn test_unmask_leaves_unmatched_mask_for_rejection() {
        let stored: Value = serde_json::from_str(r#"{"auth":{"token":"old"}}"#).expect("test");
        let mut incoming: Value =
            serde_json::from_str(r#"{"auth":{"token":"old"},"password":"******"}"#).expect("test");

        unmask_config(&mut incoming, &stored);

        assert_eq!(find_masked_value(&incoming).as_deref(), Some("password"));
    }

    // ---------------------------------------------------------------
    // S18 follow-up: positional restore for multi-secret URLs
    // ---------------------------------------------------------------

    /// (a) Both positions still masked: the untouched round-trip restores
    /// both real secrets.
    #[test]
    fn test_unmask_restores_both_url_positions() {
        let stored: Value = serde_json::from_str(
            r#"{"type":"http","url":"https://svc:hunter2@api.example.com/v1?api_key=real-key&page=2"}"#,
        )
        .expect("test");
        let mut incoming: Value =
            serde_json::from_str(&mask_connector_secrets(&stored.to_string())).expect("test");
        assert_eq!(
            incoming["url"],
            "https://svc:******@api.example.com/v1?api_key=******&page=2"
        );

        unmask_config(&mut incoming, &stored);

        assert_eq!(
            incoming["url"],
            "https://svc:hunter2@api.example.com/v1?api_key=real-key&page=2"
        );
        assert_eq!(find_masked_value(&incoming), None);
    }

    /// (b) The password rotates while the query secret rides along masked:
    /// the masked position restores from the stored original and the
    /// rotation survives. This exact shape used to persist the literal
    /// sentinel as the live credential — the whole-string restore could not
    /// match a partially-edited URL, and the identity-based detection did
    /// not fire because the fresh password re-masked to something else.
    #[test]
    fn test_unmask_restores_query_secret_while_password_rotates() {
        let stored: Value = serde_json::from_str(
            r#"{"url":"https://svc:oldpass@api.example.com/v1?api_key=real-key&page=2"}"#,
        )
        .expect("test");
        let mut incoming: Value = serde_json::from_str(
            r#"{"url":"https://svc:newpass@api.example.com/v1?api_key=******&page=2"}"#,
        )
        .expect("test");

        unmask_config(&mut incoming, &stored);

        assert_eq!(
            incoming["url"],
            "https://svc:newpass@api.example.com/v1?api_key=real-key&page=2"
        );
        assert_eq!(find_masked_value(&incoming), None);
    }

    /// (b, mirrored) The query secret rotates while the password rides
    /// along masked.
    #[test]
    fn test_unmask_restores_password_while_query_secret_rotates() {
        let stored: Value = serde_json::from_str(
            r#"{"url":"https://svc:oldpass@api.example.com/v1?api_key=old-key"}"#,
        )
        .expect("test");
        let mut incoming: Value = serde_json::from_str(
            r#"{"url":"https://svc:******@api.example.com/v1?api_key=new-key"}"#,
        )
        .expect("test");

        unmask_config(&mut incoming, &stored);

        assert_eq!(
            incoming["url"],
            "https://svc:oldpass@api.example.com/v1?api_key=new-key"
        );
        assert_eq!(find_masked_value(&incoming), None);
    }

    /// (c) Two masked query secrets restore independently — an unrelated
    /// query edit in between proves the whole-string path is not doing the
    /// work.
    #[test]
    fn test_unmask_restores_two_query_secrets_independently() {
        let stored: Value = serde_json::from_str(
            r#"{"url":"https://api.example.com/v1?api_key=k1&page=2&access_token=k2"}"#,
        )
        .expect("test");
        let mut incoming: Value = serde_json::from_str(
            r#"{"url":"https://api.example.com/v1?api_key=******&page=3&access_token=******"}"#,
        )
        .expect("test");

        unmask_config(&mut incoming, &stored);

        assert_eq!(
            incoming["url"],
            "https://api.example.com/v1?api_key=k1&page=3&access_token=k2"
        );
        assert_eq!(find_masked_value(&incoming), None);
    }

    /// A masked query parameter the stored URL never carried has no
    /// counterpart: left for rejection, the same contract as a whole-value
    /// mask with no stored field.
    #[test]
    fn test_unmask_leaves_unmatched_query_mask_for_rejection() {
        let stored: Value =
            serde_json::from_str(r#"{"url":"https://api.example.com/v1?page=2"}"#).expect("test");
        let mut incoming: Value =
            serde_json::from_str(r#"{"url":"https://api.example.com/v1?page=2&api_key=******"}"#)
                .expect("test");

        unmask_config(&mut incoming, &stored);

        assert_eq!(find_masked_value(&incoming).as_deref(), Some("url"));
    }

    /// A masked password over a stored URL that has none: same rejection.
    #[test]
    fn test_unmask_leaves_unmatched_url_password_for_rejection() {
        let stored: Value =
            serde_json::from_str(r#"{"url":"https://api.example.com/v1"}"#).expect("test");
        let mut incoming: Value =
            serde_json::from_str(r#"{"url":"https://svc:******@api.example.com/v1"}"#)
                .expect("test");

        unmask_config(&mut incoming, &stored);

        assert_eq!(find_masked_value(&incoming).as_deref(), Some("url"));
    }

    /// Detection is positional too: a fresh secret in one position must not
    /// launder the sentinel in the other past the write guard. Under the
    /// old whole-string identity check both of these passed undetected.
    #[test]
    fn test_find_masked_value_detects_partially_masked_urls() {
        let config: Value = serde_json::from_str(
            r#"{"url":"https://svc:newpass@api.example.com/v1?api_key=******"}"#,
        )
        .expect("test");
        assert_eq!(find_masked_value(&config).as_deref(), Some("url"));

        let config: Value = serde_json::from_str(
            r#"{"url":"https://svc:******@api.example.com/v1?api_key=fresh"}"#,
        )
        .expect("test");
        assert_eq!(find_masked_value(&config).as_deref(), Some("url"));
    }

    /// The sentinel is rejected under a non-secret parameter name too:
    /// masking never produces it there, so it can never be restored, and it
    /// must never persist as data either.
    #[test]
    fn test_find_masked_value_detects_the_sentinel_under_any_query_name() {
        let config: Value =
            serde_json::from_str(r#"{"url":"https://api.example.com/v1?page=******"}"#)
                .expect("test");
        assert_eq!(find_masked_value(&config).as_deref(), Some("url"));
    }

    #[test]
    fn test_find_masked_value_reports_a_dotted_path() {
        let config: Value = serde_json::from_str(r#"{"a":{"b":[{"c":"******"}]}}"#).expect("test");
        assert_eq!(find_masked_value(&config).as_deref(), Some("a.b[0].c"));
    }

    #[test]
    fn test_find_masked_value_detects_the_url_form() {
        let config: Value =
            serde_json::from_str(r#"{"url":"redis://admin:******@redis:6379"}"#).expect("test");
        assert_eq!(find_masked_value(&config).as_deref(), Some("url"));
    }

    #[test]
    fn test_find_masked_value_ignores_a_clean_config() {
        let config: Value = serde_json::from_str(
            r#"{"url":"https://api.example.com","auth":{"token":"real"},"n":6}"#,
        )
        .expect("test");
        assert_eq!(find_masked_value(&config), None);
    }
}
