//! A plugin that does what its function name says. The dispatch is on the
//! last label of the registered name, so the same component serves under
//! any plugin id the manifest gives it.

wit_bindgen::generate!({
    world: "plugin",
    path: "../../../../wit",
});

use exports::orion::plugin::functions::{ErrorClass, Guest, PluginError};

struct Fixture;

impl Guest for Fixture {
    fn invoke(function: String, input: String) -> Result<String, PluginError> {
        let label = function.rsplit('.').next().unwrap_or("");
        match label {
            // The well-behaved ones.
            "identity" => Ok(input),
            "wrap" => Ok(format!("{{\"wrapped\":{input},\"len\":{}}}", input.len())),
            "upper" => Ok(input.to_uppercase()),

            // Host limits.
            "trap" => core::arch::wasm32::unreachable(),
            "alloc-forever" => {
                let mut kept: Vec<Vec<u8>> = Vec::new();
                loop {
                    let mut chunk = vec![0u8; 4 * 1024 * 1024];
                    chunk[0] = kept.len() as u8;
                    core::hint::black_box(&chunk);
                    kept.push(chunk);
                }
            }
            "spin" => {
                let mut n: u64 = 0;
                loop {
                    n = core::hint::black_box(n.wrapping_add(1));
                    if n == u64::MAX {
                        return Ok("\"unreachable\"".to_string());
                    }
                }
            }
            "big-output" => {
                // The input is the task's input object; `size` is its one
                // declared field. No JSON parser here — the digits after the
                // key are enough for a fixture.
                let size: usize = input
                    .split("\"size\":")
                    .nth(1)
                    .map(|rest| rest.trim_start())
                    .map(|rest| rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
                    .and_then(|digits| digits.parse().ok())
                    .unwrap_or(2 * 1024 * 1024);
                let mut s = String::with_capacity(size + 2);
                s.push('"');
                s.extend(core::iter::repeat('x').take(size));
                s.push('"');
                Ok(s)
            }

            // Bad results.
            "bad-json" => Ok("not json".to_string()),
            "bad-code" => Err(PluginError {
                code: "lowercase code".to_string(),
                class: ErrorClass::Internal,
                message: "a code outside the grammar".to_string(),
            }),
            "caller-input-error" => Err(PluginError {
                code: "BAD_MESSAGE".to_string(),
                class: ErrorClass::CallerInput,
                message: "the message is not ISO 8583".to_string(),
            }),
            "internal-error" => Err(PluginError {
                code: "BOOM".to_string(),
                class: ErrorClass::Internal,
                message: "the guest failed\nwith a newline".to_string(),
            }),
            other => Err(PluginError {
                code: "UNKNOWN_FUNCTION".to_string(),
                class: ErrorClass::CallerInput,
                message: format!("this component exports no '{other}'"),
            }),
        }
    }
}

export!(Fixture);
