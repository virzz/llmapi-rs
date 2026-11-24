# LLMApi

[English](README.en.md) | 简体中文 | [变更日志](CHANGELOG.md)

`llmapi` 是一个独立的 Rust CLI 和 HTTP 代理，用于在 OpenAI Chat Completions、OpenAI Responses 和 Anthropic Messages API 之间转换协议。它也支持 provider 轮询与故障切换、按模型限制请求，以及中继到远端 llmapi。

## 快速开始

在仓库目录运行 `cargo install --path . --locked`，然后创建 `llmapi.toml`。

```toml
server = "127.0.0.1:8080"
apikey = "${LLMAPI_API_KEY}"
default = "openai"

[providers.openai]
type = "responses"
baseurl = "https://api.openai.com/v1"
apikey = "${OPENAI_API_KEY}"
```

设置环境变量 `OPENAI_API_KEY` 作为上游凭据，`LLMAPI_API_KEY` 作为客户端访问本服务的密钥。启动服务后，可从另一个终端检查当前配置。

```bash
llmapi --config llmapi.toml server
```

```bash
curl -H "Authorization: Bearer $LLMAPI_API_KEY" http://127.0.0.1:8080/providers
```

客户端可使用 `http://127.0.0.1:8080/v1` 访问默认 provider，或使用 `http://127.0.0.1:8080/openai/v1` 显式选择 `openai`。完整配置见 [TOML 示例](examples/config.toml) 和 [YAML 示例](examples/config.yaml)。

## 命令

```bash
llmapi list # 别名 ls、l
llmapi check [provider | --all | -a] [--model <model>] [--timeout <seconds>]
llmapi completion [bash|elvish|fish|powershell|zsh] # 别名: comp
llmapi add <name> --type <chat|responses|messages|gemini|llmapi> --baseurl <url> [--apikey|--apikey-stdin] # 别名 a
llmapi add <name> --type lb --members <provider,provider>
llmapi rm <name>
llmapi set default <provider>
llmapi server [--server 127.0.0.1:8080] [--default <provider>]
llmapi daemon install [--workdir <path>] [--args server ...]
llmapi daemon start|stop|restart|status|uninstall|remove
```

使用 `llmapi check -a` 或 `llmapi check --all` 按名称顺序检查所有 provider。每个 provider 独立使用 `--timeout`；遇到失败会继续检查，最后以非零状态退出。

`check` 在不启动服务的情况下探测 provider。使用参数指定的 provider；省略参数时使用配置的默认 provider。不带 `--model` 时请求 `GET /models` 并报告模型数量；带 `--model` 时改为发送一个最小补全请求。命令会输出 provider、状态、延迟和简短响应摘要，并在传输错误或非 2xx 响应时以非零状态退出。`--timeout` 默认 30 秒，是本次命令的总时间预算。对负载均衡 provider，`check` 从首成员开始按配置顺序探测，输出最终响应的成员和尝试次数，并使用与请求路由相同的失败分类。

`completion` 将 shell 补全脚本写入 stdout。未指定 shell 参数时使用检测到的当前 shell，否则使用 Bash。

`add --apikey` 以不回显的方式提示输入密钥。管道输入使用
`printf '%s\n' "$API_KEY" | llmapi add <name> --type chat --baseurl <url> --apikey-stdin`。
`add` 拒绝空密钥和明文密钥参数。省略两个参数时不设置上游密钥。`remove`（别名 `rm`）删除 provider；删除默认 provider 后按名称选择剩余的第一个 provider。最后一个 provider 也可以删除，但被 `lb` 组引用的 provider 必须先从组中移除。保存的 provider 密钥以明文保留在配置文件中。

macOS 和 Linux 的 `daemon` 命令由 `daemon-rs` 提供后端。默认工作目录是当前目录，默认参数是 `server`。`--args` 会替换完整参数列表。`daemon uninstall` 会先停止服务，再移除服务定义，`remove` 是它的别名。

在 macOS 上，普通用户安装的 LaunchAgent 位于 `~/Library/LaunchAgents/com.virzz.enyo.llmapi.plist`，日志写入 `~/Library/Logs/com.virzz.enyo.llmapi/`。以 root 运行时，命令管理 `/Library/LaunchDaemons/com.virzz.enyo.llmapi.plist`，日志写入 `/Library/Logs/com.virzz.enyo.llmapi/`。`daemon restart` 重新加载 plist 并重启服务。

在 Linux 上，`daemon` 通过 D-Bus 管理 systemd。普通用户安装的 unit 位于 `${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/llmapi.service`，随用户的 systemd 会话启动。以 root 运行时，命令管理 `/etc/systemd/system/llmapi.service`，服务随系统启动。安装后用 `daemon start` 立即启动。

