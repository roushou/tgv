//! The host's clocks, randomness, and the observation plane's thread, as the
//! daemon hands them to components.
//!
//! These are the production implementations of the recorder's `Clock` and
//! `IdSource` ports, and the only code that reads the host clocks or mints
//! random identities. Components receive them as values; nothing else calls
//! `SystemTime::now`, `Instant::now`, or `Uuid::new_v4`.

use crate::tools::ToolStore;
use ren_depot::toolset::Generation;
use ren_depot::{ToolHost, package::ProgramName, source::Platform, version::ProbeFailure};
use ren_harness::{launch::LaunchPlan, pi::PiCache};
use ren_recorder::{
    id::IdSource,
    time::{Clock, Moment, Monotonic, UnixMillis},
};
use ren_supervisor::{
    AgentHost, AgentProcess, FoundPath, KillSwitch, Look, PathKind, ProcessExit, RunTools,
    WalkLimits, Workdir,
};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Instant, SystemTime};
use tokio::sync::oneshot;
use uuid::Uuid;

/// The depot's view of this host: programs on a search path, and their
/// versions, read by running them briefly. The search path is passed in
/// (from `HostEnv`), never read here.
#[derive(Debug, Clone)]
pub struct HostTools {
    search_path: Vec<PathBuf>,
}

impl HostTools {
    /// How often, and how many times, a probe checks whether its program
    /// has exited: 60 checks 50 ms apart, about three seconds in all.
    const POLL: std::time::Duration = std::time::Duration::from_millis(50);
    const POLLS: u32 = 60;
    /// The most of a probe's output that is read.
    const OUTPUT_LIMIT: u64 = 64 * 1024;

    pub fn new(search_path: Vec<PathBuf>) -> Self {
        Self { search_path }
    }

    /// The platform this binary was built for, if Ren installs tools there.
    pub fn platform() -> Option<Platform> {
        Platform::of(std::env::consts::OS, std::env::consts::ARCH)
    }

    fn executable(path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|metadata| {
            if !metadata.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
    }

    fn output(child: &mut std::process::Child) -> String {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            let _ = stdout.take(Self::OUTPUT_LIMIT).read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl ToolHost for HostTools {
    fn locate(&self, program: &ProgramName) -> Option<PathBuf> {
        self.search_path.iter().find_map(|dir| {
            #[cfg(windows)]
            let file = dir.join(format!("{program}.exe"));
            #[cfg(not(windows))]
            let file = dir.join(program.as_str());
            Self::executable(&file).then_some(file)
        })
    }

    fn probe(&self, path: &Path, args: &[&str]) -> Result<String, ProbeFailure> {
        let mut command = std::process::Command::new(path);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[expect(
            clippy::disallowed_methods,
            reason = "the host runs a program it found to read its version"
        )]
        let mut child = command.spawn().map_err(|_| ProbeFailure::NotStarted)?;
        for _ in 0..Self::POLLS {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(Self::output(&mut child)),
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(Self::POLL),
            }
        }
        let timed_out = matches!(child.try_wait(), Ok(None));
        let _ = child.kill();
        let _ = child.wait();
        Err(if timed_out {
            ProbeFailure::TimedOut
        } else {
            ProbeFailure::Failed
        })
    }
}

/// The host's clocks. Monotonic time counts from [`SystemClock::start`].
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    #[expect(
        clippy::disallowed_methods,
        reason = "the one sanctioned reader of the host's monotonic clock"
    )]
    pub fn start() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    /// A host clock set before 1970 reads as the epoch: wall time is for
    /// display only, while ordering and durations use monotonic time.
    #[expect(
        clippy::disallowed_methods,
        reason = "the one sanctioned reader of the host's wall clock"
    )]
    fn now(&self) -> Moment {
        Moment {
            wall: UnixMillis::try_from(SystemTime::now()).unwrap_or(UnixMillis::EPOCH),
            monotonic: Monotonic::from_elapsed(self.origin.elapsed()),
        }
    }
}

