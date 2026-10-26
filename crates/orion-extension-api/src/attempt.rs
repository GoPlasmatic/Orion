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

use crate::contract::{ConnectorError, EffectStatus};
use crate::identity::{ConnectionReference, EffectiveConnectionIdentity};

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
