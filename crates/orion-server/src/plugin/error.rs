//! How a plugin failure becomes a task error.
//!
//! Two sources, one table (`docs/src/reference/plugins.md`, "Errors"):
//!
//! | Source | Condition | Class | Retryable |
//! |---|---|---|---|
//! | guest | `caller-input` | `CallerInput` | no |
//! | guest | `internal` | `Backend` | no |
//! | host | fuel, memory, sizes, permit, instance pool | `Limit` | no |
//! | host | epoch or wall-clock deadline | `Timeout` | yes — pure, so free |
//! | host | trap, panic, unparseable result | `Backend` | no |
//!
//! A guest's own strings are untrusted: its `code` must match the ABI's
//! grammar, its `message` is capped, and Wasmtime's internals never reach a
//! client — they go to the operator log with the plugin, digest, function and
//! trace id, which is the only place they mean anything.

use crate::engine::{ErrorClass, HandlerError};

/// Longest guest message a client may see.
pub const MAX_GUEST_MESSAGE: usize = 512;

/// Why an invocation failed, in the categories the metrics count by. Every
/// label value is one of these names, never a guest string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// The guest returned `caller-input`.
    CallerInput,
    /// The guest returned `internal`.
    GuestError,
    /// The guest returned a code outside the ABI's grammar.
    BadCode,
    /// Serialised input over `max_request_bytes`.
    RequestSize,
    /// Returned JSON over `max_response_bytes`.
    ResponseSize,
    /// No concurrency permit before the deadline.
    Permit,
    /// The instance pool was full.
    Instances,
    /// Fuel exhausted.
    Fuel,
    /// A memory or table growth the host refused.
    Memory,
    /// Epoch or wall-clock deadline.
    Timeout,
    /// Any other trap.
    Trap,
    /// The guest returned something that is not JSON.
    BadResult,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CallerInput => "caller_input",
            Self::GuestError => "guest_error",
            Self::BadCode => "bad_code",
            Self::RequestSize => "request_size",
            Self::ResponseSize => "response_size",
            Self::Permit => "permit",
            Self::Instances => "instances",
            Self::Fuel => "fuel",
            Self::Memory => "memory",
            Self::Timeout => "timeout",
            Self::Trap => "trap",
            Self::BadResult => "bad_result",
        }
    }

    pub fn class(self) -> ErrorClass {
        match self {
            Self::CallerInput => ErrorClass::CallerInput,
            Self::GuestError | Self::BadCode | Self::Trap | Self::BadResult => ErrorClass::Backend,
            Self::RequestSize
            | Self::ResponseSize
            | Self::Permit
            | Self::Instances
            | Self::Fuel
            | Self::Memory => ErrorClass::Limit,
            Self::Timeout => ErrorClass::Timeout,
        }
    }
}

/// A failure classified, with the client-safe message and the operator-only
/// detail kept apart.
#[derive(Debug)]
pub struct Failure {
    pub category: Category,
    /// Safe for a client: names the category and, for a guest error, its code
    /// and capped message.
    pub message: String,
    /// The host's own account — a trap name, Wasmtime's error text. Logged,
    /// never returned.
    pub detail: Option<String>,
}

impl Failure {
    pub fn host(category: Category, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl std::fmt::Display) -> Self {
        self.detail = Some(detail.to_string());
        self
    }

    /// A refusal the guest chose to make. `code` is validated against the
    /// ABI grammar — a code outside it is the guest's bug, reported as such,
    /// so a client never keys on a string the plugin author did not commit
    /// to. `message` is capped.
    pub fn guest(code: &str, caller_input: bool, message: &str) -> Self {
        let message = cap(message);
        if !super::manifest::is_valid_error_code(code) {
            return Self {
                category: Category::BadCode,
                message: "the plugin returned an error code outside the ABI grammar \
                          (^[A-Z][A-Z0-9_]{0,63}$)"
                    .to_string(),
                detail: Some(format!("code={code:?} message={message:?}")),
            };
        }
        Self {
            category: if caller_input {
                Category::CallerInput
            } else {
                Category::GuestError
            },
            message: format!("{code}: {message}"),
            detail: None,
        }
    }

