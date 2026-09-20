//! `orion-server plugin|model digest|keygen|pubkey|sign|verify`, through the
//! binary: what a release pipeline runs to produce what `[plugins.trust]`
//! and `[models.trust]` check.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn orion(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_orion-server"))
        .args(args)
        .env_remove("ORION_SIGNING_KEY")
        .output()
        .expect("invoke orion-server")
}

#[track_caller]
fn ok(out: &Output) -> String {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

#[track_caller]
fn fails(out: &Output) -> String {
    assert!(
        !out.status.success(),
        "expected failure; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("orion-signing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("scratch dir");
        Self(path)
    }

    fn path(&self, rel: &str) -> String {
        self.0.join(rel).to_string_lossy().into_owned()
    }

    /// A copy of the fixture plugin — manifest and component — under `rel`.
    fn plugin(&self, rel: &str) -> String {
        let dir = self.0.join(rel);
        std::fs::create_dir_all(&dir).expect("plugin dir");
        for file in ["fixture.toml", "fixture.wasm"] {
            std::fs::copy(
                Path::new(FIXTURES).join("plugins").join(file),
                dir.join(file),
            )
            .expect("copy fixture");
        }
        dir.to_string_lossy().into_owned()
    }

    /// A copy of the fixture model — manifest and artifact.
    fn model(&self, rel: &str) -> String {
        let dir = self.0.join(rel);
        std::fs::create_dir_all(&dir).expect("model dir");
        for file in ["model.json", "c4-tiny.onnx"] {
            std::fs::copy(
                Path::new(FIXTURES).join("models/c4-tiny").join(file),
                dir.join(file),
            )
            .expect("copy fixture");
        }
        dir.to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn keygen(scratch: &Scratch, name: &str) -> (String, String) {
    let key = scratch.path(name);
    let stdout = ok(&orion(&["plugin", "keygen", "-o", &key]));
    let public = stdout
        .lines()
        .find_map(|l| l.strip_prefix("public_keys value: "))
        .expect("keygen prints the public key")
        .to_string();
    (key, public)
}

#[test]
fn digest_of_a_dir_a_manifest_and_a_bare_file_agree() {
    let scratch = Scratch::new();
    let dir = scratch.plugin("p");
    let expected = orion::plugin::WasmRuntime::digest(
        &std::fs::read(Path::new(&dir).join("fixture.wasm")).expect("component"),
    );
    for path in [
        dir.clone(),
        format!("{dir}/fixture.toml"),
        format!("{dir}/fixture.wasm"),
    ] {
        assert_eq!(
            ok(&orion(&["plugin", "digest", &path])).trim(),
            expected,
            "{path}"
        );
    }
}

#[test]
fn keygen_sign_verify_round_trip() {
    let scratch = Scratch::new();
    let dir = scratch.plugin("p");
    let (key, public) = keygen(&scratch, "signer.pem");
    assert_eq!(
        ok(&orion(&["plugin", "pubkey", "--key", &key])).trim(),
        public
    );

    let stdout = ok(&orion(&["plugin", "sign", &dir, "--key", &key]));
    assert!(stdout.contains("fixture.wasm.sig"), "{stdout}");
    let sig = std::fs::read_to_string(Path::new(&dir).join("fixture.wasm.sig")).expect("sig");
    assert!(sig.ends_with('\n') && sig.trim().len() == 88, "{sig:?}");

    let stdout = ok(&orion(&["plugin", "verify", &dir, "--public-key", &public]));
    assert!(stdout.starts_with("ok  test.fixture  sha256:"), "{stdout}");

    // Another key does not verify it.
    let (_, stranger) = keygen(&scratch, "stranger.pem");
    let stderr = fails(&orion(&[
        "plugin",
        "verify",
        &dir,
        "--public-key",
        &stranger,
    ]));
    assert!(stderr.contains("does not verify"), "{stderr}");

    // The key from the environment signs identically: Ed25519 is
    // deterministic.
    std::fs::remove_file(Path::new(&dir).join("fixture.wasm.sig")).expect("rm");
    let pem = std::fs::read_to_string(&key).expect("pem");
    let out = Command::new(env!("CARGO_BIN_EXE_orion-server"))
        .args(["plugin", "sign", &dir])
        .env("ORION_SIGNING_KEY", pem)
        .output()
        .expect("sign from env");
    ok(&out);
    assert_eq!(
        std::fs::read_to_string(Path::new(&dir).join("fixture.wasm.sig")).expect("sig"),
        sig
    );

    // No key anywhere is an error naming both sources.
    let stderr = fails(&orion(&["plugin", "sign", &dir]));
    assert!(
        stderr.contains("--key") && stderr.contains("ORION_SIGNING_KEY"),
        "{stderr}"
    );
}

#[test]
fn keygen_refuses_to_overwrite() {
    let scratch = Scratch::new();
    let (key, _) = keygen(&scratch, "signer.pem");
    let before = std::fs::read_to_string(&key).expect("key");
    let stderr = fails(&orion(&["plugin", "keygen", "-o", &key]));
    assert!(stderr.contains("--force"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&key).expect("key"), before);
    ok(&orion(&["plugin", "keygen", "-o", &key, "--force"]));
    assert_ne!(std::fs::read_to_string(&key).expect("key"), before);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&key).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "a private key is owner-only");
    }
}

