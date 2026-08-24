//! Startup refusal for `ORION_*` variables that are not overrides (C4d).
//!
//! C4b closed the config *file*: `deny_unknown_fields` means a misspelled key
//! or section fails to deserialize and names itself. The environment half
//! stayed open, because overrides are matched **by name** — `env_overrides.rs`
//! asks for each variable it knows about and never looks at the rest of the
//! environment — so `ORION_SERVER__PORTT=3000` did exactly nothing, quietly,
//! and the operator learned about it from a port number in a log line.
//!
//! This module closes it from the other end: take the set of names the
//! overrides actually consult ([`super::env_overrides::known_env_override_keys`],
//! derived by running them against a recording reader, so it cannot drift from
//! the macros) and refuse to start when the process environment holds a
//! variable that *looks like* one of them and is not.
//!
//! # What counts as looking like one
//!
//! The `ORION_` prefix alone is not enough, and assuming it was is what made
//! the first cut of this guard unbootable on Kubernetes. Unless a PodSpec sets
//! `enableServiceLinks: false`, the kubelet injects a Docker-link-style block
//! of variables for every Service in the pod's namespace, named after the
//! *Service*: a Service called `orion` puts `ORION_SERVICE_HOST`, `ORION_PORT`,
//! `ORION_PORT_8080_TCP_ADDR` and a dozen more into every container, written by
//! the kubelet rather than by any manifest an operator can edit. Sibling tools
//! collide the same way — `ORION_SERVER_URL` and `ORION_API_KEY` are
//! `orion-cli`'s, and a shell that exports them for the CLI hands them to a
//! server started from that shell.
//!
//! Enumerating those collisions would be a losing game. The narrowing is
//! structural instead: **an override is `ORION_` + the config field path,
//! uppercased, with [`SECTION_SEPARATOR`] between levels**, so every override
//! naming a nested field carries a `__`, and a name without one cannot be a
//! misspelling of one. `ORION_PORT` is not a typo for anything Orion has; it is
//! somebody else's variable that happens to share a prefix, and Orion says
//! nothing about it. See [`could_be_a_misspelled_override`] for the one
//! wrinkle: a setting at the top level of the config has a single-segment path
//! and so no separator (`ORION_ENVIRONMENT` is the only one today), and those
//! keep a near-miss check of their own so the guard still covers them.
//!
//! Three further kinds of variable do carry the separator, or would, and must
//! still not be refused:
//!
//! 1. **Retired names.** `retired_env` runs first and produces a far more
//!    useful error — *"`ORION_QUEUE__WORKERS` -> `ORION_TRACE_QUEUE__WORKERS`"*.
//!    They are skipped here so that error can never be displaced by a generic
//!    "unknown variable" one. That pass probes exact names and is unaffected
//!    by the narrowing above, so separator-less `ORION_ENV` still reports its
//!    rename.
//! 2. **Names the config file references.** `${VAR}` substitution runs over
//!    the raw file before TOML parsing and reads arbitrary variables; a file
//!    saying `url = "${ORION_DB_URL}"` makes that variable Orion's to read.
//!    The same substitution also runs over every connector's `config_json`,
//!    whose names live in the database and cannot be enumerated here — those
//!    belong under [`RESERVED_PREFIX`]. `load_config` passes the file's names
//!    in.
//! 3. **The reserved [`RESERVED_PREFIX`] namespace.** `env://` connector
//!    secrets (`connector/secrets.rs`) name arbitrary variables too, and the
//!    connectors live in the database — there is no way to enumerate them
//!    while the config is loading. Anything under `ORION_SECRET_*` is
//!    therefore never interpreted as configuration.

use std::collections::BTreeSet;

use crate::errors::OrionError;

/// The one `ORION_*` namespace Orion never interprets as configuration.
///
/// Use it for values *you* reference: an `env://ORION_SECRET_DB_PASSWORD`
/// connector secret, or a `${ORION_SECRET_…}` placeholder in the config file.
/// Everything else that follows the override grammar is a setting, and a name
/// that follows it without being one is a startup error.
pub const RESERVED_PREFIX: &str = "ORION_SECRET_";

