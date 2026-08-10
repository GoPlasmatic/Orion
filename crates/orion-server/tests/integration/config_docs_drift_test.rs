//! Documentation drift guard for the configuration surface (proposal C2).
//!
//! Three documents describe the same structs and had drifted apart:
//! `docs/src/reference/configuration.md` claimed `storage.max_connections = 25`,
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
//! `docs/src/reference/configuration.md` is read from its settings tables: a row
//! whose first cell is a backticked dotted path is a setting row, and its second
//! and third cells are the default and the env override.

use std::collections::{BTreeMap, BTreeSet};

use orion::config::AppConfig;

// The docs tree lives at the repo root, two levels above this crate.
const REFERENCE_MD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/src/reference/configuration.md"
);
const EXAMPLE_TOML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml.example");
const ENV_OVERRIDES_RS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/config/env_overrides.rs");

/// Real config fields that have no default value to state: `Option<T>` fields
/// (unset means "don't send this setting at all") and the element fields of
/// free-form maps and arrays of tables. Documented with an illustrative value,
/// so their value is not compared — but the path still has to be real, and
/// everything *not* listed here is compared against the struct default.
const NO_DEFAULT: &[&str] = &[
    // Option<bool>: unset means "enabled outside production" (S17).
    "server.docs.enabled",
    // Option<bool>: unset means "verbose outside production", the same rule.
    // An explicit `true` in production is refused at startup.
    "server.verbose_errors",
    // Option<String>: unset keeps /metrics on the main listener (O12).
    "metrics.bind_addr",
    // Option<String>: unset leaves the librdkafka default untouched.
    "kafka.auth.security_protocol",
    "kafka.auth.sasl_mechanism",
    "kafka.auth.sasl_username",
    "kafka.auth.sasl_password",
    "kafka.auth.ssl_ca_location",
    // Option<u32>: unset means the route group has no limit of its own.
    "rate_limit.endpoints.admin_rps",
    "rate_limit.endpoints.data_rps",
    // Option<u32>: unset means keep every backup (no pruning).
    "storage.backup_retention_count",
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

/// C4d: the names the scraper finds in the source must be exactly the names
/// the overrides consult at runtime.
///
/// `known_env_override_keys()` is the allowlist the unknown-variable guard
/// refuses against, and it is derived by *running* the overrides with a
/// recording reader. That is only equivalent to reading the source if every
/// override consults its variable unconditionally — an override moved behind
/// an `if` would drop out of the allowlist and turn a legitimate deployment
/// variable into a startup error. This is the assertion that catches it.
#[test]
fn the_runtime_override_set_matches_the_source() {
    let scraped = env_override_names();
    let runtime = orion::config::known_env_override_keys();
    let missing: Vec<&String> = scraped.difference(&runtime).collect();
    let extra: Vec<&String> = runtime.difference(&scraped).collect();
    assert!(
        missing.is_empty(),
        "src/config/env_overrides.rs declares {} override(s) that are not \
         consulted at runtime, so the C4d guard would refuse them: {missing:?}",
        missing.len()
    );
    assert!(
        extra.is_empty(),
        "{} variable(s) are consulted at runtime but invisible to the docs \
         scraper, so the reference page cannot be checked against them: {extra:?}",
        extra.len()
    );
}

/// The env var that overrides a setting: `ORION_SECTION__KEY`, uppercased with
/// `__` between levels. No exceptions since C22 retired the `ORION_ENV` alias
/// — retired names live in `src/config/retired_env.rs`, out of the scraper's
/// reach, and are refused at startup rather than ignored.
fn expected_env_name(path: &str) -> String {
    format!("ORION_{}", path.to_uppercase().replace('.', "__"))
}

// ---------------------------------------------------------------------------
// Every ORION_* name this repository writes down (C4d)
// ---------------------------------------------------------------------------

/// Directories walked for `ORION_*` tokens, plus the loose files at the root.
///
/// `.github` is in scope because a mistyped variable in a workflow fails the
/// same way a mistyped one in a manifest does, only less visibly.
/// `CHANGELOG.md` is deliberately out: a historical record that quotes names
/// precisely because they were wrong or have since been renamed, and
/// rewriting history to satisfy a lint would defeat the point of keeping it.
const PROSE_DIRS: &[&str] = &["docs", "examples", "deploy", ".github"];
const PROSE_FILES: &[&str] = &[
    "README.md",
    "CONTRIBUTING.md",
    "CLAUDE.md",
    "crates/orion-server/config.toml.example",
    "docker-compose.yml",
    "docker-compose.ha.yml",
    "docker-compose.ha.build.yml",
    "Dockerfile",
];

/// File extensions worth scanning: prose, manifests and scripts. Recordings
/// (`.cast`, `.gif`, `.webm`, `.png`) are excluded — they are generated.
const PROSE_EXTENSIONS: &[&str] = &[
    "md", "sh", "yml", "yaml", "toml", "tpl", "example", "mjs", "json", "conf", "txt",
];

/// `ORION_*` tokens this repository writes down that read as settings and
/// deliberately are not: `(name, only_in, reason)`. Everything that reads as a
/// setting must be a live override or a retired name.
///
/// "Reads as a setting" is [`looks_like_a_setting`]. Names outside it need no
/// entry, because neither the server nor a reader can mistake them for
/// settings: `ORION_VERSION` in a Compose file, `orion-cli`'s
/// `ORION_SERVER_URL`, the kubelet's `ORION_SERVICE_HOST`, `ORION_DIR` in a
/// shell script.
///
/// `only_in` scopes the excuse to one path when the name is a deliberate
/// *counter*-example — a wrong name a page quotes on purpose. Those must not
/// buy an exemption anywhere else, which is precisely the hole this test was
/// added to close: `ORION_ADMIN_AUTH__API_KEY` (singular) sat in two pages, one
/// quoting it as the mistake and one telling readers to set it.
const NOT_A_SERVER_SETTING: &[(&str, &str, &str)] = &[
    (
        "ORION_SECTION__KEY",
        "",
        "the naming rule itself, written as a placeholder wherever it is quoted",
    ),
    (
        "ORION_SERVER__PORTT",
        "docs/src/reference/configuration.md",
        "the worked example of the misspelling C4d refuses",
    ),
    (
        "ORION_ENVIRONMEN",
        "docs/src/reference/configuration.md",
        "the worked example of the top-level near-miss C4d also refuses",
    ),
    (
        "ORION_SERVER_PORT",
        "docs/src/reference/configuration.md",
        "the worked example of the one shape C4d cannot refuse — a setting typed \
         with a single underscore, which is also exactly the service link a \
         Service named `orion-server` produces",
    ),
    (
        "ORION_ADMIN_AUTH__API_KEY",
        "docs/src/features/deployability.md",
        "the name this page shipped wrong until 1.0, now quoted as the mistake",
    ),
    (
        "ORION_CORS_ALLOWED_ORIGINS",
        "",
        "compose interpolation, resolved by `docker compose` in the operator's \
         shell; the container's environment block spells out the real \
         ORION_CORS__ALLOWED_ORIGINS it fills",
    ),
    // The upgrade guide teaches the same rule the reference page does, so it
    // quotes the same counter-examples. Scoped per path rather than blanket,
    // for the reason the doc comment above gives.
    (
        "ORION_SERVER__PORTT",
        "docs/src/operate/upgrading-to-1.0.md",
        "the misspelling C4d refuses, quoted in the error message the section \
         shows an operator",
    ),
    (
        "ORION_SERVER_PORT",
        "docs/src/operate/upgrading-to-1.0.md",
        "the one shape C4d cannot refuse, quoted as the caveat: a setting typed \
         with a single underscore is indistinguishable from a service link",
    ),
    (
        "ORION_ADMIN_AUTH__API_KEY",
        "docs/src/operate/upgrading-to-1.0.md",
        "quoted as the mistake, telling readers who copied it that the real \
         name is the plural ORION_ADMIN_AUTH__API_KEYS",
    ),
    (
        "ORION_DB__PASSWORD",
        "docs/src/operate/upgrading-to-1.0.md",
        "a connector's `env://` secret, not a setting — the example of the one \
         class of name that has to move under ORION_SECRET_* because it \
         follows the override grammar",
    ),
];

/// Whether `token` is excused in the file at repository-relative `path`.
fn is_excused(token: &str, path: &str) -> bool {
    NOT_A_SERVER_SETTING
        .iter()
        .any(|(name, only_in, _)| *name == token && (only_in.is_empty() || *only_in == path))
}

/// Whether `token`, printed on a page, reads as an Orion setting. Two classes,
/// and the difference between them is the point:
///
/// * The server would **refuse** it at startup. That is
///   `config::looks_like_env_override` — the guard's own predicate, shared
///   rather than restated, so this test cannot drift from the rule it enforces.
///   A page printing one of these in a copy-pasteable block ships a server that
///   will not boot.
/// * It is a real override with its `__` collapsed to a single `_`:
///   `ORION_SERVER_PORT` for `ORION_SERVER__PORT`. The server *cannot* refuse
///   these — the section separator is the only thing telling a setting apart
///   from a Kubernetes service link, so single-underscore names have to be let
///   through — which leaves this test as the only thing between a reader and a
///   variable that silently does nothing.
fn looks_like_a_setting(token: &str, known: &BTreeSet<String>) -> bool {
    orion::config::looks_like_env_override(token, known)
        || known.iter().any(|key| key.replace("__", "_") == token)
}

/// Collect every file under `PROSE_DIRS`/`PROSE_FILES` worth scanning.
fn prose_files() -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if !matches!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("node_modules" | "out" | "target" | "casts" | "videos" | "images")
                ) {
                    walk(&path, out);
                }
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| PROSE_EXTENSIONS.contains(&ext))
            {
                out.push(path);
            }
        }
    }

    // Prose scanning starts at the repo root, two levels above this crate.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    for dir in PROSE_DIRS {
        walk(&root.join(dir), &mut out);
    }
    for file in PROSE_FILES {
        let path = root.join(file);
        if path.is_file() {
            out.push(path);
        }
    }
    out
}