    /// Classify what Wasmtime reported. `memory_refused` is the limiter's
    /// flag: a trap after a refused growth is the guest running out of the
    /// memory it was given, which is a limit and not a defect.
    pub fn from_wasmtime(err: &wasmtime::Error, memory_refused: bool) -> Self {
        if let Some(trap) = err.downcast_ref::<wasmtime::Trap>() {
            let category = match trap {
                wasmtime::Trap::OutOfFuel => Category::Fuel,
                wasmtime::Trap::Interrupt => Category::Timeout,
                _ if memory_refused => Category::Memory,
                _ => Category::Trap,
            };
            return Self {
                category,
                message: match category {
                    Category::Fuel => "the invocation exhausted its fuel backstop".to_string(),
                    Category::Timeout => "the invocation exceeded its deadline".to_string(),
                    Category::Memory => "the invocation exceeded its memory limit".to_string(),
                    _ => "the plugin trapped".to_string(),
                },
                detail: Some(format!("trap: {trap:?}")),
            };
        }
        if memory_refused {
            return Self::host(Category::Memory, "the invocation exceeded its memory limit")
                .with_detail(err);
        }
        // Pool exhaustion is reported by the allocator as a plain error; the
        // message is the only signal, and a wrong guess here costs a
        // `backend` label where `instances` was right, never a wrong class
        // for the caller — both are non-retryable failures of this node.
        let text = err.to_string();
        if text.contains("concurrent") && text.contains("limit") {
            return Self::host(
                Category::Instances,
                "no plugin instance was available; the pool is full",
            )
            .with_detail(text);
        }
        Self::host(Category::Trap, "the plugin failed to run").with_detail(text)
    }

    /// Into the engine's error vocabulary, message prefixed with the function
    /// name by the caller.
    pub fn into_handler_error(self) -> HandlerError {
        let mut e = HandlerError::new(self.category.class(), self.message);
        if let Some(detail) = self.detail {
            e = e.with_detail(detail);
        }
        e
    }
}

/// Cap a guest string, on a char boundary, and strip control characters so
/// a message cannot break a log line or a terminal.
fn cap(message: &str) -> String {
    let clean: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.chars().count() <= MAX_GUEST_MESSAGE {
        return clean;
    }
    let mut out: String = clean.chars().take(MAX_GUEST_MESSAGE).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_category_has_a_class_and_a_stable_label() {
        let all = [
            Category::CallerInput,
            Category::GuestError,
            Category::BadCode,
            Category::RequestSize,
            Category::ResponseSize,
            Category::Permit,
            Category::Instances,
            Category::Fuel,
            Category::Memory,
            Category::Timeout,
            Category::Trap,
            Category::BadResult,
        ];
        let mut labels: Vec<&str> = all.iter().map(|c| c.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), all.len(), "labels must be distinct");
        assert!(Category::Timeout.class().is_retryable());
        for c in all {
            if c != Category::Timeout {
                assert!(!c.class().is_retryable(), "{c:?} must not be retryable");
            }
        }
    }

    #[test]
    fn a_guest_error_keeps_its_code_and_caps_its_message() {
        let f = Failure::guest("BAD_MESSAGE", true, "not ISO 8583");
        assert_eq!(f.category, Category::CallerInput);
        assert_eq!(f.message, "BAD_MESSAGE: not ISO 8583");
        let long = "x".repeat(2000);
        let f = Failure::guest("BOOM", false, &long);
        assert_eq!(f.category, Category::GuestError);
        assert!(f.message.chars().count() < 600);
        assert!(f.message.ends_with('…'));
        let f = Failure::guest("E", false, "a\nb\u{1b}c");
        assert_eq!(f.message, "E: a b c");
    }

    #[test]
    fn a_code_outside_the_grammar_is_the_guests_bug() {
        let f = Failure::guest("bad code", true, "x");
        assert_eq!(f.category, Category::BadCode);
        assert_eq!(f.category.class(), ErrorClass::Backend);
        assert!(!f.message.contains("bad code"), "{}", f.message);
        assert!(f.detail.as_deref().is_some_and(|d| d.contains("bad code")));
    }

    #[test]
    fn a_trap_after_a_refused_growth_is_a_limit() {
        let err = wasmtime::Error::from(wasmtime::Trap::UnreachableCodeReached);
        assert_eq!(
            Failure::from_wasmtime(&err, true).category,
            Category::Memory
        );
        assert_eq!(Failure::from_wasmtime(&err, false).category, Category::Trap);
        let fuel = wasmtime::Error::from(wasmtime::Trap::OutOfFuel);
        assert_eq!(
            Failure::from_wasmtime(&fuel, false).category,
            Category::Fuel
        );
        let epoch = wasmtime::Error::from(wasmtime::Trap::Interrupt);
        assert_eq!(
            Failure::from_wasmtime(&epoch, false).category,
            Category::Timeout
        );
        let f = Failure::from_wasmtime(&err, false);
        assert!(
            f.detail
                .as_deref()
                .is_some_and(|d| d.contains("Unreachable"))
        );
        assert!(
            !f.message.contains("Unreachable"),
            "internals stay out of the client message"
        );
    }
}