/// The prefix that makes a variable Orion's business.
const ORION_PREFIX: &str = "ORION_";

/// What an override puts between config-path levels — `server.port` becomes
/// `ORION_SERVER__PORT`. This is the grammar the scan recognises candidates
/// by; see the module docs.
const SECTION_SEPARATOR: &str = "__";

/// Edit distance below which a name is close enough to be worth suggesting.
/// One to three transposed or duplicated characters is the shape of a real
/// typo; beyond that the "nearest" key is noise.
///
/// Also the window in which a separator-less name is treated as a candidate at
/// all — see [`could_be_a_misspelled_override`].
const SUGGESTION_MAX_DISTANCE: usize = 3;

/// Refuse to start when the environment holds a variable that follows the
/// override grammar without being an override, reporting every offender at
/// once with the nearest valid key where there is an obvious one.
///
/// `present` is the process environment's variable names, `known` the set the
/// overrides consult, and `referenced_by_config_file` the names the config
/// file's `${VAR}` placeholders resolve.
pub(super) fn reject_unknown_env_vars<I>(
    present: I,
    known: &BTreeSet<String>,
    referenced_by_config_file: &BTreeSet<String>,
) -> Result<(), OrionError>
where
    I: IntoIterator<Item = String>,
{
    let top_level = top_level_keys(known);

    let mut offenders: Vec<String> = present
        .into_iter()
        .filter(|name| name.starts_with(ORION_PREFIX))
        .filter(|name| !name.starts_with(RESERVED_PREFIX))
        .filter(|name| !known.contains(name))
        .filter(|name| !referenced_by_config_file.contains(name))
        .filter(|name| !super::retired_env::is_retired(name))
        .filter(|name| could_be_a_misspelled_override(name, &top_level))
        .map(|name| match nearest_key(&name, known) {
            Some(suggestion) => format!("  {name} (did you mean {suggestion}?)"),
            None => format!("  {name}"),
        })
        .collect();

    if offenders.is_empty() {
        return Ok(());
    }
    offenders.sort();
    Err(OrionError::Config {
        message: format!(
            "these ORION_* environment variables are not Orion settings and would be \
             silently ignored:\n{}\nEvery override is ORION_ + the config field path, \
             uppercased with __ between levels — docs/src/configuration/reference.md \
             lists all of them. {RESERVED_PREFIX}* is reserved for values you \
             reference yourself (env:// connector secrets, ${{VAR}} placeholders in \
             the config file or in a connector config_json) and is never read as \
             configuration.",
            offenders.join("\n")
        ),
    })
}

/// Whether the environment scan would refuse `name` if it turned out not to be
/// a setting — the grammar test, with the prefix and reservation rules applied.
///
/// Exported as `config::looks_like_env_override` for `config_docs_drift_test`,
/// which holds this repository's prose to the same rule: a page that prints a
/// name the server would refuse is a page that ships a server which will not
/// boot. Sharing the predicate is what keeps the two from drifting.
pub fn looks_like_env_override(name: &str, known: &BTreeSet<String>) -> bool {
    if !name.starts_with(ORION_PREFIX) || name.starts_with(RESERVED_PREFIX) {
        return false;
    }
    could_be_a_misspelled_override(name, &top_level_keys(known))
}

/// The overrides whose config path is a single segment, and so carry no
/// [`SECTION_SEPARATOR`] to be recognised by.
///
/// Derived from `known` rather than listed a second time, so a setting added
/// at the top level of the config joins it automatically.
fn top_level_keys(known: &BTreeSet<String>) -> Vec<&str> {
    known
        .iter()
        .filter(|key| !key.contains(SECTION_SEPARATOR))
        .map(String::as_str)
        .collect()
}