/// Random version 4 UUIDs.
#[derive(Debug, Default, Clone, Copy)]
pub struct RandomIds;

impl IdSource for RandomIds {
    #[expect(
        clippy::disallowed_methods,
        reason = "the one sanctioned source of random identities"
    )]
    fn next_uuid(&self) -> Uuid {
        Uuid::new_v4()
    }
}

/// The observation plane's own thread and runtime. The recorder runs there,
/// so cutting chunks, inspecting, deriving digests, and writing storage never
/// occupy a worker that forwards traffic, however much work they take.
#[must_use = "the observation task stops when this handle is aborted"]
pub struct ObservationThread<T> {
    output: oneshot::Receiver<T>,
    stop: Option<oneshot::Sender<()>>,
}

impl<T: Send + 'static> ObservationThread<T> {
    /// Run `task` on a current-thread runtime of a new thread.
    pub fn start<F>(task: F) -> io::Result<Self>
    where
        F: Future<Output = T> + Send + 'static,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .thread_name("ren-observation")
            .build()?;
        let (output_sender, output) = oneshot::channel();
        let (stop, stopped) = oneshot::channel::<()>();
        std::thread::Builder::new()
            .name("ren-observation".to_owned())
            .spawn(move || {
                runtime.block_on(async move {
                    tokio::select! {
                        result = task => {
                            let _ = output_sender.send(result);
                        }
                        // Only an explicit stop ends it early; a dropped
                        // handle lets the task finish on its own.
                        Ok(()) = stopped => {}
                    }
                });
            })?;
        Ok(Self {
            output,
            stop: Some(stop),
        })
    }

    /// The task's output, once it finishes. `None` when it stopped without
    /// one: it panicked, or it was stopped.
    pub async fn join(&mut self) -> Option<T> {
        (&mut self.output).await.ok()
    }

    /// Stop the task at its next await point; [`Self::join`] then returns
    /// `None`.
    pub fn abort(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Starts agent processes on this host: the production [`AgentHost`]. Each
/// process works in its run's directory with piped stdin and stdout; stderr
/// is discarded, and a process whose exit future is dropped is killed.
/// Pi loads Ren's extension, published to `pi` before each start.
#[derive(Debug, Clone)]
pub struct ProcessHost {
    pi: Option<PiCache>,
    /// The host's search path, which a toolset's folder goes before.
    search_path: Vec<PathBuf>,
    tools: ToolStore<()>,
}

impl ProcessHost {
    pub fn new(pi: Option<PiCache>, search_path: Vec<PathBuf>, tools: ToolStore<()>) -> Self {
        Self {
            pi,
            search_path,
            tools,
        }
    }

    /// The search path a process with `tools` gets: their folder, then the
    /// host's. `None` leaves the inherited one.
    fn search_path_with(&self, tools: &RunTools) -> io::Result<Option<std::ffi::OsString>> {
        tools
            .bin()
            .map(|bin| {
                std::env::join_paths(
                    std::iter::once(bin.to_path_buf()).chain(self.search_path.iter().cloned()),
                )
                .map_err(io::Error::other)
            })
            .transpose()
    }

    fn start(
        mut command: std::process::Command,
        workdir: &Path,
        stderr: Stderr,
    ) -> io::Result<AgentProcess> {
        command
            .current_dir(workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(match stderr {
                Stderr::Discarded => Stdio::null(),
                Stderr::WithStdout => Stdio::piped(),
            });
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        // A program installed a moment ago can still be open in a process
        // that is being started ("text file busy"): try again briefly.
        let mut tries = 0;
        let mut child = loop {
            #[expect(
                clippy::disallowed_methods,
                reason = "the one sanctioned place that starts agent processes"
            )]
            match command.spawn() {
                Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy && tries < 50 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                other => break other?,
            }
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(io::Error::other("the agent's pipes were not opened"));
        };
        let stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin> = match stderr {
            Stderr::Discarded => Box::new(stdout),
            Stderr::WithStdout => match child.stderr.take() {
                Some(stderr) => Box::new(Merged::new(stdout, stderr)),
                None => return Err(io::Error::other("the agent's stderr was not opened")),
            },
        };
        let (kill, signal) = KillSwitch::pair();
        let exit = async move {
            let status = tokio::select! {
                status = child.wait() => status,
                () = signal.fired() => match child.kill().await {
                    Ok(()) => child.wait().await,
                    Err(error) => Err(error),
                },
            };
            match status {
                Ok(status) => Self::exit(status),
                Err(_) => ProcessExit::Unknown,
            }
        };
        Ok(AgentProcess {
            stdin: Box::new(stdin),
            stdout,
            exit: Box::pin(exit),
            kill,
        })
    }

    fn exit(status: std::process::ExitStatus) -> ProcessExit {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return ProcessExit::Signal(signal);
            }
        }
        status
            .code()
            .map_or(ProcessExit::Unknown, ProcessExit::Code)
    }
}

