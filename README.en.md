# LLMApi

English | [简体中文](README.md) | [Changelog](CHANGELOG.md)

`llmapi` is a standalone Rust CLI and HTTP proxy that converts between OpenAI Chat
Completions, OpenAI Responses, and Anthropic Messages APIs. It also supports
provider round-robin and failover, model allowlists, and relaying requests to
another llmapi server.

## Quick start

Run `cargo install --path . --locked` from the repository directory, then
create `llmapi.toml`.

```toml
server = "127.0.0.1:8080"
apikey = "${LLMAPI_API_KEY}"
default = "openai"

[providers.openai]
type = "responses"
baseurl = "https://api.openai.com/v1"
apikey = "${OPENAI_API_KEY}"
```

Set `OPENAI_API_KEY` to your upstream API key and `LLMAPI_API_KEY` to the key
clients use to access this server. Start the server, then check the active
configuration from a second terminal.

```bash
llmapi --config llmapi.toml server
```

```bash
curl -H "Authorization: Bearer $LLMAPI_API_KEY" http://127.0.0.1:8080/providers
```

Use `http://127.0.0.1:8080/v1` as the client base URL for the default provider,
or `http://127.0.0.1:8080/openai/v1` to select `openai` explicitly. See the
[TOML example](examples/config.toml) and [YAML example](examples/config.yaml).

## Commands

```bash
llmapi version # also --version or -v; version comes from CARGO_PKG_VERSION
llmapi list # aliases: ls, l
llmapi usage [--name <provider>] [--date <YYYY-MM-DD> | --date-range <START> <END>]
llmapi check [provider | --all | -a] [--model <model>] [--timeout <seconds>]
llmapi completion [bash|elvish|fish|powershell|zsh] # alias: comp
llmapi add <name> --type <chat|responses|messages|gemini|llmapi> --baseurl <url> [--apikey|--apikey-stdin] # alias: a
llmapi add <name> --type lb --members <provider,provider>
llmapi rm <name>
llmapi set default <provider>
llmapi server [--server 127.0.0.1:8080] [--default <provider>]
llmapi daemon install [--workdir <path>] [--args server ...]
llmapi daemon start|stop|restart|status|uninstall|remove
```

Use `llmapi check -a` or `llmapi check --all` to check every provider in name order. Each provider gets its own `--timeout`. Checks continue after failures and the command exits with a nonzero status if any provider fails.

`check` probes a provider without starting the server. It uses the provider
named by the argument, or the configured default when omitted. Without
`--model`, it requests `GET /models` and reports the model count; with
`--model`, it sends a minimal completion request instead. The command prints
the provider, status, latency, and a short response summary, then exits with a
non-zero status for transport errors or non-2xx responses. `--timeout` defaults
to 30 seconds and is the total budget for the command. For a load-balancer
provider, `check` tries members in configuration order starting from the first
one, reports the member and attempt count that answered, and uses the same
failure classification as request routing.

`completion` writes a shell completion script to stdout. With no shell
argument, it uses the current shell when detected, or Bash otherwise.

`add --apikey` prompts without echoing the key. For piped input, use
`printf '%s\n' "$API_KEY" | llmapi add <name> --type chat --baseurl <url> --apikey-stdin`.
`add` rejects empty keys and plaintext key arguments. Omit both flags to leave
the upstream key unset. `remove` (alias `rm`) deletes a provider; deleting the
default selects the first remaining provider by name. The last provider can
also be removed, unless an `lb` provider references it. Saved provider keys
remain in the config file as plaintext.

On macOS and Linux, `daemon` uses `daemon-rs` as its backend. The working
directory defaults to the current directory and the argument list defaults to
`server`. Use `--args` to replace the full argument list. `daemon uninstall`
stops the service before removing its definition, and `remove` is its alias.

On macOS, the user LaunchAgent is installed at
`~/Library/LaunchAgents/com.virzz.enyo.llmapi.plist` and logs to
`~/Library/Logs/com.virzz.enyo.llmapi/`. When invoked as root, the command
manages `/Library/LaunchDaemons/com.virzz.enyo.llmapi.plist` and logs to
`/Library/Logs/com.virzz.enyo.llmapi/`. Use `daemon restart` to apply an updated
plist and restart the service.

