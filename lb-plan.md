# `type = "lb"` 开发计划

状态：已实现。T1–T5 完成，A1–A15 均有自动化测试或回归证据；交付说明见文末。

## 1. 目标与范围

新增特殊 provider 类型 `lb`，通过名称引用多个现有 provider，对请求执行轮询负载均衡；当前成员不可用时，自动尝试下一成员，直到获得可返回的响应或候选成员耗尽。

首版采用以下设计决策：

- 等权轮询（round-robin），不增加策略或权重配置。
- 支持现有 HTTP 非流式、SSE 和 `/models` 请求，可设为默认 provider，也可通过命名路由访问。
- 成员可以使用不同上游协议，继续复用现有协议转换；每次尝试使用该成员自己的地址、凭据和协议。
- 采用请求内故障切换，不记录跨请求的故障状态；后续请求仍会轮询故障成员，使恢复后的成员自然重新参与服务。
- 不实现后台健康检查、熔断冷却、嵌套 LB、模型映射、会话粘性或多进程间的轮询协调。
- 首版 LB 不支持 WebSocket，升级前返回明确的 HTTP 400；普通 `openai-responses` provider 的 WebSocket 功能保持现状。

## 2. 当前实现与改动入口

| 文件                             | 当前职责                                                  | 本次开发需要处理的内容                     |
| -------------------------------- | --------------------------------------------------------- | ------------------------------------------ |
| `src/config.rs`                  | `Provider`、`ProviderConfig`、配置读写校验、provider 增删 | 新增 LB 配置、成员引用校验及删除约束       |
| `src/lib.rs`                     | `add/list/check/set/remove` CLI                           | 新增 LB 创建参数，适配展示与探测           |
| `src/server.rs`                  | 请求状态快照、配置热加载、`/providers`                    | 共享轮询状态，并保证状态与配置属于同一版本 |
| `src/proxy.rs`                   | 选择单个 provider、发送请求、转换响应、SSE/WebSocket      | 在响应交给客户端之前实现候选尝试与失败分类 |
| `src/protocol.rs`、`src/auth.rs` | 上游协议映射、鉴权头生成                                  | 只接收解析后的真实上游，避免将 LB 当成协议 |
| `src/codex.rs`                   | 自动发现的 Codex provider                                 | 复用现有凭据和请求处理，不改变登录发现机制 |

当前 `ProviderConfig.base_url` 必填，`forward_raw` 和 `convert_and_forward` 会将上游结果直接包装成客户端响应，不能仅在其返回的 HTTP 状态外层增加重试：必须保留传输错误、上游状态与本地转换错误的区别。

## 3. 配置契约

### 3.1 示例

```toml
server = "127.0.0.1:8080"
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

对应 YAML：

```yaml
server: 127.0.0.1:8080
default: pool
providers:
  primary:
    type: chat
    baseurl: https://primary.example.com/v1
    apikey: ${PRIMARY_API_KEY}
  secondary:
    type: responses
    baseurl: https://secondary.example.com/v1
    apikey: ${SECONDARY_API_KEY}
  pool:
    type: lb
    members: [primary, secondary]
