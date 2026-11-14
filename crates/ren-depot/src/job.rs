//! Installing one release: fetch its artifact, verify its SHA-256, unpack it
//! and find its program, and place it in the store. A pure machine: the
//! shell carries out each [`JobAction`] and reports what happened as a
//! [`JobEvent`]. Every way it can end is explicit, interruption included.

use crate::{
    source::{ArchiveFormat, Artifact, ArtifactUrl, RelativePath, Sha256},
    toolset::Install,
};
use serde_with::{DeserializeFromStr, SerializeDisplay};
use strum::{Display, EnumString, IntoStaticStr, VariantArray};

/// A step of an install.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum Stage {
    Fetch,
    Verify,
    Unpack,
    Place,
}

/// What went wrong on the host while carrying out an action.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum HostFailure {
    /// The download could not be made or did not finish.
    Network,
    /// The server answered with something other than the artifact.
    Refused,
    /// The download was larger than an artifact may be.
    TooLarge,
    /// The archive could not be read, or held a path outside its folder.
    BadArchive,
    /// Writing to the store failed.
    Filesystem,
}

/// Why an install failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The download's SHA-256 is not the one the catalog pins.
    Mismatch {
        actual: Sha256,
    },
    /// The unpacked archive has no program where the catalog says.
    ProgramMissing,
    Host(HostFailure),
}

/// Whether the unpacked archive has its program where the catalog says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramCheck {
    Found,
    Missing,
}

/// Where an install stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Fetching,
    Unpacking,
    Placing,
    /// In the store, ready for a generation to hold.
    Installed,
    Failed {
        stage: Stage,
        failure: Failure,
    },
    /// Stopped by shutdown or the operator during `stage`.
    Interrupted {
        stage: Stage,
    },
}

/// What the shell does next. Every action works on the job's own staging
/// folder until `Place` moves the result into the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobAction {
    /// Download `url` into staging, and report its SHA-256.
    Fetch { url: ArtifactUrl },
    /// Unpack the download, and report whether `program_at` is there.
    Unpack {
        format: ArchiveFormat,
        program_at: RelativePath,
    },
    /// Move the unpacked folder to `dir` in the store, in one rename.
    Place { dir: RelativePath },
    /// Remove the staging folder.
    Discard,
}

/// What the shell reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEvent {
    Fetched { sha256: Sha256 },
    Unpacked(ProgramCheck),
    Placed,
    HostFailed(HostFailure),
    Interrupted,
}

/// An event the job's state cannot take; the state is left as it was.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("an install in state {state:?} cannot take {event:?}")]
pub struct IllegalStep {
    pub state: JobState,
    pub event: JobEvent,
}

/// One release being installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallJob {
    install: Install,
    artifact: Artifact,
    state: JobState,
}

impl InstallJob {
    /// A job for `install` from `artifact`, and its first action.
    pub fn start(install: Install, artifact: Artifact) -> (Self, Vec<JobAction>) {
        let fetch = JobAction::Fetch {
            url: artifact.url.clone(),
        };
        let job = Self {
            install,
            artifact,
            state: JobState::Fetching,
        };
        (job, vec![fetch])
    }

    pub fn install(&self) -> &Install {
        &self.install
    }

    pub fn state(&self) -> &JobState {
        &self.state
    }

    /// The stage an unfinished job is in.
    fn stage(&self) -> Option<Stage> {
        match self.state {
            JobState::Fetching => Some(Stage::Fetch),
            JobState::Unpacking => Some(Stage::Unpack),
            JobState::Placing => Some(Stage::Place),
            JobState::Installed | JobState::Failed { .. } | JobState::Interrupted { .. } => None,
        }
    }

    /// Take `event`, returning what to do next.
    pub fn step(&mut self, event: JobEvent) -> Result<Vec<JobAction>, IllegalStep> {
        let illegal = |state: &JobState, event: JobEvent| IllegalStep {
            state: state.clone(),
            event,
        };
        let Some(stage) = self.stage() else {
            return Err(illegal(&self.state, event));
        };
        let (state, actions) = match (&self.state, event) {
            (_, JobEvent::HostFailed(failure)) => (
                JobState::Failed {
                    stage,
                    failure: Failure::Host(failure),
                },
                vec![JobAction::Discard],
            ),
            (_, JobEvent::Interrupted) => {
                (JobState::Interrupted { stage }, vec![JobAction::Discard])
            }
            (JobState::Fetching, JobEvent::Fetched { sha256 })
                if sha256 == self.artifact.sha256 =>
            {
                (
                    JobState::Unpacking,
                    vec![JobAction::Unpack {
                        format: self.artifact.format,
                        program_at: self.artifact.program_at.clone(),
                    }],
                )
            }
            (JobState::Fetching, JobEvent::Fetched { sha256 }) => (
                JobState::Failed {
                    stage: Stage::Verify,
                    failure: Failure::Mismatch { actual: sha256 },
                },
                vec![JobAction::Discard],
            ),
            (JobState::Unpacking, JobEvent::Unpacked(ProgramCheck::Found)) => (
                JobState::Placing,
                vec![JobAction::Place {
                    dir: self.install.dir(),
                }],
            ),
            (JobState::Unpacking, JobEvent::Unpacked(ProgramCheck::Missing)) => (
                JobState::Failed {
                    stage: Stage::Unpack,
                    failure: Failure::ProgramMissing,
                },
                vec![JobAction::Discard],
            ),
            (JobState::Placing, JobEvent::Placed) => (JobState::Installed, Vec::new()),
            (
                JobState::Fetching
                | JobState::Unpacking
                | JobState::Placing
                | JobState::Installed
                | JobState::Failed { .. }
                | JobState::Interrupted { .. },
                event @ (JobEvent::Fetched { .. } | JobEvent::Unpacked(_) | JobEvent::Placed),
            ) => return Err(illegal(&self.state, event)),
        };
        self.state = state;
        Ok(actions)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Failure, HostFailure, IllegalStep, InstallJob, JobAction, JobEvent, JobState, ProgramCheck,
        Stage,
    };
    use crate::{
        package::PackageId,
        source::{ArchiveFormat, Artifact, Platform, Sha256},
        toolset::Install,
    };

