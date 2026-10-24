use super::*;
use crate::extensions::contract::ProviderReference;
use std::time::Duration;

fn sample_connection() -> (ConnectionReference, EffectiveConnectionIdentity) {
    let provider = ProviderReference {
        provider_id: "orion.http".to_string(),
        version: "1.0.0".to_string(),
        package_digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
            .to_string(),
    };
    let conn = ConnectionReference {
        connection_id: "conn-primary".to_string(),
        revision: 1,
        provider,
    };
    let id = EffectiveConnectionIdentity {
        provider_digest: conn.provider.package_digest.clone(),
        connection_id: conn.connection_id.clone(),
        revision: conn.revision,
        secret_hash: "sha256:aaaa".to_string(),
    };
    (conn, id)
}

fn sample_authority() -> AuthorityGrant {
    let (conn, _) = sample_connection();
    let mut ops = BTreeMap::new();
    ops.insert(
        conn.connection_id.clone(),
        BTreeSet::from(["get".to_string(), "post".to_string()]),
    );
    let mut deps = BTreeMap::new();
    deps.insert(
        "upstream_http".to_string(),
        (conn, BTreeSet::from(["get".to_string()])),
    );
    AuthorityGrant {
        allowed_operations: ops,
        declared_dependencies: deps,
        allowed_channels: BTreeSet::from(["orders".to_string()]),
    }
}

#[test]
fn ctx_01_and_06_child_inherits_generation_clamps_deadline_and_reduces_authority() {
    let now = Instant::now();
    let parent_deadline = now + Duration::from_secs(10);
    let run = AdmittedRun::admit(
        "run-1",
        41,
        parent_deadline,
        BudgetLimits::default(),
        sample_authority(),
    );
    let parent_ctx = run.context();

    // Simulate child requesting a longer deadline (60s) and broader operations ("delete").
    let mut broader = sample_authority();
    broader
        .allowed_operations
        .get_mut("conn-primary")
        .expect("exists")
        .insert("delete".to_string());

    let child_ctx = parent_ctx
        .derive_child(
            now + Duration::from_secs(60),
            BudgetLimits {
                max_attempts: 100, // exceeds parent's 32
                ..BudgetLimits::default()
            },
            &broader,
        )
        .expect("child context derived");

    assert_eq!(
        child_ctx.generation_id(),
        41,
        "must keep pinned generation 41"
    );
    assert_eq!(
        child_ctx.deadline(),
        parent_deadline,
        "child deadline must clamp to parent deadline"
    );
    assert_eq!(
        child_ctx.budget().limits().max_attempts,
        32,
        "child budget ceiling cannot exceed parent ceiling"
    );
    let child_ops = child_ctx
        .authority()
        .allowed_operations
        .get("conn-primary")
        .expect("exists");
    assert!(
        !child_ops.contains("delete"),
        "child cannot gain 'delete' authority absent from parent"
    );
}

#[test]
fn ctx_02_and_03_atomic_reservation_prevents_overspend_and_partial_debits() {
    let scope = BudgetScope::new_root(BudgetLimits {
        max_attempts: 5,
        max_tasks: 5,
        max_network_sends: 1,
        max_bytes: 1000,
        max_active_jobs: 2,
        max_depth: 4,
    });
    let child = scope.derive_child(BudgetLimits {
        max_attempts: 5,
        max_tasks: 5,
        max_network_sends: 1,
        max_bytes: 500, // tighter child byte ceiling
        max_active_jobs: 2,
        max_depth: 4,
    });

    // CTX-03: Charge fits root bytes (1000) and attempts (5) but exceeds child bytes (500).
    let err = child
        .reserve(
            ChargeVector {
                attempts: 1,
                tasks: 1,
                network_sends: 0,
                bytes: 600,
                active_jobs: 0,
            },
            EffectStatus::NotStarted,
        )
        .expect_err("must reject vector exceeding child byte limit");
    assert_eq!(err.code, codes::BUDGET_EXHAUSTED);
    assert_eq!(
        scope.snapshot().spent_attempts,
        0,
        "failed reservation must leave zero partial debit on root"
    );

    // CTX-02: Two siblings race for the single remaining network_sends unit (1).
    let sib_a = scope.derive_child(BudgetLimits::default());
    let sib_b = scope.derive_child(BudgetLimits::default());

    let res_a = sib_a
        .reserve(ChargeVector::network_job(100), EffectStatus::NotStarted)
        .expect("first sibling reserves the 1 network send");
    let _lease_a = res_a.commit_started();

    let err_b = sib_b
        .reserve(ChargeVector::network_job(100), EffectStatus::NotStarted)
        .expect_err("second sibling must be rejected");
    assert_eq!(err_b.code, codes::BUDGET_EXHAUSTED);
}