```

### 3.2 字段与校验

- `type = "lb"` 只接受非空、有序的 `members` 名称列表；允许单成员组，不允许重复成员。
- LB 不配置 `baseurl` / `base_url` 或 `apikey` / `api_key`；提供这些字段时报告配置错误。普通 provider 不接受 `members`，原有必填字段和类型别名继续有效。
- 成员必须引用现有普通 provider；拒绝未知名称、自引用及引用其他 LB。所有嵌套都拒绝，因此无需实现递归或环检测框架。
- 允许引用自动发现的 `codex`：配置阶段允许该名称，运行时凭据不可用则跳过。若配置中存在同名普通 provider，仍按现有规则优先使用该配置；同名 LB 仍受禁止嵌套规则约束。
- 支持 TOML、YAML、YML、JSON 的读取和保存，保留成员顺序。LB 序列化不产生空地址或伪造密钥。
- 被 LB 引用的普通 provider 不允许直接删除，错误指出引用它的 LB，配置不发生变化。先修改或删除引用组后才能删除成员；删除 LB 本身不删除成员。
- `default` 可以指向 LB；删除默认 LB 后沿用现有默认 provider 重选规则。

模型名不做转换或匹配。部署者应将支持同一模型名及所需能力的 provider 放入同一组；协议转换不等于模型映射。依赖上游存储状态的请求（如 `previous_response_id`、`conversation`）应使用具体 provider 路由，本期不保证跨成员续接会话。

## 4. 调度与故障切换契约

### 4.1 轮询与状态

1. 每个 LB 在服务进程内维护独立、并发安全的轮询游标，所有请求快照共享该状态。
2. 每个进入调度的请求仅领取一次起始位置。`[A, B, C]` 的连续请求起点为 `A → B → C → A`。
3. 从起点按配置顺序循环尝试。起点为 B 时，候选顺序为 `B → C → A`；成功后立即结束。
4. 同一请求中，每个成员最多尝试一次，总上游发送次数不超过成员数。一次失败切换不再推动全局游标。
5. 不并行发送同一生成请求，不等待故障成员恢复，不对同一成员进行额外重试。客户端取消请求后停止本次调度。
6. 只访问具体 provider 的请求不进入 LB，也不消耗其游标。`/models` 与生成请求共享所选 LB 的游标。

轮询保证的是首选成员的分配，不保证故障期间实际成功请求数均匀。状态更新只在短临界区内完成，不持锁执行网络请求。

### 4.2 可切换与不可切换的结果

以下规则仅适用于通过 LB 发起的请求，普通 provider 直连行为不变。

| 结果                                                              | LB 行为                                  |
| ----------------------------------------------------------------- | ---------------------------------------- |
| DNS、连接、TLS、发送请求或等待响应头阶段的传输失败                | 记录失败，尝试下一成员                   |
| 等待上游响应头超时                                                | 取消当前尝试，尝试下一成员               |
| 上游 HTTP `401`、`403`、`408`、`429`、`500..=599`                 | 视为该成员当前不可用，尝试下一成员       |
| 成员凭据环境变量无法展开，或自动 Codex 凭据缺失                   | 不发送请求，记录原因并尝试下一成员       |
| 客户端未通过本服务鉴权、路由或方法错误、已识别的请求解析/转换错误 | 立即返回现有错误，不触发故障切换         |
| 其余上游状态，包括 `400`、`404`、`422`                            | 返回该响应，沿用现有错误格式转换，不切换 |
| 已接受响应后的 JSON/SSE 解析错误、响应体读取失败或流中断          | 沿用现有响应错误处理，不重放请求         |

上游 `401/403` 与本服务的客户端鉴权失败必须区分。协议转换不支持请求字段时，继续明确报错，不通过切换成员静默丢弃字段。

首版 LB 使用固定 **30 秒单次响应头等待上限**，覆盖发送请求到收到响应头的阶段；复用现有 **15 秒连接超时**。不新增超时配置字段，不将 30 秒上限作用于已建立的 SSE 流。N 个成员的候选尝试等待最多约为 N × 30 秒，不包含接受响应后读取响应体的时间。

所有成员均因可切换结果失败时，返回 HTTP `503 Service Unavailable`，错误结构符合客户端协议，包含 LB 名称及简短的失败摘要。日志可记录成员名、尝试序号、失败类别、状态码和耗时；不记录密钥、完整请求体或未经脱敏的上游错误正文。不将最后一个失败成员的 `Retry-After` 等元数据冒充为整个 LB 的状态。

### 4.3 请求重放与响应边界

- 客户端请求体只读取一次，沿用现有 64 MiB 上限；保留原始字节或可重复转换的输入，每次尝试独立生成目标请求。
- 同协议转发保留原始请求语义；跨协议切换必须从原始输入重新转换，不能把上一成员转换后的请求再次当作客户端输入。
- 每次尝试重新选择成员凭据及协议头，保持既有客户端鉴权和密钥转发规则，不能携带上一成员的密钥。
- 在选择是否切换之前，不向客户端提交失败成员的响应头或响应体。
- **选定一个不需要切换的上游响应后即停止重试。** 即使尚未输出第一条 SSE 数据，其后发生的读取或解析错误也不再切换；提交响应后更不能拼接另一成员的输出。
- `upstream_raw` 仍执行上述上游选择与状态分类；选定响应后保留现有原始响应模式。
- 故障切换不能保证上游恰好执行一次：超时或断连前，上游可能已经开始生成并产生费用；现有 `idempotency-key` 转发不能保证不同 provider 间去重。

### 4.4 模型列表、Codex 与热加载

- `GET /models` 及其 `/v1`、命名前缀路由按相同规则轮询和切换，返回选定成员的模型列表，继续按客户端转换 `data` / `models`，不合并多个目录。
- LB 模型列表不跨成员转发 `If-None-Match`，也不暴露成员专属的 `ETag`，避免轮询到另一成员时错误复用缓存；直连 provider 保持现有条件请求行为。
- Codex 成员继续使用现有自动凭据、请求准备和 SSE/JSON 处理。通过 LB 遇到 Codex `401` 时可切换其他成员；直接访问 Codex 仍不重试，也不刷新凭据。
- 每次有效配置实际变更时，原子发布配置与对应 LB 状态，新版本游标从零开始。正在处理的请求继续使用旧版本的成员、配置和状态。
- 无效配置、缺失引用等热加载错误保留上一有效版本。不能每个请求或每次 500 ms 文件检查都重建游标；Codex 凭据轮询本身不重置 LB 游标。

## 5. CLI 与展示契约

```bash
llmapi add pool --type lb --members primary,secondary
llmapi set default pool
llmapi list
llmapi check pool
llmapi check pool --model shared-model --timeout 30
```

- `add --type lb` 要求 `--members`，与 `--baseurl`、`--apikey`、`--apikey-stdin` 互斥。普通类型仍要求 `--baseurl`，不接受 `--members`。
- `list` 展示 LB 类型、有序成员及默认标记，不伪造上游地址，不输出成员密钥。
- `GET /providers` 展示 `{ "type": "lb", "members": [...] }`；普通 provider 的密钥脱敏行为不变，查询不联系上游。
- `check pool` 使用相同成员顺序和失败分类，从首成员开始，首个成功探测即成功；报告 LB、最终成员、尝试次数及结果。`--model` 原样用于各成员的现有探测请求。
- `check --timeout` 是本次命令的总时间预算，包含所有成员和响应读取，不乘以成员数。预算耗尽或无可用成员时非零退出；CLI 不连接运行中服务器，也不影响其轮询游标。

## 6. 开发任务与完成条件

按依赖顺序执行；每项先设计并编写对应业务测试，再实现代码。不为枚举展示、getter 或纯转发单独增加重复测试。

| 任务             | 任务说明                                                                                                          | 完成条件                                                          |
| ---------------- | ----------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| ✅ T1 配置与管理    | 在 `src/config.rs` 区分普通上游与 LB 配置，增加成员引用和删除约束；适配 `src/lib.rs` 创建、默认选择与展示         | 配置读写及 CLI 契约成立；旧配置和普通 provider 命令兼容           |
| ✅ T2 轮询核心      | 新增小型 `src/lb.rs`，负责有界候选遍历、共享游标与可切换结果分类；在 `src/server.rs` 接入版本一致的运行时状态     | 并发首选分配正确，组间独立，无跨请求游标重置，无持锁跨 `.await`   |
| ✅ T3 HTTP 故障切换 | 整理 `src/proxy.rs` 的单成员发送与响应构造边界，接入逐成员转换、鉴权和超时；适配 `src/protocol.rs`、`src/auth.rs` | 状态码、传输失败、全失败和流式边界符合约定；LB 不参与真实协议映射 |
| ✅ T4 完整接入      | 完成 `/models`、Codex、`upstream_raw`、热加载和 CLI `check`；LB WebSocket 明确拒绝                                | 各入口共用调度规则，现有直连行为不变                              |
| ✅ T5 文档与回归    | 更新 `README.md`、`README.en.md`、`examples/config.toml`、`examples/config.yaml`，补充下列业务验收场景并运行检查  | 示例可加载，全部验收通过，限制与错误行为有明确说明                |

实现保持具体、局部：不引入通用调度插件、全新 provider trait 体系或新依赖。数据模型应让 LB 无法被误当作带 URL 的真实上游；如调整现有公开 Rust 类型或函数签名，需列明调用方迁移点，避免用空 URL、默认协议或不可达分支掩盖类型变化。

## 7. 验收要求

所有新测试使用本地 mock 上游、调用计数器和可控同步/时间，不依赖真实 API、网络账号或本机 Codex 登录。优先扩展现有测试设施，复杂 LB 场景可放入 `src/server/tests/lb_tests.rs`。

| 编号 | 场景           | 验收标准                                                                                                                                            |
| ---- | -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| ✅ A1   | 配置读取与保存 | TOML/YAML/YML/JSON 中普通 provider 与 LB 可共存并往返保存；类型别名、成员顺序和旧配置兼容                                                           |
| ✅ A2   | 配置约束       | 空列表、重复、未知、自引用、嵌套、混用字段均被拒绝并指出配置位置；单成员有效；无凭据时仍可配置自动 `codex` 引用                                     |
| ✅ A3   | 管理命令       | 能创建 LB、设为默认、展示及删除；删除被引用成员失败且配置不变；`/providers` 输出成员关系且无明文密钥                                                |
| ✅ A4   | 正常轮询       | `[A,B,C]` 的六次串行请求首选为 `A,B,C,A,B,C`；单成员正常工作；不同组的游标互不影响                                                                  |
| ✅ A5   | 并发轮询       | 健康组承接一批并发请求时，各成员首选次数之差不超过 1；请求快照共享游标，完成先后不影响该结论                                                        |
| ✅ A6   | 有界切换       | 起点 B、B/C 失败时按 `B,C,A` 成功；每个成员最多调用一次，成功后不再调用其他成员；下一请求起点仍为 C                                                 |
| ✅ A7   | 错误分类       | 本地连接失败、响应头超时以及表中可切换 HTTP 状态触发切换；代表性的 `400/404/422` 和本服务鉴权错误不切换；全失败返回客户端协议格式的 503             |
| ✅ A8   | 恢复与取消     | 失败成员恢复后在后续轮询中再次被使用，无永久隔离；客户端取消后不发起下一成员请求                                                                    |
| ✅ A9   | 协议与凭据     | Chat、Responses、Messages 客户端均可访问 LB；至少一次跨协议故障切换验证模型名、消息、工具参数及成员鉴权正确，不复用上一成员的转换结果或密钥         |
| ✅ A10  | 请求及流式边界 | 请求体超过上限或已识别的转换错误不调用其他成员；收到可切换状态后可切换至 SSE 成功响应；选定响应后、首事件前断流及输出部分事件后断流均不调用下一成员 |
| ✅ A11  | 路由与模式     | 默认/命名路由及 `/v1` 变体可用；`upstream_raw` 选择正确成员；LB WebSocket 在升级前拒绝，普通 provider WebSocket 回归通过                            |
| ✅ A12  | 模型与 Codex   | 模型列表切换并保留客户端格式，不合并目录，不跨成员复用 ETag；自动 Codex 不可用时跳过，同名配置优先；模拟 Codex 401 只在 LB 路径触发切换             |
| ✅ A13  | 热加载         | 有效配置变更新请求生效且重置游标，在途请求维持旧快照；无效更新保留旧状态；无内容变更的轮询和 Codex 凭据更新不重置游标                               |
| ✅ A14  | CLI 探测       | `check pool` 成功、失败、`--model`、总超时均符合约定；输出可定位最终成员和尝试次数，退出码正确                                                      |
| ✅ A15  | 现有功能回归   | 普通 provider 的转换、SSE、鉴权、错误响应、Codex、模型条件请求及热加载测试继续通过                                                                  |

功能开发完成后执行：

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test
cargo clippy --all-targets -- -D warnings
git diff --check
```

