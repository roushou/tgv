//! What a host provides for a package, and whether that serves a
//! requirement.

use crate::version::ObservedVersion;
use serde_with::{DeserializeFromStr, SerializeDisplay};
use std::path::PathBuf;
use strum::{Display, EnumString, IntoStaticStr, VariantArray};

/// What the host provides for a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provision {
    /// Its program is not on the search path.
    Missing,
    /// Its program is on the search path, installed by something other than
    /// Ren.
    System(Found),
}

/// A program found on the host, where, and the version it reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub version: ObservedVersion,
}

/// Whether what the host provides serves a requirement.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum Fit {
    /// Its version is one the requirement accepts.
    Fits,
    /// Its version is one the requirement does not accept.
    Outside,
    /// It is there, but its version could not be read, so it may or may not
    /// fit; it is never taken to fit.
    Unverified,
    /// It is not there.
    Missing,
}

impl Provision {
    /// How this serves a requirement for `wants`. Any version that is there
    /// serves `*`, even one that could not be read.
    pub fn fit(&self, wants: &semver::VersionReq) -> Fit {
        match self {
            Self::Missing => Fit::Missing,
            Self::System(found) => match &found.version {
                ObservedVersion::Read(version) if wants.matches(version) => Fit::Fits,
                ObservedVersion::Read(_) => Fit::Outside,
                ObservedVersion::Unparsed(_) | ObservedVersion::Unread(_)
                    if *wants == semver::VersionReq::STAR =>
                {
                    Fit::Fits
                }
                ObservedVersion::Unparsed(_) | ObservedVersion::Unread(_) => Fit::Unverified,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Fit, Found, Provision};
    use crate::version::{ObservedVersion, ProbeFailure};

    fn found(version: ObservedVersion) -> Provision {
        Provision::System(Found {
            path: "/usr/bin/node".into(),
            version,
        })
    }

    #[test]
    fn a_read_version_fits_or_falls_outside() {
        let node = found(ObservedVersion::Read("22.17.1".parse().unwrap()));
        assert_eq!(node.fit(&"^22".parse().unwrap()), Fit::Fits);
        assert_eq!(node.fit(&">=24".parse().unwrap()), Fit::Outside);
        assert_eq!(node.fit(&semver::VersionReq::STAR), Fit::Fits);
    }

    #[test]
    fn an_unread_version_is_never_taken_to_fit_a_bound() {
        let node = found(ObservedVersion::Unread(ProbeFailure::TimedOut));
        assert_eq!(node.fit(&"^22".parse().unwrap()), Fit::Unverified);
        assert_eq!(node.fit(&semver::VersionReq::STAR), Fit::Fits);
        assert_eq!(
            Provision::Missing.fit(&semver::VersionReq::STAR),
            Fit::Missing
        );
    }
}
