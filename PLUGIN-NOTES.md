# rpi 插件开发笔记（Rust / cdylib）

> 面向「给 rpi 写一个 Rust 原生插件」的实操记录。
> 环境：rpi `0.22.1`、插件 ABI `4`、Windows 11 x64、MSYS2 MINGW64。
> 每条结论都标注了**来源**：`实测` = 在本机跑过；`源码` = 读过对应版本源码。
>
> 源码位置（本机已下载的 crate）：
> - SDK：`~/.cargo/registry/src/rsproxy.cn-*/rpi-plugin-sdk-0.22.1/`
> - AI：`~/.cargo/registry/src/rsproxy.cn-*/rpi-ai-0.22.1/`
> - 可用的完整例子：`~/Desktop/src/foxer/rpi-plugins/opencode/`

---

## 0. 三分钟决定"要不要写插件"

| 你的需求 | 该怎么做 | 说明 |
| --- | --- | --- |
| 只是想换个端点 / key / 模型名 | 改 `~/.rpi/agent/models.json` | 不用写代码 |
| 想给请求加固定 header | 先试 `models.json` 的 `headers` | 写死的值够用就别上插件 |
| header 要**动态**（真实 session id、时间戳、签名） | 写 provider 插件 | `models.json` 只能写死 |
| 要新增一个工具 | 写工具插件 | 见 §5 |
| 要新协议 / 改请求响应格式 | 写 provider 插件 | 见 §4 |

**一句话**：`models.json` 是静态配置，插件是动态逻辑。能静态解决就别写插件。

---

## 1. 目录结构与 Cargo.toml

单个插件 = 单个 crate = 单个 `cdylib`。**不要**建 workspace（实测无必要，且会让产物路径变复杂）。

```
rpi-plugins/
└── my-plugin/
    ├── Cargo.toml
    ├── README.md
    └── src/
        └── lib.rs
```

最小可用 `Cargo.toml`：

```toml
[package]
name = "rpi-my-plugin"      # 产物是 rpi_my_plugin.dll（连字符转下划线）
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]      # 必须是 cdylib，宿主按动态库加载

[dependencies]
# 版本必须与宿主 rpi 完全一致，否则 ABI 不匹配、插件被静默跳过
rpi-plugin-sdk = "=0.22.1"
serde_json = "1"
```

要点（`源码`）：

- `crate-type = ["cdylib"]` 不可省。写成 `["lib"]` 不会产出 `.dll`。
- `rpi-plugin-sdk` 用 `=` 精确锁版本。宿主 `abi_version` 是常量 `4`（`RPI_PLUGIN_ABI_VERSION_UNIFIED = 4`），SDK 与宿主不匹配时宿主会拒绝加载。
- **不要**依赖 `tokio` / `reqwest`：SDK 与 `rpi-ai` 已经带进来了，你自己再声明只会增加体积和版本冲突风险。同步 HTTP 用 `ureq`（它自带 rustls，不引入 OpenSSL）。
- provider 插件才需要 `features = ["provider"]` 和 `rpi-ai`（见 §4）。

---

## 2. 入口：`export_plugin!`

所有插件都只有一个入口符号 `rpi_plugin_register`，由宏生成（`源码`：`export_plugin!` 展开为 `#[no_mangle] pub extern "C" fn rpi_plugin_register`）。

```rust
rpi_plugin_sdk::export_plugin!(|api| {
    // api: &PluginApi，宿主的函数指针表
    // 注册工具 / provider / 命令 / 事件处理器...
    0   // 0 = 成功；非 0 = 宿主报告 "register returned nonzero code"
});
```

计数器直觉：**一个插件只能有一个 `export_plugin!`**。想要多个能力，就在 `lib.rs` 里派发到多个模块，而不是写多个宏。

推荐的 lib.rs 骨架（照抄 `opencode` 项目已验证的写法）：

```rust
mod features;   // 每个能力一个模块
mod registry;   // 统一的注册装配

rpi_plugin_sdk::export_plugin!(|api| registry::register_all(api));
```

