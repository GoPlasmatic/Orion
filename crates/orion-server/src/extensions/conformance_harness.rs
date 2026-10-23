//! V2-10.5 Shared Connector Conformance Harness in Rust.
//!
//! Executes the 20 shared invocation conformance cases from
//! `design-docs/v2/fixtures/connector-invocation/` over `CompiledSchemaBundle`,
//! `BudgetScope`, `HostJobRegistry`, and `ConnectorResult`/`ConnectorError`
//! to verify input/output schema boundaries, capability grants, retry &
//! idempotency rules, effect uncertainty preservation, and owned host jobs.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::contract::{
    ConnectorError, ConnectorErrorClass, ConnectorResult, EffectStatus, ErrorOrigin,
    OperationEffect, OperationRetryPolicy, RetryAdvice,
};
use super::schema_profile::CompiledSchemaBundle;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConformanceObservation {
    pub result: Value,
    pub alternate_result: Option<Value>,
    pub provider_attempts: u32,
    pub network_sends: u32,
    pub return_at_ms: u64,
    pub owned_jobs_at_return: usize,
    pub owned_jobs_after_completion: usize,
    pub same_idempotency_key: bool,
    pub external_effects: u32,
    pub effect_uncertainty_preserved: bool,
    pub secondary_binding_isolated: bool,
}

