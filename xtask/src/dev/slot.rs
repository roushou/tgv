//! The dev slot: this checkout's build running beside the installed service,
//! with its own configuration, data, and loopback ports, pointed only at the
//! fake provider. Each process runs as a transient systemd user unit named
//! `ren-dev-*`, so it is stopped by name and never by a pattern that could
//! match the installed service.

use super::DevError;
use crate::{
    daemon::{Busy, LiveRun, RunPolicy, Runs},
    install::{InstallError, InstallVps, TailscaleCli},
    invocation::{Echo, Invocation},
    tailscale::{Route, ServeStatus},
    ui::UiDeploy,
};
use ren_daemon::config::{Config, RunsOptions, ToolsOptions, UpstreamConfig};
use ren_gateway::{
    ControlToken, EntryIngress, Environment, InstanceOptions,
    user::{TokenDigest, UserConfig},
};
use ren_harness::{endpoint::UserToken, host::HostEnv};
use ren_protocol::route::{Provider, Wire};
use ren_recorder::{
    caller::{TailscaleLogin, UserId},
    routing::UpstreamId,
};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

/// One process of the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Provider,
    Daemon,
    Ui,
    Traffic,
}

impl Unit {
    /// Every unit, in the order they stop.
    pub const ALL: [Self; 4] = [Self::Traffic, Self::Ui, Self::Daemon, Self::Provider];

    /// The transient unit's name; every one starts with `ren-dev-`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Provider => "ren-dev-provider",
            Self::Daemon => "ren-dev-daemon",
            Self::Ui => "ren-dev-ui",
            Self::Traffic => "ren-dev-traffic",
        }
    }

    fn service(self) -> String {
        format!("{}.service", self.name())
    }

    /// Which of the unit's processes systemd signals when it stops. The
    /// daemon is signalled alone, as the installed unit does, so it drains
    /// its runs itself before the final kill covers what is left; the rest
    /// stop as a whole.
    fn kill_mode(self) -> &'static str {
        match self {
            Self::Daemon => "mixed",
            Self::Provider | Self::Ui | Self::Traffic => "control-group",
        }
    }

    pub fn active(self) -> Result<bool, InstallError> {
        Invocation::new("systemctl")
            .args(["--user", "is-active", "--quiet"])
            .arg(self.service())
            .succeeds(Echo::Hide)
    }

    pub fn stop(self) -> Result<(), InstallError> {
        if self.active()? {
            Invocation::new("systemctl")
                .args(["--user", "stop"])
                .arg(self.service())
                .run()?;
        }
        // A failed transient unit stays loaded until reset.
        Invocation::new("systemctl")
            .args(["--user", "reset-failed"])
            .arg(self.service())
            .succeeds(Echo::Hide)?;
        Ok(())
    }

    /// Run `command` as this unit, with `environment`, until stopped.
    pub fn start(
        self,
        dir: Option<&Path>,
        command: &[&Path],
        args: &[String],
        environment: &[(&str, String)],
    ) -> Result<(), InstallError> {
        let mut run = Invocation::new("systemd-run")
            .args(["--user", "--collect", "--quiet"])
            .arg(format!("--unit={}", self.name()))
            .arg(format!("--description=Ren dev slot: {}", self.name()))
            .arg(format!("--property=KillMode={}", self.kill_mode()));
        if let Some(dir) = dir {
            run = run.arg(format!("--working-directory={}", dir.display()));
        }
        for (name, value) in environment {
            run = run.arg(format!("--setenv={name}={value}"));
        }
        run.arg("--").args(command).args(args).run()
    }
}

/// How `dev up` restarts a dev daemon that drives runs: it waits up to
/// `timeout` seconds for runs mid-way, unless `policy` drains them.
#[derive(Debug, Clone, Copy)]
pub struct RunWait {
    pub policy: RunPolicy,
    pub timeout: u64,
}

/// Where the dev preview is reached.
#[derive(Debug, Clone, Copy)]
pub enum Preview {
    /// Published on the tailnet with Tailscale Serve.
    Tailnet(TailscaleCli),
    /// Only on this host's loopback.
    Local,
}