交付条件：T1–T5 完成、A1–A15 有测试或明确的回归证据、上述检查通过，并在交付说明中列出任何未解决项。本次仅创建开发计划，不运行或宣称通过尚未实现的功能验收。

## 8. 交付说明

### 8.1 实现结构

- `src/lb.rs`：每个 LB 组的候选顺序与共享游标（原子计数，无持锁 `.await`）、可切换状态分类、30 秒响应头超时常量。
- `src/config.rs`：`ProviderConfig` 拆分为 `Upstream(UpstreamConfig)` 与 `Lb(LbConfig)`；新增 `Protocol`，使真实上游无法表示 LB；自定义序列化保证 LB 不产生 `baseurl`/`apikey`。
- `src/proxy.rs`：把单次发送拆成 `RequestPlan`（按成员重新转换、重新选择凭据）与 `build_response`；直接访问与 LB 共用同一套计划/构造逻辑，`lb_dispatch` 只负责有界候选遍历。
- `src/server.rs`：`Runtime` 将配置与 `lb::State` 作为同一版本原子发布；Codex 凭据仍在独立 `watch` 中，不重建游标。
- `src/lib.rs`：`add --type lb --members`、`list` 展示、`check` 的总预算成员遍历与报告。
- 测试：`src/lb.rs`（游标/并发/状态分类）、`src/config.rs`（读写与约束）、`src/lib.rs`（CLI 与 `check`）、`src/server/tests/lb_tests.rs`（调度、故障切换、协议、模型、Codex、热加载、流边界、WebSocket）。

