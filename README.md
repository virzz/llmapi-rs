# llmapi

`llmapi` is a standalone Rust CLI and HTTP proxy that converts between OpenAI Chat
Completions, OpenAI Responses, and Anthropic Messages APIs.

## Commands

```bash
llmapi list
llmapi add <name> --type <chat|responses|messages> --baseurl <url> [--apikey|--apikey-stdin]
llmapi rm <name>
llmapi set default <provider>
llmapi server [--server 127.0.0.1:8080] [--default <provider>]
llmapi daemon install [--workdir <path>] [--args server ...]
llmapi daemon start|stop|restart|status|uninstall|remove
```

`add --apikey` prompts without echoing the key. For piped input, use
`printf '%s\n' "$API_KEY" | llmapi add <name> --type chat --baseurl <url> --apikey-stdin`.
`add` rejects empty keys and plaintext key arguments. Omit both flags to leave
the upstream key unset. `remove` (alias `rm`) deletes a provider; deleting the
default selects the first remaining provider by name. The last provider can
also be removed. Saved provider keys remain in the config file as plaintext.

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

`GET /models` and `GET /v1/models` (also under `/{provider}`) accept upstream
OpenAI `data` or Codex `models` lists. Codex requests (identified by
`client_version` or a Codex User-Agent) receive `models`; other clients receive
OpenAI `data`. Missing metadata is synthesized when converting between formats.

`GET /providers` returns the active default and all configured providers. Each
configured `apikey` is returned as `***`; unset keys are omitted. The endpoint
does not resolve environment variables or contact upstream providers.

See [examples/config.yaml](examples/config.yaml) for the provider configuration format.

## Development

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```
