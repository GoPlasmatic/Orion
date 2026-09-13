//! How a model failure becomes a task error.
//!
//! One table, in the categories the metrics count by:
//!
//! | Category | Condition | Class | Retryable |
//! |---|---|---|---|
//! | `caller_input` | the message does not marshal: a missing field, a value the adapter cannot turn into the declared tensor | `CallerInput` | no |
//! | `unavailable` | the model is not loaded on this node (not admitted, not preloaded and the load failed, evicted) | `Backend` | no |
//! | `runtime_unavailable` | the runtime the row names is disabled or absent here | `Backend` | no |
//! | `adapter` | an adapter or the result expression errored at evaluation | `CallerInput` | no |
//! | `input_size` | inputs over `max_input_elements` | `Limit` | no |
//! | `output_size` | outputs over `max_output_elements` | `Limit` | no |
//! | `permit` | no concurrency permit before the deadline | `Limit` | no |
//! | `timeout` | the inference outlived its deadline | `Timeout` | yes — pure, so free |
//! | `run` | the runtime failed mid-graph | `Backend` | no |
//!
//! A runtime's own strings never reach a client: they go to the operator log
//! with the model, digest and trace id, which is the only place they mean
//! anything.

use crate::engine::{ErrorClass, HandlerError};

/// Why an inference failed, in the categories the metrics count by. Every
/// label value is one of these names, never a string a row or a message
/// chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// The message does not marshal into the declared inputs.
    CallerInput,
    /// The model is not loaded on this node.
    Unavailable,
    /// The runtime the row names is not offered here.
    RuntimeUnavailable,
    /// An adapter or the result expression errored.
    Adapter,
    /// Inputs over `max_input_elements`.
    InputSize,
    /// Outputs over `max_output_elements`.
    OutputSize,
    /// No concurrency permit before the deadline.
    Permit,
    /// Wall-clock deadline.
    Timeout,
    /// The runtime failed mid-graph.
    Run,
}

impl Category {
    /// Every category, for tests and for a catalogue.
    pub const ALL: [Category; 9] = [
        Category::CallerInput,
        Category::Unavailable,
        Category::RuntimeUnavailable,
        Category::Adapter,
        Category::InputSize,
        Category::OutputSize,
        Category::Permit,
        Category::Timeout,
        Category::Run,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CallerInput => "caller_input",
            Self::Unavailable => "unavailable",
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::Adapter => "adapter",
            Self::InputSize => "input_size",
            Self::OutputSize => "output_size",
            Self::Permit => "permit",
            Self::Timeout => "timeout",
            Self::Run => "run",
        }
    }

    pub fn class(self) -> ErrorClass {
        match self {
            Self::CallerInput | Self::Adapter => ErrorClass::CallerInput,
            Self::Unavailable | Self::RuntimeUnavailable | Self::Run => ErrorClass::Backend,
            Self::InputSize | Self::OutputSize | Self::Permit => ErrorClass::Limit,
            Self::Timeout => ErrorClass::Timeout,
        }
    }
}

/// A failure classified, with the client-safe message and the operator-only
/// detail kept apart.
#[derive(Debug)]
pub struct Failure {
    pub category: Category,
    /// Safe for a client: names the category and what the caller can act on.
    pub message: String,
    /// The host's own account — a runtime's error text, a tensor shape.
    /// Logged, never returned.
    pub detail: Option<String>,
}

impl Failure {
    pub fn new(category: Category, message: impl Into<String>) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The table in the module docs, pinned: every category has a distinct
    /// label, exactly one is retryable, and the classes are the ones stated.
    #[test]
    fn every_category_has_a_class_and_a_stable_label() {
        let mut labels: Vec<&str> = Category::ALL.iter().map(|c| c.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), Category::ALL.len(), "labels must be distinct");
        for c in Category::ALL {
            assert_eq!(
                c.class().is_retryable(),
                c == Category::Timeout,
                "{c:?}: only a timeout is retryable"
            );
        }
        let expected = [
            (
                Category::CallerInput,
                "caller_input",
                ErrorClass::CallerInput,
            ),
            (Category::Unavailable, "unavailable", ErrorClass::Backend),
            (
                Category::RuntimeUnavailable,
                "runtime_unavailable",
                ErrorClass::Backend,
            ),
            (Category::Adapter, "adapter", ErrorClass::CallerInput),
            (Category::InputSize, "input_size", ErrorClass::Limit),
            (Category::OutputSize, "output_size", ErrorClass::Limit),
            (Category::Permit, "permit", ErrorClass::Limit),
            (Category::Timeout, "timeout", ErrorClass::Timeout),
            (Category::Run, "run", ErrorClass::Backend),
        ];
        for (category, label, class) in expected {
            assert_eq!(category.as_str(), label);
            assert_eq!(category.class(), class, "{label}");
        }
    }

    #[test]
    fn a_failure_keeps_its_detail_out_of_the_message() {
        let f = Failure::new(Category::Run, "the model failed to run")
            .with_detail("tract: shape mismatch at node 12");
        let e = f.into_handler_error();
        assert_eq!(e.class, ErrorClass::Backend);
        assert_eq!(e.msg, "the model failed to run");
        assert_eq!(
            e.detail.as_deref(),
            Some("tract: shape mismatch at node 12")
        );
    }
}
