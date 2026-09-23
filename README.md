# llmapi

`llmapi` is a standalone Rust CLI and HTTP proxy that converts between OpenAI Chat
Completions, OpenAI Responses, and Anthropic Messages APIs.

## Commands

```bash
llmapi list
llmapi add <name> --type <chat|responses|messages> --baseurl <url> [--apikey <key>]
llmapi set default <provider>
llmapi server [--server 127.0.0.1:8080] [--default <provider>]
llmapi daemon install [--workdir <path>] [--args server ...]
llmapi daemon start|stop|status|uninstall|remove
```

On macOS, `daemon install` writes a user LaunchAgent to
`~/Library/LaunchAgents/com.virzz.enyo.llmapi.plist`. The default working
directory is the current directory and the default argument is `server`.
`--args` replaces the full argument list. Logs go to
`~/Library/Logs/com.virzz.enyo.llmapi/`. Installation and removal only change
files; use `daemon start` or `daemon stop` to change launchctl state. `remove`
is an alias for `uninstall`; neither command deletes logs.

Configuration is loaded from `--config <path>` when specified; otherwise from
`./config.toml`, then `./config.yaml`, then `~/.config/enyo/llmapi.yaml`.
An explicitly selected file never falls back; `add` can create a new file there.
The `server` command's `--server` and `--default` arguments override the selected
file's listen address and default provider without modifying the file.
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
