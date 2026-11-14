//! What a provider sees of one attempt at a connector call.
//!
//! The node owns the run: its deadline, its cancellation tree, its budgets
//! and the authority that decided this call may be made at all. A provider
//! is handed an [`AttemptContext`], a read-only view of that state with one
//! write: [`AttemptContext::record_effect`], which can only add evidence that
//! an external effect started (the merge is monotonic, see
//! [`EffectStatus::merge`]).
//!
//! The node implements [`AttemptScope`] over its own invocation state. A
//! provider's unit tests can implement it over a fixed value.

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::contract::{ConnectorError, ConnectorOutput, EffectStatus, codes};
use crate::identity::{ConnectionReference, EffectiveConnectionIdentity};

/// A resolved secret value whose `Debug` and `Display` implementations are
/// redacted (`"******"`) so credentials never leak into logs or error strings.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wrap a resolved secret string.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Explicitly expose the underlying secret string at the point of use.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(\"******\")")
    }
}

impl fmt::Display for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("******")
    }
}

/// Host-provided resolver for the governed connection revision an
/// [`AttemptContext`] runs over.
///
/// Allows external [`ProviderBackend`](crate::ProviderBackend) implementations
/// to read their connection's public configuration and resolve its secret
/// locators on demand without depending on internal deployment-store types.
#[async_trait]
pub trait ConnectionResolver: Send + Sync + fmt::Debug {
    /// The governed connection revision's `public_config` JSON object.
    fn public_config(&self) -> Option<&Value>;

    /// Resolve the secret locator registered at `path` (e.g. `"connection_string"`
    /// or `"auth.token"`), or `Ok(None)` if no secret locator is configured at
    /// `path`.
    async fn resolve_secret(&self, path: &str) -> Result<Option<SecretValue>, ConnectorError>;

    /// Return `public_config` with every configured secret locator resolved and
    /// merged at its dotted path.
    async fn resolve_merged_config(&self) -> Result<Option<Value>, ConnectorError> {
        Ok(self.public_config().cloned())
    }
}

/// The node's side of an attempt: the invocation an [`AttemptContext`]
/// views. Implemented by the host, read by a provider through the context.
pub trait AttemptScope: Any + Send + Sync + fmt::Debug {
    /// The admitted run this call belongs to.
    fn run_id(&self) -> &str;

    /// The runtime generation the run is pinned to; every nested call keeps it.
    fn generation_id(&self) -> u64;

    /// When the run's time is up: the earliest deadline of this call and
    /// every call it is nested in.
    fn deadline(&self) -> Instant;

    /// How deeply this call is nested (`0` for a call a workflow made).
    fn depth(&self) -> u32;

    /// Whether the run, or any call this one is nested in, was cancelled.
    fn is_cancelled(&self) -> bool;

    /// The logical invocation this attempt is one try at.
    fn invocation_id(&self) -> u64;

    /// The invocation that made this one, for a provider's own dependency call.
    fn parent_invocation_id(&self) -> Option<u64>;

    /// The connection revision the call runs over, and its provider.
    fn connection(&self) -> &ConnectionReference;

    /// What partitions the call's sessions and pools.
    fn effective_identity(&self) -> &EffectiveConnectionIdentity;

    /// The operation being invoked.
    fn operation(&self) -> &str;

    /// The caller's idempotency key, when it gave one.
    fn idempotency_key(&self) -> Option<&str>;

    /// The effect evidence recorded across the invocation so far.
    fn effect_status(&self) -> EffectStatus;

    /// Merge `observed` into the invocation's evidence and answer the result.
    fn record_effect(&self, observed: EffectStatus) -> EffectStatus;

    /// Optional governed connection configuration and secret resolver for this
    /// invocation's connection revision.
    fn connection_resolver(&self) -> Option<&dyn ConnectionResolver> {
        None
    }

    /// The caller for the provider's declared dependencies, when the host
    /// binds one ([`AttemptContext::call_dependency`]).
    fn dependency_caller(&self) -> Option<&dyn DependencyCaller> {
        None
    }
}

