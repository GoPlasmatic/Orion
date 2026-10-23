//! Workflow-Group composite connector compiler and provider backend (`V2-12.1`–`V2-12.4`).
//!
//! Enables combining private workflows, optional internal sub-channels, and
//! declared connector `binding_slot` dependencies into a hermetic connector
//! package (`ImplementationKind::Workflow`) that implements [`ProviderBackend`]
//! and [`SourceConnectorFacet`].
//!
//! Guarantees:
//! - **Private Digest Namespace (`V2-12.1`)**: Internal workflows and sub-channels
//!   are scoped under `__provider::{package_digest}::*` and never appear in
//!   application HTTP routing or public workflow enumeration.
//! - **Static Closure & Cycle Verification (`V2-12.2`)**: Rejects missing entries,
//!   undeclared helpers/sub-channels, undeclared `binding_slot`s, capability
//!   expansion, and call-graph cycles at compile time before admission.
//! - **Isolated Invocation State & Scoped Dependencies (`V2-12.3`, `V2-12.4`)**:
//!   Each invocation runs on a fresh state envelope and dispatches declared
//!   `binding_slot` calls through [`Invocation::derive_dependency`] and
//!   [`ProviderRegistry::dispatch_attempt`], sharing the root budget and deadline.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::admission::AdmissionPolicy;
use super::context::AttemptContext;
use super::contract::{
    ConnectorError, ConnectorOutput, ConnectorResult, ImplementationKind, ProviderReference, codes,
};
use super::provider_backend::{
    BoxProviderFuture, ProviderBackend, ProviderRegistry, SourceBatchItem, SourceConnectorFacet,
    SourcePollBatch, SourceSettlement,
};

/// Declared connector dependency slot inside a Workflow-Group manifest (`V2-12.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingSlotSpec {
    pub slot: String,
    pub provider_id: String,
    pub implementation: ImplementationKind,
    pub required_operations: BTreeSet<String>,
}

/// A step inside a private workflow of a Workflow-Group package (`V2-12.2`–`V2-12.4`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step_kind", rename_all = "snake_case")]
pub enum WorkflowGroupStep {
    /// Pure data projection / transform merging fields into the isolated output object.
    Transform {
        output_key: String,
        template: Value,
    },
    /// Call another private helper workflow in the same package.
    CallHelper {
        helper_id: String,
    },
    /// Dispatch through a bundled internal sub-channel in the same package.
    CallSubChannel {
        sub_channel_id: String,
    },
    /// Invoke an external connector bound to `binding_slot` (`V2-12.4`).
    InvokeSlot {
        binding_slot: String,
        operation: String,
        input: Value,
        output_key: String,
    },
}

/// Private workflow definition inside a Workflow-Group package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNodeDef {
    pub id: String,
    pub steps: Vec<WorkflowGroupStep>,
}

/// Internal sub-channel bundled inside a Workflow-Group package (`V2-12.1`).
///
/// Sub-channels never register on the application `RouteTable`; they are
/// addressable only by private workflows inside the same package digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubChannelDef {
    pub id: String,
    pub target_workflow_id: String,
}

/// Optional source facet entry workflows (`open`, `poll`, `settle`, `close`) for
/// a Workflow-Group connector (`V2-15`, §5.1 & §7.2 of `v2-design.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSourceEntries {
    pub open_workflow_id: String,
    pub poll_workflow_id: String,
    pub settle_workflow_id: String,
    pub close_workflow_id: String,
}

/// Input specification for compiling a Workflow-Group composite connector (`V2-12.1`, `V2-12.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowGroupManifest {
    pub provider: ProviderReference,
    /// Map from exported operation name to private entry workflow ID.
    pub entries: BTreeMap<String, String>,
    /// Private entry and helper workflows bundled in the package.
    pub workflows: BTreeMap<String, WorkflowNodeDef>,
    /// Optional internal sub-channels bundled in the package.
    #[serde(default)]
    pub sub_channels: BTreeMap<String, SubChannelDef>,
    /// Declared connector binding slots (`binding_slot -> spec`).
    #[serde(default)]
    pub binding_slots: BTreeMap<String, BindingSlotSpec>,
    /// Optional source facet workflow entries.
    pub source_entries: Option<WorkflowSourceEntries>,
}

