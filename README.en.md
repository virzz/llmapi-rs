# LLMApi

English | [简体中文](README.md)

`llmapi` is a standalone Rust CLI and HTTP proxy that converts between OpenAI Chat
Completions, OpenAI Responses, and Anthropic Messages APIs.

## Commands

```bash
llmapi list
llmapi check [provider] [--model <model>] [--timeout <seconds>]
llmapi completion [bash|elvish|fish|powershell|zsh] # alias: comp
llmapi add <name> --type <chat|responses|messages> --baseurl <url> [--apikey|--apikey-stdin]
llmapi add <name> --type lb --members <provider,provider>
llmapi rm <name>
llmapi set default <provider>
llmapi server [--server 127.0.0.1:8080] [--default <provider>]
llmapi daemon install [--workdir <path>] [--args server ...]
llmapi daemon start|stop|restart|status|uninstall|remove
```

`check` probes a provider without starting the server. It uses the provider
named by the argument, or the configured default when omitted. Without
`--model`, it requests `GET /models` and reports the model count; with
`--model`, it sends a minimal completion request instead. The command prints
the provider, status, latency, and a short response summary, then exits with a
non-zero status for transport errors or non-2xx responses. `--timeout` defaults
to 30 seconds and is the total budget for the command. For a load-balancer
provider, `check` tries members in configuration order starting from the first
one, reports the member and attempt count that answered, and uses the same
failure classification as request routing. Codex checks use the local Codex
login.

`completion` writes a shell completion script to stdout. With no shell
argument, it uses the current shell when detected, or Bash otherwise.

`add --apikey` prompts without echoing the key. For piped input, use
`printf '%s\n' "$API_KEY" | llmapi add <name> --type chat --baseurl <url> --apikey-stdin`.
`add` rejects empty keys and plaintext key arguments. Omit both flags to leave
the upstream key unset. `remove` (alias `rm`) deletes a provider; deleting the
default selects the first remaining provider by name. The last provider can
also be removed, unless an `lb` provider references it. Saved provider keys
remain in the config file as plaintext.

On macOS, `daemon install` writes a user LaunchAgent to
`~/Library/LaunchAgents/com.virzz.enyo.llmapi.plist`. The default working
directory is the current directory and the default argument is `server`.
`--args` replaces the full argument list. Logs go to
`~/Library/Logs/com.virzz.enyo.llmapi/`. Installation and removal only change
files; use `daemon start` or `daemon stop` to change launchctl state. `remove`
is an alias for `uninstall`; neither command deletes logs.
`daemon restart` unloads and bootstraps the service to apply an updated plist.
`daemon status` reports `not loaded` or `not installed` when launchctl has no
matching service; other launchctl errors remain errors.

Configuration is loaded from `--config <path>` when specified; otherwise from
`./config.{toml,yaml,yml,json}` in that order, then
`~/.config/enyo/llmapi.{yaml,toml,yml,json}` in that order. When none exists,
the fallback path is `~/.config/enyo/llmapi.yaml`.
An explicitly selected file never falls back; `add` can create a new file there.
The `server` command's `--server` and `--default` arguments override the selected
file's listen address and default provider without modifying the file.
Set top-level `apikey` (for example, `apikey: ${LLMAPI_API_KEY}`) to require a
client key on every HTTP route. Clients can send `Authorization: Bearer <key>`,
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
server retries. CLI overrides remain in effect. Invalid or missing files keep
the last valid config; changing the selected config path requires a restart.

## HTTP routes

| Client protocol    | Default provider    | Named provider                 |
| ------------------ | ------------------- | ------------------------------ |
| OpenAI Chat        | `/chat/completions` | `/{provider}/chat/completions` |
| OpenAI Responses   | `/responses`        | `/{provider}/responses`        |
| Anthropic Messages | `/messages`         | `/{provider}/messages`         |

`/responses` also accepts a WebSocket upgrade when the selected provider uses
`openai-responses`; WebSocket messages are forwarded transparently to that
provider.

`GET /models` and `GET /v1/models` (also under `/{provider}`) accept upstream
OpenAI `data` or Codex `models` lists. Codex requests (identified by
`client_version` or a Codex User-Agent) receive `models`; other clients receive
OpenAI `data`. Missing metadata is synthesized when converting between formats.

