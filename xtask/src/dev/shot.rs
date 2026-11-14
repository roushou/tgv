//! `cargo xtask dev shot`: screenshots of the dev UI's pages with a headless
//! Chromium, for checking a UI change without a browser. Each page loads
//! for a few seconds, then is captured; the live feed never holds it open.

use super::DevError;
use crate::{
    install::InstallError,
    invocation::{Echo, Invocation},
};
use clap::ValueEnum;
use ren_harness::host::HostEnv;
use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

/// A page of the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Page {
    Overview,
    Live,
    Traffic,
    Recordings,
    /// The newest recording, with its turns.
    Recording,
    Usage,
    Search,
    /// The newest run's session, with its conversation.
    Sessions,
    /// The tools Ren installs for its agents.
    Tools,
}

/// The newest recording and run, which some pages show.
#[derive(Debug, Default)]
struct Newest {
    recording: Option<String>,
    run: Option<String>,
}

impl Page {
    pub const ALL: [Self; 9] = [
        Self::Overview,
        Self::Live,
        Self::Traffic,
        Self::Recordings,
        Self::Recording,
        Self::Usage,
        Self::Search,
        Self::Sessions,
        Self::Tools,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Live => "live",
            Self::Traffic => "traffic",
            Self::Recordings => "recordings",
            Self::Recording => "recording",
            Self::Usage => "usage",
            Self::Search => "search",
            Self::Sessions => "sessions",
            Self::Tools => "tools",
        }
    }

    /// The page's path: the newest recording for `Recording`, the newest
    /// run's session (if any) for `Sessions`.
    fn path(self, newest: &Newest) -> Option<String> {
        match self {
            Self::Overview => Some("/".into()),
            Self::Live => Some("/live".into()),
            Self::Traffic => Some("/traffic".into()),
            Self::Recordings => Some("/recordings".into()),
            Self::Recording => newest
                .recording
                .as_ref()
                .map(|id| format!("/recordings/{id}")),
            Self::Usage => Some("/usage".into()),
            Self::Search => Some("/search?q=retry".into()),
            Self::Sessions => Some(
                newest
                    .run
                    .as_ref()
                    .map_or_else(|| "/sessions".into(), |id| format!("/sessions?run={id}")),
            ),
            Self::Tools => Some("/tools".into()),
        }
    }
}

/// Takes screenshots of one UI.
pub struct Camera {
    chrome: PathBuf,
    ui: SocketAddr,
    gateway: SocketAddr,
    out: PathBuf,
}

impl Camera {
    /// How long a page loads before it is captured, in milliseconds.
    const LOAD_MS: &'static str = "4000";

    /// A headless Chromium: `REN_CHROME`, else `chrome-headless-shell` or
    /// `chromium` on PATH, else Playwright's downloaded headless shell.
    pub fn new(
        env: &HostEnv,
        ui: SocketAddr,
        gateway: SocketAddr,
        out: PathBuf,
    ) -> Result<Self, DevError> {
        let chrome = env
            .absolute_path("REN_CHROME")
            .or_else(|| Self::on_path(env))
            .or_else(|| Self::playwright(env))
            .ok_or(DevError::NoChrome)?;
        Ok(Self {
            chrome,
            ui,
            gateway,
            out,
        })
    }

    fn on_path(env: &HostEnv) -> Option<PathBuf> {
        let path = env.get("PATH")?;
        ["chrome-headless-shell", "chromium", "chromium-browser"]
            .iter()
            .flat_map(|name| path.split(':').map(move |dir| Path::new(dir).join(name)))
            .find(|candidate| candidate.is_absolute() && candidate.is_file())
    }

    fn playwright(env: &HostEnv) -> Option<PathBuf> {
        let cache = env.absolute_path("HOME")?.join(".cache/ms-playwright");
        let mut shells: Vec<PathBuf> = fs::read_dir(&cache)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|dir| {
                dir.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("chromium_headless_shell-"))
            })
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("chrome-headless-shell"))
            .filter(|binary| binary.is_file())
            .collect();
        shells.sort();
        shells.pop()
    }

    /// The newest item's id in a listing of the dev gateway's API, such as
    /// `sessions` or `runs`.
    fn newest(&self, listing: &str) -> Option<String> {
        let answer = Invocation::new("curl")
            .args(["--silent", "--fail", "--max-time", "5"])
            .arg(format!("http://{}/api/{listing}?limit=1", self.gateway))
            .capture()
            .ok()?;
        let answer: serde_json::Value = serde_json::from_str(&answer).ok()?;
        answer[listing][0]["id"].as_str().map(str::to_owned)
    }

    /// Capture `pages` at `width`×`height`; returns the files written.
    pub fn shoot(
        &self,
        pages: &[Page],
        (width, height): (u16, u16),
    ) -> Result<Vec<PathBuf>, DevError> {
        fs::create_dir_all(&self.out).map_err(InstallError::file(&self.out))?;
        let newest = Newest {
            // The read API still lists recordings as `sessions`.
            recording: self.newest("sessions"),
            run: self.newest("runs"),
        };
        let mut written = Vec::new();
        for page in pages {
            let Some(path) = page.path(&newest) else {
                println!("skipped {}: no recording yet", page.name());
                continue;
            };
            let file = self.out.join(format!("{}.png", page.name()));
            // `timeout` bounds a page that would never settle.
            let taken = Invocation::new("timeout")
                .arg("60")
                .arg(&self.chrome)
                .args([
                    "--no-sandbox",
                    "--hide-scrollbars",
                    &format!("--window-size={width},{height}"),
                    &format!("--timeout={}", Self::LOAD_MS),
                ])
                .arg(format!("--screenshot={}", file.display()))
                .arg(format!("http://{}{path}", self.ui))
                .succeeds(Echo::Hide)?;
            if taken && file.is_file() {
                println!("{}", file.display());
                written.push(file);
            } else {
                println!("failed {}", page.name());
            }
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::{Newest, Page};

    #[test]
    fn every_page_has_a_path_and_the_recording_needs_one() {
        let none = Newest::default();
        let some = Newest {
            recording: Some("s1".into()),
            run: Some("r1".into()),
        };
        for page in Page::ALL {
            match page {
                Page::Recording => {
                    assert_eq!(page.path(&none), None);
                    assert_eq!(page.path(&some).as_deref(), Some("/recordings/s1"));
                }
                Page::Sessions => {
                    assert_eq!(page.path(&none).as_deref(), Some("/sessions"));
                    assert_eq!(page.path(&some).as_deref(), Some("/sessions?run=r1"));
                }
                Page::Overview
                | Page::Live
                | Page::Traffic
                | Page::Recordings
                | Page::Usage
                | Page::Search
                | Page::Tools => assert!(page.path(&none).is_some_and(|p| p.starts_with('/'))),
            }
        }
    }
}