On Linux, `daemon` manages systemd over D-Bus. The user unit is installed at
`${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/llmapi.service` and starts with
the user's systemd session. When invoked as root, the command manages
`/etc/systemd/system/llmapi.service`, which starts at boot. Use `daemon start`
to start it immediately after installation.

```bash
llmapi daemon install --workdir "$PWD" --args server --config llmapi.toml
llmapi daemon start
llmapi daemon status
journalctl --user -u llmapi.service -f
```

`daemon restart` reloads the unit and restarts the service. Uninstalling also
disables the service. systemd reads an optional `llmapi.env` in the working
directory, which can supply environment variables referenced by the configuration.

Configuration is loaded from `--config <path>` when specified; otherwise from
`./config.{toml,yaml,yml,json}` in that order, then
`~/.config/enyo/llmapi.{yaml,toml,yml,json}` in that order. When none exists,
the fallback path is `~/.config/enyo/llmapi.yaml`.
An explicitly selected file never falls back; `add` can create a new file there.
The `server` command's `--server` and `--default` arguments override the selected
file's listen address and default provider without modifying the file.
Set top-level `apikey` (for example, `apikey: ${LLMAPI_API_KEY}`) to require a
client key on every HTTP route except `/health`. Clients can send `Authorization: Bearer <key>`,
`x-api-key`, `api-key`, or the `key` query parameter. Missing or incorrect keys
return 401. An unset top-level `apikey` keeps the server open; provider `apikey`
values remain separate upstream credentials.
Set top-level `access_log = true` (YAML: `access_log: true`) to write one access
log line per HTTP request to stdout, including the client address, method and
redacted path, response status, and handling latency. Access logs are written
for every request, including rejected ones, and default to off. When the server
reloads the config file, the setting takes effect for new requests.
While running, the server checks the selected file every 500 ms and applies valid
provider/default changes to new requests. Changes to `server` bind a new listener
before closing the old one; if binding fails, the old listener remains and the
server retries. Invalid or missing files keep the last valid config.
Changing the selected config path requires a restart.

## HTTP routes

| Client protocol    | Default provider    | Named provider                 |
| ------------------ | ------------------- | ------------------------------ |
| OpenAI Chat        | `/chat/completions` | `/{provider}/chat/completions` |
| OpenAI Responses   | `/responses`        | `/{provider}/responses`        |
| Anthropic Messages | `/messages`         | `/{provider}/messages`         |

All generation routes also accept `/v1`, as in `/v1/responses` and
`/{provider}/v1/responses`.

`/responses` also accepts a WebSocket upgrade when the selected provider uses
`openai-responses` or `llmapi`.

A concrete provider can set `models = ["gpt-5", "gpt-5-mini"]` to allow only
those case-sensitive model IDs. Omit `models` or use an empty list to allow
any model. Direct HTTP requests for disallowed models return `400`.
WebSocket requests that violate the restriction close the connection.
`check --model` also honors the list.

`GET /models` and `GET /v1/models` (also under `/{provider}`) accept upstream
OpenAI `data` or Codex `models` lists and filter them by the selected
provider's model allowlist. Codex requests (identified by
`client_version` or a Codex User-Agent) receive `models`; other clients receive
OpenAI `data`. Missing metadata is synthesized when converting between formats.

`GET /providers` returns the active default and all configured providers. Each
configured `apikey` is returned as `***`; unset keys are omitted. The endpoint
does not resolve environment variables or contact upstream providers.

## Health and upstream discovery

Unauthenticated `GET /health` returns only `{"service":"llmapi","instance_id":"<unique ID>"}`. On first startup, `server` generates a UUID and saves it as the top-level `instance_id`. It survives restarts and IP changes. Remove that top-level ID when copying a configuration to an independent instance.

Providers with `type = "llmapi"` are checked at startup and record the upstream `instance_id` on first successful contact. During normal operation, discovery runs only after connection failures, not HTTP error statuses or response-stream failures. Newly added or edited providers are checked on first use.

If a known instance is unreachable or its identity differs, discovery scans the original IPv4 `/24` (`.1`–`.254`), preserving the scheme, port and path. Only a unique matching `instance_id` is accepted; the provider's `baseurl` is then atomically saved and the connection retried. Probes use at most 32 concurrent requests, a 750 ms timeout each, and no API credentials. Duplicate matching IDs or an unreachable provider without a known ID cannot be automatically rebound. Hostnames and IPv6 addresses are not scanned.

