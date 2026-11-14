//! What a package is: its identity, the program it provides, and how its
//! version is read. A requirement names a package and the versions that fit.

use serde_with::{DeserializeFromStr, SerializeDisplay};
use std::{borrow::Cow, fmt, str::FromStr};
use strum::{Display, EnumString, IntoStaticStr, VariantArray};

/// A package's name in the catalog: lowercase ASCII letters, digits, and
/// inner hyphens, at most 64 bytes (`node`, `pi-extension`).
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, SerializeDisplay, DeserializeFromStr,
)]
pub struct PackageId(Cow<'static, str>);

/// Why text is not a [`PackageId`] or a [`ProgramName`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("a package name is 1 to 64 lowercase letters, digits, and inner hyphens")]
    Package,
    #[error("a program name is a file name: not empty, no path separator, not . or ..")]
    Program,
}

impl PackageId {
    pub const CLAUDE: Self = Self::known("claude");
    pub const CODEX: Self = Self::known("codex");
    pub const PI: Self = Self::known("pi");
    pub const OPENCODE: Self = Self::known("opencode");
    pub const NODE: Self = Self::known("node");
    pub const BUN: Self = Self::known("bun");
    pub const GIT: Self = Self::known("git");
    pub const JQ: Self = Self::known("jq");
    pub const FD: Self = Self::known("fd");

    /// A catalog name written in this crate's source; the catalog's tests
    /// check every one parses.
    const fn known(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn valid(name: &str) -> bool {
        (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && !name.starts_with('-')
            && !name.ends_with('-')
    }
}

impl FromStr for PackageId {
    type Err = NameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        if Self::valid(name) {
            Ok(Self(Cow::Owned(name.to_owned())))
        } else {
            Err(NameError::Package)
        }
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The file name of a program, as it is looked up on a search path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub struct ProgramName(Cow<'static, str>);

impl ProgramName {
    const fn known(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn valid(name: &str) -> bool {
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
    }
}

impl FromStr for ProgramName {
    type Err = NameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        if Self::valid(name) {
            Ok(Self(Cow::Owned(name.to_owned())))
        } else {
            Err(NameError::Program)
        }
    }
}

impl fmt::Display for ProgramName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a package is for, which groups it for the operator.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum Kind {
    /// A coding agent Ren drives.
    Client,
    /// A language runtime other programs run on.
    Runtime,
    /// A command-line tool agents and Ren use.
    Utility,
}

/// A package the catalog knows: the program it provides and how its version
/// is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    id: PackageId,
    kind: Kind,
    program: ProgramName,
    version_args: &'static [&'static str],
}

impl Package {
    /// A package whose version is printed by `program --version`.
    pub(crate) const fn versioned(id: PackageId, kind: Kind, program: &'static str) -> Self {
        Self {
            id,
            kind,
            program: ProgramName::known(program),
            version_args: &["--version"],
        }
    }

    pub fn id(&self) -> &PackageId {
        &self.id
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn program(&self) -> &ProgramName {
        &self.program
    }

    /// The arguments that make the program print its version.
    pub fn version_args(&self) -> &'static [&'static str] {
        self.version_args
    }
}

/// A package something needs, and the versions that serve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    package: PackageId,
    wants: semver::VersionReq,
}

impl Requirement {
    /// `package`, at any version.
    pub fn any(package: PackageId) -> Self {
        Self {
            package,
            wants: semver::VersionReq::STAR,
        }
    }

    /// `package`, at a version `wants` matches.
    pub fn of(package: PackageId, wants: semver::VersionReq) -> Self {
        Self { package, wants }
    }

    pub fn package(&self) -> &PackageId {
        &self.package
    }

    pub fn wants(&self) -> &semver::VersionReq {
        &self.wants
    }
}

#[cfg(test)]
mod tests {
    use super::{PackageId, ProgramName};

    #[test]
    fn package_names_are_lowercase_words_joined_by_hyphens() {
        for good in ["node", "pi-extension", "x", "ripgrep14"] {
            assert_eq!(good.parse::<PackageId>().unwrap().as_str(), good);
        }
        for bad in [
            "",
            "Node",
            "-node",
            "node-",
            "no de",
            "node/x",
            &"a".repeat(65),
        ] {
            assert!(bad.parse::<PackageId>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn program_names_are_file_names() {
        for good in ["node", "rg", "node.exe"] {
            assert_eq!(good.parse::<ProgramName>().unwrap().as_str(), good);
        }
        for bad in ["", ".", "..", "bin/node", "a\\b", "a\0b"] {
            assert!(bad.parse::<ProgramName>().is_err(), "{bad:?}");
        }
    }
}