/// Where the slot lives and listens.
pub struct Slot {
    pub repo: PathBuf,
    cargo: PathBuf,
    pub config: PathBuf,
    pub data_dir: PathBuf,
    pub bin_dir: PathBuf,
    home: PathBuf,
    ui_dir: PathBuf,
    ui_next: PathBuf,
    /// The dev daemon's run root, with a few fake projects to start runs in.
    pub work_dir: PathBuf,
    /// Its other place, with a few fake folders, like `~/.config`.
    places_dir: PathBuf,
    /// Where its scratch runs get their folders.
    scratch_dir: PathBuf,
}

impl Slot {
    pub const PROVIDER: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 18999);
    pub const DAEMON: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 18787);
    pub const UI: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 13000);
    /// The tailnet HTTPS ports of the dev preview, beside the installed
    /// service's.
    pub const GATEWAY_PORT: u16 = 9443;
    pub const UI_PORT: u16 = 9444;
    /// The traffic generator's control API, which the dev UI's toolbar
    /// reaches through the UI server; never published.
    pub const CONTROL: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 13001);
    /// The operator key the dev config's operator-key entries read: fake.
    const OPERATOR_KEY_ENV: &'static str = "REN_DEV_OPERATOR_KEY";
    const OPERATOR_KEY: &'static str = "fake-dev-operator-key";
    /// Seconds between checks for runs mid-way.
    const RUN_POLL: Duration = Duration::from_secs(2);

    pub fn new(env: &HostEnv) -> Result<Self, DevError> {
        let home = env
            .absolute_path("HOME")
            .ok_or(DevError::Command(InstallError::MissingEnv("HOME")))?;
        let share = home.join(".local/share/ren-dev");
        Ok(Self {
            repo: Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
            cargo: env
                .absolute_path("CARGO")
                .unwrap_or_else(|| PathBuf::from("cargo")),
            config: home.join(".config/ren-dev/ren.toml"),
            data_dir: home.join(".local/state/ren-dev"),
            bin_dir: share.join("bin"),
            ui_dir: share.join("ui"),
            ui_next: share.join("ui.next"),
            work_dir: share.join("work"),
            places_dir: share.join("config"),
            scratch_dir: share.join("scratch"),
            home: home.clone(),
        })
    }

    /// The dev user `ci`'s token. Fixed, since the slot's data is fake and
    /// its gateway reaches only the fake provider.
    pub fn ci_token() -> UserToken {
        UserToken::from_random(
            0x00de_7000_0000_0000_0000_0000_0000_0001,
            0x00de_7000_0000_0000_0000_0000_0000_00c1,
        )
    }

    /// The dev configuration: every provider is the fake one, two named
    /// users, and operator-key entries whose key is fake.
    pub fn config(&self) -> Result<Config, DevError> {
        let fake = |prefix: &str| format!("http://{}/{prefix}", Self::PROVIDER);
        let id = |text: &str| UpstreamId::parse(text).map_err(|_| DevError::Config);
        let user = |text: &str| UserId::parse(text).map_err(|_| DevError::Config);
        let login = TailscaleLogin::parse("alice@example.com").map_err(|_| DevError::Config)?;
        Ok(Config {
            listen: Self::DAEMON,
            data_dir: Some(self.data_dir.clone()),
            anthropic_base_url: fake("anthropic"),
            codex_base_url: fake("codex"),
            openai_base_url: fake("openai"),
            deepseek_base_url: fake("deepseek"),
            users: vec![
                UserConfig {
                    id: user("alice")?,
                    tailscale_logins: vec![login],
                    token_blake3: None,
                    max_concurrent: None,
                },
                UserConfig {
                    id: user("ci")?,
                    tailscale_logins: vec![],
                    token_blake3: Some(TokenDigest::of(&Self::ci_token())),
                    max_concurrent: std::num::NonZeroU32::new(4),
                },
            ],
            upstreams: vec![
                UpstreamConfig {
                    id: id("team-key")?,
                    provider: Provider::Anthropic,
                    wire: None,
                    base_url: fake("anthropic"),
                    operator_key_env: Self::OPERATOR_KEY_ENV.into(),
                    ingress: EntryIngress::Direct,
                    users: Some(vec![user("ci")?]),
                },
                UpstreamConfig {
                    id: id("chat-key")?,
                    provider: Provider::OpenAi,
                    wire: Some(Wire::OpenAiChatCompletions),
                    base_url: fake("openai"),
                    operator_key_env: Self::OPERATOR_KEY_ENV.into(),
                    ingress: EntryIngress::Direct,
                    users: None,
                },
            ],
            instance: InstanceOptions {
                environment: Environment::Development,
            },
            runs: RunsOptions {
                roots: vec![self.work_dir.clone()],
                places: vec![self.places_dir.clone()],
                scratch: Some(self.scratch_dir.clone()),
            },
            tools: toml::from_str::<ToolsOptions>(Self::TOOLS).map_err(|_| DevError::Config)?,
            ..Config::default()
        })
    }

    /// An operator catalog entry, so the Tools view shows one. Its download
    /// is invented and never answers, so installing it fails at fetch.
    const TOOLS: &'static str = r#"