```toml
[providers.remote]
type = "llmapi"
baseurl = "http://192.168.1.10:8080/v1"
instance_id = "ID returned by upstream /health" # Optional until first successful contact
```

Automatic updates require a writable configuration file and reserialize it, losing comments and original formatting. Embedded `serve` has no config path and keeps discovery state in memory; embedded `app` only supplies routes, leaving identity and discovery management to the caller.

## Token usage

`usage` shows the active UTC date range and provider filter, groups rows by provider / model, and adds a total row. Tokens use decimal `K / M / B / T` suffixes with up to two decimal places (for example, `12.35K` or `1.2M`); request counts use thousands separators. Cache and reasoning columns are hidden only when entirely unreported; reported zeros remain visible. `-` means unreported. Totals sum reported values only, without deriving missing totals or adding cache/reasoning counts to them again.

Token usage recording is enabled by default. The first write creates `usage.db` beside the configuration file. Set the top-level `usage_db = false` to disable recording. A custom relative database path resolves against the configuration file’s directory.

The server and the reporting CLI use Turso multiprocess WAL on a supported local filesystem. Each usage update commits atomically, and a report reads a consistent snapshot.

The proxy records usage reported by the upstream under the serving provider and model. Cumulative streaming updates share one row. Missing counters remain SQL `NULL`. For OpenAI Chat streams, request usage with `stream_options.include_usage = true`.

Usage snapshots are written asynchronously through a queue of 1024 entries. A full queue or a database write failure logs a warning and drops that snapshot. Pending snapshots can be lost when the process exits. Interaction creation, polling, and resumed streams share a record keyed by provider and interaction ID, with cumulative counters only increasing.

```toml
usage_db = false
```

Usage is grouped by provider and model. Names match providers exactly. Dates are in UTC and both range endpoints are inclusive.

```sh
llmapi usage
llmapi usage --name gemini --date 2026-11-24
llmapi usage --date-range 2026-11-01 2026-11-24
```

## Gemini upstream

Unsupported non-default conversion parameters (such as Chat `n: 2`, `seed`, and `logprobs: true`) return `400`. Chat `reasoning_effort`, Responses `reasoning`, and Messages `thinking` / `output_config.effort` are not currently mapped to Gemini thinking settings; non-empty settings return `400`. Omit them to use model defaults. Native Interactions requests are unaffected by this conversion restriction.

With tools enabled, Chat / Responses `parallel_tool_calls: false` and Messages `disable_parallel_tool_use: true` return `400` because Gemini cannot guarantee at most one call per turn. Chat streams include upstream usage before `[DONE]` only when `stream_options.include_usage: true`.

Gemini tool-call `thoughtSignature` values round-trip through `extra_content.google.thought_signature` on Chat, Responses, and Messages output items. Clients must preserve this extension when sending tool-call history back.

```toml
[providers.gemini]
type = "gemini"
baseurl = "https://generativelanguage.googleapis.com/v1"
apikey = "${GEMINI_API_KEY}"
```

Use a Gemini model such as `gemini-2.5-flash` through
`/gemini/v1/chat/completions`, `/gemini/v1/responses`, or `/gemini/v1/messages`.
The proxy calls Google's `generateContent` or `streamGenerateContent` endpoint
according to the request's `stream` field. List models at `/gemini/v1/models` or
probe generation with `llmapi check gemini --model gemini-2.5-flash`.

`GET /gemini/v1/models` converts the Gemini model list to OpenAI or Codex
format, preserving model names, token limits, and `nextPageToken`. Use
`pageSize` and `pageToken` to request another page.

The Interactions API uses the same Gemini provider. Request bodies, upstream
responses, and SSE events are forwarded in their native format. Use the
provider that created the interaction for all subsequent operations.

| Method | Route | Operation |
| --- | --- | --- |
| POST | `/gemini/v1/interactions` | Create |
| GET | `/gemini/v1/interactions/{id}` | Get or resume SSE |
| POST | `/gemini/v1/interactions/{id}/cancel` | Cancel |
| DELETE | `/gemini/v1/interactions/{id}` | Delete |

Omit the `/gemini` prefix when Gemini is the default provider. The `baseurl`
sets the upstream API version.

## llmapi relay

```toml
[providers.relay]
upstream = "llmapi" # Also accepts type = "llmapi".
baseurl = "https://remote.example.com/pool/v1"
apikey = "${REMOTE_LLMAPI_API_KEY}"
```

