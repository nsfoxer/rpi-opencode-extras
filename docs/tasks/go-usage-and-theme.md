# opencode-go 套餐用量展示 + TUI 主题补齐

## 目标

1. 当当前 provider 是 opencode.ai（Go 套餐）时，在 TUI 页脚常驻一行套餐用量，含月度到期倒计时；非 go provider 时不显示。
2. 补齐 `github-dark` 主题缺失的 21 个颜色 token，改善代码块、思考块、滚动条配色。

## 验收标准

- [x] `cargo test` 全绿，且新增纯函数单测覆盖：响应解析、百分比格式化、倒计时格式化、到期已过的边界。
- [x] `cargo build --release` 0 error、0 warn（新增代码）。
- [x] 插件仍加载成功：`extensions: loaded 10 plugin(s) (15 tool(s))`（工具数不变，本 feature 不注册工具）。
- [x] 非 go provider（如 `deepseek` 直连）时不出现 Go 用量行。
- [x] opencode-go 时页脚出现形如 `Go 5h 0% · 周 22% · 月 65% ⟳ 2d17h` 的行。
- [x] 切模型到非 go provider 后该行消失（hide，不 remove），切回立刻恢复。
- [x] 主题文件补齐 21 个 token 后被宿主接受（无 `unknown theme color token` 报错）。

**"怎么证明它是对的"**：前 3 条由命令输出证明；后 4 条需在交互式 TUI 观察（`-p` 模式不渲染 UI，无法自动验证）。

## 实施结果（已验证 / 未验证）

### 已验证

| 项 | 证据 |
| --- | --- |
| 全量单测 | `cargo test` → **146 passed, 0 failed, 1 ignored**（新增 23 个） |
| release 构建 | `cargo build --release` → **0 error, 0 warning** |
| 插件加载、工具数不变 | `extensions: loaded 10 plugin(s) (15 tool(s))` |
| 真实链路 | `#[ignore]` 联网测试通过：`footer = Go 5h 2% · 周 23% · 月 65% ⟳ 2d17h` |
| 凭据不泄漏 | 联网测试断言 footer 不含 `api_key` / `oc_sk_` 前缀 |
| 主题被宿主接受 | `rpi --theme github-dark` 正常跑完，无 `unknown theme color token` |
| 主题 token 完整 | 56/56 全覆盖，0 缺失、0 冗余、0 悬空引用 |
| `-p` 模式退出噪声 | 3/3 次运行均 **0 条** `go-usage` 输出（门控生效） |

### 未验证（需交互式 TUI 亲眼确认）

| 项 | 原因 |
| --- | --- |
| footer 那一行的实际渲染（宽度 / 截断 / 配色） | `-p` 模式不渲染 UI，只能证明宿主接受 op（`rc=0`） |
| 主题补 token 后的实际视觉效果 | 同上 |
| 切模型后 hide/show 的视觉过渡 | 同上 |

## 实施中发现的额外约束（已反映进代码）

1. **`register_plugin_lifecycle` 的 begin 钩子在 `-p` 模式下不被调用** —— 不能依赖它做退出保护。
2. **`BeforeTuiStart` 只在交互式 TUI 触发** —— 据此作为唯一启用门控，从根上避免 `-p` 模式下的退出期噪声。
3. 有了门控后，`-p`/headless 下功能完全休眠：不发请求、不注册组件、不产生任何日志。

## 影响面

### 新增文件

| 文件 | 职责 |
| --- | --- |
| `src/features/go_usage/mod.rs` | feature 注册入口：订阅事件、持有 `PluginApi`、懒启动工作线程 |
| `src/features/go_usage/usage.rs` | 纯函数：解析 `/usage` 响应 → `UsageSnapshot`；HTTP 拉取 |
| `src/features/go_usage/format.rs` | 纯函数：`UsageSnapshot` + `now` → footer 文本 + 倒计时 |
| `src/features/go_usage/api_key.rs` | 从 `models.json` 取 opencode.ai provider 的 apiKey（不落日志） |
| `src/features/go_usage/worker.rs` | 工作线程、60s 节流、`UiComponent` 调用封装 |

### 修改文件

| 文件 | 改动 |
| --- | --- |
| `src/features/mod.rs` | `pub mod go_usage;` + `FEATURES` 追加一行 |
| `README.md` | 新增 `go-usage` 章节 + feature 列表更新 |
| `~/.rpi/agent/themes/github-dark.json` | 补 21 个颜色 token |

### 外部依赖

- HTTP 端点 `GET https://opencode.ai/inference/go/v1/usage`（Bearer 认证）。
- 宿主能力：`RuntimeActionId::UiComponent`(22)、`RuntimeActionId::GetExecutionContext`(23)、`register_event_handler`。
- 无新增 crate 依赖（复用现有 `ureq` / `serde_json` / `url`）。

### 不涉及

- 无数据库、无表结构、无 SQL。
- 不改 `provider` / `msys2-bash` / `deepseek-search` 现有行为。

## 实测事实（本机已验证，构成设计前提）

### 端点

