//! What Ren has installed and what a run is given: immutable installs, each
//! in its own content-addressed folder, gathered into numbered toolset
//! generations. A change is planned as the next generation; the one a run
//! started with stays whole until no run uses it.

use crate::{
    Depot,
    package::{PackageId, ProgramName},
    source::{Artifact, Platform, RelativePath, Sha256, Source},
};
use serde_with::{DeserializeFromStr, SerializeDisplay};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    num::ParseIntError,
    str::FromStr,
};

/// One package's release, unpacked into a folder of its own that never
/// changes: `<id>/<version>-<first 16 digits of its SHA-256>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Install {
    pub package: PackageId,
    pub version: semver::Version,
    pub sha256: Sha256,
    /// The program it provides, and where it is inside the folder.
    pub program: ProgramName,
    pub program_at: RelativePath,
}

impl Install {
    fn of(
        package: PackageId,
        program: ProgramName,
        version: semver::Version,
        artifact: &Artifact,
    ) -> Self {
        Self {
            package,
            version,
            sha256: artifact.sha256,
            program,
            program_at: artifact.program_at.clone(),
        }
    }

    /// Its folder, relative to the store's root.
    pub fn dir(&self) -> RelativePath {
        // Each part is a package name, or a version and hex digits: never
        // empty, `.`, or `..`, and never holding a `/`.
        RelativePath::trusted(format!(
            "{}/{}-{}",
            self.package,
            self.version,
            self.sha256.prefix(16)
        ))
    }

    /// Its program, relative to the store's root.
    pub fn program_path(&self) -> RelativePath {
        self.dir().join(&self.program_at)
    }
}

/// A toolset's number. Each change makes the next one; numbers never repeat.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, SerializeDisplay, DeserializeFromStr,
)]
pub struct Generation(u64);

impl Generation {
    /// The generation before anything is installed.
    pub const NONE: Self = Self(0);

    pub fn number(self) -> u64 {
        self.0
    }

    fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl From<u64> for Generation {
    fn from(number: u64) -> Self {
        Self(number)
    }
}

impl FromStr for Generation {
    type Err = ParseIntError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse().map(Self)
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The installs one generation gives a run, at most one per package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolset {
    generation: Generation,
    installs: BTreeMap<PackageId, Install>,
}

/// A program a toolset puts on a run's search path: its name in the
/// toolset's `bin` folder, and the program in the store it leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub name: ProgramName,
    pub target: RelativePath,
}

/// What the operator asks to change: packages to add (or bring to the
/// catalog's release) and packages to remove, applied to whichever
/// generation is current when the change is made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit {
    pub add: BTreeSet<PackageId>,
    pub remove: BTreeSet<PackageId>,
}

/// One package's change between a toolset and the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Add(Install),
    Replace { from: Install, to: Install },
    Remove(Install),
}

/// The changes that turn a toolset into what the operator wants. Each new
/// install also names the artifact to fetch for it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct Plan {
    from: Generation,
    changes: Vec<Change>,
    fetches: Vec<(Install, Artifact)>,
}

/// Why a plan could not be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("no package is named {0}")]
    Unknown(PackageId),
    #[error("{0} is only adopted from the system; Ren does not install it")]
    SystemOnly(PackageId),
    #[error("{package} has no release for {platform}")]
    NoArtifact {
        package: PackageId,
        platform: Platform,
    },
    #[error("{0} is not installed by Ren")]
    NotInstalled(PackageId),
    #[error("{0} is both added and removed")]
    Conflicting(PackageId),
    #[error("the plan was made from generation {planned}, but generation {current} is current")]
    Stale {
        planned: Generation,
        current: Generation,
    },
}

impl Toolset {
    /// Generation 0: nothing installed.
    pub fn empty() -> Self {
        Self {
            generation: Generation::NONE,
            installs: BTreeMap::new(),
        }
    }