/// Whether `name` is shaped like an override, and so worth reporting when it
/// is not one.
///
/// Two ways to qualify:
///
/// - It carries the [`SECTION_SEPARATOR`]. Every override for a nested field
///   does, and nothing else in a normal environment does — the kubelet's
///   service links, `orion-cli`'s `ORION_SERVER_URL` and a Compose
///   `ORION_VERSION` are all single-underscore names.
/// - It is within [`SUGGESTION_MAX_DISTANCE`] of one of the `top_level`
///   overrides, which have a one-segment path and so no separator to be
///   recognised by. Without this, `ORION_ENVIRONMEN` would be ignored — and
///   that variable decides whether the production checks are fatal, which is
///   the last one that should fail open. The window is narrow enough that no
///   unrelated name lands in it: `ORION_PORT` is ten edits from
///   `ORION_ENVIRONMENT`, `ORION_SERVER_URL` nine.
fn could_be_a_misspelled_override(name: &str, top_level: &[&str]) -> bool {
    name.contains(SECTION_SEPARATOR)
        || top_level
            .iter()
            .any(|key| crate::text::edit_distance(name, key) <= SUGGESTION_MAX_DISTANCE)
}

/// The closest override name to `candidate`, when one is close enough that a
/// typo is the likely explanation.
fn nearest_key(candidate: &str, known: &BTreeSet<String>) -> Option<String> {
    let needle: Vec<char> = candidate.chars().collect();
    known
        .iter()
        .map(|key| {
            let key_chars: Vec<char> = key.chars().collect();
            (crate::text::edit_distance_chars(&needle, &key_chars), key)
        })
        .filter(|(distance, _)| *distance <= SUGGESTION_MAX_DISTANCE)
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)))
        .map(|(_, key)| key.clone())
}

#[cfg(test)]
mod tests {
    use super::super::env_overrides::known_env_override_keys;
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn empty() -> BTreeSet<String> {
        BTreeSet::new()
    }

    /// The case C4d is named for: one extra character, and the setting silently
    /// did nothing. The error must name the offender *and* the key meant.
    #[test]
    fn a_typo_is_refused_and_names_the_nearest_key() {
        let known = known_env_override_keys();
        let err = reject_unknown_env_vars(names(&["ORION_SERVER__PORTT"]), &known, &empty())
            .expect_err("a misspelled override must not be ignored");
        let message = err.to_string();
        assert!(message.contains("ORION_SERVER__PORTT"), "{message}");
        assert!(
            message.contains("did you mean ORION_SERVER__PORT?"),
            "{message}"
        );
    }

    /// A name with no near neighbour is still refused — just without a guess.
    /// It carries the separator, so it is unambiguously meant as a setting.
    #[test]
    fn an_unrecognisable_name_is_refused_without_a_suggestion() {
        let known = known_env_override_keys();
        let err = reject_unknown_env_vars(names(&["ORION_QUANTUM__FLUX"]), &known, &empty())
            .expect_err("an unknown variable must be refused");
        let message = err.to_string();
        assert!(message.contains("ORION_QUANTUM__FLUX"), "{message}");
        assert!(!message.contains("did you mean"), "{message}");
    }

    /// Every legitimate override must pass. This is the check that keeps the
    /// guard from breaking real deployments: the allowlist is derived from the
    /// overrides themselves, so the whole set has to be accepted verbatim.
    #[test]
    fn every_real_override_is_accepted() {
        let known = known_env_override_keys();
        assert!(
            known.len() > 90,
            "the override set looks broken: {} names",
            known.len()
        );
        let present: Vec<String> = known.iter().cloned().collect();
        reject_unknown_env_vars(present, &known, &empty())
            .expect("every override Orion reads must be accepted");
    }

    /// Non-`ORION_` variables are none of Orion's business.
    #[test]
    fn unrelated_variables_are_ignored() {
        let known = known_env_override_keys();
        reject_unknown_env_vars(
            names(&["PATH", "HOME", "RUST_LOG", "KUBERNETES_SERVICE_HOST"]),
            &known,
            &empty(),
        )
        .expect("only ORION_* names are checked");
    }