/// Every `ORION_*` token in `text`, as `(token, line number)`.
///
/// Two shapes are skipped. A token ending in `_` is a prefix written in prose
/// (*"`ORION_QUEUE__*` -> `ORION_TRACE_QUEUE__*`"*), not a name. A token
/// immediately preceded by `${` is a substitution placeholder — the config
/// file's `${VAR}` grammar, a Compose interpolation, or a shell expansion —
/// and those name variables the *reader* chooses.
fn orion_tokens(text: &str) -> Vec<(String, usize)> {
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let bytes = line.as_bytes();
        let mut at = 0;
        while let Some(offset) = line[at..].find("ORION_") {
            let start = at + offset;
            let end = start
                + line[start..]
                    .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                    .unwrap_or(line.len() - start);
            at = end.max(start + 1);
            if start >= 2 && &bytes[start - 2..start] == b"${" {
                continue;
            }
            let token = &line[start..end];
            if token.ends_with('_') {
                continue;
            }
            found.push((token.to_string(), index + 1));
        }
    }
    found
}

/// No page, manifest or script in this repository may print an `ORION_*` name
/// that reads as a setting without being one.
///
/// The reference page's tables are checked above; this is the rest of the
/// documentation, where the mistyped `ORION_ADMIN_AUTH__API_KEY` lived — in two
/// pages, of which the sweep that noticed fixed one. Since C4d a name like that
/// in the server's environment stops the boot, so a page shipping it in a
/// copy-pasteable block ships a server that will not start; and a real override
/// typed with a single underscore is worse, because the server has no way to
/// object. [`looks_like_a_setting`] covers both.
#[test]
fn every_documented_env_name_is_real() {
    let known = orion::config::known_env_override_keys();
    let retired = orion::config::retired_env_names();
    // Prose scanning starts at the repo root, two levels above this crate.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

    let mut files = 0usize;
    let mut problems = Vec::new();
    for path in prose_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        files += 1;
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        for (token, line) in orion_tokens(&text) {
            if known.contains(&token)
                || retired.contains(&token)
                || !looks_like_a_setting(&token, &known)
                || is_excused(&token, &relative)
            {
                continue;
            }
            problems.push(format!(
                "{relative}:{line} writes `{token}`, which reads as an Orion setting \
                 without being one. A name following the override grammar stops the \
                 server at startup (C4d); a real override typed with a single \
                 underscore is ignored instead, which is quieter and no better. Fix \
                 the name, or add it to NOT_A_SERVER_SETTING with the reason it is \
                 written that way."
            ));
        }
    }

    assert!(
        files > 50,
        "only {files} files scanned — the walker is broken, not the docs"
    );
    assert!(problems.is_empty(), "{}", problems.join("\n  "));
}