#[test]
fn verify_without_a_key_is_an_error_not_ok() {
    let scratch = Scratch::new();
    let dir = scratch.plugin("p");
    let (key, _) = keygen(&scratch, "signer.pem");
    ok(&orion(&["plugin", "sign", &dir, "--key", &key]));
    let stderr = fails(&orion(&["plugin", "verify", &dir]));
    assert!(stderr.contains("no public key"), "{stderr}");

    // A config with no trust keys is refused the same way: its node checks
    // nothing, so "ok" would be a lie.
    let config = scratch.path("config.toml");
    std::fs::write(&config, "[plugins]\nenabled = true\n").expect("config");
    let stderr = fails(&orion(&["-c", &config, "plugin", "verify", &dir]));
    assert!(stderr.contains("declares no public_keys"), "{stderr}");
}

#[test]
fn verify_reads_the_nodes_own_trust_keys_with_dash_c() {
    let scratch = Scratch::new();
    let dir = scratch.plugin("p");
    let (key, public) = keygen(&scratch, "signer.pem");
    ok(&orion(&["plugin", "sign", &dir, "--key", &key]));
    let config = scratch.path("config.toml");
    std::fs::write(
        &config,
        format!("[plugins]\nenabled = true\n\n[plugins.trust]\npublic_keys = [\"{public}\"]\n"),
    )
    .expect("config");
    let stdout = ok(&orion(&["-c", &config, "plugin", "verify", &dir]));
    assert!(stdout.contains("ok  test.fixture"), "{stdout}");
    // The model noun reads [models.trust], which this config leaves empty.
    let model = scratch.model("m");
    let stderr = fails(&orion(&["-c", &config, "model", "verify", &model]));
    assert!(stderr.contains("[models.trust]"), "{stderr}");
}