/// Static compile-time capability summary for one exported operation (`V2-12.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitiveCapabilityReport {
    pub operation: String,
    pub namespaced_entry: String,
    pub reachable_workflows: BTreeSet<String>,
    pub reachable_sub_channels: BTreeSet<String>,
    /// Map of `binding_slot -> operations` transitively invoked by this operation.
    pub required_slot_operations: BTreeMap<String, BTreeSet<String>>,
}

/// Compile-time error when validating a [`WorkflowGroupManifest`] (`V2-12.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowGraphCompileError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for WorkflowGraphCompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for WorkflowGraphCompileError {}

/// Hermetically compiled Workflow-Group graph keyed by package digest (`V2-12.1`, `V2-12.2`).
#[derive(Debug, Clone)]
pub struct CompiledWorkflowProviderGraph {
    pub provider: ProviderReference,
    pub entries: BTreeMap<String, String>,
    pub namespaced_workflows: BTreeMap<String, String>,
    pub namespaced_sub_channels: BTreeMap<String, String>,
    pub workflows: BTreeMap<String, WorkflowNodeDef>,
    pub sub_channels: BTreeMap<String, SubChannelDef>,
    pub binding_slots: BTreeMap<String, BindingSlotSpec>,
    pub source_entries: Option<WorkflowSourceEntries>,
    pub capability_reports: BTreeMap<String, TransitiveCapabilityReport>,
}

impl CompiledWorkflowProviderGraph {
    /// Format a package-private namespaced identifier that cannot collide across digests (`V2-12.1`).
    pub fn namespaced_id(package_digest: &str, kind: &str, local_id: &str) -> String {
        format!("__provider::{package_digest}::{kind}::{local_id}")
    }