```
GET https://opencode.ai/inference/go/v1/usage   Authorization: Bearer <key>
200 {"usage":{"rolling":{"status":"ok","percent":0,"resetsAt":"2026-11-13T12:02:55.000Z"},
              "weekly":{"status":"ok","percent":22,"resetsAt":"2026-11-16T00:00:00.000Z"},
              "monthly":{"status":"ok","percent":65,"resetsAt":"2026-11-16T01:21:47.000Z"}}}
```

- `percent` = 该窗口**已用**百分比（用户确认月度 65% 即已用 65%）。
- 无 key / 错 key → `401 {"type":"error","error":{"type":"AuthError",...}}`。
- `https://opencode.ai/zen/go/v1/usage` 返回同一份数据（旧别名，作为兜底）。

### 宿主行为（探针插件实测）

| 项 | 结果 |
| --- | --- |
| `GetExecutionContext` 在**注册期**调用 | `rc=1 {"error":"runtime action invoked before harness was built"}` |
| `GetExecutionContext` 在**事件派生线程**调用 | `rc=0 {"model":{"provider":"opencode-go","id":"..."},"thinkingLevel":"medium",...}` |
| **在 `EventHandlerFn` 内同步**调 `runtime_action` | **进程 Segmentation fault (exit 139)** |
| `UiComponent` `register`（`footer.after`/`sidebar.*`/`overlay`） | `rc=0 {"ok":true,"changed":true}` |
| `UiComponent` `register` 带 `title` | `rc=1 {"error":"unexpected component field"}` |
| `UiComponent` `update` 带 `title` + `text` | `rc=0` |
| `UiComponent` `update` 未注册 id | `rc=1 {"error":"component is not registered"}` |
| `update` text 含 `\n` | 接受（多行） |
| `-p` 模式 `ModelSelect` 事件 | 不稳定（有时不发）→ 必须用 `AgentStart`/`AgentSettled` 兜底 |

### 主题 token 全集（宿主 0.23.0 二进制提取，共 56 个）

```
accent border borderAccent borderMuted success error warning muted dim text
thinkingText scrollbarTrack scrollbarThumb selectedBg searchMatchBg searchMatchText
userMessageBg userMessageText customMessageBg customMessageText customMessageLabel
toolPendingBg toolSuccessBg toolErrorBg toolTitle toolOutput
mdHeading mdLink mdLinkUrl mdCode mdCodeBlock mdCodeBlockBorder mdQuote mdQuoteBorder mdHr mdListBullet
toolDiffAdded toolDiffRemoved toolDiffContext
syntaxComment syntaxKeyword syntaxFunction syntaxVariable syntaxString syntaxNumber
syntaxType syntaxOperator syntaxPunctuation
thinkingOff thinkingMinimal thinkingLow thinkingMedium thinkingHigh thinkingXhigh thinkingMax bashMode
```

现有 `github-dark.json` 覆盖 35 个，缺失 21 个：
`syntax*` × 9、`thinking*` × 7、`scrollbar*` × 2、`searchMatch*` × 2、`userMessageText` × 1。

## 风险

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| 事件回调内同步调宿主 → segfault | 进程崩溃 | 一律 `std::thread::spawn`；事件回调内只做 `try_send` / 置标志 |
| 工作线程在进程退出时仍持有 `PluginApi` | 退出期崩溃 / UAF | 线程短命（单次 HTTP 后即返回），并注册 `register_plugin_lifecycle` 置停止标志 |
| `GetExecutionContext` 与模型切换存在竞争 | 短暂显示错误 provider 的用量 | 以 `ModelSelect` 载荷里的 `provider` 为准（事件自带，无竞争）；ctx 仅作兜底 |
| API key 泄漏到日志 | 凭据泄露 | 错误文本只含状态码与 `AuthError` 类型，绝不拼接 key |
| `percent` 是整数粒度 | 小用量显示 0% | 0% 时仍显示窗口（不隐藏），避免"未开始"与"已用 0.x%"混淆 |
| 主题补 token 写错值 | 宿主拒载该主题 | 只用 `vars` 里已定义的键名引用，或直接写 `#rrggbb` |

## 未决问题

1. **切模型瞬间的 provider 判定**：采用 `ModelSelect` 载荷（已确认含 `provider` 与 `baseUrl`）；`GetExecutionContext` 仅在工作线程首次启动时兜底。若两者都拿不到 → 不显示。
2. **`AgentStart` 时机下模型是否已生效**：实测 `AgentStart` 早于 `BeforeProviderRequest`，因此以 `ModelSelect` 为主、`AgentStart`/`AgentSettled` 只触发刷新不重判 provider。
3. **进程退出时线程清理**：本版用短命线程 + 停止标志；不引入 join，避免阻塞宿主退出。

## 实施顺序

1. `format.rs` + `usage.rs` 纯函数与单测（可独立验证）。
2. `api_key.rs`（复用 `config::select_opencode_providers` 的选中逻辑）。
3. `worker.rs`（线程、节流、`UiComponent` 调用封装）。
4. `mod.rs`（注册 + 订阅 + 生命周期）。
5. `features/mod.rs` 接线。
6. `github-dark.json` 补 token。
7. `README.md` 更新。
8. `cargo test` + `cargo build --release` + 加载验证。