```rust
// registry.rs
pub fn register_all(api: &rpi_plugin_sdk::PluginApi) -> i32 {
    for feature in crate::features::FEATURES {
        let rc = (feature.register)(api);
        if rc != 0 {
            eprintln!("my-plugin: feature `{}` failed rc={rc}", feature.name);
            return rc;      // 失败即中止，不留半注册状态
        }
    }
    0
}
```

```rust
// features/mod.rs
pub struct Feature {
    pub name: &'static str,
    pub register: fn(&rpi_plugin_sdk::PluginApi) -> i32,
}

pub static FEATURES: &[Feature] = &[
    Feature { name: "provider", register: provider::register },
];
```

---

## 3. 插件加载：产物放哪、怎么验证

### 放哪里

`~/.rpi/agent/extensions/` 下放 `.dll`。**文件名必须与 crate 名对应**（`rpi-my-plugin` → `rpi_my_plugin.dll`）。

### 验证是否加载

```sh
# 注意：一定要先 unset RPI_CODING_AGENT_DIR（见下方坑 #1）
env -u RPI_CODING_AGENT_DIR RPI_STARTUP_DIAGNOSTICS=1 rpi --verbose -p "1" 2>&1 | grep -i 'loaded'
```

期望输出形如（实测）：

```
extensions: loaded 10 plugin(s) (15 tool(s))
```

- `plugin(s)` 计数 +1 → 插件被加载。
- `tool(s)` 计数不变 → 没意外新增工具。

### 复制产物时 DLL 被占用

`实测` 结论，与直觉不同：

| 操作 | 宿主运行中 | 说明 |
| --- | --- | --- |
| `cp -f new.dll existing.dll` | **成功** | 覆盖能过 |
| `echo -n "" >> existing.dll` | **可能失败**（`Device or resource busy`） | 追加写会被拦 |

但仍建议**先退出 rpi 再复制**：覆盖成功后若插件正被使用，行为不确定（Windows 下已加载的 DLL 内容不会回滚，可能导致新旧代码混用）。稳妥流程：

```sh
# 1. 退出正在运行的 rpi（TUI 里 /exit）
# 2. 构建
cargo build --release
# 3. 复制
cp target/release/rpi_my_plugin.dll ~/.rpi/agent/extensions/
```

---

## 4. Provider 插件（流式 + 动态 header）

这是最容易踩坑的部分。先明确一个**关键事实**：

> **`factory` 里修改 `options.headers` 是无效的。**
> `源码`：SDK 的 `provider::drive()` 先解析出 `options`，然后
> `let provider = factory(&request.model, &options);`
> `drive_stream(provider, &request, &options, ...)`
> — 传给 `stream_simple` 的是**同一份原始 `options`**，不是 factory 返回值里的副本。
>
> 因此 header 注入必须发生在 `Provider::stream_simple` 内部（包装层）。

（这一条和某些文档的写法不一致，以 `rpi-plugin-sdk-0.22.1/src/provider.rs` 第 384–396 行为准。）

### 4.1 Cargo.toml

```toml
[dependencies]
rpi-plugin-sdk = { version = "=0.22.1", features = ["provider"] }
rpi-ai = "=0.22.1"
serde_json = "1"
async-trait = "0.1"
```

`features = ["provider"]` 不可省（`源码`：`provider` feature 才启用 `provider` 模块与 `export_provider!`，同时带入 tokio / reqwest / rpi-ai）。

### 4.2 两种注册方式，别选错

| 方式 | 适合 | 代价 |
| --- | --- | --- |
| `export_provider!(id, base_url, style, manifest, factory)` | 单 provider、教学/简单场景 | 会 `declare` 一份模型目录，**可能覆盖 `models.json` 的模型定义** |
| `export_plugin!` + 手写 `register_streaming_provider` | 想完全掌握目录、要按条件包装 | 代码多，但不动用户配置 |

`opencode` 项目选的是第二种（`源码`：`src/features/provider/mod.rs` 注释明确写了"不调用 `register()`：那会 declare 一份模型目录"）。

如果你只关心"注入 header"、希望 `--list-models` 完全由 `models.json` 决定 → **选第二种**。