    /// Statically validate and compile a [`WorkflowGroupManifest`] (`V2-12.1`, `V2-12.2`).
    ///
    /// Enforces:
    /// 1. Non-empty operation entries mapping to defined private workflows.
    /// 2. Closed helper and internal sub-channel references (no undeclared targets).
    /// 3. Closed `binding_slot` references and operation grants (no capability expansion).
    /// 4. Acyclic call graph across private workflows and internal sub-channels.
    pub fn compile(
        manifest: WorkflowGroupManifest,
    ) -> Result<Self, WorkflowGraphCompileError> {
        if manifest.entries.is_empty() {
            return Err(WorkflowGraphCompileError {
                code: "WORKFLOW_GROUP_NO_ENTRIES",
                message: "Workflow-group provider must declare at least one operation entry"
                    .to_string(),
            });
        }

        let digest = &manifest.provider.package_digest;
        let mut namespaced_workflows = BTreeMap::new();
        for wf_id in manifest.workflows.keys() {
            namespaced_workflows.insert(
                wf_id.clone(),
                Self::namespaced_id(digest, "workflow", wf_id),
            );
        }

        let mut namespaced_sub_channels = BTreeMap::new();
        for (sc_id, sc_def) in &manifest.sub_channels {
            if !manifest.workflows.contains_key(&sc_def.target_workflow_id) {
                return Err(WorkflowGraphCompileError {
                    code: "WORKFLOW_GROUP_UNDECLARED_HELPER",
                    message: format!(
                        "Internal sub-channel '{sc_id}' targets undeclared workflow '{}'",
                        sc_def.target_workflow_id
                    ),
                });
            }
            namespaced_sub_channels.insert(
                sc_id.clone(),
                Self::namespaced_id(digest, "sub_channel", sc_id),
            );
        }

        // Validate source entries if present.
        if let Some(ref src) = manifest.source_entries {
            for (role, wf_id) in [
                ("open", &src.open_workflow_id),
                ("poll", &src.poll_workflow_id),
                ("settle", &src.settle_workflow_id),
                ("close", &src.close_workflow_id),
            ] {
                if !manifest.workflows.contains_key(wf_id) {
                    return Err(WorkflowGraphCompileError {
                        code: "WORKFLOW_GROUP_MISSING_SOURCE_ENTRY",
                        message: format!(
                            "Source facet '{role}' entry references missing workflow '{wf_id}'"
                        ),
                    });
                }
            }
        }

        // Validate every workflow step's edges and slot grants.
        for (wf_id, wf_def) in &manifest.workflows {
            for step in &wf_def.steps {
                match step {
                    WorkflowGroupStep::Transform { .. } => {}
                    WorkflowGroupStep::CallHelper { helper_id } => {
                        if !manifest.workflows.contains_key(helper_id) {
                            return Err(WorkflowGraphCompileError {
                                code: "WORKFLOW_GROUP_UNDECLARED_HELPER",
                                message: format!(
                                    "Workflow '{wf_id}' calls undeclared helper workflow '{helper_id}'"
                                ),
                            });
                        }
                    }
                    WorkflowGroupStep::CallSubChannel { sub_channel_id } => {
                        if !manifest.sub_channels.contains_key(sub_channel_id) {
                            return Err(WorkflowGraphCompileError {
                                code: "WORKFLOW_GROUP_UNDECLARED_SUB_CHANNEL",
                                message: format!(
                                    "Workflow '{wf_id}' calls undeclared internal sub-channel '{sub_channel_id}'"
                                ),
                            });
                        }
                    }
                    WorkflowGroupStep::InvokeSlot {
                        binding_slot,
                        operation,
                        ..
                    } => {
                        let Some(slot_spec) = manifest.binding_slots.get(binding_slot) else {
                            return Err(WorkflowGraphCompileError {
                                code: "WORKFLOW_GROUP_UNDECLARED_SLOT",
                                message: format!(
                                    "Workflow '{wf_id}' references undeclared binding_slot '{binding_slot}'"
                                ),
                            });
                        };
                        if !slot_spec.required_operations.contains(operation) {
                            return Err(WorkflowGraphCompileError {
                                code: "WORKFLOW_GROUP_CAPABILITY_EXPANSION",
                                message: format!(
                                    "Workflow '{wf_id}' invokes operation '{operation}' not declared on binding_slot '{binding_slot}'"
                                ),
                            });
                        }
                    }
                }
            }
        }

        // Validate each entry and build transitive capability reports + check cycles.
        let mut capability_reports = BTreeMap::new();
        for (op_name, entry_wf_id) in &manifest.entries {
            if !manifest.workflows.contains_key(entry_wf_id) {
                return Err(WorkflowGraphCompileError {
                    code: "WORKFLOW_GROUP_MISSING_ENTRY",
                    message: format!(
                        "Operation '{op_name}' references missing entry workflow '{entry_wf_id}'"
                    ),
                });
            }

            let mut visiting = BTreeSet::new();
            let mut visited = BTreeSet::new();
            let mut reachable_sub_channels = BTreeSet::new();
            let mut required_slot_operations: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

            Self::traverse_graph(
                entry_wf_id,
                &manifest,
                &mut visiting,
                &mut visited,
                &mut reachable_sub_channels,
                &mut required_slot_operations,
            )?;

            let namespaced_entry = namespaced_workflows
                .get(entry_wf_id)
                .cloned()
                .unwrap_or_default();
            capability_reports.insert(
                op_name.clone(),
                TransitiveCapabilityReport {
                    operation: op_name.clone(),
                    namespaced_entry,
                    reachable_workflows: visited,
                    reachable_sub_channels,
                    required_slot_operations,
                },
            );
        }

        Ok(Self {
            provider: manifest.provider,
            entries: manifest.entries,
            namespaced_workflows,
            namespaced_sub_channels,
            workflows: manifest.workflows,
            sub_channels: manifest.sub_channels,
            binding_slots: manifest.binding_slots,
            source_entries: manifest.source_entries,
            capability_reports,
        })
    }

