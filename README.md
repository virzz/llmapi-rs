# LLMApi

[English](README.en.md) | 简体中文

`llmapi` 是一个独立的 Rust CLI 和 HTTP 代理，用于在 OpenAI Chat Completions、OpenAI Responses 和 Anthropic Messages API 之间转换协议。

## 命令

```bash
llmapi list
llmapi check [provider] [--model <model>] [--timeout <seconds>]
llmapi completion [bash|elvish|fish|powershell|zsh] # 别名: comp
llmapi add <name> --type <chat|responses|messages> --baseurl <url> [--apikey|--apikey-stdin]
llmapi rm <name>
llmapi set default <provider>
llmapi server [--server 127.0.0.1:8080] [--default <provider>]
llmapi daemon install [--workdir <path>] [--args server ...]
llmapi daemon start|stop|restart|status|uninstall|remove
```

`check` 在不启动服务的情况下探测 provider。使用参数指定的 provider；省略参数时使用配置的默认 provider。不带 `--model` 时请求 `GET /models` 并报告模型数量；带 `--model` 时改为发送一个最小补全请求。命令会输出 provider、状态、延迟和简短响应摘要，并在传输错误或非 2xx 响应时以非零状态退出。`--timeout` 默认 30 秒。Codex 检查使用本地 Codex 登录。

`completion` 将 shell 补全脚本写入 stdout。未指定 shell 参数时使用检测到的当前 shell，否则使用 Bash。

`add --apikey` 以不回显的方式提示输入密钥。管道输入使用
`printf '%s\n' "$API_KEY" | llmapi add <name> --type chat --baseurl <url> --apikey-stdin`。
`add` 拒绝空密钥和明文密钥参数。省略两个参数时不设置上游密钥。`remove`（别名 `rm`）删除 provider；删除默认 provider 后按名称选择剩余的第一个 provider。最后一个 provider 也可以删除。保存的 provider 密钥以明文保留在配置文件中。

在 macOS 上，`daemon install` 将用户 LaunchAgent 写入
`~/Library/LaunchAgents/com.virzz.enyo.llmapi.plist`。默认工作目录是当前目录，默认参数是 `server`。`--args` 会替换完整参数列表。日志写入
`~/Library/Logs/com.virzz.enyo.llmapi/`。安装和删除只修改文件；使用 `daemon start` 或 `daemon stop` 改变 launchctl 状态。`remove` 是 `uninstall` 的别名；两者都不会删除日志。
`daemon restart` 卸载并重新引导服务，以应用更新后的 plist。
当 launchctl 没有匹配的服务时，`daemon status` 报告 `not loaded` 或 `not installed`；其他 launchctl 错误仍按错误处理。

配置按以下顺序加载：指定 `--config <path>` 时使用该文件；否则依次查找
`./config.{toml,yaml,yml,json}`，再依次查找
`~/.config/enyo/llmapi.{yaml,toml,yml,json}`。全部不存在时，回退路径为
`~/.config/enyo/llmapi.yaml`。显式选择的文件不会回退；`add` 可以在该路径创建新文件。`server` 命令的 `--server` 和 `--default` 参数会覆盖所选文件中的监听地址和默认 provider，但不修改文件。
设置顶层 `apikey`（例如 `apikey: ${LLMAPI_API_KEY}`）后，所有 HTTP 路由都要求客户端密钥。客户端可以通过 `Authorization: Bearer <key>`、`x-api-key`、`api-key` 或 `key` 查询参数发送密钥。缺失或错误的密钥返回 401。未设置顶层 `apikey` 时服务器保持开放；provider 的 `apikey` 仍是独立的上游凭据。
设置顶层 `access_log = true`（YAML：`access_log: true`）后，每个 HTTP 请求都会向 stdout 写入一行访问日志，包含客户端地址、方法、脱敏后的路径、响应状态和处理耗时。所有请求都会记录，包括被拒绝的请求；默认关闭。服务器重新加载配置文件后，该设置对新请求生效。
服务运行期间，每 500 ms 检查一次所选配置文件，并将有效的 provider/默认 provider 变更应用到新请求。修改 `server` 时先绑定新监听器，再关闭旧监听器；绑定失败时保留旧监听器并重试。CLI 覆盖始终有效。无效或缺失的文件保留最后一次有效配置；修改所选配置路径需要重启。