    /// A retired name must keep producing `retired_env`'s specific
    /// "renamed to X" error rather than a generic unknown-variable one.
    /// `ORION_ENV` is the one that matters: it has no separator, so only the
    /// retired table — which probes exact names, whatever their shape — can
    /// catch it, and letting it fall through to `development` would relax the
    /// production safety checks.
    #[test]
    fn a_retired_name_is_left_to_the_retired_check() {
        let known = known_env_override_keys();
        reject_unknown_env_vars(
            names(&[
                "ORION_ENV",
                "ORION_QUEUE__WORKERS",
                "ORION_KAFKA__MAX_INFLIGHT",
            ]),
            &known,
            &empty(),
        )
        .expect("retired names belong to reject_retired_env_vars");
        // …and that check still fires on the separator-less one.
        let err = super::super::retired_env::reject_retired_env_vars(|key| {
            if key == "ORION_ENV" {
                Ok("production".to_string())
            } else {
                Err(std::env::VarError::NotPresent)
            }
        })
        .expect_err("ORION_ENV must still report its rename");
        assert!(err.to_string().contains("ORION_ENVIRONMENT"), "{err}");
    }

    /// `${VAR}` substitution reads arbitrary names out of the config file, so
    /// a variable the file references is one Orion genuinely reads.
    #[test]
    fn a_variable_the_config_file_references_is_allowed() {
        let known = known_env_override_keys();
        let referenced: BTreeSet<String> = ["ORION_DB__PASSWORD".to_string()].into_iter().collect();
        reject_unknown_env_vars(names(&["ORION_DB__PASSWORD"]), &known, &referenced)
            .expect("a name the config file substitutes must be allowed");
        // …and only because the file references it.
        assert!(
            reject_unknown_env_vars(names(&["ORION_DB__PASSWORD"]), &known, &empty()).is_err(),
            "without the reference it is just an unknown variable"
        );
    }

    /// The escape hatch for names Orion cannot enumerate — `env://` connector
    /// secrets live in the database, not the config.
    #[test]
    fn the_reserved_prefix_is_never_interpreted() {
        let known = known_env_override_keys();
        reject_unknown_env_vars(
            names(&["ORION_SECRET_DB_PASSWORD", "ORION_SECRET_SERVER__PORTT"]),
            &known,
            &empty(),
        )
        .expect("the reserved namespace is never read as configuration");
    }

    /// Exactly what the kubelet injects into a pod whose namespace holds
    /// Services named `orion`, `orion-postgres` and `orion-redis` — which is
    /// what `helm template orion deploy/helm/orion --set devStack.enabled=true`
    /// renders. Every name is `ORION_`-prefixed and none carries the section
    /// separator.
    fn injected_service_links() -> Vec<String> {
        names(&[
            "ORION_SERVICE_HOST",
            "ORION_SERVICE_PORT",
            "ORION_SERVICE_PORT_HTTP",
            "ORION_PORT",
            "ORION_PORT_8080_TCP",
            "ORION_PORT_8080_TCP_PROTO",
            "ORION_PORT_8080_TCP_PORT",
            "ORION_PORT_8080_TCP_ADDR",
            "ORION_POSTGRES_SERVICE_HOST",
            "ORION_POSTGRES_SERVICE_PORT",
            "ORION_POSTGRES_PORT",
            "ORION_POSTGRES_PORT_5432_TCP",
            "ORION_POSTGRES_PORT_5432_TCP_PROTO",
            "ORION_POSTGRES_PORT_5432_TCP_PORT",
            "ORION_POSTGRES_PORT_5432_TCP_ADDR",
            "ORION_REDIS_SERVICE_HOST",
            "ORION_REDIS_SERVICE_PORT",
            "ORION_REDIS_PORT",
            "ORION_REDIS_PORT_6379_TCP",
            "ORION_REDIS_PORT_6379_TCP_PROTO",
            "ORION_REDIS_PORT_6379_TCP_PORT",
            "ORION_REDIS_PORT_6379_TCP_ADDR",
        ])
    }