使用 `export_provider!` 的 manifest 里 `"models": []` 意味着不声明任何模型；此时模型能否出现完全取决于 `models.json`（实测：`opencode-go` 的 3 个模型在 `models.json` 里，`--list-models` 正常显示）。

### 4.3 三层结构（推荐）

```
factory  →  构造内层 provider，失败时返回 ErrorProvider
  └─ HeaderInjectingProvider  →  每次请求注入 header
        └─ OpenAiCompletionsProvider（SDK 的 completions() 提供）
```

**第一层：factory**

```rust
pub fn factory(model: &Model, options: &SimpleStreamOptions) -> Arc<dyn Provider> {
    match rpi_plugin_sdk::provider::completions(model, options, false) {
        Ok(inner) => Arc::new(HeaderInjectingProvider::new(inner)),
        Err(error) => Arc::new(ErrorProvider::new(error)),   // 不 panic、不 unwrap
    }
}
```

- `completions(model, options, false)` 返回 `Result<Arc<dyn Provider>, String>`，`false` = 需要认证。
- `Err` 必须转成 `ErrorProvider`：`Provider::stream_simple` 的签名不返回 `Result`，失败只能通过事件流表达。

**第二层：HeaderInjectingProvider（真正生效的地方）**

```rust
#[async_trait]
impl Provider for HeaderInjectingProvider {
    fn id(&self) -> &str { self.inner.id() }
    fn models(&self) -> &[Model] { self.inner.models() }

    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let mut opts = opts.clone();                       // 只改自己这份
        opts.headers = Some(build_headers(
            opts.headers.clone(),
            opts.session_id.as_deref(),
        ));
        self.inner.stream_simple(model, ctx, &opts).await
    }
}
```

**第三层：ErrorProvider**

```rust
async fn stream_simple(
    &self, model: &Model, _ctx: &Context, opts: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let (mut producer, stream) =
        create_assistant_message_event_stream_with_cancellation(&opts.signal);
    let error = AssistantMessage::terminal(
        model.api.clone(), model.provider.clone(), model.id.clone(),
        StopReason::Error, self.message.clone(), 0,
    );
    producer.push(AssistantMessageEvent::Error {
        reason: ErrorReason::Error, error,
    }).await;
    stream
}
```

`源码`要点：
- `AssistantMessage::terminal(api, provider, model, stop_reason, message, timestamp)` — 6 个参数。
- `create_assistant_message_event_stream_with_cancellation(&opts.signal)` 继承取消信号。
- `push` 是 `async` 的。
- **不要跨线程、不要阻塞**：这个 future 在宿主当前线程 block_on 里跑。

### 4.4 header 构造写成纯函数（必须）

把 header 逻辑抽成不碰 IO 的纯函数，才能单测：

```rust
pub fn build_headers(
    existing: Option<BTreeMap<String, String>>,
    session_id: Option<&str>,
) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();

    // 第一步：已有 header 全部小写化，实现大小写不敏感合并
    for (name, value) in existing.unwrap_or_default() {
        headers.insert(name.to_ascii_lowercase(), value);
    }

    // 第二步：session 头（有值加 ses_ 前缀；无值用 rpi-<pid> 兜底，保证非空）
    headers.insert(
        "x-opencode-session".to_string(),
        normalize_session_id(session_id),
    );

    // 第三步：覆盖 user-agent
    headers.insert("user-agent".to_string(), "rpi-my-plugin/0.1".to_string());

    headers
}

fn normalize_session_id(session_id: Option<&str>) -> String {
    let trimmed = session_id.unwrap_or("").trim();
    if trimmed.is_empty() {
        return format!("rpi-{}", std::process::id());
    }
    if trimmed.starts_with("ses_") {
        return trimmed.to_string();       // 不重复加前缀
    }
    format!("ses_{trimmed}")
}
```

`源码`：`SimpleStreamOptions.headers` 是 `Option<BTreeMap<String, String>>`；
`openai_completions::prepare_request` 合并顺序是 `model.headers` → `opts.headers` → apiKey → `ensure_user_agent`（后者只在没有 user-agent 时才补，所以我们设的会被保留）。