/// How a provider reaches a connection it declared as a dependency
/// ([`crate::DependencyDescription`]): the host resolves the alias to the
/// connection revision the deployment pins and dispatches one call on it,
/// under this call's deadline and budget, with only the operations the
/// dependency declares.
#[async_trait]
pub trait DependencyCaller: Send + Sync + fmt::Debug {
    /// Call `operation` on the connection bound to `alias`.
    async fn call(
        &self,
        alias: &str,
        operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError>;

    /// The connection revision bound to `alias` in this run, when one is.
    fn bound(&self, alias: &str) -> Option<ConnectionReference>;
}

/// One attempt at a connector call, as the provider dispatched to sees it.
#[derive(Debug, Clone)]
pub struct AttemptContext {
    scope: Arc<dyn AttemptScope>,
    attempt_number: u32,
}

impl AttemptContext {
    /// Attempt `attempt_number` (from 1) of the invocation `scope` describes.
    pub fn new(scope: Arc<dyn AttemptScope>, attempt_number: u32) -> Self {
        Self {
            scope,
            attempt_number,
        }
    }

    /// Which try this is, from 1.
    pub fn attempt_number(&self) -> u32 {
        self.attempt_number
    }

    /// See [`AttemptScope::run_id`].
    pub fn run_id(&self) -> &str {
        self.scope.run_id()
    }

    /// See [`AttemptScope::generation_id`].
    pub fn generation_id(&self) -> u64 {
        self.scope.generation_id()
    }

    /// See [`AttemptScope::deadline`].
    pub fn deadline(&self) -> Instant {
        self.scope.deadline()
    }

    /// See [`AttemptScope::depth`].
    pub fn depth(&self) -> u32 {
        self.scope.depth()
    }

    /// See [`AttemptScope::is_cancelled`].
    pub fn is_cancelled(&self) -> bool {
        self.scope.is_cancelled()
    }

    /// See [`AttemptScope::invocation_id`].
    pub fn invocation_id(&self) -> u64 {
        self.scope.invocation_id()
    }

    /// See [`AttemptScope::parent_invocation_id`].
    pub fn parent_invocation_id(&self) -> Option<u64> {
        self.scope.parent_invocation_id()
    }

    /// See [`AttemptScope::connection`].
    pub fn connection(&self) -> &ConnectionReference {
        self.scope.connection()
    }

    /// See [`AttemptScope::effective_identity`].
    pub fn effective_identity(&self) -> &EffectiveConnectionIdentity {
        self.scope.effective_identity()
    }

    /// See [`AttemptScope::operation`].
    pub fn operation(&self) -> &str {
        self.scope.operation()
    }

    /// See [`AttemptScope::idempotency_key`].
    pub fn idempotency_key(&self) -> Option<&str> {
        self.scope.idempotency_key()
    }

    /// See [`AttemptScope::effect_status`].
    pub fn effect_status(&self) -> EffectStatus {
        self.scope.effect_status()
    }

    /// See [`AttemptScope::record_effect`].
    pub fn record_effect(&self, observed: EffectStatus) -> EffectStatus {
        self.scope.record_effect(observed)
    }

    /// Governed connection configuration and secret resolver for the
    /// connection revision this attempt runs over, when bound to a host store.
    pub fn connection_resolver(&self) -> Option<&dyn ConnectionResolver> {
        self.scope.connection_resolver()
    }

    /// The governed connection revision's `public_config`, when bound to a host
    /// store.
    pub fn public_config(&self) -> Option<&Value> {
        self.connection_resolver()
            .and_then(ConnectionResolver::public_config)
    }

    /// Resolve the secret locator at `path` for this attempt's connection
    /// revision, when bound to a host store.
    pub async fn resolve_secret(&self, path: &str) -> Result<Option<SecretValue>, ConnectorError> {
        match self.connection_resolver() {
            Some(resolver) => resolver.resolve_secret(path).await,
            None => Ok(None),
        }
    }