[[package]]
id = "acme-lint"
kind = "utility"
program = "acme-lint"

[package.release]
version = "0.3.0"

[[package.release.artifact]]
platform = "linux_x64"
url = "https://example.invalid/acme-lint-0.3.0.tar.gz"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
format = "tar_gz"
program_at = "acme-lint"
"#;

    fn step(message: &str) {
        InstallVps::step(message);
    }

    /// Build this checkout and copy the binaries the units run, so a later
    /// build never replaces one under a running unit.
    fn build(&self) -> Result<(), DevError> {
        Self::step("Building ren and xtask from this checkout");
        Invocation::new(&self.cargo)
            .current_dir(&self.repo)
            .args(["build", "--locked", "-p", "ren-rs", "-p", "xtask"])
            .run()?;
        fs::create_dir_all(&self.bin_dir).map_err(InstallError::file(&self.bin_dir))?;
        for name in ["ren", "xtask"] {
            let built = self.repo.join("target/debug").join(name);
            let contents = fs::read(&built).map_err(InstallError::file(&built))?;
            InstallVps::write_private(&self.bin_dir.join(name), &contents, 0o755)?;
        }
        self.write_agent()
    }

    /// The fake projects runs start in.
    pub const PROJECTS: [&'static str; 2] = ["acme-api", "acme-web"];

    /// The fake folders of its other place.
    const PLACES: [&'static str; 3] = ["nvim", "acme-cli", ".hidden-tool"];

    /// The folder holding the `codex`, `claude`, and `pi` the dev daemon starts:
    /// the fake agent.
    fn agent_dir(&self) -> PathBuf {
        self.bin_dir.join("agent")
    }

    /// Put the fake agent first on the dev daemon's PATH as `codex`,
    /// `claude`, and `pi`, and create the fake projects under the run root.
    fn write_agent(&self) -> Result<(), DevError> {
        let dir = self.agent_dir();
        fs::create_dir_all(&dir).map_err(InstallError::file(&dir))?;
        for (program, client, name) in [
            ("codex", "codex", "Codex"),
            ("claude", "claude", "Claude Code"),
            ("pi", "pi", "Pi"),
            ("opencode", "opencode", "OpenCode"),
        ] {
            let script = format!(
                "#!/bin/sh\n# The dev slot's fake {name} (`cargo xtask dev agent`); never the real one.\nexec {} dev agent --client {client} --data-dir {} \"$@\"\n",
                Self::quote(&self.bin_dir.join("xtask")),
                Self::quote(&self.data_dir),
            );
            InstallVps::write_private(&dir.join(program), script.as_bytes(), 0o755)?;
        }
        for place in Self::PLACES {
            let folder = self.places_dir.join(place);
            fs::create_dir_all(&folder).map_err(InstallError::file(&folder))?;
        }
        for project in Self::PROJECTS {
            let folder = self.work_dir.join(project);
            fs::create_dir_all(folder.join("src")).map_err(InstallError::file(&folder))?;
            let readme = folder.join("README.md");
            if !readme.exists() {
                fs::write(
                    &readme,
                    format!("# {project}\n\nA fake project for the dev slot's runs.\n"),
                )
                .map_err(InstallError::file(&readme))?;
            }
        }
        Ok(())
    }

    /// `path` as one single-quoted shell word.
    fn quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }

    fn write_config(&self) -> Result<(), DevError> {
        let config = self.config()?;
        config.check().map_err(|_| DevError::Config)?;
        let text = toml::to_string(&config).map_err(|_| DevError::Config)?;
        let dir = self.config.parent().unwrap_or(Path::new("/"));
        fs::create_dir_all(dir).map_err(InstallError::file(dir))?;
        fs::create_dir_all(&self.data_dir).map_err(InstallError::file(&self.data_dir))?;
        InstallVps::write_private(
            &self.config,
            format!("# Generated by `cargo xtask dev up`; rewritten on every run.\n{text}")
                .as_bytes(),
            0o600,
        )?;
        Ok(())
    }

    /// Whether `address` answers `GET /health` (or `GET /` for the UI)
    /// within a few seconds.
    pub fn wait_for(address: SocketAddr, path: &str) -> Result<(), DevError> {
        for _ in 0..100 {
            if Self::answers(address, path) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err(DevError::NotReady(address))
    }

    fn answers(address: SocketAddr, path: &str) -> bool {
        let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(500))
        else {
            return false;
        };
        let request =
            format!("GET {path} HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\n\r\n");
        let mut head = [0_u8; 12];
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .is_ok()
            && stream.write_all(request.as_bytes()).is_ok()
            && stream.read_exact(&mut head).is_ok()
            && (head.ends_with(b"200") || head.ends_with(b"503"))
    }

    /// Build, configure, and (re)start the provider and daemon.
    pub fn up(&self, wait: RunWait) -> Result<(), DevError> {
        self.start_core(wait)
    }

    /// Start the core, then build and run the UI and, unless `Preview::Local`,
    /// publish the dev gateway and UI on the tailnet.
    pub fn up_with_ui(
        &self,
        env: &HostEnv,
        preview: Preview,
        wait: RunWait,
    ) -> Result<(), DevError> {
        let ui = UiDeploy::new(env, &self.home, &self.repo)?;
        self.start_core(wait)?;
        ui.build_into(&self.ui_next)?;
        Unit::Ui.stop()?;
        if self.ui_dir.exists() {
            fs::remove_dir_all(&self.ui_dir).map_err(InstallError::file(&self.ui_dir))?;
        }
        fs::rename(&self.ui_next, &self.ui_dir).map_err(InstallError::file(&self.ui_dir))?;
        let (public_gateway, origin) = match preview {
            Preview::Local => (
                format!("http://{}", Self::DAEMON),
                format!("http://{}", Self::UI),
            ),
            Preview::Tailnet(tailscale) => {
                let dns = InstallVps::publish(
                    tailscale,
                    Self::GATEWAY_PORT,
                    &format!("http://{}", Self::DAEMON),
                )?
                .ok_or(InstallError::NoDnsName)?;
                (
                    format!("https://{dns}:{}", Self::GATEWAY_PORT),
                    format!("https://{dns}:{}", Self::UI_PORT),
                )
            }
        };
        Self::step(&format!("Starting the dev UI on {}", Self::UI));
        Unit::Ui.start(
            Some(&self.ui_dir),
            &[ui.node()],
            &["build".into()],
            &[
                ("HOST", Self::UI.ip().to_string()),
                ("PORT", Self::UI.port().to_string()),
                ("REN_GATEWAY_URL", format!("http://{}", Self::DAEMON)),
                ("REN_PUBLIC_GATEWAY_URL", public_gateway),
                ("ORIGIN", origin.clone()),
                ("REN_DEV_TRAFFIC_URL", format!("http://{}", Self::CONTROL)),
                (
                    "REN_CONTROL_TOKEN_FILE",
                    self.data_dir.join(ControlToken::FILE).display().to_string(),
                ),
                // A fake project stands in for Ren's checkout: the slot's
                // fake agents never work in the real one.
                (
                    "REN_SOURCE_DIR",
                    self.work_dir.join(Self::PROJECTS[1]).display().to_string(),
                ),
            ],
        )?;
        // The UI's /health relays the dev daemon's.
        Self::wait_for(Self::UI, "/health")?;
        if let Preview::Tailnet(tailscale) = preview {
            InstallVps::publish(tailscale, Self::UI_PORT, &format!("http://{}", Self::UI))?;
        }
        println!("\nDev preview: {origin}\n");
        Ok(())
    }

    fn start_core(&self, wait: RunWait) -> Result<(), DevError> {
        self.build()?;
        self.write_config()?;
        let ending = self.settle_runs(wait)?;
        InstallVps::announce_ending(&ending);
        for unit in [Unit::Daemon, Unit::Provider] {
            unit.stop()?;
        }
        let xtask = self.bin_dir.join("xtask");
        let ren = self.bin_dir.join("ren");
        Self::step(&format!("Starting the fake provider on {}", Self::PROVIDER));
        Unit::Provider.start(
            None,
            &[&xtask],
            &[
                "dev".into(),
                "provider".into(),
                "--listen".into(),
                Self::PROVIDER.to_string(),
                "--settings".into(),
                self.traffic_settings().display().to_string(),
            ],
            &[],
        )?;
        Self::wait_for(Self::PROVIDER, "/anthropic/v1/models")?;
        Self::step(&format!("Starting the dev daemon on {}", Self::DAEMON));
        Unit::Daemon.start(
            None,
            &[&ren],
            &[
                "--config".into(),
                self.config.display().to_string(),
                "serve".into(),
            ],
            &[
                (Self::OPERATOR_KEY_ENV, Self::OPERATOR_KEY.into()),
                // Runs start the fake agent as `codex`, never a real client.
                (
                    "PATH",
                    format!(
                        "{}:/usr/local/bin:/usr/bin:/bin",
                        self.agent_dir().display()
                    ),
                ),
            ],
        )?;
        Self::wait_for(Self::DAEMON, "/health")?;
        InstallVps::remind_ending(&ending);
        self.start_traffic()
    }

    /// Waits until the running dev daemon has no run mid-way (calls are not
    /// waited for: the fake traffic never stops), and returns the runs that
    /// end with it. Polls are counted rather than timed, so this reads no
    /// clock.
    fn settle_runs(&self, wait: RunWait) -> Result<Vec<LiveRun>, DevError> {
        if !Unit::Daemon.active()? {
            return Ok(Vec::new());
        }
        let ren = || {
            Invocation::new(self.bin_dir.join("ren"))
                .arg("--config")
                .arg(&self.config)
        };
        let polls = wait.timeout.div_ceil(Self::RUN_POLL.as_secs()).max(1);
        let mut reported = None;
        for _ in 0..polls {
            let runs = InstallVps::runs(ren())?;
            let busy = Busy::of_runs(&runs, wait.policy);
            if busy.is_quiet() {
                return Ok(runs.ending());
            }
            InstallVps::report_wait(&busy, &mut reported, wait.timeout);
            thread::sleep(Self::RUN_POLL);
        }
        Err(InstallError::Busy {
            busy: reported.unwrap_or_else(|| Busy::of_runs(&Runs::default(), wait.policy)),
            waited: wait.timeout,
        }
        .into())
    }

    /// (Re)start the traffic generator, which follows the settings file.
    pub fn start_traffic(&self) -> Result<(), DevError> {
        Unit::Traffic.stop()?;
        Self::step("Starting the traffic generator");
        Unit::Traffic.start(
            None,
            &[&self.bin_dir.join("xtask")],
            &[
                "dev".into(),
                "traffic".into(),
                "run".into(),
                "--control".into(),
                Self::CONTROL.to_string(),
            ],
            // The user manager gives its units HOME, which the slot reads.
            &[],
        )?;
        Ok(())
    }

    /// The live traffic settings' file.
    pub fn traffic_settings(&self) -> PathBuf {
        self.config.with_file_name("traffic.toml")
    }

    pub fn down() -> Result<(), DevError> {
        for unit in Unit::ALL {
            Self::step(&format!("Stopping {}", unit.name()));
            unit.stop()?;
        }
        Ok(())
    }

    /// Remove the dev preview's Serve routes, each only if it still proxies
    /// to the slot; the installed service's routes are never touched.
    pub fn unpublish(tailscale: TailscaleCli) -> Result<(), DevError> {
        let status = Invocation::new("tailscale")
            .args(["serve", "status", "--json"])
            .capture()?;
        let serve = ServeStatus::parse(&status).map_err(|_| DevError::Config)?;
        for (port, target) in [
            (Self::GATEWAY_PORT, format!("http://{}", Self::DAEMON)),
            (Self::UI_PORT, format!("http://{}", Self::UI)),
        ] {
            match serve.route(port, &target) {
                Route::Target => {
                    Self::step(&format!("Unpublishing port {port}"));
                    tailscale
                        .invocation()
                        .args(["serve", &format!("--https={port}"), "off"])
                        .run()?;
                }
                Route::Free | Route::Funnel { .. } | Route::Other { .. } | Route::TcpForward => {}
            }
        }
        Ok(())
    }

    /// Stop the slot and remove its data. Only the slot's own directory is
    /// removed, and only if it is the one this slot names.
    pub fn wipe(&self) -> Result<(), DevError> {
        Self::down()?;
        if self.data_dir.ends_with(".local/state/ren-dev") && self.data_dir.exists() {
            Self::step(&format!("Removing {}", self.data_dir.display()));
            fs::remove_dir_all(&self.data_dir).map_err(InstallError::file(&self.data_dir))?;
        }
        Ok(())
    }

    pub fn status(&self) -> Result<(), DevError> {
        for unit in Unit::ALL.iter().rev() {
            let state = if unit.active()? { "running" } else { "stopped" };
            println!("{:<18} {state}", unit.name());
        }
        println!("fake provider      http://{}", Self::PROVIDER);
        println!("dev gateway        http://{}", Self::DAEMON);
        println!("dev UI             http://{}", Self::UI);
        println!("traffic control    http://{}", Self::CONTROL);
        println!("config             {}", self.config.display());
        println!("data               {}", self.data_dir.display());
        println!("run root           {}", self.work_dir.display());
        println!(
            "history            {} --config {} history",
            self.bin_dir.join("ren").display(),
            self.config.display()
        );
        println!("logs               journalctl --user -u 'ren-dev-*'");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Slot, Unit};
    use ren_harness::host::HostEnv;

    #[test]
    fn every_unit_is_named_for_the_dev_slot() {
        for unit in Unit::ALL {
            assert!(unit.name().starts_with("ren-dev-"), "{unit:?}");
            assert_ne!(unit.name(), "ren");
            assert_ne!(unit.name(), "ren-ui");
        }
    }

    #[test]
    fn the_dev_config_is_valid_and_reaches_only_the_fake_provider() {
        let slot = Slot::new(&HostEnv::from_pairs([("HOME", "/home/dev")])).unwrap();
        let config = slot.config().unwrap();
        config.check().unwrap();
        assert_eq!(config.tools.package.len(), 1);
        let text = toml::to_string(&config).unwrap();
        for url in [
            &config.anthropic_base_url,
            &config.codex_base_url,
            &config.openai_base_url,
            &config.deepseek_base_url,
        ] {
            assert!(url.starts_with("http://127.0.0.1:18999/"), "{url}");
        }
        assert!(
            config
                .upstreams
                .iter()
                .all(|upstream| upstream.base_url.starts_with("http://127.0.0.1:18999/"))
        );
        assert!(config.listen.ip().is_loopback());
        assert_eq!(
            config.runs.roots,
            [std::path::Path::new("/home/dev/.local/share/ren-dev/work")]
        );
        assert_eq!(
            config.instance.environment,
            ren_gateway::Environment::Development
        );
        assert_eq!(
            slot.data_dir,
            std::path::Path::new("/home/dev/.local/state/ren-dev")
        );
        // The token itself is never written, only its digest.
        assert!(!text.contains(Slot::ci_token().expose()));
    }
}