    /// A generation as it was stored.
    pub fn restore(generation: Generation, installs: impl IntoIterator<Item = Install>) -> Self {
        Self {
            generation,
            installs: installs
                .into_iter()
                .map(|install| (install.package.clone(), install))
                .collect(),
        }
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn installs(&self) -> impl Iterator<Item = &Install> {
        self.installs.values()
    }

    /// The install of `package`, if this toolset has one.
    pub fn install(&self, package: &PackageId) -> Option<&Install> {
        self.installs.get(package)
    }

    /// The programs this toolset puts on a run's search path.
    pub fn links(&self) -> Vec<Link> {
        self.installs
            .values()
            .map(|install| Link {
                name: install.program.clone(),
                target: install.program_path(),
            })
            .collect()
    }

    /// The packages this toolset holds after `edit`. Removing a package it
    /// does not hold, or adding and removing one, is refused.
    pub fn wanted_after(&self, edit: &Edit) -> Result<BTreeSet<PackageId>, PlanError> {
        if let Some(both) = edit.add.intersection(&edit.remove).next() {
            return Err(PlanError::Conflicting(both.clone()));
        }
        let mut wanted: BTreeSet<PackageId> = self.installs.keys().cloned().collect();
        for package in &edit.remove {
            if !wanted.remove(package) {
                return Err(PlanError::NotInstalled(package.clone()));
            }
        }
        wanted.extend(edit.add.iter().cloned());
        Ok(wanted)
    }

    /// The changes that make this toolset hold exactly `wanted`, each at the
    /// release the catalog pins, for `platform`.
    pub fn plan(
        &self,
        depot: &Depot,
        platform: Platform,
        wanted: &BTreeSet<PackageId>,
    ) -> Result<Plan, PlanError> {
        let mut changes = Vec::new();
        let mut fetches = Vec::new();
        for id in wanted {
            let package = depot
                .package(id)
                .ok_or_else(|| PlanError::Unknown(id.clone()))?;
            let release = match package.source() {
                Source::System => return Err(PlanError::SystemOnly(id.clone())),
                Source::Release(release) => release,
            };
            let artifact = release
                .artifact(platform)
                .ok_or_else(|| PlanError::NoArtifact {
                    package: id.clone(),
                    platform,
                })?;
            let target = Install::of(
                id.clone(),
                package.program().clone(),
                release.version.clone(),
                artifact,
            );
            match self.installs.get(id) {
                Some(current) if *current == target => continue,
                Some(current) => changes.push(Change::Replace {
                    from: current.clone(),
                    to: target.clone(),
                }),
                None => changes.push(Change::Add(target.clone())),
            }
            fetches.push((target, artifact.clone()));
        }
        changes.extend(
            self.installs
                .iter()
                .filter(|(id, _)| !wanted.contains(*id))
                .map(|(_, install)| Change::Remove(install.clone())),
        );
        Ok(Plan {
            from: self.generation,
            changes,
            fetches,
        })
    }

    /// The install folders the toolsets in `live` hold: the current
    /// generation and every generation a live run started with. Collection
    /// keeps these and removes every other folder in the store.
    pub fn retained(live: &[&Toolset]) -> BTreeSet<RelativePath> {
        live.iter()
            .flat_map(|toolset| toolset.installs())
            .map(Install::dir)
            .collect()
    }
}

impl Plan {
    /// Whether nothing would change.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// The installs this plan adds, each with the artifact to fetch for it.
    /// The store may already hold some of them, from an earlier generation.
    pub fn fetches(&self) -> &[(Install, Artifact)] {
        &self.fetches
    }