### 8.2 公开 Rust API 迁移点

- `ProviderConfig` 由结构体变为枚举（`Upstream` / `Lb`）；构造使用 `ProviderConfig::upstream(...)` 或 `ProviderConfig::lb(...)`。字段访问改用 `as_upstream()` / `as_upstream_mut()` / `as_lb()`。
- 真实上游配置为 `UpstreamConfig { protocol, base_url, api_key }`；`Provider` 新增 `Lb` 变体，新增独立 `Protocol`。
- `protocol::upstream_for`、`auth::apply_api_key`、`auth::apply_protocol_headers` 改为接收 `Protocol`，避免 LB 参与协议映射。
- `proxy::forward_raw` 被 `RequestPlan` + `send_plan` + `build_response` 取代；`server::AppState` 新增 `lb: Arc<lb::State>`，并新增 `server::Runtime`。
- `Config::provider()` 仍返回 `(&str, &ProviderConfig)`（现为枚举）；调用方按变体分支。

### 8.3 顺带修复

- `proxy_websocket` 原先用新的鉴权头整体替换上游握手请求头，会丢失 `Sec-WebSocket-Key` 等必需头；现改为在生成的握手头上叠加鉴权头，普通 `openai-responses` provider 的 WebSocket 回归测试因此成立。

### 8.4 未解决项与边界

