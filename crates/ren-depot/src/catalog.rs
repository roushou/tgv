//! Reading a catalog from its TOML form (`catalog.toml` is the built-in
//! one), and adding the operator's entries to it. Every value parses into
//! its typed form here, once.

use crate::{
    package::{Kind, Package, PackageId, ProgramName},
    source::{Artifact, Release, Source},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Why a catalog was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("the catalog is not valid: {0}")]
    Syntax(String),
    #[error("the catalog names {0} twice")]
    Duplicate(PackageId),
    #[error("{package}'s release has two artifacts for one platform")]
    DuplicatePlatform { package: PackageId },
    #[error("{first} and {second} both provide the program {program}")]
    SharedProgram {
        program: ProgramName,
        first: PackageId,
        second: PackageId,
    },
    #[error("{0} is a client; configured packages are runtimes and utilities")]
    Client(PackageId),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFile {
    package: Vec<CatalogEntry>,
}

/// One package as a catalog writes it: the built-in `catalog.toml`'s
/// `[[package]]` tables, and the operator's `[[tools.package]]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    pub id: PackageId,
    pub kind: Kind,
    pub program: ProgramName,
    /// The release Ren installs; without one, the package is only found
    /// on the host's search path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleaseEntry>,
}

/// A catalog entry's release: its version and one artifact per platform.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseEntry {
    pub version: semver::Version,
    pub artifact: Vec<Artifact>,
}

impl CatalogEntry {
    /// The package this entry describes.
    fn package(self) -> Result<Package, CatalogError> {
        let source = match self.release {
            None => Source::System,
            Some(release) => {
                let platforms: BTreeSet<&str> = release
                    .artifact
                    .iter()
                    .map(|artifact| artifact.platform.into())
                    .collect();
                if platforms.len() != release.artifact.len() {
                    return Err(CatalogError::DuplicatePlatform { package: self.id });
                }
                Source::Release(Release {
                    version: release.version,
                    artifacts: release.artifact,
                })
            }
        };
        Ok(Package::new(self.id, self.kind, self.program, source))
    }
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
            packages.push(entry.package()?);
        }
        let catalog = Self(packages);
        catalog.check_programs()?;
        Ok(catalog)
    }

    /// This catalog with the operator's `entries`: each adds a package, or
    /// replaces the built-in runtime or utility of the same name in place.
    /// Clients stay built in, since Ren drives them and they update
    /// themselves.
    pub(crate) fn with(mut self, entries: &[CatalogEntry]) -> Result<Self, CatalogError> {
        let mut seen = BTreeSet::new();
        for entry in entries {
            if !seen.insert(entry.id.clone()) {
                return Err(CatalogError::Duplicate(entry.id.clone()));
            }
            let package = entry.clone().package()?.operator();
            if package.kind() == Kind::Client {
                return Err(CatalogError::Client(package.id().clone()));
            }
            match self.0.iter_mut().find(|known| known.id() == package.id()) {
                Some(known) if known.kind() == Kind::Client => {
                    return Err(CatalogError::Client(package.id().clone()));
                }
                Some(known) => *known = package,
                None => self.0.push(package),
            }
        }
        self.check_programs()?;
        Ok(self)
    }

    /// Two packages providing one program would link over each other on a
    /// run's search path.
    fn check_programs(&self) -> Result<(), CatalogError> {
        let mut programs: BTreeMap<&str, &PackageId> = BTreeMap::new();
        for package in &self.0 {
            if let Some(first) = programs.insert(package.program().as_str(), package.id()) {
                return Err(CatalogError::SharedProgram {
                    program: package.program().clone(),
                    first: first.clone(),
                    second: package.id().clone(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Catalog, CatalogEntry, CatalogError};
    use crate::package::{Origin, PackageId};

    /// Operator entries as `ren.toml`'s `[tools]` section writes them.
    fn entries(text: &str) -> Vec<CatalogEntry> {
        #[derive(serde::Deserialize)]
        struct Tools {
            package: Vec<CatalogEntry>,
        }
        toml::from_str::<Tools>(text).unwrap().package
    }

    const ACME: &str = "[[package]]\nid = \"acme-lint\"\nkind = \"utility\"\nprogram = \"acme-lint\"\n[package.release]\nversion = \"0.3.0\"\n[[package.release.artifact]]\nplatform = \"linux_x64\"\nurl = \"https://example.test/acme-lint.tar.gz\"\nsha256 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nformat = \"tar_gz\"\nprogram_at = \"acme-lint\"\n";

    #[test]
    fn operator_entries_add_packages_and_replace_builtin_tools_in_place() {
        let builtin = Catalog::read(include_str!("catalog.toml")).unwrap();
        let count = builtin.0.len();
        let node = "[[package]]\nid = \"node\"\nkind = \"runtime\"\nprogram = \"node\"\n";
        let merged = builtin.with(&entries(&format!("{ACME}{node}"))).unwrap();
        assert_eq!(merged.0.len(), count + 1);
        let last = merged.0.last().unwrap();
        assert_eq!(last.id().as_str(), "acme-lint");
        assert_eq!(last.origin(), Origin::Operator);
        // Node keeps its place and is now found on the host only.
        let position = merged.0.iter().position(|p| p.id() == &PackageId::NODE);
        let node = &merged.0[position.unwrap()];
        assert_eq!(node.origin(), Origin::Operator);
        assert_eq!(node.source(), &crate::source::Source::System);
        assert!(
            merged
                .0
                .iter()
                .filter(|p| p.id() != &PackageId::NODE && p.id().as_str() != "acme-lint")
                .all(|p| p.origin() == Origin::Builtin)
        );
        // The entry writes back as it was read.
        let entry = entries(ACME);
        let written = toml::to_string(&serde_json::json!({ "package": entry })).unwrap();
        assert_eq!(entries(&written), entry);
    }

    #[test]
    fn operator_entries_leave_clients_alone_and_programs_unshared() {
        let builtin = || Catalog::read(include_str!("catalog.toml")).unwrap();
        assert!(matches!(
            builtin().with(&entries(&format!("{ACME}{ACME}"))),
            Err(CatalogError::Duplicate(_))
        ));
        for client in [
            "[[package]]\nid = \"claude\"\nkind = \"utility\"\nprogram = \"claude\"\n",
            "[[package]]\nid = \"acme-agent\"\nkind = \"client\"\nprogram = \"acme-agent\"\n",
        ] {
            assert!(
                matches!(
                    builtin().with(&entries(client)),
                    Err(CatalogError::Client(_))
                ),
                "{client}"
            );
        }
        let shared = "[[package]]\nid = \"acme-grep\"\nkind = \"utility\"\nprogram = \"rg\"\n";
        assert_eq!(
            builtin().with(&entries(shared)).err(),
            Some(CatalogError::SharedProgram {
                program: "rg".parse().unwrap(),
                first: PackageId::RIPGREP,
                second: "acme-grep".parse().unwrap(),
            })
        );
    }

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
