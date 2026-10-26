//! What a deployment store operation refuses with: a code from the closed
//! v2 registry (`orion_api::V2Code`), which carries the status kind the
//! admin API answers with, and a message.

pub use orion_api::{ErrorKind as ErrorClass, V2Code as DeploymentErrorKind};

/// A refusal: its registered [`kind`](Self::kind) and the message the admin
/// API answers with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message}", kind.as_str())]
pub struct DeploymentError {
    pub kind: DeploymentErrorKind,
    pub message: String,
}

impl DeploymentError {
    pub fn new(kind: DeploymentErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// A refusal another layer raised under its own `code` — a package or
    /// lock finding, an admission policy's rejection, a build issue. A code
    /// the registry holds is kept; any other is answered as `fallback`, with
    /// the code it was raised under kept in the message, so the wire code
    /// stays one a client can branch on.
    pub fn passed(code: &str, message: impl Into<String>, fallback: DeploymentErrorKind) -> Self {
        let message = message.into();
        match DeploymentErrorKind::parse(code) {
            Some(kind) => Self { kind, message },
            None => Self {
                kind: fallback,
                message: format!("{code}: {message}"),
            },
        }
    }

    /// The wire code.
    pub fn code(&self) -> &'static str {
        self.kind.as_str()
    }

    /// The HTTP class the admin API answers this refusal with.
    pub fn class(&self) -> ErrorClass {
        self.kind.kind()
    }

    pub(super) fn read(e: sqlx::Error) -> Self {
        Self::new(DeploymentErrorKind::DbRead, e.to_string())
    }

    pub(super) fn write(e: sqlx::Error) -> Self {
        Self::new(DeploymentErrorKind::DbWrite, e.to_string())
    }

    pub(super) fn corrupt(message: impl Into<String>) -> Self {
        Self::new(DeploymentErrorKind::CorruptPersistedState, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A code another layer raises that the registry holds keeps its kind;
    /// one it does not hold is answered as the caller's fallback, never as a
    /// code outside the registry.
    #[test]
    fn a_passed_code_is_kept_only_when_registered() {
        let kept = DeploymentError::passed(
            "PROVIDER_REVOKED",
            "policy",
            DeploymentErrorKind::AdmissionRejected,
        );
        assert_eq!(kept.kind, DeploymentErrorKind::ProviderRevoked);
        assert_eq!(kept.class(), ErrorClass::Forbidden);
        let other = DeploymentError::passed(
            "ENTITLEMENT_EXPIRED",
            "x",
            DeploymentErrorKind::AdmissionRejected,
        );
        assert_eq!(other.code(), "ADMISSION_REJECTED");
        assert_eq!(other.message, "ENTITLEMENT_EXPIRED: x");
        assert_eq!(other.class(), ErrorClass::Forbidden);
    }
}