    fn traverse_graph(
        wf_id: &str,
        manifest: &WorkflowGroupManifest,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
        reachable_sub_channels: &mut BTreeSet<String>,
        required_slot_operations: &mut BTreeMap<String, BTreeSet<String>>,
    ) -> Result<(), WorkflowGraphCompileError> {
        if visiting.contains(wf_id) {
            return Err(WorkflowGraphCompileError {
                code: "WORKFLOW_GROUP_CYCLE_DETECTED",
                message: format!("Cycle detected in workflow-group call graph at '{wf_id}'"),
            });
        }
        if visited.contains(wf_id) {
            return Ok(());
        }

        visiting.insert(wf_id.to_string());
        if let Some(wf) = manifest.workflows.get(wf_id) {
            for step in &wf.steps {
                match step {
                    WorkflowGroupStep::Transform { .. } => {}
                    WorkflowGroupStep::CallHelper { helper_id } => {
                        Self::traverse_graph(
                            helper_id,
                            manifest,
                            visiting,
                            visited,
                            reachable_sub_channels,
                            required_slot_operations,
                        )?;
                    }
                    WorkflowGroupStep::CallSubChannel { sub_channel_id } => {
                        reachable_sub_channels.insert(sub_channel_id.clone());
                        if let Some(sc) = manifest.sub_channels.get(sub_channel_id) {
                            Self::traverse_graph(
                                &sc.target_workflow_id,
                                manifest,
                                visiting,
                                visited,
                                reachable_sub_channels,
                                required_slot_operations,
                            )?;
                        }
                    }
                    WorkflowGroupStep::InvokeSlot {
                        binding_slot,
                        operation,
                        ..
                    } => {
                        required_slot_operations
                            .entry(binding_slot.clone())
                            .or_default()
                            .insert(operation.clone());
                    }
                }
            }
        }
        visiting.remove(wf_id);
        visited.insert(wf_id.to_string());
        Ok(())
    }
}

/// Workflow-Group composite connector provider backend (`V2-12.3`, `V2-12.4`).
pub struct WorkflowGroupProviderBackend {
    graph: CompiledWorkflowProviderGraph,
    providers: ProviderRegistry,
    admission: Arc<dyn AdmissionPolicy>,
}

impl WorkflowGroupProviderBackend {
    pub fn new(
        graph: CompiledWorkflowProviderGraph,
        providers: ProviderRegistry,
        admission: Arc<dyn AdmissionPolicy>,
    ) -> Self {
        Self {
            graph,
            providers,
            admission,
        }
    }

    pub fn compiled_graph(&self) -> &CompiledWorkflowProviderGraph {
        &self.graph
    }

