//! `ren tools`: the runtimes and utilities Ren installs on this host for its
//! agents, from the catalog built into this Ren. It acts on this host's
//! data directory, beside or without a running daemon.
use crate::context::Context;
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use ren_daemon::{
    host::HostTools,
    tools::{Changed, ToolStore, fetch::ReqwestFetch},
};
use ren_depot::{
    Depot, Surveyed,
    job::{Failure, JobState},
    package::PackageId,
    provision::Provision,
    source::Source,
    toolset::{Change, Edit, Toolset},
    version::ObservedVersion,
};
use ren_store::{Reader, Store};
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
    /// Delete every generation but the current one and those runs still
    /// use, and every install none of them holds.
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
        let data_dir = ctx.data_dir(&config)?;
        let root = data_dir.join(ToolStore::FOLDER);
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
                let edit = Edit {
                    add: args.packages.into_iter().collect(),
                    remove: BTreeSet::new(),
                };
                Self::change(&store, &depot, &edit).await
            }
            ToolsAction::Remove(args) => {
                let edit = Edit {
                    add: BTreeSet::new(),
                    remove: args.packages.into_iter().collect(),
                };
                Self::change(&store, &depot, &edit).await
            }
            ToolsAction::Gc => {
                // Generations runs still use stay. Without a database, no
                // run was ever recorded here.
                let live = if data_dir.join(Store::METADATA_FILE).exists() {
                    Reader::open(&data_dir, 1)
                        .await?
                        .live_toolsets()
                        .await
                        .context("cannot read which generations runs use")?
                } else {
                    Vec::new()
                };
                let collected = store.collect(&live)?;
                for generation in &live {
                    println!("kept generation {generation}: a run uses it");
                }
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

    /// Apply `edit`: install what it adds and make the next generation
    /// current; nothing changes unless every install succeeds.
    async fn change(
        store: &ToolStore<ReqwestFetch>,
        depot: &Depot,
        edit: &Edit,
    ) -> Result<ExitCode> {
        let platform = HostTools::platform()
            .context("Ren installs tools only on Linux x64, Linux arm64, and macOS arm64")?;
        match store.change(depot, platform, edit).await? {
            Changed::Unchanged(current) => {
                println!("nothing to change (generation {})", current.generation());
                Ok(ExitCode::SUCCESS)
            }
            Changed::Failed { current, jobs } => {
                for (install, state) in jobs {
                    let name = format!("{} {}", install.package, install.version);
                    match state {
                        JobState::Installed => println!("installed {name}"),
                        JobState::Failed { stage, failure } => {
                            eprintln!("{name}: {stage} failed: {}", Self::describe(failure));
                        }
                        JobState::Interrupted { stage } => {
                            eprintln!("{name}: interrupted during {stage}");
                        }
                        JobState::Fetching | JobState::Unpacking | JobState::Placing => {
                            eprintln!("{name}: the install stopped unfinished");
                        }
                    }
                }
                bail!(
                    "nothing changed; generation {} is still current",
                    current.generation()
                )
            }
            Changed::Activated { toolset, changes } => {
                for change in changes {
                    match change {
                        Change::Add(install) => {
                            println!("+ {} {}", install.package, install.version);
                        }
                        Change::Replace { from, to } => {
                            println!("~ {} {} -> {}", to.package, from.version, to.version);
                        }
                        Change::Remove(install) => {
                            println!("- {} {}", install.package, install.version);
                        }
                    }
                }
                println!(
                    "generation {} is current: {}",
                    toolset.generation(),
                    store.bin(toolset.generation()).display()
                );
                Ok(ExitCode::SUCCESS)
            }
        }
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
