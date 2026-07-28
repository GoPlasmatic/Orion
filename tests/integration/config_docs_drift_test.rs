//! Documentation drift guard for the configuration surface (proposal C2).
//!
//! Three documents describe the same structs and had drifted apart:
//! `docs/src/configuration/reference.md` claimed `storage.max_connections = 25`,
//! `config.toml.example` claimed `10`, and `StorageConfig::default()` — the only
//! one that runs — is `50`. This module makes the code authoritative and fails
//! the build when either document disagrees with it again.
//!
//! What is asserted, in both documents:
//!
//! 1. **Every documented value equals the struct default.** Values are parsed as
//!    TOML and compared against the config structs flattened to dotted paths —
//!    see [`documented_defaults`] for which "default" that means.
//! 2. **Every setting is documented.** A new field added to a config struct fails
//!    the suite until it appears in both documents — this is what C1/C3 were.
//! 3. **No document invents a setting.** A key that is not a real field fails,
//!    which is how `server.workers` (C4) survived for so long.
//! 4. **Every env override is documented, and only real ones.** The overrides
//!    in `src/config/env_overrides.rs` are the source of truth for the
//!    reference page's env-var column: the `ov*!(path)` one-liners (whose
//!    variable names derive from the field path) plus the hand-written
//!    `"ORION_*"` literals for the alias/enum/list/pair shapes. A row claiming
//!    "no env var" (`—`) is checked against the same list.
//!
//! ## Conventions this relies on
//!
//! `config.toml.example` is read as TOML with comment markers ignored: a line is
//! a documented setting when it reads `key = value` or `# key = value` (one `#`,
//! at most one space). Indenting the payload by two or more spaces
//! (`#   key = value`) marks it as an illustrative snippet and excludes it, which
//! is how worked examples live in the file without pretending to be defaults.
//!
//! `docs/src/configuration/reference.md` is read from its settings tables: a row
//! whose first cell is a backticked dotted path is a setting row, and its second
//! and third cells are the default and the env override.

use std::collections::{BTreeMap, BTreeSet};

use orion::config::AppConfig;

const REFERENCE_MD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/docs/src/configuration/reference.md"
);
const EXAMPLE_TOML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml.example");
const ENV_OVERRIDES_RS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/config/env_overrides.rs");

/// Real config fields that have no default value to state: `Option<T>` fields
/// (unset means "don't send this setting at all") and the element fields of
/// free-form maps and arrays of tables. Documented with an illustrative value,
/// so their value is not compared — but the path still has to be real, and
/// everything *not* listed here is compared against the struct default.
const NO_DEFAULT: &[&str] = &[
    // Option<String>: unset leaves the librdkafka default untouched.
    "kafka.auth.security_protocol",
    "kafka.auth.sasl_mechanism",
    "kafka.auth.sasl_username",
    "kafka.auth.sasl_password",
    "kafka.auth.ssl_ca_location",
    // Option<u32>: unset means the route group has no limit of its own.
    "rate_limit.endpoints.admin_rps",
    "rate_limit.endpoints.data_rps",
    // Free-form map, and the fields of the `[[kafka.topics]]` array of tables.
    "kafka.extra_config",
    "kafka.topics.topic",
    "kafka.topics.channel",
];

/// Settings where the derived `Default` and the `#[serde(default = "…")]`
/// attribute disagree, so "the default" would depend on whether the config file
/// declares the section at all. **This list must stay empty.**
///
/// It is not a suppression list — it exists so that
/// [`derived_and_serde_defaults_agree`] can name exactly which settings broke
/// the rule. Start Orion with no config file and the derived `Default` applies;
/// declare a section and omit a key and the serde default applies. A reference
/// page can only state one number, so the two have to be the same value.
///
/// F36 was the case that motivated the rule: `RateLimitConfig` derived
/// `Default` (`0`/`0`) while its serde defaults were `100`/`50`, so
/// `ORION_RATE_LIMIT__ENABLED=true` with no config file failed startup
/// validation. Both structs now implement `Default` by hand in terms of their
/// `default_*` functions.
const SERDE_DEFAULT_DIFFERS: &[&str] = &[];

/// Marker for "this setting has no env override" in the reference table.
const NONE_MARKER: &str = "—";

fn is_no_default(path: &str) -> bool {
    NO_DEFAULT.contains(&path) || path.starts_with("kafka.extra_config.")
}

// ---------------------------------------------------------------------------
// The code side: struct defaults and env-override literals
// ---------------------------------------------------------------------------

