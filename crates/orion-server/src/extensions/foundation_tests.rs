use super::*;
use crate::extensions::context::{AdmittedRun, AuthorityGrant, BudgetLimits};
use crate::extensions::contract::{ConnectionReference, ConnectorResult, ProviderReference};
use std::collections::BTreeSet;

fn make_invocation(
    driver_id: &str,
    conn_id: &str,
    revision: u64,
    op: &str,
) -> crate::extensions::context::Invocation {
    let provider = ProviderReference {
        provider_id: driver_id.to_string(),
        version: "2.0.0".to_string(),
        package_digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .to_string(),
    };
    let conn = ConnectionReference {
        connection_id: conn_id.to_string(),
        revision,
        provider,
    };
    let eff_id = EffectiveConnectionIdentity {
        provider_digest: conn.provider.package_digest.clone(),
        connection_id: conn.connection_id.clone(),
        revision,
        secret_hash: "sha256:1234".to_string(),
    };
    let mut allowed_operations = BTreeMap::new();
    allowed_operations.insert(conn_id.to_string(), BTreeSet::from([op.to_string()]));
    let run = AdmittedRun::admit(
        "run-foundation",
        1,
        Instant::now() + Duration::from_secs(30),
        BudgetLimits::default(),
        AuthorityGrant {
            allowed_operations,
            declared_dependencies: BTreeMap::new(),
            allowed_channels: BTreeSet::new(),
        },
    );
    run.context()
        .start_invocation(conn, eff_id, op, None, 3)
        .expect("invocation starts")
}

/// A store holding revision 1 of `conn_id` with `public_config` and the
/// `(path, reference)` secret locators.
fn with_connection(
    conn_id: &str,
    public_config: Value,
    secret_locators: &[(&str, &str)],
) -> crate::extensions::ImmutableDeploymentStore {
    let store = crate::extensions::ImmutableDeploymentStore::default();
    store
        .store_connection_revision(
            conn_id,
            1,
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            public_config,
            secret_locators
                .iter()
                .map(|(path, reference)| (path.to_string(), reference.to_string()))
                .collect(),
        )
        .expect("connection revision stored");
    store
}

