//! `env://` in the config file, and the `[storage]` scheme check (#311).
//!
//! A connector naming a Postgres database takes a reference and resolves it;
//! the server's own state database could not, so the same credential ended up
//! in two shapes in one deployment. Worse, `validate-config` — the command
//! whose whole job is to catch a config that cannot boot — exited `0` on the
//! reference and the server then died at startup.

use crate::common;

const REF_ENV: &str = "ORION_SECRET_CONFIG_REF_STATE_DB";
const REF_VALUE: &str = "postgres://orion:s3cret@db.internal:5432/orion";

/// Set once, before anything reads the environment. `std::env::set_var` is
/// `unsafe` in Rust 2024 because another thread may be reading; these run at
/// the top of each test that needs the value.
fn set_ref_env() {
    // SAFETY: the value is constant, so a concurrent reader sees either the
    // absence or this exact string — never a torn or unexpected one.
    unsafe { std::env::set_var(REF_ENV, REF_VALUE) };
}

fn write(dir: &std::path::Path, name: &str, content: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write fixture");
    path.to_string_lossy().into_owned()
}

/// The asymmetry the issue is about, closed: `[storage]` names its source the
/// way a connector does.
#[test]
fn storage_url_takes_an_env_reference() {
    set_ref_env();
    let scratch = common::ScratchDir::new("config-ref-storage");
    let path = write(
        scratch.path(),
        "orion.toml",
        &format!("[storage]\nurl = \"env://{REF_ENV}\"\n"),
    );

    let config = orion::config::load_config(Some(&path)).expect("a referenced URL loads");
    assert_eq!(config.storage.url, REF_VALUE);
    assert_eq!(
        orion::storage::detect_backend(&config.storage.url).expect("a real scheme"),
        orion::storage::DbBackend::Postgres
    );
}

/// Not a `[storage]` special case. A per-field allowlist would have made every
/// other credential in the file its own request.
#[test]
fn a_reference_resolves_anywhere_in_the_file() {
    set_ref_env();
    let scratch = common::ScratchDir::new("config-ref-anywhere");
    let path = write(
        scratch.path(),
        "orion.toml",
        &format!(
            "[storage]\nurl = \"sqlite::memory:\"\n\n\
             [admin_auth]\nenabled = true\napi_keys = [\"env://{REF_ENV}\"]\n"
        ),
    );

    let config = orion::config::load_config(Some(&path)).expect("loads");
    assert_eq!(config.admin_auth.api_keys, vec![REF_VALUE.to_string()]);
}

/// Strict, exactly as `${VAR}` with no `:-default` already is. A config file is
/// per-deployment, so "validates here, cannot boot there" is the outcome worth
/// preventing — and the message has to name the variable, or the operator is
/// left diffing environments.
#[test]
fn an_unset_reference_names_the_variable_and_the_field() {
    let scratch = common::ScratchDir::new("config-ref-unset");
    let path = write(
        scratch.path(),
        "orion.toml",
        "[storage]\nurl = \"env://ORION_SECRET_CONFIG_REF_DEFINITELY_UNSET\"\n",
    );

    let err = orion::config::load_config(Some(&path)).expect_err("must not load");
    let message = err.to_string();
    assert!(
        message.contains("ORION_SECRET_CONFIG_REF_DEFINITELY_UNSET"),
        "{message}"
    );
    assert!(message.contains("storage.url"), "{message}");
}

/// `vault://` resolves for a connector and cannot resolve here: the config is
/// what tells the process how to reach a network at all. Refused by name, so
/// the difference is stated rather than discovered when the URL reaches
/// Postgres as nine literal characters.
#[test]
fn a_vault_reference_in_the_config_file_is_refused_with_the_reason() {
    let scratch = common::ScratchDir::new("config-ref-vault");
    let path = write(
        scratch.path(),
        "orion.toml",
        "[storage]\nurl = \"vault://secret/data/orion#url\"\n",
    );

    let err = orion::config::load_config(Some(&path)).expect_err("must not load");
    let message = err.to_string();
    assert!(message.contains("vault://"), "{message}");
    assert!(message.contains("[secrets]"), "{message}");
}

/// The half that actually prevents the outage. The scheme check already
/// existed — inside `if config.cluster.enabled && …`, where `&&` short-circuits
/// it away for every single-node deployment, which is nearly all of them.
#[test]
fn an_unbootable_storage_url_fails_validation_outside_cluster_mode() {
    let scratch = common::ScratchDir::new("config-ref-scheme");
    for url in [
        // What the issue reported: a reference the loader did not understand.
        "notascheme://ORION_STATE_DB_URL",
        // And the ordinary typo it also lets through.
        "postgre://orion@db/orion",
    ] {
        let path = write(
            scratch.path(),
            "orion.toml",
            &format!("[storage]\nurl = \"{url}\"\n"),
        );
        let err = orion::config::load_config(Some(&path))
            .expect_err(&format!("'{url}' must not validate"));
        let message = err.to_string();
        assert!(
            message.contains("Unsupported database URL scheme"),
            "'{url}': {message}"
        );
    }
}

/// A resolved credential must not then be printed back by the command an
/// operator runs to inspect the config.
#[test]
fn a_resolved_password_is_still_masked_on_read_back() {
    set_ref_env();
    let scratch = common::ScratchDir::new("config-ref-mask");
    let path = write(
        scratch.path(),
        "orion.toml",
        &format!("[storage]\nurl = \"env://{REF_ENV}\"\n"),
    );
    let config = orion::config::load_config(Some(&path)).expect("loads");

    let mut tree =
        serde_json::to_value(toml::Value::try_from(&config).expect("toml")).expect("json");
    orion::connector::mask_secrets(&mut tree);

    let url = tree["storage"]["url"].as_str().expect("a url");
    assert!(
        !url.contains("s3cret"),
        "the resolved password must not survive a masked read: {url}"
    );
    assert!(
        url.contains("db.internal"),
        "the host is not a secret: {url}"
    );
}
