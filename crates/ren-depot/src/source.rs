//! Where a package comes from: the system it is adopted from, or a release
//! archive per platform, pinned by its SHA-256.

use serde_with::{DeserializeFromStr, SerializeDisplay};
use std::{fmt, str::FromStr};
use strum::{Display, EnumString, IntoStaticStr, VariantArray};

/// A platform Ren installs packages for.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum Platform {
    LinuxX64,
    LinuxArm64,
    MacosArm64,
}

impl Platform {
    /// The platform an operating system and architecture name, as Rust
    /// names them (`linux`, `x86_64`), or none Ren installs for.
    pub fn of(os: &str, arch: &str) -> Option<Self> {
        match (os, arch) {
            ("linux", "x86_64") => Some(Self::LinuxX64),
            ("linux", "aarch64") => Some(Self::LinuxArm64),
            ("macos", "aarch64") => Some(Self::MacosArm64),
            _ => None,
        }
    }
}

/// How a release artifact is packed.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum ArchiveFormat {
    TarGz,
    Zip,
    /// The download is the program itself.
    Bare,
}

/// Why a source's text was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    #[error("a SHA-256 is 64 hexadecimal digits")]
    Sha256,
    #[error("an artifact URL is https, at most 2048 bytes, with no spaces")]
    Url,
    #[error("a relative path names folders and a file with /, never . or ..")]
    RelativePath,
}

/// A SHA-256 digest.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, SerializeDisplay, DeserializeFromStr,
)]
pub struct Sha256([u8; 32]);

impl Sha256 {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The first `n` hexadecimal digits, for names; all 64 at most.
    pub fn prefix(&self, n: usize) -> String {
        let mut hex = self.to_string();
        hex.truncate(n);
        hex
    }

    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
}

impl FromStr for Sha256 {
    type Err = SourceError;

    fn from_str(hex: &str) -> Result<Self, Self::Err> {
        let bytes = hex.as_bytes();
        if bytes.len() != 64 {
            return Err(SourceError::Sha256);
        }
        let mut digest = [0; 32];
        for (out, pair) in digest.iter_mut().zip(bytes.as_chunks::<2>().0) {
            let [high, low] = *pair;
            let high = Self::digit(high).ok_or(SourceError::Sha256)?;
            let low = Self::digit(low).ok_or(SourceError::Sha256)?;
            *out = high << 4 | low;
        }
        Ok(Self(digest))
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256({self})")
    }
}

/// An https URL a release artifact is downloaded from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub struct ArtifactUrl(String);

impl ArtifactUrl {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ArtifactUrl {
    type Err = SourceError;

    fn from_str(url: &str) -> Result<Self, Self::Err> {
        let host = url.strip_prefix("https://").ok_or(SourceError::Url)?;
        if url.len() > 2048
            || host.is_empty()
            || host.starts_with('/')
            || url.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(SourceError::Url);
        }
        Ok(Self(url.to_owned()))
    }
}

impl fmt::Display for ArtifactUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A path inside an install, `/`-separated, that cannot leave it: no empty,
/// `.`, or `..` parts, no leading `/`, and no `\`.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, SerializeDisplay, DeserializeFromStr,
)]
pub struct RelativePath(String);

impl RelativePath {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Its parts, in order.
    pub fn parts(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// A path built in this crate from parts that are each known to be
    /// valid; callers state why.
    pub(crate) fn trusted(path: String) -> Self {
        Self(path)
    }

    /// `self` followed by `other`.
    pub fn join(&self, other: &Self) -> Self {
        Self(format!("{}/{}", self.0, other.0))
    }
}

impl FromStr for RelativePath {
    type Err = SourceError;

    fn from_str(path: &str) -> Result<Self, Self::Err> {
        let valid = !path.is_empty()
            && !path.contains(['\\', '\0'])
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..");
        if valid {
            Ok(Self(path.to_owned()))
        } else {
            Err(SourceError::RelativePath)
        }
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One platform's download of a release.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub platform: Platform,
    pub url: ArtifactUrl,
    pub sha256: Sha256,
    pub format: ArchiveFormat,
    /// Where the program is once the artifact is unpacked.
    pub program_at: RelativePath,
}

/// A version of a package as its publisher released it, one artifact per
/// platform it is built for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: semver::Version,
    pub artifacts: Vec<Artifact>,
}

impl Release {
    /// The artifact for `platform`.
    pub fn artifact(&self, platform: Platform) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.platform == platform)
    }
}

/// Where a package comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Only adopted from the host's search path; Ren does not install it
    /// (a client that updates itself, or one without a pinned release yet).
    System,
    /// Installed by Ren from a pinned release.
    Release(Release),
}

#[cfg(test)]
mod tests {
    use super::{ArtifactUrl, Platform, RelativePath, Sha256};

    const DIGEST: &str = "9f2c0e1d3b4a5968778695a4b3c2d1e0f0e1d2c3b4a5968778695a4b3c2da41e";

    #[test]
    fn sha256_reads_64_hex_digits_and_prints_them_lowercase() {
        let digest: Sha256 = DIGEST.to_uppercase().parse().unwrap();
        assert_eq!(digest.to_string(), DIGEST);
        assert_eq!(digest.prefix(16), &DIGEST[..16]);
        assert!(DIGEST[..63].parse::<Sha256>().is_err());
        assert!(format!("{}g", &DIGEST[..63]).parse::<Sha256>().is_err());
    }

    #[test]
    fn artifact_urls_are_https() {
        assert!(
            "https://nodejs.org/dist/v22.17.1/node.tar.xz"
                .parse::<ArtifactUrl>()
                .is_ok()
        );
        for bad in [
            "http://nodejs.org/x",
            "https://",
            "https:///x",
            "https://a b",
            "file:///etc/passwd",
        ] {
            assert!(bad.parse::<ArtifactUrl>().is_err(), "{bad}");
        }
    }

    #[test]
    fn relative_paths_cannot_leave_their_folder() {
        let path: RelativePath = "node-v22/bin/node".parse().unwrap();
        assert_eq!(
            path.parts().collect::<Vec<_>>(),
            ["node-v22", "bin", "node"]
        );
        for bad in [
            "",
            "/bin/node",
            "bin//node",
            "../node",
            "bin/./node",
            "bin/",
            "a\\b",
        ] {
            assert!(bad.parse::<RelativePath>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn platforms_are_named_as_rust_names_them() {
        assert_eq!(Platform::of("linux", "x86_64"), Some(Platform::LinuxX64));
        assert_eq!(Platform::of("macos", "aarch64"), Some(Platform::MacosArm64));
        assert_eq!(Platform::of("macos", "x86_64"), None);
        assert_eq!(Platform::of("windows", "x86_64"), None);
    }
}