/// The value each setting takes when a config file declares its section and
/// omits the key — which is what a config reference documents — flattened to
/// `dotted.path -> value`. Tables recurse; arrays are leaves (so
/// `kafka.topics = []` is one entry, not zero). Fields that are `None` are
/// absent, which is exactly the set `NO_DEFAULT` enumerates.
///
/// This is `AppConfig::default()` for every field except the two in
/// [`SERDE_DEFAULT_DIFFERS`], where a `#[serde(default = "…")]` attribute
/// disagrees with the derived `Default` — see that constant.
fn documented_defaults() -> BTreeMap<String, toml::Value> {
    let skeleton = section_skeleton();
    let parsed: AppConfig = toml::from_str(&skeleton)
        .expect("a config file of empty sections must deserialize into AppConfig");
    let value =
        toml::Value::try_from(parsed).expect("AppConfig must be representable as a TOML value");
    let mut out = BTreeMap::new();
    flatten("", &value, &mut out);
    out
}

/// `AppConfig::default()` flattened the same way — what the binary uses when
/// started with no config file at all.
fn derived_defaults() -> BTreeMap<String, toml::Value> {
    let value = toml::Value::try_from(AppConfig::default())
        .expect("AppConfig::default() must be representable as TOML");
    let mut out = BTreeMap::new();
    flatten("", &value, &mut out);
    out
}

/// A TOML document declaring every section of the config and setting nothing,
/// derived from the shape of `AppConfig::default()` so new sections are picked
/// up automatically.
fn section_skeleton() -> String {
    let mut sections = BTreeSet::new();
    for path in derived_defaults().keys() {
        let mut parts: Vec<&str> = path.split('.').collect();
        parts.pop(); // drop the leaf key
        while !parts.is_empty() {
            sections.insert(parts.join("."));
            parts.pop();
        }
    }
    sections
        .iter()
        .map(|section| format!("[{section}]\n"))
        .collect()
}

fn flatten(prefix: &str, value: &toml::Value, out: &mut BTreeMap<String, toml::Value>) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&path, child, out);
            }
        }
        leaf => {
            out.insert(prefix.to_string(), leaf.clone());
        }
    }
}

/// Every env override `env_overrides.rs` honours, excluding its test module.
/// Two shapes exist: `ov*!(path ...)` one-liners whose variable name derives
/// from the field path (the same `ORION_SECTION__KEY` rule as
/// [`expected_env_name`]), and hand-written `"ORION_*"` literals for the
/// alias/enum/list/pair special cases.
fn env_override_names() -> BTreeSet<String> {
    let source = read(ENV_OVERRIDES_RS);
    let production = source
        .split_once("#[cfg(test)]")
        .map(|(before, _)| before)
        .unwrap_or(&source);

    let mut names = BTreeSet::new();
    for (index, _) in production.match_indices("\"ORION_") {
        let rest = &production[index + 1..];
        let end = rest
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(rest.len());
        // Skip the bare "ORION_" prefix literal inside env_key itself.
        if end > "ORION_".len() {
            names.insert(rest[..end].to_string());
        }
    }
    for line in production.lines() {
        let trimmed = line.trim();
        let rest = ["ov!(", "ov_opt!(", "ov_opt_str!(", "ov_list!("]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix));
        if let Some(rest) = rest {
            let path = rest.split([':', ')']).next().unwrap_or("").trim();
            if !path.is_empty() {
                names.insert(format!("ORION_{}", path.to_uppercase().replace('.', "__")));
            }
        }
    }
    assert!(
        names.len() > 90,
        "env-override extraction found only {} names — the parser is broken, not the docs",
        names.len()
    );
    names
}

/// The env var that overrides a setting: `ORION_SECTION__KEY`, uppercased with
/// `__` between levels. `environment` is the one documented deviation — it is
/// `ORION_ENV`, not `ORION_ENVIRONMENT`.
fn expected_env_name(path: &str) -> String {
    if path == "environment" {
        return "ORION_ENV".to_string();
    }
    format!("ORION_{}", path.to_uppercase().replace('.', "__"))
}

// ---------------------------------------------------------------------------
// The document side: parsing settings out of prose
// ---------------------------------------------------------------------------

/// One documented setting, with enough context to point at it on failure.
struct Documented {
    path: String,
    /// `None` when the document states no default (the `—` marker).
    value: Option<String>,
    line: usize,
}

/// Settings parsed out of `config.toml.example`.
fn example_settings() -> Vec<Documented> {
    let source = read(EXAMPLE_TOML);
    let mut section = String::new();
    let mut found = Vec::new();

    for (index, line) in source.lines().enumerate() {
        let Some(payload) = uncomment(line) else {
            continue;
        };
        if let Some(header) = section_header(payload) {
            section = header.to_string();
            continue;
        }
        let Some((key, value)) = setting(payload) else {
            continue;
        };
        let path = if section.is_empty() {
            key.to_string()
        } else {
            format!("{section}.{key}")
        };
        found.push(Documented {
            path,
            value: Some(value.to_string()),
            line: index + 1,
        });
    }

    assert!(
        found.len() > 50,
        "only {} settings parsed out of config.toml.example — the parser is broken",
        found.len()
    );
    found
}

