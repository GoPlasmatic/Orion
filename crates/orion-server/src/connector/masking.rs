//! Secret masking for what Orion prints or answers about a config.
//!
//! [`mask_secrets`] is a denylist walk (O15): `validate-config` prints the
//! whole effective server config, and a leaf whose key names a secret is
//! replaced with [`MASK`]. A URL-shaped value has its in-band secrets
//! redacted — userinfo password and secret-named query values
//! (`postgres://:PASSWORD@host`, `?api_key=SECRET`) — by
//! [`redact_url_secrets`], which the startup log lines and outbound-request
//! error messages use too.
//!
//! [`find_masked_value`] refuses a document that still carries the mask: a
//! value copied out of a masked read must never be stored as the credential.
//!
//! Known limitation (S18): a credential embedded in a URL *path* —
//! `https://hooks.slack.com/services/T00/B00/XX` — is not redacted. A path
//! segment carries no name to judge.

use serde_json::Value;

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
pub(crate) fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEY_EXACT.contains(&lower.as_str())
        || SECRET_KEY_SUBSTRINGS.iter().any(|p| lower.contains(p))
}

/// A URL-shaped string split into the segments masking cares about. One
/// parse shared by redaction ([`redact_url_secrets`]) and detection
/// ([`url_carries_mask`]), so the two can never disagree about what a
/// maskable position is. `None` from [`split_url`] means the string is not
/// URL-shaped and neither applies.
///
/// Hand-rolled rather than parsed with the `url` crate: re-serialising a
/// parsed URL rewrites strings that had nothing to hide (`https://h` →
/// `https://h/`).
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
/// whose name satisfies `is_secret_key` (`?api_key=…`, `?sig=…`) — one
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

