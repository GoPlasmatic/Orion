//! Real-Vault coverage for the `vault://` secret resolver (T39).
//!
//! The in-crate unit tests exercise the resolver against a hand-rolled fake
//! Vault; nothing had ever pointed it at the real server, so a Vault-side
//! change in response shape or status usage would surface first in a user's
//! deployment. This drives `VaultSecretResolver` against a genuine
//! `hashicorp/vault` dev server: the KV v2 read path, fail-closed behaviour
//! for a missing field, and a bad token.
//!
//! Constructed via `VaultSecretResolver::new` rather than `from_env`:
//! `VAULT_ADDR` depends on the container's mapped port, and mutating the
//! process environment mid-test is unsound in a multi-threaded binary (the
//! `from_env` half is covered by the unit tests).

use orion::connector::secrets::{SecretResolver, VaultSecretResolver};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test integration -- --ignored vault_test"]
async fn vault_resolver_reads_a_real_kv_v2_server_and_fails_closed() {
    let container = GenericImage::new("hashicorp/vault", "1.15")
        .with_exposed_port(8200.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Root Token:"))
        .with_env_var("VAULT_DEV_ROOT_TOKEN_ID", "orion-test-root")
        .with_env_var("VAULT_DEV_LISTEN_ADDRESS", "0.0.0.0:8200")
        .start()
        .await
        .expect("start vault container");
    let port = container
        .get_host_port_ipv4(8200.tcp())
        .await
        .expect("vault port");
    let addr = format!("http://127.0.0.1:{port}");

    // Seed a KV v2 secret through the real API (dev servers mount `secret/`
    // as v2, hence the `data/` segment in both the write and the reference).
    let client = reqwest::Client::new();
    let seed = client
        .post(format!("{addr}/v1/secret/data/db-creds"))
        .header("X-Vault-Token", "orion-test-root")
        .json(&serde_json::json!({"data": {"password": "s3cr3t-from-vault"}}))
        .send()
        .await
        .expect("seed request");
    assert!(seed.status().is_success(), "seed failed: {}", seed.status());

    let resolver = VaultSecretResolver::new(addr.clone(), "orion-test-root");
    let value = resolver
        .resolve("secret/data/db-creds#password")
        .await
        .expect("a seeded KV v2 field must resolve");
    assert_eq!(value, "s3cr3t-from-vault");

    // Fail closed: a missing field is an error, and the error must not quote
    // the response body (the body of a successful read IS the secret).
    let err = resolver
        .resolve("secret/data/db-creds#no_such_field")
        .await
        .expect_err("a missing field must not resolve");
    let message = err.client_message();
    assert!(
        !message.contains("s3cr3t-from-vault"),
        "the error must not leak sibling fields: {message}"
    );

    // A wrong token is an error, never a value.
    VaultSecretResolver::new(addr, "wrong-token")
        .resolve("secret/data/db-creds#password")
        .await
        .expect_err("a rejected token must fail resolution");
}