/// Settings parsed out of the reference page's tables, paired with the env
/// override each row claims.
fn reference_settings() -> Vec<(Documented, Option<String>)> {
    let source = read(REFERENCE_MD);
    let mut found = Vec::new();

    for (index, line) in source.lines().enumerate() {
        let Some(cells) = table_cells(line) else {
            continue;
        };
        if cells.len() < 3 {
            continue;
        }
        let Some(path) = backticked(cells[0]).filter(|p| is_setting_path(p)) else {
            continue;
        };
        let default = backticked(cells[1]).map(str::to_string);
        if default.is_none() && cells[1] != NONE_MARKER {
            continue; // not a settings row after all
        }
        let env = backticked(cells[2]).map(str::to_string);
        found.push((
            Documented {
                path: path.to_string(),
                value: default,
                line: index + 1,
            },
            env,
        ));
    }

    assert!(
        found.len() > 50,
        "only {} settings parsed out of reference.md — the parser is broken",
        found.len()
    );
    found
}

/// Strip an optional single `#` comment marker. Returns `None` for payloads
/// indented two or more spaces past the marker: those are illustrative
/// snippets, not documented settings.
fn uncomment(line: &str) -> Option<&str> {
    let content = line.trim_start();
    let payload = match content.strip_prefix('#') {
        Some(rest) => rest.strip_prefix(' ').unwrap_or(rest),
        None => content,
    };
    if payload.starts_with(char::is_whitespace) {
        return None;
    }
    Some(payload)
}

fn section_header(payload: &str) -> Option<&str> {
    let payload = strip_inline_comment(payload);
    if let Some(rest) = payload.strip_prefix("[[") {
        return rest.strip_suffix("]]");
    }
    if let Some(rest) = payload.strip_prefix('[') {
        return rest.strip_suffix(']');
    }
    None
}

fn setting(payload: &str) -> Option<(&str, &str)> {
    let (key, value) = payload.split_once('=')?;
    let key = key.trim_end();
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '"'))
    {
        return None;
    }
    let value = strip_inline_comment(value.trim());
    if value.is_empty() {
        return None;
    }
    Some((key, value))
}

/// Drop a trailing `# comment`, respecting double-quoted strings.
fn strip_inline_comment(value: &str) -> &str {
    let mut in_string = false;
    for (index, ch) in value.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '#' if !in_string => return value[..index].trim_end(),
            _ => {}
        }
    }
    value.trim_end()
}

fn table_cells(line: &str) -> Option<Vec<&str>> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|')?.strip_suffix('|')?;
    Some(inner.split('|').map(str::trim).collect())
}

fn backticked(cell: &str) -> Option<&str> {
    cell.strip_prefix('`')?.strip_suffix('`')
}

/// `storage.max_connections`, `environment` — but not prose or metric names.
fn is_setting_path(candidate: &str) -> bool {
    if candidate == "environment" {
        return true;
    }
    candidate.contains('.')
        && candidate.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
}

