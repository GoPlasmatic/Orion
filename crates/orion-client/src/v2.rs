//! Typed calls on the v2 admin API.
//!
//! [`V2Admin`] is one method per v2 admin operation, each taking the request
//! DTO `orion_api::v2` defines and answering the response DTO, over any
//! transport that can send one JSON request ([`V2Admin::send`]). The
//! [`OrionClient`] implements it over HTTP; a caller with its own transport
//! (a test double, an in-process router) implements `send` and gets every
//! typed method for free, so no caller builds a v2 body or path by hand.

use std::future::Future;

use orion_api::v2::{
    AdmitPackageRequest, AdmitPackageResponse, ConnectionProbeResponse, ConnectionRevisionResponse,
    ConnectionsListResponse, ConnectorPackageResponse, ConnectorPackagesListResponse,
    ConvergenceStatusResponse, CreateConnectionRevisionRequest, CreateConnectionRevisionResponse,
    CreateDeploymentPlanRequest, CreateDeploymentRequest, CreateDeploymentResponse, Definition,
    DefinitionResponse, DeploymentPlanResponse, DeploymentRevisionResponse, DeploymentsPage,
    EditionTransitionPreflightRequest, EditionTransitionPreflightResponse, LintResponse,
    PackResponse, RevokePackageRequest, RevokePackageResponse, RuntimeCapabilitiesResponse,
    ScaffoldRequest, ScaffoldResponse, TestDefinitionRequest, TestDefinitionResponse,
    UploadPackageRequest, UploadPackageResponse,
};
use reqwest::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{ClientError, OrionClient, paths};

/// Send `body` to `path` and decode the answer as `Resp`.
async fn call<C, Resp>(
    client: &C,
    method: Method,
    path: String,
    body: Option<Value>,
) -> Result<Resp, C::Error>
where
    C: V2Admin + ?Sized,
    Resp: DeserializeOwned,
{
    let answer = client.send(method, &path, body).await?;
    Ok(serde_json::from_value(answer)?)
}

fn body<T: Serialize + ?Sized>(request: &T) -> Result<Option<Value>, serde_json::Error> {
    serde_json::to_value(request).map(Some)
}

/// Every v2 admin operation, typed. See the module documentation.
pub trait V2Admin: Sync {
    /// What a call fails with. A body that does not decode as the response
    /// type is one of these too.
    type Error: From<serde_json::Error> + Send;

    /// Send one request — `body` as JSON when there is one — and answer the
    /// 2xx body. A non-2xx answer is an error.
    fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> impl Future<Output = Result<Value, Self::Error>> + Send;

    // -- Capabilities --

    fn runtime_capabilities(
        &self,
    ) -> impl Future<Output = Result<RuntimeCapabilitiesResponse, Self::Error>> + Send {
        call(
            self,
            Method::GET,
            paths::V2_RUNTIME_CAPABILITIES.into(),
            None,
        )
    }

    // -- Connector packages --

    fn list_connector_packages(
        &self,
    ) -> impl Future<Output = Result<ConnectorPackagesListResponse, Self::Error>> + Send {
        call(self, Method::GET, paths::V2_CONNECTOR_PACKAGES.into(), None)
    }

    fn get_connector_package(
        &self,
        digest: &str,
    ) -> impl Future<Output = Result<ConnectorPackageResponse, Self::Error>> + Send {
        call(self, Method::GET, paths::v2_connector_package(digest), None)
    }

    fn upload_connector_package(
        &self,
        request: &UploadPackageRequest,
    ) -> impl Future<Output = Result<UploadPackageResponse, Self::Error>> + Send {
        let body = body(request);
        async move {
            call(
                self,
                Method::POST,
                paths::V2_CONNECTOR_PACKAGES.into(),
                body?,
            )
            .await
        }
    }

    /// `PUT` the package's admission.
    fn admit_connector_package(
        &self,
        digest: &str,
        request: &AdmitPackageRequest,
    ) -> impl Future<Output = Result<AdmitPackageResponse, Self::Error>> + Send {
        let (path, body) = (paths::v2_connector_package_admission(digest), body(request));
        async move { call(self, Method::PUT, path, body?).await }
    }

    /// Record a revocation of the package.
    fn revoke_connector_package(
        &self,
        digest: &str,
        request: &RevokePackageRequest,
    ) -> impl Future<Output = Result<RevokePackageResponse, Self::Error>> + Send {
        let (path, body) = (
            paths::v2_connector_package_revocations(digest),
            body(request),
        );
        async move { call(self, Method::POST, path, body?).await }
    }

    // -- Connections --

    /// Every stored revision of every connection.
    fn list_connections(
        &self,
    ) -> impl Future<Output = Result<ConnectionsListResponse, Self::Error>> + Send {
        call(self, Method::GET, paths::V2_CONNECTIONS.into(), None)
    }

    fn list_connection_revisions(
        &self,
        connection_id: &str,
    ) -> impl Future<Output = Result<ConnectionsListResponse, Self::Error>> + Send {
        call(
            self,
            Method::GET,
            paths::v2_connection_revisions(connection_id),
            None,
        )
    }

