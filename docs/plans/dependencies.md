# Plan: dependencies

Ren manages the tools its agents run with: external ones (Node.js, Bun, jq, fd,
and the clients themselves) and Ren's own packages. This plan is the design
and the order it is built in. `ren-depot` holds the built part; each phase
below says whether it is built. Record every deviation here, with its reason.

## Decisions

- **Ren's own manager.** No mise, asdf, or OS package manager as a backend:
  Ren has to own which tools each run used, the generations it pins, and
  their record. Their registries may be read for ideas.
- **The catalog ships with Ren.** Each release carries its built-in catalog,
  versioned with the binary. The operator can add and override entries in
  `ren.toml` (not built). The catalog is never fetched at runtime, which
  would be a second update channel to trust.
- **Platforms:** linux-x64, linux-arm64, and macOS arm64.
- **Managed first.** A run's search path puts the active toolset before the
  system's (`PathPolicy::ManagedFirst`); the policy is recorded with the run.
- **Self-updating clients are adopted, not managed.** Claude Code and Codex
  replace their own binaries, which an immutable install cannot allow. Ren
  reports them as `System` and manages runtimes and utilities first.
- **Verification.** Every release archive has a pinned SHA-256 per platform.
  A signature (minisign, GPG, or a GitHub attestation) is checked where the
  source publishes one. No installer scripts: archives only.

## Model

- `Package`: a catalog entry: `PackageId`, `Kind`, the program it provides,
  and how its version is read. Built.
- `Requirement`: a package and a `semver` range; `Harness::requirements`
  states each client's. Built.
- `ObservedVersion` and `Provision`: what a host has (`Missing`, or
  `System` with its path and version, which keeps output it cannot read) and
  whether it fits (`Fit`: fits, outside, unverified, missing). Built. Phase 2
  adds `Managed(Install)`.
- `Source`: a closed enum. Built: `System` (adopted, never installed) and
  `Release` (a version with one `Artifact` per `Platform`: an https URL, a
  pinned `Sha256`, an `ArchiveFormat`, and where the program is inside).
  Planned: `Embedded` (inside the `ren` binary, as the Pi extension is) and
  `Build { repo, rev, recipe }` for Ren's own packages, added when first used.
- `Install`: an immutable, content-addressed folder,
  `<id>/<version>-<first 16 digits of its SHA-256>` under the store. Built
  (the type and its paths; the store on disk is not).
- `Toolset` generation: a numbered set of installs exposed as one `bin/` of
  links (`Toolset::links`), switched atomically. `Toolset::plan` turns it
  into what the operator wants as a `Plan` (add, replace, remove, and the
  artifacts to fetch), and `Plan::apply` makes the next generation, refusing
  a plan made from an older one. `Toolset::collectable` lists installs no
  live generation holds. Built as pure types; switching on disk, and a run
  recording its generation as a call records its configuration revision, are
  not.
- `InstallJob`: a pure state machine: fetching, unpacking (after
  verification), placing, then `Installed`, `Failed { stage, failure }`, or
  `Interrupted { stage }`; an illegal step is a typed error. Built; the shell
  that carries out its actions is not.

## Deviations

- **No separate `Lock`.** The catalog pins one exact release per package for
  each Ren version, so a toolset's installs already are the resolved,
  digest-pinned versions. `Requirement` ranges are checked against adopted
  system tools (`Fit`).
- **No tar.xz.** Every release planned so far (node, bun, jq, fd, ripgrep)
  is published as tar.gz, zip, or a bare binary, so `ArchiveFormat` has those
  three; tar.xz is added when a source needs it.
- **`Fetch` is a seam inside the daemon, not a port in the depot.** The
  depot's job only names the URL to fetch and takes the digest the shell
  computed, so it needs no download capability; the daemon's store is the
  only consumer, and tests substitute it there.
- **The catalog is TOML data** (`catalog.toml`), read once by
  `Depot::builtin`, rather than Rust tables.
- **Digests, not signatures.** Node.js's digests come from its
  `SHASUMS256.txt` over https; its GPG signature (`SHASUMS256.txt.asc`) is
  not checked. fd publishes no checksum file, so its digests are GitHub's
  recorded asset digests. The others come from the publisher's checksum
  file, cross-checked against GitHub's digests. Each digest is pinned in the
  catalog, so a download is checked against what was reviewed, not against
  what the server says at install time.
- **Bun's Linux x64 artifact is the baseline build**, which runs on CPUs
  without AVX2, since the catalog cannot choose per CPU.
- **Two writers, one lock.** `ren tools` on the host and the daemon's
  `/control/v1/tools` both change the store; a lock file serializes them
  (`tools_busy` for the second), and each change applies an add/remove
  `Edit` to whichever generation is current under the lock. `ren tools`
  does not go through a remote gateway's API yet.
- **`ClientStatus` keeps its presence check** (`AgentHost::finds`) instead of
  coming from `Provision`. Reading versions runs each client, which would
  stall the supervisor's task on every clients request. Once runs get a
  toolset's `bin` on their search path, presence covers managed tools too.

## Phases

1. **Observe.** Built. `ren-depot` with the catalog, requirements, version
   reading, and `Provision`; the daemon's `HostTools` implements `ToolHost`;
   `ren doctor` reports every package with its version. Not yet: the
   supervisor's `ClientStatus` still checks presence only
   (`AgentHost::finds`), and `ProcessHost` still reads `PATH` itself.
2. **Store and generations.** Built: the pure core (`source`, `toolset`,
   `job`) and the daemon's `tools::ToolStore`, which carries out install jobs
   (download through `Fetch`, verify, unpack, place) and activates, restores,
   and collects generations under `<data>/tools`. Runs get the current
   generation's `bin` first on their `PATH` (`AgentHost::tools`,
   `RunTools`) and record it (`runs.toolset_generation`, migration 0011;
   `None` when the store was unreadable); `ren tools gc` keeps generations
   runs without an end use (`Reader::live_toolsets`). Verified in the dev
   slot: a fake codex run's process had the generation's `bin` first on its
   `PATH`, and `/api/runs` recorded the generation.
3. **Catalog sources.** Built. `crates/ren-depot/src/catalog.toml` pins
   node 26.13.1, bun 1.4.4, jq 1.8.2, fd 10.5.2, and ripgrep 15.2.0 for
   each platform; `Depot::builtin` reads it. Each was installed for real on
   Linux x64 while building it; the other platforms' entries are unverified
   beyond their digests.
4. **Control API and CLI.** Built: `/control/v1/tools` (listing, change,
   collection) through the gateway's `Toolbox` port, implemented by the
   daemon's `HostToolbox`, which `ren tools list|install|remove|gc` uses too,
   so both print the same JSON. Not built: rollback to an older generation,
   and `which`.
5. **Tools view.** Built (`/tools`): one flush table grouped by kind
   (standing, Ren's release, the install, what `PATH` has, with each row's
   one change), changes staged in a plan bar and applied as one generation,
   and an inspector with the release, its digest and folder, what `PATH`
   has, the clients that need it, and the generations kept with
   "Remove unused". Verified in the dev slot (install, removal, collection
   keeping a live run's generation, and a run's agent getting the
   generation's `bin` first on its `PATH`). Not built: rollback, and the
   runs that used each generation.
6. **Ren's own packages and project requirements.** Not built. `Build`
   sources, and requirements a project declares.
