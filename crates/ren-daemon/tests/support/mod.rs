//! Shared integration-test support: loopback mock upstreams, history readers,
//! and the `TestGateway`, which assembles only the gateway, recorder, store,
//! and read API with test capabilities.
#![allow(dead_code, reason = "each test binary uses a different subset")]

pub mod agent;

use axum::Router;
use ren_daemon::config::Config;
use ren_gateway::build::Revision;
use ren_gateway::{
    Build, ControlToken, Gateway, Ingress, IngressError, ManagementSetup, ReqwestUpstream,
    Shutdown, Stopped, Upstream, control::Controls,
};
use ren_harness::{endpoint::GatewayEndpoint, lines::LineLimit};
use ren_protocol::{direction::Direction, label::Stored};
use ren_recorder::{
    Recorder, RecorderSetup,
    capture::{AttemptPhase, AttemptTerminal, TerminalCause},
    id::{CallId, SequentialIds},
    time::{ManualClock, UnixMillis},
};
use ren_store::{
    Reader, Store, StoreError,
    backup::Backup,
    query::{CallQuery, PageLimit},
    view::{CallDetail, CallView},
};
use ren_supervisor::{AgentHost, Supervisor, SupervisorOptions};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;

pub struct Mock {
    pub address: SocketAddr,
    task: JoinHandle<()>,
    stop: CancellationToken,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

pub async fn mock(app: Router) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let signal = stop.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    });
    Mock {
        address,
        task,
        stop,
    }
}

/// Every provider pointed at `mock`; Codex and DeepSeek below their native
/// path prefixes.
pub fn config(mock: &Mock) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        anthropic_base_url: format!("http://{}", mock.address),
        codex_base_url: format!("http://{}/backend-api/codex", mock.address),
        openai_base_url: format!("http://{}", mock.address),
        deepseek_base_url: format!("http://{}/anthropic", mock.address),
        ..Config::default()
    }
}