### 4.5 单测：不引 tokio 也能测 `stream_simple`

`Provider::stream_simple` 是 async，但被测实现不会挂起，所以一个 mini executor 就够（**不需要** `tokio` dev-dependency，实测可用）：

```rust
#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::task::{Context as TaskContext, Poll, Wake, Waker};

    fn block_on<F: Future>(future: F) -> Option<F::Output> {
        struct NoopWake;
        impl Wake for NoopWake { fn wake(self: Arc<Self>) {} }
        let waker = Waker::from(Arc::new(NoopWake));
        let mut task = TaskContext::from_waker(&waker);
        let mut future = Box::pin(future);
        match future.as_mut().poll(&mut task) {
            Poll::Ready(v) => Some(v),
            Poll::Pending => None,
        }
    }
}
```

验证"header 真的注入到了内层"：写一个 `CapturingProvider` 把收到的 `opts.headers` 存进 `Arc<Mutex<Option<...>>>`，然后断言。

---

## 5. 工具插件（简述）

```rust
let Some(register_tool) = api.register_tool else { return 1 };

let schema = Box::new(StableToolSchema {
    name: StbString::from_string(TOOL_NAME.to_string()),
    description: StbString::from_string(TOOL_DESCRIPTION.to_string()),
    parameters: StbString::from_string(TOOL_PARAMETERS.to_string()),  // JSON Schema 字符串
});

let rc = register_tool(
    &*schema as *const StableToolSchema,
    worker::execute,   // 执行
    worker::poll,      // 轮询
    worker::cancel,    // 取消
    worker::destroy,   // 销毁
    plugin_free_string, // 释放回调
);
drop(schema);          // 宿主已复制走字段，只回收容器
```

`源码`要点：
- `StableToolSchema` 三个字段都是 `StbString`；`parameters` 是**序列化成字符串的 JSON Schema**，不是对象。
- 工具参数里宿主会注入保留键 `__rpi`（含 `cwd` / `sessionId`）。**参数类型必须容忍未知顶层字段**，否则解析失败。别用 `#[serde(deny_unknown_fields)]`。
- 可选槽位 `api.register_tool_prompt` 提供 `promptSnippet` / `promptGuidelines`；旧宿主没有该槽位时跳过，工具仍可用。

---

## 6. 坑位清单（按踩坑频率排序）

### 坑 1：`RPI_CODING_AGENT_DIR` 覆盖了配置目录 ⚠️ 最隐蔽

**现象**（`实测`，两种都出现过）：

| 命令 | 带该变量 | 清掉变量 |
| --- | --- | --- |
| `rpi --list-models` | `grep -c opencode-go` → **0** | → **3** |
| `rpi --verbose -p "1"` | 启动就 `error: No API key. Set one of: models.json apiKey, ...`，**看不到 loaded 行** | `extensions: loaded 10 plugin(s) (15 tool(s))` |

所以症状不只是“插件变少”，也可能是“凭据突然全没了”。看到一个明明配好的 key 报 `No API key`，先查这个变量。

**原因**（`实测`）：环境里存在 `RPI_CODING_AGENT_DIR=C:/msys64/tmp/verify-new`（某个子代理/测试留下的），导致宿主去读那个空目录，`models.json` / `extensions/` 全落空。

**排查**：
```sh
env | grep RPI_CODING_AGENT_DIR
```

**解决**：验证时清掉它
```sh
env -u RPI_CODING_AGENT_DIR rpi --list-models
```

### 坑 2：以为在 `factory` 里改 headers 就行

无效。见 §4 开头。必须在 `stream_simple` 里改。

### 坑 3：`--list-models` 里模型消失了

如果用了 `export_provider!` 或调用 `provider::register()`，插件会 `declare` 模型目录，可能覆盖 `models.json` 的定义。

**对策**：manifest 用 `"models": []`（不声明），让 `models.json` 独占模型定义；或干脆改用 `export_plugin!` 手写注册。

### 坑 4：请求 400，但插件看着"加载成功"