- 无后台健康检查、熔断冷却、权重、嵌套、会话粘性、跨进程协调（计划内不做）。
- 故障切换不保证上游恰好执行一次；`idempotency-key` 不跨 provider 去重（计划已声明）。
- 在途请求维持旧版本通过“配置与游标同版本快照”测试证明；未额外使用长耗时网络请求构造并发在途场景。
- 经 LB 的 `/models` 不合并目录、不转发 `If-None-Match`、不暴露成员 `ETag`；直连 provider 行为不变。

### 8.5 复审修复（P2）

1. **传输错误不再泄露 URL 查询参数**：新增 `redact::transport_error`，使用 `reqwest::Error::without_url()` 并附加失败类别；`src/proxy.rs` 的成员切换、直接访问错误、缓冲/流式读取错误，以及 `src/lib.rs` 的探测错误全部改用该函数。LB 503 摘要与日志不再包含完整上游 URL 或 `api_key` 等查询值。
2. **CLI 在收到可切换状态后立即切换**：`send_probe` 只负责发送并返回响应头，`finish_probe` 单独读取响应体。`check` 对 LB 成员先判断状态码，`401/403/408/429/5xx` 不读取错误体，直接尝试下一成员。
3. **CLI 接受响应后不再重放**：同一拆分后，成员响应头一旦被接受（包括 200），`finish_probe` 的读取/断流错误立即以非零状态返回，不再调用其他成员，符合 §4.3。

对应回归测试：`redact::tests::transport_error_omits_url_and_query_secrets`、`lb_tests::transport_failure_does_not_leak_query_secrets`、`tests::check_lb_switches_on_retryable_status_without_reading_the_body`、`tests::check_lb_does_not_replay_after_accepting_headers`。

4. **LB 不再自动跟随重定向**：`AppState` 新增禁止重定向的 `lb_client` 专供 LB 尝试使用；直接 provider 仍使用原客户端。LB 收到 `3xx` 时按非切换状态返回，单次成员尝试始终对应至多一次上游请求。CLI `check` 同样使用禁止重定向的客户端。
5. **CLI 每成员响应头上限**：`send_probe` 增加独立的响应头等待上限；`check` 对 LB 成员使用 `min(30 秒, 剩余预算)`，整个命令预算（含响应体读取）仍由请求总超时约束。

对应回归测试：`lb_tests::lb_does_not_follow_upstream_redirects`、`tests::check_does_not_follow_redirects_or_replay_members`、`tests::check_lb_caps_each_member_header_wait_but_keeps_total_budget`。

6. **LB 关闭隐式协议重试**：reqwest 0.13 默认对协议 NACK（如 HTTP/2 `REFUSED_STREAM`）最多重试 2 次。服务端 `lb_client` 与 CLI LB 客户端均显式设置 `.retry(reqwest::retry::never())`，确保“每成员最多一次”；普通 provider 直连仍保留默认重试。
7. **CLI 仅对 LB 禁用重定向**：`check` 为普通 provider 使用默认客户端（跟随重定向），仅 LB 分支构建禁止重定向和隐式重试的客户端，恢复“普通 provider 直连行为不变”。
8. **LB 重定向保留 Location**：`copy_response_headers` 与 `copy_response_metadata_headers` 增加 `Location`，使 `/models`、透明转发与跨协议转换三条路径都能把 3xx 的目标地址交给客户端。

对应回归测试：`lb_tests::lb_preserves_redirect_location_for_every_route`、`tests::check_concrete_provider_still_follows_redirects`。隐式重试为客户端策略配置，由 `retry::never()` 显式关闭。