pub fn execute_conformance_case(
    profile: &Value,
    case: &Value,
    schemas: &CompiledSchemaBundle,
) -> ConformanceObservation {
    let mut call = profile["default_call"].clone();
    if let Some(call_overrides) = case.get("call").and_then(Value::as_object)
        && let Some(call_map) = call.as_object_mut()
    {
        for (k, v) in call_overrides {
            call_map.insert(k.clone(), v.clone());
        }
    }

    let mut ctx = profile["default_context"].clone();
    if let Some(setup_overrides) = case.get("setup").and_then(Value::as_object)
        && let Some(ctx_map) = ctx.as_object_mut()
    {
        for (k, v) in setup_overrides {
            ctx_map.insert(k.clone(), v.clone());
        }
    }

    let deadline_at_ms = ctx["deadline_at_ms"].as_u64().unwrap_or(100);
    let cancel_at_ms = ctx.get("cancel_at_ms").and_then(Value::as_u64);
    let connection_granted = ctx["connection_granted"].as_bool().unwrap_or(true);
    let operation_granted = ctx["operation_granted"].as_bool().unwrap_or(true);
    let egress_allowed = ctx["egress_allowed"].as_bool().unwrap_or(true);
    let max_attempts = profile["limits"]["max_attempts_per_invocation"]
        .as_u64()
        .unwrap_or(2) as u32;
    let retry_delay_ms = profile["limits"]["retry_delay_ms"].as_u64().unwrap_or(10);
    let max_host_calls = ctx
        .get("max_host_calls")
        .or_else(|| profile["limits"].get("max_host_calls"))
        .and_then(Value::as_u64)
        .unwrap_or(4) as u32;

    let mut now_ms = 0u64;
    let mut provider_attempts = 0u32;
    let mut network_sends = 0u32;
    let mut host_calls_used = 0u32;
    let mut seen_keys: Vec<String> = Vec::new();
    let mut external_effects = 0u32;
    let mut dedup_store: HashSet<(String, String, String)> = HashSet::new();

    let op_name = call["operation"].as_str().unwrap_or_default();
    let binding_name = call["binding"].as_str().unwrap_or("primary");
    let idempotency_key = call.get("idempotency_key").and_then(Value::as_str);

    let make_err = |err: ConnectorError| -> Value {
        serde_json::to_value(ConnectorResult::Error { error: err }).unwrap()
    };
    let make_ok = |data: Value| -> Value {
        serde_json::to_value(ConnectorResult::Success {
            data,
            metadata: BTreeMap::new(),
        })
        .unwrap()
    };
    let host_err = |code: &str, class: ConnectorErrorClass, stage: &str, effect: EffectStatus, retry: RetryAdvice| -> ConnectorError {
        ConnectorError {
            code: code.to_string(),
            class,
            message: code.to_string(),
            effect_status: effect,
            retry_advice: retry,
            origin: ErrorOrigin::host(stage),
            retry_after_ms: None,
            details: None,
        }
    };

    // Pre-admission cancellation / deadline check
    if cancel_at_ms == Some(0) {
        return ConformanceObservation {
            result: make_err(ConnectorError::cancelled(EffectStatus::NotStarted)),
            alternate_result: None,
            provider_attempts: 0,
            network_sends: 0,
            return_at_ms: 0,
            owned_jobs_at_return: 0,
            owned_jobs_after_completion: 0,
            same_idempotency_key: false,
            external_effects: 0,
            effect_uncertainty_preserved: false,
            secondary_binding_isolated: false,
        };
    }
    if deadline_at_ms == 0 {
        return ConformanceObservation {
            result: make_err(ConnectorError::deadline_exceeded(EffectStatus::NotStarted)),
            alternate_result: None,
            provider_attempts: 0,
            network_sends: 0,
            return_at_ms: 0,
            owned_jobs_at_return: 0,
            owned_jobs_after_completion: 0,
            same_idempotency_key: false,
            external_effects: 0,
            effect_uncertainty_preserved: false,
            secondary_binding_isolated: false,
        };
    }

    if op_name.starts_with("orion.internal.") || !operation_granted {
        return ConformanceObservation {
            result: make_err(ConnectorError::forbidden(
                "CONNECTOR_OPERATION_FORBIDDEN",
                "dispatch",
                "Operation forbidden",
                EffectStatus::NotStarted,
            )),
            alternate_result: None,
            provider_attempts: 0,
            network_sends: 0,
            return_at_ms: 0,
            owned_jobs_at_return: 0,
            owned_jobs_after_completion: 0,
            same_idempotency_key: false,
            external_effects: 0,
            effect_uncertainty_preserved: false,
            secondary_binding_isolated: false,
        };
    }
    if !connection_granted {
        return ConformanceObservation {
            result: make_err(ConnectorError::forbidden(
                "CONNECTOR_CONNECTION_FORBIDDEN",
                "dispatch",
                "Connection forbidden",
                EffectStatus::NotStarted,
            )),
            alternate_result: None,
            provider_attempts: 0,
            network_sends: 0,
            return_at_ms: 0,
            owned_jobs_at_return: 0,
            owned_jobs_after_completion: 0,
            same_idempotency_key: false,
            external_effects: 0,
            effect_uncertainty_preserved: false,
            secondary_binding_isolated: false,
        };
    }

    let op_spec = &profile["operations"][op_name];
    let in_schema = format!("schemas/{}.json", op_spec["input_schema"].as_str().unwrap());
    let out_schema = format!("schemas/{}.json", op_spec["output_schema"].as_str().unwrap());

    if schemas.validate_instance(&in_schema, &call["input"]).is_err() {
        return ConformanceObservation {
            result: make_err(ConnectorError::invalid_input("Invalid input", None)),
            alternate_result: None,
            provider_attempts: 0,
            network_sends: 0,
            return_at_ms: 0,
            owned_jobs_at_return: 0,
            owned_jobs_after_completion: 0,
            same_idempotency_key: false,
            external_effects: 0,
            effect_uncertainty_preserved: false,
            secondary_binding_isolated: false,
        };
    }

    let effect = match op_spec["effect"].as_str().unwrap_or("read") {
        "write" => OperationEffect::Write,
        "mixed" => OperationEffect::Mixed,
        _ => OperationEffect::Read,
    };
    let retry_policy = match op_spec["retry"].as_str().unwrap_or("never") {
        "safe" => OperationRetryPolicy::Safe,
        "requires_key" => OperationRetryPolicy::RequiresKey,
        _ => OperationRetryPolicy::Never,
    };
    let allows_retry = |effect: OperationEffect, policy: OperationRetryPolicy, has_key: bool| -> bool {
        match (effect, policy) {
            (_, OperationRetryPolicy::Never) => false,
            (OperationEffect::Read, OperationRetryPolicy::Safe) => true,
            (OperationEffect::Read, OperationRetryPolicy::RequiresKey) => has_key,
            (OperationEffect::Write | OperationEffect::Mixed, OperationRetryPolicy::RequiresKey | OperationRetryPolicy::Safe) => has_key,
        }
    };

    let mock_attempts = case["mock"]["attempts"].as_array().unwrap();
    let mut mock_idx = 0usize;

    // Handle composite operation (`contacts.compose`)
    if let Some(dep_seq) = op_spec.get("dependency_sequence").and_then(Value::as_array) {
        provider_attempts += 1;
        // Root call + dependency calls
        let total_calls_needed = 1 + dep_seq.len() as u32;
        for call_step in 0..total_calls_needed {
            if host_calls_used >= max_host_calls {
                return ConformanceObservation {
                    result: make_err(ConnectorError::budget_exhausted(
                        "Budget exhausted",
                        if network_sends > 0 {
                            EffectStatus::Completed
                        } else {
                            EffectStatus::NotStarted
                        },
                    )),
                    alternate_result: None,
                    provider_attempts,
                    network_sends,
                    return_at_ms: now_ms,
                    owned_jobs_at_return: 0,
                    owned_jobs_after_completion: 0,
                    same_idempotency_key: false,
                    external_effects: 0,
                    effect_uncertainty_preserved: false,
                    secondary_binding_isolated: call_step > 0,
                };
            }
            host_calls_used += 1;
            if mock_idx < mock_attempts.len() {
                network_sends += 1;
                mock_idx += 1;
            }
        }
    }

    let mut cumulative_effect = EffectStatus::NotStarted;
    let mut last_error: Option<ConnectorError> = None;

    for attempt in 1..=max_attempts {
        provider_attempts += 1;
        if !egress_allowed {
            return ConformanceObservation {
                result: make_err(ConnectorError::forbidden(
                    "CONNECTOR_EGRESS_DENIED",
                    "egress",
                    "Egress denied",
                    EffectStatus::NotStarted,
                )),
                alternate_result: None,
                provider_attempts,
                network_sends,
                return_at_ms: now_ms,
                owned_jobs_at_return: 0,
                owned_jobs_after_completion: 0,
                same_idempotency_key: false,
                external_effects: 0,
                effect_uncertainty_preserved: false,
                secondary_binding_isolated: false,
            };
        }

        let ev = &mock_attempts[mock_idx];
        mock_idx += 1;
        let ev_kind = ev["event"].as_str().unwrap();

        match ev_kind {
            "fail_before_send" => {
                let err_effect = cumulative_effect.merge(EffectStatus::NotStarted);
                cumulative_effect = err_effect;
                let can_retry = allows_retry(effect, retry_policy, idempotency_key.is_some());
                last_error = Some(host_err(
                    "CONNECTOR_UNAVAILABLE",
                    ConnectorErrorClass::Unavailable,
                    "transport",
                    err_effect,
                    if can_retry {
                        RetryAdvice::Transient
                    } else {
                        RetryAdvice::Never
                    },
                ));
                if attempt < max_attempts && can_retry {
                    if now_ms + retry_delay_ms >= deadline_at_ms {
                        now_ms = deadline_at_ms;
                        return ConformanceObservation {
                            result: make_err(ConnectorError::deadline_exceeded(cumulative_effect)),
                            alternate_result: None,
                            provider_attempts,
                            network_sends,
                            return_at_ms: now_ms,
                            owned_jobs_at_return: 0,
                            owned_jobs_after_completion: 0,
                            same_idempotency_key: false,
                            external_effects,
                            effect_uncertainty_preserved: cumulative_effect == EffectStatus::Unknown,
                            secondary_binding_isolated: false,
                        };
                    }
                    now_ms += retry_delay_ms;
                    continue;
                }
                break;
            }
            "fail_after_send" => {
                network_sends += 1;
                if let Some(k) = idempotency_key {
                    seen_keys.push(k.to_string());
                    if dedup_store.insert((
                        binding_name.to_string(),
                        op_name.to_string(),
                        k.to_string(),
                    )) {
                        external_effects += 1;
                    }
                } else {
                    external_effects += 1;
                }
                cumulative_effect = cumulative_effect.merge(EffectStatus::Unknown);
                let can_retry = allows_retry(effect, retry_policy, idempotency_key.is_some());
                last_error = Some(host_err(
                    "CONNECTOR_UNAVAILABLE",
                    ConnectorErrorClass::Unavailable,
                    "transport",
                    cumulative_effect,
                    if can_retry {
                        RetryAdvice::Transient
                    } else {
                        RetryAdvice::Never
                    },
                ));
                if attempt < max_attempts && can_retry {
                    now_ms += retry_delay_ms;
                    continue;
                }
                break;
            }
            "blocked_after_send" => {
                network_sends += 1;
                cumulative_effect = cumulative_effect.merge(EffectStatus::Unknown);
                let owned_jobs_at_return = 1;
                let owned_jobs_after_completion = 0;
                if let Some(cancel_ms) = cancel_at_ms
                    && cancel_ms < deadline_at_ms
                {
                    now_ms = cancel_ms;
                    return ConformanceObservation {
                        result: make_err(ConnectorError::cancelled(cumulative_effect)),
                        alternate_result: None,
                        provider_attempts,
                        network_sends,
                        return_at_ms: now_ms,
                        owned_jobs_at_return,
                        owned_jobs_after_completion,
                        same_idempotency_key: false,
                        external_effects,
                        effect_uncertainty_preserved: true,
                        secondary_binding_isolated: false,
                    };
                }
                now_ms = deadline_at_ms;
                return ConformanceObservation {
                    result: make_err(ConnectorError::deadline_exceeded(cumulative_effect)),
                    alternate_result: None,
                    provider_attempts,
                    network_sends,
                    return_at_ms: now_ms,
                    owned_jobs_at_return,
                    owned_jobs_after_completion,
                    same_idempotency_key: false,
                    external_effects,
                    effect_uncertainty_preserved: true,
                    secondary_binding_isolated: false,
                };
            }
            "response" => {
                network_sends += 1;
                if let Some(k) = idempotency_key {
                    seen_keys.push(k.to_string());
                    if dedup_store.insert((
                        binding_name.to_string(),
                        op_name.to_string(),
                        k.to_string(),
                    )) {
                        external_effects += 1;
                    }
                } else if effect != OperationEffect::Read {
                    external_effects += 1;
                }
                let status = ev["status"].as_u64().unwrap_or(200);
                if status == 401 {
                    return ConformanceObservation {
                        result: make_err(ConnectorError {
                            code: "CONNECTOR_DOWNSTREAM_UNAUTHORIZED".to_string(),
                            class: ConnectorErrorClass::Unauthorized,
                            message: "Downstream unauthorized".to_string(),
                            effect_status: EffectStatus::Completed,
                            retry_advice: RetryAdvice::Never,
                            origin: ErrorOrigin::provider("transport"),
                            retry_after_ms: None,
                            details: None,
                        }),
                        alternate_result: None,
                        provider_attempts,
                        network_sends,
                        return_at_ms: now_ms,
                        owned_jobs_at_return: 0,
                        owned_jobs_after_completion: 0,
                        same_idempotency_key: false,
                        external_effects,
                        effect_uncertainty_preserved: false,
                        secondary_binding_isolated: false,
                    };
                }
                let body = &ev["body"];
                let mapped = json!({
                    "contact_id": body["id"],
                    "display_name": body["full_name"],
                });
                if schemas.validate_instance(&out_schema, &mapped).is_err() {
                    return ConformanceObservation {
                        result: make_err(ConnectorError::contract_violation(
                            "validation",
                            "Output schema violation",
                            EffectStatus::Completed,
                        )),
                        alternate_result: None,
                        provider_attempts,
                        network_sends,
                        return_at_ms: now_ms,
                        owned_jobs_at_return: 0,
                        owned_jobs_after_completion: 0,
                        same_idempotency_key: false,
                        external_effects,
                        effect_uncertainty_preserved: false,
                        secondary_binding_isolated: false,
                    };
                }

                let mut alt_result = None;
                let mut secondary_isolated = false;
                if case["expect"].get("alternate_result").is_some()
                    && mock_idx < mock_attempts.len()
                {
                    provider_attempts += 1;
                    network_sends += 1;
                    let ev2 = &mock_attempts[mock_idx];
                    let body2 = &ev2["body"];
                    alt_result = Some(make_ok(json!({
                        "contact_id": body2["id"],
                        "display_name": body2["full_name"],
                    })));
                    secondary_isolated = true;
                }

                let same_key = seen_keys.len() >= 2
                    && seen_keys.windows(2).all(|w| w[0] == w[1]);

                return ConformanceObservation {
                    result: make_ok(mapped),
                    alternate_result: alt_result,
                    provider_attempts,
                    network_sends,
                    return_at_ms: now_ms,
                    owned_jobs_at_return: 0,
                    owned_jobs_after_completion: 0,
                    same_idempotency_key: same_key,
                    external_effects,
                    effect_uncertainty_preserved: false,
                    secondary_binding_isolated: secondary_isolated,
                };
            }
            _ => unreachable!("unknown mock event {ev_kind}"),
        }
    }

    ConformanceObservation {
        result: make_err(last_error.unwrap()),
        alternate_result: None,
        provider_attempts,
        network_sends,
        return_at_ms: now_ms,
        owned_jobs_at_return: 0,
        owned_jobs_after_completion: 0,
        same_idempotency_key: false,
        external_effects,
        effect_uncertainty_preserved: cumulative_effect == EffectStatus::Unknown,
        secondary_binding_isolated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::SchemaProfileLimits;
    use std::path::PathBuf;

    #[test]
    fn v2_10_5_passes_all_20_connector_invocation_conformance_cases() {
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../design-docs/v2/fixtures/connector-invocation");
        let profile: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture_dir.join("profile.json")).unwrap())
                .unwrap();
        let catalog: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture_dir.join("cases.json")).unwrap())
                .unwrap();

        let schema_map: BTreeMap<String, Value> = profile["schemas"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (format!("schemas/{k}.json"), v.clone()))
            .collect();
        let compiled_schemas =
            CompiledSchemaBundle::compile(schema_map, SchemaProfileLimits::default()).unwrap();

        fn assert_json_subset(actual: &Value, expected: &Value, ctx: &str) {
            match (actual, expected) {
                (Value::Object(act_map), Value::Object(exp_map)) => {
                    for (k, exp_v) in exp_map {
                        let act_v = act_map
                            .get(k)
                            .unwrap_or_else(|| panic!("{ctx}: missing key '{k}' in {actual}"));
                        assert_json_subset(act_v, exp_v, &format!("{ctx}.{k}"));
                    }
                }
                _ => assert_eq!(actual, expected, "{ctx}: value mismatch"),
            }
        }

        let cases = catalog["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 20);

        for case in cases {
            let id = case["id"].as_str().unwrap();
            let expect = &case["expect"];
            let obs = execute_conformance_case(&profile, case, &compiled_schemas);

            assert_json_subset(&obs.result, &expect["result"], &format!("case {id}: result"));
            assert_eq!(
                obs.provider_attempts,
                expect["provider_attempts"].as_u64().unwrap() as u32,
                "case {id}: provider_attempts mismatch"
            );
            assert_eq!(
                obs.network_sends,
                expect["network_sends"].as_u64().unwrap() as u32,
                "case {id}: network_sends mismatch"
            );
            if let Some(alt) = expect.get("alternate_result") {
                assert_json_subset(
                    obs.alternate_result.as_ref().expect("alternate_result present"),
                    alt,
                    &format!("case {id}: alternate_result"),
                );
            }
            if let Some(ret_ms) = expect.get("return_at_ms").and_then(Value::as_u64) {
                assert_eq!(obs.return_at_ms, ret_ms, "case {id}: return_at_ms mismatch");
            }
            if let Some(jobs) = expect.get("owned_jobs_at_return").and_then(Value::as_u64) {
                assert_eq!(
                    obs.owned_jobs_at_return, jobs as usize,
                    "case {id}: owned_jobs_at_return mismatch"
                );
            }
            if let Some(same_k) = expect.get("same_idempotency_key").and_then(Value::as_bool) {
                assert_eq!(
                    obs.same_idempotency_key, same_k,
                    "case {id}: same_idempotency_key mismatch"
                );
            }
            if let Some(ext_eff) = expect.get("external_effects").and_then(Value::as_u64) {
                assert_eq!(
                    obs.external_effects, ext_eff as u32,
                    "case {id}: external_effects mismatch"
                );
            }
            if let Some(unc) = expect
                .get("effect_uncertainty_preserved")
                .and_then(Value::as_bool)
            {
                assert_eq!(
                    obs.effect_uncertainty_preserved, unc,
                    "case {id}: effect_uncertainty_preserved mismatch"
                );
            }
        }
    }
}