`GET /providers` returns the active default and all configured providers. Each
configured `apikey` is returned as `***`; unset keys are omitted. The endpoint
does not resolve environment variables or contact upstream providers.

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

`lb` providers do not accept `baseurl` or `apikey`; `members` must be a
non-empty, ordered list without duplicates. Members must be existing concrete
providers: unknown names, self references, and nested `lb` providers are
rejected. The automatically discovered `codex` provider may be referenced even
when it is not configured; it is skipped at request time when credentials are
unavailable. A configured provider named `codex` takes precedence. A provider
that is referenced by an `lb` group cannot be removed until that group is
changed or deleted. Deleting an `lb` group leaves its members intact.

Failover applies only to requests that go through an `lb` group:

| Result                                                                   | Behavior                          |
| ------------------------------------------------------------------------ | --------------------------------- |
| DNS, connection, TLS, send, or response-header transport failure         | try the next member               |
| 30-second response-header timeout                                        | try the next member               |
| Upstream `401`, `403`, `408`, `429`, `500..=599`                         | try the next member               |
| Missing member credentials                                               | skip the member                   |
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
Load-balanced model requests do not forward `If-None-Match` and do not expose a
member `ETag`; direct providers keep their conditional-request behavior.
Session state stored by an upstream (`previous_response_id`, `conversation`)
should be sent to a concrete provider route, because an `lb` group may switch
members between requests. Model names are not mapped: put providers that serve
the same models and capabilities in one group.

Each config version keeps its own cursors and publishes them atomically with
the config. A valid config change starts the new cursors at zero and leaves
in-flight requests on the old version; invalid reloads keep the previous
version. Requests to a concrete provider do not consume an `lb` cursor, and
Codex credential refreshes do not reset cursors.

`lb` groups do not support WebSocket requests; an upgrade is rejected with HTTP
400 before connecting upstream. There are no background health checks, circuit
breakers, nested groups, weights, sticky sessions, or cross-process cursor
coordination.

See [examples/config.yaml](examples/config.yaml) for the provider configuration format.

## Codex ChatGPT provider

`llmapi` automatically discovers `~/.codex/auth.json` when `auth_mode` is
`chatgpt` and `tokens.access_token` is present. No provider entry or API key is
needed. It reads the file at server startup and every **5 minutes**, including
when the file does not yet exist. Missing, unreadable, invalid, or non-ChatGPT
credentials disable the automatic provider on the next poll.

```bash
llmapi server
# Or select Codex as the default without adding a provider entry:
llmapi set default codex
```

Use `/codex/responses`, `/codex/chat/completions`, `/codex/messages`, and
`/codex/models` (also with `/v1` after `/codex`). The automatic provider appears
in `llmapi list` and `GET /providers`. Configured providers and their default
remain in effect; a configured provider named `codex` takes precedence. With
no config file or providers, Codex becomes the runtime default when available.
An explicitly selected missing config file still returns an error.

The proxy uses the local access token and account ID for
`https://chatgpt.com/backend-api/codex`. It **never refreshes tokens or writes
auth.json**. Keep the login managed by Codex; updates can take up to five
minutes to reach llmapi. An upstream 401 is returned without retrying. Credentials
are not written to llmapi config or exposed by the provider listing. Only the
file at `~/.codex/auth.json` is used; Keychain-only logins are not discovered.

Both streaming and non-streaming HTTP requests are supported. The backend
always uses Responses SSE with `store=false`; non-streaming clients receive
the final JSON response. Native Responses streams preserve upstream events.
WebSocket requests are not supported for this provider.

Codex requests omit unsupported sampling/output-limit fields (`temperature`,
`top_p`, `stop`, `max_output_tokens`, and `max_completion_tokens`), including
output limits converted from Chat or Messages. Those settings have no effect
on the backend. `user`, `safety_identifier`, and `prompt_cache_options` are also
omitted; only `priority` and `ultrafast` service tiers are forwarded. Stored
conversation references (`previous_response_id`, `conversation`, and input
`item_reference`) are rejected; send the full history instead. Model access
and usage limits follow the ChatGPT account.

## Development

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```