The relay forwards Chat Completions, Responses, Messages, SSE, and model lists
in the client's protocol. The remote llmapi selects the upstream and performs
protocol conversion. Set `baseurl` to the remote server address for its default
provider, or append a prefix such as `/pool/v1` to select a named provider.
Responses WebSocket upgrades require support from the selected remote provider.
Set `apikey` to the remote llmapi server key. `check --model` uses the remote
Chat Completions endpoint.

## Load-balancer provider

`type = "lb"` groups existing providers and serves requests with equal-weight
round-robin. Each request takes one starting position and then tries members in
configuration order, so `[A, B, C]` receives requests at `A`, `B`, `C`, `A`, ...
and a request starting at `B` tries `B`, `C`, `A`. A member is tried at most
once per request, and failover never advances the shared cursor a second time.

```toml
default = "pool"

[providers.primary]
type = "chat"
baseurl = "https://primary.example.com/v1"
apikey = "${PRIMARY_API_KEY}"

[providers.secondary]
type = "responses"
baseurl = "https://secondary.example.com/v1"
apikey = "${SECONDARY_API_KEY}"

[providers.pool]
type = "lb"
members = ["primary", "secondary"]
```

```bash
llmapi add pool --type lb --members primary,secondary
llmapi set default pool
llmapi list
```

`lb` providers do not accept `baseurl`, `apikey`, or `models`; `members` must be a
non-empty, ordered list without duplicates. Members must be existing concrete
providers: unknown names, self references, and nested `lb` providers are
rejected. A provider referenced by an `lb` group cannot be removed until that group is
changed or deleted. Deleting an `lb` group leaves its members intact.

Failover applies only to requests that go through an `lb` group:

| Result                                                                   | Behavior                          |
| ------------------------------------------------------------------------ | --------------------------------- |
| DNS, connection, TLS, send, or response-header transport failure         | try the next member               |
| 30-second response-header timeout                                        | try the next member               |
| Upstream `401`, `403`, `408`, `429`, `500..=599`                         | try the next member               |
| Missing member credentials                                               | skip the member                   |
| Requested model excluded by the member's allowlist                       | skip the member                   |
| Other upstream statuses (for example `400`, `404`, `422`)                | return the response               |
| Client auth, routing, method, or request conversion errors               | return the error immediately      |
| Parse failures, read failures, or stream breaks after a response is chosen | keep the existing error handling |
| Redirects (`3xx`)                                                        | return the response and its `Location`; never follow |

Every member attempt waits at most 30 seconds for response headers and reuses
the 15-second connect timeout; an established SSE stream is not limited by the
30-second value. Load-balanced requests never follow redirects automatically,
so one member attempt maps to at most one upstream request. `check` applies the
same per-member header cap while keeping `--timeout` as the total budget. Up to `N × 30` seconds may pass before `N` candidates are
exhausted, excluding response-body reading. When every member fails with a
retryable result, the client receives `503 Service Unavailable` in its own
protocol's error shape, naming the `lb` group and a short failure summary.
Member `Retry-After` metadata is never presented as the group's state. The
request body is read once and re-converted from the original client input for
each member, so cross-protocol failover does not reuse another member's
converted body or credentials. `upstream_raw` keeps the selected member's raw
response. Failover does not guarantee exactly-once upstream execution, and
`idempotency-key` is not deduplicated across providers.

`GET /models` (also under `/v1` and `/{provider}`) uses the same round-robin
cursor and returns the selected member's catalog without merging directories.
Only direct providers without a model restriction forward `If-None-Match`
and retain the existing `ETag` behavior.
Session state stored by an upstream (`previous_response_id`, `conversation`)
should be sent to a concrete provider route, because an `lb` group may switch
members between requests. Model names are not mapped.

Each config version keeps its own cursors and publishes them atomically with
the config. A valid config change starts the new cursors at zero and leaves
in-flight requests on the old version; invalid reloads keep the previous
version. Requests to a concrete provider do not consume an `lb` cursor.

`lb` groups do not support WebSocket requests; an upgrade is rejected with HTTP
400 before connecting upstream. There are no background health checks, circuit
breakers, nested groups, weights, sticky sessions, or cross-process cursor
coordination.

See [examples/config.yaml](examples/config.yaml) for the provider configuration format.

## Development

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```
