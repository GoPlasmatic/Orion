//! The detached-signature file convention: what a `.sig` file is called and
//! what is inside it.
//!
//! A signature for `[plugins.trust]` or `[models.trust]` belongs to whoever
//! holds the key — the deployment — not to the definition set, so it travels
//! beside the artifact as a file rather than inside it. One rule, read by
//! every surface that writes or looks for one: `orion-server plugin|model
//! sign|verify`, and the package verbs that attach signatures at deploy time.
//!
//! - **Contents.** Text: the standard-base64 Ed25519 signature over the
//!   ASCII digest string `sha256:<64 hex>`. Whitespace is ignored, so a
//!   trailing newline or a wrapped `base64` reads as what it spells
//!   ([`crate::crypto::ed25519::normalize_signature`]).
//! - **Name.** `<artifact file name>.sig` beside the artifact
//!   (`scoring.wasm` → `scoring.wasm.sig`), or `<id>.sig` in a directory of
//!   signatures, where file names can collide and ids cannot. A lookup tries
//!   the id form first.
//!
//! A leaf: it names `crate::crypto` and `std`, nothing else.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The extension of a detached signature file.
pub const SIG_EXTENSION: &str = "sig";

/// `scoring.wasm` → `scoring.wasm.sig`.
pub fn sig_file_name(artifact_file_name: &str) -> String {
    format!("{artifact_file_name}.{SIG_EXTENSION}")
}

/// Read one `.sig` file as the one-line base64 an upload carries.
///
/// # Errors
///
/// Unreadable, or not a base64 Ed25519 signature — prefixed with the path.
pub fn read_sig_file(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    crate::crypto::ed25519::normalize_signature(&text)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Which trust table a signature is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plugin,
    Model,
}

impl Kind {
    /// `plugin` / `model`, as a message names it.
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Plugin => "plugin",
            Kind::Model => "model",
        }
    }
}

/// One signed artifact, as a lookup needs to know it.
#[derive(Debug, Clone)]
pub struct Subject {
    pub kind: Kind,
    /// The plugin or model id; empty for a bare file with no manifest.
    pub id: String,
    /// Candidate artifact file names, most specific first. May be empty.
    pub file_names: Vec<String>,
    /// `sha256:<64 hex>` — the signed message.
    pub digest: String,
    /// A signature the entry already carries (an export's, or compile-time).
    pub carried: Option<String>,
}

impl Subject {
    /// The `.sig` file names that would match this subject, in the order a
    /// lookup tries them: the id form, then each file name.
    pub fn candidates(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.id.is_empty() {
            out.push(sig_file_name(&self.id));
        }
        for name in &self.file_names {
            let candidate = sig_file_name(name);
            if !out.contains(&candidate) {
                out.push(candidate);
            }
        }
        out
    }
}

/// A directory of detached signatures: every `*.sig` file directly in it.
///
/// Only regular files (or links to them) with the `.sig` extension are
/// considered, and never a dotfile or anything in a sub-directory — so a
/// mounted Kubernetes secret, whose real files sit under a `..data` link
/// farm, is read by the names it shows and not twice.
#[derive(Debug)]
pub struct SignatureDir {
    root: PathBuf,
    files: BTreeMap<String, PathBuf>,
}

impl SignatureDir {
    /// # Errors
    ///
    /// Not a directory, or unreadable.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let entries = std::fs::read_dir(dir)
            .map_err(|e| format!("signatures directory {}: {e}", dir.display()))?;
        let mut files = BTreeMap::new();
        for entry in entries {
            let entry =
                entry.map_err(|e| format!("signatures directory {}: {e}", dir.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if name.starts_with('.')
                || path.extension().and_then(|e| e.to_str()) != Some(SIG_EXTENSION)
                || !std::fs::metadata(&path).is_ok_and(|m| m.is_file())
            {
                continue;
            }
            files.insert(name, path);
        }
        Ok(Self {
            root: dir.to_path_buf(),
            files,
        })
    }

    /// The directory this was opened on.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every `.sig` file name in the directory, sorted.
    pub fn file_names(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// One subject's file — `<id>.sig` before `<file name>.sig` — with no
    /// accounting for the files nothing claims.
    pub fn lookup(&self, subject: &Subject) -> Option<&Path> {
        subject
            .candidates()
            .iter()
            .find_map(|name| self.files.get(name))
            .map(PathBuf::as_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("orion-signatures-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("scratch dir");
            Self(path)
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("parent");
            }
            std::fs::write(&path, text).expect("write");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn subject(id: &str, files: &[&str]) -> Subject {
        Subject {
            kind: Kind::Plugin,
            id: id.to_string(),
            file_names: files.iter().map(|f| (*f).to_string()).collect(),
            digest: crate::crypto::sha256_digest(b"x"),
            carried: None,
        }
    }

    #[test]
    fn a_sig_file_is_named_after_the_artifact() {
        assert_eq!(sig_file_name("scoring.wasm"), "scoring.wasm.sig");
        assert_eq!(
            subject("acme.scoring", &["scoring.wasm"]).candidates(),
            ["acme.scoring.sig", "scoring.wasm.sig"]
        );
        assert_eq!(
            subject("", &["model.onnx"]).candidates(),
            ["model.onnx.sig"]
        );
    }

    #[test]
    fn the_id_form_wins_over_the_file_form() {
        let dir = Scratch::new();
        let key = crate::crypto::ed25519::SigningKey::generate();
        let sig = key.sign("sha256:00");
        dir.write("scoring.wasm.sig", &sig);
        let by_id = dir.write("acme.scoring.sig", &sig);
        let sigs = SignatureDir::open(&dir.0).expect("open");
        assert_eq!(
            sigs.lookup(&subject("acme.scoring", &["scoring.wasm"])),
            Some(by_id.as_path())
        );
        assert!(
            sigs.lookup(&subject("acme.other", &["other.wasm"]))
                .is_none()
        );
    }

    #[test]
    fn sub_directories_and_dotfiles_are_ignored() {
        let dir = Scratch::new();
        dir.write("..data/scoring.wasm.sig", "x");
        dir.write(".hidden.sig", "x");
        dir.write("notes.txt", "x");
        dir.write("scoring.wasm.sig", "x");
        let sigs = SignatureDir::open(&dir.0).expect("open");
        assert_eq!(sigs.file_names().collect::<Vec<_>>(), ["scoring.wasm.sig"]);
    }

    #[test]
    fn whitespace_is_tolerated_and_garbage_names_the_file() {
        let dir = Scratch::new();
        let key = crate::crypto::ed25519::SigningKey::generate();
        let sig = key.sign("sha256:00");
        let (a, b) = sig.split_at(40);
        let wrapped = dir.write("a.sig", &format!("{a}\n{b}\n"));
        assert_eq!(read_sig_file(&wrapped).expect("wrapped"), sig);
        let bad = dir.write("b.sig", "not base64!");
        let err = read_sig_file(&bad).expect_err("garbage");
        assert!(err.contains("b.sig") && err.contains("not base64"), "{err}");
    }

    #[test]
    fn opening_a_missing_directory_says_which() {
        let err = SignatureDir::open(Path::new("/definitely/not/here")).expect_err("missing");
        assert!(err.contains("/definitely/not/here"), "{err}");
    }
}