### 8.6 第二轮复审修复（codex review R1–R7）

第二轮独立复审确认核心调度、有界切换、响应接受后不重放、配置与游标同版本发布均正确，同时指出 7 项问题；均已修复并补测试：

1. **R1 输出保密（P0）**：`redact::is_sensitive_key` 补齐 `api-key`、`x-api-key`、`refresh_token`、`password`、`secret`、`authorization` 等命名；敏感值改为固定 `***`，不再保留可能等于完整短密钥的前缀；`redact::url` 增加 URL userinfo 脱敏；`/providers` 与 CLI `list` 展示时对 `base_url` 应用同一脱敏（保存与真实请求 URL 不变）；CLI LB 最终错误正文替换当前成员已展开密钥及 Codex bearer token。
   回归测试：`redact::tests::redacts_sensitive_query_values`、`redacts_url_userinfo_and_query_secrets`、`request_keeps_path_and_redacts_query`、`tests::add_lb_command_persists_members_and_list_shows_them`（URL 凭据展示脱敏）、`lb_tests::providers_endpoint_reports_lb_members_without_keys`（URL 内嵌凭据脱敏）、`tests::probe_detail_redacts_known_member_secret`。
2. **R2 显式 `null` 绕过字段互斥（P1）**：反序列化层用双重 `Option` 保留“字段缺失 / 显式 null”的区别；LB 拒绝任何形式的 `baseurl`/`apikey`（含 null），普通 provider 拒绝任何形式的 `members`（含 null），普通 provider 的 `apikey: null` 兼容行为不变。
   回归测试：`config::tests::rejects_forbidden_fields_even_when_explicitly_null`、`explicit_null_upstream_key_is_still_accepted`；原 `rejects_mixed_lb_and_upstream_fields` 继续通过。
3. **R3 CLI 响应缓冲无上限（P1）**：`finish_probe` 改为复用 `proxy::read_upstream_body`（成功响应 64 MiB、错误响应 1 MiB 上限）；接受响应后的读取/超限错误仍直接失败、不重放。
   回归测试：`tests::check_lb_bounds_accepted_error_body_without_replay`；共享读取器已有 `proxy::tests::bounds_buffered_upstream_response_bodies`。
4. **R4 取消测试假阳性（P1）**：改为“首成员可切换失败 + 正反对照”事件同步场景：不取消时第二成员恰好调用一次；取消后首成员释放为 503 也不尝试第二成员。原测试首成员最终返回 200，无法区分取消失效。
   回归测试：`lb_tests::cancelled_request_does_not_attempt_the_next_member`（同一设施内两个场景）。
5. **R5 进程级测试超时污染（P1）**：删除 `lb` 模块的全局测试超时覆盖与锁；超时改为配置版本运行时字段 `AppState.lb_header_timeout`（生产固定 30 秒，测试经 `Runtime::with_lb_header_timeout` 注入）；CLI `check` 通过 `check_with_header_timeout` 参数注入，不再有跨测试共享可变状态。
6. **R6 总预算测试不能拒绝“按成员重新计时”（P1）**：重写为可失败的验收场景——首成员耗时 700 ms 后返回可切换状态，第二成员需 500 ms 超过剩余预算；错误实现会以新预算让第二成员成功。另增“第二成员已接受响应头、响应体读取耗尽预算”边界。
   回归测试：`tests::check_lb_spends_one_shared_timeout_budget`、`check_lb_budget_covers_response_body_read_without_replay`。
7. **R7 接受响应后读取失败缺少尝试次数（P2）**：失败上下文加入 `after N attempt(s)`，提前返回行为不变。
   回归测试：扩展 `tests::check_lb_does_not_replay_after_accepting_headers`（前置一个可切换失败成员，断言错误包含最终成员与 2 次尝试）。

同时补齐复审指出的验收证据缺口：

- A6 环回顺序：`lb_tests::failover_wraps_to_members_before_the_starting_member`（起点 B 时 `B→C→A`，起点 C 时 `C→A→B`，失败切换只推动一次游标）。
- A9 首次尝试内容：`lb_tests::cross_protocol_failover_rebuilds_the_request_for_the_next_member` 现在同时捕获第一次 Chat 尝试与第二次 Anthropic 尝试的请求体、工具参数和成员鉴权头。
- A13 真实网络在途热加载：`lb_tests::in_flight_request_keeps_old_members_and_cursor_across_reload`——旧请求阻塞在旧成员上，发布新配置后新请求使用新成员且游标从零开始，释放旧请求后仍在旧成员列表内切换。

