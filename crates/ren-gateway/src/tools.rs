//! The tools Ren installs for its agents, as the control API serves them
//! (`/control/v1/tools`). The gateway declares the [`Toolbox`] port and the
//! views it answers with; the daemon implements it over its tool store.

use futures_util::future::BoxFuture;
use ren_depot::{
    job::Stage,
    package::{Kind, Origin, PackageId, ProgramName},
    source::Platform,
    toolset::{Edit, Generation, PlanError},
};
use ren_harness::Harness;
use serde::Serialize;
use serde_with::{DeserializeFromStr, SerializeDisplay};
use strum::{Display, EnumString, IntoStaticStr, VariantArray};

/// What the control API needs of the host's tools: a listing, a change,
/// and a collection. The daemon implements it; tests substitute it.
pub trait Toolbox: Send + Sync + 'static {
    /// Every package, what this host has of it, and the generations.
    fn listing(&self) -> BoxFuture<'static, Result<ToolListing, ToolboxError>>;

    /// Apply `edit` to the current generation, installing what it adds.
    fn change(&self, edit: Edit) -> BoxFuture<'static, Result<ToolChange, ToolboxError>>;

    /// Make the next generation hold what a kept `generation` held.
    fn roll_back(
        &self,
        generation: Generation,
    ) -> BoxFuture<'static, Result<ToolChange, ToolboxError>>;

    /// Remove generations and installs nothing current or running uses.
    fn collect(&self) -> BoxFuture<'static, Result<ToolCollection, ToolboxError>>;
}

/// Why a tools request was not carried out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolboxError {
    #[error("another change to the tools is in progress")]
    Busy,
    #[error("Ren installs tools only on Linux x64, Linux arm64, and macOS arm64")]
    Unsupported,
    #[error(transparent)]
    Refused(#[from] PlanError),
    #[error("generation {0} is not kept")]
    NoGeneration(Generation),
    #[error("generation {0}'s installs were removed")]
    Collected(Generation),
    #[error("the tool store failed: {0}")]
    Failed(String),
}

/// Every package Ren knows, what this host has of each, and the toolset
/// generations kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolListing {
    /// The platform Ren installs for here; `None` where it installs nothing.
    pub platform: Option<Platform>,
    /// The current generation; 0 before any.
    pub generation: Generation,
    /// The current generation's folder of programs, which runs put first on
    /// their search path; `None` before any.
    pub bin: Option<String>,
    pub packages: Vec<ToolPackage>,
    /// Every generation kept, newest first.
    pub generations: Vec<GenerationView>,
}

/// One package: one the catalog names, or one the current generation
/// holds that the catalog no longer names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolPackage {
    pub id: PackageId,
    /// Which catalog names it; `None` once it was taken out of the
    /// operator's configuration and only the install is left.
    pub origin: Option<Origin>,
    /// What it is for; `None` when the catalog no longer names it.
    pub kind: Option<Kind>,
    /// The program it provides (`rg` for `ripgrep`).
    pub program: ProgramName,
    /// The release Ren installs; `None` for a package it only finds.
    pub release: Option<semver::Version>,
    /// What the current generation holds of it.
    pub installed: Option<InstalledTool>,
    /// What this host's search path has.
    pub system: Option<SystemTool>,
    /// The clients that need it.
    pub needed_by: Vec<Harness>,
}

/// A package's install in the current generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledTool {
    pub version: semver::Version,
    pub sha256: String,
    /// Its folder in the store.
    pub dir: String,
}

/// A package's program on the host's search path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SystemTool {
    pub path: String,
    /// The version it reported, when Ren could read one.
    pub version: Option<semver::Version>,
    /// What it printed instead, or why it did not answer.
    pub unread: Option<String>,
}

/// Where a generation stands.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum GenerationState {
    /// New runs start with it.
    Current,
    /// Runs without an end started with it; collection keeps it.
    InUse,
    /// Nothing uses it; collection removes it.
    Idle,
}

/// A generation kept in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GenerationView {
    pub generation: Generation,
    pub state: GenerationState,
    pub installs: Vec<GenerationInstall>,
}

/// One install a generation holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GenerationInstall {
    pub package: PackageId,
    pub version: semver::Version,
}

/// How a change ended.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum ChangeOutcome {
    /// The current generation already held what was asked.
    Unchanged,
    /// Every install succeeded and the next generation is current.
    Activated,
    /// An install failed; the current generation stays.
    Failed,
}

/// What a change did to one package.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum ChangeKind {
    Add,
    Replace,
    Remove,
}

/// A change's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolChange {
    pub outcome: ChangeOutcome,
    /// The current generation after the change.
    pub generation: Generation,
    pub changes: Vec<PackageChange>,
    pub failures: Vec<InstallFailure>,
}

/// One package's change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageChange {
    pub change: ChangeKind,
    pub package: PackageId,
    pub from: Option<semver::Version>,
    pub to: Option<semver::Version>,
}

/// An install that did not succeed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallFailure {
    pub package: PackageId,
    pub version: semver::Version,
    pub stage: Stage,
    pub reason: String,
}

/// What a collection removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolCollection {
    pub generations: Vec<Generation>,
    pub installs: Vec<String>,
}