fn parse_value(raw: &str) -> Option<toml::Value> {
    let document: toml::Value = toml::from_str(&format!("x = {raw}")).ok()?;
    document.get("x").cloned()
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

/// Compare one document's settings against the struct defaults.
fn assert_values_match(document: &str, settings: &[Documented]) {
    let defaults = documented_defaults();
    let mut problems = Vec::new();

    for setting in settings {
        // Checked for every row, including rows that state no default.
        if !defaults.contains_key(&setting.path) && !is_no_default(&setting.path) {
            problems.push(format!(
                "{document}:{} documents `{}`, which is not a field of any config struct",
                setting.line, setting.path
            ));
            continue;
        }
        let Some(raw) = &setting.value else {
            continue;
        };
        if is_no_default(&setting.path) {
            continue;
        }
        let Some(parsed) = parse_value(raw) else {
            problems.push(format!(
                "{document}:{} value for `{}` is not valid TOML: {raw}",
                setting.line, setting.path
            ));
            continue;
        };
        let expected = &defaults[&setting.path];
        if &parsed != expected {
            problems.push(format!(
                "{document}:{} documents `{} = {raw}` but the struct default is `{expected:?}`",
                setting.line, setting.path
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "documented config defaults have drifted from src/config/*.rs \
         (the code is authoritative — fix the document):\n  {}",
        problems.join("\n  ")
    );
}

/// Every setting that exists must be documented.
fn assert_coverage(document: &str, settings: &[Documented]) {
    let documented: BTreeSet<&str> = settings.iter().map(|s| s.path.as_str()).collect();
    let defaults = documented_defaults();
    let missing: Vec<&String> = defaults
        .keys()
        .filter(|path| !documented.contains(path.as_str()))
        .collect();

    assert!(
        missing.is_empty(),
        "{document} does not document {} config setting(s) that exist in \
         src/config/*.rs: {missing:?}",
        missing.len()
    );
}

#[test]
fn config_example_matches_struct_defaults() {
    assert_values_match("config.toml.example", &example_settings());
}

#[test]
fn config_example_documents_every_setting() {
    assert_coverage("config.toml.example", &example_settings());
}

#[test]
fn reference_page_matches_struct_defaults() {
    let settings: Vec<Documented> = reference_settings().into_iter().map(|(s, _)| s).collect();
    assert_values_match("docs/src/configuration/reference.md", &settings);
}

#[test]
fn reference_page_documents_every_setting() {
    let settings: Vec<Documented> = reference_settings().into_iter().map(|(s, _)| s).collect();
    assert_coverage("docs/src/configuration/reference.md", &settings);
}

/// The env-var column is checked three ways: the name must be the one the
/// `ORION_SECTION__KEY` scheme produces, it must exist in `env_overrides.rs`,
/// and a `—` ("no override") must still be true.
#[test]
fn reference_page_matches_env_overrides() {
    let names = env_override_names();
    let mut problems = Vec::new();
    let mut documented = BTreeSet::new();

    for (setting, env) in reference_settings() {
        let expected = expected_env_name(&setting.path);
        match env {
            Some(name) => {
                if name != expected {
                    problems.push(format!(
                        "reference.md:{} documents `{name}` for `{}`, but the \
                         override for that setting is `{expected}`",
                        setting.line, setting.path
                    ));
                } else if !names.contains(&name) {
                    problems.push(format!(
                        "reference.md:{} documents `{name}` for `{}`, but no such \
                         override exists in src/config/env_overrides.rs",
                        setting.line, setting.path
                    ));
                }
                documented.insert(name);
            }
            None => {
                if names.contains(&expected) {
                    problems.push(format!(
                        "reference.md:{} says `{}` has no env override, but \
                         `{expected}` exists in src/config/env_overrides.rs",
                        setting.line, setting.path
                    ));
                }
            }
        }
    }

    let undocumented: Vec<&String> = names.difference(&documented).collect();
    assert!(
        undocumented.is_empty(),
        "src/config/env_overrides.rs defines {} override(s) the reference page \
         does not list: {undocumented:?}",
        undocumented.len()
    );
    assert!(problems.is_empty(), "{}", problems.join("\n  "));
}

/// Every setting must mean the same thing whether or not the config file
/// declares its section, so the documents can state a single value (F36).
#[test]
fn derived_and_serde_defaults_agree() {
    let derived = derived_defaults();
    let documented = documented_defaults();
    let divergent: Vec<String> = documented
        .iter()
        .filter(|(path, value)| derived.get(*path) != Some(*value))
        .map(|(path, documented_value)| {
            let derived_value = derived
                .get(path)
                .map_or_else(|| "absent".to_string(), |v| format!("{v:?}"));
            format!(
                "{path}: no config file gives {derived_value}, \
                 empty section gives {documented_value:?}"
            )
        })
        .collect();
    let expected: Vec<&str> = SERDE_DEFAULT_DIFFERS.to_vec();

    assert_eq!(
        divergent.len(),
        expected.len(),
        "a setting's derived Default disagrees with its #[serde(default = \"…\")], \
         so \"the default\" depends on how the config was produced and no reference \
         page can state it. Implement Default by hand in terms of the default_* \
         function, as AppConfig and RateLimitConfig do:\n  {}",
        divergent.join("\n  ")
    );
}

/// The example file is what Docker users get at `/app/config.toml`, so it has
/// to be loadable as-is — including every commented setting, uncommented.
#[test]
fn config_example_parses_as_toml() {
    let source = read(EXAMPLE_TOML);
    toml::from_str::<AppConfig>(&source).expect("config.toml.example must be valid TOML");
}

/// Parsing the file as TOML is not the same as *loading* it, and the gap is
/// where a real bug lived: `${VAR}` substitution runs over the raw file text
/// before the TOML parser ever sees it, comments included. Two prose comments
/// mentioning `${VAR}` and three showing `${CONFLUENT_API_KEY}`-style secrets
/// were therefore read as required variables, and the shipped example refused
/// to load with *"Required environment variable 'VAR' is not set"* on a clean
/// machine — the one file every new user starts from.
///
/// This goes through the real entry point, with no variables set, so the
/// substitution pass is covered rather than skipped.
#[test]
fn config_example_loads_through_the_real_entry_point() {
    orion::config::load_config(Some(EXAMPLE_TOML))
        .expect("config.toml.example must load with no environment variables set");
}
