//! A program's version as its own output states it, read without trusting
//! any one format: `v22.17.1`, `jq-1.7.1`, `codex-cli 0.61.2`,
//! `2.3.0 (Claude Code)`, `git version 2.43.0`.

use std::fmt;

/// What a version probe learned. A version that could not be read is kept
/// as such, never taken for one that fits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedVersion {
    /// The program printed a version Ren reads.
    Read(semver::Version),
    /// The program answered, but with no version Ren reads; its first line.
    Unparsed(RawVersion),
    /// The program did not answer.
    Unread(ProbeFailure),
}

/// The first line of a probe's output, kept verbatim and cut to 200 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawVersion(String);

impl RawVersion {
    const LIMIT: usize = 200;

    pub(crate) fn of(output: &str) -> Self {
        let line = output
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("");
        let mut end = line.len().min(Self::LIMIT);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        Self(line[..end].to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RawVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a program did not answer its version probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProbeFailure {
    #[error("it could not be started")]
    NotStarted,
    #[error("it did not answer in time")]
    TimedOut,
    #[error("it exited unsuccessfully")]
    Failed,
}

impl ObservedVersion {
    /// The version in a probe's output: the first `MAJOR.MINOR[.PATCH]`,
    /// with an optional `-PRERELEASE`, that starts a word or follows a `v`
    /// that does. Two parts are read as `MAJOR.MINOR.0`.
    pub fn read(output: &str) -> Self {
        output
            .lines()
            .find_map(Self::in_line)
            .map_or_else(|| Self::Unparsed(RawVersion::of(output)), Self::Read)
    }

    fn in_line(line: &str) -> Option<semver::Version> {
        let bytes = line.as_bytes();
        (0..bytes.len()).find_map(|start| {
            Self::starts_word(bytes, start)
                .then(|| Self::at(&line[start..]))
                .flatten()
        })
    }

    /// Whether a version may start at `start`: a digit at the start of a
    /// word, or after a `v` that starts one.
    fn starts_word(bytes: &[u8], start: usize) -> bool {
        let word_start = |at: usize| {
            at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'.')
        };
        bytes[start].is_ascii_digit()
            && (word_start(start) || (bytes[start - 1] == b'v' && word_start(start - 1)))
    }

    fn at(text: &str) -> Option<semver::Version> {
        let numbers: Vec<&str> = text
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .next()?
            .split('.')
            .take(3)
            .collect();
        if numbers.len() < 2 || numbers.iter().any(|part| part.is_empty()) {
            return None;
        }
        let core = numbers.join(".");
        let rest = &text[core.len()..];
        let pre = rest
            .strip_prefix('-')
            .map(|rest| {
                rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(['.', '-'])
            })
            .filter(|pre| !pre.is_empty());
        let padded = if numbers.len() == 2 {
            format!("{core}.0")
        } else {
            core
        };
        let full = match pre {
            Some(pre) => format!("{padded}-{pre}"),
            None => padded,
        };
        semver::Version::parse(&full)
            .ok()
            .or_else(|| semver::Version::parse(full.split('-').next().unwrap_or("")).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::{ObservedVersion, RawVersion};

    fn read(output: &str) -> String {
        match ObservedVersion::read(output) {
            ObservedVersion::Read(version) => version.to_string(),
            other @ (ObservedVersion::Unparsed(_) | ObservedVersion::Unread(_)) => {
                panic!("{output:?} read as {other:?}")
            }
        }
    }

    #[test]
    fn reads_the_versions_tools_print() {
        assert_eq!(read("v22.17.1\n"), "22.17.1");
        assert_eq!(read("jq-1.7.1"), "1.7.1");
        assert_eq!(read("jq-1.7"), "1.7.0");
        assert_eq!(read("fd 10.2.0"), "10.2.0");
        assert_eq!(read("1.2.19"), "1.2.19");
        assert_eq!(read("codex-cli 0.61.2"), "0.61.2");
        assert_eq!(read("2.3.0 (Claude Code)"), "2.3.0");
        assert_eq!(read("git version 2.43.0"), "2.43.0");
        assert_eq!(read("bun 1.3.0-canary.12+abc"), "1.3.0-canary.12");
        assert_eq!(read("\n\ntool x86_64 build\n4.5.6\n"), "4.5.6");
    }

    #[test]
    fn digits_inside_a_word_are_not_a_version() {
        assert_eq!(
            ObservedVersion::read("x86_64 rev2.3"),
            ObservedVersion::Unparsed(RawVersion("x86_64 rev2.3".into()))
        );
    }

    #[test]
    fn keeps_what_it_cannot_read() {
        assert_eq!(
            ObservedVersion::read("\n  unknown build  \nmore"),
            ObservedVersion::Unparsed(RawVersion("unknown build".into()))
        );
        let long = "é".repeat(150);
        match ObservedVersion::read(&long) {
            ObservedVersion::Unparsed(raw) => assert!(raw.as_str().len() <= 200),
            other @ (ObservedVersion::Read(_) | ObservedVersion::Unread(_)) => panic!("{other:?}"),
        }
    }
}
