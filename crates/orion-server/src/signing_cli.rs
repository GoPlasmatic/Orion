//! `orion-server plugin …` and `orion-server model …`: digest, sign and
//! verify the artifacts `[plugins.trust]` and `[models.trust]` check.
//!
//! One set of verbs, two nouns. The noun decides how a manifest resolves to
//! the file whose digest is signed — a plugin's `component`, a model's
//! `artifact` — and which trust table `-c` reads; everything else is shared.
//! The digest, the signature and the check are the server's own
//! (`crypto::sha256_digest`, `crypto::ed25519`), so what these verbs produce
//! is exactly what an upload is verified against, and the file convention
//! is `orion::signatures`.

use std::path::{Path, PathBuf};

use orion::crypto::ed25519::{self, SigningKey};
use orion::signatures::{self, Kind, SignatureDir, Subject};

type CliError = Box<dyn std::error::Error>;

/// The environment variable `sign` reads a PEM key from when `--key` is
/// absent — a CI secret store hands out variables, not files.
pub(crate) const SIGNING_KEY_ENV: &str = "ORION_SIGNING_KEY";

#[derive(clap::Subcommand)]
pub(crate) enum SigningCommand {
    /// Print the `sha256:` digest a signature is made over.
    ///
    /// PATH is a manifest, a directory of manifests, or any file. For one
    /// target the bare digest is printed, so `$(orion-server plugin digest
    /// …)` works; for several, one `digest  id  file` line each.
    Digest {
        /// A manifest, a directory searched for manifests, or a bare file.
        path: String,
    },
    /// Generate an Ed25519 signing key and print its public key.
    ///
    /// The key is written as an unencrypted PKCS#8 PEM — the form `openssl
    /// genpkey -algorithm ed25519` writes — readable only by its owner.
    Keygen {
        /// Where to write the private key; `-` writes it to stdout.
        #[arg(short, long, value_name = "FILE")]
        output: String,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Print the `public_keys` value for an existing signing key.
    Pubkey {
        /// A PEM private key, from `keygen` or `openssl genpkey`.
        #[arg(long, value_name = "FILE")]
        key: String,
    },
    /// Sign the digest of every artifact PATH names.
    ///
    /// Each signature is written as one line of base64 to `<artifact>.sig`
    /// beside the artifact, or into `-o`.
    Sign {
        /// A manifest, a directory searched for manifests, or a bare file.
        path: String,
        /// The PEM private key. Without it, the PEM text in
        /// `ORION_SIGNING_KEY` is used.
        #[arg(long, value_name = "FILE")]
        key: Option<String>,
        /// A file (one artifact only) or a directory, which is created and
        /// receives flat `<artifact file>.sig` names — the layout `package
        /// apply --signatures` reads.
        #[arg(short, long, value_name = "FILE|DIR")]
        output: Option<String>,
        /// Name each file `<plugin or model id>.sig` rather than after the
        /// artifact file, for ids whose artifact files share a name.
        #[arg(long)]
        by_id: bool,
    },
    /// Verify each artifact's signature against the trusted public keys.
    Verify {
        /// A manifest, a directory searched for manifests, or a bare file.
        path: String,
        /// A trusted public key, base64. Repeatable. Without one, the keys
        /// under `[plugins.trust]` (or `[models.trust]`) of the `-c` config
        /// are used.
        #[arg(long = "public-key", value_name = "BASE64")]
        public_keys: Vec<String>,
        /// The signature file, for a single artifact.
        #[arg(long, value_name = "FILE", conflicts_with = "signatures")]
        signature: Option<String>,
        /// A directory of `.sig` files, looked up by id then artifact file
        /// name. Without this or `--signature`, the `.sig` beside each
        /// artifact is read.
        #[arg(long, value_name = "DIR")]
        signatures: Option<String>,
    },
}

/// One artifact a verb acts on.
struct Target {
    /// The plugin or model id, when a manifest named the artifact.
    id: Option<String>,
    file: PathBuf,
    digest: String,
}

impl Target {
    fn file_name(&self) -> String {
        self.file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn label(&self) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| self.file.display().to_string())
    }

    fn subject(&self, kind: Kind) -> Subject {
        Subject {
            kind,
            id: self.id.clone().unwrap_or_default(),
            file_names: vec![self.file_name()],
            digest: self.digest.clone(),
            carried: None,
        }
    }
}

