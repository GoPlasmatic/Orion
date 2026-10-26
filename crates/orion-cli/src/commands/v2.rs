//! What the three v2 command groups — [`deployments`](super::deployments),
//! [`providers`](super::providers) and [`connections`](super::connections) —
//! share: reading a request file and the API-version check every one of them
//! runs first, plus the flow tests that drive all three against one fake
//! server.

use anyhow::{Context, Result};
use orion_client::V2Admin;

use crate::client::OrionClient;

fn load_file_or_stdin(file: &str) -> Result<serde_json::Value> {
    if file == "-" {
        crate::utils::read_json_input(None, None, true)
    } else {
        crate::utils::read_json_input(Some(file), None, false)
    }
}

/// Refuse a server whose v2 contract revision this CLI does not speak,
/// before any v2 command reads an answer it could misread.
pub async fn require_api_version(client: &OrionClient) -> Result<()> {
    let server = client
        .server_api_version()
        .await
        .context("reading the server's v2 admin API version")?;
    orion_client::check_api_version(server).map_err(anyhow::Error::from)
}

/// `file` (or stdin for `-`) read as the request type `T`.
pub(super) fn load_request<T: serde::de::DeserializeOwned>(file: &str, what: &str) -> Result<T> {
    let body = load_file_or_stdin(file).with_context(|| format!("failed to read '{file}'"))?;
    serde_json::from_value(body).with_context(|| format!("invalid {what} in '{file}'"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::super::connections::*;
    use super::super::deployments::*;
    use super::super::providers::*;
    use super::*;
    use crate::output::OutputFormat;
    use orion_api::v2::*;
    use serde_json::{Value, json};

    /// Answers as the server spells them: every field, so a fixture that
    /// drifts from the contract fails to decode here as it would against a
    /// server.
    fn to_value(answer: impl serde::Serialize) -> Result<Value> {
        Ok(serde_json::to_value(answer)?)
    }

    fn capabilities(backends: Vec<ProviderBackendInfo>) -> RuntimeCapabilitiesResponse {
        RuntimeCapabilitiesResponse {
            api_version: API_VERSION,
            edition: EditionTier::Community,
            admission_policy_id: "community".into(),
            channel_drivers: vec!["rest".into()],
            schema_profiles: vec![],
            wasm_worlds: vec![],
            provider_backends: backends,
            deployment_head: DeploymentHead {
                revision: 0,
                epoch: 0,
            },
            native_scheduler: NativeSchedulerStatus {
                in_flight: 0,
                max_in_flight: 4,
            },
            node_status: NodeStatus {
                node_id: "node-1".into(),
                boot_id: "boot-1".into(),
                desired_revision: 0,
                applied_revision: 0,
                applied_manifest_digest: None,
                configuration_degraded: false,
                quarantined_entities: vec![],
                last_error: None,
                backoff_attempt: 0,
                backoff_delay_ms: 0,
                security_lease_expires_at_ms: 0,
                retained_revisions: vec![],
            },
        }
    }

    fn package() -> ConnectorPackageSummary {
        ConnectorPackageSummary {
            publisher: "org.plasmatic.foundation".into(),
            provider_id: "memory-cache".into(),
            version: "2.0.0".into(),
            digest: "sha256:1111".into(),
            implementation: ImplementationKind::Rust,
            tier: EditionTier::Community,
            admitted: false,
            revoked: false,
            artifact_state: PackageArtifactState::Active,
            manifest: json!({}),
            distribution: json!({}),
            inventory: json!({}),
        }
    }

    fn connection(id: &str, revision: u64, config_digest: &str) -> ConnectionRevisionResponse {
        ConnectionRevisionResponse {
            connection_id: id.into(),
            revision,
            provider_digest: "sha256:1111".into(),
            public_config: json!({}),
            configuration_digest: config_digest.into(),
            ..Default::default()
        }
    }

    pub(super) fn committed(revision: u64, warnings: Vec<String>) -> CreateDeploymentResponse {
        CreateDeploymentResponse {
            committed_revision: revision,
            receipt: DeploymentReceiptResponse {
                operation_id: "op".into(),
                idempotency_key: "k".into(),
                principal: "anonymous".into(),
                request_hash: "h".into(),
                committed_revision: revision,
                manifest_digest: "sha256:3333".into(),
                epoch: revision,
                receiving_node_id: "node-1".into(),
                created_at: String::new(),
            },
            warnings,
            evaluation: ConvergenceStatusResponse::default(),
            local_error: None,
        }
    }

    fn plan(base_revision: u64, digest: &str) -> DeploymentPlanResponse {
        DeploymentPlanResponse {
            base_revision,
            reviewed_manifest_digest: digest.into(),
            added_entities: vec![],
            updated_entities: vec![],
            removed_entities: vec![],
        }
    }

    fn summary(revision: u64, digest: &str) -> DeploymentRevisionSummary {
        DeploymentRevisionSummary {
            revision,
            manifest_digest: digest.into(),
            ..Default::default()
        }
    }

    fn handle_v2_request(method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let body_val = body.cloned().unwrap_or_else(|| json!({}));
        match (method, path) {
            ("GET", "/api/v2/admin/runtime-capabilities") => to_value(capabilities(vec![])),
            ("POST", "/api/v2/admin/authoring/scaffold") => {
                let req: ScaffoldRequest = serde_json::from_value(body_val)?;
                assert_eq!(req.kind, ImplementationKind::Rust);
                to_value(ScaffoldResponse {
                    publisher: req.publisher,
                    provider_id: req.provider_id,
                    version: req.version,
                    implementation: req.kind,
                    package_digest: "sha256:1111".into(),
                    files: [("manifest.json".to_string(), "{}".to_string())].into(),
                })
            }
            ("POST", "/api/v2/admin/authoring/lint") => to_value(LintResponse {
                valid: true,
                ..Default::default()
            }),
            ("POST", "/api/v2/admin/authoring/pack") => to_value(PackResponse {
                package_digest: "sha256:1111".into(),
                files: [
                    ("manifest.json".to_string(), "{}".to_string()),
                    ("inventory.json".to_string(), "{}".to_string()),
                ]
                .into(),
            }),
            ("PUT", p)
                if p.starts_with("/api/v2/admin/connector-packages/")
                    && p.ends_with("/admission") =>
            {
                to_value(AdmitPackageResponse {
                    admission: ProviderAdmissionDto {
                        package_digest: "sha256:1111".into(),
                        policy_id: "community".into(),
                        admission_fingerprint: "fp1".into(),
                        revoked: false,
                    },
                })
            }
            ("POST", "/api/v2/admin/connector-packages") => to_value(UploadPackageResponse {
                package: package(),
                admission: None,
            }),
            ("GET", "/api/v2/admin/connections/cache-1/revisions") => {
                to_value(ConnectionsListResponse {
                    count: 1,
                    connections: vec![connection("cache-1", 1, "sha256:2222")],
                })
            }
            ("POST", "/api/v2/admin/connections/cache-1/revisions/1/probes") => {
                to_value(ConnectionProbeResponse {
                    connection_id: "cache-1".into(),
                    revision: 1,
                    driver_id: "memory-cache".into(),
                    healthy: true,
                    latency_ms: 1,
                    details: ProbeDetails {
                        probe: "cache read of a probe key".into(),
                        error: None,
                        error_class: None,
                        effect_status: None,
                    },
                })
            }
            ("POST", "/api/v2/admin/connections/cache-1/revisions") => {
                to_value(CreateConnectionRevisionResponse {
                    connection: connection("cache-1", 1, "sha256:2222"),
                })
            }
            ("POST", "/api/v2/admin/deployment-plans") => to_value(plan(0, "sha256:3333")),
            ("POST", "/api/v2/admin/deployments") => {
                match serde_json::from_value::<CreateDeploymentRequest>(body_val)? {
                    CreateDeploymentRequest::Rollback(req) => {
                        assert_eq!(req.rollback_to, 1);
                        assert_eq!(req.base_revision, 1);
                        assert!(!req.reason.is_empty());
                        to_value(committed(2, vec!["no undo".into()]))
                    }
                    CreateDeploymentRequest::Apply(req) => {
                        assert!(!req.reason.is_empty(), "apply must send required reason");
                        assert_eq!(req.reviewed_manifest_digest, "sha256:3333");
                        to_value(committed(1, vec![]))
                    }
                }
            }
            ("GET", "/api/v2/admin/deployments?limit=50") => to_value(DeploymentsPage {
                head_revision: 1,
                data: vec![summary(1, "sha256:3333")],
                limit: 50,
                ..Default::default()
            }),
            ("GET", p) if p.starts_with("/api/v2/admin/deployments/1/convergence") => {
                to_value(ConvergenceStatusResponse {
                    converged: true,
                    target_revision: 1,
                    converged_nodes: vec!["node-1".into()],
                    ..Default::default()
                })
            }
            ("GET", "/api/v2/admin/deployments/1") => to_value(DeploymentRevisionResponse {
                summary: summary(1, "sha256:3333"),
                manifest: DeploymentManifest::of_members(vec![]),
            }),
            _ => anyhow::bail!("unexpected {method} {path}"),
        }
    }

    /// A server speaking another contract revision, or none, is refused
    /// before any v2 command runs.
    #[tokio::test]
    async fn a_server_speaking_another_api_version_is_refused() {
        let ok = OrionClient::with_mock_transport(handle_v2_request).unwrap();
        require_api_version(&ok).await.unwrap();
        for answer in [
            json!({"api_version": API_VERSION + 1}),
            json!({"api_version": "v2"}),
        ] {
            let client =
                OrionClient::with_mock_transport(move |_, _, _| Ok(answer.clone())).unwrap();
            let err = require_api_version(&client).await.unwrap_err().to_string();
            assert!(err.contains("this client speaks version 1"), "{err}");
        }
    }

    #[tokio::test]
    async fn cli_v2_commands_exercise_full_flow_against_http_server() {
        let client = OrionClient::with_mock_transport(handle_v2_request).unwrap();
        let fmt = OutputFormat::Json;

        // 1. Capabilities
        let code = DeploymentsCmd::new(DeploymentsSubcommand::Capabilities)
            .run(&client, &fmt, true)
            .await
            .unwrap();
        assert_eq!(code, 0);

        // 2. Scaffold
        let code = ProviderCmd::new(ProviderSubcommand::Scaffold {
            publisher: "org.plasmatic.foundation".to_string(),
            provider_id: "memory-cache".to_string(),
            version: "2.0.0".to_string(),
            kind: ImplementationKind::Rust,
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        // Write temp files for lint, pack, upload, connection create, plan, apply
        let dir = tempfile::tempdir().unwrap();
        let pkg_file = dir.path().join("pkg.json");
        std::fs::write(&pkg_file, r#"{"files":{"manifest.json":"{}"}}"#).unwrap();

        let code = ProviderCmd::new(ProviderSubcommand::Lint {
            file: pkg_file.to_string_lossy().to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = ProviderCmd::new(ProviderSubcommand::Pack {
            file: pkg_file.to_string_lossy().to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = ProviderCmd::new(ProviderSubcommand::Upload {
            file: pkg_file.to_string_lossy().to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = ProviderCmd::new(ProviderSubcommand::Admit {
            digest: "sha256:1111".to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let conn_file = dir.path().join("conn.json");
        std::fs::write(
            &conn_file,
            r#"{"revision":1,"provider_digest":"sha256:1111","public_config":{}}"#,
        )
        .unwrap();
        let code = ConnectionsCmd::new(ConnectionsSubcommand::Create {
            id: "cache-1".to_string(),
            file: conn_file.to_string_lossy().to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = ConnectionsCmd::new(ConnectionsSubcommand::Probe {
            id: "cache-1".to_string(),
            revision: Some(1),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);
        // The latest revision when none is named.
        let code = ConnectionsCmd::new(ConnectionsSubcommand::Probe {
            id: "cache-1".to_string(),
            revision: None,
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let manifest_file = dir.path().join("manifest.json");
        std::fs::write(
            &manifest_file,
            r#"{"schema_version":2,"providers":[],"members":[]}"#,
        )
        .unwrap();

        let code = DeploymentsCmd::new(DeploymentsSubcommand::Plan {
            file: Some(manifest_file.to_string_lossy().to_string()),
            dir: None,
            base_revision: Some(0),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = DeploymentsCmd::new(DeploymentsSubcommand::Apply {
            file: Some(manifest_file.to_string_lossy().to_string()),
            dir: None,
            base_revision: Some(0),
            reviewed_digest: Some("sha256:3333".to_string()),
            idempotency_key: Some("idem-1".to_string()),
            reason: "cli apply".to_string(),
            wait_ms: 1000,
            wait_target: "this-node".to_string(),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = DeploymentsCmd::new(DeploymentsSubcommand::List {
            limit: 50,
            cursor: None,
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let code = DeploymentsCmd::new(DeploymentsSubcommand::Get { revision: 1 })
            .run(&client, &fmt, true)
            .await
            .unwrap();
        assert_eq!(code, 0);

        let code = DeploymentsCmd::new(DeploymentsSubcommand::Rollback {
            target_revision: 1,
            expected_head_revision: 1,
            idempotency_key: "idem-rb".to_string(),
            reason: Some("rollback".to_string()),
        })
        .run(&client, &fmt, true)
        .await
        .unwrap();
        assert_eq!(code, 0);
    }

    /// `apply -d`: every definition uploaded and checked against its file
    /// name, a connection revision created past the stored ones, members
    /// given revisions against the head, and the plan's digest applied.
    #[tokio::test]
    async fn apply_dir_assigns_revisions_against_the_head() {
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let wf_hex = "a".repeat(64);
        let ch_hex = "b".repeat(64);
        std::fs::create_dir_all(dir.path().join("definitions")).unwrap();
        std::fs::create_dir_all(dir.path().join("connections")).unwrap();
        std::fs::write(
            dir.path()
                .join("definitions")
                .join(format!("{wf_hex}.json")),
            r#"{"kind":"workflow","definition":{"name":"w","tasks":[]}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("definitions")
                .join(format!("{ch_hex}.json")),
            r#"{"kind":"channel","definition":{"name":"c","protocol":"rest"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("connections/db.json"),
            r#"{"connection_id":"db","driver":"org.orion.sqlite","provider_digest":"sha256:p",
                "public_config":{"connection_string":"sqlite::memory:"},"secret_locators":{}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("deployment.json"),
            format!(
                r#"{{"schema_version":1,"members":[
                {{"kind":"channel","id":"c","definition":"sha256:{ch_hex}",
                  "requires":[{{"kind":"workflow","id":"w"}}]}},
                {{"kind":"connection","id":"db","connection":"db","requires":[]}},
                {{"kind":"workflow","id":"w","definition":"sha256:{wf_hex}",
                  "requires":[{{"kind":"connection","id":"db"}}]}}]}}"#
            ),
        )
        .unwrap();

        let applied: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let seen = applied.clone();
        let client = OrionClient::with_mock_transport(move |method, path, body| {
            let body = body.cloned().unwrap_or(Value::Null);
            match (method, path) {
                // Head 4 deploys the channel as it is, the workflow at an
                // older definition.
                ("GET", "/api/v2/admin/deployments") => to_value(DeploymentsPage {
                    head_revision: 4,
                    data: vec![summary(4, "sha256:head")],
                    limit: 50,
                    ..Default::default()
                }),
                ("GET", "/api/v2/admin/deployments/4") => {
                    let member = |kind, id: &str, revision, digest: String| ManifestMember {
                        entity: EntityRef {
                            kind,
                            id: id.into(),
                            revision,
                        },
                        definition_digest: digest,
                        owning_package: None,
                        requires: vec![],
                        rollouts: vec![],
                    };
                    to_value(DeploymentRevisionResponse {
                        summary: summary(4, "sha256:head"),
                        manifest: DeploymentManifest::of_members(vec![
                            member(
                                EntityKind::Channel,
                                "c",
                                2,
                                format!("sha256:{}", "b".repeat(64)),
                            ),
                            member(EntityKind::Workflow, "w", 5, "sha256:old".into()),
                        ]),
                    })
                }
                ("POST", "/api/v2/admin/definitions") => {
                    let workflow = body["kind"] == "workflow";
                    let hex = if workflow { "a" } else { "b" };
                    to_value(DefinitionResponse {
                        digest: format!("sha256:{}", hex.repeat(64)),
                        kind: if workflow {
                            EntityKind::Workflow
                        } else {
                            EntityKind::Channel
                        },
                        size_bytes: 1,
                        advisories: vec![],
                    })
                }
                ("GET", "/api/v2/admin/runtime-capabilities") => {
                    to_value(capabilities(vec![ProviderBackendInfo {
                        kind: ImplementationKind::Rust,
                        driver_id: "org.orion.sqlite".into(),
                        package_digest: Some("sha256:p".into()),
                    }]))
                }
                ("GET", "/api/v2/admin/connections") => to_value(ConnectionsListResponse {
                    count: 1,
                    connections: vec![ConnectionRevisionResponse {
                        connection_id: "db".into(),
                        revision: 2,
                        provider_digest: "sha256:p".into(),
                        public_config: json!({"connection_string": "sqlite:/other.db"}),
                        ..Default::default()
                    }],
                }),
                ("POST", "/api/v2/admin/connections/db/revisions") => {
                    assert_eq!(body["revision"], 3, "one past the stored revisions");
                    assert!(body.get("connection_id").is_none(), "the path names it");
                    to_value(CreateConnectionRevisionResponse {
                        connection: ConnectionRevisionResponse {
                            connection_id: "db".into(),
                            revision: 3,
                            configuration_digest: "sha256:cfg".into(),
                            ..Default::default()
                        },
                    })
                }
                ("POST", "/api/v2/admin/deployment-plans") => {
                    assert_eq!(body["base_revision"], 4);
                    to_value(plan(4, "sha256:next"))
                }
                ("POST", "/api/v2/admin/deployments") => {
                    *seen.lock().unwrap() = Some(body);
                    to_value(committed(5, vec![]))
                }
                _ => anyhow::bail!("unexpected {method} {path}"),
            }
        })
        .unwrap();

        let code = DeploymentsCmd::new(DeploymentsSubcommand::Apply {
            file: None,
            dir: Some(dir.path().to_string_lossy().to_string()),
            base_revision: None,
            reviewed_digest: None,
            idempotency_key: None,
            reason: "cli apply".to_string(),
            wait_ms: 0,
            wait_target: "this-node".to_string(),
        })
        .run(&client, &OutputFormat::Json, true)
        .await
        .unwrap();
        assert_eq!(code, 0);

        let request = applied.lock().unwrap().clone().expect("applied");
        assert_eq!(request["base_revision"], 4);
        assert_eq!(request["reviewed_manifest_digest"], "sha256:next");
        assert_eq!(request["idempotency_key"], "deploy-4-next");
        let members = request["manifest"]["members"].as_array().unwrap();
        let revision_of = |kind: &str| {
            members
                .iter()
                .find(|m| m["entity"]["kind"] == kind)
                .map(|m| m["entity"]["revision"].clone())
                .unwrap()
        };
        assert_eq!(revision_of("channel"), 2, "unchanged keeps its revision");
        assert_eq!(revision_of("workflow"), 6, "changed takes the next");
        assert_eq!(revision_of("connection"), 3);
        let workflow = members
            .iter()
            .find(|m| m["entity"]["kind"] == "workflow")
            .unwrap();
        assert_eq!(
            workflow["requires"],
            json!([{"kind": "connection", "id": "db", "revision": 3}])
        );
        let channel = members
            .iter()
            .find(|m| m["entity"]["kind"] == "channel")
            .unwrap();
        assert_eq!(
            channel["requires"],
            json!([{"kind": "workflow", "id": "w", "revision": 6}])
        );
    }
}