```bash
llmapi daemon install --workdir "$PWD" --args server --config llmapi.toml
llmapi daemon start
llmapi daemon status
journalctl --user -u llmapi.service -f
```

`daemon restart` 重新加载 unit 并重启服务。卸载时还会禁用服务。systemd 会读取工作目录中的可选 `llmapi.env`，可用它为配置提供环境变量。

配置按以下顺序加载：指定 `--config <path>` 时使用该文件；否则依次查找
`./config.{toml,yaml,yml,json}`，再依次查找
`~/.config/enyo/llmapi.{yaml,toml,yml,json}`。全部不存在时，回退路径为
`~/.config/enyo/llmapi.yaml`。显式选择的文件不会回退；`add` 可以在该路径创建新文件。`server` 命令的 `--server` 和 `--default` 参数会覆盖所选文件中的监听地址和默认 provider，但不修改文件。
设置顶层 `apikey`（例如 `apikey: ${LLMAPI_API_KEY}`）后，所有 HTTP 路由都要求客户端密钥。客户端可以通过 `Authorization: Bearer <key>`、`x-api-key`、`api-key` 或 `key` 查询参数发送密钥。缺失或错误的密钥返回 401。未设置顶层 `apikey` 时服务器保持开放；provider 的 `apikey` 仍是独立的上游凭据。
设置顶层 `access_log = true`（YAML：`access_log: true`）后，每个 HTTP 请求都会向 stdout 写入一行访问日志，包含客户端地址、方法、脱敏后的路径、响应状态和处理耗时。所有请求都会记录，包括被拒绝的请求；默认关闭。服务器重新加载配置文件后，该设置对新请求生效。
服务运行期间，每 500 ms 检查一次所选配置文件，并将有效的 provider/默认 provider 变更应用到新请求。修改 `server` 时先绑定新监听器，再关闭旧监听器；绑定失败时保留旧监听器并重试。无效或缺失的文件保留最后一次有效配置；修改所选配置路径需要重启。

## HTTP 路由

| 客户端协议         | 默认 provider       | 命名 provider                  |
| ------------------ | ------------------- | ------------------------------ |
| OpenAI Chat        | `/chat/completions` | `/{provider}/chat/completions` |
| OpenAI Responses   | `/responses`        | `/{provider}/responses`        |
| Anthropic Messages | `/messages`         | `/{provider}/messages`         |

各生成路由均支持 `/v1`，例如 `/v1/responses` 和 `/{provider}/v1/responses`。

当选中的 provider 使用 `openai-responses` 或 `llmapi` 时，`/responses` 也接受 WebSocket 升级。

普通 provider 可配置 `models = ["gpt-5", "gpt-5-mini"]`，按区分大小写的模型 ID 限制生成请求。省略 `models` 或设置为空列表时不限制模型。直连 HTTP 请求使用未列出的模型时返回 `400`，WebSocket 请求违反限制时关闭连接，`check --model` 也遵守此限制。

`GET /models` 和 `GET /v1/models`（也支持 `/{provider}` 前缀）接受上游 OpenAI `data` 或 Codex `models` 列表，并按所选 provider 的模型限制过滤。Codex 请求（通过 `client_version` 或 Codex User-Agent 识别）收到 `models`；其他客户端收到 OpenAI `data`。格式转换时会补齐缺失的元数据。

`GET /providers` 返回当前默认 provider 和所有已配置 provider。每个已配置的 `apikey` 返回为 `***`；未设置的密钥会被省略。该端点不会展开环境变量，也不会联系上游 provider。

## Gemini 上游

```toml
[providers.gemini]
type = "gemini"
baseurl = "https://generativelanguage.googleapis.com/v1"
apikey = "${GEMINI_API_KEY}"
```

通过 `/gemini/v1/chat/completions`、`/gemini/v1/responses` 或 `/gemini/v1/messages` 使用 Gemini 模型，例如 `gemini-2.5-flash`。代理按请求的 `stream` 字段调用 Google 的 `generateContent` 或 `streamGenerateContent`。模型列表可通过 `/gemini/v1/models` 获取，也可用 `llmapi check gemini --model gemini-2.5-flash` 探测生成接口。

`GET /gemini/v1/models` 将 Gemini 模型列表转换为 OpenAI 或 Codex 格式，保留模型名称、token 限制和 `nextPageToken`。使用 `pageSize` 与 `pageToken` 翻页。

Interactions API 使用同一个 Gemini provider。请求体、上游响应和 SSE 事件按原格式转发。后续操作应使用创建交互时的 provider。

