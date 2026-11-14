# Ren contributor guidance

Ren is a self-hosted gateway and flight recorder for coding agents (Claude Code,
Codex, Pi, and OpenCode). It forwards native provider traffic while recording, inspecting,
and exposing it, using the operator's own Anthropic and OpenAI subscriptions. One executable and one storage format serve both
a laptop and a VPS reached through Tailscale.

[`docs/architecture.md`](docs/architecture.md) describes how Ren is built and
lists its known limitations. Section 4 below maps the nine crates and the `xtask` package.

- All code follows the principles (section 2) and the workspace lints.
- Never describe unbuilt functionality as working. Read the source before assuming
  how something behaves.

---

## 1. Product invariants

These describe what Ren must do. They hold through every refactor.

### Forwarding

- Forward native payloads byte for byte. Never reconstruct traffic through a universal
  message schema. Inspection is a separate, read-only observer: observing traffic must
  never change it.
- Apply the required HTTP proxy header rules (hop-by-hop headers, proxy identity
  headers). Preserving payloads does not mean forwarding every header blindly.
- Any deliberate transformation records the original payload, the resulting payload,
  and the reason, according to capture policy.
- Preserve stream ordering, backpressure, cancellation, trailers, and terminal
  events. Network chunks are arbitrary byte fragments, not protocol event boundaries.
- Track upstream acceptance and downstream delivery separately. Never retry inference
  blindly after ambiguous acceptance or partial delivery. Each retry is a distinct
  attempt. No generic retry middleware around inference.
- Every request uses one immutable configuration snapshot. Record the configuration
  revision and routing decision that request used.

### Lifecycle truthfulness

- Response headers are not stream completion. HTTP completion is not model
  completion. Cancellation requested is not cancellation observed.
- Preserve interrupted and unknown outcomes. Never round them up to success.
- Missing telemetry is not evidence of idleness or completion. Token usage,
  subscription allowance, and monetary cost are different measurements.
- Distinguish wire captures, client-exported data, inferred states, and estimates.

### Capture and storage

- Recording never stalls forwarding. Database writes, parsing, and UI consumers stay
  off the forwarding path. Queues are bounded by bytes and by item count, and overload
  behavior is defined.
- Report dropped or incomplete capture through a status path that does not depend on
  the saturated capture queue.
- Store versioned observations with provenance, timestamps, correlation IDs, ordering,
  and completeness. Build query views from retained events. Don't event-source the
  whole application without a concrete need.
- One active daemon owns its storage on its own host. Remote clients use the API,
  never shared SQLite files. Account for migrations, retention, payload/index
  consistency, and recoverable shutdown. On shutdown, stop HTTP before draining capture.
- A remote daemon cannot read a laptop's session files. Collecting client telemetry
  and forwarding traffic are separate integration responsibilities.

### Security and authentication

- Keep four concepts separate: client, wire protocol, upstream provider, and
  authentication. Gateway credentials and provider credentials are separate.
- Clients own subscription sign-in and token refresh. If Ren ever owns refresh,
  serialize refreshes and define ownership explicitly.
- Never record credential headers or token-bearing URLs. Never expose secrets through
  `Debug`, errors, fixtures, logs, or exports. Sanitize before anything is persisted.
- Never spoof client identity. OAuth availability does not prove that every client is
  entitled to a subscription. Check current official provider documentation before
  changing authentication or gateway behavior.
- Ren starts and drives only the official client programs, under the sign-in the
  operator gave each one. It never signs in for a client, never reads or copies a
  client's stored credentials, and never imitates a client (its user agent, system
  prompt, tool names, or originator) to use a subscription.
- Listeners default to loopback. Remote access and management controls are explicit,
  with Tailscale as the preferred private path.

---

## 2. Design principles

Ren follows the shared design principles in `~/dev/principles/design.md` (type-driven
design with a sans-IO core). Read that file before making design decisions. Where
it's unavailable, the Ren-specific rules below still apply.

Ren-specific applications:

- **Identities:** channels, calls, attempts, recordings, turns, and tool calls each get
  their own type (`ren_recorder::id`). Provider-native identifiers keep separate types
  (`NativeSessionId` is not `RecordingId`).
- **Vocabularies:** a closed set of labels is an enum deriving `strum`'s
  `Display`, `EnumString`, `IntoStaticStr`, and `VariantArray` with
  `serialize_all = "snake_case"`, plus `serde_with`'s `SerializeDisplay` and
  `DeserializeFromStr`, so every representation reads the labels from the strum
  attributes (`ren_protocol::label` lists the derives). An irregular label is a
  `#[strum(to_string = "…")]` on its variant. Never add `#[serde(rename…)]`, which
  would be a second table. Every label is pinned in its crate's `tests/fixtures/labels.txt`
  (`tests/labels.rs`); add a new vocabulary there, and treat a changed line as a
  storage format change. Labels read from storage or the wire use `Stored<T>`,
  which keeps unknown text.