#[tokio::test]
async fn ctx_04_unstarted_reservation_rolls_back_and_running_job_retains_occupancy() {
    let scope = BudgetScope::new_root(BudgetLimits {
        max_attempts: 4,
        max_tasks: 4,
        max_network_sends: 4,
        max_bytes: 1000,
        max_active_jobs: 1,
        max_depth: 4,
    });

    // Unstarted reservation aborted before start rolls back both cumulative and occupancy.
    let unstarted = scope
        .reserve(ChargeVector::network_job(200), EffectStatus::NotStarted)
        .expect("reserves");
    assert_eq!(scope.snapshot().active_jobs, 1);
    assert_eq!(scope.snapshot().spent_network_sends, 1);
    unstarted.abort_before_start();
    assert_eq!(scope.snapshot().active_jobs, 0);
    assert_eq!(scope.snapshot().spent_network_sends, 0);

    // Started job retains occupancy after caller timeout until actual worker completion.
    let registry = HostJobRegistry::new();
    let started_res = scope
        .reserve(ChargeVector::network_job(200), EffectStatus::NotStarted)
        .expect("reserves");
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let job = registry.spawn_owned(started_res, async move {
        let _ = release_rx.await;
        42usize
    });

    // Caller awaits with an already-expired deadline.
    let expired_run = AdmittedRun::admit(
        "run-timeout",
        1,
        Instant::now()
            .checked_sub(Duration::from_millis(10))
            .unwrap_or_else(Instant::now),
        BudgetLimits::default(),
        sample_authority(),
    );
    let err = job
        .await_with_context(&expired_run.context(), EffectStatus::Unknown)
        .await
        .expect_err("caller times out");
    assert_eq!(err.code, codes::DEADLINE_EXCEEDED);
    assert_eq!(err.effect_status, EffectStatus::Unknown);

    // Worker is still blocked -> occupancy is still 1!
    assert_eq!(scope.snapshot().active_jobs, 1);
    assert_eq!(registry.active_jobs(), 1);

    // Unblock worker and allow completion.
    let _ = release_tx.send(());
    for _ in 0..20 {
        if scope.snapshot().active_jobs == 0 && registry.active_jobs() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        scope.snapshot().active_jobs,
        0,
        "occupancy released on actual completion"
    );
    assert_eq!(
        scope.snapshot().spent_network_sends,
        1,
        "cumulative network_sends remains spent"
    );
}

#[test]
fn ctx_05_forged_cross_invocation_wrong_kind_and_retired_handles_rejected() {
    let run = AdmittedRun::admit(
        "run-cap",
        7,
        Instant::now() + Duration::from_secs(30),
        BudgetLimits::default(),
        sample_authority(),
    );
    let (conn, eff_id) = sample_connection();
    let inv = run
        .context()
        .start_invocation(conn, eff_id.clone(), "get", None, 3)
        .expect("invocation starts");

    // Record an earlier uncertain effect and verify capability failure preserves it.
    inv.record_effect(EffectStatus::Unknown);

    let handle = inv.capabilities().mint(CapabilityGrant {
        kind: ResourceKind::Http,
        connection: eff_id.clone(),
        allowed_operations: BTreeSet::from(["get".to_string()]),
        allowed_endpoints: BTreeSet::from(["https://api.example.com".to_string()]),
    });

    // Valid borrow succeeds.
    assert!(
        inv.capabilities()
            .validate_borrow(
                &handle,
                ResourceKind::Http,
                Some("get"),
                Some("https://api.example.com"),
                inv.effect_status(),
            )
            .is_ok()
    );

    // Wrong kind rejected with CONNECTOR_CAPABILITY_INVALID and preserves Unknown effect.
    let err = inv
        .capabilities()
        .validate_borrow(
            &handle,
            ResourceKind::Sqlite,
            None,
            None,
            inv.effect_status(),
        )
        .expect_err("wrong kind must fail");
    assert_eq!(err.code, codes::CAPABILITY_INVALID);
    assert_eq!(err.effect_status, EffectStatus::Unknown);

    // Forged handle rejected.
    let forged = CapabilityHandle::forged_for_test(
        handle.entry_id,
        inv.invocation_id(),
        7,
        ResourceKind::Http,
    );
    let err = inv
        .capabilities()
        .validate_borrow(&forged, ResourceKind::Http, None, None, inv.effect_status())
        .expect_err("forged token must fail");
    assert_eq!(err.code, codes::CAPABILITY_INVALID);

    // Retired handle rejected.
    inv.capabilities().retire(&handle);
    let err = inv
        .capabilities()
        .validate_borrow(&handle, ResourceKind::Http, None, None, inv.effect_status())
        .expect_err("retired handle must fail");
    assert_eq!(err.code, codes::CAPABILITY_INVALID);
}

#[test]
fn ctx_10_root_attempts_exhausted_across_multiple_children() {
    let run = AdmittedRun::admit(
        "run-attempts",
        1,
        Instant::now() + Duration::from_secs(30),
        BudgetLimits {
            max_attempts: 3, // root total attempt cap is 3
            ..BudgetLimits::default()
        },
        sample_authority(),
    );
    let (conn, eff_id) = sample_connection();

    // Child invocation 1 has local cap 2 and uses 2 attempts.
    let inv1 = run
        .context()
        .start_invocation(conn.clone(), eff_id.clone(), "get", None, 2)
        .expect("inv1");
    assert!(inv1.next_attempt().is_ok());
    assert!(inv1.next_attempt().is_ok());
    assert!(inv1.next_attempt().is_err(), "exceeds inv1 local cap 2");

    // Child invocation 2 also has local cap 2, but root only has 1 attempt left!
    let inv2 = run
        .context()
        .start_invocation(conn, eff_id, "get", None, 2)
        .expect("inv2");
    assert!(
        inv2.next_attempt().is_ok(),
        "uses 3rd and final root attempt"
    );
    let err = inv2
        .next_attempt()
        .expect_err("4th root attempt must be rejected even though inv2 is at 1/2");
    assert_eq!(err.code, codes::BUDGET_EXHAUSTED);
}
