use serde::{Deserialize, Serialize};

use crate::errors::OrionError;

/// Packages this node applies at startup.
///
/// Each entry is a compiled package artifact — what `orion-server compile`
/// or `package export` writes — applied after the first generation is
/// published, in list order, through the same sequence `orion-server
/// package apply` runs. `/readyz` answers `503` until every one is applied
/// and serving, and a package that fails to apply stops the process with a
/// non-zero exit. A restart with the same artifacts is a no-op.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PackagesConfig {
    /// Paths of the artifacts to apply, in order. A later artifact may
    /// `require` what an earlier one carries.
    pub apply: Vec<String>,

    /// A directory of detached signatures attached to each artifact's
    /// plugins and models before it is applied — `<id>.sig` or
    /// `<artifact file>.sig`, as `orion-server plugin sign -o <dir>` writes
    /// them. Unset attaches nothing.
    pub signatures_dir: Option<String>,

    /// The longest the whole startup apply may take, model admission
    /// included, before the node gives up and exits. `0` means no limit.
    pub apply_timeout_secs: u64,
}

impl Default for PackagesConfig {
    fn default() -> Self {
        Self {
            apply: Vec::new(),
            signatures_dir: None,
            apply_timeout_secs: 1800,
        }
    }
}

impl PackagesConfig {
    /// The shape only. Whether the files exist and lint is checked by
    /// `validate-config` and again at startup, not here: this runs for every
    /// subcommand, `migrate` in an init container included, where the
    /// artifacts may not be mounted.
    pub fn validate(&self) -> Result<(), OrionError> {
        let config = |message: String| OrionError::Config { message };
        for (index, path) in self.apply.iter().enumerate() {
            if path.trim().is_empty() {
                return Err(config(format!("packages.apply[{index}] is empty")));
            }
            if self.apply[..index].contains(path) {
                return Err(config(format!(
                    "packages.apply[{index}] '{path}' is listed twice"
                )));
            }
        }
        if self
            .signatures_dir
            .as_deref()
            .is_some_and(|dir| dir.trim().is_empty())
        {
            return Err(config(
                "packages.signatures_dir is empty — remove it to attach no signatures".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shape_is_checked_and_the_files_are_not() {
        let mut config = PackagesConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.apply_timeout_secs, 1800);
        config.apply = vec!["/pkg/missing.json".to_string()];
        assert!(
            config.validate().is_ok(),
            "a missing file is not refused here"
        );
        config.apply.push(" ".to_string());
        assert!(
            config
                .validate()
                .expect_err("refused")
                .to_string()
                .contains("apply[1] is empty")
        );
        config.apply[1] = "/pkg/missing.json".to_string();
        assert!(
            config
                .validate()
                .expect_err("refused")
                .to_string()
                .contains("listed twice")
        );
        config.apply.pop();
        config.signatures_dir = Some(String::new());
        assert!(config.validate().is_err());
    }
}
