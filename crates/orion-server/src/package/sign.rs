//! Signatures attached to an artifact at deploy time: the detached `.sig`
//! files a deployment holds, matched to the artifact's plugins and models
//! and set on their entries in memory. A signature is not content, so the
//! artifact's version and hash do not move.

use serde_json::{Value, json};

use super::artifact::PackageArtifact;
use super::{Error, Reporter};
use crate::signatures::{Kind, Outcome, SignatureDir, Subject};

/// The signature subjects of an artifact: its `plugins[]` then its
/// `models[]`, in order. A plugin is named by its manifest and its
/// component's file name; a model by its id and the file name of its bucket
/// key — at apply time the local file a model was compiled from is gone.
pub fn signature_subjects(artifact: &PackageArtifact) -> Result<Vec<Subject>, Error> {
    let file_name = |path: &str| {
        std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    };
    let carried = |entry: &Value| {
        entry["signature"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut subjects = Vec::new();
    for entry in &artifact.plugins {
        let content = super::artifact::plugin_import_content(entry)?;
        let manifest: crate::plugin::Manifest =
            serde_json::from_value(content["manifest"].clone())?;
        subjects.push(Subject {
            kind: Kind::Plugin,
            file_names: manifest
                .component
                .as_deref()
                .and_then(file_name)
                .into_iter()
                .collect(),
            id: manifest.name,
            digest: content["digest"].as_str().unwrap_or_default().to_string(),
            carried: carried(entry),
        });
    }
    for entry in &artifact.models {
        subjects.push(Subject {
            kind: Kind::Model,
            id: entry["model_id"].as_str().unwrap_or_default().to_string(),
            file_names: entry["artifact"]["key"]
                .as_str()
                .and_then(file_name)
                .into_iter()
                .collect(),
            digest: entry["artifact"]["digest"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            carried: carried(entry),
        });
    }
    Ok(subjects)
}

/// Attach the signatures `dir` holds to the artifact's `plugins[]` and
/// `models[]` entries, in memory. The artifact's version and `content_hash`
/// do not move — a signature is not content. `more_names` adds candidate
/// file names per subject (`compile` still knows a model's local file).
///
/// # Errors
///
/// Every orphan, ambiguous and malformed `.sig` file, each printed; or a
/// directory that cannot be read.
pub fn attach_signatures(
    artifact: &mut PackageArtifact,
    dir: &std::path::Path,
    more_names: &dyn Fn(&Subject) -> Vec<String>,
    report: &dyn Reporter,
) -> Result<Vec<(Subject, Outcome)>, Error> {
    let sigs = SignatureDir::open(dir)?;
    let mut subjects = signature_subjects(artifact)?;
    for subject in &mut subjects {
        for name in more_names(subject) {
            if !subject.file_names.contains(&name) {
                subject.file_names.push(name);
            }
        }
    }
    let outcomes = sigs.resolve(&subjects).map_err(|errors| {
        for error in &errors {
            report.err(&format!("error: {error}"));
        }
        format!(
            "{} problem(s) with the signatures in {} — nothing was sent",
            errors.len(),
            dir.display()
        )
    })?;
    let entries = artifact
        .plugins
        .iter_mut()
        .chain(artifact.models.iter_mut());
    for (entry, outcome) in entries.zip(&outcomes) {
        if let Outcome::Signed { signature, .. } = outcome
            && let Some(obj) = entry.as_object_mut()
        {
            obj.insert("signature".to_string(), json!(signature));
        }
    }
    Ok(subjects.into_iter().zip(outcomes).collect())
}

/// One line per subject: where its signature came from, or that it has none.
pub fn print_signature_report(
    signed: &[(Subject, Outcome)],
    dir: &std::path::Path,
    report: &dyn Reporter,
) {
    let width = signed.iter().map(|(s, _)| s.id.len()).max().unwrap_or(0);
    for (subject, outcome) in signed {
        match outcome {
            Outcome::Signed {
                file,
                replaced_carried,
                ..
            } => report.out(&format!(
                "signed    {:width$}  <- {}{}",
                subject.id,
                file.display(),
                if *replaced_carried {
                    " (replaces the signature the artifact carried)"
                } else {
                    ""
                }
            )),
            Outcome::Carried => report.out(&format!(
                "carried   {:width$}  (signature from the artifact; none in {})",
                subject.id,
                dir.display()
            )),
            Outcome::Unsigned { looked_for } => report.out(&format!(
                "unsigned  {:width$}  (no {})",
                subject.id,
                looked_for.join(" or ")
            )),
        }
    }
}

/// `--signatures <dir>` (or `[packages] signatures_dir`): attach and report,
/// or do nothing without one.
pub fn attach_from_dir(
    artifact: &mut PackageArtifact,
    signatures: Option<&str>,
    report: &dyn Reporter,
) -> Result<Vec<(Subject, Outcome)>, Error> {
    let Some(dir) = signatures else {
        return Ok(Vec::new());
    };
    let dir = std::path::Path::new(dir);
    let signed = attach_signatures(artifact, dir, &|_| Vec::new(), report)?;
    print_signature_report(&signed, dir, report);
    Ok(signed)
}

#[cfg(test)]
mod tests {
    use super::super::artifact::artifact_content_hash;
    use super::super::artifact::test_support::*;
    use super::*;

    /// A plugin's subject is the same whether its manifest travels as TOML
    /// text or as the object an export writes; a model's falls back to its
    /// bucket key's file name. Attaching a signature leaves the hash alone.
    #[test]
    fn signature_subjects_read_either_manifest_form_and_attaching_keeps_the_hash() {
        use base64::Engine as _;
        let toml = include_str!("../../tests/fixtures/plugins/fixture-upload.toml");
        let component = include_bytes!("../../tests/fixtures/plugins/fixture.wasm");
        let digest = crate::crypto::sha256_digest(component);
        let object = serde_json::to_value(crate::plugin::Manifest::parse(toml).expect("manifest"))
            .expect("object");
        let mut artifact = artifact(vec![model_entry()]);
        artifact.plugins = vec![
            json!({"plugin_id": "test.fixture", "manifest": toml,
                   "component": base64::engine::general_purpose::STANDARD.encode(component)}),
            json!({"plugin_id": "test.fixture", "manifest": object, "digest": digest}),
        ];
        let subjects = signature_subjects(&artifact).expect("subjects");
        assert_eq!(subjects.len(), 3);
        for plugin in &subjects[..2] {
            assert_eq!(plugin.id, "test.fixture");
            assert_eq!(plugin.file_names, ["fixture.wasm"]);
            assert_eq!(plugin.digest, digest);
        }
        assert_eq!(subjects[2].id, "ada.c4-tiny");
        assert_eq!(subjects[2].file_names, ["0.1.0.onnx"]);

        let dir = std::env::temp_dir().join(format!("orion-attach-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        let key = crate::crypto::ed25519::SigningKey::generate();
        std::fs::write(dir.join("test.fixture.sig"), key.sign(&digest)).expect("sig");
        std::fs::write(dir.join("ada.c4-tiny.sig"), key.sign("sha256:abc")).expect("sig");
        artifact.plugins.truncate(1);
        let hash = artifact_content_hash(&artifact).expect("hash");
        let report = attach_signatures(
            &mut artifact,
            &dir,
            &|_| Vec::new(),
            &crate::package::Console,
        )
        .expect("attach");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            report
                .iter()
                .all(|(_, o)| matches!(o, Outcome::Signed { .. }))
        );
        assert_eq!(artifact.plugins[0]["signature"], key.sign(&digest));
        assert_eq!(artifact.models[0]["signature"], key.sign("sha256:abc"));
        assert_eq!(artifact_content_hash(&artifact).expect("hash"), hash);
    }
}