impl ProcessHost {
    /// Whether `dir` holds a git repository, and its `HEAD`'s text when it
    /// can be read. `.git` is a folder, or, in a linked worktree, a file
    /// naming the repository's folder (`gitdir: <path>`).
    fn git(dir: &Path) -> Option<Option<String>> {
        let dot_git = dir.join(".git");
        let kind = std::fs::symlink_metadata(&dot_git).ok()?.file_type();
        let git_dir = if kind.is_dir() {
            Some(dot_git)
        } else if kind.is_file() {
            Self::small(&dot_git).and_then(|text| {
                let path = text.trim().strip_prefix("gitdir:")?.trim().to_owned();
                Some(dir.join(path))
            })
        } else {
            None
        };
        Some(git_dir.and_then(|git_dir| Self::small(&git_dir.join("HEAD"))))
    }

    /// A small text file's first bytes; `HEAD` is one line.
    fn small(path: &Path) -> Option<String> {
        use std::io::Read as _;
        let mut text = String::new();
        std::fs::File::open(path)
            .ok()?
            .take(1024)
            .read_to_string(&mut text)
            .ok()?;
        Some(text)
    }
}

impl AgentHost for ProcessHost {
    fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn look(&self, dir: &Path) -> io::Result<Look> {
        let mut folders = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Ok(name) = entry.file_name().into_string()
            {
                folders.push(name);
            }
        }
        Ok(Look {
            folders,
            git: Self::git(dir),
        })
    }

    fn files(&self, dir: &Path, limits: WalkLimits) -> io::Result<Vec<FoundPath>> {
        // Hidden files and what git ignores are left out, as an editor's
        // file finder does; links are not followed.
        let walk = ignore::WalkBuilder::new(dir)
            .max_depth(Some(limits.depth))
            .follow_links(false)
            .require_git(false)
            .build();
        let mut found = Vec::new();
        for entry in walk {
            let entry = entry.map_err(io::Error::other)?;
            if entry.depth() == 0 {
                continue;
            }
            let Some(kind) = entry.file_type().map(|kind| {
                if kind.is_dir() {
                    PathKind::Directory
                } else {
                    PathKind::File
                }
            }) else {
                continue;
            };
            let Some(path) = entry
                .path()
                .strip_prefix(dir)
                .ok()
                .and_then(Path::to_str)
                .map(|path| path.replace(std::path::MAIN_SEPARATOR, "/"))
            else {
                continue;
            };
            found.push(FoundPath { path, kind });
            if found.len() >= limits.entries {
                break;
            }
        }
        Ok(found)
    }

    fn finds(&self, program: &str) -> bool {
        // The same search a started process makes: this process's path,
        // which every agent it starts inherits.
        #[expect(
            clippy::disallowed_methods,
            reason = "the host finds a program on the path its agents inherit"
        )]
        let path = std::env::var_os("PATH");
        path.is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
    }

    fn make_dir(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    fn make_scratch(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir(dir)?;
        let mut git = std::process::Command::new("git");
        git.args(["init", "--quiet"])
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = git.status();
        // Without git the folder still works; its changes are not tracked.
        if let Err(error) = status {
            tracing::debug!(%error, "cannot make a scratch folder a repository");
        }
        Ok(())
    }

    fn tools(&self) -> RunTools {
        match self.tools.current() {
            Ok(toolset) if toolset.generation() == Generation::NONE => RunTools::Empty,
            Ok(toolset) => RunTools::Toolset {
                generation: toolset.generation(),
                bin: self.tools.bin(toolset.generation()),
            },
            Err(error) => {
                tracing::warn!(%error, "cannot read the tool store; runs start without its tools");
                RunTools::Unreadable
            }
        }
    }

    fn spawn(
        &self,
        plan: &LaunchPlan,
        workdir: &Workdir,
        tools: &RunTools,
    ) -> io::Result<AgentProcess> {
        let extension = if plan.loads_pi_extension() {
            let cache = self.pi.as_ref().ok_or_else(|| {
                io::Error::other("set XDG_CACHE_HOME or HOME for the Pi extension cache")
            })?;
            Some(cache.materialize().map_err(io::Error::other)?)
        } else {
            None
        };
        let mut command = plan.command(extension.as_deref(), &[]);
        if let Some(path) = self.search_path_with(tools)? {
            command.env("PATH", path);
        }
        Self::start(command, workdir.as_path(), Stderr::Discarded)
    }

    fn spawn_with_stderr(
        &self,
        plan: &LaunchPlan,
        workdir: &Workdir,
        tools: &RunTools,
    ) -> io::Result<AgentProcess> {
        if plan.loads_pi_extension() {
            return Err(io::Error::other(
                "a client's own short commands load no extension",
            ));
        }
        let mut command = plan.command(None, &[]);
        if let Some(path) = self.search_path_with(tools)? {
            command.env("PATH", path);
        }
        Self::start(command, workdir.as_path(), Stderr::WithStdout)
    }
}

