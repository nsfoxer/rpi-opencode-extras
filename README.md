# rpi-opencode

rpi 原生 Rust `cdylib` 插件：给**所有 baseUrl 指向 `opencode.ai` 的 provider** 在每次请求上
注入两个请求头。

- 宿主：rpi `0.22.1`，插件 ABI `4`，Windows x64
- 依赖：`rpi-plugin-sdk = 0.22.1`（`provider` feature）、`rpi-ai = 0.22.1`、`serde_json`、`async-trait`、`url`
- 不引入 `tokio` / `reqwest` / `ureq`（HTTP 客户端由宿主 SDK 提供）
- 不新增工具、不声明模型目录

## 注入内容

| header | 值 |
| --- | --- |
| `x-opencode-session` | `ses_<宿主真实 session id>`；无 session 时 `rpi-<pid>`；已带 `ses_` 前缀则不重复 |
| `user-agent` | `rpi-opencode/0.1` |

已有 header 全部保留，header 名统一小写，同名（大小写不同）只留一份，插件值优先。

## 匹配规则

只按 URL 判断，**provider 名字不参与决策**：

- `host == "opencode.ai"` → 命中
- `host` 以 `.opencode.ai` 结尾 → 命中
- 其他 → 不处理（`notopencode.ai`、`opencode.ai.evil.example` 都不命中）
- URL 解析失败 → 不处理

## 架构

单个 cdylib、单个 `export_plugin!` 入口；内部是「共享核心 + 可插拔 feature 模块」。

```
src/
  lib.rs            只做入口装配：mod 声明 + 一次 export_plugin!
  registry.rs       register_all：按 FEATURES 顺序执行，失败即中止
  config.rs         共享核心：定位 agent 目录、解析 models.json、按 URL 筛选 provider
  headers.rs        共享核心：纯函数 is_opencode_ai_url / build_headers
  error.rs          共享核心：统一错误类型 GoError
  features/
    mod.rs          Feature 结构 + FEATURES 列表
    provider.rs     provider feature：factory + HeaderInjectingProvider + ErrorProvider
                    + ABI request/stream + pub fn register(api)
    msys2_bash/
      mod.rs        msys2-bash feature：注册 4 个工具 + bash 提示词 + 工具生命周期 ABI
      config.rs     三路径定位、解析、校验、黑名单
      job.rs        JobManager：线程、超时杀树、保留策略、cwd 解析
      output.rs     TailBuffer：保留最后 N 字节 + UTF-8 lossy
      tools/
        mod.rs      工具名 / schema / wait_for
        bash.rs     bash（覆盖内置）
        get.rs      get_cmd_result
        kill.rs     kill_cmd
        list.rs     list_cmds
        result.rs   结果封装与参数解析辅助
```

注册流程：`export_plugin!` → `registry::register_all` → 遍历 `FEATURES` → 每个
`Feature::register(api)`。任一 feature 返回非 0 就立即中止并上报，避免半注册状态。

配置目录：`RPI_CODING_AGENT_DIR` 优先，否则 `<USERPROFILE|HOME>/.rpi/agent`，读取其中的 `models.json`。

注册时优先 `api.register_streaming_provider`，槽位为 `None` 时降级 `api.register_provider`。
**不调用** `rpi_plugin_sdk::provider::register()`（它会 `declare` 模型目录，可能覆盖
`models.json` 里用户维护的模型定义）。

## 当前 feature 列表

| name | 模块 | 作用 |
| --- | --- | --- |
| `provider` | `features/provider.rs` | 发现 baseUrl 指向 opencode.ai 的 provider，注册带 header 注入的流式包装层 |
| `msys2-bash` | `features/msys2_bash/` | **可选**：在 MSYS2 里执行命令（4 个工具）。无配置文件时不注册任何工具 |

## msys2-bash（可选子插件）

移植自独立的 `msys2-mcp` 项目，重新实现为插件内的 feature 模块。

### 配置文件就是开关

| 情形 | 行为 |
| --- | --- |
| 找到配置且校验通过 | 注册 `bash`（**覆盖内置 bash**）+ `get_cmd_result` + `kill_cmd` + `list_cmds` |
| 三个路径都没有配置 | 不注册任何工具，返回 0，内置 Git Bash 原样保留 |
| 配置存在但无效 | 打印一行 warning，不注册任何工具，不回退 Git Bash、不中断加载 |

配置路径优先级：`MSYS2_BASH_CONFIG` → `<agent_dir>/msys2-bash.json` → `~/.rpi/agent/msys2-bash.json`。

```json
{
  "msys2Root": "C:\\msys64",
  "msystem": "MINGW64",
  "shell": "usr/bin/bash.exe",
  "waitSeconds": 30,
  "defaultTimeoutSeconds": 900,
  "maxOutputBytes": 1048576,
  "maxJobs": 20
}
```

校验：`msys2Root` 必须存在且是目录；`shell`（相对 root 或绝对）必须是文件；
`waitSeconds` 夹紧 1-120；`defaultTimeoutSeconds` 夹紧 1-1800；`maxOutputBytes > 0`；`maxJobs >= 1`。
可选 `security.blacklistEnabled` / `security.blacklistPatterns`（默认开启内置黑名单，与 `msys2-mcp` 一致）。

### 两个不能混淆的时间参数

| 参数 | 含义 | 默认 | 范围 |
| --- | --- | --- | --- |
| `waitSeconds` | 本次调用阻塞多久，**不影响命令寿命** | 配置 `waitSeconds` | 1-120 |
| `timeout` | 命令**总截止**，到点杀进程树 | 配置 `defaultTimeoutSeconds` | 1-1800 |

