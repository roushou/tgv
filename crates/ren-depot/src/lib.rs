//! The tools Ren's agents run with: the packages Ren knows (the clients it
//! drives, the runtimes they run on, and the utilities they use), what each
//! client requires, and what a host provides for each.
//!
//! The primary type is [`Depot`]. It is sans-IO: it reaches the host only
//! through the [`ToolHost`] port, which the daemon implements. Today it
//! observes the host's search path; installing packages is planned
//! (docs/plans/dependencies.md).

pub mod job;
pub mod package;
pub mod provision;
pub mod source;
pub mod toolset;
pub mod version;

#[cfg(test)]
mod purity;

use package::{Kind, Package, PackageId, ProgramName};
use provision::{Found, Provision};
use std::path::{Path, PathBuf};
use version::{ObservedVersion, ProbeFailure};

/// The host capability the depot needs: finding a program on the search
/// path the host's agents inherit, and running it to read its version.
pub trait ToolHost {
    /// Where `program` is on the search path, if it is there and executable.
    fn locate(&self, program: &ProgramName) -> Option<PathBuf>;

    /// Run the program at `path` with `args` and return what it printed to
    /// standard output, within a short time limit.
    fn probe(&self, path: &Path, args: &[&str]) -> Result<String, ProbeFailure>;
}

/// The packages Ren knows, and what a host provides for them.
#[derive(Debug, Clone)]
pub struct Depot {
    catalog: Vec<Package>,
}

/// One package, and what the host provides for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Surveyed {
    pub package: Package,
    pub provision: Provision,
}

impl Depot {
    /// The catalog built into this Ren: the four clients, the runtimes and
    /// the utilities they and Ren use.
    pub fn builtin() -> Self {
        Self {
            catalog: vec![
                Package::versioned(PackageId::CLAUDE, Kind::Client, "claude"),
                Package::versioned(PackageId::CODEX, Kind::Client, "codex"),
                Package::versioned(PackageId::PI, Kind::Client, "pi"),
                Package::versioned(PackageId::OPENCODE, Kind::Client, "opencode"),
                Package::versioned(PackageId::NODE, Kind::Runtime, "node"),
                Package::versioned(PackageId::BUN, Kind::Runtime, "bun"),
                Package::versioned(PackageId::GIT, Kind::Utility, "git"),
                Package::versioned(PackageId::JQ, Kind::Utility, "jq"),
                Package::versioned(PackageId::FD, Kind::Utility, "fd"),
            ],
        }
    }

    /// A depot with this catalog.
    #[cfg(test)]
    pub(crate) fn of(catalog: Vec<Package>) -> Self {
        Self { catalog }
    }

    /// Every package, in catalog order.
    pub fn packages(&self) -> &[Package] {
        &self.catalog
    }

    /// The package named `id`.
    pub fn package(&self, id: &PackageId) -> Option<&Package> {
        self.catalog.iter().find(|package| package.id() == id)
    }

    /// What `host` provides for `package`.
    pub fn provision(&self, host: &impl ToolHost, package: &Package) -> Provision {
        match host.locate(package.program()) {
            None => Provision::Missing,
            Some(path) => {
                let version = match host.probe(&path, package.version_args()) {
                    Ok(output) => ObservedVersion::read(&output),
                    Err(failure) => ObservedVersion::Unread(failure),
                };
                Provision::System(Found { path, version })
            }
        }
    }

    /// What `host` provides for every package, in catalog order.
    pub fn survey(&self, host: &impl ToolHost) -> Vec<Surveyed> {
        self.catalog
            .iter()
            .map(|package| Surveyed {
                package: package.clone(),
                provision: self.provision(host, package),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{Depot, ToolHost};
    use crate::{
        package::{PackageId, ProgramName},
        provision::{Found, Provision},
        version::{ObservedVersion, ProbeFailure, RawVersion},
    };
    use std::path::{Path, PathBuf};

    /// A host with `node` that answers, `jq` that prints no version, and
    /// `fd` that hangs.
    struct FakeHost;

    impl ToolHost for FakeHost {
        fn locate(&self, program: &ProgramName) -> Option<PathBuf> {
            ["node", "jq", "fd"]
                .contains(&program.as_str())
                .then(|| PathBuf::from("/usr/bin").join(program.as_str()))
        }

        fn probe(&self, path: &Path, args: &[&str]) -> Result<String, ProbeFailure> {
            assert_eq!(args, ["--version"]);
            match path.to_str() {
                Some("/usr/bin/node") => Ok("v22.17.1\n".into()),
                Some("/usr/bin/jq") => Ok("a build\n".into()),
                _ => Err(ProbeFailure::TimedOut),
            }
        }
    }

    #[test]
    fn every_builtin_name_parses_and_is_unique() {
        let depot = Depot::builtin();
        let mut ids: Vec<&str> = depot.packages().iter().map(|p| p.id().as_str()).collect();
        for package in depot.packages() {
            let id: PackageId = package.id().as_str().parse().unwrap();
            assert_eq!(&id, package.id());
            let program: ProgramName = package.program().as_str().parse().unwrap();
            assert_eq!(&program, package.program());
        }
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    #[test]
    fn surveys_what_the_host_provides_keeping_what_it_cannot_read() {
        let depot = Depot::builtin();
        let survey = depot.survey(&FakeHost);
        let of = |name: &str| {
            survey
                .iter()
                .find(|surveyed| surveyed.package.id().as_str() == name)
                .map(|surveyed| surveyed.provision.clone())
                .unwrap()
        };
        assert_eq!(
            of("node"),
            Provision::System(Found {
                path: "/usr/bin/node".into(),
                version: ObservedVersion::Read("22.17.1".parse().unwrap()),
            })
        );
        assert_eq!(
            of("jq"),
            Provision::System(Found {
                path: "/usr/bin/jq".into(),
                version: ObservedVersion::Unparsed(RawVersion::of("a build")),
            })
        );
        assert_eq!(
            of("fd"),
            Provision::System(Found {
                path: "/usr/bin/fd".into(),
                version: ObservedVersion::Unread(ProbeFailure::TimedOut),
            })
        );
        assert_eq!(of("bun"), Provision::Missing);
        assert_eq!(survey.len(), depot.packages().len());
    }
}
