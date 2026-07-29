# llmapi

`llmapi` is a standalone Rust CLI and HTTP proxy that converts between OpenAI Chat
Completions, OpenAI Responses, and Anthropic Messages APIs.

## Commands

```bash
llmapi list
llmapi add <name> --type <chat|responses|messages> --baseurl <url> [--apikey <key>]
llmapi set default <provider>
llmapi server [--server 127.0.0.1:8080]
```

The default configuration path remains `~/.config/enyo/llmapi.yaml` for compatibility
with the Enyo command.

## HTTP routes

| Client protocol | Default provider | Named provider |
| --- | --- | --- |
| OpenAI Chat | `/chat/completions` | `/{provider}/chat/completions` |
| OpenAI Responses | `/responses` | `/{provider}/responses` |
| Anthropic Messages | `/messages` | `/{provider}/messages` |

See [examples/config.yaml](examples/config.yaml) for the provider configuration format.

## Development

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```
