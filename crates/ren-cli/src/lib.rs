//! The `ren` command line, with [`Cli`] as its primary type: arguments and
//! dispatch, where each command owns its argument struct and execution, plus
//! the doctor report and service definitions the commands produce.
pub mod client;
mod commands;
mod context;
pub mod doctor;
pub mod management;
pub mod mcp;
pub mod service;
pub mod style;
pub mod tail;

use crate::context::Context;
use crate::style::{Terminal, Tone};
use anyhow::Result;
use clap::{Parser, Subcommand};
use ren_gateway::build::{Build, Revision};
use ren_harness::host::HostEnv;
use std::{path::PathBuf, process::ExitCode};

#[derive(Parser)]
// The program is `ren`, whatever the package is called: clap would otherwise
// print the package name (`ren-rs`) in `--version`.
#[command(
    name = "ren",
    version = Cli::VERSION,
    about = "Self-hosted gateway and flight recorder for coding agents"
)]
pub struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Override the capture/history directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve(commands::serve::ServeCmd),
    Service(commands::service::ServiceCmd),
    Init(commands::init::InitCmd),
    Doctor(commands::doctor::DoctorCmd),
    Run(commands::run::RunCmd),
    History(commands::history::HistoryCmd),
    Sessions(commands::history::SessionsCmd),
    Timeline(commands::history::TimelineCmd),
    Turns(commands::history::TurnsCmd),
    Search(commands::history::SearchCmd),
    Exposures(commands::history::ExposuresCmd),
    Report(commands::history::ReportCmd),
    Turn(commands::history::TurnCmd),
    Activity(commands::history::ActivityCmd),
    Inspect(commands::history::InspectCmd),
    Tail(commands::tail::TailCmd),
    Control(commands::control::ControlCmd),
    Runs(commands::runs::RunsCmd),
    Users(commands::users::UsersCmd),
    Tools(commands::tools::ToolsCmd),
    Mcp(commands::mcp::McpCmd),
}

impl Cli {
    /// `--version`: the package version and the revision build.rs recorded.
    const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("REN_REVISION"), ")");

    /// What this binary was compiled from.
    pub fn build() -> Build {
        // build.rs writes only `<commit>`, `<commit>-dirty`, or `unknown`
        // (pinned by a test), so a parse failure cannot happen; it would
        // still read as an unknown revision, never as a known one.
        let revision = env!("REN_REVISION").parse().unwrap_or(Revision::Unknown);
        Build::new(env!("CARGO_PKG_VERSION"), revision)
    }

    /// Runs the command; a failure is reported on stderr and exits 1.
    pub async fn run(self, env: HostEnv, terminal: Terminal) -> ExitCode {
        match self.dispatch(env, terminal).await {
            Ok(code) => code,
            Err(error) => {
                // The same text Rust prints for an error returned from main.
                eprintln!("{} {error:?}", terminal.stderr.paint(Tone::Bad, "Error:"));
                ExitCode::FAILURE
            }
        }
    }

    async fn dispatch(self, env: HostEnv, terminal: Terminal) -> Result<ExitCode> {
        let ctx = Context::new(self.config, self.data_dir, env, terminal);
        match self.command {
            Command::Serve(cmd) => cmd.run(&ctx).await,
            Command::Service(cmd) => cmd.run(&ctx),
            Command::Init(cmd) => cmd.run(&ctx).await,
            Command::Doctor(cmd) => cmd.run(&ctx).await,
            Command::Run(cmd) => cmd.run(&ctx).await,
            Command::History(cmd) => cmd.run(&ctx).await,
            Command::Sessions(cmd) => cmd.run(&ctx).await,
            Command::Timeline(cmd) => cmd.run(&ctx).await,
            Command::Turns(cmd) => cmd.run(&ctx).await,
            Command::Search(cmd) => cmd.run(&ctx).await,
            Command::Exposures(cmd) => cmd.run(&ctx).await,
            Command::Report(cmd) => cmd.run(&ctx).await,
            Command::Turn(cmd) => cmd.run(&ctx).await,
            Command::Activity(cmd) => cmd.run(&ctx).await,
            Command::Inspect(cmd) => cmd.run(&ctx).await,
            Command::Tail(cmd) => cmd.run(&ctx).await,
            Command::Control(cmd) => cmd.run(&ctx).await,
            Command::Runs(cmd) => cmd.run(&ctx).await,
            Command::Users(cmd) => cmd.run(&ctx),
            Command::Tools(cmd) => cmd.run(&ctx).await,
            Command::Mcp(cmd) => cmd.run(&ctx).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recorded_revision_parses() {
        assert!(env!("REN_REVISION").parse::<Revision>().is_ok());
    }
}