    async fn execute_workflow_node(
        &self,
        ctx: &AttemptContext,
        wf_id: &str,
        state: &mut serde_json::Map<String, Value>,
    ) -> Result<(), ConnectorError> {
        ctx.run_context().check_live(ctx.invocation().effect_status())?;
        let Some(wf) = self.graph.workflows.get(wf_id) else {
            return Err(ConnectorError::contract_violation(
                "dispatch",
                format!("Compiled workflow '{wf_id}' missing from provider graph"),
                ctx.invocation().effect_status(),
            ));
        };

        for step in &wf.steps {
            ctx.run_context().check_live(ctx.invocation().effect_status())?;
            match step {
                WorkflowGroupStep::Transform {
                    output_key,
                    template,
                } => {
                    let resolved = if template == &json!("$input") {
                        state.get("input").cloned().unwrap_or(Value::Null)
                    } else {
                        template.clone()
                    };
                    if let Some(out_obj) = state.get_mut("output").and_then(Value::as_object_mut) {
                        out_obj.insert(output_key.clone(), resolved);
                    }
                }
                WorkflowGroupStep::CallHelper { helper_id } => {
                    Box::pin(self.execute_workflow_node(ctx, helper_id, state)).await?;
                }
                WorkflowGroupStep::CallSubChannel { sub_channel_id } => {
                    let Some(sc) = self.graph.sub_channels.get(sub_channel_id) else {
                        return Err(ConnectorError::contract_violation(
                            "dispatch",
                            format!("Internal sub-channel '{sub_channel_id}' not found"),
                            ctx.invocation().effect_status(),
                        ));
                    };
                    Box::pin(self.execute_workflow_node(ctx, &sc.target_workflow_id, state))
                        .await?;
                }
                WorkflowGroupStep::InvokeSlot {
                    binding_slot,
                    operation,
                    input,
                    output_key,
                } => {
                    let Some(slot_spec) = self.graph.binding_slots.get(binding_slot) else {
                        return Err(ConnectorError::forbidden(
                            codes::DEPENDENCY_UNDECLARED,
                            "dispatch",
                            format!("Undeclared binding_slot '{binding_slot}'"),
                            ctx.invocation().effect_status(),
                        ));
                    };

                    // Derive child invocation with inherited generation, clamped deadline,
                    // shared root budget, and reduced authority (`V2-12.4`, `V2-05.1`).
                    let child_inv = ctx.invocation().derive_dependency(
                        binding_slot,
                        ctx.invocation().effective_identity().clone(),
                        operation.clone(),
                        ctx.run_context().deadline(),
                        ctx.run_context().budget().limits(),
                        1,
                    )?;

                    let step_input = if input == &json!("$input") {
                        state.get("input").cloned().unwrap_or(Value::Null)
                    } else {
                        input.clone()
                    };

                    let res = self
                        .providers
                        .dispatch_attempt(
                            self.admission.as_ref(),
                            &child_inv,
                            slot_spec.implementation,
                            &slot_spec.provider_id,
                            step_input,
                        )
                        .await;

                    match res {
                        ConnectorResult::Success { data, .. } => {
                            if let Some(out_obj) =
                                state.get_mut("output").and_then(Value::as_object_mut)
                            {
                                out_obj.insert(output_key.clone(), data);
                            }
                        }
                        ConnectorResult::Error { error } => return Err(error),
                    }
                }
            }
        }
        Ok(())
    }
}

impl ProviderBackend for WorkflowGroupProviderBackend {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Workflow
    }

    fn driver_id(&self) -> &str {
        &self.graph.provider.provider_id
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        if !config.is_object() {
            return Err(ConnectorError::invalid_input(
                "Workflow-group connector config must be a JSON object",
                None,
            ));
        }
        Ok(())
    }

    fn invoke<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        operation: &'a str,
        input: Value,
    ) -> BoxProviderFuture<'a, Result<ConnectorOutput, ConnectorError>> {
        Box::pin(async move {
            ctx.run_context().check_live(ctx.invocation().effect_status())?;
            let Some(entry_wf_id) = self.graph.entries.get(operation) else {
                return Err(ConnectorError::forbidden(
                    codes::OPERATION_FORBIDDEN,
                    "dispatch",
                    format!(
                        "Workflow-group provider '{}' exports no operation '{operation}'",
                        self.driver_id()
                    ),
                    ctx.invocation().effect_status(),
                ));
            };

            // Isolated per-invocation message state (`V2-12.3`).
            let mut state = serde_json::Map::new();
            state.insert("input".to_string(), input);
            state.insert("output".to_string(), Value::Object(serde_json::Map::new()));

            self.execute_workflow_node(ctx, entry_wf_id, &mut state)
                .await?;

            let Some(output) = state.remove("output") else {
                return Err(ConnectorError::contract_violation(
                    "output_validation",
                    "Workflow-group entry produced no output object",
                    ctx.invocation().effect_status(),
                ));
            };
            if !output.is_object() || output.as_object().is_some_and(|m| m.is_empty()) {
                return Err(ConnectorError::contract_violation(
                    "output_validation",
                    "Workflow-group entry produced an empty or non-object output",
                    ctx.invocation().effect_status(),
                ));
            }

            let mut metadata = BTreeMap::new();
            metadata.insert(
                "provider_digest".to_string(),
                json!(self.graph.provider.package_digest),
            );
            Ok(ConnectorOutput::with_metadata(output, metadata))
        })
    }

    fn as_source(&self) -> Option<&dyn SourceConnectorFacet> {
        self.graph.source_entries.as_ref().map(|_| self as &dyn SourceConnectorFacet)
    }
}