- **Keep the unknown:** wire enums for Anthropic Messages, OpenAI Responses, OpenAI
  Chat Completions, and SSE
  events keep an `Unknown(Raw…)` variant. `#[serde(other)]` loses the value, so don't
  use it on wire data.
- **Sans-IO core:** SSE framing, native protocol parsing, inspection, attempt
  lifecycle transitions, retry eligibility (`ren_gateway::retry`), operator
  state (`ren_gateway::operator`), call planning and transforms (`policy`), retention decisions,
  and history query building. Framing
  stays independent of JSON and of providers.
- **Shell:** Axum handlers, the Reqwest transport, SQLx persistence, the clap CLI, and
  daemon wiring. Recording, inspection, and storage stay independent of CLI argument
  parsing and Axum handlers.
- **Capabilities:** components receive `Clock` (`ren_recorder::time`) and
  `IdSource` (`ren_recorder::id`) instead of reading the host clock or generating random IDs. The
  gateway receives an `Upstream` (`ren_gateway::upstream`) and the shared
  `Controls` (`ren_gateway::control`). Environment variables (operator API keys) are read once into
  `HostEnv` (`ren_harness::host`), captured once in `main` and carried by the CLI
  `Context`;
  configuration, the Pi cache, launch, and doctor receive it. The recorder writes
  through its `RecordSink` port (`ren_recorder::RecordSink`), which `Store` implements.
- **Lifecycles** are pure state machines, one per transport (`HttpAttempt` in
  `ren_gateway::attempt`, and `WsChannel` in `ren_gateway::channel`, which owns
  its calls' states), with explicit terminals that include interrupted and unknown outcomes.

### Lints

The strict lints are workspace-wide, in root `Cargo.toml` `[workspace.lints.clippy]`.
They deny, among others, wildcard enum arms and imports, `unwrap`/`expect`/`panic!`
outside tests, `as` conversions, excessive bools, and the ambient calls listed in
`clippy.toml` (host clocks, random UUIDs, environment variables, task spawning,
starting processes).
Modules carry no lint headers of their own. A module with submodules lives in
`name/mod.rs`, never `name.rs` beside a `name/` directory
(`self_named_module_files`).

- The only exemptions are the capability implementations themselves, marked with
  `#[expect(clippy::disallowed_methods, reason = "…")]`: `HostEnv::capture`, the
  daemon's `SystemClock`, `RandomIds`, `ProcessHost` (`ren_daemon::host`, its process
  starts and its program lookup on the agents' `PATH`), and `HostTools` (its
  `--version` probes), and the daemon's server
  and supervisor task spawns. The recorder runs on `ObservationThread` (`ren_daemon::host`), a
  thread with its own runtime.
- Integration-test crates (`crates/*/tests/*.rs`) act as a shell: each has one
  crate-level `#![allow(...)]` with a reason, covering helpers outside `#[test]`
  functions and spawned mock servers. Unit-test modules use `clippy.toml`'s
  `allow-*-in-tests` instead.
- `anyhow` is used only in `ren-cli`, the command shell (`context.rs`,
  `commands`, and `Cli::run`). Every other module returns its own
  `thiserror` enum, including `doctor` and `service` in `ren-cli`.

## 3. Testing

Follow the testing section of the shared principles. Priority scenarios for Ren:
fragmented streams, partial failures, retry decisions, cancellation, capture
saturation, and persistence recovery.

- End-to-end gateway tests use `TestGateway` (`crates/ren-daemon/tests/support`): the
  gateway, recorder, store, and read and control APIs with a fixed clock and sequential IDs,
  forwarding to a loopback mock upstream or a scripted `Upstream`. Full-daemon
  tests (`tests/daemon.rs`) are only for wiring, lifecycle, and shutdown.
- Automated checks never need personal credentials or consume subscription allowance.

---

## 4. Workspace map

This map describes the code as it is: the nine crates, listed with their
dependencies under "Crates" below. Read the code for current module ownership.

- `docs/architecture.md`: how Ren is built, and its known limitations. The other
  `docs/*.md` files are operator guides linked from `README.md`.
