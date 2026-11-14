//! The daemon's [`Toolbox`]: the control API's tools, over this host's tool
//! store, the built-in catalog, and its search path. The CLI uses it too,
//! so `ren tools` and `/control/v1/tools` answer alike.

use super::{Changed, ToolStore, ToolStoreError, fetch::Fetch};
use crate::host::HostTools;
use futures_util::{FutureExt as _, future::BoxFuture};
use ren_depot::{
    Depot, Surveyed,
    job::{Failure, JobState, Stage},
    provision::Provision,
    source::{Platform, Source},
    toolset::{Change, Edit, Generation, Toolset},
    version::ObservedVersion,
};
use ren_gateway::tools::{
    ChangeKind, ChangeOutcome, GenerationInstall, GenerationState, GenerationView, InstallFailure,
    InstalledTool, PackageChange, SystemTool, ToolChange, ToolCollection, ToolListing, ToolPackage,
    Toolbox, ToolboxError,
};
use ren_harness::Harness;
use ren_protocol::label::Labeled as _;
use ren_store::Reader;
use std::sync::Arc;

/// The tools of this host.
pub struct HostToolbox<F> {
    inner: Arc<Inner<F>>,
}

struct Inner<F> {
    store: ToolStore<F>,
    depot: Depot,
    host: HostTools,
    platform: Option<Platform>,
    /// Which generations runs use; without one, none are known to.
    reader: Option<Reader>,
}

impl<F> Clone for HostToolbox<F> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<F: Fetch + 'static> HostToolbox<F> {
    pub fn new(
        store: ToolStore<F>,
        depot: Depot,
        host: HostTools,
        platform: Option<Platform>,
        reader: Option<Reader>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                depot,
                host,
                platform,
                reader,
            }),
        }
    }

    /// The listing; reading versions runs each program found, so it runs
    /// on a blocking thread.
    pub async fn read_listing(&self) -> Result<ToolListing, ToolboxError> {
        let inner = self.inner.clone();
        let live = self.live().await?;
        tokio::task::spawn_blocking(move || inner.listing(&live))
            .await
            .map_err(|error| ToolboxError::Failed(error.to_string()))?
    }

    pub async fn apply(&self, edit: Edit) -> Result<ToolChange, ToolboxError> {
        let platform = self.inner.platform.ok_or(ToolboxError::Unsupported)?;
        let changed = self
            .inner
            .store
            .change(&self.inner.depot, platform, &edit)
            .await
            .map_err(Inner::<F>::failed)?;
        Ok(Inner::<F>::change_view(changed))
    }

    pub async fn go_back(&self, generation: Generation) -> Result<ToolChange, ToolboxError> {
        let inner = self.inner.clone();
        let changed = tokio::task::spawn_blocking(move || inner.store.roll_back(generation))
            .await
            .map_err(|error| ToolboxError::Failed(error.to_string()))?
            .map_err(|error| match error {
                ToolStoreError::NotInstalled(_) => ToolboxError::Collected(generation),
                other @ (ToolStoreError::Io { .. }
                | ToolStoreError::Corrupt(_)
                | ToolStoreError::Plan(_)
                | ToolStoreError::Unsupported
                | ToolStoreError::Busy
                | ToolStoreError::NoGeneration(_)) => Inner::<F>::failed(other),
            })?;
        Ok(Inner::<F>::change_view(changed))
    }

    pub async fn sweep(&self) -> Result<ToolCollection, ToolboxError> {
        let live = self.live().await?;
        let collected = self
            .inner
            .store
            .collect(&live)
            .map_err(Inner::<F>::failed)?;
        Ok(ToolCollection {
            generations: collected.generations,
            installs: collected.installs.iter().map(ToString::to_string).collect(),
        })
    }

    async fn live(&self) -> Result<Vec<Generation>, ToolboxError> {
        match &self.inner.reader {
            None => Ok(Vec::new()),
            Some(reader) => reader
                .live_toolsets()
                .await
                .map_err(|error| ToolboxError::Failed(error.to_string())),
        }
    }
}

impl<F> Inner<F> {
    fn failed(error: ToolStoreError) -> ToolboxError {
        match error {
            ToolStoreError::Busy => ToolboxError::Busy,
            ToolStoreError::Plan(error) => ToolboxError::Refused(error),
            ToolStoreError::Unsupported => ToolboxError::Unsupported,
            ToolStoreError::NoGeneration(generation) => ToolboxError::NoGeneration(generation),
            error @ (ToolStoreError::Io { .. }
            | ToolStoreError::Corrupt(_)
            | ToolStoreError::NotInstalled(_)) => ToolboxError::Failed(error.to_string()),
        }
    }

