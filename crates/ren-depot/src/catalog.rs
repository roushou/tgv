//! Reading a catalog from its TOML form (`catalog.toml` is the built-in
//! one). Every value parses into its typed form here, once.

use crate::{
    package::{Kind, Package, PackageId, ProgramName},
    source::{Artifact, Release, Source},
};
use serde::Deserialize;
use std::collections::BTreeSet;

/// Why a catalog was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("the catalog is not valid: {0}")]
    Syntax(String),
    #[error("the catalog names {0} twice")]
    Duplicate(PackageId),
    #[error("{package}'s release has two artifacts for one platform")]
    DuplicatePlatform { package: PackageId },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFile {
    package: Vec<PackageEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageEntry {
    id: PackageId,
    kind: Kind,
    program: ProgramName,
    release: Option<ReleaseEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseEntry {
    version: semver::Version,
    artifact: Vec<Artifact>,
}

/// A catalog's packages, in the order it lists them.
pub(crate) struct Catalog(pub Vec<Package>);

impl Catalog {
    pub(crate) fn read(text: &str) -> Result<Self, CatalogError> {
        let file: CatalogFile = toml::from_str(text)
            .map_err(|error| CatalogError::Syntax(error.message().to_owned()))?;
        let mut seen = BTreeSet::new();
        let mut packages = Vec::with_capacity(file.package.len());
        for entry in file.package {
            if !seen.insert(entry.id.clone()) {
                return Err(CatalogError::Duplicate(entry.id));
            }
            let source = match entry.release {
                None => Source::System,
                Some(release) => {
                    let platforms: BTreeSet<&str> = release
                        .artifact
                        .iter()
                        .map(|artifact| artifact.platform.into())
                        .collect();
                    if platforms.len() != release.artifact.len() {
                        return Err(CatalogError::DuplicatePlatform { package: entry.id });
                    }
                    Source::Release(Release {
                        version: release.version,
                        artifacts: release.artifact,
                    })
                }
            };
            packages.push(Package::new(entry.id, entry.kind, entry.program, source));
        }
        Ok(Self(packages))
    }
}

#[cfg(test)]
mod tests {
    use super::{Catalog, CatalogError};

    #[test]
    fn refuses_unknown_fields_bad_values_and_duplicates() {
        let package = "[[package]]\nid = \"jq\"\nkind = \"utility\"\nprogram = \"jq\"\n";
        assert_eq!(Catalog::read(package).unwrap().0.len(), 1);
        assert!(matches!(
            Catalog::read(&format!("{package}{package}")),
            Err(CatalogError::Duplicate(_))
        ));
        for bad in [
            "[[package]]\nid = \"jq\"\nkind = \"utility\"\nprogram = \"jq\"\nextra = 1\n",
            "[[package]]\nid = \"JQ\"\nkind = \"utility\"\nprogram = \"jq\"\n",
            "[[package]]\nid = \"jq\"\nkind = \"tool\"\nprogram = \"jq\"\n",
        ] {
            assert!(
                matches!(Catalog::read(bad), Err(CatalogError::Syntax(_))),
                "{bad}"
            );
        }
        let artifact = "[[package.release.artifact]]\nplatform = \"linux_x64\"\nurl = \"https://example.test/jq\"\nsha256 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nformat = \"bare\"\nprogram_at = \"jq\"\n";
        let twice =
            format!("{package}[package.release]\nversion = \"1.8.2\"\n{artifact}{artifact}");
        assert!(matches!(
            Catalog::read(&twice),
            Err(CatalogError::DuplicatePlatform { .. })
        ));
        let http = artifact.replace("https://", "http://");
        assert!(matches!(
            Catalog::read(&format!(
                "{package}[package.release]\nversion = \"1.8.2\"\n{http}"
            )),
            Err(CatalogError::Syntax(_))
        ));
    }
}