/// A query's rows are capped while they stream, and a `secret_locator`
/// that is not a reference is refused rather than read as a literal.
#[tokio::test]
async fn sql_rows_are_capped_and_a_secret_locator_must_be_a_reference() {
    let sqlite = SqliteProviderBackend::default().with_limits(SqlLimits {
        max_rows: 2,
        max_result_bytes: 0,
    });
    let err = sqlite
        .execute_live_sqlite(
            "sqlite::memory:",
            "query",
            "SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3",
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert!(err.message.contains("2 rows"), "{}", err.message);

    // A literal where a reference belongs is refused when the revision is
    // stored, so it never reaches a provider.
    let refused = crate::extensions::ImmutableDeploymentStore::default()
        .store_connection_revision(
            "pg-literal",
            1,
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            json!({}),
            [(
                "connection_string".to_string(),
                "postgres://u:p@db/app".to_string(),
            )]
            .into(),
        )
        .expect_err("a literal locator is refused");
    assert_eq!(refused.code, "CONNECTION_SECRET_LOCATOR_INVALID");
}

/// Each built-in driver answers to one stable digest, which maps back to
/// it, and an id this build does not ship has none.
#[test]
fn a_builtin_driver_has_one_stable_digest() {
    for driver in COMMUNITY_RUST_DRIVERS {
        let digest = builtin_provider_digest(driver).expect("a built-in driver");
        assert!(
            digest.starts_with("sha256:") && digest.len() == 71,
            "{digest}"
        );
        assert_eq!(builtin_driver(&digest), Some(*driver));
        assert_eq!(builtin_provider_digest(driver), Some(digest));
    }
    let distinct: BTreeSet<String> = COMMUNITY_RUST_DRIVERS
        .iter()
        .filter_map(|d| builtin_provider_digest(d))
        .collect();
    assert_eq!(distinct.len(), COMMUNITY_RUST_DRIVERS.len());
    assert_eq!(builtin_provider_digest("com.example.other"), None);
    assert_eq!(
        builtin_driver("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        None
    );
}

#[test]
fn sqlite_provider_enforces_approved_root_and_rejects_traversal_and_orion_db() {
    let sqlite = SqliteProviderBackend::new("/var/lib/orion/sqlite", 4);

    // In-memory is allowed.
    assert!(
        sqlite
            .validate_config(&json!({"connection_string": "sqlite::memory:"}))
            .is_ok()
    );

    // Relative path under approved_root is allowed.
    let resolved = sqlite
        .resolve_sandboxed_target("sqlite://tenant_a/app.db")
        .expect("valid relative path");
    assert_eq!(resolved, "sqlite:///var/lib/orion/sqlite/tenant_a/app.db");

    // Path traversal `..` is rejected.
    let err = sqlite
        .validate_config(&json!({"connection_string": "sqlite://../secrets.db"}))
        .expect_err("traversal must fail");
    assert_eq!(err.code, codes::CAPABILITY_INVALID);

    // Absolute path is rejected.
    let err = sqlite
        .validate_config(&json!({"connection_string": "sqlite:///etc/passwd"}))
        .expect_err("absolute path must fail");
    assert_eq!(err.code, codes::CAPABILITY_INVALID);

    // Reserved Orion state DB filename is rejected.
    let err = sqlite
        .validate_config(&json!({"connection_string": "sqlite://orion.db"}))
        .expect_err("reserved orion.db must fail");
    assert_eq!(err.code, codes::CAPABILITY_INVALID);
}

#[test]
fn postgres_provider_accepts_only_postgres_schemes() {
    let pg = PostgresProviderBackend::default();
    assert!(
        pg.validate_config(&json!({"connection_string": "postgres://user:pass@db/app"}))
            .is_ok()
    );
    assert!(
        pg.validate_config(&json!({"connection_string": "postgresql://user:pass@db/app"}))
            .is_ok()
    );
    assert!(
        pg.validate_config(&json!({"connection_string": "mysql://user:pass@db/app"}))
            .is_err()
    );
}

#[test]
fn mysql_provider_accepts_only_mysql_and_mariadb_schemes() {
    let mysql = MysqlProviderBackend::default();
    for ok in [
        "mysql://user:pass@db/app",
        "mariadb://user:pass@db/app",
        "  MySQL://db/app",
    ] {
        assert!(
            mysql
                .validate_config(&json!({"connection_string": ok}))
                .is_ok(),
            "{ok}"
        );
    }
    for bad in [
        json!({"connection_string": "postgres://user:pass@db/app"}),
        json!({"connection_string": "sqlite::memory:"}),
        json!({"connection_string": "db/app"}),
        json!({}),
    ] {
        let err = mysql.validate_config(&bad).expect_err("refused");
        assert_eq!(err.code, codes::INVALID_INPUT, "{bad}");
    }
}

/// MySQL refuses a target in the input, an unknown operation, and a
/// statement that is not a string, and names its statement keys as the
/// literals `connector_call` checks at load.
#[tokio::test]
async fn mysql_provider_refuses_input_targets_and_non_literal_statements() {
    let mysql = MysqlProviderBackend::default();
    assert_eq!(mysql.literal_input_keys("query"), SQL_STATEMENT_KEYS);
    assert_eq!(mysql.literal_input_keys("execute"), SQL_STATEMENT_KEYS);

    let inv = make_invocation(DRIVER_MYSQL, "mysql-conn", 1, "query");
    let attempt = inv.next_attempt().unwrap();
    let err = mysql
        .invoke(
            &attempt,
            "query",
            json!({"connection_string": "mysql://evil.example/db", "sql": "SELECT 1"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::CONNECTION_FORBIDDEN);

    let err = mysql
        .invoke(&attempt, "query", json!({"sql": {"var": "data.q"}}))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);

    let err = mysql
        .invoke(&attempt, "drop", json!({"sql": "SELECT 1"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::OPERATION_FORBIDDEN);

    // Without a governed connection string the call is refused, never
    // answered with synthetic rows.
    let err = mysql
        .invoke(&attempt, "query", json!({"sql": "SELECT 1"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);

    // A revision naming another backend's scheme is refused before connecting.
    let mysql = MysqlProviderBackend::with_deployment_store(
        4,
        with_connection(
            "mysql-conn",
            json!({"connection_string": "postgres://127.0.0.1:1/app"}),
            &[],
        ),
    );
    let err = mysql
        .invoke(&attempt, "query", json!({"sql": "SELECT 1"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_INPUT);

    // An unreachable server is a transport failure, which is retryable.
    let mysql = MysqlProviderBackend::with_deployment_store(
        4,
        with_connection(
            "mysql-conn",
            json!({"connection_string": "mysql://127.0.0.1:1/app"}),
            &[],
        ),
    );
    let inv = make_invocation(DRIVER_MYSQL, "mysql-conn", 1, "execute");
    let attempt = inv.next_attempt().unwrap();
    let err = mysql
        .invoke(
            &attempt,
            "execute",
            json!({"sql": "INSERT INTO items VALUES (?)", "params": [1]}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::UNAVAILABLE);
}

#[tokio::test]
async fn memory_cache_provider_partitions_by_connection_and_revision_and_evicts() {
    let comp = crate::extensions::CompositionRoot::community()
        .build_in_memory()
        .unwrap();
    let providers = comp.providers().clone();
    let pools = comp.pools().clone();

    // Write key "k1" on conn-a revision 1.
    let inv_set = make_invocation(DRIVER_MEMORY_CACHE, "conn-a", 1, "set");
    let res = dispatch(
        &providers,
        &inv_set,
        ImplementationKind::Rust,
        DRIVER_MEMORY_CACHE,
        json!({"key": "k1", "value": "v1", "ttl_secs": 60}),
    )
    .await;
    assert!(matches!(res, ConnectorResult::Success { .. }));

    // Read key "k1" on conn-a revision 1 -> hit!
    let inv_get_r1 = make_invocation(DRIVER_MEMORY_CACHE, "conn-a", 1, "get");
    let res_r1 = dispatch(
        &providers,
        &inv_get_r1,
        ImplementationKind::Rust,
        DRIVER_MEMORY_CACHE,
        json!({"key": "k1"}),
    )
    .await;
    assert_eq!(
        res_r1,
        ConnectorResult::Success {
            data: json!({"hit": true, "value": "v1"}),
            metadata: BTreeMap::new(),
        }
    );

    // Read key "k1" on conn-a revision 2 -> isolated (miss)!
    let inv_get_r2 = make_invocation(DRIVER_MEMORY_CACHE, "conn-a", 2, "get");
    let res_r2 = dispatch(
        &providers,
        &inv_get_r2,
        ImplementationKind::Rust,
        DRIVER_MEMORY_CACHE,
        json!({"key": "k1"}),
    )
    .await;
    assert_eq!(
        res_r2,
        ConnectorResult::Success {
            data: json!({"hit": false, "value": Value::Null}),
            metadata: BTreeMap::new(),
        }
    );

    // Read key "k1" on conn-b revision 1 -> isolated (miss)!
    let inv_get_b = make_invocation(DRIVER_MEMORY_CACHE, "conn-b", 1, "get");
    let res_b = dispatch(
        &providers,
        &inv_get_b,
        ImplementationKind::Rust,
        DRIVER_MEMORY_CACHE,
        json!({"key": "k1"}),
    )
    .await;
    assert_eq!(
        res_b,
        ConnectorResult::Success {
            data: json!({"hit": false, "value": Value::Null}),
            metadata: BTreeMap::new(),
        }
    );

    // Evicting "conn-a" via ResourcePoolRegistry clears revision 1.
    pools.evict_connector("conn-a").await;
    let inv_get_after = make_invocation(DRIVER_MEMORY_CACHE, "conn-a", 1, "get");
    let res_after = dispatch(
        &providers,
        &inv_get_after,
        ImplementationKind::Rust,
        DRIVER_MEMORY_CACHE,
        json!({"key": "k1"}),
    )
    .await;
    assert_eq!(
        res_after,
        ConnectorResult::Success {
            data: json!({"hit": false, "value": Value::Null}),
            metadata: BTreeMap::new(),
        }
    );
}

#[tokio::test]
async fn http_ssrf_guard_and_live_sqlite_execution() {
    let http = HttpProviderBackend::new();
    let inv = make_invocation(DRIVER_HTTP, "http-conn", 1, "get");
    let attempt = inv.next_attempt().unwrap();

    // Missing URL returns explicit error instead of fabricated 200 (Claim 5)
    let missing_url_err = http
        .invoke(&attempt, "get", json!({"path": "/contacts"}))
        .await
        .expect_err("HTTP invocation without URL must not fabricate 200");
    assert_eq!(missing_url_err.code, codes::INVALID_INPUT);

    // A target in the input is refused: it comes from the connection revision.
    let input_target_err = http
        .invoke(
            &attempt,
            "get",
            json!({"path": "/contacts", "url": "https://api.example.com"}),
        )
        .await
        .unwrap_err();
    assert_eq!(input_target_err.code, codes::CONNECTION_FORBIDDEN);

    // A private address the connection revision names is blocked by the SSRF guard (`V2-14.2`)
    let store = with_connection(
        "http-conn",
        json!({"url": "http://169.254.169.254/latest"}),
        &[],
    );
    let http = HttpProviderBackend::with_deployment_store(store);
    let ssrf_err = http
        .invoke(&attempt, "get", json!({"path": "/meta-data"}))
        .await
        .unwrap_err();
    assert_eq!(ssrf_err.code, codes::CAPABILITY_INVALID);

    // PostgreSQL without connection_string or with unreachable host returns explicit error instead of synthetic success (Claim 5)
    let pg = PostgresProviderBackend::default();
    let pg_inv_q = make_invocation(DRIVER_POSTGRESQL, "pg-conn", 1, "query");
    let pg_attempt_q = pg_inv_q.next_attempt().unwrap();
    let pg_missing_err = pg
        .invoke(&pg_attempt_q, "query", json!({"sql": "SELECT 1"}))
        .await
        .expect_err(
            "Postgres query without connection_string must not return synthetic empty rows",
        );
    assert_eq!(pg_missing_err.code, codes::INVALID_INPUT);

    // A connection string in the input is refused.
    let pg_input_err = pg
        .invoke(
            &pg_attempt_q,
            "query",
            json!({"connection_string": "postgres://evil.example/db", "sql": "SELECT 1"}),
        )
        .await
        .unwrap_err();
    assert_eq!(pg_input_err.code, codes::CONNECTION_FORBIDDEN);

    let pg = PostgresProviderBackend::with_deployment_store(
        4,
        with_connection(
            "pg-conn",
            json!({"connection_string": "postgres://127.0.0.1:1/nonexistent"}),
            &[],
        ),
    );
    let pg_inv_w = make_invocation(DRIVER_POSTGRESQL, "pg-conn", 1, "execute");
    let pg_attempt_w = pg_inv_w.next_attempt().unwrap();
    let pg_write_err = pg
        .invoke(
            &pg_attempt_w,
            "execute",
            json!({"sql": "INSERT INTO items VALUES ($1)", "params": [1]}),
        )
        .await
        .expect_err(
            "Postgres write to unreachable host must not return synthetic rows_affected: 1",
        );
    assert_eq!(pg_write_err.code, codes::UNAVAILABLE);

    // Live SQLite execution against sqlite::memory: (`V2-14.3`)
    let sqlite = SqliteProviderBackend::default();
    let out = sqlite
        .execute_live_sqlite("sqlite::memory:", "query", "SELECT 'orion-v2' AS edition")
        .await
        .unwrap();
    assert_eq!(out["count"], 1);
    assert_eq!(out["rows"][0]["edition"], "orion-v2");

    // SQLite errors are never swallowed into synthetic rows: [] or rows_affected: 1 (Claim 5)
    let sq_inv = make_invocation(DRIVER_SQLITE, "sqlite-err-conn", 1, "execute");
    let sq_attempt = sq_inv.next_attempt().unwrap();
    let sq_err = sqlite
            .invoke(
                &sq_attempt,
                "execute",
                json!({"sql": "INSERT INTO missing_table VALUES (1)"}),
            )
            .await
            .expect_err("SQLite error without explicit connection_string must not return synthetic rows_affected: 1");
    assert_eq!(sq_err.code, codes::INVALID_INPUT);
}

#[tokio::test]
async fn sqlite_invoke_persists_in_memory_and_memory_cache_bounds_partitions_and_ttl_ms() {
    let sqlite = SqliteProviderBackend::default();
    let inv_create = make_invocation(DRIVER_SQLITE, "sqlite-conn", 1, "execute");
    let attempt_create = inv_create.next_attempt().unwrap();
    sqlite
        .invoke(
            &attempt_create,
            "execute",
            json!({
                "sql": "CREATE TABLE items (id INTEGER PRIMARY KEY, label TEXT)"
            }),
        )
        .await
        .expect("create table in shared memory pool");

    let inv_insert = make_invocation(DRIVER_SQLITE, "sqlite-conn", 1, "execute");
    let attempt_insert = inv_insert.next_attempt().unwrap();
    let ins_out = sqlite
        .invoke(
            &attempt_insert,
            "execute",
            json!({
                "sql": "INSERT INTO items (id, label) VALUES (?, ?)",
                "params": [7, "persisted"]
            }),
        )
        .await
        .expect("insert into shared memory pool");
    assert_eq!(ins_out.data["rows_affected"], 1);

    let inv_select = make_invocation(DRIVER_SQLITE, "sqlite-conn", 1, "query");
    let attempt_select = inv_select.next_attempt().unwrap();
    let sel_out = sqlite
        .invoke(
            &attempt_select,
            "query",
            json!({
                "sql": "SELECT id, label FROM items"
            }),
        )
        .await
        .expect("query from shared memory pool");
    assert_eq!(sel_out.data["count"], 1);
    assert_eq!(sel_out.data["rows"][0]["id"], 7);
    assert_eq!(sel_out.data["rows"][0]["label"], "persisted");

    // TD-29: PartitionedMemoryCache bounds total partitions, drops empty partitions, and honors ttl_ms.
    let cache = PartitionedMemoryCache::with_limits(4, 2, 128);
    let id1 = EffectiveConnectionIdentity {
        provider_digest: "sha256:1".to_string(),
        connection_id: "c1".to_string(),
        revision: 1,
        secret_hash: "s1".to_string(),
    };
    let id2 = EffectiveConnectionIdentity {
        provider_digest: "sha256:1".to_string(),
        connection_id: "c2".to_string(),
        revision: 1,
        secret_hash: "s1".to_string(),
    };
    let id3 = EffectiveConnectionIdentity {
        provider_digest: "sha256:1".to_string(),
        connection_id: "c3".to_string(),
        revision: 1,
        secret_hash: "s1".to_string(),
    };

    cache.set(&id1, "k".to_string(), json!("v1"), None);
    assert_eq!(cache.partition_count(), 1);
    assert!(cache.delete(&id1, "k"));
    assert_eq!(
        cache.partition_count(),
        0,
        "deleting last key must remove empty partition"
    );

    cache.set(&id1, "k".to_string(), json!("v1"), None);
    cache.set(&id2, "k".to_string(), json!("v2"), None);
    cache.set(&id3, "k".to_string(), json!("v3"), None);
    assert_eq!(
        cache.partition_count(),
        2,
        "total partitions must not exceed max_partitions (2)"
    );

    // MemoryCacheProviderBackend rejects invalid ttl_ms <= 0.
    let mem_backend = MemoryCacheProviderBackend::default();
    let inv_bad_ttl = make_invocation(DRIVER_MEMORY_CACHE, "cache-conn", 1, "set");
    let attempt_bad_ttl = inv_bad_ttl.next_attempt().unwrap();
    let ttl_err = mem_backend
        .invoke(
            &attempt_bad_ttl,
            "set",
            json!({"key": "k", "value": "v", "ttl_ms": 0}),
        )
        .await
        .expect_err("zero ttl_ms must fail");
    assert_eq!(ttl_err.code, codes::INVALID_INPUT);
}

/// An HTTP connection's `auth` is read strictly: a known type, the public
/// fields that type needs, no secret field, and a header name that is not
/// the request's framing.
#[test]
fn http_auth_is_read_strictly() {
    let auth = |value: Value| http_auth(&json!({"url": "https://x.example", "auth": value}));
    assert_eq!(
        auth(json!({"type": "bearer"})).unwrap(),
        Some(HttpAuth::Bearer)
    );
    assert_eq!(
        auth(json!({"type": "api_key", "header": "X-API-Key"})).unwrap(),
        Some(HttpAuth::ApiKey(reqwest::header::HeaderName::from_static(
            "x-api-key"
        )))
    );
    assert_eq!(
        auth(json!({"type": "basic", "username": "svc"})).unwrap(),
        Some(HttpAuth::Basic {
            username: "svc".to_string()
        })
    );
    assert_eq!(
        http_auth(&json!({"url": "https://x.example"})).unwrap(),
        None
    );
    for bad in [
        json!("bearer"),
        json!({"type": "digest"}),
        json!({"type": "bearer", "token": "inline"}),
        json!({"type": "api_key"}),
        json!({"type": "api_key", "header": "not a header"}),
        json!({"type": "api_key", "header": "Host"}),
        json!({"type": "basic"}),
    ] {
        let err = auth(bad.clone()).expect_err("refused");
        assert_eq!(err.code, codes::INVALID_INPUT, "{bad}");
    }
    // The provider's config check reads it too.
    assert!(
        HttpProviderBackend::new()
            .validate_config(&json!({"url": "https://x.example", "auth": {"type": "digest"}}))
            .is_err()
    );
}

/// The secret goes where `auth` says, and a header carrying it is marked
/// sensitive.
#[test]
fn http_auth_puts_the_secret_where_it_says() {
    let client = reqwest::Client::new();
    let built = |auth: &HttpAuth| {
        authorize(client.get("https://x.example/"), auth, "s3cret")
            .unwrap()
            .build()
            .unwrap()
    };
    let bearer = built(&HttpAuth::Bearer);
    assert_eq!(bearer.headers()["authorization"], "Bearer s3cret");
    assert!(bearer.headers()["authorization"].is_sensitive());

    let header = built(&HttpAuth::ApiKey(reqwest::header::HeaderName::from_static(
        "x-api-key",
    )));
    assert_eq!(header.headers()["x-api-key"], "s3cret");
    assert!(header.headers()["x-api-key"].is_sensitive());

    let basic = built(&HttpAuth::Basic {
        username: "svc".to_string(),
    });
    // base64("svc:s3cret")
    assert_eq!(basic.headers()["authorization"], "Basic c3ZjOnMzY3JldA==");
}

/// An `auth` whose credential has no secret locator is refused before the
/// call leaves the node.
#[tokio::test]
async fn http_auth_needs_a_secret_locator_for_its_credential() {
    let inv = make_invocation(DRIVER_HTTP, "http-conn", 1, "get");
    let attempt = inv.next_attempt().unwrap();
    for (config, locators) in [
        (
            json!({"url": "https://api.example.com", "auth": {"type": "bearer"}}),
            vec![],
        ),
        // A locator at another scheme's path does not stand in.
        (
            json!({"url": "https://api.example.com", "auth": {"type": "bearer"}}),
            vec![("auth.password", "env://ORION_TEST_HTTP_TOKEN")],
        ),
    ] {
        let http = HttpProviderBackend::with_deployment_store(with_connection(
            "http-conn",
            config,
            &locators,
        ));
        let err = http
            .invoke(&attempt, "get", json!({"path": "/"}))
            .await
            .expect_err("refused");
        assert_eq!(err.code, codes::INVALID_INPUT);
        assert!(
            err.message.contains("no secret locator for 'auth.token'"),
            "{}",
            err.message
        );
    }
}

/// A connection's configuration as a provider reads it: each locator placed
/// at its path; and a locator is refused where it would shadow a public
/// value or is not a dotted path.
#[test]
fn locators_are_placed_at_their_paths() {
    let store = with_connection(
        "c",
        json!({"url": "https://x.example", "auth": {"type": "bearer"}}),
        &[
            ("auth.token", "env://TOKEN"),
            ("extra.deep.key", "vault://k#v"),
        ],
    );
    let row = store.get_connection("c", 1).expect("stored");
    assert_eq!(
        row.config_with_locators(),
        json!({
            "url": "https://x.example",
            "auth": {"type": "bearer", "token": "env://TOKEN"},
            "extra": {"deep": {"key": "vault://k#v"}},
        })
    );
    for (public, path) in [
        (
            json!({"auth": {"type": "basic", "username": "u"}}),
            "auth.username",
        ),
        (json!({}), "not a path"),
        (json!({}), "trailing."),
    ] {
        let err = crate::extensions::ImmutableDeploymentStore::default()
            .store_connection_revision(
                "c",
                1,
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                public,
                [(path.to_string(), "env://X".to_string())].into(),
            )
            .expect_err("refused");
        assert_eq!(err.code, "CONNECTION_SECRET_LOCATOR_INVALID", "{path}");
    }
}

/// One attempt on the backend registered as `(kind, driver_id)`.
async fn dispatch(
    providers: &crate::extensions::ProviderRegistry,
    invocation: &crate::extensions::Invocation,
    kind: ImplementationKind,
    driver_id: &str,
    input: Value,
) -> ConnectorResult {
    let backend = providers.get(kind, driver_id).expect("registered");
    providers
        .dispatch_attempt(invocation, backend.as_ref(), input)
        .await
}
