# llmapi

`llmapi` is a standalone Rust CLI and HTTP proxy that converts between OpenAI Chat
Completions, OpenAI Responses, and Anthropic Messages APIs.

## Commands

```bash
llmapi list
llmapi add <name> --type <chat|responses|messages> --baseurl <url> [--apikey <key>]
llmapi set default <provider>
llmapi server [--server 127.0.0.1:8080] [--default <provider>]
```

Configuration is loaded from `--config <path>` when specified; otherwise from
`./config.toml`, then `./config.yaml`, then `~/.config/enyo/llmapi.yaml`.
An explicitly selected file never falls back; `add` can create a new file there.
The `server` command's `--server` and `--default` arguments override the selected
file's listen address and default provider without modifying the file.
While running, the server checks the selected file every 500 ms and applies valid
provider/default changes to new requests. CLI overrides remain in effect. Invalid
or missing files keep the last valid config; changing the listen address or the
selected config path requires a restart.

## HTTP routes

| Client protocol | Default provider | Named provider |
| --- | --- | --- |
| OpenAI Chat | `/chat/completions` | `/{provider}/chat/completions` |
| OpenAI Responses | `/responses` | `/{provider}/responses` |
| Anthropic Messages | `/messages` | `/{provider}/messages` |

`GET /models` and `GET /v1/models` (also under `/{provider}`) accept upstream
OpenAI `data` or Codex `models` lists. Codex requests (identified by
`client_version` or a Codex User-Agent) receive `models`; other clients receive
OpenAI `data`. Missing metadata is synthesized when converting between formats.

See [examples/config.yaml](examples/config.yaml) for the provider configuration format.

## Development

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```