impl SourceConnectorFacet for WorkflowGroupProviderBackend {
    fn open<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        checkpoint: Option<Value>,
    ) -> BoxProviderFuture<'a, Result<Value, ConnectorError>> {
        Box::pin(async move {
            let Some(ref src) = self.graph.source_entries else {
                return Err(ConnectorError::forbidden(
                    codes::OPERATION_FORBIDDEN,
                    "dispatch",
                    "Workflow-group provider declares no source facet",
                    ctx.invocation().effect_status(),
                ));
            };
            let mut state = serde_json::Map::new();
            state.insert(
                "input".to_string(),
                json!({"checkpoint": checkpoint.unwrap_or(Value::Null)}),
            );
            state.insert("output".to_string(), Value::Object(serde_json::Map::new()));
            self.execute_workflow_node(ctx, &src.open_workflow_id, &mut state)
                .await?;
            Ok(state.remove("output").unwrap_or_else(|| json!({})))
        })
    }

    fn poll<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        cursor: &'a Value,
        max_items: u32,
    ) -> BoxProviderFuture<'a, Result<SourcePollBatch, ConnectorError>> {
        Box::pin(async move {
            let Some(ref src) = self.graph.source_entries else {
                return Err(ConnectorError::forbidden(
                    codes::OPERATION_FORBIDDEN,
                    "dispatch",
                    "Workflow-group provider declares no source facet",
                    ctx.invocation().effect_status(),
                ));
            };
            let mut state = serde_json::Map::new();
            state.insert(
                "input".to_string(),
                json!({"cursor": cursor, "max_items": max_items}),
            );
            state.insert("output".to_string(), Value::Object(serde_json::Map::new()));
            self.execute_workflow_node(ctx, &src.poll_workflow_id, &mut state)
                .await?;
            Ok(SourcePollBatch {
                items: Vec::<SourceBatchItem>::new(),
                next_checkpoint: state.remove("output"),
            })
        })
    }

    fn settle<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        delivery_id: &'a str,
        settlement: SourceSettlement,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>> {
        Box::pin(async move {
            let Some(ref src) = self.graph.source_entries else {
                return Err(ConnectorError::forbidden(
                    codes::OPERATION_FORBIDDEN,
                    "dispatch",
                    "Workflow-group provider declares no source facet",
                    ctx.invocation().effect_status(),
                ));
            };
            let mut state = serde_json::Map::new();
            state.insert(
                "input".to_string(),
                json!({"delivery_id": delivery_id, "settlement": settlement}),
            );
            state.insert("output".to_string(), Value::Object(serde_json::Map::new()));
            self.execute_workflow_node(ctx, &src.settle_workflow_id, &mut state)
                .await?;
            Ok(())
        })
    }

    fn close<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        cursor: Value,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>> {
        Box::pin(async move {
            let Some(ref src) = self.graph.source_entries else {
                return Err(ConnectorError::forbidden(
                    codes::OPERATION_FORBIDDEN,
                    "dispatch",
                    "Workflow-group provider declares no source facet",
                    ctx.invocation().effect_status(),
                ));
            };
            let mut state = serde_json::Map::new();
            state.insert("input".to_string(), json!({"cursor": cursor}));
            state.insert("output".to_string(), Value::Object(serde_json::Map::new()));
            self.execute_workflow_node(ctx, &src.close_workflow_id, &mut state)
                .await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::CompositionRoot;
    use crate::extensions::context::{AdmittedRun, AuthorityGrant, BudgetLimits};
    use crate::extensions::contract::{ConnectionReference, EffectiveConnectionIdentity};
    use crate::extensions::foundation::DRIVER_HTTP;
    use std::time::{Duration, Instant};

    fn sample_provider_ref() -> ProviderReference {
        ProviderReference {
            provider_id: "acme.crm-composite".to_string(),
            version: "1.0.0".to_string(),
            package_digest:
                "sha256:abababababababababababababababababababababababababababababababab"
                    .to_string(),
        }
    }

    #[test]
    fn v2_12_1_and_2_rejects_cycles_undeclared_helpers_and_capability_expansion() {
        let mut workflows = BTreeMap::new();
        workflows.insert(
            "entry_a".to_string(),
            WorkflowNodeDef {
                id: "entry_a".to_string(),
                steps: vec![WorkflowGroupStep::CallHelper {
                    helper_id: "helper_b".to_string(),
                }],
            },
        );
        workflows.insert(
            "helper_b".to_string(),
            WorkflowNodeDef {
                id: "helper_b".to_string(),
                steps: vec![WorkflowGroupStep::CallHelper {
                    helper_id: "entry_a".to_string(),
                }],
            },
        );

        let cyclic = WorkflowGroupManifest {
            provider: sample_provider_ref(),
            entries: BTreeMap::from([("sync_contact".to_string(), "entry_a".to_string())]),
            workflows,
            sub_channels: BTreeMap::new(),
            binding_slots: BTreeMap::new(),
            source_entries: None,
        };
        let err = CompiledWorkflowProviderGraph::compile(cyclic).expect_err("cycle must fail");
        assert_eq!(err.code, "WORKFLOW_GROUP_CYCLE_DETECTED");

        // Test capability expansion on a binding_slot (requesting "delete" when only "get" is declared).
        let mut wf_cap = BTreeMap::new();
        wf_cap.insert(
            "entry".to_string(),
            WorkflowNodeDef {
                id: "entry".to_string(),
                steps: vec![WorkflowGroupStep::InvokeSlot {
                    binding_slot: "http_slot".to_string(),
                    operation: "delete".to_string(),
                    input: json!({"path": "/contacts/1"}),
                    output_key: "res".to_string(),
                }],
            },
        );
        let mut slots = BTreeMap::new();
        slots.insert(
            "http_slot".to_string(),
            BindingSlotSpec {
                slot: "http_slot".to_string(),
                provider_id: DRIVER_HTTP.to_string(),
                implementation: ImplementationKind::Rust,
                required_operations: BTreeSet::from(["get".to_string()]),
            },
        );
        let cap_expansion = WorkflowGroupManifest {
            provider: sample_provider_ref(),
            entries: BTreeMap::from([("remove".to_string(), "entry".to_string())]),
            workflows: wf_cap,
            sub_channels: BTreeMap::new(),
            binding_slots: slots,
            source_entries: None,
        };
        let err = CompiledWorkflowProviderGraph::compile(cap_expansion)
            .expect_err("capability expansion must fail");
        assert_eq!(err.code, "WORKFLOW_GROUP_CAPABILITY_EXPANSION");
    }

    #[tokio::test]
    async fn v2_12_3_and_4_executes_composite_workflow_with_sub_channel_and_binding_slot() {
        let root = CompositionRoot::community();

        // Build a composite Workflow-Group connector that:
        // 1. Calls an internal sub-channel ("normalize_sub") -> "normalize_wf"
        // 2. Invokes the Community foundation `org.orion.http` connector via `binding_slot: "upstream_api"`
        let mut workflows = BTreeMap::new();
        workflows.insert(
            "fetch_contact_entry".to_string(),
            WorkflowNodeDef {
                id: "fetch_contact_entry".to_string(),
                steps: vec![
                    WorkflowGroupStep::CallSubChannel {
                        sub_channel_id: "normalize_sub".to_string(),
                    },
                    WorkflowGroupStep::InvokeSlot {
                        binding_slot: "upstream_api".to_string(),
                        operation: "get".to_string(),
                        input: json!({"path": "/v1/contacts/123"}),
                        output_key: "upstream_response".to_string(),
                    },
                ],
            },
        );
        workflows.insert(
            "normalize_wf".to_string(),
            WorkflowNodeDef {
                id: "normalize_wf".to_string(),
                steps: vec![WorkflowGroupStep::Transform {
                    output_key: "normalized_input".to_string(),
                    template: json!("$input"),
                }],
            },
        );

        let mut sub_channels = BTreeMap::new();
        sub_channels.insert(
            "normalize_sub".to_string(),
            SubChannelDef {
                id: "normalize_sub".to_string(),
                target_workflow_id: "normalize_wf".to_string(),
            },
        );

        let mut binding_slots = BTreeMap::new();
        binding_slots.insert(
            "upstream_api".to_string(),
            BindingSlotSpec {
                slot: "upstream_api".to_string(),
                provider_id: DRIVER_HTTP.to_string(),
                implementation: ImplementationKind::Rust,
                required_operations: BTreeSet::from(["get".to_string()]),
            },
        );

        let manifest = WorkflowGroupManifest {
            provider: sample_provider_ref(),
            entries: BTreeMap::from([(
                "get_contact".to_string(),
                "fetch_contact_entry".to_string(),
            )]),
            workflows,
            sub_channels,
            binding_slots,
            source_entries: None,
        };

        let compiled = CompiledWorkflowProviderGraph::compile(manifest).expect("compiles cleanly");
        assert!(
            compiled.namespaced_workflows["fetch_contact_entry"]
                .starts_with("__provider::sha256:abab")
        );

        let wf_backend = Arc::new(WorkflowGroupProviderBackend::new(
            compiled,
            root.providers.clone(),
            root.admission.clone(),
        ));
        root.providers.register(wf_backend);

        // Set up run context authorizing both the composite connector and its declared `upstream_api` dependency slot.
        let composite_conn = ConnectionReference {
            connection_id: "crm-conn".to_string(),
            revision: 1,
            provider: sample_provider_ref(),
        };
        let http_conn = ConnectionReference {
            connection_id: "http-conn".to_string(),
            revision: 1,
            provider: ProviderReference {
                provider_id: DRIVER_HTTP.to_string(),
                version: "2.0.0".to_string(),
                package_digest:
                    "sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"
                        .to_string(),
            },
        };
        let eff_id = EffectiveConnectionIdentity {
            provider_digest: composite_conn.provider.package_digest.clone(),
            connection_id: "crm-conn".to_string(),
            revision: 1,
            secret_hash: "sha256:9999".to_string(),
        };

        let mut allowed_operations = BTreeMap::new();
        allowed_operations.insert(
            "crm-conn".to_string(),
            BTreeSet::from(["get_contact".to_string()]),
        );
        let mut declared_dependencies = BTreeMap::new();
        declared_dependencies.insert(
            "upstream_api".to_string(),
            (http_conn, BTreeSet::from(["get".to_string()])),
        );

        let run = AdmittedRun::admit(
            "run-composite",
            1,
            Instant::now() + Duration::from_secs(30),
            BudgetLimits::default(),
            AuthorityGrant {
                allowed_operations,
                declared_dependencies,
                allowed_channels: BTreeSet::new(),
            },
        );
        let inv = run
            .context()
            .start_invocation(composite_conn, eff_id, "get_contact", None, 2)
            .expect("composite invocation starts");

        let res = root
            .providers
            .dispatch_attempt(
                root.admission.as_ref(),
                &inv,
                ImplementationKind::Workflow,
                "acme.crm-composite",
                json!({"contact_id": "123"}),
            )
            .await;

        let ConnectorResult::Success { data, .. } = res else {
            panic!("expected composite workflow success, got {res:?}");
        };
        assert_eq!(data["normalized_input"], json!({"contact_id": "123"}));
        assert_eq!(data["upstream_response"]["status_code"], 200);
        assert_eq!(data["upstream_response"]["path"], "/v1/contacts/123");
        // Verify both parent attempt and child dependency attempt debited the shared root budget!
        assert_eq!(run.context().budget().snapshot().spent_attempts, 2);
    }
}