### 取消与 kill 的区别

- 模型取消等待（`cancel`）：只停止轮询，命令**继续跑**，之后仍能用 `get_cmd_result` 取结果。
- `kill_cmd`：`taskkill /F /T` 杀整个进程树，状态标记 `killed`，幂等（已结束则 no-op）。

### 执行细节

- 可执行：`<msys2Root>/<shell>`，参数 `-lc "<command>"`
- 环境：`MSYSTEM=<msystem>`、`CHERE_INVOKING=1`、`LANG=C.UTF-8`
- `cwd` 优先级：工具参数 `cwd` → 宿主 `__rpi.cwd` → 进程当前目录；支持 MSYS2 路径（`/tmp` → `C:\msys64\tmp`）
- 输出：stdout+stderr 合并，**保留最后 `maxOutputBytes` 字节**，前置 `[truncated N bytes]`，UTF-8 lossy 解码
- job 状态：`running` / `completed` / `failed` / `timed_out` / `killed`；已完成保留最近 `maxJobs` 条，运行中的始终保留

### 工具数量参考（实测）

| 情形 | 输出 |
| --- | --- |
| 无插件 | `10 plugin(s) (11 tool(s))` |
| 插件加载、msys2-bash 未配置/无效 | `11 plugin(s) (11 tool(s))` |
| 插件加载、msys2-bash 启用 | `11 plugin(s) (15 tool(s))` |

启用时是 **15** 而不是 11+3=14：注册 4 个工具，其中 `bash` 覆盖同名内置工具，但宿主
计数时**内置与插件各算一次**（实测），净增 4。

## 如何新增一个 feature

1. 新建 `src/features/<name>.rs`，实现
   `pub fn register(api: &rpi_plugin_sdk::PluginApi) -> i32`。
2. 在 `src/features/mod.rs` 里加 `pub mod <name>;`，并在 `FEATURES` 追加一行。
3. `cargo test` + 重启 rpi 验证（`RPI_STARTUP_DIAGNOSTICS=1 rpi --verbose -p "1"`）。

最小示例（**模板，未启用**）：

```rust
// src/features/example.rs
use rpi_plugin_sdk::PluginApi;

/// 示例 feature：不做任何事，只演示注册签名。
pub fn register(_api: &PluginApi) -> i32 {
    // 实际能力在这里注册；返回 0 表示成功。
    0
}
```

```rust
// src/features/mod.rs 的改动（两行）
pub mod example;
// FEATURES 里追加：
// Feature { name: "example", register: example::register },
```

注意：`export_plugin!` 只能有一个，固定在 `lib.rs`；新 feature 不要自己导出入口。

## 已知限制

- **配置只在加载时读**：`models.json` 与 `msys2-bash.json` 都在插件注册阶段读取一次，
  之后修改需要重启 rpi（或触发宿主重载）才会生效。
- **不 declare 模型目录**：插件不向宿主声明任何模型，`--list-models` 的目录完全来自
  `models.json`；这也意味着插件无法自带默认模型。
- **header 注入只看 baseUrl**：若同一 provider 在不同模型上使用不同端点，仍按每次请求的
  `model.base_url` 判断；不读取 provider 名字。
- **job 状态是进程内状态**：`get_cmd_result` 只能取回**同一个 rpi 进程**内创建的 job。
  每次 `rpi -p` 都是新进程，所以跨命令行调用拿不到上一次的 id（返回 `no such job`）。
  同一会话内多轮调用正常。
- **`bash` 覆盖内置工具**：启用后宿主内置的 Git Bash 版本被插件的 MSYS2 版本替代。
  想恢复内置 bash，删掉配置文件即可（无需卸载 DLL）。
- **`poll` 是同步阻塞的**：限宿主的工具驱动没有 wake 回调，工具在 `poll` 内最多阻塞
  `waitSeconds`（≤120s）；因此不存在 Pending 分支，也意味着长命令会占住该次工具调用。

## 构建与安装

```bash
cd /c/Users/nsfoxer/Desktop/src/foxer/rpi-plugins/opencode
cargo build --release

# 先退出正在运行的 rpi，否则 DLL 被占用复制失败
cp target/release/rpi_opencode.dll /c/Users/nsfoxer/.rpi/agent/extensions/
```

## 校验

```bash
cargo test                 # 单元测试：URL 判定 / header 构造 / provider 筛选 / 配置校验
                           #            / 输出截断 / job 保留 / 工具边界（共 73 个）
cargo build --release      # 0 warning、0 error，只产出 rpi_opencode.dll

# 插件数 +1；msys2-bash 未配置时工具数不变
RPI_STARTUP_DIAGNOSTICS=1 rpi --verbose -p "1"

rpi --list-models          # models.json 里的模型依然可见
rpi --model opencode-go/deepseek-v4.1-flash -p "只回复 GO-OK"
rpi --model deepseek/deepseek-flash -p "只回复 DS-OK"   # 不经过插件
```

header 缺失时 opencode.ai 返回 400，因此请求成功即证明注入生效。

## 行为边界

- 构造 provider 失败（传输层）不会 panic：转成 `StopReason::Error` 终态交给宿主。
- 某个 provider 注册被宿主拒绝时插件返回非 0，宿主会报告
  `register returned nonzero code`；`models.json` 缺失或无匹配 provider 时返回 0，
  插件照常加载且不改变任何模型目录。
- 日志与输出中不出现 apiKey。
