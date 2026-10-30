use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::super::deployment_store::{
    ConnectionRevisionRow, ImmutableDeploymentStore, ProviderPackageRow,
};
use super::super::{ConnectionReference, ImplementationKind, ProviderRef};
use super::{
    PACKAGE_HOST_FUNCTIONS, PackageDependency, PackageProblem, PackageWorkflow, WorkflowPackage,
    declared_dependencies, dependency_key,
};

/// The keys `implementation` may carry in a workflow package.
const IMPLEMENTATION_KEYS: [&str; 3] = ["kind", "entries", "workflows"];

impl WorkflowPackage {
    /// Read and check the package `manifest` describes from its `files`.
    ///
    /// # Errors
    ///
    /// Every problem found, each naming where it was written.
    pub fn from_files(
        provider: ProviderRef,
        manifest: &Value,
        files: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Self, Vec<PackageProblem>> {
        use orion_api::error::field_codes::{INVALID, REQUIRED, TYPE_MISMATCH, UNKNOWN_FIELD};

        let mut problems = Vec::new();
        let implementation = &manifest["implementation"];
        if let Some(obj) = implementation.as_object() {
            for key in obj.keys() {
                if IMPLEMENTATION_KEYS.contains(&key.as_str()) {
                    continue;
                }
                let message = if matches!(
                    key.as_str(),
                    "helpers" | "sub_channels" | "subchannels" | "binding_slots"
                ) {
                    format!(
                        "a workflow package declares no `{key}`: each operation is one entry \
                         workflow, and shared logic belongs in compile-time fragments"
                    )
                } else {
                    format!(
                        "`implementation` of a workflow package takes `kind`, `entries` and \
                         `workflows`, not `{key}`"
                    )
                };
                problems.push(PackageProblem::new(
                    format!("connector.json:implementation.{key}"),
                    UNKNOWN_FIELD,
                    message,
                ));
            }
        }

        // Every listed file, parsed and keyed by its workflow id.
        let mut by_id: BTreeMap<String, (String, Value)> = BTreeMap::new();
        let listed = implementation["workflows"].as_array();
        if listed.is_none() {
            problems.push(PackageProblem::new(
                "connector.json:implementation.workflows",
                REQUIRED,
                "a workflow package lists its workflow files under `implementation.workflows`",
            ));
        }
        for (i, entry) in listed.into_iter().flatten().enumerate() {
            let Some(file) = entry.as_str() else {
                problems.push(PackageProblem::new(
                    format!("connector.json:implementation.workflows[{i}]"),
                    TYPE_MISMATCH,
                    "a workflow file is named by its path in the package",
                ));
                continue;
            };
            let Some(bytes) = files.get(file) else {
                problems.push(PackageProblem::new(
                    format!("connector.json:implementation.workflows[{i}]"),
                    REQUIRED,
                    format!("the package has no file '{file}'"),
                ));
                continue;
            };
            let document: Value = match serde_json::from_slice(bytes) {
                Ok(doc) => doc,
                Err(e) => {
                    problems.push(PackageProblem::new(file, INVALID, format!("not JSON: {e}")));
                    continue;
                }
            };
            let Some(id) = document["id"].as_str().map(str::to_string) else {
                problems.push(PackageProblem::new(
                    format!("{file}:id"),
                    REQUIRED,
                    "a package workflow carries its `id`, which `implementation.entries` names",
                ));
                continue;
            };
            if by_id.contains_key(&id) {
                problems.push(PackageProblem::new(
                    format!("{file}:id"),
                    INVALID,
                    format!("two workflow files declare the id '{id}'"),
                ));
                continue;
            }
            by_id.insert(id, (file.to_string(), document));
        }

        // One entry workflow per operation, and no workflow that is not one.
        let mut entries = BTreeMap::new();
        let mut entered: BTreeMap<&str, &str> = BTreeMap::new();
        match implementation["entries"].as_object() {
            None => problems.push(PackageProblem::new(
                "connector.json:implementation.entries",
                REQUIRED,
                "a workflow package maps each operation to its entry workflow under \
                 `implementation.entries`",
            )),
            Some(map) => {
                for (operation, target) in map {
                    let at = format!("connector.json:implementation.entries.{operation}");
                    let Some(workflow_id) = target.as_str() else {
                        problems.push(PackageProblem::new(
                            at,
                            TYPE_MISMATCH,
                            "an entry names a workflow id",
                        ));
                        continue;
                    };
                    let Some((file, document)) = by_id.get(workflow_id) else {
                        problems.push(PackageProblem::new(
                            at,
                            INVALID,
                            format!("no listed workflow file declares the id '{workflow_id}'"),
                        ));
                        continue;
                    };
                    if let Some(other) = entered.insert(workflow_id, operation) {
                        problems.push(PackageProblem::new(
                            at,
                            INVALID,
                            format!(
                                "workflow '{workflow_id}' is already the entry of '{other}': \
                                 each operation has an entry workflow of its own"
                            ),
                        ));
                        continue;
                    }
                    entries.insert(
                        operation.clone(),
                        PackageWorkflow {
                            file: file.clone(),
                            document: document.clone(),
                        },
                    );
                }
            }
        }
        for (id, (file, _)) in &by_id {
            if !entered.contains_key(id.as_str()) {
                problems.push(PackageProblem::new(
                    file.clone(),
                    INVALID,
                    format!(
                        "workflow '{id}' is the entry of no operation: a workflow package has no \
                         helper workflows, and shared logic belongs in compile-time fragments"
                    ),
                ));
            }
        }

        let dependencies = declared_dependencies(manifest);
        for (operation, workflow) in &mut entries {
            problems.extend(check_workflow(
                &workflow.file,
                &workflow.document,
                &dependencies,
            ));
            if let Some(doc) = workflow.document.as_object_mut() {
                doc.insert("channel".to_string(), json!(operation));
            }
        }

        if problems.is_empty() {
            Ok(Self {
                provider,
                entries,
                dependencies,
            })
        } else {
            Err(problems)
        }
    }

    /// The package `row` as the store holds it: its files read back from its
    /// inventory, then checked as at upload.
    ///
    /// # Errors
    ///
    /// Every problem found; a file the inventory lists that is not stored is
    /// one.
    pub async fn from_store(
        store: &ImmutableDeploymentStore,
        pool: &crate::storage::DbPool,
        row: &ProviderPackageRow,
    ) -> Result<Self, Vec<PackageProblem>> {
        let workflows: Vec<&str> = row.manifest_json["implementation"]["workflows"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let files = store
            .package_files(pool, &row.package_digest, &workflows)
            .await
            .map_err(|e| {
                vec![PackageProblem::new(
                    "",
                    orion_api::error::field_codes::INVALID,
                    format!("the package's files could not be read: {e}"),
                )]
            })?;
        Self::from_files(row.provider_ref(), &row.manifest_json, &files)
    }
}

/// The problems with one package workflow: its own parse, its channel, and
/// every task against the hermetic set.
fn check_workflow(
    file: &str,
    document: &Value,
    dependencies: &BTreeMap<String, PackageDependency>,
) -> Vec<PackageProblem> {
    use orion_api::error::field_codes::{INVALID, UNKNOWN_FUNCTION, UNRESOLVED_SECRET_REF};

    let mut problems = Vec::new();
    let at = |path: &str| format!("{file}:{path}");

    let parsed = dataflow_rs::def::parse(document, crate::engine::parse_options());
    for issue in parsed
        .issues
        .iter()
        .filter(|i| crate::engine::quarantines(i))
    {
        problems.push(PackageProblem::new(
            at(&issue.path.to_string()),
            INVALID,
            issue.message.clone(),
        ));
    }
    if document.get("channel").is_some() {
        problems.push(PackageProblem::new(
            at("channel"),
            INVALID,
            "a package workflow is addressed by its operation and names no channel",
        ));
    }

    secrets_in(document, "", &mut |path| {
        problems.push(PackageProblem::new(
            at(path),
            UNRESOLVED_SECRET_REF,
            "a package workflow reads no secret: neither a `{\"secret\": …}` node nor a secret \
             reference. Key material reaches it through its input or a dependency",
        ));
    });

    let steps = crate::engine::walk_steps(&document["tasks"], document.get("loop"));
    for (path, task) in steps.tasks {
        let function = &task["function"];
        let Some(name) = function["name"].as_str() else {
            continue;
        };
        let name_at = at(&format!("{path}.function.name"));
        if dataflow_rs::def::BUILTIN_FUNCTIONS.contains(&name) {
            continue;
        }
        if name == crate::engine::functions::connector_call::NAME {
            problems.extend(check_dependency_call(
                &at(&format!("{path}.function.input")),
                &function["input"],
                dependencies,
            ));
            continue;
        }
        if PACKAGE_HOST_FUNCTIONS.contains(&name) {
            continue;
        }
        if crate::engine::FunctionRegistry::builtin().contains(name) {
            problems.push(PackageProblem::new(
                name_at,
                INVALID,
                format!(
                    "task '{}' calls `{name}`, which a package workflow may not: it reaches the \
                     deployment's own connections or channels by name. A package workflow calls \
                     the dataflow built-ins, `crypto`, `jwt_sign`, `jwt_verify`, plugin \
                     functions, and `connector_call` on its declared dependencies",
                    task["id"].as_str().unwrap_or_default()
                ),
            ));
            continue;
        }
        // A plugin function is named `<plugin>.<label>`; the build compiles
        // it against the plugins the generation serves.
        if !name.contains('.') {
            problems.push(PackageProblem::new(
                name_at,
                UNKNOWN_FUNCTION,
                format!(
                    "task '{}' calls `{name}`, which is neither a dataflow built-in, an Orion \
                     function a package may call, nor a plugin function",
                    task["id"].as_str().unwrap_or_default()
                ),
            ));
        }
    }
    problems
}

/// A package workflow's `connector_call`: an alias the manifest declares,
/// with an operation that dependency lists, and nothing that picks the
/// provider, which the binding decides.
fn check_dependency_call(
    at: &str,
    input: &Value,
    dependencies: &BTreeMap<String, PackageDependency>,
) -> Vec<PackageProblem> {
    use orion_api::error::field_codes::{INVALID, REQUIRED};

    let mut problems = Vec::new();
    for key in ["kind", "target_id"] {
        if input.get(key).is_some() {
            problems.push(PackageProblem::new(
                format!("{at}.{key}"),
                INVALID,
                format!(
                    "a package's `connector_call` names a dependency alias, and the \
                     connection it is bound to decides the provider: drop `{key}`"
                ),
            ));
        }
    }
    let Some(alias) = input["connection"].as_str() else {
        problems.push(PackageProblem::new(
            format!("{at}.connection"),
            REQUIRED,
            "a package's `connector_call` names one of the package's dependency aliases",
        ));
        return problems;
    };
    let Some(dependency) = dependencies.get(alias) else {
        let declared: Vec<&str> = dependencies.keys().map(String::as_str).collect();
        problems.push(PackageProblem::new(
            format!("{at}.connection"),
            INVALID,
            format!(
                "'{alias}' is not a dependency the package declares (declared: {declared:?}): \
                 a package workflow calls only what its manifest's `dependencies` name"
            ),
        ));
        return problems;
    };
    match input["operation"].as_str() {
        Some(operation) if dependency.operations.contains(operation) => {}
        Some(operation) => problems.push(PackageProblem::new(
            format!("{at}.operation"),
            INVALID,
            format!(
                "dependency '{alias}' declares the operations {:?}, not '{operation}'",
                dependency.operations
            ),
        )),
        None => problems.push(PackageProblem::new(
            format!("{at}.operation"),
            REQUIRED,
            "a `connector_call` names its operation",
        )),
    }
    problems
}

/// Report, by path, every `{"secret": …}` node and every secret reference
/// string in `value`.
fn secrets_in(value: &Value, path: &str, found: &mut dyn FnMut(&str)) {
    match value {
        Value::Object(map) => {
            if map.len() == 1 && map.contains_key("secret") {
                found(path);
                return;
            }
            for (key, child) in map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                secrets_in(child, &child_path, found);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                secrets_in(child, &format!("{path}[{i}]"), found);
            }
        }
        Value::String(s) if crate::connector::secrets::is_secret_reference(s) => found(path),
        _ => {}
    }
}

/// The problems with `row`'s `bindings` as a connection over the package
/// `provider_digest` names, for its revision create: every declared alias
/// bound, nothing else bound, and each target a stored connection over a
/// provider other than a workflow package (a dependency's own dependencies
/// are not carried, so a package cannot call another), running on the
/// provider the dependency names when it names one.
pub fn binding_problems(
    store: &ImmutableDeploymentStore,
    connection_id: &str,
    provider_digest: &str,
    bindings: &BTreeMap<String, String>,
) -> Vec<PackageProblem> {
    use orion_api::error::field_codes::{INVALID, REQUIRED, UNKNOWN_FIELD};

    let package = store
        .get_package(provider_digest)
        .filter(|p| p.implementation == ImplementationKind::Workflow);
    let Some(package) = package else {
        if bindings.is_empty() {
            return Vec::new();
        }
        return vec![PackageProblem::new(
            "bindings",
            INVALID,
            "only a connection over a workflow provider package binds dependencies",
        )];
    };
    let dependencies = declared_dependencies(&package.manifest_json);
    let mut problems = Vec::new();
    for alias in dependencies.keys() {
        if !bindings.contains_key(alias) {
            problems.push(PackageProblem::new(
                format!("bindings.{alias}"),
                REQUIRED,
                format!(
                    "provider '{}' declares the dependency '{alias}': bind it to a connection",
                    package.provider_id
                ),
            ));
        }
    }
    let connections = store.list_connections();
    for (alias, target) in bindings {
        let at = format!("bindings.{alias}");
        let Some(dependency) = dependencies.get(alias) else {
            problems.push(PackageProblem::new(
                at,
                UNKNOWN_FIELD,
                format!(
                    "provider '{}' declares no dependency '{alias}'",
                    package.provider_id
                ),
            ));
            continue;
        };
        if target == connection_id {
            problems.push(PackageProblem::new(
                at,
                INVALID,
                "a connection cannot be its own dependency",
            ));
            continue;
        }
        let Some(latest) = connections
            .iter()
            .filter(|c| &c.connection_id == target)
            .max_by_key(|c| c.revision)
        else {
            problems.push(PackageProblem::new(
                at,
                INVALID,
                format!("no connection '{target}' is stored"),
            ));
            continue;
        };
        if let Some(problem) = target_problem(store, latest, dependency) {
            problems.push(PackageProblem::new(at, INVALID, problem));
        }
    }
    problems
}

/// Why `target` cannot serve `dependency`, if it cannot.
fn target_problem(
    store: &ImmutableDeploymentStore,
    target: &ConnectionRevisionRow,
    dependency: &PackageDependency,
) -> Option<String> {
    let provider_id = match super::super::foundation::builtin_driver(&target.provider_digest)
        .map(ToString::to_string)
        .or_else(|| {
            store
                .composed_rust_driver(&target.provider_digest)
                .map(|id| id.to_string())
        }) {
        Some(driver) => driver,
        None => match store.get_package(&target.provider_digest) {
            Some(pkg) if pkg.implementation == ImplementationKind::Workflow => {
                return Some(format!(
                    "connection '{}' runs on the workflow package '{}': a dependency is a \
                     connection over a built-in or rust provider",
                    target.connection_id, pkg.provider_id
                ));
            }
            Some(pkg) => pkg.provider_id.to_string(),
            None => {
                return Some(format!(
                    "connection '{}' names provider '{}', which is not uploaded",
                    target.connection_id, target.provider_digest
                ));
            }
        },
    };
    match &dependency.provider_id {
        Some(wanted) if wanted != &provider_id => Some(format!(
            "dependency '{}' runs on '{wanted}', and connection '{}' on '{provider_id}'",
            dependency.alias, target.connection_id
        )),
        _ => None,
    }
}

/// What a manifest's connections over workflow packages depend on, keyed by
/// [`dependency_key`]: each binding's target at the revision the member
/// requires, with the operations the dependency declares.
///
/// # Errors
///
/// A binding whose target the member does not require, or that names a
/// connection the store does not hold at that revision.
pub fn manifest_dependencies(
    store: &ImmutableDeploymentStore,
    manifest: &super::super::deployment_store::CanonicalManifest,
) -> Result<BTreeMap<String, (ConnectionReference, BTreeSet<String>)>, String> {
    use super::super::deployment_store::EntityKind;

    let mut out = BTreeMap::new();
    for member in &manifest.members {
        if member.entity.kind != EntityKind::Connection {
            continue;
        }
        let Some(row) = store.get_connection(&member.entity.id, member.entity.revision) else {
            continue;
        };
        if row.bindings.is_empty() {
            continue;
        }
        let Some(package) = store.get_package(&row.provider_digest) else {
            return Err(format!(
                "connection '{}' binds dependencies over provider '{}', which is not uploaded",
                row.connection_id, row.provider_digest
            ));
        };
        let dependencies = declared_dependencies(&package.manifest_json);
        for (alias, target) in &row.bindings {
            let Some(required) = member
                .requires
                .iter()
                .find(|r| r.kind == EntityKind::Connection && &r.id == target)
            else {
                return Err(format!(
                    "connection '{}' binds '{alias}' to connection '{target}', which its \
                     deployment member does not require: add a requirement on connection \
                     '{target}'",
                    row.connection_id
                ));
            };
            let Some(target_row) = store.get_connection(target, required.revision) else {
                return Err(format!(
                    "connection '{}' binds '{alias}' to connection '{target}' revision {}, \
                     which is not stored",
                    row.connection_id, required.revision
                ));
            };
            let Some(provider) = store.provider_ref(&target_row.provider_digest) else {
                return Err(format!(
                    "connection '{target}' names provider '{}', which is not uploaded",
                    target_row.provider_digest
                ));
            };
            let operations = dependencies
                .get(alias)
                .map(|d| d.operations.clone())
                .unwrap_or_default();
            out.insert(
                dependency_key(&row.connection_id, alias),
                (target_row.reference(provider), operations),
            );
        }
    }
    Ok(out)
}
