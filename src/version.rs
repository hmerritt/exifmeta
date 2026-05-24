pub const APP_NAME: &str = "exifmeta";
pub const BINARY_FILENAME: &str = "exifmeta";

const UNKNOWN: &str = "unknown";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    pub revision: String,
    pub branch: String,
    pub build_date: String,
    pub app_name: String,
    pub version: String,
    pub version_prerelease: String,
    pub version_metadata: String,
}

pub fn get_version() -> VersionInfo {
    let configured_prerelease = option_env!("EXIFMETA_VERSION_PRERELEASE").unwrap_or_default();
    let version_prerelease = if configured_prerelease.is_empty()
        && option_env!("EXIFMETA_GIT_DIRTY").unwrap_or_default() == "true"
    {
        "dirty"
    } else {
        configured_prerelease
    };

    VersionInfo {
        revision: option_env!("EXIFMETA_GIT_COMMIT")
            .unwrap_or_default()
            .to_string(),
        branch: option_env!("EXIFMETA_GIT_BRANCH")
            .unwrap_or_default()
            .to_string(),
        build_date: option_env!("EXIFMETA_BUILD_DATE")
            .unwrap_or_default()
            .to_string(),
        app_name: APP_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        version_prerelease: version_prerelease.to_string(),
        version_metadata: option_env!("EXIFMETA_VERSION_METADATA")
            .unwrap_or_default()
            .to_string(),
    }
}

impl VersionInfo {
    pub fn version_number(&self) -> String {
        if self.version == UNKNOWN && self.version_prerelease == UNKNOWN {
            return "Version unknown".to_string();
        }

        let mut version = self.version.clone();

        if !self.version_prerelease.is_empty() {
            version.push('-');
            version.push_str(&self.version_prerelease);
        }

        if !self.version_metadata.is_empty() {
            version.push('+');
            version.push_str(&self.version_metadata);
        }

        version
    }

    pub fn full_version_number(&self, include_revision: bool) -> String {
        if self.version == UNKNOWN && self.version_prerelease == UNKNOWN {
            return format!("{} [Version unknown]", self.app_name);
        }

        let mut version = format!("{} [Version {}", self.app_name, self.version_number());

        if include_revision && !self.revision.is_empty() {
            version.push_str(" (");

            if !self.branch.is_empty() && self.branch != "master" && self.branch != "HEAD" {
                version.push_str(&self.branch);
                version.push('/');
            }

            version.push_str(&self.revision);
            version.push(')');
        }

        version.push(']');
        version
    }

    pub fn should_include_revision(&self) -> bool {
        !(self.branch == "master"
            && self.version_prerelease.is_empty()
            && self.version_metadata.is_empty())
    }
}

pub fn title() -> String {
    let version = get_version();

    format!(
        "{}\n(c) MerrittCorp. All rights reserved.\n",
        version.full_version_number(version.should_include_revision())
    )
}

pub fn print_title() {
    println!("{}", title());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version_info(
        version: &str,
        prerelease: &str,
        metadata: &str,
        branch: &str,
        revision: &str,
    ) -> VersionInfo {
        VersionInfo {
            revision: revision.to_string(),
            branch: branch.to_string(),
            build_date: String::new(),
            app_name: APP_NAME.to_string(),
            version: version.to_string(),
            version_prerelease: prerelease.to_string(),
            version_metadata: metadata.to_string(),
        }
    }

    #[test]
    fn formats_version_number() {
        assert_eq!(
            version_info("1.2.3", "", "", "", "").version_number(),
            "1.2.3"
        );
        assert_eq!(
            version_info("1.2.3", "dev", "", "", "").version_number(),
            "1.2.3-dev"
        );
        assert_eq!(
            version_info("1.2.3", "", "abc123", "", "").version_number(),
            "1.2.3+abc123"
        );
        assert_eq!(
            version_info("1.2.3", "dev", "abc123", "", "").version_number(),
            "1.2.3-dev+abc123"
        );
        assert_eq!(
            version_info(UNKNOWN, UNKNOWN, "", "", "").version_number(),
            "Version unknown"
        );
    }

    #[test]
    fn formats_full_version_number_without_revision() {
        assert_eq!(
            version_info("1.2.3", "", "", "", "").full_version_number(false),
            "exifmeta [Version 1.2.3]"
        );
    }

    #[test]
    fn formats_full_version_number_with_revision() {
        assert_eq!(
            version_info("1.2.3", "", "", "", "abc123").full_version_number(true),
            "exifmeta [Version 1.2.3 (abc123)]"
        );
        assert_eq!(
            version_info("1.2.3", "", "", "feature", "abc123").full_version_number(true),
            "exifmeta [Version 1.2.3 (feature/abc123)]"
        );
        assert_eq!(
            version_info("1.2.3", "", "", "master", "abc123").full_version_number(true),
            "exifmeta [Version 1.2.3 (abc123)]"
        );
        assert_eq!(
            version_info("1.2.3", "", "", "HEAD", "abc123").full_version_number(true),
            "exifmeta [Version 1.2.3 (abc123)]"
        );
    }

    #[test]
    fn formats_development_version_with_branch_and_short_revision() {
        assert_eq!(
            version_info("0.1.0", "", "", "v2", "230027c9dc06").full_version_number(true),
            "exifmeta [Version 0.1.0 (v2/230027c9dc06)]"
        );
    }

    #[test]
    fn formats_dirty_version() {
        assert_eq!(
            version_info("0.1.0", "dirty", "", "v2", "230027c9dc06").full_version_number(true),
            "exifmeta [Version 0.1.0-dirty (v2/230027c9dc06)]"
        );
        assert_eq!(
            version_info("0.1.0", "dirty", "", "master", "230027c9dc06").full_version_number(true),
            "exifmeta [Version 0.1.0-dirty (230027c9dc06)]"
        );
    }

    #[test]
    fn detects_when_revision_should_be_included() {
        assert!(!version_info("0.1.0", "", "", "master", "230027c9dc06").should_include_revision());
        assert!(
            version_info("0.1.0", "dirty", "", "master", "230027c9dc06").should_include_revision()
        );
        assert!(version_info("0.1.0", "", "", "v2", "230027c9dc06").should_include_revision());
        assert!(version_info("0.1.0", "", "", "HEAD", "230027c9dc06").should_include_revision());
    }

    #[test]
    fn formats_full_unknown_version() {
        assert_eq!(
            version_info(UNKNOWN, UNKNOWN, "", "", "").full_version_number(true),
            "exifmeta [Version unknown]"
        );
    }

    #[test]
    fn gets_package_version_and_app_identity() {
        let version = get_version();

        assert_eq!(version.app_name, APP_NAME);
        assert_eq!(version.version, env!("CARGO_PKG_VERSION"));
    }
}
