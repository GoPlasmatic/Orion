//! This build's version, and the range a definition set or an artifact may
//! declare it needs (`package.requires.orion`).
//!
//! A set using newer features, fed to an older binary, fails with errors
//! that blame the definitions — `unknown variant 'cron'`, an unknown plugin
//! function — instead of the version. A declared range lets every surface
//! check itself first and say so in one line.
//!
//! A leaf: it names `semver` and `std`, nothing else.

/// This build. One spelling for every surface that reports or compares it.
pub const ORION_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A parsed `requires.orion` range.
#[derive(Debug, Clone)]
pub struct OrionRequirement {
    text: String,
    req: semver::VersionReq,
}

impl OrionRequirement {
    /// Parse a range in `semver`'s comparator syntax: `>=1.8.2, <2`,
    /// `^1.8`, `~1.8.2`, `1.8.*`.
    ///
    /// # Errors
    ///
    /// Not a range, or one that admits every version (`*`, empty) — a
    /// declaration that constrains nothing is a mistake, not a range.
    pub fn parse(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed == "*" {
            return Err(format!(
                "'{text}' constrains nothing — write the range this set needs, like \">=1.8.2, <2\""
            ));
        }
        let req = semver::VersionReq::parse(trimmed).map_err(|e| {
            format!("'{text}' is not a version range ({e}) — write it like \">=1.8.2, <2\"")
        })?;
        Ok(Self {
            text: trimmed.to_string(),
            req,
        })
    }

    /// The range as written.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether the range admits `version`. A pre-release or build suffix is
    /// ignored — `1.9.0-rc.1` is judged as `1.9.0` — because `semver` only
    /// matches a pre-release that a comparator names, which would fail every
    /// release candidate against every range.
    ///
    /// # Errors
    ///
    /// `version` is not a version.
    pub fn admits(&self, version: &str) -> Result<bool, String> {
        let mut parsed = semver::Version::parse(version.trim())
            .map_err(|e| format!("'{version}' is not a version ({e})"))?;
        parsed.pre = semver::Prerelease::EMPTY;
        parsed.build = semver::BuildMetadata::EMPTY;
        Ok(self.req.matches(&parsed))
    }

    /// Against [`ORION_VERSION`]. `what` names the thing that declared the
    /// range.
    ///
    /// # Errors
    ///
    /// `{what} requires Orion {range}; this is orion-server {version}`.
    pub fn check_this_binary(&self, what: &str) -> Result<(), String> {
        if self.admits(ORION_VERSION)? {
            return Ok(());
        }
        Err(format!(
            "{what} requires Orion {}; this is orion-server {ORION_VERSION}",
            self.text
        ))
    }

    /// Against a target instance's version.
    ///
    /// # Errors
    ///
    /// `{what} requires Orion {range}; the target {target} runs {version}`,
    /// or a version that does not parse.
    pub fn check_target(&self, what: &str, target: &str, version: &str) -> Result<(), String> {
        if self.admits(version)? {
            return Ok(());
        }
        Err(format!(
            "{what} requires Orion {}; the target {target} runs {version}",
            self.text
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_admits_and_refuses() {
        let req = OrionRequirement::parse(">=1.8.2, <2").expect("range");
        assert!(req.admits("1.8.2").expect("version"));
        assert!(req.admits("1.99.0").expect("version"));
        assert!(!req.admits("1.8.1").expect("version"));
        assert!(!req.admits("2.0.0").expect("version"));
        assert!(
            OrionRequirement::parse("1.8.*")
                .expect("range")
                .admits("1.8.7")
                .expect("v")
        );
        assert!(
            OrionRequirement::parse("^1.8")
                .expect("range")
                .admits("1.9.0")
                .expect("v")
        );
    }

    #[test]
    fn a_prerelease_binary_is_judged_as_its_release() {
        let req = OrionRequirement::parse(">=1.8.2, <2").expect("range");
        assert!(req.admits("1.9.0-rc.1").expect("version"));
        assert!(req.admits("1.9.0+abc").expect("version"));
        assert!(!req.admits("2.0.0-rc.1").expect("version"));
    }

    #[test]
    fn a_malformed_range_says_how_to_write_one() {
        let err = OrionRequirement::parse(">=1.8.x <").expect_err("malformed");
        assert!(
            err.contains("is not a version range") && err.contains(">=1.8.2, <2"),
            "{err}"
        );
    }

    #[test]
    fn star_and_empty_are_refused() {
        for text in ["", "  ", "*"] {
            let err = OrionRequirement::parse(text).expect_err(text);
            assert!(err.contains("constrains nothing"), "{err}");
        }
    }

    #[test]
    fn the_messages_name_the_range_and_both_versions() {
        let req = OrionRequirement::parse(">=99.0.0").expect("range");
        let err = req
            .check_this_binary("the definition set (definitions/package.json)")
            .expect_err("too old");
        assert_eq!(
            err,
            format!(
                "the definition set (definitions/package.json) requires Orion >=99.0.0; this \
                 is orion-server {ORION_VERSION}"
            )
        );
        let err = req
            .check_target("orders@1.4.0", "https://prod", "1.8.2")
            .expect_err("target too old");
        assert_eq!(
            err,
            "orders@1.4.0 requires Orion >=99.0.0; the target https://prod runs 1.8.2"
        );
        OrionRequirement::parse(&format!(">={ORION_VERSION}"))
            .expect("range")
            .check_this_binary("x")
            .expect("this binary satisfies its own version");
    }
}
