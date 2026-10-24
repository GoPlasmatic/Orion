#[allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::extensions::admission::CommunityAdmissionPolicy;
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
            package_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
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

    #[tokio::test]
    async fn memory_cache_provider_partitions_by_connection_and_revision_and_evicts() {
        let providers = ProviderRegistry::new();
        let pools = ResourcePoolRegistry::new();
        register_community_foundation_providers(&providers, &pools);
        let policy = CommunityAdmissionPolicy::new();

        // Write key "k1" on conn-a revision 1.
        let inv_set = make_invocation(DRIVER_MEMORY_CACHE, "conn-a", 1, "set");
        let res = providers
            .dispatch_attempt(
                &policy,
                &inv_set,
                ImplementationKind::Rust,
                DRIVER_MEMORY_CACHE,
                json!({"key": "k1", "value": "v1", "ttl_secs": 60}),
            )
            .await;
        assert!(matches!(res, ConnectorResult::Success { .. }));

        // Read key "k1" on conn-a revision 1 -> hit!
        let inv_get_r1 = make_invocation(DRIVER_MEMORY_CACHE, "conn-a", 1, "get");
        let res_r1 = providers
            .dispatch_attempt(
                &policy,
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
        let res_r2 = providers
            .dispatch_attempt(
                &policy,
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
        let res_b = providers
            .dispatch_attempt(
                &policy,
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
        let res_after = providers
            .dispatch_attempt(
                &policy,
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
    async fn v2_14_2_and_3_http_ssrf_guard_and_live_sqlite_execution() {
        let http = HttpProviderBackend::new();
        let inv = make_invocation(DRIVER_HTTP, "http-conn", 1, "get");
        let attempt = inv.next_attempt().unwrap();

        // Missing URL returns explicit error instead of fabricated 200 (Claim 5)
        let missing_url_err = http
            .invoke(&attempt, "get", json!({"path": "/contacts"}))
            .await
            .expect_err("HTTP invocation without URL must not fabricate 200");
        assert_eq!(missing_url_err.code, codes::INVALID_INPUT);

        // Private loopback URL is blocked by SSRF guard (`V2-14.2`)
        let ssrf_err = http
            .invoke(
                &attempt,
                "get",
                json!({"path": "/metadata", "url": "http://169.254.169.254/latest/meta-data"}),
            )
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
            .expect_err("Postgres query without connection_string must not return synthetic empty rows");
        assert_eq!(pg_missing_err.code, codes::INVALID_INPUT);

        let pg_inv_w = make_invocation(DRIVER_POSTGRESQL, "pg-conn", 1, "execute");
        let pg_attempt_w = pg_inv_w.next_attempt().unwrap();
        let pg_write_err = pg
            .invoke(
                &pg_attempt_w,
                "execute",
                json!({
                    "connection_string": "postgres://postgres:postgres@127.0.0.1:1/nonexistent",
                    "sql": "INSERT INTO items VALUES (1)"
                }),
            )
            .await
            .expect_err("Postgres write to unreachable host must not return synthetic rows_affected: 1");
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
    async fn td24_and_td29_sqlite_invoke_persists_in_memory_and_memory_cache_bounds_partitions_and_ttl_ms() {
        let sqlite = SqliteProviderBackend::default();
        let inv_create = make_invocation(DRIVER_SQLITE, "sqlite-conn", 1, "execute");
        let attempt_create = inv_create.next_attempt().unwrap();
        sqlite
            .invoke(
                &attempt_create,
                "execute",
                json!({
                    "connection_string": "sqlite::memory:",
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
                    "connection_string": "sqlite::memory:",
                    "sql": "INSERT INTO items (id, label) VALUES (7, 'persisted')"
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
                    "connection_string": "sqlite::memory:",
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
