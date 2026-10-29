use super::vault::unconfigured;
use super::*;
use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

/// Test-only resolver that returns canned values from a map. Lets us
/// exercise `resolve_in_place` without touching the real environment.
struct StubResolver {
    scheme: &'static str,
    values: std::collections::HashMap<&'static str, &'static str>,
}
#[async_trait::async_trait]
impl SecretResolver for StubResolver {
    fn scheme(&self) -> &'static str {
        self.scheme
    }
    async fn resolve(&self, reference: &str) -> Result<String, OrionError> {
        self.values
            .get(reference)
            .map(|v| (*v).to_string())
            .ok_or_else(|| OrionError::Config {
                message: format!("stub: '{reference}' not registered"),
            })
    }
}

fn stub(values: &[(&'static str, &'static str)]) -> Vec<Box<dyn SecretResolver>> {
    vec![Box::new(StubResolver {
        scheme: "env",
        values: values.iter().copied().collect(),
    })]
}

#[test]
fn parse_reference_recognizes_scheme() {
    assert_eq!(parse_reference("env://FOO"), Some(("env", "FOO")));
    assert_eq!(
        parse_reference("https://example.com"),
        Some(("https", "example.com"))
    );
}

#[test]
fn parse_reference_rejects_uppercase_scheme() {
    // Schemes are lowercase; "ENV://..." stays as a literal so it's
    // not silently resolved despite the typo.
    assert_eq!(parse_reference("ENV://FOO"), None);
}

#[tokio::test]
async fn parse_reference_returns_none_for_plain_string() {
    assert_eq!(parse_reference("plain text"), None);
    assert_eq!(parse_reference(""), None);
}

#[tokio::test]
async fn resolve_in_place_replaces_string() {
    let mut v = json!({ "token": "env://API_TOKEN" });
    resolve_in_place(&mut v, &stub(&[("API_TOKEN", "s3cret")]), "test")
        .await
        .expect("test");
    assert_eq!(v["token"], "s3cret");
}

#[tokio::test]
async fn resolve_in_place_leaves_unknown_schemes_alone() {
    // https:// has no resolver and is not reserved — must pass through
    // unchanged, or every connector URL would be mangled.
    let mut v = json!({ "url": "https://example.com/api" });
    resolve_in_place(&mut v, &stub(&[]), "test")
        .await
        .expect("test");
    assert_eq!(v["url"], "https://example.com/api");
}

#[tokio::test]
async fn reserved_scheme_errors_instead_of_becoming_the_literal_password() {
    // `aws-sm` rather than `vault`: vault has a real resolver now, so its
    // refusal is about configuration, not about the scheme. This test is
    // about the schemes with no implementation at all.
    let mut v = json!({ "auth": { "password": "aws-sm://prod/db#password" } });
    let err = resolve_in_place(&mut v, default_resolvers(), "connector 'db'")
        .await
        .expect_err("an unimplemented scheme must fail loudly, not pass through as the password");
    let OrionError::Config { message } = err else {
        unreachable!("expected Config error");
    };
    assert!(message.contains("aws-sm"), "{message}");
    assert!(message.contains("not supported"), "{message}");
    assert!(message.contains("connector 'db'"), "{message}");
}

/// The other half of the same guarantee, for the one reserved scheme that
/// *is* implemented: a process without `VAULT_ADDR`/`VAULT_TOKEN` must
/// refuse a `vault://` reference rather than hand it on as the credential.
///
/// Asserted against the refusal itself rather than by resolving through
/// the registry, because mutating the process environment mid-test is
/// unsound in a multi-threaded binary and a developer machine may well
/// have a live Vault environment.
#[test]
fn vault_without_an_environment_refuses_rather_than_passing_through() {
    let OrionError::Config { message } = unconfigured("VAULT_ADDR") else {
        unreachable!("expected Config error");
    };
    assert!(message.contains("vault://"), "{message}");
    assert!(message.contains("VAULT_ADDR"), "{message}");
}

/// The registry is the same one every call site sees — the property that
/// makes borrowing it at five call sites sound (§3.7).
#[test]
fn the_default_registry_is_built_once() {
    assert!(
        std::ptr::eq(default_resolvers(), default_resolvers()),
        "default_resolvers() must hand back one process-wide registry"
    );
}

#[tokio::test]
async fn every_reserved_scheme_is_rejected() {
    for scheme in RESERVED_SCHEMES {
        let mut v = json!({ "token": format!("{scheme}://some/path") });
        assert!(
            resolve_in_place(&mut v, default_resolvers(), "test")
                .await
                .is_err(),
            "scheme '{scheme}' must be rejected"
        );
    }
}