未新增证据的边界（复查未发现实现缺陷，记录为已知未验证项）：双成员不同 `ETag` 的 304 对照、HTTP/2 `REFUSED_STREAM` 线级计数、sibling workspace 构建（已核对调用点仅使用 `llmapi::Cmd`）。

### 8.7 第三轮复审修复（codex review N1–N4）

第二轮复审确认 R1–R7 与 A6/A9/A13 补测成立，但指出 3 项脱敏遗漏和 1 项测试竞态；均已修复：

1. **N1 userinfo 脱敏与客户端 URL 解析不一致（P1）**：`redact_userinfo` 改用 `url::Url::parse`（与 reqwest 相同的 WHATWG 解析），覆盖 `https:///user:pass@host`、单斜杠、百分号编码密码等非规范形式；解析失败时保留旧的 authority 回退（含 IPv6/非法端口场景），无法解析也不直接回显疑似凭据。`redact::url_secrets` 同样按解析结果收集 userinfo 原值/解码值及敏感查询参数值。
   回归测试：`redact::tests::redacts_url_userinfo_and_query_secrets`（无斜杠、单斜杠、额外斜杠、`%40`/`%23`、非法端口、无 scheme）、`collects_url_credentials_for_display_redaction`、`lb_tests::providers_endpoint_reports_lb_members_without_keys`（`/providers` 展示）、`tests::add_lb_command_persists_members_and_list_shows_them`（`list` 展示）。
2. **N2 CLI 正文脱敏未覆盖 URL 中实际使用的凭据（P1）**：新增 `probe_secrets`，在探测前收集成员展开后的 API key、Codex bearer token、URL userinfo（原值与百分号解码值）及敏感查询参数值；LB 与普通 provider 探测共用。`finish_probe` 接收敏感值集合并在生成摘要前替换。
   回归测试：`tests::probe_detail_redacts_credentials_embedded_in_member_url`、`probe_detail_redacts_sensitive_query_values_from_member_url`；并用真实 `llmapi check` + 本地 mock 验证 `DETAIL` 为 `invalid password: ***`。
3. **N3 先截断后脱敏导致密钥前缀泄漏（P1）**：错误正文与成功回复改为“先对完整文本替换已知凭据，再折叠空白并截断（240/80 字符）”；`compact_text` 直接接收 `&str`，`response_text` 返回完整文本由 `finish_probe` 统一收尾。
   回归测试：`tests::probe_detail_redacts_before_truncating_error_summary`、`probe_detail_redacts_before_truncating_success_reply`（密钥跨越截断边界）。
4. **N4 取消测试启动竞态（P2）**：不再从请求启动时计时；先以 5 秒上限等待首成员进入回调，再 abort 请求任务并确认取消，随后释放首成员的可切换 503 并断言第二成员未被调用；保留“不取消会切换”的正对照。
   回归测试：`lb_tests::cancelled_request_does_not_attempt_the_next_member`。

### 8.8 第三轮复审修复（codex review T1–T2）

第三轮复审确认 N3/N4 修复成立，但指出 N1/N2 仍有两个边界反例，均已修复：

1. **T1 凭据集合未覆盖实际 wire 表示（P1）**：`redact::url_secrets` 对敏感查询参数同时收集原始编码值与解码值；新增 `redact::url_basic_auth` 复刻 reqwest 的 URL authority 提取语义（percent-decoded 用户名/密码），`probe_secrets` 据此生成并收集 reqwest 自动发出的 `Authorization: Basic` 值及其 base64 载荷。
   回归测试：`tests::probe_detail_redacts_generated_basic_authorization`（mock 回显实际收到的 Authorization，断言 400 摘要不含 Basic token/密码）、`probe_detail_redacts_raw_encoded_query_secrets`（断言原始 `sk%2Dquery%2Dsecret` 与解码形式均被替换）；并用真实 `llmapi check` + 回显 mock 验证 `DETAIL` 为 `rejected authorization: ***`、`api_key=***/models`。
2. **T2 解析失败回退遗漏 authority 形式（P2）**：`redact_userinfo`/`url_secrets` 的回退改为统一的 `unparsed_authority`：识别 HTTP-like scheme 后任意数量的斜杠（含 `https:/...` 与非法端口）以及 network-path 引用（`//user:pass@host`），不再只认字面量 `://`。
   回归测试：`redact::tests::redacts_url_userinfo_and_query_secrets`、`collects_url_credentials_for_display_redaction`（两个反例）、`lb_tests::providers_endpoint_reports_lb_members_without_keys`（`/providers` 展示）、`tests::add_lb_command_persists_members_and_list_shows_them`（`list` 展示）。

