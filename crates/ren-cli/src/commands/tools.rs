//! `ren tools`: the runtimes and utilities Ren installs on this host for its
//! agents, from the catalog built into this Ren. It acts on this host's
//! data directory, beside or without a running daemon.
use crate::context::Context;
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use ren_daemon::{
    host::HostTools,
    tools::{ToolStore, fetch::ReqwestFetch},
};
use ren_depot::{
    Depot, Surveyed,
    job::{Failure, JobState},
    package::PackageId,
    provision::Provision,
    source::Source,
    toolset::{Change, Toolset},
    version::ObservedVersion,
};
use serde::Serialize;
use std::{collections::BTreeSet, process::ExitCode};

/// Install runtimes and utilities for Ren's agents on this host.
#[derive(Args)]
pub struct ToolsCmd {
    #[command(subcommand)]
    action: ToolsAction,
}

#[derive(Subcommand)]
enum ToolsAction {
    /// Every package Ren knows: what this host's PATH has, and what Ren
    /// installed.
    List(ListArgs),
    /// Download, verify, and install packages as a new generation.
    Install(Packages),
    /// Leave packages out of a new generation. Their files stay until `gc`.
    Remove(Packages),
    /// Delete every generation but the current one, and every install it
    /// does not hold.
    Gc,
}

#[derive(Args)]
struct ListArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct Packages {
    /// Package names, as `ren tools list` shows them.
    #[arg(required = true)]
    packages: Vec<PackageId>,
}

/// One row of `ren tools list --json`.
#[derive(Serialize)]
struct Row {
    id: PackageId,
    kind: ren_depot::package::Kind,
    /// The version Ren installs, when it installs this package.
    release: Option<String>,
    /// The version in the current generation.
    installed: Option<String>,
    /// What this host's PATH has.
    path: Option<String>,
    path_version: Option<String>,
}

#[derive(Serialize)]
struct Listing {
    generation: u64,
    packages: Vec<Row>,
}