| 方法 | 路由 | 操作 |
| --- | --- | --- |
| POST | `/gemini/v1/interactions` | 创建 |
| GET | `/gemini/v1/interactions/{id}` | 读取或续读 SSE |
| POST | `/gemini/v1/interactions/{id}/cancel` | 取消 |
| DELETE | `/gemini/v1/interactions/{id}` | 删除 |

默认 provider 为 Gemini 时可以省略 `/gemini` 前缀。上游 API 版本由 `baseurl` 决定。

## llmapi 中继

```toml
[providers.relay]
upstream = "llmapi" # 也可以写成 type = "llmapi"
baseurl = "https://remote.example.com/pool/v1"
apikey = "${REMOTE_LLMAPI_API_KEY}"
```

中继保留客户端的生成协议，转发 Chat Completions、Responses、Messages、SSE 和模型列表，由远端 llmapi 选择上游并转换协议。`baseurl` 只写远端地址时使用其默认 provider，带 `/pool/v1` 时路由到远端的 `pool`。Responses WebSocket 需要远端选中的 provider 支持升级。`apikey` 填远端 llmapi 的服务端密钥。`check --model` 使用远端的 Chat Completions 入口。

## 负载均衡 provider

`type = "lb"` 将已有 provider 组成等权轮询组。每个请求只领取一次起始位置，然后按配置顺序尝试成员，因此 `[A, B, C]` 的请求起点依次为 `A`、`B`、`C`、`A`……；起点为 `B` 时尝试顺序为 `B → C → A`。同一请求中每个成员最多尝试一次，故障切换不会再次推动共享游标。

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

`lb` provider 不接受 `baseurl`、`apikey` 或 `models`；`members` 必须是非空、有序且不重复的成员列表。成员必须是现有普通 provider：未知名称、自引用和嵌套 `lb` 都会被拒绝。被 `lb` 引用的 provider 在组被修改或删除前不能删除；删除 `lb` 本身不会删除成员。

故障切换只作用于通过 `lb` 发起的请求：

| 结果                                                         | 行为                     |
| ------------------------------------------------------------ | ------------------------ |
| DNS、连接、TLS、发送或等待响应头阶段的传输失败               | 尝试下一成员             |
| 30 秒响应头等待超时                                          | 尝试下一成员             |
| 上游 `401`、`403`、`408`、`429`、`500..=599`                 | 尝试下一成员             |
| 成员凭据缺失                                                 | 跳过该成员               |
| 请求的模型不在成员的允许列表中                               | 跳过该成员               |
| 其他上游状态（如 `400`、`404`、`422`）                       | 返回该响应               |
| 本服务鉴权、路由、方法或请求转换错误                         | 立即返回错误             |
| 已选定响应后的解析错误、响应体读取失败或流中断               | 沿用现有响应错误处理     |
| 重定向（`3xx`）                                              | 返回该响应及 `Location`，不自动跟随 |

每次成员尝试最多等待 30 秒响应头，并复用 15 秒连接超时；已建立的 SSE 流不受该 30 秒限制。经 LB 的请求不会自动跟随重定向，因此单次成员尝试最多对应一次上游请求。CLI `check` 对每个成员应用相同的响应头上限，同时保持 `--timeout` 为整条命令的总预算。N 个成员全部耗尽前最多等待约 N × 30 秒，不包含响应体读取时间。所有成员均因可切换结果失败时，客户端收到符合自身协议错误结构的 `503 Service Unavailable`，其中包含 `lb` 名称和简短失败摘要。不会把某个成员的 `Retry-After` 冒充为整组状态。请求体只读取一次，每次尝试都从原始客户端输入重新转换，因此跨协议切换不会复用上一成员转换后的请求体或密钥。`upstream_raw` 返回选定成员的原始响应。故障切换不保证上游恰好执行一次，`idempotency-key` 不会在不同 provider 之间去重。

`GET /models`（也支持 `/v1` 和 `/{provider}`）与生成请求共享轮询游标，只返回选定成员的目录，不合并多个目录。只有直连且未配置模型限制的 provider 才会转发 `If-None-Match` 并保留原有的 `ETag` 行为。依赖上游会话状态的请求（`previous_response_id`、`conversation`）应使用具体 provider 路由，因为 `lb` 可能在请求之间切换成员。模型名不做映射。

每个生效配置版本原子发布自己的配置和游标。有效配置变更会从零开始新游标，在途请求继续使用旧版本；无效热加载保留上一版本。访问具体 provider 的请求不消耗 `lb` 游标。

`lb` 组不支持 WebSocket；升级请求会在连接上游前收到 HTTP 400。首版不提供后台健康检查、熔断冷却、嵌套组、权重、会话粘性或跨进程游标协调。

限制与错误行为详见上述表格；provider 配置格式见 [examples/config.yaml](examples/config.yaml)。

## 开发

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```