### 8.9 第四轮复审修复（codex review Q1–Q3）

第四轮复审确认 T1/T2 主体修复成立，另指出 3 项 P2 边界问题，均已修复：

1. **Q1 JSON 转义凭据绕过摘要脱敏（P2）**：错误正文若为合法 JSON，先反序列化、递归对字符串值做凭据替换，再重新序列化并摘要；非 JSON 正文沿用文本替换。保证凭据中的引号/反斜杠在 JSON 转义后仍可被识别。回归测试：`tests::probe_detail_redacts_json_escaped_credentials`；真实 CLI + 回显 mock 验证 `DETAIL` 为 `{"error":{"message":"invalid password: ***"}}`。
2. **Q2 Basic token 预测与 reqwest 严格解码不一致（P2）**：`redact::url_basic_auth` 对 userinfo 采用严格 UTF-8 percent-decode：用户名解码失败视为无认证，密码解码失败视为 `None`，与 reqwest `extract_authority` 一致。回归测试：扩展 `tests::probe_detail_redacts_generated_basic_authorization`（`alice:%FF` 时实际发送 `Basic YWxpY2U6`）、`redact::tests::extracts_basic_auth_userinfo`（`%FF`、Unicode 用户名）。
3. **Q3 非 HTTP scheme 展示回归（P2）**：`redact_userinfo`/`url_secrets` 恢复对任意 `scheme://authority` 的 userinfo 处理；HTTP-like 非规范斜杠、非法端口与 network-path 仍走统一回退。回归测试：`redact::tests::redacts_url_userinfo_and_query_secrets`（`ftp://…`）、`collects_url_credentials_for_display_redaction`、`lb_tests::providers_endpoint_reports_lb_members_without_keys`、`tests::add_lb_command_persists_members_and_list_shows_them`。

### 8.10 第五轮复审修复（codex review F1）

第五轮复审确认 Q1/Q2 修复成立，指出 Q3 回退仍遗漏非 HTTP scheme + 非法端口组合，已修复：

- **F1 非 HTTP scheme 且解析失败时的 userinfo 回退（P2）**：`unparsed_authority` 在 HTTP-like scheme 分支之后补回通用 `scheme://authority` 分支（RFC 3986 scheme 语法校验，排除 opaque URL/Windows 路径），非法端口导致的解析失败仍能定位 authority 并脱敏；`url` 展示与 `url_secrets` 收集共用该定位。
  回归测试：`redact::tests::redacts_url_userinfo_and_query_secrets`、`collects_url_credentials_for_display_redaction`（`ftp://alice:s3cr3t@example.test:bad/v1`）、`lb_tests::providers_endpoint_reports_lb_members_without_keys`、`tests::add_lb_command_persists_members_and_list_shows_them`。

### 8.11 检查结果（最终）

```text
cargo fmt --all -- --check   通过
cargo check --all-targets    通过
cargo test                   172 passed
cargo clippy --all-targets -- -D warnings  通过
git diff --check             通过
```

### 8.12 最终确认复审

第六轮独立复审复核 F1 修复后给出**收敛/可交付**结论，未发现需要修复的 P0/P1/P2 问题。前五轮累计 17 项问题（R1–R7、N1–N4、T1–T2、Q1–Q3、F1）全部关闭，均有对应回归测试或可复现验证。

非阻断残余风险（不随本次改动扩大修复范围）：

- 不受支持的 opaque URL / Windows 路径式输入可能被既有兜底启发式过度脱敏（不造成凭据泄露，不改变正常 HTTP 请求语义）。
- 双成员不同 `ETag` 的 304 对照、HTTP/2 `REFUSED_STREAM` 线级发送计数、sibling workspace 构建未实测；已静态核对 sibling 调用点仅使用 `llmapi::Cmd`。
- 取消测试使用 20 ms 观察断连、400 ms 观察后续调用；生产 30 秒响应头上限未实等验证，由注入短超时覆盖同一代码路径。
- 高负载下曾复现热加载测试读取到 `fs::write` 半写配置的竞态（watcher 可能短暂发布缺字段的合法前缀配置）。已通过测试侧原子写（临时文件 + rename）消除，复现后于 8 核 CPU 满载下连续 40 次全量测试通过；生产 watcher 对“编辑器非原子写”的短暂读取行为不在本次 LB 范围。