    /// The next generation: `current` with this plan's changes, once every
    /// install it adds is in the store. A plan made from another generation
    /// is refused, since `current` changed under it.
    pub fn apply(self, current: &Toolset) -> Result<Toolset, PlanError> {
        if current.generation != self.from {
            return Err(PlanError::Stale {
                planned: self.from,
                current: current.generation,
            });
        }
        let mut installs = current.installs.clone();
        for change in self.changes {
            match change {
                Change::Add(install) | Change::Replace { to: install, .. } => {
                    installs.insert(install.package.clone(), install);
                }
                Change::Remove(install) => {
                    installs.remove(&install.package);
                }
            }
        }
        Ok(Toolset {
            generation: current.generation.next(),
            installs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Change, Edit, Generation, PlanError, Toolset};
    use crate::{
        Depot,
        package::{Kind, Package, PackageId},
        source::{ArchiveFormat, Artifact, Platform, Release, Source},
    };
    use std::collections::BTreeSet;

    fn release(version: &str, digest: char) -> Source {
        Source::Release(Release {
            version: version.parse().unwrap(),
            artifacts: vec![Artifact {
                platform: Platform::LinuxX64,
                url: format!("https://example.test/{version}.tar.gz")
                    .parse()
                    .unwrap(),
                sha256: digest.to_string().repeat(64).parse().unwrap(),
                format: ArchiveFormat::TarGz,
                program_at: "bin/tool".parse().unwrap(),
            }],
        })
    }

    fn depot(node: &str, digest: char) -> Depot {
        Depot::of(vec![
            Package::new(
                PackageId::NODE,
                Kind::Runtime,
                "node".parse().unwrap(),
                Source::System,
            )
            .sourced(release(node, digest)),
            Package::new(
                PackageId::JQ,
                Kind::Utility,
                "jq".parse().unwrap(),
                Source::System,
            )
            .sourced(release("1.7.1", 'b')),
            Package::new(
                PackageId::CODEX,
                Kind::Client,
                "codex".parse().unwrap(),
                Source::System,
            ),
        ])
    }

    fn wanted(ids: &[PackageId]) -> BTreeSet<PackageId> {
        ids.iter().cloned().collect()
    }

    #[test]
    fn a_plan_adds_replaces_and_removes_and_applies_as_the_next_generation() {
        let empty = Toolset::empty();
        let first = empty
            .plan(
                &depot("22.17.1", 'a'),
                Platform::LinuxX64,
                &wanted(&[PackageId::NODE, PackageId::JQ]),
            )
            .unwrap();
        assert_eq!(first.fetches().len(), 2);
        let one = first.apply(&empty).unwrap();
        assert_eq!(one.generation(), "1".parse::<Generation>().unwrap());
        let node = one.install(&PackageId::NODE).unwrap();
        assert_eq!(node.dir().as_str(), "node/22.17.1-aaaaaaaaaaaaaaaa");
        assert_eq!(
            one.links()
                .iter()
                .map(|link| link.target.as_str())
                .collect::<Vec<_>>(),
            [
                "jq/1.7.1-bbbbbbbbbbbbbbbb/bin/tool",
                "node/22.17.1-aaaaaaaaaaaaaaaa/bin/tool"
            ]
        );

        // Nothing to do when the toolset already holds what is wanted.
        let same = one
            .plan(
                &depot("22.17.1", 'a'),
                Platform::LinuxX64,
                &wanted(&[PackageId::NODE, PackageId::JQ]),
            )
            .unwrap();
        assert!(same.is_empty());
        assert!(same.fetches().is_empty());

        // A new node release replaces it; jq, no longer wanted, goes.
        let second = one
            .plan(
                &depot("24.1.0", 'c'),
                Platform::LinuxX64,
                &wanted(&[PackageId::NODE]),
            )
            .unwrap();
        assert!(matches!(
            second.changes(),
            [Change::Replace { from, to }, Change::Remove(jq)]
                if from.version.to_string() == "22.17.1"
                    && to.version.to_string() == "24.1.0"
                    && jq.package == PackageId::JQ
        ));
        let two = second.apply(&one).unwrap();
        assert_eq!(two.generation().number(), 2);
        assert!(two.install(&PackageId::JQ).is_none());
    }

    #[test]
    fn an_edit_adds_to_and_removes_from_what_is_held() {
        let empty = Toolset::empty();
        let depot = depot("22.17.1", 'a');
        let one = empty
            .plan(&depot, Platform::LinuxX64, &wanted(&[PackageId::NODE]))
            .unwrap()
            .apply(&empty)
            .unwrap();
        let edit = |add: &[PackageId], remove: &[PackageId]| Edit {
            add: wanted(add),
            remove: wanted(remove),
        };
        assert_eq!(
            one.wanted_after(&edit(&[PackageId::JQ], &[])),
            Ok(wanted(&[PackageId::NODE, PackageId::JQ]))
        );
        assert_eq!(
            one.wanted_after(&edit(&[PackageId::JQ], &[PackageId::NODE])),
            Ok(wanted(&[PackageId::JQ]))
        );
        assert_eq!(
            one.wanted_after(&edit(&[], &[PackageId::JQ])),
            Err(PlanError::NotInstalled(PackageId::JQ))
        );
        assert_eq!(
            one.wanted_after(&edit(&[PackageId::JQ], &[PackageId::JQ])),
            Err(PlanError::Conflicting(PackageId::JQ))
        );
    }

    #[test]
    fn a_plan_made_from_an_older_generation_is_refused() {
        let empty = Toolset::empty();
        let depot = depot("22.17.1", 'a');
        let plan = |toolset: &Toolset| {
            toolset
                .plan(&depot, Platform::LinuxX64, &wanted(&[PackageId::NODE]))
                .unwrap()
        };
        let stale = plan(&empty);
        let one = plan(&empty).apply(&empty).unwrap();
        assert_eq!(
            stale.apply(&one),
            Err(PlanError::Stale {
                planned: Generation::NONE,
                current: one.generation(),
            })
        );
    }

    #[test]
    fn a_plan_names_what_cannot_be_installed() {
        let empty = Toolset::empty();
        let depot = depot("22.17.1", 'a');
        assert_eq!(
            empty.plan(&depot, Platform::LinuxX64, &wanted(&[PackageId::CODEX])),
            Err(PlanError::SystemOnly(PackageId::CODEX))
        );
        assert_eq!(
            empty.plan(&depot, Platform::MacosArm64, &wanted(&[PackageId::NODE])),
            Err(PlanError::NoArtifact {
                package: PackageId::NODE,
                platform: Platform::MacosArm64,
            })
        );
        assert_eq!(
            empty.plan(&depot, Platform::LinuxX64, &wanted(&[PackageId::FD])),
            Err(PlanError::Unknown(PackageId::FD))
        );
    }

    #[test]
    fn collection_retains_every_install_a_live_generation_holds() {
        let empty = Toolset::empty();
        let one = empty
            .plan(
                &depot("22.17.1", 'a'),
                Platform::LinuxX64,
                &wanted(&[PackageId::NODE, PackageId::JQ]),
            )
            .unwrap()
            .apply(&empty)
            .unwrap();
        let two = one
            .plan(
                &depot("24.1.0", 'c'),
                Platform::LinuxX64,
                &wanted(&[PackageId::NODE]),
            )
            .unwrap()
            .apply(&one)
            .unwrap();
        let dirs = |live: &[&Toolset]| -> Vec<String> {
            Toolset::retained(live)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        // A run still uses generation 1: its node and jq stay.
        assert_eq!(
            dirs(&[&two, &one]),
            [
                "jq/1.7.1-bbbbbbbbbbbbbbbb",
                "node/22.17.1-aaaaaaaaaaaaaaaa",
                "node/24.1.0-cccccccccccccccc"
            ]
        );
        // Once it ends, only generation 2's node stays.
        assert_eq!(dirs(&[&two]), ["node/24.1.0-cccccccccccccccc"]);
    }
}