/// The build `/health` reports in tests.
pub fn build() -> Build {
    Build::new("0.0.0-test", Revision::Unknown)
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Calls recorded in `dir`, newest first.
pub async fn recorded(dir: &Path) -> Vec<CallView> {
    let reader = Reader::open(dir, 1).await.unwrap();
    let query = CallQuery {
        limit: PageLimit::new(1000).unwrap(),
        ..CallQuery::default()
    };
    let calls = reader.calls(&query).await.unwrap().calls;
    reader.close().await;
    calls
}

pub async fn detail(dir: &Path, call: CallId) -> CallDetail {
    let reader = Reader::open(dir, 1).await.unwrap();
    let detail = reader.call(call).await.unwrap();
    reader.close().await;
    detail
}

pub async fn exported(
    dir: &Path,
    call: CallId,
    direction: Direction,
) -> Result<Vec<u8>, StoreError> {
    let reader = Reader::open(dir, 1).await.unwrap();
    let key = reader.body_key(call, direction).await?;
    let mut bytes = Vec::new();
    reader
        .export(key, |chunk| {
            bytes.extend_from_slice(chunk);
            Ok(())
        })
        .await?;
    reader.close().await;
    Ok(bytes)
}

pub fn terminal(call: &CallView) -> AttemptTerminal {
    call.terminal.as_ref().and_then(Stored::known).unwrap()
}

/// The final attempt's terminal, cause, and phase, all known labels.
pub async fn ending(
    dir: &Path,
    call: CallId,
) -> (AttemptTerminal, Option<TerminalCause>, Option<AttemptPhase>) {
    let detail = detail(dir, call).await;
    let attempt = detail.attempts.last().unwrap();
    (
        attempt.terminal.as_ref().and_then(Stored::known).unwrap(),
        attempt
            .terminal_cause
            .as_ref()
            .map(|cause| cause.known().unwrap()),
        attempt
            .terminal_phase
            .as_ref()
            .map(|phase| phase.known().unwrap()),
    )
}

/// Each gateway run counts identifiers from its own block, so a run restarted
/// on the same directory never repeats one.
static RUNS: AtomicU64 = AtomicU64::new(0);

/// The gateway, recorder, store, and read API on a loopback listener, with a
/// fixed clock and sequential identifiers. No daemon, signals, or host config.
pub struct TestGateway {
    address: SocketAddr,
    router: Router,
    shutdown: Shutdown,
    server: JoinHandle<Result<Stopped, IngressError>>,
    worker: JoinHandle<Result<(), StoreError>>,
    /// The supervisor's task, when runs are enabled.
    runs: Option<JoinHandle<()>>,
    clock: Arc<ManualClock>,
}

impl TestGateway {
    /// How long running calls may continue after shutdown before they are
    /// cancelled. Short, so shutdown scenarios stay fast.
    pub const DRAIN_DEADLINE: Duration = Duration::from_secs(1);

    /// The environment every gateway resolves configuration with: one
    /// fixture operator key, never a real one.
    pub const OPERATOR_KEY_ENV: &'static str = "REN_TEST_OPERATOR_KEY";
    pub const OPERATOR_KEY: &'static str = "sk-test-FIXTURE";
    /// The control API's bearer token in every gateway.
    pub const CONTROL_TOKEN: &'static str = "testgatewaycontroltoken";

    fn resolve(config: &Config) -> ren_daemon::config::RuntimeConfig {
        config
            .resolve_with(&ren_harness::host::HostEnv::from_pairs([(
                Self::OPERATOR_KEY_ENV,
                Self::OPERATOR_KEY,
            )]))
            .unwrap()
    }

    /// Forward to the upstreams named in `config` over HTTP.
    pub async fn start(config: &Config, dir: &Path) -> Self {
        let runtime = Self::resolve(config);
        let upstream =
            ReqwestUpstream::new(runtime.upstreams().clone(), runtime.transport()).unwrap();
        Self::with_upstream(config, dir, upstream).await
    }

    /// Forward to a substituted upstream; `config` supplies everything else.
    pub async fn with_upstream<U: Upstream>(config: &Config, dir: &Path, upstream: U) -> Self {
        Self::assemble(
            config,
            dir,
            upstream,
            None::<(Vec<PathBuf>, agent::ScriptedHost)>,
            None,
        )
        .await
    }

    /// Forward over HTTP, with `tools` behind `/control/v1/tools`.
    pub async fn with_tools(
        config: &Config,
        dir: &Path,
        tools: Arc<dyn ren_gateway::tools::Toolbox>,
    ) -> Self {
        let runtime = Self::resolve(config);
        let upstream =
            ReqwestUpstream::new(runtime.upstreams().clone(), runtime.transport()).unwrap();
        Self::assemble(
            config,
            dir,
            upstream,
            None::<(Vec<PathBuf>, agent::ScriptedHost)>,
            Some(tools),
        )
        .await
    }

    /// Forward over HTTP, with agent runs in `roots` started by `host`.
    pub async fn with_runs<H: AgentHost>(
        config: &Config,
        dir: &Path,
        roots: Vec<PathBuf>,
        host: H,
    ) -> Self {
        let runtime = Self::resolve(config);
        let upstream =
            ReqwestUpstream::new(runtime.upstreams().clone(), runtime.transport()).unwrap();
        Self::assemble(config, dir, upstream, Some((roots, host)), None).await
    }

    async fn assemble<U: Upstream, H: AgentHost>(
        config: &Config,
        dir: &Path,
        upstream: U,
        runs: Option<(Vec<PathBuf>, H)>,
        tools: Option<Arc<dyn ren_gateway::tools::Toolbox>>,
    ) -> Self {
        let runtime = Self::resolve(config);
        let clock = Arc::new(ManualClock::at(UnixMillis::from_millis(1_800_000_000_000)));
        let block = RUNS.fetch_add(1, Ordering::Relaxed);
        let ids = Arc::new(SequentialIds::starting_at((block << 32) | 1));
        let (store, recovery) = Store::open(dir, clock.as_ref(), ids.clone()).await.unwrap();
        let operator = recovery
            .operator
            .as_ref()
            .map_or_else(Default::default, |stored| {
                ren_gateway::operator::OperatorState::restore(stored.revision, &stored.document)
                    .unwrap()
            });
        let backup = Backup::new(dir, store.sweep_gate());
        let (recorder, task) = Recorder::new(
            store,
            recovery.calls,
            RecorderSetup {
                options: runtime.capture().clone(),
                retention: runtime.retention().clone(),
                max_calls: runtime.recorder_calls(),
                live_buffer: 64,
                scanner: std::sync::Arc::new(ren_recorder::secrets::SecretScanner::new().unwrap()),
            },
            clock.clone(),
            ids.clone(),
        )
        .unwrap();
        let worker = tokio::spawn(task.run());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (supervisor, runs) = match runs {
            Some((roots, host)) => {
                let (supervisor, task) = Supervisor::new(
                    SupervisorOptions {
                        roots,
                        places: Vec::new(),
                        scratch: Some(std::path::PathBuf::from("/scratch")),
                        home: Some(std::path::PathBuf::from("/work")),
                        warmup: ren_supervisor::Warmup::WhenAsked,
                        gateway: GatewayEndpoint::parse(&format!("http://{address}")).unwrap(),
                        version: "0.0.0-test".into(),
                        line_limit: LineLimit::DEFAULT,
                        idle: ren_supervisor::IdleStop::Never,
                    },
                    host,
                    ids,
                    recorder.clone(),
                )
                .unwrap();
                (Some(supervisor), Some(tokio::spawn(task.run())))
            }
            None => (None, None),
        };
        let reader = Reader::open(dir, 2).await.unwrap();
        let shutdown = Shutdown::default();
        let controls = Arc::new(Controls::restore(operator));
        let gateway = Arc::new(Gateway::new(
            runtime.gateway(),
            upstream,
            recorder,
            controls,
            clock.clone(),
            shutdown.clone(),
        ));
        let router = gateway.router(ManagementSetup {
            reader,
            backup,
            retention: runtime.retention().clone(),
            control: runtime.control().clone(),
            token: ControlToken::parse(Self::CONTROL_TOKEN).unwrap(),
            build: build(),
            instance: runtime.instance(),
            runs: supervisor,
            tools,
        });
        let ingress = Ingress::new(listener, router.clone());
        let server = tokio::spawn(ingress.serve(shutdown.clone(), Self::DRAIN_DEADLINE));
        Self {
            address,
            router,
            shutdown,
            server,
            worker,
            runs,
            clock,
        }
    }

    /// The gateway's clock, which moves only when advanced.
    pub fn clock(&self) -> &ManualClock {
        &self.clock
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The served router, for driving a request without a connection: a test
    /// can drop the handler at any point, as a closing connection would.
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    pub fn base(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Begin the drain without waiting for it.
    pub fn shutdown(&self) {
        self.shutdown.drain();
    }

    /// Drain, wait for the server, then for every accepted record to commit.
    /// Every call must have finished within the drain deadline.
    pub async fn stop(self) {
        self.stop_expecting(Stopped::Drained).await;
    }

    /// Stop, asserting how the server stopped.
    pub async fn stop_expecting(self, expected: Stopped) {
        // The router holds the gateway, and with it the recorder's sender.
        drop(self.router);
        self.shutdown.drain();
        let stopped = timeout(Duration::from_secs(10), self.server)
            .await
            .expect("server failed to stop")
            .unwrap()
            .unwrap();
        if let Some(runs) = self.runs {
            timeout(Duration::from_secs(10), runs)
                .await
                .expect("runs failed to drain")
                .unwrap();
        }
        timeout(Duration::from_secs(10), self.worker)
            .await
            .expect("recorder failed to drain")
            .unwrap()
            .unwrap();
        assert_eq!(stopped, expected, "unexpected shutdown outcome");
    }
}
