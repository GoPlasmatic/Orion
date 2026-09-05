//! The guest side of Orion's plugin ABI.
//!
//! A plugin function is a pure JSON → JSON transformation: Orion evaluates
//! the task's `function.input`, hands it to the component as one JSON value,
//! and writes the value the component returns at the `output` path the
//! workflow author chose. The world the component implements imports
//! nothing — no filesystem, clock, randomness, sockets, logging, connectors
//! or secrets — so everything a function knows arrives in its input and
//! everything it does leaves in its return value.
//!
//! Implement [`Plugin`] for a type and hand it to [`export_plugin!`]:
//!
//! ```ignore
//! use orion_plugin_sdk::{Plugin, PluginError, Value, export_plugin};
//!
//! struct Codec;
//!
//! impl Plugin for Codec {
//!     fn invoke(function: &str, input: Value) -> Result<Value, PluginError> {
//!         match function {
//!             "acme.codec.parse" => parse(&input),
//!             other => Err(PluginError::caller_input(
//!                 "UNKNOWN_FUNCTION",
//!                 format!("this component exports no '{other}'"),
//!             )),
//!         }
//!     }
//! }
//!
//! export_plugin!(Codec);
//! ```
//!
//! Build the crate as a `cdylib` for `wasm32-unknown-unknown` — not a WASI
//! target, whose standard library would import WASI and be refused by the
//! world — and turn the core module into a component:
//!
//! ```text
//! cargo build --release --target wasm32-unknown-unknown
//! wasm-tools component new target/wasm32-unknown-unknown/release/my_plugin.wasm -o plugin.wasm
//! ```
//!
//! The manifest (`plugin.toml`) beside the component names the functions the
//! component exports and the fields each accepts; `orion-cli plugins create
//! -f plugin.toml` uploads both. The manifest's `abi` must equal [`ABI`].

#![forbid(unsafe_code)]

pub use serde_json::{self, Value, json};

/// The generated bindings for the `orion:plugin` world. Reached through
/// [`export_plugin!`]; a plugin needs them directly only to implement the
/// `Guest` trait by hand.
pub mod bindings {
    wit_bindgen::generate!({
        world: "plugin",
        path: "wit",
        pub_export_macro: true,
    });
}

pub use bindings::exports::orion::plugin::functions::ErrorClass;

/// The WIT package version these bindings speak, echoed as `abi` in a
/// manifest. A component built against a later world is refused at upload.
pub const ABI: &str = "orion:plugin@1.0.0";

/// A refusal the plugin chose to make.
///
/// `code` is a stable identifier matching `^[A-Z][A-Z0-9_]{0,63}$` — the host
/// refuses any other spelling — and the thing a workflow can branch on.
/// `message` is capped by the host and prefixed with the function name
/// before a client sees it. Neither class is retried: `CallerInput` says the
/// input was wrong for this function and the same input cannot succeed;
/// `Internal` says the plugin itself failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginError {
    pub code: String,
    pub class: ErrorClass,
    pub message: String,
}

impl PluginError {
    /// The input was well-formed to the schema but wrong for this function.
    pub fn caller_input(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            class: ErrorClass::CallerInput,
            message: message.into(),
        }
    }

    /// The plugin's own failure.
    pub fn internal(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            class: ErrorClass::Internal,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PluginError {}

impl From<PluginError> for bindings::exports::orion::plugin::functions::PluginError {
    fn from(e: PluginError) -> Self {
        Self {
            code: e.code,
            class: e.class,
            message: e.message,
        }
    }
}

/// What a plugin implements.
///
/// One component may export many functions; `function` is the registered
/// name — `<plugin>.<label>` as the manifest spelled it — so an
/// implementation dispatches on it. `input` is the task's evaluated input as
/// one JSON object: every declared field the task set, with `template_at`
/// fields evaluated and `resolvable` fields folded by the host, and never
/// `output`, which is the host's.
pub trait Plugin {
    /// Compute the value Orion writes at the task's `output`.
    fn invoke(function: &str, input: Value) -> Result<Value, PluginError>;

    /// The raw boundary: the input as the JSON text the host sent, the result
    /// as the JSON text the host will parse. The default parses, calls
    /// [`Self::invoke`] and serialises; override it only to bypass the JSON
    /// layer — a codec that emits JSON text directly, or a test fixture that
    /// deliberately returns something the host must refuse.
    fn invoke_raw(function: &str, input: &str) -> Result<String, PluginError> {
        let value: Value = serde_json::from_str(input).map_err(|e| {
            PluginError::internal(
                "BAD_INPUT_JSON",
                format!("the host sent input that is not JSON: {e}"),
            )
        })?;
        let out = Self::invoke(function, value)?;
        serde_json::to_string(&out)
            .map_err(|e| PluginError::internal("BAD_OUTPUT_JSON", e.to_string()))
    }
}

/// The export shim [`export_plugin!`] wires up — not an API.
#[doc(hidden)]
pub fn dispatch<P: Plugin>(
    function: String,
    input: String,
) -> Result<String, bindings::exports::orion::plugin::functions::PluginError> {
    P::invoke_raw(&function, &input).map_err(Into::into)
}

/// Export a [`Plugin`] implementation as the component's `orion:plugin`
/// world. Call it once, at crate level, in the `cdylib` crate.
#[macro_export]
macro_rules! export_plugin {
    ($plugin:ty) => {
        const _: () = {
            struct __OrionPluginGuest;

            impl $crate::bindings::exports::orion::plugin::functions::Guest for __OrionPluginGuest {
                fn invoke(
                    function: ::std::string::String,
                    input: ::std::string::String,
                ) -> ::core::result::Result<
                    ::std::string::String,
                    $crate::bindings::exports::orion::plugin::functions::PluginError,
                > {
                    $crate::dispatch::<$plugin>(function, input)
                }
            }

    $crate::bindings::export!(__OrionPluginGuest with_types_in $crate::bindings);
        };
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl Plugin for Echo {
        fn invoke(function: &str, input: Value) -> Result<Value, PluginError> {
            match function {
                "t.echo.identity" => Ok(input),
                "t.echo.fail" => Err(PluginError::caller_input("NOPE", "as asked")),
                other => Err(PluginError::internal("UNKNOWN_FUNCTION", other.to_string())),
            }
        }
    }

    #[test]
    fn the_default_raw_boundary_round_trips_json_and_maps_errors() {
        let out = dispatch::<Echo>("t.echo.identity".into(), r#"{"a":[1,2,{"b":null}]}"#.into())
            .expect("identity");
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap(),
            json!({"a": [1, 2, {"b": null}]})
        );

        let err = dispatch::<Echo>("t.echo.fail".into(), "{}".into()).expect_err("fails");
        assert_eq!(err.code, "NOPE");
        assert_eq!(err.class, ErrorClass::CallerInput);
        assert_eq!(err.message, "as asked");

        let err = dispatch::<Echo>("t.echo.identity".into(), "not json".into())
            .expect_err("bad input json");
        assert_eq!(err.code, "BAD_INPUT_JSON");
        assert_eq!(err.class, ErrorClass::Internal);
    }

    #[test]
    fn the_abi_is_the_wit_package_version() {
        let wit = include_str!("../wit/orion-plugin.wit");
        assert!(
            wit.contains(&format!("package {ABI};")),
            "{ABI} is not the WIT package"
        );
    }
}