    fn get_connection_revision(
        &self,
        connection_id: &str,
        revision: u64,
    ) -> impl Future<Output = Result<ConnectionRevisionResponse, Self::Error>> + Send {
        call(
            self,
            Method::GET,
            paths::v2_connection_revision(connection_id, revision),
            None,
        )
    }

    fn create_connection_revision(
        &self,
        connection_id: &str,
        request: &CreateConnectionRevisionRequest,
    ) -> impl Future<Output = Result<CreateConnectionRevisionResponse, Self::Error>> + Send {
        let (path, body) = (paths::v2_connection_revisions(connection_id), body(request));
        async move { call(self, Method::POST, path, body?).await }
    }

    /// Run one probe of the revision's backend. A probe that ran answers
    /// whatever it found; `healthy` is the finding.
    fn probe_connection(
        &self,
        connection_id: &str,
        revision: u64,
    ) -> impl Future<Output = Result<ConnectionProbeResponse, Self::Error>> + Send {
        call(
            self,
            Method::POST,
            paths::v2_connection_probes(connection_id, revision),
            None,
        )
    }

    // -- Definitions --

    fn upload_definition(
        &self,
        definition: &Definition,
    ) -> impl Future<Output = Result<DefinitionResponse, Self::Error>> + Send {
        let body = body(definition);
        async move { call(self, Method::POST, paths::V2_DEFINITIONS.into(), body?).await }
    }

    fn get_definition(
        &self,
        digest: &str,
    ) -> impl Future<Output = Result<Definition, Self::Error>> + Send {
        call(self, Method::GET, paths::v2_definition(digest), None)
    }

    fn test_definition(
        &self,
        request: &TestDefinitionRequest,
    ) -> impl Future<Output = Result<TestDefinitionResponse, Self::Error>> + Send {
        let body = body(request);
        async move { call(self, Method::POST, paths::V2_DEFINITIONS_TEST.into(), body?).await }
    }

    // -- Deployments --

    fn plan_deployment(
        &self,
        request: &CreateDeploymentPlanRequest,
    ) -> impl Future<Output = Result<DeploymentPlanResponse, Self::Error>> + Send {
        let body = body(request);
        async move { call(self, Method::POST, paths::V2_DEPLOYMENT_PLANS.into(), body?).await }
    }

    /// Commit the next revision: an apply or a rollback.
    fn create_deployment(
        &self,
        request: &CreateDeploymentRequest,
    ) -> impl Future<Output = Result<CreateDeploymentResponse, Self::Error>> + Send {
        let body = body(request);
        async move { call(self, Method::POST, paths::V2_DEPLOYMENTS.into(), body?).await }
    }

    /// One page of committed revisions, newest first.
    fn list_deployments(
        &self,
        limit: Option<u64>,
        cursor: Option<&str>,
    ) -> impl Future<Output = Result<DeploymentsPage, Self::Error>> + Send {
        call(
            self,
            Method::GET,
            paths::v2_deployments(limit, cursor),
            None,
        )
    }

    fn get_deployment(
        &self,
        revision: u64,
    ) -> impl Future<Output = Result<DeploymentRevisionResponse, Self::Error>> + Send {
        call(self, Method::GET, paths::v2_deployment(revision), None)
    }

    fn deployment_convergence(
        &self,
        revision: u64,
        target: Option<&str>,
    ) -> impl Future<Output = Result<ConvergenceStatusResponse, Self::Error>> + Send {
        call(
            self,
            Method::GET,
            paths::v2_deployment_convergence(revision, target),
            None,
        )
    }

    fn edition_transition_preflight(
        &self,
        request: &EditionTransitionPreflightRequest,
    ) -> impl Future<Output = Result<EditionTransitionPreflightResponse, Self::Error>> + Send {
        let body = body(request);
        async move {
            call(
                self,
                Method::POST,
                paths::V2_EDITION_TRANSITION_PREFLIGHT.into(),
                body?,
            )
            .await
        }
    }

    // -- Provider package authoring --

    fn authoring_scaffold(
        &self,
        request: &ScaffoldRequest,
    ) -> impl Future<Output = Result<ScaffoldResponse, Self::Error>> + Send {
        let body = body(request);
        async move {
            call(
                self,
                Method::POST,
                paths::V2_AUTHORING_SCAFFOLD.into(),
                body?,
            )
            .await
        }
    }

    fn authoring_lint(
        &self,
        request: &UploadPackageRequest,
    ) -> impl Future<Output = Result<LintResponse, Self::Error>> + Send {
        let body = body(request);
        async move { call(self, Method::POST, paths::V2_AUTHORING_LINT.into(), body?).await }
    }

    fn authoring_pack(
        &self,
        request: &UploadPackageRequest,
    ) -> impl Future<Output = Result<PackResponse, Self::Error>> + Send {
        let body = body(request);
        async move { call(self, Method::POST, paths::V2_AUTHORING_PACK.into(), body?).await }
    }
}

impl V2Admin for OrionClient {
    type Error = ClientError;

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ClientError> {
        let bytes = self.execute(method, path, body.as_ref()).await?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(ClientError::from)
    }
}