/// Run one verb; the returned code is the process exit code.
pub(crate) fn run(
    kind: Kind,
    command: SigningCommand,
    config_path: Option<&str>,
) -> Result<i32, CliError> {
    match command {
        SigningCommand::Digest { path } => digest(kind, &path),
        SigningCommand::Keygen { output, force } => keygen(&output, force),
        SigningCommand::Pubkey { key } => {
            println!("{}", read_key_file(&key)?.public_key_base64());
            Ok(0)
        }
        SigningCommand::Sign {
            path,
            key,
            output,
            by_id,
        } => sign(kind, &path, key.as_deref(), output.as_deref(), by_id),
        SigningCommand::Verify {
            path,
            public_keys,
            signature,
            signatures,
        } => verify(
            kind,
            &path,
            public_keys,
            signature.as_deref(),
            signatures.as_deref(),
            config_path,
        ),
    }
}

// ============================================================
// Resolving PATH
// ============================================================

/// What `path` names, as artifacts with their digests.
///
/// A directory is searched for the noun's manifests, and finding none is an
/// error — signing nothing must not look like success. A manifest names its
/// artifact, which must be on disk. The other noun's manifest is refused
/// with the noun that takes it. Any other file is a bare artifact.
fn resolve(kind: Kind, path: &str) -> Result<Vec<Target>, CliError> {
    let p = Path::new(path);
    if !p.exists() {
        return Err(format!("{path}: no such file or directory").into());
    }
    if p.is_dir() {
        let mut targets = from_manifests(kind, path)?;
        if targets.is_empty() {
            let other = match kind {
                Kind::Plugin => Kind::Model,
                Kind::Model => Kind::Plugin,
            };
            let hint = if from_manifests(other, path).is_ok_and(|t| !t.is_empty()) {
                format!(
                    " — it holds {} manifests; use `orion-server {}`",
                    other.noun(),
                    other.noun()
                )
            } else {
                String::new()
            };
            return Err(format!("no {} manifest under {path}{hint}", kind.noun()).into());
        }
        targets.sort_by(|a, b| a.file.cmp(&b.file));
        return Ok(targets);
    }
    // A file: a manifest of this noun, the other noun's (refused), or bare.
    let text = std::fs::read_to_string(p).ok();
    let as_plugin = text
        .as_deref()
        .is_some_and(orion::definitions::is_plugin_manifest);
    let as_model = text.as_deref().is_some_and(|t| {
        serde_json::from_str::<serde_json::Value>(t)
            .is_ok_and(|doc| orion::model::is_model_manifest(&doc))
    });
    match (kind, as_plugin, as_model) {
        (Kind::Plugin, true, _) | (Kind::Model, _, true) => from_manifests(kind, path),
        (Kind::Plugin, _, true) => {
            Err(format!("{path} is a model manifest — use `orion-server model`").into())
        }
        (Kind::Model, true, _) => {
            Err(format!("{path} is a plugin manifest — use `orion-server plugin`").into())
        }
        _ => {
            let bytes = std::fs::read(p).map_err(|e| format!("{path}: {e}"))?;
            Ok(vec![Target {
                id: None,
                file: p.to_path_buf(),
                digest: orion::crypto::sha256_digest(&bytes),
            }])
        }
    }
}

/// Every manifest of `kind` under `path` (a directory or one manifest), with
/// its artifact. A manifest problem or an artifact that is not on disk is an
/// error naming the manifest.
fn from_manifests(kind: Kind, path: &str) -> Result<Vec<Target>, CliError> {
    let mut set = orion::definitions::DefinitionSet::default();
    let dirs = [path.to_string()];
    let findings = match kind {
        Kind::Plugin => set.add_plugin_dirs(&dirs)?,
        Kind::Model => set.add_model_dirs(&dirs)?,
    };
    let errors: Vec<_> = findings.iter().filter(|f| f.is_error()).collect();
    if !errors.is_empty() {
        for finding in &errors {
            eprintln!("{finding}");
        }
        return Err(format!("{path}: {} manifest problem(s)", errors.len()).into());
    }
    let mut targets = Vec::new();
    let mut missing = Vec::new();
    match kind {
        Kind::Plugin => {
            for plugin in &set.plugins {
                match (&plugin.component_path, &plugin.digest) {
                    (Some(file), Some(digest)) => targets.push(Target {
                        id: Some(plugin.manifest.name.clone()),
                        file: file.clone(),
                        digest: digest.clone(),
                    }),
                    _ => missing.push(missing_artifact(
                        &plugin.origin,
                        "component",
                        plugin.manifest.component.as_deref(),
                    )),
                }
            }
        }
        Kind::Model => {
            for model in &set.models {
                match (&model.artifact_path, &model.digest) {
                    (Some(file), Some(digest)) => targets.push(Target {
                        id: Some(model.manifest.name.clone()),
                        file: file.clone(),
                        digest: digest.clone(),
                    }),
                    _ => missing.push(missing_artifact(
                        &model.origin,
                        "artifact",
                        model.manifest.artifact.as_deref(),
                    )),
                }
            }
        }
    }
    if !missing.is_empty() {
        for line in &missing {
            eprintln!("error: {line}");
        }
        return Err(format!(
            "{} manifest(s) name an artifact that is not on disk",
            missing.len()
        )
        .into());
    }
    Ok(targets)
}

