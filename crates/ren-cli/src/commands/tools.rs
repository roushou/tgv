//! `ren tools`: the runtimes and utilities Ren installs on this host for its
//! agents, from the catalog built into this Ren. It acts on this host's
//! data directory through the same toolbox as the daemon's
//! `/control/v1/tools`, beside or without a running daemon; the store's
//! lock keeps the two from changing it at once.
use crate::context::Context;
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use ren_daemon::{
    host::HostTools,
    tools::{HostToolbox, ToolStore, fetch::ReqwestFetch},
};
use ren_depot::{Depot, package::PackageId, toolset::Edit};
use ren_gateway::tools::{ChangeKind, ChangeOutcome, ToolListing};
use ren_store::{Reader, Store};
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
    /// Print the listing `/control/v1/tools` answers with.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct Packages {
    /// Package names, as `ren tools list` shows them.
    #[arg(required = true)]
    packages: Vec<PackageId>,
}

impl ToolsCmd {
    pub async fn run(self, ctx: &Context) -> Result<ExitCode> {
        let config = ctx.config()?;
        let data_dir = ctx.data_dir(&config)?;
        // Which generations runs use. Without a database, no run was ever
        // recorded here.
        let reader = if data_dir.join(Store::METADATA_FILE).exists() {
            Some(Reader::open(&data_dir, 1).await?)
        } else {
            None
        };
        let toolbox = HostToolbox::new(
            ToolStore::new(
                data_dir.join(ToolStore::FOLDER),
                ReqwestFetch::new().context("cannot start downloads")?,
            ),
            Depot::builtin()?,
            HostTools::new(ctx.env.search_path()),
            HostTools::platform(),
            reader,
        );
        match self.action {
            ToolsAction::List(args) => {
                let listing = toolbox.read_listing().await?;
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&listing)?);
                } else {
                    print!("{}", Self::table(&listing));
                }
                Ok(ExitCode::SUCCESS)
            }
            ToolsAction::Install(args) => {
                Self::change(
                    &toolbox,
                    Edit {
                        add: args.packages.into_iter().collect(),
                        remove: BTreeSet::new(),
                    },
                )
                .await
            }
            ToolsAction::Remove(args) => {
                Self::change(
                    &toolbox,
                    Edit {
                        add: BTreeSet::new(),
                        remove: args.packages.into_iter().collect(),
                    },
                )
                .await
            }
            ToolsAction::Gc => {
                let collected = toolbox.sweep().await?;
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
    async fn change(toolbox: &HostToolbox<ReqwestFetch>, edit: Edit) -> Result<ExitCode> {
        let changed = toolbox.apply(edit).await?;
        match changed.outcome {
            ChangeOutcome::Unchanged => {
                println!("nothing to change (generation {})", changed.generation);
                Ok(ExitCode::SUCCESS)
            }
            ChangeOutcome::Failed => {
                for failure in &changed.failures {
                    eprintln!(
                        "{} {}: {} failed: {}",
                        failure.package, failure.version, failure.stage, failure.reason
                    );
                }
                bail!(
                    "nothing changed; generation {} is still current",
                    changed.generation
                )
            }
            ChangeOutcome::Activated => {
                for change in &changed.changes {
                    let from = change.from.as_ref().map(ToString::to_string);
                    let to = change.to.as_ref().map(ToString::to_string);
                    let (from, to) = (from.unwrap_or_default(), to.unwrap_or_default());
                    match change.change {
                        ChangeKind::Add => println!("+ {} {to}", change.package),
                        ChangeKind::Replace => println!("~ {} {from} -> {to}", change.package),
                        ChangeKind::Remove => println!("- {} {from}", change.package),
                    }
                }
                println!("generation {} is current", changed.generation);
                Ok(ExitCode::SUCCESS)
            }
        }
    }

    fn table(listing: &ToolListing) -> String {
        let rows: Vec<[String; 5]> = listing
            .packages
            .iter()
            .map(|package| {
                [
                    package.id.to_string(),
                    package.kind.to_string(),
                    package
                        .release
                        .as_ref()
                        .map_or_else(|| "system only".to_owned(), ToString::to_string),
                    package
                        .installed
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), |installed| installed.version.to_string()),
                    match &package.system {
                        None => "not found".to_owned(),
                        Some(system) => {
                            let version = system
                                .version
                                .as_ref()
                                .map(ToString::to_string)
                                .or_else(|| system.unread.clone())
                                .unwrap_or_default();
                            format!("{version} at {}", system.path)
                        }
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
