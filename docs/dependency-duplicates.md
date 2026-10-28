# Dependency duplicate review

Reviewed the locked workspace on 2026-10-28 while adopting dataflow-rs 4.0.3.
This is a maintenance record for [Orion #389](https://github.com/GoPlasmatic/Orion/issues/389).

The upgrade resolves dataflow-rs and its derive crate to 4.0.3 and UUID to 1.29.0.
Compatible published updates for reqwest, SQLx, dashmap, metrics-util,
OpenTelemetry SDK, sea-query, tabled and governor do not currently converge the
remaining versions. Keep `deny.toml`'s duplicate warnings enabled.

| Family | Locked versions | Why it remains |
|---|---|---|
| `hashbrown` | 0.12.3, 0.14.5, 0.16.1, 0.17.1 | indexmap 1, dashmap, SQLx/governor/metrics, and the Wasmtime/compiler/indexmap 2 branches require different versions. |
| `rand` | 0.8.8, 0.9.5, 0.10.3 | PHF's generator uses 0.8; Bollard/metrics/OpenTelemetry use 0.9; Orion, SQLx and UUID use 0.10. |
| `getrandom` | 0.2.17, 0.3.4, 0.4.3 | Crypto and random-number branches retain different requirements. Ring still needs 0.2 independently of Orion's own randomness. |
| `sha2` / `digest` | 0.10.x, 0.11.x | SQLx core/macros and Wasmtime/Cranelift retain SHA-2 0.10. Orion and the SQLx network drivers use 0.11. |
| `syn` / `darling` | syn 2/3, darling 0.20/0.24 | Different derive-macro branches require them, including SQLx, sea-query, Wasmtime and serde. These are build-time dependencies. |
| `tower-http` | 0.6.11, 0.7.1 | Published reqwest 0.13.5 uses 0.6; Orion HTTP uses 0.7. Recheck a reqwest release containing its upstream 0.7 update. |
| `wasm-encoder` and related tooling | 0.258.3, 0.262.0 | Wasmtime and component generation use different toolchain releases. Align them only with a compatible runtime/SDK release and plugin regression checks. |
| `wit-bindgen` | 0.57.1, 0.63.0 | Target-specific `wasip2` requires 0.57; Orion's plugin SDK uses 0.63. Native server builds do not necessarily link both. |

Reproduce the graph from the workspace root:

```sh
cargo tree --workspace --locked --duplicates
cargo tree --workspace --locked -i tower-http@0.6.11
cargo tree --workspace --locked -i sha2@0.10.9
```

Revisit each branch when its owning dependency publishes a compatible update.
Do not force incompatible replacements, downgrade Orion's maintained APIs, or
carry forks just to reduce the duplicate count. Source and license policy still
applies to every resolved version. Validate component build/load behaviour after
changing the Wasm toolchain.