fn missing_artifact(origin: &str, field: &str, named: Option<&str>) -> String {
    match named {
        Some(rel) => {
            let expected = Path::new(origin)
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(rel);
            format!(
                "{origin}: {field} '{rel}' is not on disk (expected {})",
                expected.display()
            )
        }
        None => format!("{origin}: names no {field} file, so there is nothing to sign"),
    }
}

// ============================================================
// Verbs
// ============================================================

fn digest(kind: Kind, path: &str) -> Result<i32, CliError> {
    let targets = resolve(kind, path)?;
    if let [one] = targets.as_slice() {
        println!("{}", one.digest);
        return Ok(0);
    }
    for target in &targets {
        println!(
            "{}  {}  {}",
            target.digest,
            target.id.as_deref().unwrap_or("-"),
            target.file.display()
        );
    }
    Ok(0)
}

fn keygen(output: &str, force: bool) -> Result<i32, CliError> {
    let key = SigningKey::generate();
    let pem = key.to_pkcs8_pem();
    if output == "-" {
        print!("{pem}");
        eprintln!("public_keys value: {}", key.public_key_base64());
        return Ok(0);
    }
    write_private(Path::new(output), &pem, force)?;
    println!("wrote {output}");
    println!("public_keys value: {}", key.public_key_base64());
    Ok(0)
}