#[tokio::test]
async fn default_resolvers_leave_connection_urls_untouched() {
    // The reserved list must not catch ordinary connector URLs.
    let mut v = json!({
        "connection_string": "postgres://user:pass@db.internal:5432/app",
        "url": "http://api.internal:8080/v1",
        "endpoint": "https://s3.internal:9000"
    });
    resolve_in_place(&mut v, default_resolvers(), "test")
        .await
        .expect("test");
    assert_eq!(
        v["connection_string"],
        "postgres://user:pass@db.internal:5432/app"
    );
    assert_eq!(v["url"], "http://api.internal:8080/v1");
    assert_eq!(v["endpoint"], "https://s3.internal:9000");
}

#[tokio::test]
async fn resolve_in_place_recurses_into_objects() {
    let mut v = json!({
        "auth": { "type": "bearer", "token": "env://TOK" },
        "max_retries": 3
    });
    resolve_in_place(&mut v, &stub(&[("TOK", "abc")]), "test")
        .await
        .expect("test");
    assert_eq!(v["auth"]["token"], "abc");
    assert_eq!(v["max_retries"], 3);
}

#[tokio::test]
async fn resolve_in_place_recurses_into_arrays() {
    let mut v = json!({ "brokers": ["env://B1", "literal:9092"] });
    resolve_in_place(&mut v, &stub(&[("B1", "broker.local:9092")]), "test")
        .await
        .expect("test");
    assert_eq!(v["brokers"][0], "broker.local:9092");
    assert_eq!(v["brokers"][1], "literal:9092");
}

#[tokio::test]
async fn missing_env_var_errors_with_source_label() {
    let mut v = json!({ "token": "env://NOPE" });
    let err = resolve_in_place(&mut v, &stub(&[]), "connector 'foo'")
        .await
        .expect_err("test");
    let OrionError::Config { message } = err else {
        unreachable!("expected Config error");
    };
    assert!(message.contains("NOPE"));
    assert!(message.contains("connector 'foo'"));
}

#[tokio::test]
async fn env_resolver_rejects_invalid_var_name() {
    let r = EnvSecretResolver;
    assert!(r.resolve("").await.is_err());
    assert!(r.resolve("has-hyphen").await.is_err());
    assert!(r.resolve("with space").await.is_err());
}