    fn digest(c: char) -> Sha256 {
        c.to_string().repeat(64).parse().unwrap()
    }

    fn job() -> InstallJob {
        let artifact = Artifact {
            platform: Platform::LinuxX64,
            url: "https://example.test/jq.tar.gz".parse().unwrap(),
            sha256: digest('a'),
            format: ArchiveFormat::TarGz,
            program_at: "bin/jq".parse().unwrap(),
        };
        let install = Install {
            package: PackageId::JQ,
            version: "1.7.1".parse().unwrap(),
            sha256: artifact.sha256,
            program: "jq".parse().unwrap(),
            program_at: artifact.program_at.clone(),
        };
        let (job, actions) = InstallJob::start(install, artifact);
        assert_eq!(
            actions,
            [JobAction::Fetch {
                url: "https://example.test/jq.tar.gz".parse().unwrap()
            }]
        );
        job
    }

    #[test]
    fn a_verified_release_is_unpacked_and_placed_in_its_own_folder() {
        let mut job = job();
        assert!(matches!(
            job.step(JobEvent::Fetched { sha256: digest('a') }).unwrap().as_slice(),
            [JobAction::Unpack { format: ArchiveFormat::TarGz, program_at }] if program_at.as_str() == "bin/jq"
        ));
        assert_eq!(
            job.step(JobEvent::Unpacked(ProgramCheck::Found)).unwrap(),
            [JobAction::Place {
                dir: "jq/1.7.1-aaaaaaaaaaaaaaaa".parse().unwrap()
            }]
        );
        assert!(job.step(JobEvent::Placed).unwrap().is_empty());
        assert_eq!(job.state(), &JobState::Installed);
    }

    #[test]
    fn a_download_with_another_digest_fails_verification_and_is_discarded() {
        let mut job = job();
        assert_eq!(
            job.step(JobEvent::Fetched {
                sha256: digest('b')
            })
            .unwrap(),
            [JobAction::Discard]
        );
        assert_eq!(
            job.state(),
            &JobState::Failed {
                stage: Stage::Verify,
                failure: Failure::Mismatch {
                    actual: digest('b')
                },
            }
        );
    }

    #[test]
    fn host_failures_and_interruptions_end_in_the_stage_they_happen() {
        let mut job = job();
        job.step(JobEvent::Fetched {
            sha256: digest('a'),
        })
        .unwrap();
        assert_eq!(
            job.step(JobEvent::HostFailed(HostFailure::BadArchive))
                .unwrap(),
            [JobAction::Discard]
        );
        assert_eq!(
            job.state(),
            &JobState::Failed {
                stage: Stage::Unpack,
                failure: Failure::Host(HostFailure::BadArchive),
            }
        );

        let mut job = super::tests::job();
        assert_eq!(
            job.step(JobEvent::Interrupted).unwrap(),
            [JobAction::Discard]
        );
        assert_eq!(
            job.state(),
            &JobState::Interrupted {
                stage: Stage::Fetch
            }
        );
    }

    #[test]
    fn an_archive_without_its_program_fails() {
        let mut job = job();
        job.step(JobEvent::Fetched {
            sha256: digest('a'),
        })
        .unwrap();
        job.step(JobEvent::Unpacked(ProgramCheck::Missing)).unwrap();
        assert_eq!(
            job.state(),
            &JobState::Failed {
                stage: Stage::Unpack,
                failure: Failure::ProgramMissing,
            }
        );
    }

    #[test]
    fn illegal_steps_are_refused_and_leave_the_state_alone() {
        let mut job = job();
        assert_eq!(
            job.step(JobEvent::Placed),
            Err(IllegalStep {
                state: JobState::Fetching,
                event: JobEvent::Placed,
            })
        );
        assert_eq!(job.state(), &JobState::Fetching);
        job.step(JobEvent::Interrupted).unwrap();
        // An ended job takes nothing, not even another interruption.
        assert!(job.step(JobEvent::Interrupted).is_err());
        assert!(
            job.step(JobEvent::Fetched {
                sha256: digest('a')
            })
            .is_err()
        );
    }
}