/// Where a started process's stderr goes.
#[derive(Debug, Clone, Copy)]
enum Stderr {
    Discarded,
    /// Read with its stdout, as one stream.
    WithStdout,
}

/// Two pipes read as one: whatever either has, in the order it is read,
/// ending once both have ended. Lines from one never split the other's
/// within a read, but a line written in pieces may interleave with the
/// other pipe's.
struct Merged<A, B> {
    first: Option<A>,
    second: Option<B>,
}

impl<A, B> Merged<A, B> {
    fn new(first: A, second: B) -> Self {
        Self {
            first: Some(first),
            second: Some(second),
        }
    }
}

impl<A, B> tokio::io::AsyncRead for Merged<A, B>
where
    A: tokio::io::AsyncRead + Unpin,
    B: tokio::io::AsyncRead + Unpin,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        use std::task::Poll;
        let this = &mut *self;
        let before = buffer.filled().len();
        if let Some(first) = this.first.as_mut() {
            match std::pin::Pin::new(first).poll_read(context, buffer) {
                Poll::Ready(Ok(())) if buffer.filled().len() > before => {
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Ok(())) => this.first = None,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => {}
            }
        }
        if let Some(second) = this.second.as_mut() {
            match std::pin::Pin::new(second).poll_read(context, buffer) {
                Poll::Ready(Ok(())) if buffer.filled().len() > before => {
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Ok(())) => this.second = None,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => {}
            }
        }
        if this.first.is_none() && this.second.is_none() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{HostTools, ProcessHost, RandomIds, SystemClock};
    use crate::tools::ToolStore;
    use ren_depot::{
        Depot, ToolHost,
        package::PackageId,
        provision::{Found, Provision},
        source::Platform,
        toolset::Toolset,
        version::{ObservedVersion, ProbeFailure},
    };
    use ren_supervisor::RunTools;

    #[cfg(unix)]
    fn script(dir: &std::path::Path, name: &str, body: &str, mode: u32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn host_tools_find_executables_on_their_path_and_read_versions() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        // Not executable here, so the next folder's `node` is the one found.
        script(first.path(), "node", "echo v1.0.0", 0o644);
        let node = script(second.path(), "node", "echo v22.17.1", 0o755);
        script(second.path(), "jq", "exit 3", 0o755);
        let tools = HostTools::new(vec![first.path().into(), second.path().into()]);
        let depot = Depot::builtin().unwrap();
        let provision = |id: PackageId| depot.provision(&tools, depot.package(&id).unwrap());
        assert_eq!(
            provision(PackageId::NODE),
            Provision::System(Found {
                path: node,
                version: ObservedVersion::Read("22.17.1".parse().unwrap()),
            })
        );
        assert!(matches!(
            provision(PackageId::JQ),
            Provision::System(Found {
                version: ObservedVersion::Unread(ProbeFailure::Failed),
                ..
            })
        ));
        assert_eq!(provision(PackageId::BUN), Provision::Missing);
    }

    #[cfg(unix)]
    #[test]
    fn runs_get_the_current_toolset_first_on_their_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = || ToolStore::without_downloads(dir.path().join("tools"));
        let host = ProcessHost::new(None, vec!["/usr/bin".into(), "/bin".into()], store());
        // Nothing installed: the inherited path is left alone.
        assert_eq!(host.tools(), RunTools::Empty);
        assert_eq!(host.search_path_with(&RunTools::Empty).unwrap(), None);

        let depot = Depot::builtin().unwrap();
        let empty = Toolset::empty();
        let plan = empty
            .plan(
                &depot,
                Platform::LinuxX64,
                &std::collections::BTreeSet::new(),
            )
            .unwrap();
        let one = store().activate(plan).unwrap();
        let tools = host.tools();
        let bin = dir.path().join("tools/generations/1/bin");
        assert_eq!(
            tools,
            RunTools::Toolset {
                generation: one.generation(),
                bin: bin.clone(),
            }
        );
        assert_eq!(
            host.search_path_with(&tools).unwrap(),
            Some(std::env::join_paths([bin, "/usr/bin".into(), "/bin".into()]).unwrap())
        );

        // A current link to a generation that is not there is unreadable,
        // never taken for no tools.
        std::fs::remove_file(dir.path().join("tools/current")).unwrap();
        std::os::unix::fs::symlink("generations/9", dir.path().join("tools/current")).unwrap();
        assert_eq!(host.tools(), RunTools::Unreadable);
        assert_eq!(RunTools::Unreadable.generation(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_probe_that_does_not_answer_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let slow = script(dir.path(), "slow", "exec sleep 30", 0o755);
        let tools = HostTools::new(vec![dir.path().into()]);
        assert_eq!(
            tools.probe(&slow, &["--version"]),
            Err(ProbeFailure::TimedOut)
        );
    }
    use ren_supervisor::{AgentHost, FileListing, PathKind, WalkLimits};

    #[test]
    fn a_folders_files_leave_out_what_git_ignores_and_hidden_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for folder in ["src/deep/er", "target/debug", ".git", ".hidden"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        for file in [
            "src/lib.rs",
            "src/deep/er/far.rs",
            "target/debug/ren",
            ".hidden/key",
            ".env",
            "README.md",
        ] {
            std::fs::write(root.join(file), "x").unwrap();
        }
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        let host = ProcessHost::new(
            None,
            Vec::new(),
            ToolStore::without_downloads(root.join("tools")),
        );
        let mut found: Vec<(String, PathKind)> = host
            .files(root, FileListing::LIMITS)
            .unwrap()
            .into_iter()
            .map(|found| (found.path, found.kind))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            found,
            [
                ("README.md".to_owned(), PathKind::File),
                ("src".to_owned(), PathKind::Directory),
                ("src/deep".to_owned(), PathKind::Directory),
                ("src/deep/er".to_owned(), PathKind::Directory),
                ("src/deep/er/far.rs".to_owned(), PathKind::File),
                ("src/lib.rs".to_owned(), PathKind::File),
            ]
        );
        let shallow = host
            .files(
                root,
                WalkLimits {
                    entries: 2,
                    depth: 1,
                },
            )
            .unwrap();
        assert_eq!(shallow.len(), 2);
        assert!(shallow.iter().all(|found| !found.path.contains('/')));
    }
    use ren_recorder::{
        id::{AttemptId, IdSource},
        time::{Clock, UnixMillis},
    };

    #[test]
    fn system_clock_is_monotonic() {
        let clock = SystemClock::start();
        let first = clock.now();
        let second = clock.now();
        assert!(second.monotonic >= first.monotonic);
        assert!(first.wall > UnixMillis::EPOCH);
    }

    #[test]
    fn random_ids_are_distinct_and_round_trip_through_text() {
        let source: &dyn IdSource = &RandomIds;
        let first = AttemptId::generate(source);
        let second = AttemptId::generate(source);
        assert_ne!(first, second);
        assert_eq!(first.to_string().parse::<AttemptId>(), Ok(first));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn work_on_the_observation_thread_never_stalls_the_callers_runtime() {
        // A task that blocks its thread, as heavy parsing would.
        let mut thread = super::ObservationThread::start(async {
            std::thread::sleep(std::time::Duration::from_millis(300));
            std::thread::current().name().map(str::to_owned)
        })
        .unwrap();
        // Meanwhile this single-threaded runtime keeps running timers.
        let mut ticks = 0;
        let output = loop {
            tokio::select! {
                output = thread.join() => break output,
                () = tokio::time::sleep(std::time::Duration::from_millis(10)) => ticks += 1,
            }
        };
        assert_eq!(output, Some(Some("ren-observation".to_owned())));
        assert!(ticks >= 10, "the caller's runtime stalled: {ticks} ticks");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn starts_a_process_in_its_directory_and_kills_it_on_request() {
        use super::{ProcessExit, ProcessHost, Stderr};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let mut echo = std::process::Command::new("sh");
        echo.args([
            "-c",
            "pwd; echo hidden >&2; read line; echo \"$line\"; exit 3",
        ]);
        let mut process = ProcessHost::start(echo, dir.path(), Stderr::Discarded).unwrap();
        process.stdin.write_all(b"hello\n").await.unwrap();
        let mut output = String::new();
        process.stdout.read_to_string(&mut output).await.unwrap();
        let resolved = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(output, format!("{}\nhello\n", resolved.display()));
        assert_eq!(process.exit.await, ProcessExit::Code(3));

        let mut sleeper = std::process::Command::new("sleep");
        sleeper.arg("30");
        let sleeping = ProcessHost::start(sleeper, dir.path(), Stderr::Discarded).unwrap();
        sleeping.kill.fire();
        assert_eq!(sleeping.exit.await, ProcessExit::Signal(9));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_login_is_read_from_stdout_and_stderr_together() {
        use super::{ProcessExit, ProcessHost, Stderr};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let mut login = std::process::Command::new("sh");
        // Like `claude auth login`: a link on stdout, a prompt with no
        // newline, then a failure on stderr.
        login.args([
            "-c",
            "echo 'visit: https://example.com/a'; printf 'Paste code > '; read code; echo \"Login failed: $code\" >&2; exit 1",
        ]);
        let mut process = ProcessHost::start(login, dir.path(), Stderr::WithStdout).unwrap();
        process.stdin.write_all(b"bad\n").await.unwrap();
        let mut output = String::new();
        process.stdout.read_to_string(&mut output).await.unwrap();
        assert_eq!(
            output,
            "visit: https://example.com/a\nPaste code > Login failed: bad\n"
        );
        assert_eq!(process.exit.await, ProcessExit::Code(1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_stopped_task_ends_without_output() {
        let mut stopped = super::ObservationThread::start(std::future::pending::<()>()).unwrap();
        stopped.abort();
        assert_eq!(stopped.join().await, None);
        let mut finished = super::ObservationThread::start(async { 7 }).unwrap();
        assert_eq!(finished.join().await, Some(7));
    }
}
