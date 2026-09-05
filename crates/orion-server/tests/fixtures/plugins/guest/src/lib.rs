//! A plugin that does what its function name says. The dispatch is on the
//! last label of the registered name, so the same component serves under
//! any plugin id the manifest gives it.
//!
//! Written against `orion-plugin-sdk`, so the fixture that proves the host
//! is also the component that proves the SDK. `invoke_raw` is overridden
//! rather than `invoke`, because several functions here exist to return what
//! the JSON boundary would never let through — text that is not JSON, a
//! result too large to be worth parsing — and the host must refuse those.

use orion_plugin_sdk::{Plugin, PluginError, export_plugin};

struct Fixture;

impl Plugin for Fixture {
    fn invoke(
        _function: &str,
        _input: orion_plugin_sdk::Value,
    ) -> Result<orion_plugin_sdk::Value, PluginError> {
        unreachable!("the raw boundary is overridden")
    }

    fn invoke_raw(function: &str, input: &str) -> Result<String, PluginError> {
        let label = function.rsplit('.').next().unwrap_or("");
        match label {
            // The well-behaved ones.
            "identity" => Ok(input.to_string()),
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
                    .map(|rest| {
                        rest.chars()
                            .take_while(|c| c.is_ascii_digit())
                            .collect::<String>()
                    })
                    .and_then(|digits| digits.parse().ok())
                    .unwrap_or(1024);
                let mut s = String::with_capacity(size + 2);
                s.push('"');
                s.extend(core::iter::repeat_n('x', size));
                s.push('"');
                Ok(s)
            }

            // Bad results.
            "bad-json" => Ok("not json".to_string()),
            "bad-code" => Err(PluginError::internal(
                "lowercase code",
                "a code outside the grammar",
            )),
            "caller-input-error" => Err(PluginError::caller_input(
                "BAD_MESSAGE",
                "the message is not ISO 8583",
            )),
            "internal-error" => Err(PluginError::internal(
                "BOOM",
                "the guest failed\nwith a newline",
            )),
            other => Err(PluginError::caller_input(
                "UNKNOWN_FUNCTION",
                format!("this component exports no '{other}'"),
            )),
        }
    }
}

export_plugin!(Fixture);