/// Write a private key: never over an existing file unless asked, and
/// readable only by its owner where the platform can say so.
fn write_private(path: &Path, pem: &str, force: bool) -> Result<(), CliError> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "{} already exists — pass --force to replace the key it holds",
                path.display()
            )
        } else {
            format!("{}: {e}", path.display())
        }
    })?;
    file.write_all(pem.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

fn read_key_file(path: &str) -> Result<SigningKey, CliError> {
    let pem = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    Ok(SigningKey::from_pkcs8_pem(&pem).map_err(|e| format!("{path}: {e}"))?)
}

fn signing_key(key: Option<&str>) -> Result<SigningKey, CliError> {
    if let Some(path) = key {
        return read_key_file(path);
    }
    match std::env::var(SIGNING_KEY_ENV) {
        Ok(pem) if !pem.trim().is_empty() => {
            Ok(SigningKey::from_pkcs8_pem(&pem).map_err(|e| format!("{SIGNING_KEY_ENV}: {e}"))?)
        }
        _ => Err(format!("no signing key: pass --key <file> or set {SIGNING_KEY_ENV}").into()),
    }
}

fn sign(
    kind: Kind,
    path: &str,
    key: Option<&str>,
    output: Option<&str>,
    by_id: bool,
) -> Result<i32, CliError> {
    let key = signing_key(key)?;
    let targets = resolve(kind, path)?;
    if by_id && let Some(bare) = targets.iter().find(|t| t.id.is_none()) {
        return Err(format!(
            "--by-id needs a manifest: {} is a bare file with no id",
            bare.file.display()
        )
        .into());
    }
    let name_of = |t: &Target| match (&t.id, by_id) {
        (Some(id), true) => signatures::sig_file_name(id),
        _ => signatures::sig_file_name(&t.file_name()),
    };

    // Where each signature goes.
    let into_dir = output.is_some_and(|o| {
        o.ends_with('/') || o.ends_with(std::path::MAIN_SEPARATOR) || Path::new(o).is_dir()
    }) || (output.is_some() && targets.len() > 1);
    let destinations: Vec<PathBuf> = match output {
        None => targets
            .iter()
            .map(|t| t.file.with_file_name(name_of(t)))
            .collect(),
        Some(dir) if into_dir => {
            std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
            targets
                .iter()
                .map(|t| Path::new(dir).join(name_of(t)))
                .collect()
        }
        Some(file) => vec![PathBuf::from(file)],
    };
    for (i, a) in destinations.iter().enumerate() {
        if let Some(j) = destinations[..i].iter().position(|b| b == a) {
            return Err(format!(
                "{} would hold the signatures of both {} and {} — pass --by-id to name each \
                 file after its id",
                a.display(),
                targets[j].label(),
                targets[i].label()
            )
            .into());
        }
    }

    let public = key.public_key_base64();
    for (target, destination) in targets.iter().zip(&destinations) {
        let signature = key.sign(&target.digest);
        // The one check this side can make: the key verifies its own
        // signature over this digest, exactly as a trusting node will.
        ed25519::verify(
            std::slice::from_ref(&public),
            &target.digest,
            Some(&signature),
        )
        .map_err(|e| format!("{}: {e}", target.label()))?;
        std::fs::write(destination, format!("{signature}\n"))
            .map_err(|e| format!("{}: {e}", destination.display()))?;
        println!("wrote {}", destination.display());
    }
    Ok(0)
}

fn verify(
    kind: Kind,
    path: &str,
    public_keys: Vec<String>,
    signature: Option<&str>,
    signatures_dir: Option<&str>,
    config_path: Option<&str>,
) -> Result<i32, CliError> {
    let keys = if public_keys.is_empty() {
        trusted_keys(kind, config_path)?
    } else {
        public_keys
    };
    for (i, key) in keys.iter().enumerate() {
        ed25519::parse_public_key(key).map_err(|e| format!("public key {}: {e}", i + 1))?;
    }
    let targets = resolve(kind, path)?;
    if signature.is_some() && targets.len() > 1 {
        return Err(format!(
            "--signature names one file, but {path} holds {} {}s — use --signatures <dir>",
            targets.len(),
            kind.noun()
        )
        .into());
    }
    let dir = signatures_dir
        .map(|d| SignatureDir::open(Path::new(d)))
        .transpose()?;

    let mut failed = 0usize;
    for target in &targets {
        let found = match (signature, &dir) {
            (Some(file), _) => Some(PathBuf::from(file)),
            (None, Some(dir)) => dir.lookup(&target.subject(kind)).map(Path::to_path_buf),
            (None, None) => {
                let beside = target.file.parent().unwrap_or_else(|| Path::new("."));
                SignatureDir::open(beside)?
                    .lookup(&target.subject(kind))
                    .map(Path::to_path_buf)
            }
        };
        let outcome = match found {
            None => Err(format!(
                "no signature (looked for {})",
                target.subject(kind).candidates().join(" or ")
            )),
            Some(file) => signatures::read_sig_file(&file).and_then(|sig| {
                ed25519::verify(&keys, &target.digest, Some(&sig))
                    .map_err(|e| format!("{}: {e}", file.display()))
            }),
        };
        match outcome {
            Ok(()) => println!("ok  {}  {}", target.label(), target.digest),
            Err(reason) => {
                failed += 1;
                eprintln!("FAILED  {}: {reason}", target.label());
            }
        }
    }
    Ok(if failed == 0 { 0 } else { 1 })
}

/// The trust keys the `-c` config declares for this noun. None at all is an
/// error rather than a pass: with no keys `ed25519::verify` checks nothing,
/// and `verify` would print a false `ok`.
fn trusted_keys(kind: Kind, config_path: Option<&str>) -> Result<Vec<String>, CliError> {
    let section = match kind {
        Kind::Plugin => "[plugins.trust]",
        Kind::Model => "[models.trust]",
    };
    let Some(config_path) = config_path else {
        return Err(format!(
            "no public key: pass --public-key, or -c <config.toml> whose {section} names \
             public_keys"
        )
        .into());
    };
    let config = orion::config::load_config(Some(config_path))?;
    let keys = match kind {
        Kind::Plugin => config.plugins.trust.public_keys,
        Kind::Model => config.models.trust.public_keys,
    };
    if keys.is_empty() {
        return Err(format!(
            "no public key: {config_path} declares no public_keys under {section}, so a node \
             running it checks no signature — pass --public-key to verify against a key"
        )
        .into());
    }
    Ok(keys)
}