- `crates/ren-protocol`: sans-IO native protocol understanding, with
  `CallInspector` as its primary type: SSE framing, Anthropic Messages, OpenAI
  Responses, and OpenAI Chat Completions (`chat`) wire vocabularies,
  incremental decompression, per-call inspection
  (`inspect`, whose `pipeline` also reads WebSocket messages and hands every
  frame to the response's activity reader as it streams), WebSocket message
  classification, and the activity reader (`activity`: a request's newest user
  input, gathered by `RequestReader`, and a response's tool calls with their
  inputs and its reply, read by `ResponseReader`; `Reading` pairs the two).
  It also owns the label machinery (`label`, with `Stored<T>`), the `Provider`,
  `Wire`, `Endpoint`, `Method`, and `Transport` vocabularies (`route`, where
  `Provider::speak` pairs a provider with a wire format it offers as an
  `Api`), `Direction`, and
  the native identifiers and message hints the wire carries (`native`). Its
  manifest allows only pure dependencies, and a test keeps its sources free of
  I/O.
- `crates/ren-depot`: the tools Ren's agents run with, with `Depot` as its
  primary type: the built-in package catalog (`package`: `Package`,
  `PackageId`, `ProgramName`, and `Kind`, client, runtime, or utility), a
  `Requirement` (a package and a `semver` range), a program's version read
  from its own `--version` output (`version`: `ObservedVersion`, which keeps
  output it cannot read and probes that did not answer), and what a host
  provides and whether it fits (`provision`: `Provision`, `Fit`). For
  installing, it has where a package comes from (`source`: `Source`, a
  `Release` with an `Artifact` per `Platform` pinned by `Sha256`), immutable
  installs gathered into numbered generations (`toolset`: `Install`,
  `Toolset`, `Generation`, and the `Plan` from one generation to the next),
  and the pure `InstallJob` machine (`job`). It reaches the host only
  through its `ToolHost` port, which the daemon's `HostTools` implements;
  `ren doctor` surveys the host with it. The daemon's `tools` module carries
  out its jobs and generations, but nothing triggers an install yet: the
  catalog's releases, the control API and CLI, runs' pinned toolsets, and
  the UI are planned in
  `docs/plans/dependencies.md`, where deviations from it are recorded. Its
  manifest allows only pure dependencies, and `src/purity.rs` keeps its
  sources free of I/O.
- `crates/ren-harness`: the harnesses, with `Harness` (`ClaudeCode`, `Codex`, `Pi`, `OpenCode`)
  as its primary type. Each client is one module holding everything Ren
  knows of it: `codex` (the `CodexAppServer` codec for `codex app-server`
  and its model listing, its launch plan, its headers, and `CodexTool`),
  `claude_code` (`ClaudeStreamJson` for `claude -p` in stream-json, and the
  same parts), `pi` (`PiRpc` for `pi --mode rpc`, the bundled extension's
  `PiCache`, and the same parts), and `opencode` (`OpenCodeAcp` for
  `opencode acp`, the Agent Client Protocol, and the same parts). Each fills the shared types the rest of Ren reads,
  and `Harness` dispatches to them by `match`; there is no per-client trait.
  The shared types: attribution (`attribution`: `Attribution` and the hint
  types, with `HeaderReading`, which each client's headers extend, behind
  `Harness::attribution`); launch plans (`launch`: `LaunchPlan`, each client
  a named constructor, behind `Harness::launch`, `Harness::drive`, and
  `Harness::list_models`, and the packages each client needs on its host,
  `Harness::requirements`); what tools and messages mean for activity
  (`tools`: effects, the user context each client adds, exit codes, why a
  client sent a call (`CallPurpose`: work, or Claude Code's quota probe),
  and `InterpretedCall`, a `Reading` read as its client uses it); driving a
  client through its structured mode (`drive`: the `DriveCommand` and
  `DriveEvent` vocabulary, with `ItemRead`, what Ren reads of each item for
  every client, and the `Driver` over each client's codec), its pure line
  framing (`lines`), the models a client lists (`models`: `Model` by
  `ModelId`, read by `ModelListing` from a short listing process), and a
  run's `RunSettings` (`settings`: its model, `Effort`, and
  `ApprovalPolicy`, and `Midway`, what each client takes mid-run: steering
  and which settings change from the next turn); the commands a client
  offers a prompt (`commands`: `ClientCommand`, each codec reading its
  client's own list); the gateway URL a harness is pointed at and the
  `UserToken` it presents there (`endpoint`); and `HostEnv` (`host`), the
  host environment that launching and configuration read. It performs
  filesystem and process I/O but has no async runtime, HTTP, database, or
  CLI dependency.
- `crates/ren-recorder`: the bounded capture queue between forwarding and
  storage, with `Recorder` as its primary type. `CallTap` (and `ChannelTap` for a
  WebSocket's end) is the forwarding side, `RunTap` records an agent run's
  start (with the operator session it serves, `RunSession`, a new one recorded
  first as `SessionRecord`), opening, settings changed mid-run, end, and each entry of its conversation (`RunEntry`, whole,
  admitted against the queue's byte budget), and `HistorySlot` an operator's archive or delete
  of a recording, run, or operator session (`HistoryEdit`); `RecorderTask` (run by the daemon on
  its observation thread) is the shell around the pure `RecorderCore`
  (`recording`: records in queue order in, a batch of operations out, with
  chunking, inspection, and digests), and runs group commits, retention,
  rederive passes, and the live broadcast (with `progress`: unstored, redacted
  updates on streaming calls, `Recorder::subscribe_progress`), writing through the `RecordSink`
  port. `CaptureOptions::split` is the tap's pure capture arithmetic, and
  `src/purity.rs` keeps the pure modules free of runtime and I/O. Omissions are counted by reason in
  `RecorderMetrics`, readable after shutdown through `RecorderMeter`. It also
  owns what is recorded: Ren's identities and `IdSource` (`id`), `Clock`
  (`time`), terminals and other recorded facts (`capture`), routing facts and
  `OperatorRevision` (`routing`), who called (`caller`: `UserId`,
  `TailscaleLogin`, and `Caller`), batches (`batch`), the content-defined
  `Chunker` (`chunk`), the stored inspection format (`inspection`), and
  `RetentionPolicy` (`retention`), and credential detection and redaction
  (`secrets`: `SecretScanner`, well-known formats only, never storing a secret),
  so text is sanitized before anything is persisted, and the stored form of a
  call's text (`digest`: `CallDigest::seal` redacts and cuts an
  `InterpretedCall` to a `DigestBudget`, keeping only what each secret leaves;
  `StoredExchange` replays a call's stored bodies to derive it again). It has no database, HTTP,
  or CLI dependency.
- `crates/ren-store`: persistence (SQLx, SQLite WAL), with `Store`, the single
  writer, as its primary type; `Reader` is its read-only pool and `Backup` the
  online backup, which holds the `SweepGate`. `ren.sqlite3` holds the
  observation log and query tables; `blobs.sqlite3` holds deduplicated,
  content-defined body chunks, in the format `chunk` defines (hash, codec,
  encoding). Each operator session is in `operator_sessions` (not
  `sessions`, which holds recordings), and each run names the session it
  serves (`runs.operator_session`); `Reader::sessions` and `Reader::session`
  read them with their runs (`SessionView`). Each driven run's conversation is in `run_entries`, whole, and
  `Reader::conversation` reads it across the runs on one client thread. It
  contains typed query parameters and read views, turn and call
  activity assembled from the calls' stored digests (`activity`, with the
  recording context and cost report in `activity::report`), the full-text search
  and exposure listings (`search`, `Reader::search` and `Reader::exposures`) over
  the digests the recorder derives for each finished call (`call_digests`,
  `call_exposures`, and `search_texts` with its FTS5 index, in `ren.sqlite3`;
  `RecordSink::undigested` lists calls to derive again), and implements the recorder's `RecordSink`. It owns the versioned migrations for
  `ren.sqlite3` (`crates/ren-store/migrations`); never edit one that has
  shipped. Every history read is a `ReadRequest` that `Reader::serve` answers
  (`read`); the API, the CLI, and MCP all ask through it. Its tests include
  `tests/recorder.rs`, the recorder against the real store.
- `crates/ren-supervisor`: runs, with `Supervisor` as its primary type: agent
  processes started in a folder and driven through their client's
  structured mode (Codex, Claude Code, Pi, and OpenCode), each serving an operator
  session (`SessionState` in `session`: a draft until its first prompt,
  active after, archived on the shelf; see `docs/architecture.md`,
  "Sessions, runs, and recordings"). The
  task holds sessions and their processes together: `Supervisor::create`
  starts a session's client at once, `send` carries a command through its
  process or, for a prompt with none running, through a new one that resumes
  its conversation, `choose_client` replaces a draft's client, `shelve`
  archives (stopping its process) or unarchives, `adopt` serves a session the
  store holds, `forget` stops serving one being deleted, and `session_commands` and `session_files` serve its composer
  (the folder's listing is the session's, kept until a turn ends). Session
  changes go to `subscribe_sessions`. A run started through `start` is a
  session and its first process. A client idle between turns is stopped
  after `IdleStop` (ten minutes in the daemon; its run ends `idled`), and its
  session goes on. `RunState` (`run`) is the
  pure lifecycle machine (starting, idle, working with its waiting approvals and the prompt queued
  for after the turn (a `Delivery`: start, queue, or steer),
  stopping, ended with a `RunEnd`); `Workdir` (`workdir`) keeps runs inside
  `Bounds`: the configured roots, the other places (`~/.config`), and the
  scratch folder Ren owns, where a run started without a folder
  (`StartPlace::Scratch`) gets a new git repository of its own; all resolved
  by the host so links cannot escape; `files` ranks the paths of a run's folder for a prompt to
  name (`FileListing`, walked by the host's `AgentHost::files`); `project` finds the places a run can
  start in, each a `Project` of a `PlaceKind` (git repositories under the
  roots, not looked inside; other places' folders; earlier scratch
  folders); `models` lists each client's models through a short listing
  process and keeps the answer (`Supervisor::models`); `Client` and
  `ClientStatus` (`client`) say whether a run can start
  with each client here; `AgentHost` (`host`) is the port for resolving paths,
  looking in folders, making folders (a scratch run's as a repository),
  finding programs, and starting processes, which tests
  fill with
  scripted in-memory agents. `SupervisorTask` is the shell: one task reads every
  agent's stdout, writes its stdin, and waits for its exit, records each run
  through the recorder's `RunTap` (a run without room to be recorded does not
  start), with every event of its conversation as `entries` keeps it (no
  streamed pieces; a reply cut off kept as far as it came), and drains every
  run when the last handle is dropped. `src/purity.rs` keeps `run`,
  `workdir`, `project`, `entries`, `files`, and `session` free of runtime and I/O. The daemon constructs it when `[runs]`
  roots are configured, with `ProcessHost`; the gateway's control API serves it.
- `crates/ren-gateway`: Ren's HTTP interface, with `Gateway` as its primary type.
  It owns forwarding, management, and control routes on one listener, assembled
  by `Gateway::router` or `Gateway::into_ingress`. Forwarding uses a declarative
  route table, the header policy, request buffering with a global
  byte budget, the typed upstream entries with their credential override, the
  candidate loop that carries out `CallRun`, the relay that drives the pure
  `HttpAttempt` machine
  (`attempt`), the WebSocket relay that drives `WsChannel` (`channel`), the
  `Upstream` seam (HTTP send and WebSocket connect) with its Reqwest
  implementation, and the ingress with the shutdown drain, which also waits for
  upgraded channels. Use Axum's request, response, body, and WebSocket types
  directly; no framework-independent HTTP adapter layer. It also owns retry
  eligibility (`retry`), operator state (`operator`), the named users and
  how a request is matched to one (`user`: `Users::identify`, with
  `TokenDigest`, a user token's digest), each user's and the unnamed
  callers' concurrency limits (`limits`), the `Controls` that hold
  it with the running-call registry (`control`), the `Build` and the
  instance's environment (`instance`) that `/health` reports (`build`), and
  pure call planning
  (`policy`: block, transform, and fallback rules validated at load, with
  protected headers and body paths), and the call's way through its
  candidates (`call`: `CallRun`, which `Gateway::forward` carries out).
  `src/purity.rs` keeps `attempt`, `call`, `channel`, `retry`, `operator`,
  and `user` free of runtime, I/O, storage, and shell-module
  dependencies. Management receives `Reader`, `Backup`, retention settings, and
  control credentials through `ManagementSetup`; SQL stays in `ren-store`, and
  forwarding records only through the recorder.
  - `src/api`: the versioned read-only management API (`ApiState`), body previews,
    and the `/api/live` SSE feed. Reads only through `Reader`. `ApiRead`
    (`route`) is where each `ReadRequest` lives in HTTP, in both directions:
    the server parses URIs with it (one handler serves every read) and the
    CLI's client builds them with it.
  - `src/api/control`: the mutating `/control/v1` API over the gateway's
    `Controls` (token or Tailscale identity; browsers refused), and its
    `[control]` configuration (`ControlOptions`) and `ControlToken`. Handler state
    stays crate-private; configuration and token types are exported at the root.
    Its `sessions` module serves `/control/v1/sessions`: creating a session,
    prompting, unqueueing, answering approvals, configuring, choosing its
    client while a draft, interrupting, stopping, archiving, unarchiving,
    deleting, its composer's commands and files, and session changes as SSE, through
    the `Supervisor`, loading a session the store holds (`Reader::session`)
    on its first request. Its `runs` module serves `/control/v1/runs`, read only: the live runs
    of the `ren_supervisor::Supervisor` in `ManagementSetup` and their updates
    as SSE, and what a session can start with (places, clients, and models); it
    holds the run refusals the `sessions` routes share. Its `history` module serves
    `/control/v1/history`: archiving, unarchiving, and deleting a recording or
    run, checked against the `Reader` and recorded through the recorder.
- `crates/ren-daemon`: configuration, resource wiring, and lifecycle, with
  `Daemon` as its primary type (`lib.rs`). It opens storage, binds the listener,
  writes the control token, constructs the gateway and recorder, owns the
  server task and the observation thread the recorder runs on, and
  coordinates HTTP shutdown before capture drain. It defines no
  HTTP routes or handlers and has no production Axum dependency. `config` turns
  `Config` into a `RuntimeConfig`. `host` holds `SystemClock`, `RandomIds`,
  `ObservationThread`, `ProcessHost`, the supervisor's `AgentHost`, and
  `HostTools`, the depot's `ToolHost`. `tools` is the tool store on disk
  (`ToolStore`, under `<data>/tools`): it carries out the depot's
  `InstallJob`s (downloading through its `Fetch` seam, `ReqwestFetch` in
  production, https only and capped in bytes; unpacking tar.gz, zip, and
  bare releases, refusing entries that reach outside their folder), and
  activates, restores, and collects toolset generations with renames of
  finished folders and links. Nothing calls it yet.
  Its integration tests hold `TestGateway` (`tests/support`) and the gateway,
  WebSocket, decision, and full-daemon tests; they use the gateway's complete
  router with test capabilities.
- `crates/ren-cli`: the `ren` binary, with `Cli` as its primary type
  (`lib.rs`) and a thin `main.rs` that sets up logging and captures `HostEnv`.
  `commands` holds clap parsing and dispatch (`runs` is `ren runs`, the
  control API's runs, sent through `ControlTarget` like `ren control`;
  `tools` is `ren tools`, the depot's catalog and this host's tool store,
  acted on in its data directory): each command is a
  `#[derive(Args)]` struct with its own execution method, and command groups
  are `#[derive(Subcommand)]` enums. `client` chooses the gateway (`--gateway`,
  `REN_GATEWAY`, `client.toml`, then the local listener) and owns
  `client.toml`, which `ren init` writes. `management` is where history and
  control commands act: this host's storage and token, or a gateway's read and
  control APIs, printing the same JSON either way (`History` serves a
  `ReadRequest` from either). `tail` is the pure core of
  `ren tail`: the live feed decoded through the protocol crate's SSE framer, and
  its lines.
  `mcp` is the pure core of `ren mcp` (a source test keeps it free
  of runtime and I/O): JSON-RPC over stdio in both MCP eras, and the read-only
  tools, whose arguments parse into `ren_store::query` types. `style` decides once, per stream,
  whether stdout and stderr are colored (a terminal, `NO_COLOR`,
  `CLICOLOR_FORCE`, `TERM=dumb`); JSON and piped output stay plain. `build.rs` records the
  source revision (`REN_REVISION`) for `--version`, `/health`, and `doctor`. `Cli::run`
  prints errors itself, so they are styled too. `doctor` and `service` are the doctor report and the
  service definitions. Refactors preserve existing flags, output
  formats, exports, and client passthrough. Its integration tests run the built
  binary (`CARGO_BIN_EXE_ren`).
- `ui`: an independent SvelteKit, Svelte 5, and TypeScript server (Node adapter). It
  reads the daemon only through the management API and forwards only allowlisted
  read-only endpoints. It never forwards browser cookies or credentials, never reads
  storage directly, and never proxies inference. There are two exceptions.
  Runs: with `REN_CONTROL_TOKEN_FILE` set, `/control/runs*`,
  `/control/sessions*` (not the operator state's session blocks), and
  `/control/history/*` (`src/lib/server/control.ts`, `RunsProxy`) forward the
  Sessions view's same-origin JSON requests to the daemon's `/control/v1/runs*`,
  `/control/v1/sessions*`, and its archive, unarchive, and delete routes only, with the control token read from that
  file on each request, never sent to the browser, and, with
  `REN_CONTROL_LOGINS` set, only for those Tailscale logins; `src/lib/runs.ts`
  is the view's pure core (run states, the live transcript, the sessions by
  folder (`Sessions`), projects by place, and the clients a run can start with),
  `src/lib/mention.ts` the composer's offers (the client's commands after
  `/` or `$`, the folder's files after `@`), `src/lib/redline.ts` Redline's
  pure core (what a mark points at, the brief an agent gets, and the open
  sessions in Ren's checkout, `REN_SOURCE_DIR`, a brief can go to;
  `RedlineDesk` in `src/lib/redline.svelte.ts` holds the pane's marks and
  destinations, and the layer and its floating pane are `RedlineLayer.svelte`
  and `RedlinePanel.svelte`, sending through the same session routes),
  `src/lib/markdown.ts` how Markdown is read (`MarkdownTree`: CommonMark with
  GitHub's extensions, parsed by `mdast-util-from-markdown` into a typed tree
  with untrusted text made inert, rendered by `Markdown.svelte` without
  `{@html}`), and
  `src/lib/server/runsStart.ts` makes the Sessions page's first reads on the UI
  server, with the browser's Tailscale login and never its cookies, so the
  page renders as it will stay.
  The dev slot's: with `REN_DEV_TRAFFIC_URL` set (loopback only), `/dev/traffic`
  forwards the dev toolbar's three JSON requests to the slot's fake traffic
  generator, never to the daemon.
- `crates/ren-harness/pi`: the embedded Pi extension and its dependency-free contract tests.
  Pi owns provider authentication, refresh, payload conversion, and retries.
- `xtask`: repository tasks run with `cargo xtask` (alias in `.cargo/config.toml`),
  with `Xtask` as its primary type; unpublished and outside `default-members`.
  `cargo xtask install-vps` installs or upgrades Ren from a checkout as a
  systemd user service and can publish it with Tailscale Serve; it restarts a
  running daemon only between calls and between runs' turns, paused, and
  checks the new build, and with `--ui-https-port` it builds and deploys the UI as `ren-ui.service`. It is a
  shell: it runs external commands and reads configuration through
  `ren-daemon`'s `Config`. Its parsing of Tailscale status (`tailscale`) and of the
  installed `ren`'s output (`daemon`), its UI unit rendering (`ui`), its saved flags
  (`publish`), and the upgrade's wait, pause, and build-check decisions are
  unit-tested; the commands it runs are exercised only by running it.
  `cargo xtask dev` runs the dev slot (`dev`, `docs/deployment.md#dev-slot`):
  this checkout's build beside the installed service, with its own config,
  data, and ports, against a fake provider (`dev::provider`, whose replies are
  the pure, tested `script`), seeded with fake traffic (`dev::seed`, scenarios
  as pure data), as `ren-dev-*` transient units, runs driven against a fake `codex`, `claude`, `pi`, and `opencode`
  (`dev::agent`: the shell, and `script`, `claude`, `pi`, and `opencode`, the pure
  conversations it plays as each client), with live traffic whose
  settings both the generator and the fake provider reread (`dev::traffic`,
  with its loopback control API for the dev UI's toolbar), an optional private
  preview on the tailnet, and
  headless screenshots (`dev::shot`).
- `deploy`: systemd units, and in `deploy/container` the compose file, Podman
  Quadlet files, and Tailscale Serve route for the root `Dockerfile`
  (`docs/deployment.md#containers`), and in `deploy/apparmor` the profile that
  lets Codex's sandbox start where AppArmor restricts user namespaces
  (`docs/clients.md#sandbox-on-ubuntu`). `scripts`: manual smoke tests for Pi,
  systemd, and launchd.

### Crates

"Depends on" names the nearest crates below; a crate may also use lower ones
directly.

| Crate                                      | Primary type                                        | Depends on        |
| ------------------------------------------ | --------------------------------------------------- | ----------------- |
| `ren-protocol`                             | `CallInspector`                                     | none of ours      |
| `ren-depot`                                | `Depot`, with its `ToolHost` port                   | none of ours      |
| `ren-harness`                              | `Harness` (`ClaudeCode`, `Codex`, `Pi`, `OpenCode`) | protocol, depot   |
| `ren-recorder`                             | `Recorder`, with its `RecordSink` port              | protocol, harness |
| `ren-store`                                | `Store`, which implements `RecordSink`              | recorder          |
| `ren-supervisor`                           | `Supervisor`, with its `AgentHost` port             | recorder, harness |
| `ren-gateway`                              | `Gateway`                                           | supervisor, store |
| `ren-daemon`                               | `Daemon`                                            | gateway, store    |
| `ren-cli` (package `ren-rs`, binary `ren`) | `Cli`                                               | daemon            |
| `xtask` (unpublished)                      | `Xtask`                                             | daemon            |

`docs/architecture.md` lists what each crate owns. These rules apply to every
crate:

- **One primary type.** Each library crate exposes one primary type that names
  what it is. Its other public items are that type's inputs, outputs, errors, or
  handles. If a crate has no natural primary type, rethink the crate; never add a
  wrapper or namespace type to satisfy this rule.
- **Names.** Name a crate for its role or for what it describes. Never a verb, a
  category, or a grab bag (`core`, `common`, `model`, `types`, `utils`).
- **No types crate.** A type lives in the lowest crate that owns its invariant.
  Never move a type to a shared crate to break a dependency; find its owner.
- **Ports point down.** A capability trait is declared by the lowest crate that
  consumes it and implemented by a higher one. A crate never depends on a crate
  above it in the table.
- **Harnesses.** Claude Code, Codex, Pi, and OpenCode (the clients of section 1) are one
  closed enum, `Harness`. Don't add a per-harness trait.
- Don't add a crate without first adding it to this table and to
  `docs/architecture.md`.

### Stack and dependencies

Tokio, Axum/Tower, Reqwest/Rustls, SQLx with SQLite WAL, and `tracing`. Use REST and
SSE for management and live observations, and WebSockets only where an upstream
protocol requires them. Add dependencies only when functionality needs them. A
dependency used by one crate belongs in that crate's manifest (test-only usage
included). Dependencies shared by several crates, and all of our own crates, are
declared in root `[workspace.dependencies]` and inherited. Keep `Cargo.lock`
tracked and update it intentionally.

### UI and Pi (TypeScript/JavaScript)

The same principles apply in TypeScript:

- Parse API responses and environment variables at the boundary.
  `ui/src/env.ts` validates gateway origins, keeps loopback defaults, and keeps the
  server's daemon address separate from the client-facing address.
- Use discriminated unions for states instead of loose flags.
- Keep pure logic in `src/lib` modules that can be tested without a browser.

Visual style: square and flush. Views fill the workspace edge to edge (as Traffic
does), with no margin around them. Panels, sections, tiles, cards, chips, and chart
marks have square corners and touch, separated by 1px rules, never by gaps or
margins between them. Only controls (buttons, inputs,
selects, segmented controls) round their corners, by 2-3px; switches and status
dots are round.

Use filesystem routes and typed load functions. Private HTML, route data, and API
responses are non-cacheable. SvelteKit generates CSP nonces, including for the
inline theme initializer. The policy lives in `ui/src/csp.ts`; its only inline
allowance is SvelteKit's route announcer style, by hash, pinned by `tests/csp.test.ts`. Never commit generated assets (`ui/build`, `.svelte-kit`).

---

## 5. Build and verification

Use the toolchain pinned in `rust-toolchain.toml` and don't update it incidentally.
New crates inherit workspace settings and lints. Cargo builds only Rust: never run
frontend tooling from Cargo or embed UI assets. The daemon is headless and works
with no UI checkout, build, or server. The exceptions are deployment and dev steps, not
a build: `cargo xtask install-vps --ui-https-port` and `cargo xtask dev up --ui`
run Bun to build and run the UI on the host.

Markdown, JSON, and TOML formatting, from the repository root (dprint 0.56.0,
matching CI):

```sh
dprint check
```

Run `dprint fmt` to apply the formatting in `dprint.json`.

Rust changes, from the repository root:

```sh
cargo fmt --all -- --check
cargo check --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

UI changes, in `ui` (Bun from `package.json`, Node 22.17+):

```sh
bun install --frozen-lockfile
bun run format:check && bun run check && bun run test && bun run build
```

Verify changed UI interactions with synthetic traffic in the dev slot before
they reach the installed service: `cargo xtask dev up --ui`, `cargo xtask dev seed`,
then `cargo xtask dev shot` for screenshots and the private preview
(`https://<node>:9444`) for a browser. Keep focused unit tests and don't add a
browser integration suite yet. UI changes don't require rebuilding Rust.
Never stop processes by a name pattern: the slot's units are stopped by name
(`cargo xtask dev down`), and the installed `ren` and `ren-ui` services must keep
running.

Pi integration changes: `node --test crates/ren-harness/pi/ren.test.mjs`. When Pi is
installed, build Ren and run `python3 scripts/smoke-pi.py`; it uses isolated config
and OAuth-shaped fixture credentials against local mocks. Passing mock tests does
not establish live subscription access.

Service changes: `python3 scripts/smoke-systemd.py --binary <built-ren>` as a
non-root user with a systemd user manager (its `--https-port` option needs an unused
Tailscale Serve port; preserve existing Serve routes and policy), or
`python3 scripts/smoke-launchd.py` on macOS (tests `target/debug/ren`).

Publishing: every crate is published to crates.io, and the CLI package is
`ren-rs` (crates.io already has a `ren`). Our own crates' versions in root
`[workspace.dependencies]` must equal `workspace.package.version`. Check with
`cargo package --workspace --locked`, which builds each crate from its packaged
sources, then publish with `cargo publish --workspace --locked` (dependency order).
A crate may include only files inside its own directory.

Documentation-only changes don't require compiling; check the diff and that every
referenced path exists.

---

## 6. Working conventions

- Keep changes focused. Refactor in steps that each pass verification, and don't
  mix a behavior change with a structural move.
- Preserve unrelated user work.
- Update documentation when behavior or configuration changes. `README.md` and the
  guides in `docs/` are for operators; this file and `docs/architecture.md` are for
  contributors. Documentation describes the current state, not its history.
- Report concisely: what changed, what was verified, and what remains limited or
  unverified.