    /// Return `public_config` with all configured secret locators resolved and
    /// merged at their dotted paths, when bound to a host store.
    pub async fn resolve_merged_config(&self) -> Result<Option<Value>, ConnectorError> {
        match self.connection_resolver() {
            Some(resolver) => resolver.resolve_merged_config().await,
            None => Ok(None),
        }
    }

    /// Call `operation` on the connection bound to the dependency `alias`.
    ///
    /// The call shares this attempt's deadline, budget and effect evidence:
    /// an effect the dependency records is this call's effect too. Refused
    /// with `DEPENDENCY_UNDECLARED` when the host binds no dependencies, or
    /// the connection binds nothing to `alias`, and with
    /// `OPERATION_FORBIDDEN` for an operation the dependency does not
    /// declare.
    pub async fn call_dependency(
        &self,
        alias: &str,
        operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError> {
        match self.scope.dependency_caller() {
            Some(caller) => caller.call(alias, operation, input).await,
            None => Err(ConnectorError::forbidden(
                codes::DEPENDENCY_UNDECLARED,
                "dispatch",
                format!("no dependency '{alias}' is bound for this call"),
                self.effect_status(),
            )),
        }
    }

    /// The connection revision bound to `alias`, when one is: how a provider
    /// tells an unbound optional dependency from a bound one before calling.
    pub fn bound_connection(&self, alias: &str) -> Option<ConnectionReference> {
        self.scope.dependency_caller()?.bound(alias)
    }

    /// Refuse to go on once the run is cancelled or past its deadline,
    /// carrying the effect evidence recorded so far.
    pub fn check_live(&self) -> Result<(), ConnectorError> {
        let effect = self.effect_status();
        if self.is_cancelled() {
            return Err(ConnectorError::cancelled(effect));
        }
        if Instant::now() >= self.deadline() {
            return Err(ConnectorError::deadline_exceeded(effect));
        }
        Ok(())
    }

    /// The host's own scope, when it is a `T`: how the node gets its
    /// invocation state back from a context it handed out.
    pub fn scope<T: AttemptScope>(&self) -> Option<&T> {
        let scope: &dyn Any = self.scope.as_ref();
        scope.downcast_ref::<T>()
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::{Digest, ProviderRef, Revision};
    use std::future::Future as _;

    /// A scope over a fixed value: what a provider's own tests hand it.
    #[derive(Debug)]
    struct Fixed {
        connection: ConnectionReference,
        identity: EffectiveConnectionIdentity,
        deadline: Instant,
        cancelled: AtomicBool,
        effect: Mutex<EffectStatus>,
    }

    impl AttemptScope for Fixed {
        fn run_id(&self) -> &str {
            "run-1"
        }
        fn generation_id(&self) -> u64 {
            7
        }
        fn deadline(&self) -> Instant {
            self.deadline
        }
        fn depth(&self) -> u32 {
            0
        }
        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
        fn invocation_id(&self) -> u64 {
            1
        }
        fn parent_invocation_id(&self) -> Option<u64> {
            None
        }
        fn connection(&self) -> &ConnectionReference {
            &self.connection
        }
        fn effective_identity(&self) -> &EffectiveConnectionIdentity {
            &self.identity
        }
        fn operation(&self) -> &str {
            "query"
        }
        fn idempotency_key(&self) -> Option<&str> {
            None
        }
        fn effect_status(&self) -> EffectStatus {
            *self.effect.lock().unwrap()
        }
        fn record_effect(&self, observed: EffectStatus) -> EffectStatus {
            let mut effect = self.effect.lock().unwrap();
            *effect = effect.merge(observed);
            *effect
        }
    }

    fn fixed(deadline: Instant) -> Arc<Fixed> {
        let digest = Digest::parse(&format!("sha256:{}", "ab".repeat(32))).unwrap();
        Arc::new(Fixed {
            connection: ConnectionReference {
                connection_id: "orders".into(),
                revision: Revision::FIRST,
                provider: ProviderRef {
                    publisher: "acme".to_string(),
                    provider_id: "acme.db".into(),
                    version: "1.0.0".to_string(),
                    digest: digest.clone(),
                },
            },
            identity: EffectiveConnectionIdentity {
                provider_digest: digest,
                connection_id: "orders".into(),
                revision: Revision::FIRST,
                secret_hash: String::new(),
            },
            deadline,
            cancelled: AtomicBool::new(false),
            effect: Mutex::new(EffectStatus::NotStarted),
        })
    }

    #[test]
    fn the_view_reads_the_scope_and_records_only_monotonic_evidence() {
        let scope = fixed(Instant::now() + Duration::from_secs(60));
        let ctx = AttemptContext::new(scope.clone(), 2);
        assert_eq!(ctx.attempt_number(), 2);
        assert_eq!(ctx.generation_id(), 7);
        assert_eq!(ctx.connection().connection_id, "orders");
        assert!(ctx.check_live().is_ok());

        assert_eq!(
            ctx.record_effect(EffectStatus::Unknown),
            EffectStatus::Unknown
        );
        assert_eq!(
            ctx.record_effect(EffectStatus::NotStarted),
            EffectStatus::Unknown
        );

        scope.cancelled.store(true, Ordering::SeqCst);
        let err = ctx.check_live().unwrap_err();
        assert_eq!(err.code, crate::codes::CANCELLED);
        assert_eq!(err.effect_status, EffectStatus::Unknown);
    }

    /// A host that binds no dependency caller refuses every dependency call
    /// as undeclared, carrying the effect evidence so far, and reports no
    /// bound connection.
    #[test]
    fn a_scope_without_a_dependency_caller_refuses_with_its_effect() {
        let scope = fixed(Instant::now() + Duration::from_secs(60));
        let ctx = AttemptContext::new(scope, 1);
        ctx.record_effect(EffectStatus::Unknown);
        // The refusal awaits nothing, so one poll answers it (no runtime in
        // this crate's dependencies).
        let call = std::pin::pin!(ctx.call_dependency("artifact_store", "presign", Value::Null));
        let std::task::Poll::Ready(result) =
            call.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        else {
            unreachable!("the refusal is immediate");
        };
        let err = result.unwrap_err();
        assert_eq!(err.code, crate::codes::DEPENDENCY_UNDECLARED);
        assert_eq!(err.effect_status, EffectStatus::Unknown);
        assert!(err.message.contains("artifact_store"), "{}", err.message);
        assert!(ctx.bound_connection("artifact_store").is_none());
    }

    #[test]
    fn a_passed_deadline_refuses_and_the_host_gets_its_scope_back() {
        let ctx = AttemptContext::new(fixed(Instant::now()), 1);
        assert_eq!(
            ctx.check_live().unwrap_err().code,
            crate::codes::DEADLINE_EXCEEDED
        );
        assert!(ctx.scope::<Fixed>().is_some());
        #[derive(Debug)]
        struct Other;
        impl AttemptScope for Other {
            fn run_id(&self) -> &str {
                unreachable!()
            }
            fn generation_id(&self) -> u64 {
                unreachable!()
            }
            fn deadline(&self) -> Instant {
                unreachable!()
            }
            fn depth(&self) -> u32 {
                unreachable!()
            }
            fn is_cancelled(&self) -> bool {
                unreachable!()
            }
            fn invocation_id(&self) -> u64 {
                unreachable!()
            }
            fn parent_invocation_id(&self) -> Option<u64> {
                unreachable!()
            }
            fn connection(&self) -> &ConnectionReference {
                unreachable!()
            }
            fn effective_identity(&self) -> &EffectiveConnectionIdentity {
                unreachable!()
            }
            fn operation(&self) -> &str {
                unreachable!()
            }
            fn idempotency_key(&self) -> Option<&str> {
                unreachable!()
            }
            fn effect_status(&self) -> EffectStatus {
                unreachable!()
            }
            fn record_effect(&self, _: EffectStatus) -> EffectStatus {
                unreachable!()
            }
        }
        assert!(ctx.scope::<Other>().is_none());
    }
}