header 没真的注入。检查：
1. `stream_simple` 里到底改没改 `opts`（不是 `factory` 里）。
2. header 名大小写：HTTP header 不区分大小写，但如果你的代码里做了 `headers.get("X-...")` 这类查表，大小写不对就查不到。
3. 用 `CapturingProvider` 单测断言内层拿到的 header（§4.5）。

### 坑 5：插件 DLL 不加载

逐项检查：
1. 文件名与 crate 名对应（`rpi-my-plugin` → `rpi_my_plugin.dll`）。
2. 放在 `~/.rpi/agent/extensions/`。
3. `RPI_CODING_AGENT_DIR` 没把它指到别处（坑 1）。
4. `cargo build --release` 用的 SDK 版本与宿主一致（`=0.22.1`）。
5. `crate-type = ["cdylib"]` 有没有。

### 坑 6：别指望 `mvn`，也别用 PowerShell

`实测` 本机 PATH 情况：`java` / `mvn` **不存在**，`powershell` 存在（`/c/WINDOWS/System32/WindowsPowerShell/v1.0/powershell`）。

所以：
- 构建 / 测试统一用 `cargo`，路径写 `/c/Users/...`（MSYS2 风格）。
- 不要用 PowerShell 或 cmd 语法串联命令。
- 这是 Rust 插件项目，与 Java/Maven 无关，`mvn` 缺失不影响。

---

## 7. 验收清单（Provider 插件）

```sh
cd /c/Users/nsfoxer/Desktop/src/foxer/rpi-plugins/my-plugin

# 1. 构建通过
cargo build --release

# 2. 单测通过（纯函数 + 注入层）
cargo test --release

# 3. 插件计数 +1，工具数不变（先确认没有 RPI_CODING_AGENT_DIR 干扰）
env -u RPI_CODING_AGENT_DIR RPI_STARTUP_DIAGNOSTICS=1 rpi --verbose -p "1" 2>&1 | grep 'loaded'

# 4. 模型目录仍在
env -u RPI_CODING_AGENT_DIR rpi --list-models | grep my-provider

# 5. 真实请求成功（header 没带上服务端会 400，成功即证明注入生效）
env -u RPI_CODING_AGENT_DIR rpi --model my-provider/some-model -p "只回复 OK"

# 6. 连跑两次都成功（排除偶发）
```

最后一步是关键：**服务端接受 + 返回正常内容** 才算 header 注入真正生效。

---

## 8. 与 `models.json` 的分工

| 配置项 | 归谁管 |
| --- | --- |
| `apiKey` | `models.json`（插件不读、不打印、不缓存） |
| `baseUrl` | `models.json`（插件可在注册时用作筛选依据） |
| 模型列表 / context / cost | `models.json`（除非插件刻意 `declare`） |
| `headers` 里的**静态**值 | `models.json` 可留 |
| `headers` 里的**动态**值（session id） | 插件覆盖 → 从 `models.json` 里**删掉**，避免两处配置打架 |

`实测`：`opencode-go` 的 `models.json` 里原本有 `"x-opencode-session": "ses_rpi_default"`（写死值）。
现在该 `headers` 段已整体删除，只留 `api` / `baseUrl` / `apiKey` / `models`，
动态 header 完全由插件注入，不再有两处配置打架的问题。

---

## 9. 参考

- Provider 接入规范：`docs/extensions/provider-integration.md`（pi-rust 仓库）
- 扩展开发：`docs/extensions/authoring.md`
- SDK README（本机）：`~/.cargo/registry/src/rsproxy.cn-*/rpi-plugin-sdk-0.22.1/README.md`
- SDK 源码（最重要）：`.../rpi-plugin-sdk-0.22.1/src/provider.rs`
- 可运行的完整例子：`~/Desktop/src/foxer/rpi-plugins/opencode/`（provider / msys2-bash / deepseek-search 三种能力）
- 工具插件最小例子（本机）：`~/.cargo/registry/src/rsproxy.cn-*/rpi-todo-0.3.4/src/lib.rs`

> 遇到文档与源码不一致时，**以源码为准**，并把结论补到本文件。