/// [`redact_url_secrets`] with the "nothing to redact" case collapsed to the
/// original string — the form every log/print site wants (S20). One helper so
/// the redact-or-verbatim policy is decided in exactly one place: a site
/// spelling its own `unwrap_or_else` fallback would keep printing raw DSNs if
/// this policy ever tightens.
pub fn redact_url_secrets_or_raw(s: &str) -> String {
    redact_url_secrets(s).unwrap_or_else(|| s.to_string())
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
        // Arrays inherit the parent key so `api_keys: [...]` is still evaluated
        // against the name `api_keys`.
        Value::Array(items) => {
            for item in items.iter_mut() {
                mask_in_place(item, key, secret);
            }
        }
        Value::Null => {}
        Value::String(s) => {
            // A `scheme://NAME` reference is a *pointer* to a secret, not the
            // secret: the value lives in the process environment and the stored
            // config never held it. Masking it protects nothing — the variable
            // name is not a credential — and costs the thing `env://` exists
            // for: a connector authored this way could not survive
            // `GET /export` → `POST /import`, because every reference came back
            // as `******` and the re-imported connector pointed at nothing.
            //
            // Deliberately narrow, and the narrowness is load-bearing: the
            // check is against the schemes Orion actually resolves, not against
            // "looks like a URL". `postgres://user:password@host/db` parses as
            // `scheme://rest` too, and exempting it would hand every database
            // credential straight through the mask.
            if secret && !crate::connector::secrets::is_resolvable_reference(s) {
                *s = MASK.to_string();
            } else if !secret && let Some(redacted) = redact_url_secrets(s) {
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
/// Serves `validate-config`'s server-config dump only — `GET
/// /api/v2/admin/connectors` uses the allowlist walk in
/// `mask_connector_secrets` instead (H3), so the denylist here is kept as
/// one list of secret-key patterns rather than a drifting copy (O15) —
/// `admin_auth.api_keys` masks by key, `storage.url` by URL shape.
pub fn mask_secrets(value: &mut Value) {
    mask_in_place(value, None, false);
}

/// The dotted path of the first value that still carries the mask sentinel, or
/// `None` when the config is clean.
///
/// Used to reject a definition that would persist `"******"` as a real
/// credential: a value copied out of a masked read.
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

/// Mask a channel's `config` for a read, in place.
///
/// The channel config's schema is closed (`deny_unknown_fields`), and these
/// are the fields that hold a credential, named outright rather than judged
/// by key name: `auth.keys[*]` (API keys), `auth.secret` and `auth.secrets[*]`
/// (HMAC, with its rotation list), `auth.jwt_keys[*].key`, and
/// `oauth2_login.client_secret` / `oauth2_login.state_secret` with each
/// `oauth2_login.providers.<slug>.client_secret`. A resolvable reference
/// (`env://NAME`) survives: the stored config never held the value, and a
/// masked pointer could not be deployed again.
pub fn mask_channel_config(config: &mut Value) {
    if let Some(login) = config.get_mut("oauth2_login") {
        // `client_id` travels in the authorize URL in every user's address
        // bar: it is not a secret.
        for field in ["client_secret", "state_secret"] {
            if let Some(value) = login.get_mut(field) {
                mask_channel_secret_leaf(value);
            }
        }
        if let Some(providers) = login.get_mut("providers").and_then(Value::as_object_mut) {
            for provider in providers.values_mut() {
                if let Some(value) = provider.get_mut("client_secret") {
                    mask_channel_secret_leaf(value);
                }
            }
        }
    }
    let Some(auth) = config.get_mut("auth") else {
        return;
    };
    for list in ["keys", "secrets"] {
        if let Some(Value::Array(items)) = auth.get_mut(list) {
            for item in items.iter_mut() {
                mask_channel_secret_leaf(item);
            }
        }
    }
    if let Some(secret) = auth.get_mut("secret") {
        mask_channel_secret_leaf(secret);
    }
    if let Some(Value::Array(entries)) = auth.get_mut("jwt_keys") {
        for entry in entries.iter_mut() {
            if let Some(key) = entry.get_mut("key") {
                mask_channel_secret_leaf(key);
            }
        }
    }
}

fn mask_channel_secret_leaf(value: &mut Value) {
    match value {
        Value::String(s) if crate::connector::secrets::is_resolvable_reference(s) => {}
        Value::Null => {}
        other => *other = Value::String(MASK.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_find_masked_value_detects_the_query_form() {
        let config: Value =
            serde_json::from_str(r#"{"url":"https://api.example.com/v1?api_key=******"}"#)
                .expect("test");
        assert_eq!(find_masked_value(&config).as_deref(), Some("url"));
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
            serde_json::from_str(r#"{"url":"https://admin:******@api.internal"}"#).expect("test");
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

    // -- Secret *references* survive masking; secret *values* do not --

    use serde_json::json;

    /// `env://NAME` names a variable. Masking it protects nothing and hides
    /// which variable the config reads.
    #[test]
    fn a_resolvable_reference_survives_masking() {
        let mut v = json!({"auth": {"token": "env://STRIPE_KEY"}});
        mask_secrets(&mut v);
        assert_eq!(v["auth"]["token"], "env://STRIPE_KEY");
    }

    /// A scheme reserved for a backend Orion does not yet resolve is still a
    /// reference, not a credential, so it survives too.
    #[test]
    fn a_reserved_scheme_reference_survives_masking() {
        let mut v = json!({"password": "vault://kv/data/db#password"});
        mask_secrets(&mut v);
        assert_eq!(v["password"], "vault://kv/data/db#password");
    }

    /// The exemption must not widen to "anything with a scheme".
    ///
    /// This is the regression that matters: a database URL parses as
    /// `scheme://rest` exactly like a reference does, and exempting it would
    /// hand every stored credential through the mask untouched.
    #[test]
    fn a_database_url_is_not_a_reference_and_is_still_masked() {
        let mut v = json!({"connection_string": "postgres://user:hunter2@db/orders"});
        mask_secrets(&mut v);
        assert_eq!(
            v["connection_string"], MASK,
            "a URL is not a secret reference; masking it is the whole policy"
        );
    }

    /// Nor to strings that merely start with a known scheme name.
    #[test]
    fn a_malformed_reference_is_still_masked() {
        for value in ["env://", "environment://X", "env:/NAME", "env-var://NAME"] {
            let mut v = json!({ "token": value });
            mask_secrets(&mut v);
            assert_eq!(v["token"], MASK, "{value} must not read as a reference");
        }
    }

    /// The exemption is scoped to secret-looking keys behaving as before for
    /// everything else: a non-secret key holding a URL still gets its userinfo
    /// password redacted.
    #[test]
    fn url_redaction_under_a_non_secret_key_is_unchanged() {
        let mut v = json!({"url": "https://user:hunter2@es:9200"});
        mask_secrets(&mut v);
        assert_eq!(v["url"], format!("https://user:{MASK}@es:9200"));
    }

    // ---------------------------------------------------------------
    // H3: the allowlist inversion, and channel config masking
    // ---------------------------------------------------------------

    #[test]
    fn a_channel_read_masks_every_credential_and_keeps_references() {
        let mut config = json!({
            "auth": {
                "type": "api_key",
                "keys": ["sk_live_1", "env://SECOND_KEY"],
                "secret": "hmac-secret",
                "secrets": ["old-secret"],
                "jwt_keys": [{"key": "-----BEGIN KEY-----", "kid": "k1"}],
                "header": "x-api-key"
            },
            "oauth2_login": {
                "client_id": "public-id",
                "client_secret": "cs",
                "state_secret": "vault://kv/state",
                "providers": {"github": {"client_id": "gh", "client_secret": "gh-secret"}}
            },
            "rate_limit": {"requests_per_second": 5}
        });
        mask_channel_config(&mut config);
        assert_eq!(config["auth"]["keys"], json!([MASK, "env://SECOND_KEY"]));
        assert_eq!(config["auth"]["secret"], MASK);
        assert_eq!(config["auth"]["secrets"], json!([MASK]));
        assert_eq!(config["auth"]["jwt_keys"][0]["key"], MASK);
        assert_eq!(config["auth"]["jwt_keys"][0]["kid"], "k1");
        assert_eq!(config["auth"]["header"], "x-api-key");
        assert_eq!(config["oauth2_login"]["client_id"], "public-id");
        assert_eq!(config["oauth2_login"]["client_secret"], MASK);
        assert_eq!(config["oauth2_login"]["state_secret"], "vault://kv/state");
        assert_eq!(
            config["oauth2_login"]["providers"]["github"]["client_secret"],
            MASK
        );
        assert_eq!(config["rate_limit"]["requests_per_second"], 5);
    }
}