    /// The regression this guard nearly shipped: every Orion pod on Kubernetes
    /// gets a block of `ORION_*` variables written by the kubelet, not by any
    /// manifest, so refusing them would have made the chart unbootable.
    /// Sibling tooling collides the same way — `orion-cli`'s variables, and
    /// this repo's own `docs/recordings/record.sh`, which exports `ORION_PORT`
    /// and `ORION_SERVER_URL` into the server it launches.
    #[test]
    fn separatorless_names_are_not_configuration() {
        let known = known_env_override_keys();
        let mut present = injected_service_links();
        present.extend(names(&[
            "ORION_SERVER_URL",
            "ORION_API_KEY",
            "ORION_URL",
            "ORION_VERSION",
            "ORION_IMAGE",
            "ORION_BIN",
            // One underscore short of a real override, and indistinguishable
            // from the link a Service named `orion-server` would produce. It
            // has to be let through: that is the price of the narrowing, and
            // it is cheaper than a server that cannot start.
            "ORION_SERVER_PORT",
        ]));
        reject_unknown_env_vars(present, &known, &empty())
            .expect("a name without the section separator is not an override, misspelled or not");
    }

    /// A real typo sitting alongside the injected block is still reported —
    /// the narrowing must not turn into a blanket amnesty for the namespace.
    #[test]
    fn a_typo_survives_alongside_the_injected_block() {
        let known = known_env_override_keys();
        let mut present = injected_service_links();
        present.push("ORION_SERVER__PORTT".to_string());
        let err = reject_unknown_env_vars(present, &known, &empty())
            .expect_err("the typo must still be caught");
        let message = err.to_string();
        assert!(message.contains("ORION_SERVER__PORTT"), "{message}");
        assert!(!message.contains("ORION_SERVICE_HOST"), "{message}");
        assert!(!message.contains("ORION_PORT_8080"), "{message}");
    }

    /// The overrides that carry no separator, and so sit outside the structural
    /// narrowing. There is one. Adding another means re-checking that no
    /// foreign `ORION_*` name lands within [`SUGGESTION_MAX_DISTANCE`] of it,
    /// which is what would silently re-break Kubernetes.
    #[test]
    fn only_top_level_settings_have_no_separator() {
        let separatorless: BTreeSet<String> = known_env_override_keys()
            .into_iter()
            .filter(|key| !key.contains(SECTION_SEPARATOR))
            .collect();
        assert_eq!(
            separatorless,
            ["ORION_ENVIRONMENT".to_string()].into_iter().collect(),
            "a new top-level setting joined the near-miss check — confirm no \
             service link or sibling-tool variable is within \
             {SUGGESTION_MAX_DISTANCE} edits of it"
        );
    }

    /// …and that near-miss check earns its keep: a misspelled
    /// `ORION_ENVIRONMENT` has no separator to be caught by, and ignoring it
    /// would leave the instance in `development` with the production checks
    /// downgraded to warnings.
    #[test]
    fn a_misspelled_top_level_setting_is_still_refused() {
        let known = known_env_override_keys();
        for typo in [
            "ORION_ENVIRONMEN",
            "ORION_ENVIRONMENTT",
            "ORION_ENVIRONMNET",
        ] {
            let err = reject_unknown_env_vars(names(&[typo]), &known, &empty())
                .expect_err("a near-miss of a top-level setting must be refused");
            let message = err.to_string();
            assert!(message.contains(typo), "{message}");
            assert!(
                message.contains("did you mean ORION_ENVIRONMENT?"),
                "{message}"
            );
        }
    }

    /// One restart should be enough to fix a whole manifest.
    #[test]
    fn every_offender_is_reported_in_one_pass() {
        let known = known_env_override_keys();
        let err = reject_unknown_env_vars(
            names(&[
                "ORION_SERVER__PORTT",
                "ORION_METRIC__ENABLED",
                "ORION_NOPE__NOPE",
            ]),
            &known,
            &empty(),
        )
        .expect_err("all three are unknown");
        let message = err.to_string();
        for expected in [
            "ORION_SERVER__PORTT",
            "ORION_METRIC__ENABLED",
            "ORION_NOPE__NOPE",
        ] {
            assert!(message.contains(expected), "missing {expected}: {message}");
        }
    }
}
