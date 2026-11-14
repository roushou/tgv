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
- `Source`: a closed enum: `Release { url, checksums }`, `Embedded` (inside
  the `ren` binary, as the Pi extension is), `Build { repo, rev, recipe }`
  for Ren's own packages, and `System`, adopted rather than installed.
- `Lock`: each requirement resolved to an exact version and digest per
  platform.
- `Install`: an immutable, content-addressed folder,
  `<data>/tools/<id>/<version>-<digest>/`.
- `Toolset` generation: a numbered set of installs exposed as one `bin/` of
  links, switched atomically. A run records its generation as a call records
  its configuration revision. Collection removes only installs no live
  generation and no live run uses.
- `InstallJob`: a persisted, pure state machine: resolving, fetching,
  verifying, unpacking, activating, then `Installed`, `Failed { stage }`, or
  `Interrupted`.

## Phases

1. **Observe.** Built. `ren-depot` with the catalog, requirements, version
   reading, and `Provision`; the daemon's `HostTools` implements `ToolHost`;
   `ren doctor` reports every package with its version. Not yet: the
   supervisor's `ClientStatus` still checks presence only
   (`AgentHost::finds`), and `ProcessHost` still reads `PATH` itself.
2. **Store and generations.** Not built. The `Fetch` and `DepotHost` ports
   (implemented in `ren-daemon`), the store layout, `Lock`, generations, and
   collection; runs record their generation (a `ren-store` migration);
   `ClientStatus` and the clients listing come from `Provision`.
3. **Catalog sources.** Not built. Release entries for node, bun, jq, fd,
   and ripgrep with pinned checksums per platform.
4. **Control API and CLI.** Not built. `/control/v1/tools` (plan, apply,
   rollback, collect) and `ren tools list|plan|apply|rollback|gc|which`,
   printing the same report as doctor.
5. **Tools view.** Not built. One flush table grouped by kind (wanted,
   locked, source, used by, status), changes staged in a plan bar and
   applied as a new generation, and an inspector with provenance,
   generations, rollback, and the runs that used each. Verified in the dev
   slot before it is installed.
6. **Ren's own packages and project requirements.** Not built. `Build`
   sources, and requirements a project declares.