## HTTP 路由

| 客户端协议         | 默认 provider       | 命名 provider                  |
| ------------------ | ------------------- | ------------------------------ |
| OpenAI Chat        | `/chat/completions` | `/{provider}/chat/completions` |
| OpenAI Responses   | `/responses`        | `/{provider}/responses`        |
| Anthropic Messages | `/messages`         | `/{provider}/messages`         |

当选中的 provider 使用 `openai-responses` 时，`/responses` 也接受 WebSocket 升级；WebSocket 消息会透明转发到该 provider。

`GET /models` 和 `GET /v1/models`（也支持 `/{provider}` 前缀）接受上游 OpenAI `data` 或 Codex `models` 列表。Codex 请求（通过 `client_version` 或 Codex User-Agent 识别）收到 `models`；其他客户端收到 OpenAI `data`。格式转换时会补齐缺失的元数据。

`GET /providers` 返回当前默认 provider 和所有已配置 provider。每个已配置的 `apikey` 返回为 `***`；未设置的密钥会被省略。该端点不会展开环境变量，也不会联系上游 provider。

provider 配置格式见 [examples/config.yaml](examples/config.yaml)。

## Codex ChatGPT provider

当 `auth_mode` 为 `chatgpt` 且存在 `tokens.access_token` 时，`llmapi` 会自动发现
`~/.codex/auth.json`。无需 provider 条目或 API 密钥。服务启动时读取该文件，此后每 **5 分钟**读取一次，包括文件尚不存在的情况。凭据缺失、不可读、无效或不是 ChatGPT 登录时，会在下一次轮询时停用自动 provider。

```bash
llmapi server
# 或者不添加 provider 条目，直接将 Codex 设为默认：
llmapi set default codex
```

使用 `/codex/responses`、`/codex/chat/completions`、`/codex/messages` 和
`/codex/models`（也支持 `/codex` 后加 `/v1`）。自动 provider 会出现在
`llmapi list` 和 `GET /providers` 中。已配置的 provider 及其默认 provider 保持有效；名为 `codex` 的已配置 provider 优先。没有配置文件和 provider 时，Codex 在可用时成为运行时默认 provider。显式选择的配置文件缺失时仍返回错误。

代理使用本地访问令牌和账户 ID 访问
`https://chatgpt.com/backend-api/codex`。它**不会刷新令牌，也不会写入
auth.json**。请继续由 Codex 管理登录；更新最多需要五分钟才能到达 llmapi。上游 401 会直接返回，不重试。凭据不会写入 llmapi 配置，也不会通过 provider 列表暴露。仅使用 `~/.codex/auth.json`；不会发现仅存在于 Keychain 的登录。

支持流式和非流式 HTTP 请求。后端始终使用 `store=false` 的 Responses SSE；非流式客户端会收到最终 JSON 响应。原生 Responses 流会保留上游事件。该 provider 不支持 WebSocket 请求。

Codex 请求会省略不受支持的采样/输出限制字段（`temperature`、`top_p`、`stop`、`max_output_tokens` 和 `max_completion_tokens`），包括从 Chat 或 Messages 转换而来的输出限制。这些设置对后端没有作用。`user`、`safety_identifier` 和 `prompt_cache_options` 也会被省略；仅转发 `priority` 和 `ultrafast` 服务层级。存储的会话引用（`previous_response_id`、`conversation` 和输入 `item_reference`）会被拒绝；请发送完整历史。模型访问权限和使用限制取决于 ChatGPT 账户。

## 开发

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```
