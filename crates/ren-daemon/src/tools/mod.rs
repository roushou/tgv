//! The tool store on disk: carrying out the depot's install jobs and
//! switching toolset generations. Under its root:
//!
//! ```text
//! store/<id>/<version>-<digest>/   one immutable install each
//! staging/<id>-<version>-<digest>/ an install in progress
//! generations/<n>/bin/             links to the store, one per program
//! generations/<n>/toolset.json     the installs generation n holds
//! current -> generations/<n>       switched by one rename
//! ```
//!
//! Every change is a rename of a finished folder or link, so a crash leaves
//! either the old state or the new one, plus leftovers that the next install
//! or collection removes. One daemon owns its store.

mod archive;
pub mod fetch;
mod toolbox;

pub use toolbox::HostToolbox;

use archive::Download;
use fetch::{ByteLimit, Fetch};
use ren_depot::{
    Depot,
    job::{HostFailure, InstallJob, JobAction, JobEvent, JobState},
    package::PackageId,
    source::{Artifact, Platform, RelativePath, Sha256},
    toolset::{Change, Edit, Generation, Install, Plan, PlanError, Toolset},
};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::{
    collections::{BTreeSet, VecDeque},
    fs, io,
    path::{Path, PathBuf},
};

/// Why the tool store could not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum ToolStoreError {
    #[error("cannot {action}")]
    Io {
        action: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("generation {0} is unreadable")]
    Corrupt(Generation),
    #[error("{0} is not in the store; install it first")]
    NotInstalled(PackageId),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("toolsets need symbolic links, which this platform lacks")]
    Unsupported,
    #[error("another change to the tool store is in progress")]
    Busy,
    #[error("generation {0} is not kept")]
    NoGeneration(Generation),
}

impl ToolStoreError {
    fn io(action: &'static str) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io { action, source }
    }
}

/// `generations/<n>/toolset.json`.
#[derive(Debug, Serialize, Deserialize)]
struct StoredToolset {
    generation: Generation,
    installs: Vec<Install>,
}

/// How a change to the toolset ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Changed {
    /// The current generation already held what was wanted.
    Unchanged(Toolset),
    /// Every install succeeded, and the next generation is current.
    Activated {
        toolset: Toolset,
        changes: Vec<Change>,
    },
    /// An install did not succeed; the current generation stays.
    Failed {
        current: Toolset,
        jobs: Vec<(Install, JobState)>,
    },
}

/// Held while a change or collection writes the store; the store's lock
/// file is unlocked when it drops, or when the process ends.
#[derive(Debug)]
struct WriteLock(#[expect(dead_code, reason = "held for its lock")] fs::File);

/// What a collection removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collected {
    pub generations: Vec<Generation>,
    pub installs: Vec<RelativePath>,
}

/// The installs and toolset generations under one folder.
#[derive(Debug, Clone)]
pub struct ToolStore<F> {
    root: PathBuf,
    fetch: F,
}

impl ToolStore<()> {
    /// The folder under the data directory the store lives in.
    pub const FOLDER: &str = "tools";

    /// A store that cannot download: it reads generations and activates
    /// plans whose installs are already in the store.
    pub fn without_downloads(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            fetch: (),
        }
    }
}

impl<F> ToolStore<F> {
    fn installed_at(&self, dir: &RelativePath) -> PathBuf {
        dir.parts()
            .fold(self.root.join("store"), |path, part| path.join(part))
    }

    fn generation_dir(&self, generation: Generation) -> PathBuf {
        self.root.join("generations").join(generation.to_string())
    }

    /// Whether `install` is in the store.
    pub fn has(&self, install: &Install) -> bool {
        self.installed_at(&install.dir()).is_dir()
    }

    /// The folder of links a run with `generation` puts first on its path.
    pub fn bin(&self, generation: Generation) -> PathBuf {
        self.generation_dir(generation).join("bin")
    }

