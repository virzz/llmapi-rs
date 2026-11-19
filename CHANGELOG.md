# 变更日志

版本日期取自对应 Git 标签指向的提交。

## [未发布]

## [v0.1.6]（2026-11-20）

### 新增

- 支持 `upstream = "llmapi"` 或 `type = "llmapi"`，将 Chat Completions、Responses、Messages、SSE 和模型列表中继到远端 llmapi。Responses WebSocket 由支持升级的远端 provider 处理。
- 新增负载均衡 provider（`type = "lb"`），在成员间等权轮询，并在单次请求内对可重试错误进行故障切换。`check` 支持探测负载均衡组。
- 上游 provider 可通过 `models` 设置模型允许列表。省略或留空时允许任意模型，配置后也会过滤该 provider 的模型目录。
- 给 `list` 添加 `ls`、`l` 别名，给 `add` 添加 `a` 别名。

### Rust API 兼容性

- `ProviderConfig` 拆分为 `Upstream(UpstreamConfig)` 和 `Lb(LbConfig)`。原先通过结构体字段构造 `ProviderConfig` 的代码可改用 `ProviderConfig::upstream(...)`。
- 直接构造 `UpstreamConfig` 时需填写 `models` 字段。使用穷尽匹配的调用方需处理 `Provider::Lb`、`Provider::Llmapi` 和 `Protocol::Llmapi`。

## [v0.1.5]（2026-10-31）

- 新增 `llmapi check [provider]`，可查询模型数量或通过 `--model` 发送最小生成请求。`--timeout` 控制总时间预算。
- 移除 `status` 和 `select`。使用 `list` 查看默认 provider，使用 `set default <provider>` 更改默认项。
- 新增可选 HTTP 访问日志（`access_log`），包含客户端地址、方法、脱敏路径、状态码和耗时。
- 新增中文 README，并保留英文版。

## [v0.1.4]（2026-10-28）

- 自动发现本地 Codex ChatGPT 登录，并提供 `/codex` 路由和模型目录。
- 补齐 DeepSeek 模型的默认指令模板。

## [v0.1.3]（2026-09-24）

- 支持向 OpenAI Responses 上游转发 WebSocket。
- 新增 `status` 和 `select`，用于查看和切换默认 provider。

## [v0.1.2]（2026-09-24）

- 新增 shell 补全命令 `completion` 和别名 `comp`。
- 支持删除 provider，并通过交互提示或标准输入录入上游 API 密钥。

## [v0.1.1]（2026-09-24）

- 新增服务端 API 密钥鉴权。
- 发布流程可触发 Homebrew 公式更新。

## [v0.1.0]（2026-09-23）

- 首次发布独立 Rust CLI 和 HTTP 代理，支持在 OpenAI Chat Completions、Responses 和 Anthropic Messages 之间进行协议转换。
- 支持发现和加载 TOML、YAML、YML、JSON 配置，并热加载 provider 配置、默认 provider 和监听地址。
- 按客户端协议返回模型目录，内置 DeepSeek 模型默认元数据。`GET /providers` 提供脱敏后的 provider 配置。
- 支持通过 CLI 查看和管理 provider，并通过 macOS LaunchAgent 管理服务。

[未发布]: https://github.com/virzz/llmapi-rs/compare/v0.1.6...main
[v0.1.6]: https://github.com/virzz/llmapi-rs/compare/v0.1.5...v0.1.6
[v0.1.5]: https://github.com/virzz/llmapi-rs/compare/v0.1.4...v0.1.5
[v0.1.4]: https://github.com/virzz/llmapi-rs/compare/v0.1.3...v0.1.4
[v0.1.3]: https://github.com/virzz/llmapi-rs/compare/v0.1.2...v0.1.3
[v0.1.2]: https://github.com/virzz/llmapi-rs/compare/v0.1.1...v0.1.2
[v0.1.1]: https://github.com/virzz/llmapi-rs/compare/v0.1.0...v0.1.1
[v0.1.0]: https://github.com/virzz/llmapi-rs/tree/v0.1.0
