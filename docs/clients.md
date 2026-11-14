# Clients

How to point Claude Code, Codex, Pi, and OpenCode at Ren, and how Ren
attributes their traffic to recordings and turns.

Live subscription traffic has been exercised with Claude Code and Codex. Pi has
been validated against local mock providers; live Pi subscription access is
unvalidated. OpenCode runs driven by the daemon have been exercised with
OpenCode 2.2.7 on its free `opencode/big-pickle` model: a turn with tools and
edits, an approval, an interrupt, and resuming in a new process. Those calls do
not pass through the gateway, so `ren run opencode` and OpenCode's `anthropic`
and `openai` traffic through Ren follow OpenCode's documentation and source and
are covered by unit tests with synthetic headers; they have not been run
against Ren.

## Diagnose and launch

With the daemon running, use session-only launch commands:

```sh
ren doctor
ren doctor --json
ren run claude
ren run codex -- --model <model-id>
ren run pi -- --model anthropic/<model-id>
ren run opencode -- --model anthropic/<model-id>
ren run codex --dry-run
```

`doctor` checks local configuration, upstream URL validity, read-only history,
Unix storage mode bits, the clients, runtimes, and utilities on `PATH` (each
with the version it reports), and the daemon's health and storage APIs. It reports capture loss separately from storage failures,
and warns when the daemon runs a different build from the CLI (see
[builds](api.md#builds)).
It runs each program it finds only as `<program> --version`, for at most about
three seconds; it does not read credential files, run credential helpers, test
provider connectivity, or consume subscription allowance. Missing clients are
warnings, as is a missing runtime a client found here needs (Pi needs `node`);
other missing tools are skipped. Exit code is 0 with no failed checks (warnings can remain), 1 on a failed
check, and 2 for invalid CLI syntax. JSON output includes `ok` and `checks`.
These are readiness checks, not a full database-integrity or disk-space check.

`run` checks the gateway before launching, then applies only process-scoped
overrides. Claude receives its base URL and correlation-header flag. Codex gets
a complete `ren_session` provider override with OpenAI authentication, and
`--no-daemon` keeps the session out of an already-running shared Codex server.
OpenCode gets inline configuration naming two base URLs (see [OpenCode](#opencode)).
Existing model, approval, sandbox, sign-in, and credential-refresh behavior stays
owned by the client. Ren does not modify client configuration files or credentials.
On macOS/Linux the client replaces the launcher, preserving terminal interaction,
signals, and exit status. A normal client launch outside Ren restores normal routing.

Pass client arguments after `--`; Ren passes them literally without a shell.
Explicit client routing/auth options or managed client settings can supersede
the launcher's settings. API keys and token overrides already present in your
environment are retained, with a name-only warning: verify the client uses your
intended subscription login. `--dry-run` prints only Ren's generated arguments and
environment overrides, excluding inherited credentials and your client arguments;
it performs no network or client operations. Launch options target Claude Code
2.1.316, Codex CLI 0.164.0, Pi 1.0.0, and OpenCode 2.2.6.

### Named users

On a gateway with [named users](policy.md#named-users), a user who is not
identified by a Tailscale login sets the token `ren users token` printed for
them, and `run` sends it:

```sh
export REN_USER_TOKEN=renu_…
ren run claude
```

Claude Code receives it in `ANTHROPIC_CUSTOM_HEADERS`, after any headers you
set there (Claude Code 2.1.227 or later). Codex's provider override always names
it in `env_http_headers`, so Codex sends it whenever `REN_USER_TOKEN` is set. The
Pi extension reads `REN_USER_TOKEN` and refuses to load if it is set but is not a
Ren user token. OpenCode does not send it: where its configuration takes
extra headers is not confirmed, so its calls are recorded by Tailscale login or
as unidentified, and `run` says so. `--dry-run` shows the token as
`<redacted>`. The gateway removes the header before forwarding.

`doctor` (and the preflight of `run` and `init`) reports how the gateway records
your calls, from [`/api/caller`](api.md#endpoints): as a user by token or by
Tailscale login, as an unlisted login, or as unidentified. A token the gateway
does not know fails the check.

For a remote daemon, save its trusted Tailscale HTTPS **root** URL once:

```sh
ren init --gateway https://ren.your-tailnet.ts.net
ren run claude
ren run codex -- --model <model-id>
```

`ren init` checks the gateway's health and saves the URL only if the check
passes, to `$XDG_CONFIG_HOME/ren/client.toml` (or `~/.config/ren/client.toml`),
owner-only. Without `--gateway`, it asks on a terminal, offering the saved URL
as the default; with no terminal it fails instead of waiting. This file is
separate from the daemon's `ren.toml`. `run` and `doctor` choose the gateway in
this order: `--gateway`, the `REN_GATEWAY` environment variable, `client.toml`,
then the configured local listener. `doctor` reports which one it used. When
nothing names a gateway and the local daemon fails its preflight, `run` says so
and points to `ren init`; no command prompts on its own.

A selected gateway skips local history/upstream checks; remote storage is
queried through the API. Every history command (`history`, `sessions`, `timeline`,
`turns`, `turn`, `activity`, `search`, `exposures`, `report`, `inspect`),
`ren tail`, `ren mcp`, and `ren control` use the same gateway; `--config` or `--data-dir`
without `--gateway` keeps them on this host (see [History](api.md#cli) and
[operator controls](policy.md#operator-controls)).
Root URLs cannot contain credentials, queries, fragments, or provider suffixes;
HTTP is allowed only on loopback. Diagnostic requests send no provider credentials,
disable redirects/retries/environment proxies, and cap each response at 64 KiB
and five seconds. Client requests still carry client-owned credentials to the
selected gateway. No inference is sent by `doctor` or launch preflight.

The launch mechanism follows [Claude's gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol),
[Codex's per-run configuration overrides](https://learn.chatgpt.com/docs/config-file/config-advanced#one-off-overrides-from-the-cli),
and [OpenAI authentication for custom providers](https://learn.chatgpt.com/docs/auth#alternative-model-providers).

### Tools for agents

`ren tools` installs runtimes and utilities on this host from the catalog
built into this Ren: Node.js, Bun, jq, fd, and ripgrep, each at one pinned
release for Linux x64, Linux arm64, and macOS arm64. The clients themselves and
git are only found on `PATH`, never installed.

The operator adds packages in the daemon's configuration, in the built-in
catalog's form: anything published as a prebuilt archive or binary per
platform, such as a crate's release artifacts. An entry with a built-in
runtime's or utility's id replaces it (to pin another Node.js release, say);
clients stay built in, and two packages may not provide one program. The
configuration is refused at load otherwise.

```toml
[[tools.package]]
id = "acme-lint"
kind = "utility" # or "runtime"
program = "acme-lint"

[tools.package.release]
version = "0.3.0"

[[tools.package.release.artifact]]
platform = "linux_x64" # linux_arm64, macos_arm64
url = "https://github.com/acme/acme-lint/releases/download/v0.3.0/acme-lint-x86_64-unknown-linux-gnu.tar.gz"
sha256 = "<64 hex digits; GitHub shows each release asset's digest>"
format = "tar_gz" # zip, bare
program_at = "acme-lint-x86_64-unknown-linux-gnu/acme-lint"
```

Taking an entry out leaves its install in the current generation, listed as
not in the catalog, until it is removed.

```sh
ren tools list
ren tools install node jq
ren tools remove jq
ren tools rollback 2    # a new generation holding what generation 2 held
ren tools which jq      # the program a new run finds
ren tools gc
```

Each download comes over https and must match the SHA-256 the catalog pins;
anything else is refused and nothing changes. Installs go under the data
directory (`tools/`), each in its own folder, and every change makes a new
numbered generation, a folder of links (`tools/current/bin`) switched in one
step. `remove` leaves files in place until `gc` deletes every generation but
the current one and the installs it does not hold. `list --json` prints the
same listing as JSON.

Every run Ren starts gets the current generation's `bin` first on its `PATH`,
then the daemon's own, and records that generation (`toolset` in
[`/api/runs`](api.md#runs)). A run keeps the generation it started with: a
later change makes a new generation for later runs, and `gc` keeps every
generation a run without an end still uses (as the database records it).
With nothing installed, runs get the daemon's `PATH` unchanged.

The web UI's Tools view shows the same on the daemon's host, filterable by
name or path and by source: a row per package with the version runs use and
where it comes from (Ren's install,
first on their `PATH` over any copy the daemon's `PATH` has, or that copy);
staged changes, as the versions runs use before and after; the earlier
toolsets, as history; and which clients need what. Installs, updates, and removals are staged and then
applied as one generation; it needs the UI server's `REN_CONTROL_TOKEN_FILE`,
as Sessions does. Over the control API it is [`/control/v1/tools`](api.md#tools).

## Native endpoints

| Client endpoint                            | Upstream path                          |
| ------------------------------------------ | -------------------------------------- |
| `POST /anthropic/v1/messages`              | `/v1/messages`                         |
| `POST /anthropic/v1/messages/count_tokens` | `/v1/messages/count_tokens`            |
| `GET /anthropic/v1/models`                 | `/v1/models`                           |
| `POST /codex/responses`                    | `/backend-api/codex/responses`         |
| `POST /codex/responses/compact`            | `/backend-api/codex/responses/compact` |
| `GET /codex/models`                        | `/backend-api/codex/models`            |
| `POST /openai/v1/responses`                | `/v1/responses`                        |
| `POST /openai/v1/responses/compact`        | `/v1/responses/compact`                |
| `GET /openai/v1/models`                    | `/v1/models`                           |
| `POST /deepseek/v1/messages`               | `/anthropic/v1/messages`               |

The configured upstream bases determine the upstream path prefix. Query strings
and end-to-end headers are forwarded, including subscription authentication and
provider capability headers. Hop-by-hop headers are removed and Host is set for
the configured upstream. Redirects, automatic retries, response decompression,
and implicit environment proxies are disabled.

An operator-key upstream configured with `ingress = "direct"` also has routes of
its own below `/upstreams/<id>`: its wire format's paths above, such as
`POST /upstreams/<id>/v1/messages` (point Claude Code's `ANTHROPIC_BASE_URL` at
`/upstreams/<id>`), or `POST /upstreams/<id>/v1/chat/completions` for an entry on
the Chat Completions wire, which has no provider route of its own. See
[Direct routes](policy.md#direct-routes).

Ren does not sign in, refresh tokens, or substitute an API credential, except an
operator-key upstream's own key. The client owns authentication. No requests are
made to providers merely by starting Ren. For the sessions Ren drives, the web
UI can run a client's own login on the daemon's host
([Signing Claude Code in](#signing-claude-code-in)).

## Claude Code

With an existing subscription login, launch a session through Ren:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:8787/anthropic CLAUDE_CODE_GATEWAY_HINT_HEADERS=1 claude
```

An active API key, bearer-token override, or credential helper can supersede that
login. Keep the subscription login active rather than supplying a replacement
credential for Ren. This is the subscription-preserving gateway configuration
[documented by Anthropic](https://code.claude.com/docs/en/llm-gateway-protocol#request-headers).

### DeepSeek

To run Claude Code against DeepSeek's
[Anthropic-compatible API](https://api-docs.deepseek.com/guides/anthropic_api)
through Ren, point it at `/deepseek` with your own DeepSeek API key:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:8787/deepseek ANTHROPIC_API_KEY=<deepseek key> CLAUDE_CODE_GATEWAY_HINT_HEADERS=1 claude
```

The key passes through to DeepSeek untouched; Ren never holds it. Calls are
recorded with provider `deepseek`. DeepSeek documents only Messages there, so
Ren routes only `POST /deepseek/v1/messages`: Claude Code's token counting and
model listing get `404` from Ren.

### Signing Claude Code in

Sessions run Claude Code on the daemon's host under that host's sign-in. When
it is signed out, the Sessions view says so above the composer and in the
client picker, with **Sign in**. That runs `claude auth login` on the
daemon's host: choose a Claude subscription or an Anthropic Console account,
open the link it printed (on any device), approve, and paste the code the page
shows. Claude Code reads the code, stores its own sign-in, and refreshes it;
Ren only passes the link and the code through, and records neither. Ren then
runs `claude auth status` to confirm. The same works on a VPS with no browser.
Through the API: [Client sign-in](api.md#client-sign-in).

## Chat Completions clients

Clients that speak OpenAI Chat Completions (Aider, Goose, Cline, Qwen Code, and
most OpenAI SDK code) reach Ren through an operator-key upstream with
`wire = "openai_chat_completions"` and `ingress = "direct"`
([Direct routes](policy.md#direct-routes)):

```toml
[[upstreams]]
id = "deepseek-chat"
provider = "deepseek"
wire = "openai_chat_completions"
base_url = "https://api.deepseek.com"
operator_key_env = "REN_DEEPSEEK_API_KEY"
ingress = "direct"
```

Point the client's OpenAI base URL at `http://127.0.0.1:8787/upstreams/deepseek-chat/v1`.
Ren forwards `POST /v1/chat/completions` and `GET /v1/models` below it, with the
operator key as `Authorization: Bearer`, whatever key the client sends. DeepSeek
documents its path as `/chat/completions`; it also answers on
`/v1/chat/completions`. The same works with `provider = "openai"` and
`https://api.openai.com`, or `provider = "anthropic"` and
`https://api.anthropic.com` (Anthropic's compatibility layer, meant for testing).

Usage is recorded only when the client asks for it in a stream
(`stream_options.include_usage`); without it the call has no usage, which says
nothing about how much was used. A stream that ends before `data: [DONE]` is
recorded without terminal evidence. No harness is attributed, so these calls have
no recording or turn. Local servers that need no key (Ollama, vLLM, llama.cpp, LM
Studio) are not supported: every Chat Completions entry holds an operator key.

## Codex

Merge a provider entry into your Codex configuration:

```toml
model_provider = "ren"

[model_providers.ren]
name = "Ren"
base_url = "http://127.0.0.1:8787/codex"
wire_api = "responses"
requires_openai_auth = true
supports_websockets = true
```

Keep your existing ChatGPT sign-in and model selection. Ren relays Codex's
Responses WebSocket and HTTP/SSE alike; `ren run codex` offers both. The
WebSocket is relayed message for message: each client and upstream message
passes unchanged and in order, each side answers its own pings, and no
compression extension is negotiated on either leg. To keep Codex on HTTP/SSE,
set `[websocket] mode = "refuse"`: handshakes then get `426 Upgrade Required`,
and Codex falls back to HTTP/SSE for the session. Handshakes beyond
`websocket.max_channels` (default 16) get the same `426`. Each call on a socket
counts against `max_concurrent_requests`; an idle socket does not. Ren pings
each leg after `websocket.keepalive_seconds` (default 30) of silence and closes
a socket whose client or upstream stays silent for twice that, so a vanished
client (a laptop asleep behind Tailscale Serve) does not hold a channel. A
running call whose upstream sends nothing within the response header timeout
(before its first message) or the stream idle timeout (after) closes the socket
too, as an HTTP stream would end; Codex reconnects for its next turn and resends
its full input. On shutdown,
idle sockets close with code `1012` (service restart); sockets with a call still
running close the same way once it ends, or when the drain deadline passes. See
OpenAI's [gateway authentication guide](https://learn.chatgpt.com/docs/enterprise/connect-to-a-gateway)
and [configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).

### Sandbox on Ubuntu

Codex runs its commands in its own sandbox, a user namespace made by its
built-in bubblewrap, under every approval policy Ren starts it with. Ubuntu
24.04 and later restrict unprivileged user namespaces through AppArmor
(`sysctl kernel.apparmor_restrict_unprivileged_userns` reads `1`), so the
sandbox cannot start: commands fail with
`bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted`, and Codex asks
before running each one outside the sandbox, whatever the approval policy.
`codex sandbox -- /bin/echo ok` shows it.

[`deploy/apparmor/codex`](../deploy/apparmor/codex) allows user namespaces for
the Codex binary only, as Ubuntu's own profiles do for other sandboxed programs,
and confines nothing else. It names Codex's standalone install
(`~/.codex/packages/standalone/releases/<version>/bin/codex`), so upgrades stay
covered; for another install, add its binary's path (`readlink -f "$(which
codex)"`) to the profile's attachment. Install and load it as root, then check:

```sh
sudo install -m 644 deploy/apparmor/codex /etc/apparmor.d/codex
sudo apparmor_parser -r /etc/apparmor.d/codex
codex sandbox -- /bin/echo ok
```

The last command prints `ok`. Running Codex sessions pick it up when their client
next starts. To undo it, `sudo apparmor_parser -R /etc/apparmor.d/codex` and
remove the file. Leaving the system-wide restriction on is the point: only this
binary is let through.

## Pi

Start Pi with its existing sign-in and model catalog:

```sh
ren run pi -- --model anthropic/<model-id>
ren run pi -- --model openai/<model-id>
ren run pi -- --model openai-codex/<model-id>
ren run pi --gateway https://ren.your-tailnet.ts.net -- --model anthropic/<model-id>
ren run pi --dry-run
```

The embedded extension redirects **`anthropic`**, **`openai`**, and
**`openai-codex`** to Ren, forces SSE for these providers, and delegates request
conversion, authentication, token refresh, tool replay, and retries to Pi. Sign in
through Pi's own `/login` for the provider you select. Ren does not import Claude
Code/Codex credentials, create a login, inspect tokens, or supply an API key.
Existing Pi API-key overrides still apply; the launcher itself does not guarantee
subscription billing.

Pi 1.0.0 calls `openai-codex` its ChatGPT provider; its **`openai`** provider uses
Sign in with ChatGPT and the public Responses endpoint. Ren keeps these
destinations separate:

| Pi provider    | Gateway base URL | Default upstream                        |
| -------------- | ---------------- | --------------------------------------- |
| `anthropic`    | `/anthropic`     | `https://api.anthropic.com`             |
| `openai-codex` | `/codex`         | `https://chatgpt.com/backend-api/codex` |
| `openai`       | `/openai/v1`     | `https://api.openai.com`                |

Set `openai_base_url` to the public OpenAI upstream **root** (default
`https://api.openai.com`). Route availability does not guarantee that a
subscription permits every endpoint. Ren records this destination as `openai`,
separately from `codex`.

For `openai`, the extension redirects Pi's HTTP fetch while retaining the native
model URL during serialization. Pi checks that URL when omitting fields unsupported
by ChatGPT sign-in; changing it directly would disable that handling. Ren delegates
serialization to Pi and does not rewrite the generated payload. Sign-in and refresh
requests still go directly from Pi to OpenAI. The endpoint and bearer-token flow
follow [OpenAI's ChatGPT plan inference documentation](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference).

Other providers keep their normal endpoints. Select a supported provider explicitly;
an existing default for another provider will bypass Ren. Later extensions or custom
provider registrations can supersede this adapter. Explicit `--extension` loading
still works with Pi's `--no-extensions`, which disables discovery.

The launcher extracts a hash-named extension into `$XDG_CACHE_HOME/ren/pi` (or
`$HOME/.cache/ren/pi`) and loads it for that Pi process. This leaves Pi's
configuration and credentials untouched and lets `/reload` reuse the same code.
Cache files are published atomically and checked against the bundled source on each
launch; symlinks and group/other-writable cache paths are rejected. `--dry-run`
shows an extension-path placeholder and performs no cache writes. Unused versions
can be removed once their Pi sessions stop. No Node/Bun installation is needed to
build Ren; the installed Pi runtime supplies the extension's provider APIs.

Pi sessions use the client's UUID. Each agent run gets a fresh extension-generated
turn UUID shared by tool follow-ups and retries. It is not a native Pi turn ID or
proof of tool execution. Session switches reset the run; requests outside a run
have no turn, and requests for another session do not inherit the active run.
Ren records these hints under `pi_extension_headers`, distinct from Claude Code
and Codex. The `x-ren-pi-session-id` and `x-ren-pi-turn-id` headers are consumed by
Ren and stripped before forwarding. Correlation remains scoped by upstream provider,
so switching providers creates separate recordings for the same Pi UUID.

Verify with the installed Pi CLI without real credentials or inference charges:

```sh
cargo build --locked
node --test crates/ren-harness/pi/ren.test.mjs
python3 scripts/smoke-pi.py
```

The smoke test exercises all three provider paths, stored OAuth-shaped
authentication, tool invocation/results, resumed sessions, turn grouping, unchanged
auth files, and removal of internal headers. For public OpenAI it also checks that
Pi omits ChatGPT-incompatible request fields. Everything uses temporary state and
mock providers. It does not validate live entitlement or token-refresh network
calls. The adapter follows Pi's [provider extension contract](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/custom-provider.md).

## OpenCode

OpenCode speaks Anthropic Messages and OpenAI Responses natively, so it uses the
same routes as Claude Code and Pi:

```sh
ren run opencode -- --model anthropic/<model-id>
ren run opencode -- --model openai/<model-id>
ren run opencode --gateway https://ren.your-tailnet.ts.net
ren run opencode --dry-run
```

The launcher sets `OPENCODE_CONFIG_CONTENT`, OpenCode's
[inline runtime configuration](https://opencode.ai/docs/config/#precedence-order),
to a provider override with only a [base URL](https://opencode.ai/docs/providers/#base-url)
for each of two providers:

| OpenCode provider | Gateway base URL | Default upstream            |
| ----------------- | ---------------- | --------------------------- |
| `anthropic`       | `/anthropic/v1`  | `https://api.anthropic.com` |
| `openai`          | `/openai/v1`     | `https://api.openai.com`    |

It merges over the global and project `opencode.json`, so your models, agents,
and other providers stay as configured; those other providers keep their own
endpoints. A value of `OPENCODE_CONFIG_CONTENT` already in your environment is
replaced for the session, with a notice; keep such settings in `opencode.json`.
Managed (administrator) OpenCode configuration still wins over the launcher.

OpenCode keeps its own sign-in and keys (`/connect`, `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`), and Ren adds no header that names another client. Anthropic
does not permit Claude subscription sign-in from third-party clients, so use an
Anthropic API key with OpenCode. OpenCode's ChatGPT sign-in sends requests to
ChatGPT's Codex backend with headers of its own; under `ren run opencode` its
`openai` requests go to the public Responses endpoint instead, which is
unvalidated for that sign-in. Use an OpenAI API key, or run OpenCode outside Ren
for ChatGPT sign-in.

OpenCode's `openai` provider opens a Responses WebSocket by default. Ren answers
the handshake on `/openai` with `426 Upgrade Required`, and OpenCode keeps that
session on HTTP/SSE after any failed connect.

Every model request OpenCode 2.x sends carries `x-opencode-session-id`, a
`ses_…` identifier rather than a UUID, on every provider. Ren recognises it on
every route, after Pi's extension headers, and records the hint under
`opencode_headers`. Native session hints are UUIDs, so the hint holds a version 8
UUID derived from the identifier (a namespaced SHA-256): one OpenCode session is
always the same recording, and the `ses_…` text itself is never stored.
OpenCode sends no turn identifier, so its calls have recordings but no turns. A
subagent runs in a child session of its own, as OpenCode shows it. Its other
headers (`x-opencode-project`, `x-opencode-client`, `x-session-affinity`, and the
parent session) are forwarded unchanged and not recorded.

Tool calls are read by OpenCode 2.x's names: `shell`, `read`, `write`, `edit`,
`patch` (the same envelope as Codex's `apply_patch`), `glob`, `grep`,
`subagent`, `webfetch`, and `websearch`. OpenCode reports a failed command by
ending its output with `Exited with code N` (from 2.2.7, `Exit code: N`),
`Killed by SIGNAL`, or `Timed out before completion`; before 2.2.7 a successful
command says nothing, so its status stays unreported. Its notices of finished background commands, subagents, and
commands the user ran (`<shell …>`, `<subagent …>`) are not counted as the
user's prompt.

Sessions can also drive OpenCode from the web UI or `ren runs`: the daemon runs
`opencode acp` with the same override, so its `anthropic` and `openai` calls
are recorded. OpenCode's own `opencode` provider, including the free models it
offers without sign-in (such as `opencode/big-pickle`), reaches OpenCode's
service directly, so those calls are not recorded; the run's conversation still
is. With the override, OpenCode counts its `anthropic` and `openai` providers as
set up whether or not it has a key for them, and would pick one of their models
as its default. So the daemon lists OpenCode's models without the override (the
models it can use as it is set up, and its own default), and a session that
chose no model starts on that listed default, by name. See [runs](api.md#runs) for how
its settings and approvals apply.

## Recordings and turns

Ren assigns its own typed recording and turn IDs while retaining native identifiers
and their source in each call's `attribution`. Claude Code uses
`x-claude-code-session-id` and `x-claude-code-prompt-id`; the environment flag above
enables prompt hints for custom gateways. These headers are
[documented by Anthropic](https://code.claude.com/docs/en/llm-gateway-protocol#request-headers).
Codex CLI 0.164.0 sends `session-id`, `thread-id`, and a `turn_id` inside
`x-codex-turn-metadata`; this integration is version-sensitive. OpenCode's
session header is described [above](#opencode).

Recordings are scoped by provider, metadata source, and native session ID. Turns
are scoped to their recording. Tool follow-up requests carrying the same IDs stay
in one turn. Native thread IDs are retained separately for Codex session trees.
Missing identifiers remain `null`; malformed, duplicate, or conflicting metadata
is flagged. Ren does not group by prompt text, cache keys, time, account, or request
IDs. These client-supplied hints are attribution evidence, never authentication,
proof of tool execution, or a signal that a session is idle or complete.

Correlation headers are parsed once, on arrival, into bounded typed hints (Codex
turn metadata is read only up to 8 KiB). Only allowlisted UUIDs are stored; the full
Codex metadata object and arbitrary headers are discarded; OpenCode's session
is stored only as its derived UUID. Correlation works with
metadata-only body capture.