/// The excuse list must not outlive its entries: a name that became a real
/// setting, that no page mentions any more, or whose page moved, is stale.
#[test]
fn every_excused_env_name_is_still_needed() {
    let known = orion::config::known_env_override_keys();
    // Prose scanning starts at the repo root, two levels above this crate.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut mentioned: BTreeSet<(String, String)> = BTreeSet::new();
    for path in prose_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        for (token, _) in orion_tokens(&text) {
            mentioned.insert((token, relative.clone()));
        }
    }

    let mut problems = Vec::new();
    for (name, only_in, reason) in NOT_A_SERVER_SETTING {
        assert!(!reason.is_empty(), "{name} needs a reason");
        if known.contains(*name) {
            problems.push(format!("{name} is a real override now — drop the excuse"));
            continue;
        }
        if !looks_like_a_setting(name, &known) {
            problems.push(format!(
                "{name} no longer reads as a setting, so the scan would not flag it \
                 — drop the excuse"
            ));
            continue;
        }
        let still_written = mentioned
            .iter()
            .any(|(token, path)| token == name && (only_in.is_empty() || path == only_in));
        if !still_written {
            problems.push(if only_in.is_empty() {
                format!("{name} is not written down anywhere any more")
            } else {
                format!("{name} is no longer in {only_in} — the excuse points at nothing")
            });
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n  "));
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
    assert_values_match("docs/src/reference/configuration.md", &settings);
}

#[test]
fn reference_page_documents_every_setting() {
    let settings: Vec<Documented> = reference_settings().into_iter().map(|(s, _)| s).collect();
    assert_coverage("docs/src/reference/configuration.md", &settings);
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
