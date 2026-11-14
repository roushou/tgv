//! Strict startup configuration. This revision is immutable for the daemon lifetime.

use ren_depot::{CatalogEntry, CatalogError, Depot};
use ren_gateway::{
    BufferOptions, ConfiguredEntry, ControlOptions, EntryIngress, GatewayConfig, InstanceOptions,
    NativeUrls, OperatorKey, TransportTimeouts, UpstreamAuth, UpstreamEntry, UpstreamTable,
    UpstreamUrl, WebSocketOptions,
    policy::{EntryUsers, Policy, PolicyConfig},
    user::{UserConfig, Users},
};
use ren_harness::{host::HostEnv, pi::PiCache};
use ren_protocol::route::{Api, Provider, UnsupportedWire, Wire};
use ren_recorder::retention::RetentionPolicy;
use ren_recorder::{
    CaptureOptions,
    caller::UserId,
    routing::{ConfigRevision, UpstreamId},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

/// Why a configuration was refused. Invalid values are named, never echoed:
/// they can carry credentials.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read configuration file")]
    Read(#[source] std::io::Error),
    #[error("invalid TOML configuration or unknown configuration field")]
    Syntax,
    #[error("unsupported configuration version")]
    Version,
    #[error("listen must be loopback; use Tailscale Serve for private remote access")]
    PublicListener,
    #[error("max_concurrent_requests and max_concurrent_unnamed must be between 1 and 1024")]
    Concurrency,
    #[error("timeouts must be between 1 and 3600 seconds")]
    Timeout,
    #[error(transparent)]
    Capture(#[from] ren_recorder::CaptureOptionsError),
    #[error(transparent)]
    Retention(#[from] ren_recorder::retention::RetentionError),
    #[error(transparent)]
    WebSocket(#[from] ren_gateway::WebSocketOptionsError),
    #[error(transparent)]
    Buffer(#[from] ren_gateway::BufferOptionsError),
    #[error(transparent)]
    Control(#[from] ren_gateway::ControlOptionsError),
    #[error("upstream {id}: {source}")]
    UpstreamUrl {
        id: String,
        source: ren_gateway::UpstreamConfigError,
    },
    #[error(transparent)]
    ProviderUrl(#[from] ren_gateway::UpstreamConfigError),
    #[error("upstream {id}: {source}")]
    Wire { id: String, source: UnsupportedWire },
    #[error("upstream {upstream}: users names {user}, which no [[users]] entry is")]
    UnknownEntryUser { upstream: String, user: String },
    #[error("upstream {0}: users is empty, so no caller could use it; omit it to admit anyone")]
    EmptyEntryUsers(String),
    #[error("upstream {0}: the Codex backend accepts no operator keys")]
    CodexOperatorKey(String),
    #[error(transparent)]
    Upstreams(#[from] ren_gateway::UpstreamTableError),
    #[error(transparent)]
    Policy(#[from] ren_gateway::policy::PolicyError),
    #[error(transparent)]
    Users(#[from] ren_gateway::user::UsersError),
    #[error("operator key variable {name} for upstream {upstream} is not set")]
    MissingKey { name: String, upstream: String },
    #[error("operator key variable {0} is not a valid header value")]
    InvalidKey(String),
    #[error("configuration cannot be serialized for its revision")]
    Revision(#[source] serde_json::Error),
    #[error("set --data-dir when HOME is unavailable")]
    NoDataDir,
    #[error("tools: {0}")]
    Tools(#[from] CatalogError),
    #[error(
        "runs.roots entries must be absolute paths, and runs.places and runs.scratch absolute or under ~/"
    )]
    RunRoot,
}

/// Where operator keys come from while resolving.
#[derive(Debug, Clone, Copy)]
enum Keys<'a> {
    Env(&'a HostEnv),
    /// Structure only: every key is a placeholder that is never used.
    Unchecked,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
/// Editable TOML representation. Use `resolve` before constructing a daemon.
pub struct Config {
    pub version: u32,
    pub listen: SocketAddr,
    pub data_dir: Option<PathBuf>,
    pub anthropic_base_url: String,
    pub codex_base_url: String,
    pub openai_base_url: String,
    pub deepseek_base_url: String,
    pub max_concurrent_requests: usize,
    /// Calls callers that are not named users may have running at once,
    /// together.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_unnamed: Option<u32>,
    pub connect_timeout_seconds: u64,
    pub response_header_timeout_seconds: u64,
    pub stream_idle_timeout_seconds: u64,
    pub capture: CaptureOptions,
    pub retention: RetentionPolicy,
    pub websocket: WebSocketOptions,
    /// Extra upstream entries, each with an operator API key. Every
    /// provider also has a client-credential entry named after it.
    pub upstreams: Vec<UpstreamConfig>,
    pub buffer: BufferOptions,
    pub policy: PolicyConfig,
    pub control: ControlOptions,
    /// The gateway's named users, each proven by a Tailscale login or a
    /// user token.
    pub users: Vec<UserConfig>,
    /// Whether this is a production or a development instance.
    pub instance: InstanceOptions,
    /// Agent runs: off until roots are configured.
    pub runs: RunsOptions,
    /// The packages the operator adds to the built-in catalog.
    #[serde(skip_serializing_if = "ToolsOptions::is_empty")]
    pub tools: ToolsOptions,
}

/// The `[tools]` section: catalog entries in the built-in catalog's form,
/// each adding a runtime or utility Ren can install for its agents, or
/// replacing the built-in one of the same name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsOptions {
    pub package: Vec<CatalogEntry>,
}

impl ToolsOptions {
    fn is_empty(&self) -> bool {
        self.package.is_empty()
    }

    /// The built-in catalog with these entries.
    pub fn depot(&self) -> Result<Depot, ConfigError> {
        Ok(Depot::configured(&self.package)?)
    }
}

/// The `[runs]` section: the folders agent runs may work in. Runs are off
/// while `roots` is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunsOptions {
    /// Absolute paths. A run may start in a root or any folder under one,
    /// after links are resolved; the projects offered are found under them.
    pub roots: Vec<PathBuf>,
    /// Other places runs may start in, offered only when asked for: each
    /// one's own folders, hidden ones included (`~/.config`). Absolute, or
    /// under the home as `~/…`.
    pub places: Vec<PathBuf>,
    /// Where scratch runs each get a new folder. Absolute or `~/…`; by
    /// default `$XDG_DATA_HOME/ren/scratch`, else `~/.local/share/ren/scratch`.
    pub scratch: Option<PathBuf>,
}

/// The `[runs]` paths with the home resolved, as the supervisor takes them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunPlaces {
    pub places: Vec<PathBuf>,
    pub scratch: Option<PathBuf>,
    pub home: Option<PathBuf>,
}

impl RunsOptions {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let anchored = |path: &PathBuf| path.is_absolute() || path.starts_with("~");
        if self.roots.iter().all(|root| root.is_absolute())
            && self.places.iter().chain(&self.scratch).all(anchored)
        {
            Ok(())
        } else {
            Err(ConfigError::RunRoot)
        }
    }

    /// The other places and the scratch folder, `~` read as the home `env`
    /// names. A path under a home that is not known is left out.
    pub fn places_in(&self, env: &HostEnv) -> RunPlaces {
        let home = env.absolute_path("HOME");
        let anchored = |path: &PathBuf| match path.strip_prefix("~") {
            Ok(rest) => home.as_ref().map(|home| home.join(rest)),
            Err(_) => Some(path.clone()),
        };
        let scratch = match &self.scratch {
            Some(scratch) => anchored(scratch),
            None => env
                .absolute_path("XDG_DATA_HOME")
                .or_else(|| home.as_ref().map(|home| home.join(".local/share")))
                .map(|data| data.join("ren/scratch")),
        };
        RunPlaces {
            places: self.places.iter().filter_map(anchored).collect(),
            scratch,
            home,
        }
    }
}

/// An operator-key upstream entry. The key is read from the daemon's
/// environment at start, never from the file.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub id: UpstreamId,
    pub provider: Provider,
    /// The wire format the entry speaks; the provider's native one when
    /// omitted. Refused at load unless the provider offers it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<Wire>,
    pub base_url: String,
    /// The environment variable holding the API key.
    pub operator_key_env: String,
    /// Whether the entry has routes of its own; fallback only when omitted.
    #[serde(default, skip_serializing_if = "EntryIngress::is_fallback_only")]
    pub ingress: EntryIngress,
    /// The named users who may use the entry; anyone when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub users: Option<Vec<UserId>>,
}

impl UpstreamConfig {
    /// Who may use the entry. Every name must be a configured user.
    fn users(&self, known: &[UserConfig]) -> Result<EntryUsers, ConfigError> {
        let Some(users) = &self.users else {
            return Ok(EntryUsers::Anyone);
        };
        if users.is_empty() {
            return Err(ConfigError::EmptyEntryUsers(self.id.to_string()));
        }
        if let Some(unknown) = users
            .iter()
            .find(|user| !known.iter().any(|config| &config.id == *user))
        {
            return Err(ConfigError::UnknownEntryUser {
                upstream: self.id.to_string(),
                user: unknown.to_string(),
            });
        }
        Ok(EntryUsers::Only(users.clone()))
    }

    /// The provider's API in the configured wire format, or its native API.
    fn api(&self) -> Result<Api, ConfigError> {
        match self.wire {
            None => Ok(self.provider.native_api()),
            Some(wire) => self
                .provider
                .speak(wire)
                .map_err(|source| ConfigError::Wire {
                    id: self.id.to_string(),
                    source,
                }),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787),
            data_dir: None,
            anthropic_base_url: "https://api.anthropic.com".into(),
            codex_base_url: "https://chatgpt.com/backend-api/codex".into(),
            openai_base_url: "https://api.openai.com".into(),
            deepseek_base_url: "https://api.deepseek.com/anthropic".into(),
            max_concurrent_requests: 32,
            max_concurrent_unnamed: None,
            connect_timeout_seconds: 10,
            response_header_timeout_seconds: 120,
            stream_idle_timeout_seconds: 120,
            capture: CaptureOptions::default(),
            retention: RetentionPolicy::default(),
            websocket: WebSocketOptions::default(),
            upstreams: Vec::new(),
            buffer: BufferOptions::default(),
            policy: PolicyConfig::default(),
            control: ControlOptions::default(),
            users: Vec::new(),
            instance: InstanceOptions::default(),
            runs: RunsOptions::default(),
            tools: ToolsOptions::default(),
        }
    }
}

impl Config {
    /// Resolve the editable file representation into an immutable runtime
    /// snapshot, with no environment: configured operator keys are an error.
    /// Keep `load` usable by remote diagnostics and dry-run launchers, which do
    /// not use this host's upstream settings.
    pub fn resolve(&self) -> Result<RuntimeConfig, ConfigError> {
        self.resolve_with(&HostEnv::default())
    }

    /// Everything [`Self::resolve_with`] checks, except that operator keys
    /// are present: for installing a service whose environment differs.
    pub fn check(&self) -> Result<(), ConfigError> {
        self.build(Keys::Unchecked).map(drop)
    }

    /// Resolve with operator keys read from `env`.
    pub fn resolve_with(&self, env: &HostEnv) -> Result<RuntimeConfig, ConfigError> {
        self.build(Keys::Env(env))
    }

    fn key(upstream: &UpstreamConfig, keys: Keys<'_>) -> Result<OperatorKey, ConfigError> {
        let name = &upstream.operator_key_env;
        let text = match keys {
            Keys::Unchecked => "unchecked",
            Keys::Env(env) => env.get(name).ok_or_else(|| ConfigError::MissingKey {
                name: name.clone(),
                upstream: upstream.id.to_string(),
            })?,
        };
        OperatorKey::new(text).ok_or_else(|| ConfigError::InvalidKey(name.clone()))
    }

    fn build(&self, keys: Keys<'_>) -> Result<RuntimeConfig, ConfigError> {
        self.validate()?;
        let extra = self
            .upstreams
            .iter()
            .map(|upstream| {
                if upstream.provider == Provider::Codex {
                    return Err(ConfigError::CodexOperatorKey(upstream.id.to_string()));
                }
                let entry = UpstreamEntry {
                    id: upstream.id.clone(),
                    api: upstream.api()?,
                    url: UpstreamUrl::parse(&upstream.base_url).map_err(|source| {
                        ConfigError::UpstreamUrl {
                            id: upstream.id.to_string(),
                            source,
                        }
                    })?,
                    auth: UpstreamAuth::OperatorKey(Self::key(upstream, keys)?),
                };
                Ok(ConfiguredEntry {
                    entry,
                    ingress: upstream.ingress,
                    users: upstream.users(&self.users)?,
                })
            })
            .collect::<Result<Vec<_>, ConfigError>>()?;
        let upstreams = UpstreamTable::new(
            NativeUrls {
                anthropic: UpstreamUrl::parse(&self.anthropic_base_url)?,
                codex: UpstreamUrl::parse(&self.codex_base_url)?,
                openai: UpstreamUrl::parse(&self.openai_base_url)?,
                deepseek: UpstreamUrl::parse(&self.deepseek_base_url)?,
            },
            extra,
        )?;
        let policy = Policy::new(self.policy.clone(), &upstreams.summaries())?;
        let users = Users::new(self.users.clone())?;
        let depot = self.tools.depot()?;
        Ok(RuntimeConfig {
            listen: self.listen,
            upstreams,
            policy,
            users,
            instance: self.instance,
            max_concurrent_requests: self.max_concurrent_requests,
            max_concurrent_unnamed: self.max_concurrent_unnamed,
            connect_timeout: Duration::from_secs(self.connect_timeout_seconds),
            response_header_timeout: Duration::from_secs(self.response_header_timeout_seconds),
            stream_idle_timeout: Duration::from_secs(self.stream_idle_timeout_seconds),
            capture: self.capture.clone(),
            retention: self.retention.clone(),
            websocket: self.websocket,
            buffer: self.buffer,
            control: self.control.clone(),
            runs: self.runs.clone(),
            run_places: match keys {
                Keys::Env(env) => self.runs.places_in(env),
                Keys::Unchecked => RunPlaces::default(),
            },
            pi_cache: match keys {
                Keys::Env(env) => PiCache::locate(env).ok(),
                Keys::Unchecked => None,
            },
            agent_path: match keys {
                Keys::Env(env) => env.search_path(),
                Keys::Unchecked => Vec::new(),
            },
            depot,
            revision: self.revision()?,
        })
    }

    pub fn revision(&self) -> Result<String, ConfigError> {
        Ok(
            Sha256::digest(serde_json::to_vec(self).map_err(ConfigError::Revision)?)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }

    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut config: Self = match path {
            Some(path) => {
                let contents = std::fs::read_to_string(path).map_err(ConfigError::Read)?;
                // TOML diagnostics can echo credential-bearing invalid values.
                toml::from_str(&contents).map_err(|_| ConfigError::Syntax)?
            }
            None => Self::default(),
        };
        if let (Some(path), Some(dir)) = (path, config.data_dir.as_mut())
            && dir.is_relative()
        {
            *dir = path.parent().unwrap_or(Path::new(".")).join(&*dir);
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != 1 {
            return Err(ConfigError::Version);
        }
        if !self.listen.ip().is_loopback() {
            return Err(ConfigError::PublicListener);
        }
        if !(1..=1024).contains(&self.max_concurrent_requests)
            || self
                .max_concurrent_unnamed
                .is_some_and(|limit| !(1..=1024).contains(&limit))
        {
            return Err(ConfigError::Concurrency);
        }
        if [
            self.connect_timeout_seconds,
            self.response_header_timeout_seconds,
            self.stream_idle_timeout_seconds,
        ]
        .iter()
        .any(|value| !(1..=3600).contains(value))
        {
            return Err(ConfigError::Timeout);
        }
        self.capture.validate()?;
        self.retention.validate()?;
        self.websocket.validate()?;
        self.buffer.validate()?;
        self.control.validate()?;
        self.runs.validate()?;
        self.tools.depot()?;
        Ok(())
    }

    /// The data directory: the override, then `data_dir`, then
    /// `$XDG_STATE_HOME/ren`, then `~/.local/state/ren`.
    pub fn resolve_data_dir(
        &self,
        override_dir: Option<PathBuf>,
        env: &HostEnv,
    ) -> Result<PathBuf, ConfigError> {
        if let Some(dir) = override_dir.or_else(|| self.data_dir.clone()) {
            return Ok(dir);
        }
        if let Some(dir) = env.absolute_path("XDG_STATE_HOME") {
            return Ok(dir.join("ren"));
        }
        env.absolute_path("HOME")
            .map(|home| home.join(".local/state/ren"))
            .ok_or(ConfigError::NoDataDir)
    }
}

/// Validated settings consumed by daemon assembly. No mutable access or
/// deserialization bypass: all instances are produced by `Config::resolve`.
#[derive(Clone)]
pub struct RuntimeConfig {
    listen: SocketAddr,
    upstreams: UpstreamTable,
    policy: Policy,
    users: Users,
    instance: InstanceOptions,
    max_concurrent_requests: usize,
    max_concurrent_unnamed: Option<u32>,
    connect_timeout: Duration,
    response_header_timeout: Duration,
    stream_idle_timeout: Duration,
    capture: CaptureOptions,
    retention: RetentionPolicy,
    websocket: WebSocketOptions,
    buffer: BufferOptions,
    control: ControlOptions,
    runs: RunsOptions,
    /// The runs' other places and scratch folder, with the home resolved.
    run_places: RunPlaces,
    /// Where driven Pi processes find Ren's extension, when the host
    /// environment names a cache.
    pi_cache: Option<PiCache>,
    /// The host's search path, which a run's toolset folder is put before.
    agent_path: Vec<PathBuf>,
    /// The packages Ren knows: the built-in catalog with `[tools]`.
    depot: Depot,
    revision: String,
}

impl RuntimeConfig {
    pub fn listen(&self) -> SocketAddr {
        self.listen
    }
    pub fn upstreams(&self) -> &UpstreamTable {
        &self.upstreams
    }
    pub fn control(&self) -> &ControlOptions {
        &self.control
    }
    pub fn instance(&self) -> InstanceOptions {
        self.instance
    }
    pub fn runs(&self) -> &RunsOptions {
        &self.runs
    }
    pub fn run_places(&self) -> &RunPlaces {
        &self.run_places
    }
    pub fn agent_path(&self) -> &[PathBuf] {
        &self.agent_path
    }
    pub fn depot(&self) -> &Depot {
        &self.depot
    }

    pub fn pi_cache(&self) -> Option<&PiCache> {
        self.pi_cache.as_ref()
    }
    /// The gateway's settings from this snapshot.
    pub fn gateway(&self) -> GatewayConfig {
        GatewayConfig {
            max_concurrent_requests: self.max_concurrent_requests,
            response_header_timeout: self.response_header_timeout,
            stream_idle_timeout: self.stream_idle_timeout,
            config_revision: ConfigRevision::new(self.revision.as_str()),
            websocket: self.websocket,
            buffer: self.buffer,
            policy: self.policy.clone(),
            upstreams: self.upstreams.clone(),
            users: self.users.clone(),
            max_concurrent_unnamed: self.max_concurrent_unnamed,
        }
    }
    /// Lifecycle records the recorder reserves at once: every admitted call,
    /// each channel's end, and a margin for calls whose response is still
    /// draining after their admission returned.
    pub fn recorder_calls(&self) -> usize {
        self.max_concurrent_requests
            .saturating_mul(4)
            .saturating_add(self.websocket.max_channels.saturating_mul(2))
    }
    pub fn transport(&self) -> TransportTimeouts {
        TransportTimeouts {
            connect: self.connect_timeout,
            idle: self.stream_idle_timeout,
        }
    }
    pub fn max_concurrent_requests(&self) -> usize {
        self.max_concurrent_requests
    }
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }
    pub fn response_header_timeout(&self) -> Duration {
        self.response_header_timeout
    }
    pub fn stream_idle_timeout(&self) -> Duration {
        self.stream_idle_timeout
    }
    pub fn capture(&self) -> &CaptureOptions {
        &self.capture
    }
    pub fn retention(&self) -> &RetentionPolicy {
        &self.retention
    }
    pub fn revision(&self) -> &str {
        &self.revision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_keys_and_public_listener() {
        assert!(toml::from_str::<Config>("max_concurrent_request = 10").is_err());
        let config = Config {
            listen: "0.0.0.0:8787".parse().unwrap(),
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn tools_entries_join_the_builtin_catalog_and_bad_ones_are_refused_at_load() {
        let entry =
            "[[tools.package]]\nid = \"acme-lint\"\nkind = \"utility\"\nprogram = \"acme-lint\"\n\
                     [tools.package.release]\nversion = \"0.3.0\"\n\
                     [[tools.package.release.artifact]]\nplatform = \"linux_x64\"\n\
                     url = \"https://example.test/acme-lint.tar.gz\"\nsha256 = \"{digest}\"\n\
                     format = \"tar_gz\"\nprogram_at = \"acme-lint\"\n"
                .replace("{digest}", &"a".repeat(64));
        let config: Config = toml::from_str(&entry).unwrap();
        config.validate().unwrap();
        let depot = config.resolve().unwrap().depot().clone();
        let acme = depot.package(&"acme-lint".parse().unwrap()).unwrap();
        assert_eq!(acme.origin(), ren_depot::package::Origin::Operator);
        // The section round-trips, and is left out of the revision when empty.
        let written: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(written.tools, config.tools);
        assert!(
            !toml::to_string(&Config::default())
                .unwrap()
                .contains("tools")
        );

        let client = "[[tools.package]]\nid = \"codex\"\nkind = \"utility\"\nprogram = \"codex\"\n";
        let config: Config = toml::from_str(client).unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Tools(ren_depot::CatalogError::Client(_)))
        ));
        assert!(toml::from_str::<Config>(&entry.replace("https://", "http://")).is_err());
    }

    #[test]
    fn run_places_and_scratch_read_the_home() {
        let config: Config = toml::from_str(
            "[runs]\nroots = [\"/home/op/dev\"]\nplaces = [\"~/.config\", \"/srv/dotfiles\"]\n",
        )
        .unwrap();
        let env = HostEnv::from_pairs([("HOME", "/home/op")]);
        let places = config.runs.places_in(&env);
        assert_eq!(
            places.places,
            [
                PathBuf::from("/home/op/.config"),
                PathBuf::from("/srv/dotfiles")
            ]
        );
        assert_eq!(
            places.scratch,
            Some(PathBuf::from("/home/op/.local/share/ren/scratch"))
        );
        let env = HostEnv::from_pairs([("HOME", "/home/op"), ("XDG_DATA_HOME", "/data")]);
        assert_eq!(
            config.runs.places_in(&env).scratch,
            Some(PathBuf::from("/data/ren/scratch"))
        );
        // Without a home, a place under it is left out.
        let homeless = config.runs.places_in(&HostEnv::from_pairs([]));
        assert_eq!(homeless.places, [PathBuf::from("/srv/dotfiles")]);
        let relative: Config = toml::from_str("[runs]\nplaces = [\"dotfiles\"]\n").unwrap();
        assert!(relative.runs.validate().is_err());
    }

    #[test]
    fn runs_need_absolute_roots() {
        let config: Config = toml::from_str("[runs]\nroots = [\"/home/op/dev\"]\n").unwrap();
        config.resolve().unwrap();
        let config: Config = toml::from_str("[runs]\nroots = [\"dev\"]\n").unwrap();
        assert!(matches!(config.validate(), Err(ConfigError::RunRoot)));
        assert!(
            Config::default().runs.roots.is_empty(),
            "runs are off by default"
        );
    }

    #[test]
    fn example_is_valid() {
        let config: Config = toml::from_str(include_str!("../../../ren.example.toml")).unwrap();
        config.resolve().unwrap();
    }

    #[test]
    fn users_parse_and_a_duplicate_login_is_refused() {
        let digest = "ab".repeat(32);
        let config: Config = toml::from_str(&format!(
            "[[users]]\nid = \"alice\"\ntailscale_logins = [\"alice@example.com\"]\n\
             [[users]]\nid = \"ci\"\ntoken_blake3 = \"{digest}\"\n"
        ))
        .unwrap();
        config.resolve().unwrap();
        let config: Config = toml::from_str(
            "[[users]]\nid = \"a\"\ntailscale_logins = [\"x@y\"]\n\
             [[users]]\nid = \"b\"\ntailscale_logins = [\"x@y\"]\n",
        )
        .unwrap();
        assert_eq!(
            config
                .resolve()
                .err()
                .map(|error| error.to_string())
                .as_deref(),
            Some("Tailscale login x@y belongs to two users")
        );
        assert!(
            toml::from_str::<Config>("[[users]]\nid = \"Alice\"\ntailscale_logins = [\"a@b\"]\n")
                .is_err()
        );
        let entry = |users: &str| -> Option<String> {
            let config: Config = toml::from_str(&format!(
                "[[users]]\nid = \"alice\"\ntailscale_logins = [\"a@b\"]\n\
                 [[upstreams]]\nid = \"key\"\nprovider = \"anthropic\"\n\
                 base_url = \"https://api.anthropic.com\"\noperator_key_env = \"K\"\n{users}"
            ))
            .unwrap();
            config.check().err().map(|error| error.to_string())
        };
        assert_eq!(entry("users = [\"alice\"]"), None);
        assert_eq!(
            entry("users = [\"bob\"]").as_deref(),
            Some("upstream key: users names bob, which no [[users]] entry is")
        );
        assert!(entry("users = []").is_some());
    }

    #[test]
    fn runtime_snapshot_is_independent_of_later_document_edits() {
        let mut config = Config::default();
        let revision = config.revision().unwrap();
        let runtime = config.resolve().unwrap();
        config.listen = "0.0.0.0:1".parse().unwrap();
        config.max_concurrent_requests = 0;
        config.connect_timeout_seconds = 0;
        config.capture.queue_bytes = 0;
        config.retention.max_calls = 0;
        config.codex_base_url = "http://example.com".into();
        assert!(config.resolve().is_err());
        assert_eq!(runtime.listen(), "127.0.0.1:8787".parse().unwrap());
        assert_eq!(runtime.max_concurrent_requests(), 32);
        assert_eq!(runtime.connect_timeout(), Duration::from_secs(10));
        assert_eq!(runtime.capture().queue_bytes, 64 * 1024 * 1024);
        assert_eq!(runtime.retention().max_calls, 10_000);
        assert_eq!(runtime.revision(), revision);
    }

    #[test]
    fn runtime_resolution_validates_upstreams_without_echoing_invalid_values() {
        for base in [
            "https://user:SECRET@example.com",
            "https://example.com?token=SECRET",
            "https://example.com/#SECRET",
            "http://example.com",
        ] {
            let config = Config {
                openai_base_url: base.into(),
                ..Config::default()
            };
            // Remote diagnostics and launch dry-runs don't consume local upstreams.
            assert!(config.validate().is_ok());
            let error = config.resolve().err().unwrap();
            assert!(!format!("{error:?}").contains("SECRET"));
        }
    }

    fn with_operator_key(policy: &str) -> Config {
        Config {
            upstreams: vec![UpstreamConfig {
                id: UpstreamId::parse("anthropic-key").unwrap(),
                provider: Provider::Anthropic,
                wire: None,
                base_url: "https://api.anthropic.com".into(),
                operator_key_env: "REN_FIXTURE_KEY".into(),
                ingress: EntryIngress::FallbackOnly,
                users: None,
            }],
            policy: toml::from_str(policy).unwrap(),
            ..Config::default()
        }
    }

    #[test]
    fn operator_keys_come_from_the_environment_and_never_appear_in_errors() {
        let config = with_operator_key("");
        let missing = format!("{:#}", config.resolve().err().unwrap());
        assert!(missing.contains("REN_FIXTURE_KEY"), "{missing}");
        // Structure is checked without the key, for service installs.
        config.check().unwrap();
        let env = HostEnv::from_pairs([("REN_FIXTURE_KEY", "sk-SECRET")]);
        let runtime = config.resolve_with(&env).unwrap();
        let entry = runtime
            .upstreams()
            .get(&UpstreamId::parse("anthropic-key").unwrap())
            .unwrap();
        assert!(!format!("{entry:?}").contains("SECRET"));
        let invalid = HostEnv::from_pairs([("REN_FIXTURE_KEY", "sk-\nSECRET")]);
        let error = format!("{:#}", config.resolve_with(&invalid).err().unwrap());
        assert!(!error.contains("SECRET"), "{error}");
    }

    #[test]
    fn unsafe_upstreams_and_protected_transforms_are_refused_at_load() {
        let mut codex = with_operator_key("");
        codex.upstreams[0].provider = Provider::Codex;
        assert!(codex.check().is_err());
        let mut reserved = with_operator_key("");
        reserved.upstreams[0].id = UpstreamId::parse("anthropic").unwrap();
        assert!(reserved.check().is_err());
        // Loading checks the policy; its rules are tested with the policy.
        let spoof = "[[transform]]\nid = \"bad\"\nmatch = { provider = \"anthropic\" }\nsteps = [{ op = \"set_header\", name = \"user-agent\", value = \"spoof\" }]\nreason = \"fixture\"\n";
        assert!(with_operator_key(spoof).check().is_err());
    }

    #[test]
    fn an_upstream_speaks_its_providers_native_wire_unless_one_it_offers_is_named() {
        let key = UpstreamId::parse("anthropic-key").unwrap();
        let env = HostEnv::from_pairs([("REN_FIXTURE_KEY", "sk-FIXTURE")]);
        let api = |config: &Config| {
            config
                .resolve_with(&env)
                .unwrap()
                .upstreams()
                .get(&key)
                .unwrap()
                .api
        };
        let omitted = with_operator_key("");
        assert_eq!(api(&omitted), Provider::Anthropic.native_api());

        let mut named = with_operator_key("");
        named.upstreams[0].wire = Some(Wire::AnthropicMessages);
        assert_eq!(api(&named), Provider::Anthropic.native_api());

        let mut unsupported = with_operator_key("");
        unsupported.upstreams[0].wire = Some(Wire::OpenAiResponses);
        let error = unsupported.check().err().unwrap().to_string();
        assert_eq!(
            error,
            "upstream anthropic-key: provider anthropic does not offer the openai_responses wire format"
        );
    }

    #[test]
    fn optional_upstream_keys_read_labels_and_omitted_ones_are_not_written() {
        let entry = |wire: &str| {
            toml::from_str::<UpstreamConfig>(&format!(
                "id = \"k\"\nprovider = \"openai\"\n{wire}base_url = \"https://api.openai.com\"\noperator_key_env = \"K\"\n"
            ))
        };
        assert_eq!(
            entry("wire = \"openai_responses\"\n").unwrap().wire,
            Some(Wire::OpenAiResponses)
        );
        assert_eq!(
            entry("wire = \"openai_chat_completions\"\n").unwrap().wire,
            Some(Wire::OpenAiChatCompletions)
        );
        assert!(entry("wire = \"chat\"\n").is_err());
        let omitted = entry("").unwrap();
        assert_eq!(omitted.wire, None);
        // A configuration without the key keeps the revision it had.
        assert!(!serde_json::to_string(&omitted).unwrap().contains("wire"));
        let anthropic = |ingress: &str| {
            toml::from_str::<UpstreamConfig>(&format!(
                "id = \"k\"\nprovider = \"anthropic\"\nbase_url = \"https://api.anthropic.com\"\noperator_key_env = \"K\"\n{ingress}"
            ))
        };
        let omitted_ingress = anthropic("").unwrap();
        assert_eq!(omitted_ingress.ingress, EntryIngress::FallbackOnly);
        assert!(
            !serde_json::to_string(&omitted_ingress)
                .unwrap()
                .contains("ingress")
        );
        assert_eq!(
            anthropic("ingress = \"direct\"\n").unwrap().ingress,
            EntryIngress::Direct
        );
        assert!(anthropic("ingress = \"public\"\n").is_err());
    }
}