impl ToolsCmd {
    pub async fn run(self, ctx: &Context) -> Result<ExitCode> {
        let depot = Depot::builtin()?;
        let config = ctx.config()?;
        let root = ctx
            .data_dir(&config)?
            .join(ToolStore::<ReqwestFetch>::FOLDER);
        let store = ToolStore::new(root, ReqwestFetch::new().context("cannot start downloads")?);
        let current = store.current()?;
        match self.action {
            ToolsAction::List(args) => {
                let survey = depot.survey(&HostTools::new(ctx.env.search_path()));
                let listing = Self::listing(&survey, &current);
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&listing)?);
                } else {
                    print!("{}", Self::table(&listing));
                }
                Ok(ExitCode::SUCCESS)
            }
            ToolsAction::Install(args) => {
                let mut wanted: BTreeSet<PackageId> = current
                    .installs()
                    .map(|install| install.package.clone())
                    .collect();
                wanted.extend(args.packages);
                Self::change(&store, &depot, &current, &wanted).await
            }
            ToolsAction::Remove(args) => {
                let mut wanted: BTreeSet<PackageId> = current
                    .installs()
                    .map(|install| install.package.clone())
                    .collect();
                for package in &args.packages {
                    if !wanted.remove(package) {
                        bail!("{package} is not installed by Ren");
                    }
                }
                Self::change(&store, &depot, &current, &wanted).await
            }
            ToolsAction::Gc => {
                let collected = store.collect(&[])?;
                for generation in &collected.generations {
                    println!("removed generation {generation}");
                }
                for install in &collected.installs {
                    println!("removed {install}");
                }
                if collected.generations.is_empty() && collected.installs.is_empty() {
                    println!("nothing to remove");
                }
                Ok(ExitCode::SUCCESS)
            }
        }
    }

    /// Install what `wanted` adds and make it the next generation; nothing
    /// changes unless every install succeeds.
    async fn change(
        store: &ToolStore<ReqwestFetch>,
        depot: &Depot,
        current: &Toolset,
        wanted: &BTreeSet<PackageId>,
    ) -> Result<ExitCode> {
        let platform = HostTools::platform()
            .context("Ren installs tools only on Linux x64, Linux arm64, and macOS arm64")?;
        let plan = current.plan(depot, platform, wanted)?;
        if plan.is_empty() {
            println!("nothing to change (generation {})", current.generation());
            return Ok(ExitCode::SUCCESS);
        }
        let mut failed = false;
        for (install, state) in store.install_plan(&plan).await {
            let name = format!("{} {}", install.package, install.version);
            match state {
                JobState::Installed => println!("installed {name}"),
                JobState::Failed { stage, failure } => {
                    failed = true;
                    eprintln!("{name}: {stage} failed: {}", Self::describe(failure));
                }
                JobState::Interrupted { stage } => {
                    failed = true;
                    eprintln!("{name}: interrupted during {stage}");
                }
                JobState::Fetching | JobState::Unpacking | JobState::Placing => {
                    failed = true;
                    eprintln!("{name}: the install stopped unfinished");
                }
            }
        }
        if failed {
            bail!(
                "nothing changed; generation {} is still current",
                current.generation()
            );
        }
        for change in plan.changes() {
            match change {
                Change::Add(install) => println!("+ {} {}", install.package, install.version),
                Change::Replace { from, to } => {
                    println!("~ {} {} -> {}", to.package, from.version, to.version);
                }
                Change::Remove(install) => println!("- {} {}", install.package, install.version),
            }
        }
        let next = store.activate(plan)?;
        println!(
            "generation {} is current: {}",
            next.generation(),
            store.bin(next.generation()).display()
        );
        Ok(ExitCode::SUCCESS)
    }

    fn describe(failure: Failure) -> String {
        match failure {
            Failure::Mismatch { actual } => {
                format!("the download's SHA-256 is {actual}, not the one the catalog pins")
            }
            Failure::ProgramMissing => "the archive has no program where the catalog says".into(),
            Failure::Host(failure) => failure.to_string().replace('_', " "),
        }
    }

    fn listing(survey: &[Surveyed], current: &Toolset) -> Listing {
        let packages = survey
            .iter()
            .map(|Surveyed { package, provision }| {
                let (path, path_version) = match provision {
                    Provision::Missing => (None, None),
                    Provision::System(found) => (
                        Some(found.path.display().to_string()),
                        match &found.version {
                            ObservedVersion::Read(version) => Some(version.to_string()),
                            ObservedVersion::Unparsed(raw) => Some(raw.to_string()),
                            ObservedVersion::Unread(failure) => Some(format!("({failure})")),
                        },
                    ),
                };
                Row {
                    id: package.id().clone(),
                    kind: package.kind(),
                    release: match package.source() {
                        Source::System => None,
                        Source::Release(release) => Some(release.version.to_string()),
                    },
                    installed: current
                        .install(package.id())
                        .map(|install| install.version.to_string()),
                    path,
                    path_version,
                }
            })
            .collect();
        Listing {
            generation: current.generation().number(),
            packages,
        }
    }

    fn table(listing: &Listing) -> String {
        let dash = || "-".to_owned();
        let rows: Vec<[String; 5]> = listing
            .packages
            .iter()
            .map(|row| {
                [
                    row.id.to_string(),
                    row.kind.to_string(),
                    row.release.clone().unwrap_or_else(|| "system only".into()),
                    row.installed.clone().unwrap_or_else(dash),
                    match (&row.path, &row.path_version) {
                        (Some(path), Some(version)) => format!("{version} at {path}"),
                        (Some(path), None) => path.clone(),
                        (None, _) => "not found".into(),
                    },
                ]
            })
            .collect();
        let header = ["PACKAGE", "KIND", "RELEASE", "INSTALLED", "ON PATH"].map(String::from);
        let widths: Vec<usize> = (0..4)
            .map(|column| {
                rows.iter()
                    .chain([&header])
                    .map(|row| row[column].len())
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let mut out = format!("generation {}\n", listing.generation);
        for row in [&header].into_iter().chain(&rows) {
            for (column, width) in widths.iter().enumerate() {
                out.push_str(&format!("{:<width$}  ", row[column]));
            }
            out.push_str(&row[4]);
            out.push('\n');
        }
        out
    }
}