/// An in-process fake Vault speaking just enough of the KV HTTP API:
/// token-checked, one KV v2 path, one KV v1 path. The resolver is plain
/// HTTP, so nothing about this test needs a real Vault.
async fn fake_vault() -> String {
    fn authed(headers: &HeaderMap) -> bool {
        headers.get("X-Vault-Token").is_some_and(|v| v == "t0ken")
    }
    let app = axum::Router::new()
        .route(
            "/v1/secret/data/db",
            axum::routing::get(|headers: HeaderMap| async move {
                if !authed(&headers) {
                    return (StatusCode::FORBIDDEN, Json(json!({"errors": ["denied"]})));
                }
                // KV v2 nests under data.data.
                (
                    StatusCode::OK,
                    Json(json!({"data": {"data": {"password": "hunter2"}}})),
                )
            }),
        )
        .route(
            "/v1/legacy/db",
            axum::routing::get(|headers: HeaderMap| async move {
                if !authed(&headers) {
                    return (StatusCode::FORBIDDEN, Json(json!({"errors": ["denied"]})));
                }
                // KV v1 is flat under data.
                (
                    StatusCode::OK,
                    Json(json!({"data": {"password": "legacy2"}})),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn resolves_kv2_and_kv1_shapes() {
    let addr = fake_vault().await;
    let r = VaultSecretResolver::new(&addr, "t0ken");
    assert_eq!(
        r.resolve("secret/data/db#password").await.expect("kv2"),
        "hunter2"
    );
    assert_eq!(
        r.resolve("legacy/db#password").await.expect("kv1"),
        "legacy2"
    );
}

#[tokio::test]
async fn failures_are_loud_and_tokenless() {
    let addr = fake_vault().await;

    // Wrong token: the status reaches the message, the token never does.
    let r = VaultSecretResolver::new(&addr, "wrong");
    let err = r
        .resolve("secret/data/db#password")
        .await
        .expect_err("must refuse");
    let msg = err.to_string();
    assert!(msg.contains("403"), "{msg}");
    assert!(!msg.contains("wrong"), "token must not leak: {msg}");

    // A field the secret does not carry.
    let r = VaultSecretResolver::new(&addr, "t0ken");
    let err = r
        .resolve("secret/data/db#missing_field")
        .await
        .expect_err("must refuse");
    assert!(err.to_string().contains("missing_field"));

    // A reference with no field designator.
    let err = r.resolve("secret/data/db").await.expect_err("must refuse");
    assert!(err.to_string().contains("must name a field"));
}

/// End to end through the tree walk: a vault:// reference inside a config
/// resolves like env:// always has.
#[tokio::test]
async fn resolve_in_place_uses_the_vault_resolver() {
    let addr = fake_vault().await;
    let resolvers: Vec<Box<dyn SecretResolver>> = vec![
        Box::new(EnvSecretResolver),
        Box::new(VaultSecretResolver::new(&addr, "t0ken")),
    ];
    let mut v = serde_json::json!({
        "auth": {"password": "vault://secret/data/db#password"},
        "url": "https://db.example.com"
    });
    resolve_in_place(&mut v, &resolvers, "connector 'db'")
        .await
        .expect("resolves");
    assert_eq!(v["auth"]["password"], "hunter2");
    assert_eq!(v["url"], "https://db.example.com");
}

/// A Vault endpoint that streams an unbounded chunked body — no
/// `Content-Length`, the shape a read-then-measure cap cannot stop — is
/// refused *while streaming*, so the peer never hands over the whole body.
/// Before the `http_body` cap, `response.json()` read it whole into memory
/// at load time. The load-bearing assertion is on what the server managed
/// to write, not on the error: an unbounded read produces the same size
/// error, after buffering all 8 MiB.
#[tokio::test]
async fn an_oversized_vault_response_is_refused_while_streaming() {
    const CHUNK: usize = 64 * 1024;
    const CHUNKS: usize = 128; // 8 MiB if a reader lets it all through
    let (url, server) = crate::http_body::flood_server(CHUNK, CHUNKS).await;
    // `flood_server` answers any path, so the built `/v1/<path>` URL lands.
    let r = VaultSecretResolver::new(url.trim_end_matches('/'), "t0ken");
    let err = r
        .resolve("secret/data/db#password")
        .await
        .expect_err("an unbounded body must be refused");
    assert!(
        err.to_string().contains("byte limit"),
        "the refusal must be the size cap: {err}"
    );
    crate::http_body::assert_stopped_early(
        server.await.expect("test server"),
        CHUNK * CHUNKS,
        "vault resolver",
    );
}

fn env_resolvers() -> Vec<Box<dyn SecretResolver>> {
    vec![Box::new(EnvSecretResolver)]
}

fn load(
    stored: Value,
    resolved: Value,
) -> Result<crate::connector::ConnectorConfig, ResolvedParseError> {
    let sites = reference_sites(&stored, &env_resolvers());
    crate::connector::ConnectorConfig::parse_variant(
        crate::connector::ConnectorType::Http,
        &resolved,
        &ResolvedReferences { sites: &sites },
    )
}

#[test]
fn parse_resolved_coerces_true_and_false_at_sites_only() {
    let stored = json!({"url": "env://URL", "allow_private_urls": "env://PRIVATE"});
    for (text, expected) in [("true", true), (" FALSE\n", false), ("True", true)] {
        let resolved = json!({"url": "http://peer:8080", "allow_private_urls": text});
        match load(stored.clone(), resolved).expect("coerced") {
            crate::connector::ConnectorConfig::Http(http) => {
                assert_eq!(http.allow_private_urls, expected, "{text:?}");
                assert_eq!(http.url, "http://peer:8080");
            }
            other => unreachable!("{other:?}"),
        }
    }
    // A literal the author wrote as a string is theirs, not a reference.
    let literal = json!({"url": "http://peer", "allow_private_urls": "true"});
    let err = load(literal.clone(), literal).expect_err("not a site");
    assert!(matches!(err, ResolvedParseError::Shape(_)), "{err:?}");
}

#[test]
fn a_non_boolean_resolution_names_the_field_and_not_the_value() {
    let stored = json!({"url": "http://peer", "allow_private_urls": "env://PRIVATE"});
    let resolved = json!({"url": "http://peer", "allow_private_urls": "s3cr3t-token"});
    let err = load(stored, resolved).expect_err("garbage");
    assert_eq!(
        err,
        ResolvedParseError::NotABoolean {
            field: "allow_private_urls".to_string()
        }
    );
    let message = err.to_string();
    assert!(message.contains("allow_private_urls"), "{message}");
    assert!(!message.contains("s3cr3t"), "{message}");
}

#[test]
fn embedded_references_are_found_and_whole_ones_are_not() {
    for (text, expected) in [
        ("Bearer env://API_KEY", Some("env")),
        ("x vault://secret/a#b", Some("vault")),
        ("token=(env://K)", Some("env")),
        ("env://API_KEY", None),
        ("someenv://k", None),
        ("${X:-env://Y}", None),
        ("https://h/p", None),
        ("see env:// ", None),
    ] {
        assert_eq!(embedded_reference(text), expected, "{text:?}");
    }
    let found = embedded_references(&json!({
        "url": "https://api.example.com",
        "headers": {"Authorization": "Bearer env://API_KEY", "X-Other": "k env://K"},
        "auth": {"type": "bearer", "token": "env://API_KEY"},
    }));
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0].path, "headers.Authorization");
    assert!(found[0].authorization_header);
    assert!(found[0].remedy().contains("\"auth\""));
    assert_eq!(found[1].path, "headers.X-Other");
    assert!(!found[1].authorization_header);
}