#[test]
fn sign_into_a_dir_writes_the_layout_a_signatures_dir_reads() {
    let scratch = Scratch::new();
    let first = scratch.plugin("a");
    let second = scratch.plugin("b");
    let manifest = Path::new(&second).join("fixture.toml");
    let text = std::fs::read_to_string(&manifest).expect("manifest");
    std::fs::write(&manifest, text.replace("test.fixture", "test.other")).expect("rename");
    let (key, public) = keygen(&scratch, "signer.pem");
    let sigs = scratch.path("sigs");

    // Two plugins whose components share a file name cannot both be
    // `fixture.wasm.sig` — the collision names both and the way out.
    let both = scratch.0.to_string_lossy().into_owned();
    let stderr = fails(&orion(&[
        "plugin", "sign", &both, "--key", &key, "-o", &sigs,
    ]));
    assert!(
        stderr.contains("test.fixture")
            && stderr.contains("test.other")
            && stderr.contains("--by-id"),
        "{stderr}"
    );
    let stdout = ok(&orion(&[
        "plugin", "sign", &both, "--key", &key, "-o", &sigs, "--by-id",
    ]));
    assert!(stdout.contains("test.other.sig"), "{stdout}");
    std::fs::remove_dir_all(&sigs).expect("reset");

    ok(&orion(&[
        "plugin",
        "sign",
        &first,
        "--key",
        &key,
        "-o",
        &format!("{sigs}/"),
    ]));
    assert!(Path::new(&sigs).join("fixture.wasm.sig").is_file());
    ok(&orion(&[
        "plugin", "sign", &first, "--key", &key, "-o", &sigs, "--by-id",
    ]));
    assert!(Path::new(&sigs).join("test.fixture.sig").is_file());

    let stdout = ok(&orion(&[
        "plugin",
        "verify",
        &first,
        "--signatures",
        &sigs,
        "--public-key",
        &public,
    ]));
    assert!(stdout.contains("ok  test.fixture"), "{stdout}");

    // The library's lookup finds the same file the verb wrote.
    let dir = orion::signatures::SignatureDir::open(Path::new(&sigs)).expect("open");
    let subject = orion::signatures::Subject {
        kind: orion::signatures::Kind::Plugin,
        id: "test.fixture".to_string(),
        file_names: vec!["fixture.wasm".to_string()],
        digest: String::new(),
        carried: None,
    };
    assert_eq!(
        dir.lookup(&subject),
        Some(Path::new(&sigs).join("test.fixture.sig").as_path())
    );

    // --by-id needs an id.
    let stderr = fails(&orion(&[
        "plugin",
        "sign",
        &format!("{first}/fixture.wasm"),
        "--key",
        &key,
        "--by-id",
    ]));
    assert!(stderr.contains("bare file"), "{stderr}");
}

#[test]
fn a_model_manifest_under_the_plugin_noun_is_refused() {
    let scratch = Scratch::new();
    let model = scratch.model("m");
    let stderr = fails(&orion(&[
        "plugin",
        "digest",
        &format!("{model}/model.json"),
    ]));
    assert!(stderr.contains("use `orion-server model`"), "{stderr}");
    let stderr = fails(&orion(&["plugin", "digest", &model]));
    assert!(stderr.contains("use `orion-server model`"), "{stderr}");
    let plugin = scratch.plugin("p");
    let stderr = fails(&orion(&[
        "model",
        "digest",
        &format!("{plugin}/fixture.toml"),
    ]));
    assert!(stderr.contains("use `orion-server plugin`"), "{stderr}");
}

#[test]
fn the_model_noun_signs_the_artifact_not_the_manifest() {
    let scratch = Scratch::new();
    let model = scratch.model("m");
    let (key, public) = keygen(&scratch, "signer.pem");
    let artifact = std::fs::read(Path::new(&model).join("c4-tiny.onnx")).expect("artifact");
    assert_eq!(
        ok(&orion(&["model", "digest", &model])).trim(),
        orion::crypto::sha256_digest(&artifact)
    );
    ok(&orion(&["model", "sign", &model, "--key", &key]));
    assert!(Path::new(&model).join("c4-tiny.onnx.sig").is_file());
    let stdout = ok(&orion(&[
        "model",
        "verify",
        &model,
        "--public-key",
        &public,
    ]));
    assert!(stdout.starts_with("ok  "), "{stdout}");
}

#[test]
fn a_manifest_whose_artifact_is_missing_is_an_error() {
    let scratch = Scratch::new();
    let dir = scratch.plugin("p");
    std::fs::remove_file(Path::new(&dir).join("fixture.wasm")).expect("rm");
    let stderr = fails(&orion(&["plugin", "digest", &dir]));
    assert!(stderr.contains("'fixture.wasm' is not on disk"), "{stderr}");
    let empty = scratch.path("empty");
    std::fs::create_dir_all(&empty).expect("dir");
    let stderr = fails(&orion(&["plugin", "digest", &empty]));
    assert!(stderr.contains("no plugin manifest under"), "{stderr}");
}
