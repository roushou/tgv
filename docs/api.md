# History and API

Read history from the CLI or the daemon's read-only management API.

## CLI

```sh
ren history --limit 10
ren history --user alice
ren sessions
ren timeline <session-id> --limit 100
ren turns <session-id>
ren report <session-id>
ren turn <turn-id>
ren activity <call-id>
ren search "cargo test" --field tool_input
ren exposures
ren inspect <call-id>
ren inspect <call-id> --body response --output response.bin
ren tail
ren tail --json
```

Runs (see [runs](#runs)) have their own commands, through the control API:

```sh
ren runs start codex ~/dev/acme                     # prints the new session
ren runs start claude ~/dev/acme --prompt "fix the failing test"
ren runs ls                                         # the runs, live
ren runs sessions
ren runs projects                                # where a run can start
ren runs say <session-id> fix the failing test
ren runs answer <session-id> <approval-id>      # or: decline, cancel, accept-for-session
ren runs interrupt <session-id>
ren runs stop <session-id>                      # its client; the session goes on
ren runs watch --run <run-id>
ren runs history
```

`start` makes a session in the current folder when none is given, with
`--resume <thread>` to go on with a client conversation begun elsewhere, and
sends `--prompt` once it is made; through a gateway the folder must be an
absolute path on the gateway's host. `watch` prints each run update as one
JSON line. `history` reads recorded runs like the other history commands.

Commands emit the same JSON the read API answers (without `api_version`),
whether they read this host's storage or a gateway: `history` and `timeline` a
page `{calls, next}`, `sessions` `{sessions}`, `turns` `{turns}` for a recording,
`exposures` `{exposures}`, `report` a
recording's [context and cost report](#recording-report), `turn` and
`activity` what a turn or a call did (see [activity](#activity)), `search`
matching prompts, replies, and tool calls (see [search](#search)), and `inspect`
one call with its attempts and observation log. Body exports write the exact captured bytes to a new file, refuse
incomplete bodies, and never overwrite existing files. They work while the daemon
runs and after it stops. Use the same data directory (`--data-dir`) for all
commands.

The commands read the daemon they target. With `--gateway`, or else a gateway
named by `REN_GATEWAY` or saved by `ren init`, they read through that gateway's
API below and print the same JSON as from its storage; exports use the body
export endpoint. `--config` or `--data-dir` without `--gateway` reads this host's
storage, as does having no gateway at all. A remote read needs the daemon
running.

`ren tail` follows the [live feed](#live-feed) of the daemon the CLI targets
(its local listener, or the gateway) and prints a line as each call starts (provider, endpoint, and session) and
finishes (model, outcome, status, tokens, and duration), and when an operator
changes the operator state. Each line starts with the time (UTC) and the call.
Attempt and channel records are left out; `--json` prints every
observation instead, one JSON object per line. It starts from now. When the feed
ends, for example on a daemon restart, it reconnects every two seconds from the
last observation it printed, so nothing in between is skipped. It stops on
interrupt, or if the first connection fails.

## Activity

`ren turn` shows what an agent did for one user prompt, assembled from each of
its calls' activity, derived when the call finished:

- `prompt`: the user's own words, without the context a client adds to user
  messages (Claude Code's `<system-reminder>` blocks and the `Tool loaded.` it
  adds after loading a deferred tool, Codex's environment and `AGENTS.md`
  instructions);
- `calls`: each call's tool calls, with their input, their `effects` (`read_file`,
  `write_file`, `run_command`, `search`, `delegate`, or `fetch`, and the path,
  command, pattern, task, or URL the input names), and the `result` a later
  request in the turn carried (`succeeded`, `failed`, or `unreported`; `null`
  when none answered), plus the start of the model's reply;
- `files_read`, `files_written`, `commands`, and counts of tool calls, failed
  ones, and unanswered ones;
- each call's `metrics` (context, cache read and written, and output tokens,
  request bytes, duration), as in the [recording report](#recording-report);
- `findings`: `large_tool_output` (a tool result of at least 20,000 characters,
  which every later call carries) and `repeated_failure` (one tool failing on
  one target at least 3 times in the turn).

A tool call is what the model requested, not proof that it ran. A result's
status is what the client reported: Anthropic's `is_error`, or for Codex the
exit code its output states; other Responses results are `unreported`. Effects
come from each client's own tool names and input fields; a tool Ren does not
know has none. Turns are the client's grouping (Claude Code's prompt ID, Codex's
turn ID); Pi sends none, so use `ren activity <call-id>` for its calls. The daemon derives
each call's activity once, when the call finishes, from up to 16 MiB of each
body, and stores it with secrets redacted; a turn reads at most 100 calls
(`omitted_calls` counts the rest). `limitations` on each call say what could not
be read (`truncated`, `capture_gap`, `not_json`, `unfinished_tool_call`, …), or
`not_yet_derived` for a call that finished moments ago, or whose activity the
daemon is deriving again from its stored bodies. Text is
excerpted; `omitted_chars` says how much was left out, and the full text stays in
the body. Retention removes activity with its call.

`ren turns <session-id>` lists a recording's turns newest first (`--limit`, up to
1000; no paging), each with its call and error counts, tokens, and the start of
its prompt, with secrets redacted. A
turn's `ended_at` is `null` while any of its calls is still running.

## Search

`ren search <text>` finds what calls contain: the user's prompt (without the
context clients add), the model's reply, tool input (the tool's name, the files
and commands it names, and its input), and tool output. All words must match;
`"quoted text"` matches as a phrase. Punctuation separates words, so search
`"install-vps"` as a phrase. Query syntax is never interpreted: `OR`, `NEAR(`, or
`*` are searched for as text. Filter with `--session`, `--client`, and `--field`
(`prompt`, `reply`, `tool_input`, `tool_output`); `--limit` defaults to 20.

Results are newest call first: each names the call, recording (`session`), turn, client,
field, and a snippet with matches marked `«like this»`. `indexed_calls` says how
much history is searchable: every finished call, once its activity is derived
(see above). Calls retention removes leave the index too.

## Exposed secrets

`ren exposures` lists credentials that calls carried, newest first. The daemon
scans every finished call's newest user input (including context the
client adds, such as Claude Code's reminders, since that is sent to the provider
too), tool input and output, and the model's reply, for well-known formats only:
`anthropic_key` (`sk-ant-…`), `openai_key` (`sk-…`, `sk-proj-…`), `github_token`
(`ghp_…`, `github_pat_…`), `aws_access_key` (`AKIA…`), `google_api_key` (`AIza…`),
`slack_token` (`xox…`), `stripe_live_key` (`sk_live_…`), `private_key` (a PEM
block), and `jwt`. It does not guess at random-looking text, so a finding is rarely
a false alarm, but a key in another format is not found.

The secret itself is never stored or shown: an exposure keeps its `kind`, the
`field` it was in, a masked `hint` (the known prefix and last four characters,
`sk-ant-…a1b2`), a `fingerprint` (16 hex characters of its BLAKE3 hash, equal for
repeats of one secret), and how often it appeared. Activity and search keep
text with each secret replaced by `«redacted: kind»`; captured bodies are kept as
recorded. Recording reports and `ren turn` list exposures as `secret_exposure`
findings, and the Overview lists the last day's first under Needs attention. A secret sent to
a provider should be rotated: Ren cannot recall it. The scan covers the newest
input of each call, not the system prompt or older history a client resends.

## Recording report

`ren report <session-id>` shows where a recording's context and prompt-cache
tokens went, from its call records alone (no body is read, so it is fast):

- `turns`: for each turn, its calls, the idle time before it (since the
  recording's previous call ended), context tokens at its first and last call,
  the cache hit percent (cache reads over context), tokens written to the cache,
  and output tokens;
- `totals` over the recording's calls that reported usage, including the peak
  context;
- `findings`, in call order:
  - `cache_rebuild`: a call read less than half of the earlier peak context from
    the cache and wrote at least half of it again (at least 10,000 tokens). It
    reports `idle_before_ms`; a provider's cache entries expire, but Ren cannot
    see its cache, so the cause is not stated.
  - `context_growth`: the 5 largest steps of at least 10,000 tokens past the
    recording's earlier peak.

Context is new input plus cache reads and writes for Anthropic, and input for
OpenAI, whose input already counts cached tokens. Findings need an earlier peak
of at least 10,000 tokens, so a recording's start is not reported. Peaks are
recording-wide, so a subagent's smaller context is neither a rebuild nor growth.
Token counts are provider-reported, not cost or subscription allowance. A report
reads at most 5,000 calls (`omitted_calls` counts the rest).

## MCP server

`ren mcp` serves this history to coding agents as a
[Model Context Protocol](https://modelcontextprotocol.io) server over stdio, so an
agent can look up what it, or another agent, did:

```sh
claude mcp add --scope user ren -- ren mcp
codex mcp add ren -- ren mcp
```

Its tools are read-only:

| Tool                | Returns                                                                                                             |
| ------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `search_calls`      | Calls, newest first, filtered as in [call search](#call-search) (`session_id` names the recording); `{calls, next}` |
| `list_sessions`     | Recent recordings with their counts and token totals                                                                |
| `session_calls`     | One recording's calls, oldest first; `{calls, next}`                                                                |
| `search_history`    | [Full-text search](#search), as `ren search` returns it                                                             |
| `list_exposures`    | [Exposed secrets](#exposed-secrets), as `ren exposures` lists them                                                  |
| `session_report`    | A recording's context and cost report, as `ren report` shows it                                                     |
| `list_turns`        | A recording's turns, newest first, as `ren turns` lists them                                                        |
| `get_turn`          | What one turn did, as `ren turn` shows it: prompt, tool calls with effects and results, files, commands             |
| `get_call_activity` | One call's activity, as `ren activity` shows it                                                                     |
| `get_call`          | One call with its attempts, inspection, and observation log, as `ren inspect` prints it                             |
| `get_body`          | An excerpt of a [body preview](#body-previews): decoded text (or raw bytes), at most `max_chars` (default 20,000)   |
| `usage`             | Calls, errors, and tokens, as [`/api/usage`](#usage)                                                                |

Listings return at most 100 rows. Invalid arguments, unknown IDs, and an
unreachable daemon are tool errors that the agent reads and can correct.

It reads through the management API of the daemon the CLI targets, like
`ren tail`: the local listener, or the gateway from `--gateway`, `REN_GATEWAY`,
or `ren init`. The daemon must be running. Bodies may contain private prompts
and tool output, and an agent reading them sends them to its provider like any
other context. It serves protocol revision `2026-07-28`, where every request
carries its version, and, after an `initialize` handshake, the earlier revisions
`2025-11-25`, `2025-06-18`, `2025-03-26`, and `2024-11-05`. Requests are answered
in order, one at a time; stdout carries only protocol messages and logs go to
stderr.

## Endpoints

The read API calls recordings sessions, in its routes (`/api/sessions`),
parameters, and JSON fields (`session`, `sessions`), as do the CLI and the MCP tools.
Every JSON body carries `"api_version": 1`; errors are `{api_version, error}` with
`error` one of `not_found`, `invalid_query`, `busy`, `unavailable`, or
`identity_refused` (`401`, from `/api/caller` only). An identifier that is not one
(a call ID, or a body direction other than `request` or `response`) is
`not_found`, and an unknown or unreadable query parameter is `invalid_query`. The [control API](#control-api)
(`/control/v1`) is versioned separately.
A body export answers with the raw bytes (`application/octet-stream`) only when
the body was stored whole; otherwise `not_found`. At most two run at once
(`busy`), and the UI does not forward it.

| Endpoint                                                               | Result                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `GET /health`                                                          | `ok` or `degraded` (HTTP 503), the build, the instance (`environment`: `production` or `development`), a summary of the operator state (pause, and how many sessions and users are blocked), recorder counters                                                                                                                                                                                                                                                            |
| `GET /api/metrics`                                                     | Recorder counters and storage usage                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `GET /api/caller`                                                      | `{api_version, caller}`: how the gateway identifies this request's caller (`caller` is `{kind, user, login}`), as it would a forwarded call; `401` `identity_refused` for a refused token or login                                                                                                                                                                                                                                                                        |
| `GET /api/storage`                                                     | Active retention policy, disk usage of the database files, calls with derived activity, and the per-body capture limit (`capture.body_limit_bytes`)                                                                                                                                                                                                                                                                                                                       |
| `GET /api/calls?limit=100`                                             | Filtered, paginated calls, newest first (see [search](#call-search))                                                                                                                                                                                                                                                                                                                                                                                                      |
| `GET /api/calls/{call-id}`                                             | One call with its attempts, inspection, and observation log                                                                                                                                                                                                                                                                                                                                                                                                               |
| `GET /api/calls/{call-id}/body/{request\|response}`                    | Bounded body preview                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `GET /api/calls/{call-id}/body/{request\|response}/export`             | The complete captured body as raw bytes, as `ren inspect --body` exports it                                                                                                                                                                                                                                                                                                                                                                                               |
| `GET /api/sessions?limit=100`                                          | Recent recordings with call, turn, and error counts and token totals; `shelf=archived` lists the [archived](#archiving-and-deleting) ones instead                                                                                                                                                                                                                                                                                                                         |
| `GET /api/usage?from_ms=&until_ms=`                                    | Calls, errors, and tokens in total, by UTC hour, by model, and by user (see [usage](#usage))                                                                                                                                                                                                                                                                                                                                                                              |
| `GET /api/sessions/{session-id}/calls?limit=100`                       | One recording's calls, oldest first                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `GET /api/sessions/{session-id}/turns?limit=100`                       | A recording's turns, newest first, with prompt excerpts (see [activity](#activity))                                                                                                                                                                                                                                                                                                                                                                                       |
| `GET /api/search?q=&field=&session=&client=&from_ms=&until_ms=&limit=` | [Full-text search](#search), newest call first                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `GET /api/exposures?session=&from_ms=&until_ms=&limit=`                | [Exposed secrets](#exposed-secrets), newest call first                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `GET /api/sessions/{session-id}/report`                                | A recording's [context and cost report](#recording-report)                                                                                                                                                                                                                                                                                                                                                                                                                |
| `GET /api/turns/{turn-id}`                                             | One turn's [activity](#activity)                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `GET /api/calls/{call-id}/activity`                                    | One call's [activity](#activity)                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `GET /api/runs?limit=100`                                              | Recorded [runs](#runs), newest first: harness, folder, the `operator_session` it served, the client's `thread` and the `session` (recording) it names, `started_at`, the `model`, `effort`, and `approvals` it started with (`null` is the client's own), the `toolset` generation on its `PATH` (`0` with no tools installed; `null` when unknown or recorded before this was), and once ended `ended_at`, `end`, and `exit`; `shelf=archived` lists archived ended runs |
| `GET /api/runs/{run-id}/conversation`                                  | A run's [conversation](#run-conversation), whole: its own entries and those of the runs before it on its thread, oldest first                                                                                                                                                                                                                                                                                                                                             |
| `GET /api/runs/{run-id}`                                               | One recorded run                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `GET /api/live?after={seq}`                                            | Server-sent events as observations commit                                                                                                                                                                                                                                                                                                                                                                                                                                 |

Omit `after` for the first page; pass the returned `next` cursor unchanged for the
next one. The CLI accepts the same cursor with `timeline --after`. Pages order by
start time and call ID; refresh from the beginning to see late arrivals that sort
before a cursor. Limits are 1–1000. Recording is asynchronous, so new data appears
shortly after forwarding, typically within the 50 ms commit interval. Busy readers
return `503` without blocking forwarding. Management responses carry
`Cache-Control: no-store`. The UI does not relay full body exports; use `ren inspect` or the export endpoint. The API shares the
listener's loopback/Tailscale access boundary and has no separate login or
cross-origin browser access enabled.

### Recording totals

Each recording in `/api/sessions` carries `error_count`, the model calls (`kind=model`)
the `errors` status filter would find, and `tokens: {input, output, cache_read}`, summed from provider-reported
usage. Input includes cached context whichever way each call's provider accounts
for it; a total is `null` when no call reported usage. Token counts are not
subscription allowance or charges.

### Usage

`/api/usage` covers the calls started within an optional inclusive range
(`from_ms`, `until_ms`; without them, every retained call). It returns `totals`, `hours`
(only hours with calls, oldest first, keyed by the hour's UTC start; clients group
them into local days), and `models` (each provider and reported model, most input
first; `model` is `null` when no response reported one), and `users` (each named
user, most input first; `user` is `null` for calls no named user made: unlisted
and unidentified callers, and calls stored without a caller). Each carries `calls`,
`errors` (model calls the `errors` status filter finds, as in recording totals), and `tokens: {input, output,
cache_read}`, with input including cached context as in recording totals. Unknown
parameters and a reversed range are rejected.

### Builds

`/health` reports the build the daemon runs as `build`: `{"version": "0.1.0",
"revision": "<commit>"}`. The revision is the full git commit the binary was
compiled from, with `-dirty` when tracked files differed from it, or `unknown`
when the build had neither git nor packaging metadata. `ren --version` prints the
same, and `ren doctor` compares the two: it is OK only for the same version from
the same unmodified commit, and a warning otherwise. Untracked files are not
counted as modifications. The revision is read when the `ren-rs` package is
compiled, so after editing a tracked file without staging it, `-dirty` can
appear only after that package is rebuilt.

### Control API

`/control/v1` changes the operator state; see [operator controls](policy.md#operator-controls)
for who may call it. A request with `Origin` or `Sec-Fetch-Site` is refused (`403`
`cross_site`); one with a `Tailscale-User-Login` must name a login in
`control.tailscale_logins` (`403` `forbidden`); any other needs
`Authorization: Bearer <control.token>` (`401` `unauthorized`). Responses are
never cached, and errors are `{api_version, error}`.

`GET /control/v1/state`, and every command that succeeds, answer
`{api_version, state, running}`: `state` is `{revision, pause, blocked_sessions,
blocked_users}` (`pause` is `{"state": "running"}` or `paused` with its
`retry_after`; each block ends at a time (`{"kind": "time", "at": <ms>}`) or when
unblocked (`{"kind": "unblocked"}`)), and `running` lists the running calls
(`call`, `transport`, `user`).

| Endpoint                                      | Body                                           | Notes                                                                                                                    |
| --------------------------------------------- | ---------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `POST /control/v1/pause`                      | `{"retry_after_seconds": 1–3600}`, required    | `ren control pause --retry-after` (default 30)                                                                           |
| `POST /control/v1/resume`                     | none                                           |                                                                                                                          |
| `POST /control/v1/sessions/{session}/block`   | optional `{"duration_seconds": 1–604800}`      | `{session}` is the client's session UUID; without a duration, until unblocked                                            |
| `POST /control/v1/sessions/{session}/unblock` | none                                           |                                                                                                                          |
| `POST /control/v1/users/{user}/block`         | optional `{"duration_seconds": 1–604800}`      | only a configured user; any other name is `404` `not_found`                                                              |
| `POST /control/v1/users/{user}/unblock`       | none                                           |                                                                                                                          |
| `POST /control/v1/calls/{call}/cancel`        | optional `{"block_session_seconds": 1–604800}` | `202` with the state, or `200` when the session is blocked too; `404` `not_running`                                      |
| `POST /control/v1/backup`                     | none                                           | `{api_version, backup}` with the copy's counts; `409` `backup_running`, `503` `backup_incomplete`, `500` `backup_failed` |

A value out of range, or a malformed pause body, is `400` `invalid_request`, and a
malformed session ID is `404` `not_found`. A command the recorder cannot record
is not made: `503` `unrecorded` when the recorder has no room now (try again),
`500` `unrecordable` when its record cannot be written.

### Run conversation

The daemon stores each driven run's conversation as it happens, whole: every
event the client reports (a turn starting and ending, each item: the
operator's prompt, a reply, reasoning, a command with its output, a file
change, a tool call; approvals, errors, and refusals), in the order it came.
An item reported again (a command that finished, a reply that completed)
replaces its entry in place. Streamed pieces are not stored; the item they
make is, and a reply still streaming when its turn ends is stored as far as
it came. Entries are written through the recorder like bodies, never
blocking forwarding: without queue room an entry is counted as omitted.

`GET /api/runs/{run-id}/conversation` answers `{api_version, entries,
earlier}`. Each entry is `{run, seq, at, turn, event}`, where `event` is the
event as the run feed sends it. The conversation includes the runs before
this one on the same client thread (a resumed conversation reads as one), up
to the newest 20,000 entries; `earlier` says older ones were left out.
Deleting a run deletes its conversation.

### Archiving and deleting

`POST /control/v1/history/sessions/{session}/{change}` and
`POST /control/v1/history/runs/{run}/{change}` archive, unarchive, or delete
what was recorded. `{session}` is a recording's ID (as `/api/sessions` lists
it, not the client's session ID), and `{change}` is `archive`, `unarchive`, or `delete`.
A run's change applies to the recordings its client made under the run's
thread too, and a recording's to the runs that worked in it. The CLI is
`ren control archive|unarchive session|run <id>` and
`ren control delete session|run <id> --yes`.

- **Archive** hides it from `/api/sessions` and `/api/runs`, which list it
  with `shelf=archived` instead, with its `archived_at`. A recording with traffic
  newer than its archiving is listed again by itself; a run still running is
  always listed.
- **Delete** removes its finished calls with their bodies, attempts, log rows,
  digests, and search entries, and its ended runs. Calls still open and runs
  still running are kept, so a delete is refused while there are any: `409`
  `call_open` or `409` `run_running`. The next retention pass frees the chunks
  only those calls referenced. It cannot be undone; a client that keeps
  calling starts a new recording.

Each edit is recorded (a `history_edited` observation with the actor) and
applied by the recorder after what it was recording; the answer is `202`
`{api_version, subject, change}` once it is queued. `404` `not_found` is an
unknown subject or change.

### Tools

The runtimes and utilities Ren installs on the daemon's host for its agents
(see [tools for agents](clients.md#tools-for-agents)). Access is the control
API's; `ren tools` on the host prints the same JSON (`list --json`).

- `GET /control/v1/tools`: `{api_version, platform, generation, bin, packages,
  generations}`; `bin` is the current generation's folder of programs (`null`
  before any). `platform` is `linux_x64`, `linux_arm64`, `macos_arm64`, or
  `null` where Ren installs nothing. Each package has `id`, `origin`
  (`builtin`, `operator` for a `[tools]` entry, or `null` for an install the
  catalog no longer names, which can only be removed), `kind` (`client`,
  `runtime`, `utility`, or `null` with no catalog entry), the `program` it provides, the `release` Ren installs (`null`: only found on
  `PATH`), what the current generation holds (`installed`: `version`,
  `sha256`, `dir`), what the daemon's `PATH` has (`system`: `path`, the
  `version` it reported, or `unread`, what it printed instead), and the
  clients that need it (`needed_by`). `generations` lists every generation
  kept, newest first, with its `state` (`current`, `in_use` by a run without
  an end, or `idle`) and `installs`. Reading versions runs each program found
  with `--version`.
- `POST /control/v1/tools/change` `{"add": [...], "remove": [...]}` applies
  the edit to the current generation: it downloads, verifies, and installs
  what is added, and makes the next generation current only if every install
  succeeded. It answers when done: `{api_version, outcome, generation,
  changes, failures}`, with `outcome` `unchanged`, `activated`, or `failed`
  (each failure with its `package`, `version`, `stage`, and `reason`; the
  current generation stays).
- `POST /control/v1/tools/rollback` `{"generation": n}` makes the next
  generation hold exactly what generation `n` held, from installs still in
  the store (nothing is downloaded), and answers as a change does. `404`
  `tools_no_generation` when `n` is not kept; `409` `tools_collected` when
  its installs were removed.
- `POST /control/v1/tools/gc` removes every generation but the current one and
  those in use, and every install none of them holds: `{api_version,
  generations, installs}`.

Refusals: `422` `tools_refused` with a `message` (an unknown package, one only
found on `PATH`, removing what is not installed, or adding and removing one),
`409` `tools_busy` while another change or `ren tools` holds the store, `501`
`tools_unsupported` on another platform, `400` `invalid_request` for a body
that is not an edit, and `503` `tools_disabled` when the daemon could not set
up downloads or its catalog.

### Sessions

A session is what an operator works in: a folder, a client, its settings, and
its conversation across every client process (run) that serves it. It is a
draft until its first prompt goes out, active after, and archived when the
operator shelves it. These routes need runs enabled (`[runs]` roots, see
[runs](#runs)) and the control API's access.
`/control/v1/sessions/{session}/block` and `/unblock` above are the operator
state's and take a client's own session ID, not a session's.

| Endpoint                                             | Body                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | Answer                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| ---------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `GET /control/v1/sessions`                           | query `shelf` (`current`, the default, or `archived`) and `limit` (1–1000, default 100)                                                                                                                                                                                                                                                                                                                                                                                     | `{api_version, sessions}`, most recently active first, each a session body (below)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `POST /control/v1/sessions`                          | `{"harness": "codex"\|"claude_code"\|"pi"\|"opencode"}`, optional `"dir": "/abs/path"`, inside a root or another place (left out, the session gets a new folder of its own in scratch, a git repository; `503` `no_scratch` when the daemon has none), `"resume": "<thread>"`, a client conversation begun elsewhere to go on with, and its settings: `"model"`, a model's `id` from the client's listing, `"effort"`, and `"approvals"`; each left out is the client's own | `201` `{api_version, session}`, `session` the new session's ID. Its client starts at once, so its commands are listed before the first prompt                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `GET /control/v1/sessions/{session}`                 | none                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | `{api_version, session}`, a session body                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `POST /control/v1/sessions/{session}/prompt`         | `{"text": "…"}`, not blank, and optional `"delivery"`: `start` (the default) starts a turn on an idle client; `queue` keeps it for after the running turn (sent as that turn ends, or at once on an idle client; one message waits at a time, so queueing again replaces it); `steer` sends it into the running turn, where the client takes that (`midway` has `steer`), or starts a turn when none runs                                                                   | `202`. With no client running, a new one starts and resumes the session's conversation, then takes the prompt; a prompt to an archived session makes it current again                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `POST /control/v1/sessions/{session}/settings`       | any of `"model"`, `"effort"`, and `"approvals"`, as when creating; each applies from the next turn, where the client takes that change mid-run (`midway`), and to the next client when none runs                                                                                                                                                                                                                                                                            | `202`; the settings change once the client has them                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `POST /control/v1/sessions/{session}/client`         | `{"harness": …}`                                                                                                                                                                                                                                                                                                                                                                                                                                                            | `202`; while it is a draft: its process is replaced and its settings go back to the client's own. `409` `client_fixed` once a prompt went out, `409` `archived` for an archived session                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `POST /control/v1/sessions/{session}/unqueue`        | none                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | `202`, through its process                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `POST /control/v1/sessions/{session}/approvals/{id}` | `{"decision": …}`                                                                                                                                                                                                                                                                                                                                                                                                                                                           | `202`; `accept`, `accept_for_session`, `decline`, or `cancel`, through its client                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `POST /control/v1/sessions/{session}/interrupt`      | none                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | `202`, through its process                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `POST /control/v1/sessions/{session}/stop`           | none                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | `202`: stops its client, whose run ends once its process exited; the session goes on                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `POST /control/v1/sessions/{session}/archive`        | none                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | `202`; stops its process. `409` `archived` when it is already                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `POST /control/v1/sessions/{session}/unarchive`      | none                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | `202`. `409` `not_archived`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `POST /control/v1/sessions/{session}/delete`         | none                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | `202`; deletes the session for good: its ended runs, the recordings and calls they made, and the session itself, even one whose client never ran. The daemon stops serving it at once; the store deletes it when the recorder reaches the edit. `409` `run_running` while a process serves it, `409` `call_open` while one of its calls is open, `404` `not_found`                                                                                                                                                                                                                                                                                    |
| `GET /control/v1/sessions/{session}/commands`        | none                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | `{api_version, commands}`: what the session's client offers a prompt, as it listed it (none until a client has opened its conversation), each `{name, description, hint, kind, sigil, invocation}`: `kind` is `command`, `skill`, or `template`, `sigil` how a prompt names it (`/`, or `$` for Codex's skills), `invocation` what the prompt writes (`/review`, `$release-notes`). Claude Code's come from its answer to `initialize`, Codex's from `skills/list` for the session's folder (a prompt naming one as `$name` sends the skill along), Pi's from `get_commands`, OpenCode's from the Agent Client Protocol's `available_commands_update` |
| `GET /control/v1/sessions/{session}/files`           | query `q`, what was typed after `@`, and `limit` (default 20, at most 100)                                                                                                                                                                                                                                                                                                                                                                                                  | `{api_version, files}`: the paths of the session's folder, with or without a client running, that best match `q`, best first, each `{path, kind, positions}`: `path` relative to the folder, `kind` `file` or `directory`, `positions` the matched characters. The folder is walked leaving out hidden files and what git ignores, at most 20,000 paths 16 folders deep, and kept until a turn ends. `409` `unreadable` when the folder cannot be walked                                                                                                                                                                                              |
| `GET /control/v1/sessions/events`                    | none                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | server-sent `session` events, each the session as served (below), from now on; `lagged` when the feed fell behind (list them again). Each process's state and events are on the run feed                                                                                                                                                                                                                                                                                                                                                                                                                                                              |

A session body is `{session, stored, served}`. `stored` is the session as the
store holds it: `{id, harness, workdir, created_at, model, effort, approvals,
archived_at, first_turn_at, first_prompt, runs}`, `first_prompt` the first
line of its first prompt as its client echoed it (at most 200 characters, `null`
until one is recorded), its runs newest first (each as `GET /api/runs` lists
runs), or `null` for a moment after it is made, until the recorder commits it. `served` is the session as this daemon serves it now:
`{session, harness, workdir, settings, state, process}`, `state` `{"shelf":
"open"|"archived", "phase": "draft"|"active"}` and `process` its newest run, or
`null` while the daemon has not served it since it started. A session the store
holds is served again on its first command, read, or prompt. Commands that need
a process with none running are `409` `no_process`; an unknown session is
`404` `not_found`; the other refusals are as for [runs](#runs).

### Runs

`/control/v1/runs` lists agent runs, follows them live, and says what a
[session](#sessions) can start with (places, clients, and models); a run is
started and driven through its session's routes. A run is the official client, started
by the daemon in a folder under a `[runs]` root and driven through its
structured mode: Codex's `codex app-server`, Claude Code's print mode with
streaming JSON (`claude -p --input-format stream-json --output-format
stream-json`), Pi's RPC mode (`pi --mode rpc`, with Ren's extension), or
OpenCode's Agent Client Protocol mode (`opencode acp`). The
client keeps its own sign-in, and its model traffic goes through this gateway
like any other client's. Ren names a new Claude Code conversation or Pi session
itself (`--session-id`), so a run's `thread` is the session the recorded calls
carry; Claude Code's permission prompts are its approvals, and Ren answers no
other request (hooks, MCP, elicitation). Claude Code reports no live turn id
matching its recorded prompt id, so a driven turn's `turn` is its prompt's
message id, not the recorded turn's `native_id`.

Pi asks for no approvals: it runs its tools as it decides, so a Pi run never
waits on the operator mid-turn. Its turn lasts from the prompt until Pi
settles (`agent_settled`), after any retries or compaction; Pi names no turns,
so the daemon mints each driven turn's id, which is not the recorded turn's
`native_id`. A Pi run resumes the session of that id in the run's own folder.
Dialogs an extension opens are cancelled. The daemon publishes Ren's Pi
extension to `$XDG_CACHE_HOME/ren/pi` (else `~/.cache/ren/pi`) before starting
Pi, so the daemon's environment needs one of them.

OpenCode (2.x) is driven over ACP, JSON-RPC on stdio, with the same
`OPENCODE_CONFIG_CONTENT` override as `ren run opencode`. OpenCode names its own
sessions (`ses_…`), so a run's `thread` is the UUID derived from that name, the
same one its recorded calls carry in `x-opencode-session-id`. To continue a
conversation, the daemon lists the folder's sessions (`session/list`) and
reopens the one whose name derives the thread (`session/resume`, which replays
nothing). OpenCode does not echo a prompt while its turn runs, so the run's
conversation shows the prompt the daemon sent. A turn lasts until OpenCode
answers its `session/prompt`; OpenCode names no turns, so the daemon mints each
turn's id. Its `session/request_permission` requests are the run's approvals;
Ren gives OpenCode no file system or terminal of its own and answers any other
request with an error. Models on OpenCode's own `opencode` provider, including
its free models, reach OpenCode's service directly, not through this gateway,
so their calls are not recorded.

A run starts on the model chosen for it, or on the client's own default. The
models offered are the ones the client itself lists: the daemon starts it
briefly in the same structured mode, in the first root, with no conversation
(Codex's `model/list`, without the models Codex hides; the models in Claude
Code's `initialize` answer, with `--no-session-persistence`; Pi's
`get_available_models` with `--no-session`, marked with the model `get_state`
names; the `model` option of a session OpenCode opens in the first root,
without Ren's provider override, marked with its current value, which the
daemon then deletes), then stops it. An OpenCode session that chose no model
starts on the model OpenCode listed as its default, since under the override
OpenCode would take a model of a provider it may have no key for. Each model also says which efforts it takes, least
first, and the one it runs at when none is chosen, where the client says
(Codex does; Claude Code lists only the levels; Pi's reasoning models take
`off` to `high`, and the one it starts on runs at its current thinking level;
OpenCode lists the efforts of its default model only).

A run's settings are its `model`, its `effort` (`off`, `minimal`, `low`,
`medium`, `high`, `xhigh`, or `max`), and its `approvals`, what the client may
do without asking: `ask` (edits and commands wait for you), `edits` (edits in
the folder go ahead, commands wait), or `auto` (the client decides what is
safe and asks beyond that). Each left out is the client's own configuration.
They go to the client the way it takes them:

| Setting     | Codex                                                                                                                                                                   | Claude Code                                             | Pi                        | OpenCode                                                                                                           |
| ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- | ------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `model`     | `thread/start` or `thread/resume` `model`                                                                                                                               | `--model`                                               | `--model provider/id`     | `session/set_config_option` `model` (`provider/id`)                                                                |
| `effort`    | each `turn/start`'s `effort`                                                                                                                                            | `--effort`                                              | `--thinking`              | `session/set_config_option` `effort` (the model's variant)                                                         |
| `approvals` | `approvalPolicy` and `sandbox` at `thread/start` or `thread/resume`: `untrusted` and `read-only`; `untrusted` and `workspace-write`; `on-request` and `workspace-write` | `--permission-mode` `default`, `acceptEdits`, or `auto` | none: Pi asks for nothing | `permissions` rules in its configuration: `ask` makes `shell` and `edit` ask, `edits` makes `shell` ask; no `auto` |

A start with `approvals` a client does not take (any for Pi, `auto` for
OpenCode) answers `400` `no_approvals`. A run's settings
are recorded with it and with its session, shown on the run, and kept when its
session's client starts again.

Driving Claude Code with a Claude subscription is the operator signing in to
the unmodified `claude` program on their own machine, which Anthropic's
[terms for Claude Code](https://code.claude.com/docs/en/legal-and-compliance)
allow; offering that sign-in or its usage to other people is not. Allow only
your own Tailscale login to drive runs (`[control]`, and `REN_CONTROL_LOGINS`
for the web UI), and keep runs to ordinary, individual use: plan limits assume
it.
Runs are off while `runs.roots` is empty: every route then answers `404`
`runs_disabled`. Access is the control API's.

A run works in a folder under a root, in a folder of another place, or in
scratch. Other places (`[runs] places`, absolute or `~/…`) are folders
outside the projects, such as `~/.config`, that the UIs offer only when asked.
Scratch is a folder Ren owns (`[runs] scratch`, by default
`$XDG_DATA_HOME/ren/scratch`, else `~/.local/share/ren/scratch`): a run
started without a folder gets a new one of its own there, a git repository,
kept after the run ends. A root itself, a home, and anything outside these
are never offered, and a path that resolves outside them is refused.

```toml
[runs]
roots = ["/home/op/dev"]
places = ["~/.config", "~/dotfiles"]
# scratch = "~/scratch"
```

| Endpoint                                                 | Body        | Answer                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| -------------------------------------------------------- | ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `GET /control/v1/runs`                                   | none        | `{api_version, runs}`: each `{run, session, harness, workdir, state, settings}`, `session` the operator session the run serves (a session is served by one run after another)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `GET /control/v1/runs/projects`                          | none        | `{api_version, projects, scratch}`: every place a run can start in, each `{path, name, shown, kind, git}`. `kind` `project`: the git repositories under the run roots (not looked inside, three levels down at most, hidden, `node_modules`, `target`, `vendor`, `dist`, and `build` folders skipped), and each root's folders with none below them; `other`: each `[runs] places` folder's own folders, hidden ones included, or the place itself when it is a repository; `scratch`: the folders earlier scratch runs worked in. `shown` is the path under the home as `~/…`; `git` one of `{"kind": "branch", name}`, `detached` with its `commit`, `unreadable`, or `none`. `scratch` is the folder scratch runs get theirs in, or `null`                                                                                                                                                                                                               |
| `GET /control/v1/runs/clients`                           | none        | `{api_version, clients}`: every client, each `{harness, status, approvals, midway, sign_in, sign_in_methods}`, `approvals` the policies it can run under (none for Pi), `midway` what a running conversation takes: `steer` (a message into the running turn: Codex, Pi, OpenCode) and the settings that change from the next turn (`model`, `effort`, `approvals`: Codex all three, Claude Code `model` and `approvals`, Pi and OpenCode `model` and `effort`), `status` one of `ready` (Ren drives it and its program is on the daemon's path), `not_installed`, or `not_drivable` (Ren cannot drive it; all four clients can be driven); `sign_in` its [sign-in](#client-sign-in) as last checked, and `sign_in_methods` how Ren can sign it in (`claude_subscription` and `anthropic_console` for Claude Code, `chatgpt` for Codex, and `chatgpt`, `anthropic_api_key`, and `openai_api_key` for Pi and OpenCode). Waits for sign-in checks running now |
| `GET /control/v1/runs/clients/{harness}/models`          | none        | `{api_version, harness, models}`: the models the client itself lists on the daemon's host, in its order, each `{id, name, description, provider, choice, efforts, effort}`; `provider` is set where the client offers several (Pi), and `choice` is `default` for the one a run gets when none is chosen, else `other`. The daemon lists every installed client's models as it starts, so the first ask is answered at once; the first non-empty answer is kept while the daemon runs; an empty list (a client not signed in) is asked again next time. `502` `models_unavailable`, with a `message`, when the client refused, exited, or took over 20 seconds                                                                                                                                                                                                                                                                                              |
| `POST /control/v1/runs/clients/{harness}/sign-in`        | `{method?}` | `{api_version, harness, method, login}`: the client's own login, started on the daemon's host (replacing one running), answered once it prints its link or after 20 seconds; see [Client sign-in](#client-sign-in)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `POST /control/v1/runs/clients/{harness}/sign-in/answer` | `{answer}`  | `{api_version, harness, outcome}`: what the running login waits for (`answer`: the page's code, an API key, or the pasted address) handed to it, answered once it ended and the client's sign-in was checked again                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `POST /control/v1/runs/clients/{harness}/sign-in/check`  | `{}`        | `{api_version, harness, sign_in}`: the client's status command, run now                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `POST /control/v1/runs/clients/{harness}/sign-in/cancel` | `{}`        | `{api_version, ok}`: the running login stopped                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `POST /control/v1/runs/clients/{harness}/sign-in/wait`   | `{}`        | `{api_version, harness, waited}`: the running login's end and the check after it, waited on for at most 20 seconds                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `GET /control/v1/runs/events`                            | none        | server-sent `run` events from now on; `lagged` when some were skipped                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |

A run is one client process serving a [session](#sessions); it is driven
through its session, whose commands answer `202` `{api_version}` once sent to
the agent, and what it does next arrives as run events. A run's `state` has a
`phase`: `starting`, `idle` (with `thread`, the client's
conversation ID), `working` (with `turn` once known, `queued`, the message waiting for the
turn to end, when there is one, and `approvals`, each
`{approval, ask}` waiting for an answer, oldest first: `ask` is a `command`
with its `command`, `cwd`, and `reason`, a `file_change` with its
`reason`, or a `tool` (another of the client's tools) with its `tool` and
`detail`), `stopping` (with its `cause`), or `ended`
(with `end`, one of `spawn_failed`, `open_refused`, `exited`, `stopped`,
`drained`, `recovered`, `idled`, and `exit`, `{"kind": "code"|"signal", "value": n}` or
`{"kind": "unknown"}`). Run events are `{"kind": "state", run, state}` or
`{"kind": "event", run, event}`, where `event` is what the agent reported: its
`delta` text, `item`s (each with its `kind`, its `read`, what Ren read of it
the same way for every client: `{"as": "prompt"|"reply"|"plan", text}`,
`{"as": "command", command, status, output, exit_code}`,
`{"as": "file_change", paths, status}`, `{"as": "tool", name, status}`,
`{"as": "reasoning"}`, or `{"as": "unread"}`; and its `body` as the client
described it), `approval` requests, `turn_started` and `turn_ended`, and
`error`s. Every event but the streamed `delta` text is also stored, as the
[run's conversation](#run-conversation), and its model traffic is recorded like
any other client's.

### Client sign-in

Ren can sign a client in on the daemon's host by running the client's own
commands. The client signs itself in and keeps and refreshes what it receives;
Ren relays the link its login prints and what the login asks the operator for,
and never stores, logs, or records the codes, keys, or addresses it hands over,
or the client's credentials. A login that needs a terminal runs on a
pseudo-terminal of its own, echo off, and anything it prints is cleaned of
what Ren typed into it.

| Client      | Status commands                                                      | Methods and their logins                                                                                                                                                                                                   |
| ----------- | -------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Claude Code | `claude auth status --json`                                          | `claude_subscription`, `anthropic_console`: `claude auth login`, a link and the code the page shows                                                                                                                        |
| Codex       | `codex login status` (only how it is signed in is kept, never a key) | `chatgpt`: `codex login --device-auth`, a link and a one-time code to enter there                                                                                                                                          |
| Pi          | `pi auth check --json` for `openai-codex`, `anthropic`, and `openai` | `chatgpt`: `pi auth login openai-codex --no-browser`, a device code; `anthropic_api_key`, `openai_api_key`: `pi auth login <provider> --api-key`. On a terminal                                                            |
| OpenCode    | `opencode auth list --standalone`                                    | `chatgpt`: `opencode auth login openai --method chatgpt-token-sharing`, a link whose approval sends the browser to a `127.0.0.1` address on the host; `anthropic_api_key`, `openai_api_key`: `--method key`, on a terminal |

Anthropic does not permit Claude subscriptions in third-party clients, so Pi
and OpenCode are offered Anthropic API keys only.

A client's `sign_in` is `{"kind": "signed_in", method}` (`method` in the
client's own words, such as `claude.ai`, or Pi's ready providers),
`signed_out`, `unknown` (with a `reason`: a status command could not be run or
read), `checking` (nothing read yet), or `not_checked` (it is not installed).
It is checked as the daemon starts, after each login, and after any turn of
that client fails. A signed-out client still starts; its turns fail until it is
signed in, and the web UI sends nothing to it.

A `login` is `{"kind": "starting", said}`; `awaiting_code` (with `link`, an
`https` page to open, and `said`, the last other line the client printed);
`awaiting_approval` (a device login: `link`, the one-time `code` to enter
there, and `said`); `awaiting_key` (`said`); `awaiting_redirect` (`link`, and
`said`: the address the browser cannot open after approving is pasted back);
`finishing` (the answer was handed over; `link` or `null`); or `ended` (with
`end`). Starting answers once the operator has what they need. `…/answer` takes
what the login waits for as `answer`: the page's code, an API key, or the
pasted address, which the daemon opens on the host (one `GET`, no redirects)
only when it is the loopback address the link sends to and carries a `code`
and a `state`. A `waited` is `{"kind": "going", login}` or
`{"kind": "done", outcome}`: each wait lasts at most 20 seconds, so a device
login is waited on again until it is done; after it ended, a wait answers its
outcome until another login starts. An `outcome` is `{end, state}`: `end` is
`{"end": "exited", exit, said}`, `cancelled`, `timed_out` (no answer or
approval within 15 minutes, or no finish within 15 seconds of an answer), or
`broken` (with `said`, such as an address that could not be opened); `state`
is the client's `sign_in` as checked after it. An exit of `0` is not taken as a
sign-in: only `state` says whether it worked. Refusals: `409`
`sign_in_not_offered`, `not_installed`, `no_sign_in` (no login running),
`not_awaiting_answer`, `redirect_not_an_address`, `redirect_elsewhere` (not the
address the login waits on), or `redirect_incomplete` (no code: not approved
yet); `400` `invalid` for an empty or multi-line answer or a method the client
does not offer.

The web UI's Sessions view drives [sessions](#sessions) through this API when
its server is given the control token file (see the
[UI's README](../ui/README.md#sessions)), and `ren runs` drives them from the
command line.

A client sitting idle between turns for ten minutes is stopped: its run ends
`idled` (shown as Resting), and its session goes on, its next prompt starting
the client again to resume the conversation.

In the web UI, what is written while a turn runs is queued for after it (Enter),
shown under the conversation, and can be taken back to edit; it can instead go
into the running turn (Ctrl+Enter or Send now) where the client takes that. Typing `/` at the start of a prompt
offers the client's own commands and skills (`$` offers Codex's skills
anywhere), and `@` the run's folder's files; ↑ and ↓ choose, Tab or Enter
inserts, Esc closes. Each session keeps its own draft, ↑ and ↓ recall its earlier prompts, and writing to one whose
client stopped continues it. The model, effort, and approvals
change from the next turn, and a draft (no prompt sent yet) can change its
client; a folder's `+` makes a session there at once.

Refusals: `400` `invalid_directory` (not absolute, contains `..`, or outside
every root once links are resolved), `400` `directory_not_found`, `400`
`not_drivable` (a client Ren cannot drive), `502` `spawn_failed` (the
client is not installed; the run is recorded as `spawn_failed`), `503`
`unrecorded` (no room to record the run), `503` `unavailable` (the run's process went away while the command was on its way), `404` `not_found` for an unknown run or approval, and `409` when
the run's state does not allow the command: `starting`, `busy`, `not_working`,
`turn_not_started`, `no_such_approval`, `stopping`, `ended`, `nothing_queued`
(an unqueue with nothing waiting), or `not_offered` (a steer or a settings
change the client does not take mid-run); and `409` `protocol` when the client's
own protocol refuses the command in its present state.

## Live feed

`GET /api/live` streams `observation` events, one per committed observation
(`call_opened`, `attempt_started`, `response_head`, `attempt_finished`,
`call_finished`, `call_recovered`, `channel_closed`, `operator_action`,
`run_started`, `run_opened`, `run_ended`, `run_recovered`, `history_edited`,
`run_configured`, `session_created`, `session_configured`, `session_client_chosen`,
`session_archived`, `session_unarchived`). Each event's `id` is its sequence number. Without
`after`, only new observations are sent; with `after` (or a `Last-Event-ID` header,
which browsers send on reconnect) the feed first replays retained observations after
that position. A `gone` event (`{"oldest_retained": <seq>}`) means the position was
removed by retention: reload and reconnect without `after`. A `lagged` event means the subscriber fell behind
and the feed is catching up from storage. An `error` event means the daemon could not read its store; the feed then ends.
The stream also ends when the daemon shuts down.
Observations describe what was recorded, not model or tool state.

With `progress=true`, the feed also sends `progress` events while it follows:
`{call, reply, cut, tools, thinking}` for a call whose response is streaming, at
most one every 250 ms per call and only when it grew or the model started or
stopped thinking. `reply` is the last 2,000 characters
of the reply so far, with credentials in well-known formats redacted (the last
4,000 are scanned first, so one across the cut is still found); `cut` says the
reply is longer; `tools` names the tool calls begun, without their inputs; `thinking` says the
response so far ends inside a thinking block or reasoning item, whose text is
not sent. They
have no `id`: they are never stored, never replayed on resume, and a feed that
falls behind skips some. The finished call's activity is the record. The UI asks
for them, to show replies on Live cards as they stream;
`ren tail` does not.

## Call search

`GET /api/calls` filters all retained calls and returns `{calls, next}` newest
first. Parameters: `q`, `session` (a recording ID), `client` (`claude_code`,
`codex`, `pi`, `opencode`), `user` (a [named user](policy.md#named-users)), `kind` (`model`, `other`), `provider` (`anthropic`, `openai`,
`codex`, `deepseek`), `model`, `tool`, `has_tools`, `status` (`active`, `finished`, `errors`,
`interrupted`), `from_ms`, `until_ms` (inclusive), `limit` (1–1000), and `after`.
Filters combine with AND; unknown parameters are rejected.

- `client` matches the harness whose own headers or metadata attributed the call;
  a call with no attribution matches no client.
- `user` matches the calls a named user made, proven by its Tailscale login or
  token; each call's `caller` says how.
- `kind=model` keeps calls that ask a model for output (messages and responses,
  over HTTP or as a WebSocket message); `kind=other` keeps the rest: token counts,
  model listings, the WebSocket handshakes that open a channel, and a client's
  own checks. Claude Code's quota probe, sent as it starts, is one: a one-token
  request whose only message is `quota`, which Anthropic may answer with `429`.
  A call counts as a model call until its digest says otherwise. Each listed
  call carries that reading as `purpose` (`work` or `quota_probe`), `null` until
  its digest is derived.

- `q` matches call IDs, reported models, and observed tool names,
  case-insensitively; `%` and `_` are literal. Search strings are at most 128 UTF-8
  bytes. Bodies are never searched.
- Model and tool filters use the response inspection saved when a call finishes,
  so active calls and calls without observations don't match them.
- `errors` covers HTTP 4xx/5xx, upstream/stream errors, and reported model failures;
  `interrupted` is separate.
- Pass `next` unchanged with the same filters. The cursor survives retention of its
  source row. Pages are live queries, not snapshots: restart from the first page to
  see newly arrived matches.

## Body previews

`GET /api/calls/{id}/body/{request|response}` returns a bounded preview of the
first 256 KiB, not a full export; use `ren inspect --body … --output …` for
complete captured bytes. The response body is the final attempt's. Previews decode
gzip and Zstandard (recognised by their magic bytes) and Brotli (when the attempt
recorded `Content-Encoding: br`, since Brotli has no magic bytes; `decoding.applied`
is then `brotli`) where possible and report the extent (`complete`, `partial`,
`truncated`, or `gap`), the observed size (`null` while the body is still open),
and decoding integrity explicitly. Preview work is bounded and independent of
forwarding. Bodies may contain private prompts or tool results and share the
management API's private access boundary. The UI renders them as inert text and
never stores them; see [UI payload previews](../ui/README.md#payload-previews) for
limits.