    /// The current generation; generation 0, empty, before any.
    pub fn current(&self) -> Result<Toolset, ToolStoreError> {
        match fs::read_link(self.root.join("current")) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Toolset::empty()),
            Err(error) => Err(ToolStoreError::io("read the current generation")(error)),
            Ok(target) => {
                let generation = target
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.parse().ok())
                    .ok_or(ToolStoreError::Corrupt(Generation::NONE))?;
                self.generation(generation)
            }
        }
    }

    /// A generation as it was activated.
    pub fn generation(&self, generation: Generation) -> Result<Toolset, ToolStoreError> {
        let text = fs::read(self.generation_dir(generation).join("toolset.json"))
            .map_err(|_| ToolStoreError::Corrupt(generation))?;
        let stored: StoredToolset =
            serde_json::from_slice(&text).map_err(|_| ToolStoreError::Corrupt(generation))?;
        if stored.generation != generation {
            return Err(ToolStoreError::Corrupt(generation));
        }
        Ok(Toolset::restore(stored.generation, stored.installs))
    }

    const LOCK_TRIES: u32 = 100;
    const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(10);

    /// The store's write lock, or `Busy` when another writer holds it.
    fn lock(&self) -> Result<WriteLock, ToolStoreError> {
        fs::create_dir_all(&self.root).map_err(ToolStoreError::io("make the tool store"))?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(".lock"))
            .map_err(ToolStoreError::io("open the tool store's lock"))?;
        // A process started at the same moment can hold a copy of a just
        // closed lock until it runs its program, so a held lock is tried
        // again for a second before it counts as another writer's.
        for _ in 0..Self::LOCK_TRIES {
            match file.try_lock() {
                Ok(()) => return Ok(WriteLock(file)),
                Err(fs::TryLockError::WouldBlock) => std::thread::sleep(Self::LOCK_WAIT),
                Err(fs::TryLockError::Error(error)) => {
                    return Err(ToolStoreError::io("lock the tool store")(error));
                }
            }
        }
        Err(ToolStoreError::Busy)
    }

    /// Every generation kept, newest first. One that cannot be read is
    /// left out.
    pub fn generations(&self) -> Result<Vec<Toolset>, ToolStoreError> {
        let mut generations: Vec<Toolset> = Self::entries(&self.root.join("generations"))?
            .into_iter()
            .filter_map(|(name, _)| name.parse::<Generation>().ok())
            .filter_map(|generation| self.generation(generation).ok())
            .collect();
        generations.sort_by_key(|toolset| std::cmp::Reverse(toolset.generation()));
        Ok(generations)
    }

    /// Make `plan`'s toolset the current generation. Every install it adds
    /// must be in the store; a plan made from an older generation is
    /// refused.
    #[cfg(test)]
    pub(crate) fn activate(&self, plan: Plan) -> Result<Toolset, ToolStoreError> {
        let _lock = self.lock()?;
        self.activate_locked(plan)
    }

    fn activate_locked(&self, plan: Plan) -> Result<Toolset, ToolStoreError> {
        if let Some((missing, _)) = plan
            .fetches()
            .iter()
            .find(|(install, _)| !self.has(install))
        {
            return Err(ToolStoreError::NotInstalled(missing.package.clone()));
        }
        let current = self.current()?;
        let next = plan.apply(&current)?;
        let generations = self.root.join("generations");
        let partial = generations.join(format!(".{}.partial", next.generation()));
        let dir = self.generation_dir(next.generation());
        // Leftovers of an activation that did not finish; never current,
        // which is older.
        for leftover in [&partial, &dir] {
            let _ = fs::remove_dir_all(leftover);
        }
        fs::create_dir_all(partial.join("bin")).map_err(ToolStoreError::io("make a generation"))?;
        for link in next.links() {
            let target = link
                .target
                .parts()
                .fold(PathBuf::from("../../../store"), |path, part| {
                    path.join(part)
                });
            Self::link(&target, &partial.join("bin").join(link.name.as_str()))?;
        }
        let stored = StoredToolset {
            generation: next.generation(),
            installs: next.installs().cloned().collect(),
        };
        let json = serde_json::to_vec_pretty(&stored).map_err(|error| ToolStoreError::Io {
            action: "write a generation",
            source: io::Error::other(error),
        })?;
        fs::write(partial.join("toolset.json"), json)
            .map_err(ToolStoreError::io("write a generation"))?;
        fs::rename(&partial, &dir).map_err(ToolStoreError::io("make a generation"))?;
        let switching = self.root.join(".current.partial");
        let _ = fs::remove_file(&switching);
        Self::link(
            &Path::new("generations").join(next.generation().to_string()),
            &switching,
        )?;
        fs::rename(&switching, self.root.join("current"))
            .map_err(ToolStoreError::io("switch the current generation"))?;
        Ok(next)
    }

    /// Remove every generation except the current one and those in `live`
    /// (generations runs still use), and every install none of them holds.
    pub fn collect(&self, live: &[Generation]) -> Result<Collected, ToolStoreError> {
        let _lock = self.lock()?;
        let current = self.current()?;
        let mut kept = vec![current.clone()];
        for generation in live {
            if *generation != current.generation() && *generation != Generation::NONE {
                kept.push(self.generation(*generation)?);
            }
        }
        let keep: BTreeSet<Generation> = kept.iter().map(Toolset::generation).collect();
        let retained = Toolset::retained(&kept.iter().collect::<Vec<_>>());
        let mut collected = Collected::default();

        for (name, path) in Self::entries(&self.root.join("generations"))? {
            match name.parse::<Generation>() {
                Ok(generation) if keep.contains(&generation) => continue,
                Ok(generation) => collected.generations.push(generation),
                Err(_) => {}
            }
            fs::remove_dir_all(path).map_err(ToolStoreError::io("remove a generation"))?;
        }
        for (package, package_dir) in Self::entries(&self.root.join("store"))? {
            for (version, path) in Self::entries(&package_dir)? {
                let dir: Option<RelativePath> = format!("{package}/{version}").parse().ok();
                if dir.as_ref().is_some_and(|dir| retained.contains(dir)) {
                    continue;
                }
                fs::remove_dir_all(&path).map_err(ToolStoreError::io("remove an install"))?;
                collected.installs.extend(dir);
            }
            // Only succeeds once the package has no installs left.
            let _ = fs::remove_dir(&package_dir);
        }
        collected.generations.sort();
        collected.installs.sort();
        Ok(collected)
    }

    /// The entries of `dir` by name; none when it does not exist.
    fn entries(dir: &Path) -> Result<Vec<(String, PathBuf)>, ToolStoreError> {
        let read = match fs::read_dir(dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            other => other.map_err(ToolStoreError::io("list the tool store"))?,
        };
        let mut entries = Vec::new();
        for entry in read {
            let entry = entry.map_err(ToolStoreError::io("list the tool store"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            entries.push((name, entry.path()));
        }
        Ok(entries)
    }

    #[cfg(unix)]
    fn link(target: &Path, at: &Path) -> Result<(), ToolStoreError> {
        std::os::unix::fs::symlink(target, at).map_err(ToolStoreError::io("make a link"))
    }

    #[cfg(not(unix))]
    fn link(_target: &Path, _at: &Path) -> Result<(), ToolStoreError> {
        Err(ToolStoreError::Unsupported)
    }
}

impl<F: Fetch> ToolStore<F> {
    /// The most a release's download may be.
    pub const DOWNLOAD_LIMIT: ByteLimit = ByteLimit::new(512 * 1024 * 1024);

    pub fn new(root: impl Into<PathBuf>, fetch: F) -> Self {
        Self {
            root: root.into(),
            fetch,
        }
    }

    /// Apply `edit` to the current generation: plan, install what the plan
    /// adds, and activate the next generation only if every install
    /// succeeded. The store is locked throughout, so the edit applies to
    /// the generation that is current then.
    pub async fn change(
        &self,
        depot: &Depot,
        platform: Platform,
        edit: &Edit,
    ) -> Result<Changed, ToolStoreError> {
        let _lock = self.lock()?;
        let current = self.current()?;
        let wanted = current.wanted_after(edit)?;
        let plan = current.plan(depot, platform, &wanted)?;
        if plan.is_empty() {
            return Ok(Changed::Unchanged(current));
        }
        let jobs = self.install_plan(&plan).await;
        if jobs.iter().any(|(_, state)| *state != JobState::Installed) {
            return Ok(Changed::Failed { current, jobs });
        }
        let changes = plan.changes().to_vec();
        let toolset = self.activate_locked(plan)?;
        Ok(Changed::Activated { toolset, changes })
    }

    /// Make the next generation hold exactly what `generation` held, from
    /// installs still in the store; nothing is downloaded. The store is
    /// locked throughout.
    pub fn roll_back(&self, generation: Generation) -> Result<Changed, ToolStoreError> {
        let _lock = self.lock()?;
        let current = self.current()?;
        if generation == Generation::NONE || !self.generation_dir(generation).is_dir() {
            return Err(ToolStoreError::NoGeneration(generation));
        }
        let older = self.generation(generation)?;
        if let Some(missing) = older.installs().find(|install| !self.has(install)) {
            return Err(ToolStoreError::NotInstalled(missing.package.clone()));
        }
        let plan = current.back_to(&older);
        if plan.is_empty() {
            return Ok(Changed::Unchanged(current));
        }
        let changes = plan.changes().to_vec();
        let toolset = self.activate_locked(plan)?;
        Ok(Changed::Activated { toolset, changes })
    }

    /// Install every release `plan` adds that the store lacks, one at a
    /// time, and say how each ended.
    pub(crate) async fn install_plan(&self, plan: &Plan) -> Vec<(Install, JobState)> {
        let mut ended = Vec::new();
        for (install, artifact) in plan.fetches() {
            let state = self.install(install, artifact).await;
            ended.push((install.clone(), state));
        }
        ended
    }

    /// Install `install` from `artifact`, unless the store has it.
    async fn install(&self, install: &Install, artifact: &Artifact) -> JobState {
        if self.has(install) {
            return JobState::Installed;
        }
        let dir = install.dir();
        let staging = self
            .root
            .join("staging")
            .join(dir.parts().collect::<Vec<_>>().join("-"));
        // A leftover from an install that did not finish.
        let _ = fs::remove_dir_all(&staging);
        let (mut job, actions) = InstallJob::start(install.clone(), artifact.clone());
        let mut queue: VecDeque<JobAction> = actions.into();
        if fs::create_dir_all(&staging).is_err() {
            queue = VecDeque::new();
            if let Ok(next) = job.step(JobEvent::HostFailed(HostFailure::Filesystem)) {
                queue.extend(next);
            }
        }
        while let Some(action) = queue.pop_front() {
            let Some(event) = self.carry_out(action, &staging).await else {
                continue;
            };
            match job.step(event) {
                Ok(next) => queue.extend(next),
                Err(error) => {
                    tracing::warn!(%error, "an install job refused a step");
                    break;
                }
            }
        }
        let _ = fs::remove_dir_all(&staging);
        job.state().clone()
    }

    async fn carry_out(&self, action: JobAction, staging: &Path) -> Option<JobEvent> {
        let download = staging.join("download");
        let unpacked = staging.join("unpacked");
        match action {
            JobAction::Fetch { url } => Some(
                match self
                    .fetch
                    .fetch(&url, &download, Self::DOWNLOAD_LIMIT)
                    .await
                {
                    Ok(()) => match Self::digest(download).await {
                        Ok(sha256) => JobEvent::Fetched { sha256 },
                        Err(failure) => JobEvent::HostFailed(failure),
                    },
                    Err(failure) => JobEvent::HostFailed(failure),
                },
            ),
            JobAction::Unpack { format, program_at } => Some(
                match tokio::task::spawn_blocking(move || {
                    Download {
                        path: &download,
                        format,
                    }
                    .unpack(&unpacked, &program_at)
                })
                .await
                {
                    Ok(Ok(check)) => JobEvent::Unpacked(check),
                    Ok(Err(failure)) => JobEvent::HostFailed(failure),
                    Err(_) => JobEvent::HostFailed(HostFailure::Filesystem),
                },
            ),
            JobAction::Place { dir } => {
                let into = self.installed_at(&dir);
                let placed = into
                    .parent()
                    .is_some_and(|parent| fs::create_dir_all(parent).is_ok())
                    && (fs::rename(&unpacked, &into).is_ok() || into.is_dir());
                Some(if placed {
                    JobEvent::Placed
                } else {
                    JobEvent::HostFailed(HostFailure::Filesystem)
                })
            }
            JobAction::Discard => {
                let _ = fs::remove_dir_all(staging);
                None
            }
        }
    }

    async fn digest(path: PathBuf) -> Result<Sha256, HostFailure> {
        tokio::task::spawn_blocking(move || {
            use io::Read as _;
            let mut file = fs::File::open(path)?;
            let mut hasher = sha2::Sha256::new();
            let mut buffer = vec![0; 64 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                let Some(chunk) = buffer.get(..read).filter(|chunk| !chunk.is_empty()) else {
                    break;
                };
                hasher.update(chunk);
            }
            Ok::<_, io::Error>(Sha256::from_bytes(hasher.finalize().into()))
        })
        .await
        .map_err(|_| HostFailure::Filesystem)?
        .map_err(|_| HostFailure::Filesystem)
    }
}

#[cfg(test)]
mod tests;