    fn listing(&self, live: &[Generation]) -> Result<ToolListing, ToolboxError> {
        let current = self.store.current().map_err(Self::failed)?;
        let survey = self.depot.survey(&self.host);
        let packages = survey
            .into_iter()
            .map(|Surveyed { package, provision }| ToolPackage {
                id: package.id().clone(),
                kind: package.kind(),
                program: package.program().clone(),
                release: match package.source() {
                    Source::System => None,
                    Source::Release(release) => Some(release.version.clone()),
                },
                installed: current.install(package.id()).map(|install| InstalledTool {
                    version: install.version.clone(),
                    sha256: install.sha256.to_string(),
                    dir: install.dir().to_string(),
                }),
                system: match provision {
                    Provision::Missing => None,
                    Provision::System(found) => Some(match found.version {
                        ObservedVersion::Read(version) => SystemTool {
                            path: found.path.display().to_string(),
                            version: Some(version),
                            unread: None,
                        },
                        ObservedVersion::Unparsed(raw) => SystemTool {
                            path: found.path.display().to_string(),
                            version: None,
                            unread: Some(raw.to_string()),
                        },
                        ObservedVersion::Unread(failure) => SystemTool {
                            path: found.path.display().to_string(),
                            version: None,
                            unread: Some(failure.to_string()),
                        },
                    }),
                },
                needed_by: Harness::ALL
                    .iter()
                    .copied()
                    .filter(|harness| {
                        harness
                            .requirements()
                            .iter()
                            .any(|requirement| requirement.package() == package.id())
                    })
                    .collect(),
            })
            .collect();
        let generations = self
            .store
            .generations()
            .map_err(Self::failed)?
            .iter()
            .map(|toolset| Self::generation_view(toolset, &current, live))
            .collect();
        Ok(ToolListing {
            platform: self.platform,
            generation: current.generation(),
            bin: (current.generation() != Generation::NONE)
                .then(|| self.store.bin(current.generation()).display().to_string()),
            packages,
            generations,
        })
    }

    fn generation_view(
        toolset: &Toolset,
        current: &Toolset,
        live: &[Generation],
    ) -> GenerationView {
        let state = if toolset.generation() == current.generation() {
            GenerationState::Current
        } else if live.contains(&toolset.generation()) {
            GenerationState::InUse
        } else {
            GenerationState::Idle
        };
        GenerationView {
            generation: toolset.generation(),
            state,
            installs: toolset
                .installs()
                .map(|install| GenerationInstall {
                    package: install.package.clone(),
                    version: install.version.clone(),
                })
                .collect(),
        }
    }

    fn change_view(changed: Changed) -> ToolChange {
        match changed {
            Changed::Unchanged(current) => ToolChange {
                outcome: ChangeOutcome::Unchanged,
                generation: current.generation(),
                changes: Vec::new(),
                failures: Vec::new(),
            },
            Changed::Activated { toolset, changes } => ToolChange {
                outcome: ChangeOutcome::Activated,
                generation: toolset.generation(),
                changes: changes
                    .into_iter()
                    .map(|change| match change {
                        Change::Add(install) => PackageChange {
                            change: ChangeKind::Add,
                            package: install.package,
                            from: None,
                            to: Some(install.version),
                        },
                        Change::Replace { from, to } => PackageChange {
                            change: ChangeKind::Replace,
                            package: to.package,
                            from: Some(from.version),
                            to: Some(to.version),
                        },
                        Change::Remove(install) => PackageChange {
                            change: ChangeKind::Remove,
                            package: install.package,
                            from: Some(install.version),
                            to: None,
                        },
                    })
                    .collect(),
                failures: Vec::new(),
            },
            Changed::Failed { current, jobs } => ToolChange {
                outcome: ChangeOutcome::Failed,
                generation: current.generation(),
                changes: Vec::new(),
                failures: jobs
                    .into_iter()
                    .filter_map(|(install, state)| {
                        let (stage, reason) = match state {
                            JobState::Installed => return None,
                            JobState::Failed { stage, failure } => (stage, Self::describe(failure)),
                            JobState::Interrupted { stage } => (stage, "interrupted".to_owned()),
                            JobState::Fetching => (Stage::Fetch, "stopped unfinished".to_owned()),
                            JobState::Unpacking => (Stage::Unpack, "stopped unfinished".to_owned()),
                            JobState::Placing => (Stage::Place, "stopped unfinished".to_owned()),
                        };
                        Some(InstallFailure {
                            package: install.package,
                            version: install.version,
                            stage,
                            reason,
                        })
                    })
                    .collect(),
            },
        }
    }

    /// A failure in words, for the operator.
    fn describe(failure: Failure) -> String {
        match failure {
            Failure::Mismatch { actual } => {
                format!("the download's SHA-256 is {actual}, not the one the catalog pins")
            }
            Failure::ProgramMissing => "the archive has no program where the catalog says".into(),
            Failure::Host(failure) => failure.to_string().replace('_', " "),
        }
    }
}

impl<F: Fetch + 'static> Toolbox for HostToolbox<F> {
    fn listing(&self) -> BoxFuture<'static, Result<ToolListing, ToolboxError>> {
        let toolbox = self.clone();
        async move { toolbox.read_listing().await }.boxed()
    }

    fn change(&self, edit: Edit) -> BoxFuture<'static, Result<ToolChange, ToolboxError>> {
        let toolbox = self.clone();
        async move { toolbox.apply(edit).await }.boxed()
    }

    fn roll_back(
        &self,
        generation: Generation,
    ) -> BoxFuture<'static, Result<ToolChange, ToolboxError>> {
        let toolbox = self.clone();
        async move { toolbox.go_back(generation).await }.boxed()
    }

    fn collect(&self) -> BoxFuture<'static, Result<ToolCollection, ToolboxError>> {
        let toolbox = self.clone();
        async move { toolbox.sweep().await }.boxed()
    }
}
